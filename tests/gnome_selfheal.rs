//! GNOME 轮询**自愈**契约 mock 测试（迭代三 · T04）。
//!
//! CI 上没有真 GNOME Shell，这里起一个 mock `org.focusd.Gnome1`
//! （与 `packaging/gnome` extension.js 的导出契约一致），然后**把它撤掉**
//! 来模拟 `gnome-extensions disable` / Shell 重载。
//!
//! 要根治的缺陷（架构 §2.3）：旧实现在 `run()` 里只建一次 `Proxy`，
//! 扩展消失后只打 debug 日志、**不推任何快照**，于是 `GetFocus()` 返回
//! **陈旧值**——「常驻但说谎」。本测试断言的是相反的行为：
//!
//! 1. 扩展消失 → 通道立刻收到 `Focus::default()`（对外 `ss "" ""`）；
//! 2. 日志出现含 `gnome-extensions enable focusd@rayc2026.github.io` 的 WARN；
//! 3. 扩展重新上线 → **无需重启 focusd** 即恢复推送真实快照，且有 INFO 可区分。
//!
//! 三个阶段串在同一个 `#[test]` 里：`run()` 的轮询循环没有停机钩子，
//! 拆成多个用例会让上一个用例遗留的后端线程继续打下一个用例的 mock，
//! 断言互相污染（尤其是「已恢复」这类日志断言）。
//!
//! 需要会话总线：CI 中在 `dbus-run-session` 内运行（见 ci.yml）。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::blocking;
use zbus::interface;

use focusd::backend::gnome::{GNOME_BUS_NAME, GNOME_PATH};
use focusd::backend::{Backend, Focus, GnomeBackend};

/// 捕获到的日志序列（级别 + 文案）。抽成别名是为了压住 clippy 的
/// `type_complexity`。
type Records = Arc<Mutex<Vec<(log::Level, String)>>>;

/// mock 扩展：恒定返回一个焦点快照。
/// （真扩展返回实时焦点；对「端点消失 → 重现」这条自愈链路而言等价。）
struct MockExtension;

#[interface(name = "org.focusd.Gnome1")]
impl MockExtension {
    fn get_focus(&self) -> (String, String) {
        ("firefox".to_string(), "Mozilla Firefox".to_string())
    }
}

fn start_mock() -> blocking::Connection {
    blocking::connection::Builder::session()
        .expect("mock 连接会话总线失败（是否在 dbus-run-session 内？）")
        .name(GNOME_BUS_NAME)
        .expect("mock 请求 org.focusd.Gnome1 失败")
        .serve_at(GNOME_PATH, MockExtension)
        .expect("mock serve_at 失败")
        .build()
        .expect("mock 服务启动失败")
}

struct CaptureLogger {
    records: Records,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, _meta: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.records
            .lock()
            .expect("records 锁")
            .push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

fn capture_logs() -> Records {
    static CELL: std::sync::OnceLock<Records> = std::sync::OnceLock::new();
    Arc::clone(CELL.get_or_init(|| {
        let rec: Records = Arc::new(Mutex::new(Vec::new()));
        let _ = log::set_boxed_logger(Box::new(CaptureLogger { records: Arc::clone(&rec) }));
        log::set_max_level(log::LevelFilter::Debug);
        rec
    }))
}

fn wait_log(rec: &Records, level: log::Level, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let hit = rec
            .lock()
            .expect("records 锁")
            .iter()
            .any(|(l, m)| *l == level && m.contains(needle));
        if hit || Instant::now() >= deadline {
            return hit;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 等到通道里出现 `app_id == want` 的快照（`None` 即无焦点快照）。
fn wait_focus(rx: &mpsc::Receiver<Focus>, want: Option<&str>, secs: u64) -> Focus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(f) if f.app_id.as_deref() == want => return f,
            Ok(_) => {}
            Err(_) if Instant::now() >= deadline => {
                panic!("等待超时：期望 app_id={want:?} 的快照")
            }
            Err(_) => {}
        }
    }
}

#[test]
fn gnome_扩展消失推无焦点_重现后无需重启即恢复() {
    // 缩短轮询间隔与 Proxy 重建阈值，让整条自愈链路在 CI 时间预算内跑完。
    std::env::set_var("FOCUSD_POLL_MS", "40");
    std::env::set_var("FOCUSD_GNOME_FAIL_AFTER", "2");
    let rec = capture_logs();
    rec.lock().expect("records 锁").clear();

    // ---- ① 正常上报 ----
    let mock = start_mock();
    let (tx, rx) = mpsc::channel();
    let backend = GnomeBackend;
    std::thread::spawn(move || {
        let _ = backend.run(tx);
    });
    let first = wait_focus(&rx, Some("firefox"), 10);
    assert_eq!(first.title.as_deref(), Some("Mozilla Firefox"));

    // ---- ② 扩展消失（模拟 gnome-extensions disable / Shell 重载）----
    rec.lock().expect("records 锁").clear();
    drop(mock);
    let degraded = wait_focus(&rx, None, 10);
    assert_eq!(
        degraded,
        Focus::default(),
        "扩展消失必须推无焦点快照（旧实现保留陈旧值 = 常驻但说谎，D2）"
    );
    assert!(
        wait_log(&rec, log::Level::Warn, "gnome-extensions enable focusd@rayc2026.github.io", 10),
        "降级期间必须有含 gnome-extensions enable 指引的 WARN（真机排障第一现场）"
    );
    assert!(
        wait_log(&rec, log::Level::Warn, "无需重启 focusd", 5),
        "降级提示必须明确‘无需重启 focusd’"
    );

    // ---- ③ 重新 enable：无需重启 focusd 即恢复，且恢复可见 ----
    rec.lock().expect("records 锁").clear();
    let _mock2 = start_mock();
    let back = wait_focus(&rx, Some("firefox"), 10);
    assert_eq!(back.title.as_deref(), Some("Mozilla Firefox"), "恢复后应上报真实快照");
    assert!(
        wait_log(&rec, log::Level::Info, "已恢复", 10),
        "恢复必须打 INFO 日志（否则用户分不清‘恢复了’与‘一直没恢复’）"
    );
}
