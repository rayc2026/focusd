//! KDE KWin Script 后端。
//!
//! 链路（架构文档 §5.1）：KWin Scripting API 只能单向 `callDBus` 对外推送
//! （无法注册 bus name、无法导出方法、没有定时器），因此本后端在启动时
//! 通过 `org.kde.kwin.Scripting.loadScript` 加载内嵌脚本并 `run`；
//! 脚本订阅 `workspace.windowActivated` / `captionChanged`，
//! `callDBus` 推送到 `org.focusd.Focus1` 上的 `org.focusd.Focus1.Kwin.Report`。
//!
//! Report 的接收端有两条路径：
//! - **serve 模式**：主线程的 `start_serve` 已注册 `KwinReportIface`（推入主循环
//!   同一通道），后端只负责加载脚本然后驻留；
//! - **watch 模式**：无人注册 Report 接口，后端自己 `start_kwin_report`
//!   补注册一个（同样推入 tx），保证 `focusd watch` 在 KDE 上可用。
//!
//! `app_id` 来源 `resourceClass`（等价 wlroots 语义），`title` 来自 `caption`。

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::{bail, Context, Result};
use zbus::blocking;

use super::selector::{ProbeContext, probe_kde, RealProbe};
use super::{Backend, Focus};

/// 内嵌的 KWin 脚本，运行时落盘 `$XDG_DATA_HOME/focusd/kwin/main.js`。
/// 内容与 packaging/kde/org.focusd.kwin（kpackagetool6 手动安装路径）保持一致。
const KWIN_SCRIPT_JS: &str = include_str!("../../packaging/kde/org.focusd.kwin/contents/code/main.js");

/// KWin Scripting 的 bus name / 对象路径 / 接口名。
const KWIN_SERVICE: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_IFACE: &str = "org.kde.kwin.Scripting";

/// loadScript 用的插件名（同名重复加载会返回已有 id，天然幂等）。
const PLUGIN_NAME: &str = "focusd";

pub struct KdeBackend;

impl Backend for KdeBackend {
    fn id(&self) -> &'static str {
        "kde"
    }

    fn name(&self) -> &'static str {
        "KDE Plasma (KWin Script → D-Bus 推送)"
    }

    fn probe(&self) -> Result<()> {
        // 探测逻辑抽成纯函数放 selector（环境可注入），这里只是转发。
        probe_kde(&RealProbe)
    }

    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        self.load_script()?;

        // serve 模式下 org.focusd.Focus1 已由主线程注册（含 KwinReportIface）；
        // watch 模式下没人注册——这里补一个，否则脚本推送无人接收。
        let _conn = if RealProbe.bus_has_owner(crate::dbus::BUS_NAME) {
            log::info!("{} 已在线（serve 模式），Kwin 推送入口复用现有服务", crate::dbus::BUS_NAME);
            None
        } else {
            let conn = crate::dbus::start_kwin_report(tx.clone())
                .context("watch 模式注册 Kwin 推送入口失败（bus name 被其他实例占用？）")?;
            log::info!("已注册 {} 的 Kwin 推送入口（watch 模式）", crate::dbus::BUS_NAME);
            Some(conn)
        };

        log::info!("KWin 脚本已加载，等待窗口激活事件经 D-Bus 推送…");
        // 推送直接由 KwinReportIface 推入 tx 所在通道，后端线程只需驻留。
        loop {
            std::thread::park();
        }
    }
}

impl KdeBackend {
    /// 通过 `org.kde.kwin.Scripting` 加载内嵌脚本并运行。
    ///
    /// KWin 重启后脚本随之消失（不自动重试，见架构文档 UNCLEAR 2）；
    /// serve 常驻场景下焦点 daemon 也不会比 compositor 活得更久。
    pub fn load_script(&self) -> Result<()> {
        let conn = blocking::Connection::session()
            .context("无法连接会话总线（KDE 后端需要 D-Bus）")?;
        let scripting = blocking::Proxy::new(
            &conn,
            KWIN_SERVICE,
            KWIN_SCRIPTING_PATH,
            KWIN_SCRIPTING_IFACE,
        )
        .context("org.kde.KWin 不在线：请确认 KDE Plasma 会话已启动 KWin")?;

        let script_path = script_file()?;

        // KWin 约定：loadScript(filePath, pluginName) -> int 脚本 id；
        // 返回负值表示脚本加载失败（语法错误等）。
        let path_str = script_path.display().to_string();
        let id: i32 = scripting
            .call("loadScript", &(path_str.as_str(), PLUGIN_NAME))
            .with_context(|| {
                "调用 KWin loadScript 失败。可手动安装脚本：\
                 kpackagetool6 --type=KWin/Script -i packaging/kde/org.focusd.kwin，\
                 然后在 系统设置 → 窗口管理 → KWin 脚本 中启用 focusd；\
                 或在 KWin 脚本控制台（qdbus org.kde.KWin /KWin runScript）手动加载"
                    .to_string()
            })?;
        if id < 0 {
            bail!("KWin loadScript 返回 {id}：脚本加载失败（检查脚本语法/权限）");
        }
        log::info!("KWin 脚本已加载: {}（id={id}）", script_path.display());

        // 脚本对象在 /Scripting/Script<id>，接口 org.kde.kwin.Script
        let script = blocking::Proxy::new(
            &conn,
            KWIN_SERVICE,
            format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script",
        )
        .context("无法创建 KWin Script 代理")?;
        let _: () = script
            .call("run", &())
            .context("调用 Script.run 失败")?;
        Ok(())
    }
}

/// 把内嵌脚本写到 `$XDG_DATA_HOME/focusd/kwin/main.js`（默认 ~/.local/share）。
/// loadScript 需要一个 KWin 可读的文件路径，落盘是唯一通用方式。
fn script_file() -> Result<PathBuf> {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/share")))
        .context("无法确定数据目录（XDG_DATA_HOME / HOME 均未设置）")?;
    let dir = data_home.join("focusd").join("kwin");
    std::fs::create_dir_all(&dir).context("无法创建 focusd 数据目录")?;
    let path = dir.join("main.js");
    std::fs::write(&path, KWIN_SCRIPT_JS)
        .with_context(|| format!("无法写出 KWin 脚本文件: {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_script_在无_kwin_环境报可操作错误() {
        // 真机 KDE（org.kde.KWin 在线）下脚本可能成功加载，跳过；
        // CI（bus 上没有 KWin）走失败路径，断言错误文案可指导用户手动安装。
        if RealProbe.bus_has_owner(KWIN_SERVICE) {
            return;
        }
        let err = match KdeBackend.load_script() {
            Err(e) => e,
            Ok(()) => panic!("无 KWin 环境 load_script 应失败"),
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("kpackagetool6") || msg.contains("KWin"),
            "错误文案应包含安装指引: {msg}"
        );
    }
}
