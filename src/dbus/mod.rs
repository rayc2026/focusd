//! D-Bus 门面：`org.focusd.Focus1` 服务（serve 模式）。
//!
//! 一个 zbus blocking connection 同时承担两个角色（见架构文档 §1.2）：
//! - **server**：`org.focusd.Focus1.GetFocus()` 返回当前焦点快照；
//!   `org.focusd.Focus1.Kwin.Report()` 是 KWin 脚本的推送入口（KDE 后端）；
//! - **signal 源**：焦点真正变化（去重后）由 serve 主循环发 `FocusChanged`。
//!
//! 线程模型：ObjectServer 由 zbus 内部线程驱动，主循环独占写
//! `Arc<RwLock<Option<Focus>>>`，KwinReportIface 只往 channel 里推——
//! 所有事件源（Wayland / KWin 推送 / GNOME 轮询）汇入同一条通道，
//! 去重与"先写状态、后发信号"的顺序都由主循环保证。

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use zbus::interface;

use crate::backend::{Focus, empty_to_none};

/// 总线名。未来如需 fd.o namespaced 名称，迁移到 `io.github.rayc2026.focusd`
/// （见 PRD 待确认问题 1，docs/dbus.md 有迁移说明）。
pub const BUS_NAME: &str = "org.focusd.Focus1";
/// 对象路径：焦点接口与 KWin 推送接口共用。
pub const PATH: &str = "/org/focusd/Focus1";
/// 焦点查询接口名。
pub const IFACE_FOCUS: &str = "org.focusd.Focus1";
/// KWin 脚本推送接口名（KdeBackend 与 packaging/kde 的脚本依赖此约定）。
pub const IFACE_KWIN: &str = "org.focusd.Focus1.Kwin";

/// `org.focusd.Focus1`：对外只读的焦点快照。
///
/// GetFocus 把 `Focus` 两字段直接映射为 `(s app_id, s title)`；
/// `None` 序列化为空串（D-Bus 无 null 字符串，docs/dbus.md 有说明）。
pub struct FocusIface {
    state: Arc<RwLock<Option<Focus>>>,
}

#[interface(name = "org.focusd.Focus1")]
impl FocusIface {
    fn get_focus(&self) -> (String, String) {
        let guard = self.state.read().expect("state RwLock 中毒");
        match guard.as_ref() {
            Some(f) => (
                f.app_id.clone().unwrap_or_default(),
                f.title.clone().unwrap_or_default(),
            ),
            None => (String::new(), String::new()),
        }
    }
}

/// `org.focusd.Focus1.Kwin`：KWin 脚本 `callDBus` 的推送入口。
///
/// Report 不直接写状态——推入与 Wayland/轮询相同的 channel，
/// 去重与信号发射统一由 serve 主循环处理（单一事实来源）。
pub struct KwinReportIface {
    // mpsc::Sender 不是 Sync，而 zbus Interface 要求 Sync，用 Mutex 包一层；
    // 锁竞争点只有 KWin 推送瞬间，可忽略。
    tx: Mutex<Sender<Focus>>,
}

#[interface(name = "org.focusd.Focus1.Kwin")]
impl KwinReportIface {
    fn report(&self, app_id: &str, title: &str) {
        let focus = Focus {
            app_id: empty_to_none(app_id),
            title: empty_to_none(title),
        };
        // 对端（主循环）还在就一定发得出去；发不出去也只是丢一次推送，
        // 不应让 D-Bus 方法报错打挂 KWin 侧脚本。
        if let Err(e) = self.tx.lock().expect("tx Mutex 中毒").send(focus) {
            log::warn!("Kwin.Report 推送入通道失败（主循环已退出？）: {e}");
        }
    }
}

/// 组装完整 D-Bus 服务：session bus 连接 + bus name + 两个接口注册。
///
/// 任何一步失败都直接返回 Err（serve 启动失败要明确退出，不重试不挂死）。
/// 返回的 `Connection` 必须由调用方保活——它撑着 ObjectServer 的内部线程。
pub fn start_serve(
    state: Arc<RwLock<Option<Focus>>>,
    kwin_tx: Sender<Focus>,
) -> Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(PATH, FocusIface { state })?
        .serve_at(PATH, KwinReportIface { tx: Mutex::new(kwin_tx) })?
        .build()?;
    Ok(conn)
}

/// 仅注册 Kwin 推送入口（`focusd watch --backend kde` 的 watch 模式）：
/// 该模式下没有 serve 主循环与状态中心，但 KWin 脚本的推送仍需有人接收。
/// `FocusIface` / FocusChanged 属于 serve 语义，watch 模式不承诺。
pub fn start_kwin_report(kwin_tx: Sender<Focus>) -> Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(PATH, KwinReportIface { tx: Mutex::new(kwin_tx) })?
        .build()?;
    Ok(conn)
}
