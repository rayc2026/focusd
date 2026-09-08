//! KDE 契约 mock 测试（CI 无真 Plasma/KWin）。
//!
//! 验证 KWin 脚本将使用的 D-Bus 契约：KWin 侧 `callDBus(
//! "org.focusd.Focus1", "/org/focusd/Focus1", "org.focusd.Focus1.Kwin",
//! "Report", app_id, title)` 推送后——
//! 1. 事件进入主循环通道；
//! 2. 同值重发被主循环 Dedup 过滤（与 packaging/kde 脚本行为对齐）。
//!
//! 需要会话总线：CI 中在 `dbus-run-session` 内运行（见 ci.yml）。

use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use zbus::blocking;

use focusd::backend::Dedup;
use focusd::dbus;

#[test]
fn kwin_report_推送进入通道且同值重发被去重() {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(RwLock::new(None));

    // 与 serve 相同的服务组装入口（保证契约测试覆盖的就是生产路径）
    let conn = dbus::start_serve(state, tx).expect("起 D-Bus 服务失败");
    let _keep = conn; // 保活，撑住 ObjectServer

    // 模拟 KWin callDBus：接口/路径/方法/参数与 main.js 完全一致
    let client = blocking::Connection::session().expect("client 连接失败");
    let kwin = blocking::Proxy::new(
        &client,
        "org.focusd.Focus1",
        "/org/focusd/Focus1",
        "org.focusd.Focus1.Kwin",
    )
    .expect("创建 KWin 推送代理失败");

    kwin.call::<_, _, ()>("Report", &("firefox", "Mozilla Firefox")).expect("Report 调用失败");
    kwin.call::<_, _, ()>("Report", &("firefox", "Mozilla Firefox")).expect("Report 调用失败");
    kwin.call::<_, _, ()>("Report", &("foot", "~")).expect("Report 调用失败");

    // 3 次推送 → 3 次入通道；过 Dedup 后应只剩 2 个快照
    let mut dedup = Dedup::new();
    let mut out = Vec::new();
    for _ in 0..3 {
        let focus = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("等待 Report 推送超时");
        if let Some(f) = dedup.install(focus) {
            out.push(f);
        }
    }

    assert_eq!(out.len(), 2, "同值重发应被去重");
    assert_eq!(out[0].app_id.as_deref(), Some("firefox"));
    assert_eq!(out[0].title.as_deref(), Some("Mozilla Firefox"));
    assert_eq!(out[1].app_id.as_deref(), Some("foot"));

    // 空串推送映射为 None（KWin 对"无窗口"发空串）
    kwin.call::<_, _, ()>("Report", &("", "")).expect("Report 调用失败");
    let focus = rx.recv_timeout(Duration::from_secs(5)).expect("等待空推送超时");
    assert_eq!(focus.app_id, None);
    assert_eq!(focus.title, None);
}
