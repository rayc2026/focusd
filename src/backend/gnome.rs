//! GNOME Shell Extension 后端。
//!
//! 链路（架构文档 §5.2）：Shell 扩展（packaging/gnome/focusd@rayc2026.github.io）
//! 用 Gio 在会话总线上 own `org.focusd.Gnome1` 并导出
//! `GetFocus() -> (s wm_class, s title)`；本后端作为 client 按
//! `FOCUSD_POLL_MS`（默认 250ms，可配置）轮询，推入主循环同一通道，
//! 同值重发由主循环的 Dedup 过滤。
//!
//! 语义差异（文档化于 README / docs/dbus.md）：GNOME 拿到的是 **WM_CLASS**
//! （X11 语义），与 wlroots 的 app_id 不严格相等——大多数应用两者一致。
//!
//! ## 常驻自愈（迭代三 · T04）
//!
//! 旧实现在 `run()` 里只建一次 `Proxy`，扩展被 disable / Shell 重载后
//! **只打 debug 日志、不推任何快照**——`GetFocus()` 于是返回一个**陈旧的**
//! 焦点值，对外谎报。这正是本迭代要根治的「常驻但说谎」。现在：
//!
//! - 每次轮询失败都推 `Focus::default()`（D2：无法确认焦点 = 无焦点）；
//! - 连续失败达 `FOCUSD_GNOME_FAIL_AFTER`（默认 3）次**重建 Proxy**
//!   （扩展重新 enable 后 bus name 属主已换新 unique name）；
//! - 失败期间 `warn`（节流）含 `gnome-extensions enable …` 可操作指引；
//! - 恢复时打 `info`——否则用户无法区分「恢复了」与「一直没恢复」。
//!
//! 只重建 **Proxy**、不重建 **Connection**（见 [`new_proxy`] 注释）。

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use log::Level;
use zbus::blocking;

use super::reconnect::LogThrottle;
use super::selector::{probe_gnome, RealProbe};
use super::{Backend, Focus, empty_to_none};

/// focusd Shell 扩展的 D-Bus 契约（extension.js 导出）。
pub const GNOME_BUS_NAME: &str = "org.focusd.Gnome1";
pub const GNOME_PATH: &str = "/org/focusd/Gnome";
pub const GNOME_IFACE: &str = "org.focusd.Gnome1";

pub struct GnomeBackend;

impl Backend for GnomeBackend {
    fn id(&self) -> &'static str {
        "gnome"
    }

    fn name(&self) -> &'static str {
        "GNOME Shell (Extension → D-Bus 轮询)"
    }

    fn probe(&self) -> Result<()> {
        // 探测逻辑抽成纯函数放 selector（环境可注入），这里只是转发。
        probe_gnome(&RealProbe)
    }

    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        let conn = blocking::Connection::session()
            .context("无法连接会话总线（GNOME 后端需要 D-Bus）")?;
        // 首次建 Proxy 失败 = 扩展压根没装：明确报错退出（D1），不进退避。
        let mut proxy = new_proxy(&conn)?;

        let poll = poll_interval();
        let after = fail_after();
        log::info!(
            "GNOME 扩展轮询已启动（间隔 {poll:?}，连续失败 {after} 次重建 Proxy）: {GNOME_BUS_NAME}"
        );

        let mut fails: u32 = 0;
        let mut throttle = LogThrottle::new();
        loop {
            // GetFocus() -> (s wm_class, s title)；空串 = 无焦点窗口
            match proxy.call::<_, _, (String, String)>("GetFocus", &()) {
                Ok((wm_class, title)) => {
                    if fails > 0 {
                        // 恢复必须可见：否则用户分不清「恢复了」与「一直没恢复」。
                        log::info!(
                            "Shell 扩展已恢复（此前连续失败 {fails} 次，期间对外上报无焦点），\
                             无需重启 focusd"
                        );
                        fails = 0;
                        throttle.reset();
                    }
                    // 同值会重复推送（250ms 一次），去重由主循环保证
                    let focus = Focus {
                        app_id: empty_to_none(&wm_class),
                        title: empty_to_none(&title),
                    };
                    let _ = tx.send(focus);
                }
                Err(e) => {
                    // 扩展被禁用 / Shell 重载中：不退出，降级继续轮询，
                    // 否则一次瞬时失败就把整个 daemon 打死。
                    //
                    // D2：无法确认焦点 → 立刻对外上报「无焦点」。
                    // **绝不保留上一次的值**——那正是「常驻但说谎」的根因。
                    // 主循环 Dedup 保证 FocusChanged("","") 只发射一次。
                    fails = fails.saturating_add(1);
                    let _ = tx.send(Focus::default());
                    match throttle.on_failure(0) {
                        Level::Warn => log::warn!(
                            "Shell 扩展轮询失败（第 {fails} 次）: {e}；对外上报无焦点。\
                             请确认扩展已启用：\
                             gnome-extensions enable focusd@rayc2026.github.io\
                             （无需重启 focusd）"
                        ),
                        _ => log::debug!("Shell 扩展轮询失败（第 {fails} 次）: {e}"),
                    }
                    // 达阈值重建 Proxy：扩展重新 enable 后 bus name 的属主是
                    // 另一个 unique name，旧 Proxy 可能一直指向已消失的属主。
                    if fails.is_multiple_of(after) {
                        match new_proxy(&conn) {
                            Ok(p) => proxy = p,
                            Err(pe) => log::debug!("Proxy 重建失败（扩展仍未上线）: {pe}"),
                        }
                    }
                }
            }
            std::thread::sleep(poll);
        }
    }
}

/// 建一个指向 Shell 扩展的 Proxy（抽成函数，便于失败后重建）。
///
/// 只重建 **Proxy**、不重建 **Connection**：`zbus::Connection` 是与 dbus-daemon
/// 的长连接，Shell 重载不影响它；而每 250ms 重建一次连接会持续泄漏 zbus 的
/// 内部 executor 线程——对常驻进程不可接受（架构 §3.5 取舍）。
fn new_proxy(conn: &blocking::Connection) -> Result<blocking::Proxy<'static>> {
    blocking::Proxy::new(conn, GNOME_BUS_NAME, GNOME_PATH, GNOME_IFACE).with_context(|| {
        "无法创建 Shell 扩展代理。请安装扩展：\
         将 packaging/gnome/focusd@rayc2026.github.io/ 复制到 \
         ~/.local/share/gnome-shell/extensions/，重载 Shell 后 \
         gnome-extensions enable focusd@rayc2026.github.io"
    })
}

/// 轮询间隔：`FOCUSD_POLL_MS`（默认 250ms）。
/// 下限 20ms 防止误配置成 0 打成 busy loop，上限 5s 防止形同关闭。
fn poll_interval() -> Duration {
    let ms = std::env::var("FOCUSD_POLL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250)
        .clamp(20, 5000);
    Duration::from_millis(ms)
}

/// 连续失败达多少次就重建 Proxy：`FOCUSD_GNOME_FAIL_AFTER`（默认 3）。
/// clamp 到 1..=20：0 会退化成每轮都重建，>20 则失去自愈意义。
const FAIL_AFTER_DEFAULT: u32 = 3;
const KEY_FAIL_AFTER: &str = "FOCUSD_GNOME_FAIL_AFTER";

/// 纯解析（便于单测；进程级 env 在并行用例里会互相污染）。
fn fail_after_from(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(u64::from(FAIL_AFTER_DEFAULT))
        .clamp(1, 20) as u32
}

fn fail_after() -> u32 {
    fail_after_from(std::env::var(KEY_FAIL_AFTER).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 失败阈值解析默认值与clamp() {
        assert_eq!(fail_after_from(None), FAIL_AFTER_DEFAULT);
        assert_eq!(fail_after_from(Some("1")), 1);
        // 0 会退化成每轮都重建 Proxy，clamp 到 1
        assert_eq!(fail_after_from(Some("0")), 1);
        assert_eq!(fail_after_from(Some("99")), 20);
        // 非法值回落默认
        assert_eq!(fail_after_from(Some("abc")), FAIL_AFTER_DEFAULT);
        assert_eq!(fail_after_from(Some(" 5 ")), 5);
    }
}
