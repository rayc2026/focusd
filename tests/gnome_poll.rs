//! GNOME 轮询契约 mock 测试（CI 无真 GNOME Shell）。
//!
//! 测试内起一个 mock `org.focusd.Gnome1` 服务（与 packaging/gnome
//! extension.js 的导出契约一致：GetFocus() -> (s wm_class, s title)），
//! 按脚本返回焦点序列，验证 GnomeBackend 的轮询与主循环去重行为。
//!
//! 需要会话总线：CI 中在 `dbus-run-session` 内运行（见 ci.yml）。

use std::sync::mpsc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zbus::blocking;
use zbus::interface;

use focusd::backend::{Dedup, GnomeBackend};

/// mock 扩展：GetFocus 按预置序列依次返回，超出后停在最后一项。
/// （真扩展返回的是实时焦点窗口；对轮询后端而言等价。）
struct MockExtension {
    seq: Mutex<Vec<(String, String)>>,
    calls: AtomicUsize,
}

#[interface(name = "org.focusd.Gnome1")]
impl MockExtension {
    fn get_focus(&self) -> (String, String) {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let seq = self.seq.lock().expect("seq 锁");
        let idx = i.min(seq.len().saturating_sub(1));
        seq[idx].clone()
    }
}

#[test]
fn gnome_轮询按序列输出且同值被去重() {
    // 缩短轮询间隔，加速测试（GnomeBackend 启动时读取一次）
    std::env::set_var("FOCUSD_POLL_MS", "40");

    // 1) 起 mock 扩展服务：firefox → foot（真机对应"用户切换窗口"）
    let mock = MockExtension {
        seq: Mutex::new(vec![
            ("firefox".to_string(), "Mozilla Firefox".to_string()),
            ("foot".to_string(), "~".to_string()),
        ]),
        calls: AtomicUsize::new(0),
    };
    let mock_conn = blocking::connection::Builder::session()
        .expect("mock 连接失败")
        .name("org.focusd.Gnome1")
        .expect("mock 请求 bus name 失败")
        .serve_at("/org/focusd/Gnome", mock)
        .expect("mock serve_at 失败")
        .build()
        .expect("mock 服务启动失败");
    let _keep = mock_conn; // 保活

    // 2) 被测对象：真实 GnomeBackend 轮询 mock
    let (tx, rx) = mpsc::channel();
    let backend = GnomeBackend;
    std::thread::spawn(move || {
        // 轮询循环常驻；测试进程结束时线程被回收
        let _ = backend.run(tx);
    });

    // 3) 收集去重后的快照：应恰好 firefox → foot 两个
    let mut dedup = Dedup::new();
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < 2 && Instant::now() < deadline {
        if let Ok(focus) = rx.recv_timeout(Duration::from_millis(500)) {
            if let Some(f) = dedup.install(focus) {
                out.push(f);
            }
        }
    }

    assert_eq!(out.len(), 2, "应恰好输出两个去重后的快照");
    assert_eq!(out[0].app_id.as_deref(), Some("firefox"));
    assert_eq!(out[0].title.as_deref(), Some("Mozilla Firefox"));
    assert_eq!(out[1].app_id.as_deref(), Some("foot"));
    assert_eq!(out[1].title.as_deref(), Some("~"));
}
