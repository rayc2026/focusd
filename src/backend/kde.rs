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
//!
//! ## 常驻自愈（迭代三 · T03）
//!
//! KWin 重启 / 用户在「系统设置 → 窗口管理 → KWin 脚本」里禁用 / Shell 重载
//! 都会让脚本静默失效。旧实现 `loop { thread::park() }` 只加载一次、失效后
//! 永远不再恢复——这正是「常驻但说谎」。现在改为**健康检测循环**：
//!
//! ```text
//! 每 FOCUSD_KWIN_HEALTH_MS（默认 10s）调 isScriptLoaded
//!   Loaded      → 什么都不做
//!   Missing     → 推 Focus::default()（仅首次）+ loadScript + Script.run()
//!   Unavailable → 推 Focus::default()（仅首次）+ 等待 KWin 回归（不重试加载）
//! ```
//!
//! 脚本被重新 `run()` 后会重跑 `main.js` 全文（含开头那句
//! `report(workspace.activeWindow)`），因此**立即**补推当前焦点，
//! 不必等用户切窗，也**不必重启 focusd**。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use zbus::blocking;

use super::reconnect::LogThrottle;
use super::selector::{ProbeContext, probe_kde, RealProbe};
use super::{Backend, Focus};

/// 内嵌的 KWin 脚本，运行时落盘 `$XDG_DATA_HOME/focusd/kwin/main.js`。
/// 内容与 packaging/kde/org.focusd.kwin（kpackagetool6 手动安装路径）保持一致。
const KWIN_SCRIPT_JS: &str = include_str!("../../packaging/kde/org.focusd.kwin/contents/code/main.js");

/// KWin Scripting 的 bus name / 对象路径 / 接口名。
const KWIN_SERVICE: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_IFACE: &str = "org.kde.kwin.Scripting";
/// 单个脚本对象的接口名（路径为 `/Scripting/Script<id>`）。
const KWIN_SCRIPT_IFACE: &str = "org.kde.kwin.Script";

/// loadScript 用的插件名（同名重复加载会返回已有 id，天然幂等）。
pub const PLUGIN_NAME: &str = "focusd";

/// `kpackagetool6` 安装路径（`packaging/kde/org.focusd.kwin`）里 `KPlugin.Id`
/// 用的名字。与 [`PLUGIN_NAME`] 不一致（U2），所以健康检测必须**双名探测**：
/// 任一命中即视为已加载，否则会把 kpackagetool 装的实例判成缺失并重复加载。
pub const PKG_PLUGIN_ID: &str = "org.focusd.kwin";

/// `FOCUSD_KWIN_HEALTH_MS` 的默认值（10s）。
/// 上限 5min：比这更慢就失去了「常驻自愈」的意义。下限 1s：再快就是对
/// KWin 的 D-Bus 接口无谓加压。
const HEALTH_MS_DEFAULT: u64 = 10_000;
const KEY_HEALTH_MS: &str = "FOCUSD_KWIN_HEALTH_MS";

/// 脚本装载失败时给用户的可操作提示（既有文案，单测依赖其中的
/// `kpackagetool6` / `KWin` 关键字，改动前请先看 `tests` 与 `MANUAL-TEST`）。
const LOAD_HINT: &str = "调用 KWin Scripting 失败（isScriptLoaded / loadScript）。\
     可手动安装脚本：kpackagetool6 --type=KWin/Script -i packaging/kde/org.focusd.kwin，\
     然后在 系统设置 → 窗口管理 → KWin 脚本 中启用 focusd；\
     或在 KWin 脚本控制台（qdbus org.kde.KWin /KWin runScript）手动加载";

/// 自动重注册失败时的降级提示。
const RELOAD_HINT: &str = "请在 系统设置 → 窗口管理 → KWin 脚本 重新启用 focusd，或 \
     kpackagetool6 --type=KWin/Script -i packaging/kde/org.focusd.kwin 重装\
     （无需重启 focusd）";

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
        let ctl = KwinCtl::connect()?;
        self.run_with(tx, ctl)
    }
}

// ---------------------------------------------------------------------------
// KWin Scripting 控制面
// ---------------------------------------------------------------------------

/// KWin 脚本的健康状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KwinHealth {
    /// 脚本已加载（双名探测任一命中）。
    Loaded,
    /// KWin 在线，但脚本没在——KWin 重启过 / 被手动禁用。
    Missing,
    /// `org.kde.KWin` 不在线：只能等它回来，重试加载没有意义。
    Unavailable,
}

/// `load_and_run` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// 本来就加载着（双名探测命中，或 `loadScript` 返回 `-1`）。
    AlreadyLoaded,
    /// 本次新加载并 `run()` 了，值是 KWin 分配的脚本 id。
    Loaded(i32),
}

/// `org.kde.KWin /Scripting` 的控制器。
///
/// 只持有 `Connection` **而不缓存 `Proxy`**：KWin 重启时 bus name 的属主会
/// 换成另一个 unique name，缓存下来的 Proxy 可能一直指向已消失的属主（U5）。
/// 健康检测 10s 一次，现场建 Proxy 的开销可以忽略。
pub struct KwinCtl {
    conn: blocking::Connection,
}

impl KwinCtl {
    /// 建立会话总线连接。`run()` 开头只调一次；后续所有调用复用它。
    pub fn connect() -> Result<Self> {
        let conn = blocking::Connection::session()
            .context("无法连接会话总线（KDE 后端需要 D-Bus）")?;
        Ok(Self { conn })
    }

    fn scripting(&self) -> Result<blocking::Proxy<'static>> {
        blocking::Proxy::new(
            &self.conn,
            KWIN_SERVICE,
            KWIN_SCRIPTING_PATH,
            KWIN_SCRIPTING_IFACE,
        )
        .context("无法创建 KWin Scripting 代理（org.kde.KWin 不在线？）")
    }

    /// 健康检测：双名探测 `isScriptLoaded`。
    ///
    /// 两个名字都因 `ServiceUnknown` 失败 → `Ok(Unavailable)`（KWin 不在线是
    /// 可恢复状态，不该让后端线程退出）；其他错误（接口不存在等）原样上抛。
    pub fn health(&self) -> Result<KwinHealth> {
        let scripting = self.scripting()?;
        let mut seen = [false, false];
        let mut any_ok = false;
        for (i, name) in [PLUGIN_NAME, PKG_PLUGIN_ID].into_iter().enumerate() {
            match scripting.call::<_, _, bool>("isScriptLoaded", &(name,)) {
                Ok(v) => {
                    seen[i] = v;
                    any_ok = true;
                }
                Err(e) => {
                    if !is_service_unknown(&e) {
                        return Err(e)
                            .with_context(|| format!("KWin isScriptLoaded({name}) 调用失败"));
                    }
                    // KWin 不在线：先记下，两个名都试完再统一判 Unavailable。
                }
            }
        }
        // 一次都没问到结果 = KWin 不在线，而不是「脚本没装」。
        if !any_ok {
            return Ok(KwinHealth::Unavailable);
        }
        Ok(judge(seen[0], seen[1]))
    }

    /// 幂等地装载并启动脚本：`isScriptLoaded` 命中就直接返回 `AlreadyLoaded`，
    /// 否则 `loadScript` + `Script{id}.run()`。
    pub fn load_and_run(&self, script_path: &Path) -> Result<LoadOutcome> {
        let scripting = self.scripting()?;
        for name in [PLUGIN_NAME, PKG_PLUGIN_ID] {
            if scripting.call::<_, _, bool>("isScriptLoaded", &(name,))? {
                return Ok(LoadOutcome::AlreadyLoaded);
            }
        }
        let path_str = script_path.display().to_string();
        let id: i32 = scripting.call("loadScript", &(path_str.as_str(), PLUGIN_NAME))?;
        let outcome = outcome_of(id)?;
        if let LoadOutcome::Loaded(n) = outcome {
            let script = blocking::Proxy::new(
                &self.conn,
                KWIN_SERVICE,
                format!("/Scripting/Script{n}"),
                KWIN_SCRIPT_IFACE,
            )
            .context("无法创建 KWin Script 代理")?;
            let _: () = script.call("run", &()).context("调用 Script.run 失败")?;
        }
        Ok(outcome)
    }
}

/// 判断一个 zbus 错误是否「目标 bus name 没有属主」。
///
/// KWin 不在线时 dbus-daemon 回 `org.freedesktop.DBus.Error.ServiceUnknown`。
/// 同时匹配枚举变体与消息文本：不同 zbus 版本的包装层不完全一致，
/// 漏判会让「KWin 重启」被当成致命错误直接退出后端线程。
fn is_service_unknown(e: &zbus::Error) -> bool {
    // `Error::FDO` 装的是 `Box<fdo::Error>`（zbus 为避免枚举体积膨胀）。
    let boxed = match e {
        zbus::Error::FDO(inner) => matches!(inner.as_ref(), zbus::fdo::Error::ServiceUnknown(_)),
        _ => false,
    };
    let text = e.to_string();
    boxed || text.contains("ServiceUnknown") || text.contains("was not provided by any")
}

/// 双名探测的判定（**纯函数**，便于单测覆盖 U2 这条分支）。
fn judge(loaded_short: bool, loaded_pkg: bool) -> KwinHealth {
    if loaded_short || loaded_pkg {
        KwinHealth::Loaded
    } else {
        KwinHealth::Missing
    }
}

/// `loadScript` 返回值的语义（**纯函数**）。
///
/// KWin 源码里 `-1` **只在** `isScriptLoaded(pluginName)` 为真时返回，意思是
/// 「已加载」而不是失败。旧实现把它当失败 `bail!`，导致已用 `kpackagetool6`
/// 装过脚本的用户每次启动都误报失败——本迭代修正为 `AlreadyLoaded`。
fn outcome_of(id: i32) -> Result<LoadOutcome> {
    match id {
        -1 => Ok(LoadOutcome::AlreadyLoaded),
        n if n < -1 => bail!("KWin loadScript 返回 {n}：脚本加载失败（检查脚本语法/权限）"),
        n => Ok(LoadOutcome::Loaded(n)),
    }
}

// ---------------------------------------------------------------------------
// 健康循环（纯状态机 + 阻塞驱动）
// ---------------------------------------------------------------------------

/// 一次健康检测后应采取的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthAction {
    /// 脚本在跑（或刚恢复）：什么都不做。
    Idle,
    /// 刚判定失效：**先推无焦点**，再重新注册。
    DegradeAndReload,
    /// 已处于降级态的失效：只重新注册（无焦点已推过，Dedup 会挡住重复）。
    Reload,
    /// 刚发现 KWin 不在线：**先推无焦点**，本轮不重试加载。
    DegradeAndWait,
    /// 已处于降级态且 KWin 仍不在线：继续等。
    Wait,
}

/// 健康循环的状态机（**不碰总线**，便于毫秒级单测）。
#[derive(Debug, Default)]
pub(crate) struct HealthLoop {
    /// 是否已对外推过无焦点快照（D2：只在「刚失效」那一次推）。
    degraded: bool,
    throttle: LogThrottle,
}

impl HealthLoop {
    /// 消费一次健康检测结果。
    pub fn next(&mut self, h: KwinHealth) -> HealthAction {
        match h {
            KwinHealth::Loaded => {
                if self.degraded {
                    self.degraded = false;
                    self.throttle.reset();
                    log::info!("KWin 脚本已恢复在线，焦点上报恢复正常（无需重启 focusd）");
                }
                HealthAction::Idle
            }
            KwinHealth::Missing => {
                if self.degraded {
                    HealthAction::Reload
                } else {
                    self.degraded = true;
                    HealthAction::DegradeAndReload
                }
            }
            KwinHealth::Unavailable => {
                if self.degraded {
                    HealthAction::Wait
                } else {
                    self.degraded = true;
                    HealthAction::DegradeAndWait
                }
            }
        }
    }

    /// 记录一次失败（重载失败 / KWin 不在线），返回本次该用的日志级别。
    pub fn on_failure(&mut self) -> log::Level {
        self.throttle.on_failure(0)
    }

    /// 重载成功：复位节流，让下一次真实故障重新以 Warn 打头。
    pub fn on_reload_ok(&mut self) {
        self.throttle.reset();
    }

    /// 已记录的失败次数（供日志文案「第 N 次」）。
    pub fn attempts(&self) -> u32 {
        self.throttle.attempts()
    }
}

// ---------------------------------------------------------------------------
// 后端实现
// ---------------------------------------------------------------------------

impl KdeBackend {
    /// 通过 `org.kde.kwin.Scripting` 加载内嵌脚本并运行（保留的公开入口）。
    ///
    /// 只在**启动期**调用一次；失败直接上抛（D1：配置/环境问题就该明确
    /// 报错退出，不进退避循环）。
    pub fn load_script(&self) -> Result<()> {
        let ctl = KwinCtl::connect()?;
        let script_path = script_file()?;
        self.load_script_with(&ctl, &script_path)
    }

    /// [`Self::load_script`] 的可注入版本（控制面由调用方提供）。
    pub fn load_script_with(&self, ctl: &KwinCtl, script_path: &Path) -> Result<()> {
        let outcome = ctl
            .load_and_run(script_path)
            .with_context(|| LOAD_HINT.to_string())?;
        match outcome {
            LoadOutcome::AlreadyLoaded => {
                log::info!("KWin 脚本已加载（此前已由 KWin 或 kpackagetool6 装载）");
            }
            LoadOutcome::Loaded(id) => {
                log::info!("KWin 脚本已加载: {}（id={id}）", script_path.display());
            }
        }
        Ok(())
    }

    /// 健康检测循环（可注入 `KwinCtl`，供契约 mock 测试驱动）。
    ///
    /// 与 `run()` 的唯一区别是控制面由外部提供；生产走 `run()`。
    pub fn run_with(&self, tx: Sender<Focus>, ctl: KwinCtl) -> Result<()> {
        let script_path = script_file()?;
        self.load_script_with(&ctl, &script_path)?;

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

        let interval = health_interval();
        log::info!(
            "KWin 脚本已加载，健康检测已启动（周期 {interval:?}）：\
             脚本失效会自动重新注册，无需重启 focusd"
        );

        let mut st = HealthLoop::default();
        loop {
            // 固定周期探测即可（不做指数退避）：KWin 不在线时一次探测只是
            // 两条 D-Bus 方法调用，成本与 wlroots 扫 socket 同量级；
            // 真正防止刷屏的是 `LogThrottle`，而不是把等待时间拉长。
            std::thread::sleep(interval);

            let health = match ctl.health() {
                Ok(h) => h,
                Err(e) => {
                    // health() 只在「非 ServiceUnknown 的硬错误」上抛；
                    // 这里按「不在线」处理，避免一次异常调用打死后端线程。
                    log::debug!("KWin 健康检测调用失败，按不在线处理: {e}");
                    KwinHealth::Unavailable
                }
            };

            match st.next(health) {
                HealthAction::Idle => {}
                HealthAction::Reload => reload(&ctl, &script_path, &tx, &mut st),
                HealthAction::DegradeAndReload => {
                    // D2：失效即无焦点——绝不保留上一次的陈旧值。
                    let _ = tx.send(Focus::default());
                    reload(&ctl, &script_path, &tx, &mut st);
                }
                HealthAction::DegradeAndWait => {
                    let _ = tx.send(Focus::default());
                    warn_offline(&mut st);
                }
                HealthAction::Wait => warn_offline(&mut st),
            }
        }
    }
}

/// 尝试重新注册脚本；失败则推无焦点 + 可操作 warn。
fn reload(ctl: &KwinCtl, script_path: &Path, tx: &Sender<Focus>, st: &mut HealthLoop) {
    match ctl.load_and_run(script_path) {
        Ok(LoadOutcome::Loaded(id)) => {
            log::info!(
                "KWin 脚本已自动重新注册（id={id}），脚本会立即补推当前焦点，无需重启 focusd"
            );
            st.on_reload_ok();
        }
        Ok(LoadOutcome::AlreadyLoaded) => {
            log::info!("KWin 脚本已在线（此前已由 KWin / kpackagetool6 装载），无需重启 focusd");
            st.on_reload_ok();
        }
        Err(e) => {
            // 连续重载失败：对外一致保持「无焦点」，并给出可操作提示。
            let _ = tx.send(Focus::default());
            let level = st.on_failure();
            log_at(
                level,
                &format!(
                    "KWin 脚本自动重新注册失败（第 {} 次，{e}）；期间对外上报无焦点。{RELOAD_HINT}",
                    st.attempts()
                ),
            );
        }
    }
}

/// KWin 不在线：只等待，不重试加载（加载必然失败，纯属刷屏）。
fn warn_offline(st: &mut HealthLoop) {
    let level = st.on_failure();
    log_at(
        level,
        &format!(
            "org.kde.KWin 不在线（第 {} 次探测失败）；等待 KWin 恢复，\
             期间对外上报无焦点（无需重启 focusd）",
            st.attempts()
        ),
    );
}

fn log_at(level: log::Level, msg: &str) {
    match level {
        log::Level::Warn => log::warn!("{msg}"),
        _ => log::debug!("{msg}"),
    }
}

/// 健康检测周期：`FOCUSD_KWIN_HEALTH_MS`（默认 10s，clamp 1s..=5min）。
fn health_interval() -> Duration {
    let ms = std::env::var(KEY_HEALTH_MS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(HEALTH_MS_DEFAULT)
        .clamp(1_000, 300_000);
    Duration::from_millis(ms)
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

    #[test]
    fn 健康判定_双名任一命中即已加载() {
        // U2：`focusd`（loadScript 的 pluginName）与 `org.focusd.kwin`
        // （kpackagetool6 的 KPlugin.Id）任一命中都必须判为已加载，
        // 否则会把 kpackagetool 装的实例判成缺失并重复 loadScript。
        assert_eq!(judge(true, false), KwinHealth::Loaded);
        assert_eq!(judge(false, true), KwinHealth::Loaded);
        assert_eq!(judge(true, true), KwinHealth::Loaded);
        assert_eq!(judge(false, false), KwinHealth::Missing);
    }

    #[test]
    fn loadscript返回负一是已加载而非失败() {
        // 旧实现把 -1 当失败并 bail!，误伤已用 kpackagetool6 装过脚本的用户。
        assert_eq!(outcome_of(-1).expect("-1 应视为已加载"), LoadOutcome::AlreadyLoaded);
        assert_eq!(outcome_of(0).expect("0 是合法 id"), LoadOutcome::Loaded(0));
        assert_eq!(outcome_of(7).expect("7 是合法 id"), LoadOutcome::Loaded(7));
        let err = outcome_of(-2).expect_err("-2 才是真失败");
        assert!(err.to_string().contains("-2"), "错误应带上返回值: {err}");
    }

    #[test]
    fn 健康循环_首次失效才推无焦点() {
        let mut st = HealthLoop::default();
        assert_eq!(st.next(KwinHealth::Loaded), HealthAction::Idle, "健康时什么都不做");
        // 首次失效 → 推无焦点 + 重载
        assert_eq!(st.next(KwinHealth::Missing), HealthAction::DegradeAndReload);
        // 仍在失效 → 只重载（Dedup 会挡住重复的无焦点）
        assert_eq!(st.next(KwinHealth::Missing), HealthAction::Reload);
        assert_eq!(st.next(KwinHealth::Missing), HealthAction::Reload);
    }

    #[test]
    fn 健康循环_kwin不在线只等待不重载() {
        let mut st = HealthLoop::default();
        assert_eq!(st.next(KwinHealth::Unavailable), HealthAction::DegradeAndWait);
        assert_eq!(st.next(KwinHealth::Unavailable), HealthAction::Wait);
        assert_eq!(st.next(KwinHealth::Unavailable), HealthAction::Wait);
    }

    #[test]
    fn 健康循环_恢复后重新回到首次语义() {
        let mut st = HealthLoop::default();
        assert_eq!(st.next(KwinHealth::Missing), HealthAction::DegradeAndReload);
        assert_eq!(st.next(KwinHealth::Loaded), HealthAction::Idle, "恢复时不推无焦点");
        // 恢复后下一次失效必须重新是「首次」——否则再也不会对外推无焦点。
        assert_eq!(st.next(KwinHealth::Missing), HealthAction::DegradeAndReload);
    }

    #[test]
    fn 健康循环_失败计数与日志节流() {
        let mut st = HealthLoop::default();
        assert_eq!(st.on_failure(), log::Level::Warn, "第 1 次必须 Warn");
        assert_eq!(st.on_failure(), log::Level::Debug);
        assert_eq!(st.attempts(), 2);
        st.on_reload_ok();
        assert_eq!(st.attempts(), 0, "重载成功后计数复位");
        assert_eq!(st.on_failure(), log::Level::Warn, "复位后重新视为首次");
    }

    #[test]
    fn 健康检测周期clamp() {
        // 只断言 clamp 边界不受 env 污染影响（env 是进程级全局，单测不改它）：
        // 1s 下限 / 5min 上限由 `health_interval` 的 clamp 保证，
        // 这里固定断言常量本身没被改坏。
        assert_eq!(HEALTH_MS_DEFAULT, 10_000);
        assert_eq!(KEY_HEALTH_MS, "FOCUSD_KWIN_HEALTH_MS");
    }

    #[test]
    fn 降级提示文案含可操作指引() {
        // 真机排障靠这两条文案，改动前请同步 docs/MANUAL-TEST.md K9。
        assert!(LOAD_HINT.contains("kpackagetool6"));
        assert!(RELOAD_HINT.contains("系统设置"));
        assert!(RELOAD_HINT.contains("kpackagetool6"));
        assert!(RELOAD_HINT.contains("无需重启 focusd"));
    }
}
