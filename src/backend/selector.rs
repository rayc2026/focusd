//! 后端注册表与选择逻辑。
//!
//! 探测必须是「纯函数 + 可注入环境」：CI 里没有真实桌面与会话总线，
//! 只有把环境变量与会话总线查询抽象出来（[`ProbeContext`]），
//! 「wlroots 会话选中 wlroots / KDE 会话选中 kde / 全空环境聚合报错」
//! 这些选择行为才能被离线单测覆盖。
//!
//! 自动探测顺序：wlroots → kde → gnome（wlroots 协议能力最强，
//! 且 probe 成本最低；GNOME 依赖用户安装扩展，放最后）。

use anyhow::{anyhow, bail, Result};

use super::gnome::GnomeBackend;
use super::kde::KdeBackend;
use super::wlroots::WlrootsBackend;
use super::Backend;

/// 探测所需外部环境的抽象。
pub trait ProbeContext {
    /// 读环境变量；空串视为未设置（有些会话管理器会留空壳变量）。
    fn env_var(&self, key: &str) -> Option<String>;

    /// 会话总线上是否已有该 bus name 的属主。
    /// 无会话总线时返回 false（而非报错）——缺总线本身就是「不可用」。
    fn bus_has_owner(&self, name: &str) -> bool;
}

/// 生产环境实现：进程环境变量 + session bus。
pub struct RealProbe;

impl ProbeContext for RealProbe {
    fn env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn bus_has_owner(&self, name: &str) -> bool {
        // 每次新建连接：probe 是启动期一次性调用，不值得为此常驻连接。
        // 连接失败（无 DBUS_SESSION_BUS_ADDRESS 等）一律视为不在线。
        let conn = match zbus::blocking::Connection::session() {
            Ok(c) => c,
            Err(e) => {
                log::debug!("probe: 无会话总线（{e}），视为 {name} 不在线");
                return false;
            }
        };
        zbus::blocking::fdo::DBusProxy::new(&conn)
            .and_then(|p| p.name_has_owner(name.to_string()))
            .unwrap_or_else(|e| {
                log::debug!("probe: name_has_owner({name}) 失败（{e}），视为不在线");
                false
            })
    }
}

/// 注册表条目：id → 探测纯函数 → 后端构造。
/// 顺序即自动探测优先级；新增后端在此登记一行即可参与自动选择。
pub struct Entry {
    pub id: &'static str,
    pub probe: fn(&dyn ProbeContext) -> Result<()>,
    pub make: fn() -> Box<dyn Backend>,
}

/// 后端注册表（顺序 = 自动选择优先级）。
pub fn registry() -> &'static [Entry] {
    &[
        Entry { id: "wlroots", probe: probe_wlroots, make: || Box::new(WlrootsBackend) },
        Entry { id: "kde", probe: probe_kde, make: || Box::new(KdeBackend) },
        Entry { id: "gnome", probe: probe_gnome, make: || Box::new(GnomeBackend) },
    ]
}

/// 全部已注册后端实例（供 CLI 列举）。
pub fn backends() -> Vec<Box<dyn Backend>> {
    registry().iter().map(|e| (e.make)()).collect()
}

/// wlroots 探测：WAYLAND_DISPLAY 已设置即视为候选。
///
/// 只查环境变量而不真正连接 compositor——probe 的职责是区分会话类型，
/// 协议是否可用由 `run()` 里 bind 失败时兜底报错（probe 无法在不建立
/// Wayland 连接的情况下预检协议版本，建立连接的成本留给 run）。
pub fn probe_wlroots(env: &dyn ProbeContext) -> Result<()> {
    match env.env_var("WAYLAND_DISPLAY") {
        Some(_) => Ok(()),
        None => Err(anyhow!(
            "WAYLAND_DISPLAY 未设置：当前不是 Wayland 会话（X11 / TTY 下不可用）"
        )),
    }
}

/// KDE 探测：`KDE_SESSION_VERSION` 存在 + 会话总线上有 `org.kde.KWin`。
pub fn probe_kde(env: &dyn ProbeContext) -> Result<()> {
    let version = env.env_var("KDE_SESSION_VERSION").ok_or_else(|| {
        anyhow!("KDE_SESSION_VERSION 未设置：不像 KDE Plasma 会话")
    })?;
    if env.bus_has_owner("org.kde.KWin") {
        Ok(())
    } else {
        Err(anyhow!(
            "会话总线上没有 org.kde.KWin（KDE_SESSION_VERSION={version}）\
             —— KWin 未运行，或会话总线不可达"
        ))
    }
}

/// GNOME 探测：focusd Shell 扩展的 bus name `org.focusd.Gnome1` 在线。
///
/// 不检查 `XDG_CURRENT_DESKTOP`——扩展是否可用只取决于它有没有跑起来，
/// 桌面环境变量反而不准确（用户可能在 GNOME 里跑 KDE 应用等）。
pub fn probe_gnome(env: &dyn ProbeContext) -> Result<()> {
    if env.bus_has_owner("org.focusd.Gnome1") {
        return Ok(());
    }
    Err(anyhow!(
        "org.focusd.Gnome1 不在线：GNOME 后端需要先安装并启用 Shell 扩展。\
         安装：将 packaging/gnome/focusd@rayc2026.github.io/ 复制到 \
         ~/.local/share/gnome-shell/extensions/，重载 Shell（Wayland 下注销重登）\
         后执行 gnome-extensions enable focusd@rayc2026.github.io"
    ))
}

/// 自动探测 + 选择：按注册表顺序返回第一个探测通过的后端；
/// 全部失败时聚合各后端原因报错（用户能一眼看出差什么）。
pub fn select(hint: Option<&str>) -> Result<Box<dyn Backend>> {
    select_with(&RealProbe, hint)
}

/// 可注入环境的 select 主体（单测入口）。
pub fn select_with(env: &dyn ProbeContext, hint: Option<&str>) -> Result<Box<dyn Backend>> {
    match hint {
        Some(id) => select_by_id_with(env, id),
        None => auto_select(env),
    }
}

fn auto_select(env: &dyn ProbeContext) -> Result<Box<dyn Backend>> {
    let mut failures: Vec<String> = Vec::new();
    for entry in registry() {
        match (entry.probe)(env) {
            Ok(()) => {
                log::info!("自动选中后端: {}", entry.id);
                return Ok((entry.make)());
            }
            Err(err) => failures.push(format!("  - {}: {:#}", entry.id, err)),
        }
    }
    bail!("没有可用后端：\n{}", failures.join("\n"))
}

fn select_by_id_with(env: &dyn ProbeContext, id: &str) -> Result<Box<dyn Backend>> {
    let entries = registry();
    let Some(entry) = entries.iter().find(|e| e.id == id) else {
        let available = entries.iter().map(|e| e.id).collect::<Vec<_>>().join(", ");
        bail!("未知后端 `{id}`，可用后端: {available}");
    };
    (entry.probe)(env).map_err(|err| {
        anyhow!("后端 `{id}` 探测失败: {:#}\n（也可省略 --backend 让 focusd 自动探测）", err)
    })?;
    Ok((entry.make)())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 假环境：变量表 + 总线属主列表，覆盖三种会话场景。
    struct FakeEnv {
        vars: HashMap<&'static str, String>,
        owners: Vec<&'static str>,
    }

    impl FakeEnv {
        fn empty() -> Self {
            Self { vars: HashMap::new(), owners: Vec::new() }
        }
        fn wlroots_session() -> Self {
            let mut vars = HashMap::new();
            vars.insert("WAYLAND_DISPLAY", "wayland-1".to_string());
            Self { vars, owners: Vec::new() }
        }
        fn kde_session() -> Self {
            let mut vars = HashMap::new();
            vars.insert("KDE_SESSION_VERSION", "6".to_string());
            Self { vars, owners: vec!["org.kde.KWin"] }
        }
    }

    impl ProbeContext for FakeEnv {
        fn env_var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn bus_has_owner(&self, name: &str) -> bool {
            self.owners.contains(&name)
        }
    }

    #[test]
    fn wlroots会话自动选中wlroots() {
        let b = select_with(&FakeEnv::wlroots_session(), None).expect("应选中 wlroots");
        assert_eq!(b.id(), "wlroots");
    }

    #[test]
    fn kde会话自动选中kde() {
        let b = select_with(&FakeEnv::kde_session(), None).expect("应选中 kde");
        assert_eq!(b.id(), "kde");
    }

    #[test]
    fn 全空环境报错并聚合各后端原因() {
        let err = select_with(&FakeEnv::empty(), None).unwrap_err();
        let msg = format!("{err:#}");
        // 三条原因都要在，用户才知道差什么
        assert!(msg.contains("wlroots"), "缺 wlroots 原因: {msg}");
        assert!(msg.contains("kde"), "缺 kde 原因: {msg}");
        assert!(msg.contains("gnome"), "缺 gnome 原因: {msg}");
    }

    #[test]
    fn 指定不存在的后端报错并列出可用项() {
        let err = select_with(&FakeEnv::empty(), Some("cosmic")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("cosmic"), "应提到未知后端: {msg}");
        assert!(msg.contains("wlroots, kde, gnome"), "应列出可用项: {msg}");
    }

    #[test]
    fn 指定后端强校验探测() {
        // wlroots 会话里指定 gnome：探测必须失败（扩展不在线）
        let err = select_with(&FakeEnv::wlroots_session(), Some("gnome")).unwrap_err();
        assert!(format!("{err:#}").contains("gnome"));
        // wlroots 会话里指定 wlroots：通过
        let b = select_with(&FakeEnv::wlroots_session(), Some("wlroots")).unwrap();
        assert_eq!(b.id(), "wlroots");
    }

    #[test]
    fn 注册表顺序与id正确() {
        let ids: Vec<_> = registry().iter().map(|e| e.id).collect();
        assert_eq!(ids, ["wlroots", "kde", "gnome"]);
        assert_eq!(backends().len(), 3);
    }
}
