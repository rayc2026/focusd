//! KDE KWin 脚本**健康检测 + 自动重注册**契约 mock 测试（迭代三 · T03）。
//!
//! CI 上没有真 Plasma/KWin，这里起一个 mock `org.kde.KWin`（`/Scripting`
//! + `/Scripting/Script7`），按脚本驱动 `isScriptLoaded` / `loadScript` 的
//! 返回值，验证架构 §4.3 的整条链路：
//!
//! 1. `isScriptLoaded=true` 时**不重复** `loadScript`（幂等，且双名探测）；
//! 2. 脚本失效 → 健康循环自动 `loadScript` + `Script.run()`（**无需重启 focusd**）；
//! 3. 失效瞬间对外推 `Focus::default()`（D2：绝不保留陈旧值）；
//! 4. `org.kde.KWin` bus name 消失 → 推无焦点并等待；重现 → 打 INFO 恢复日志；
//! 5. 重载失败（`loadScript` 返回 `-2`）→ 持续上报无焦点 + WARN 含可操作指引。
//!
//! 为什么把这些场景放在**同一个** `#[test]` 里：mock 必须先 own
//! `org.kde.KWin`，而 `run_with` 的健康循环是无限循环、没有停机钩子——
//! 拆成多个用例会让上一个用例遗留的后端线程继续打下一个用例的 mock，
//! 断言互相污染。整条流程串起来跑，一次只有一个后端线程在场。
//!
//! 需要会话总线：CI 中在 `dbus-run-session` 内运行（见 ci.yml）。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use zbus::blocking;
use zbus::interface;

use focusd::backend::kde::{KdeBackend, KwinCtl, PKG_PLUGIN_ID, PLUGIN_NAME};
use focusd::backend::Focus;
use focusd::dbus;

const KWIN_SERVICE: &str = "org.kde.KWin";
/// mock `loadScript` 返回的脚本 id；与 `MOCK_SCRIPT_PATH` 手动保持一致。
const MOCK_SCRIPT_ID: i32 = 7;
const MOCK_SCRIPT_PATH: &str = "/Scripting/Script7";

// ---------------------------------------------------------------------------
// mock：org.kde.KWin /Scripting + /Scripting/Script7
// ---------------------------------------------------------------------------

/// 测试进程与 mock 共享的可变状态（mock 被 move 进 ObjectServer，
/// 只能靠 `Arc` 内部字段与测试侧通信）。
#[derive(Clone, Default)]
struct MockState {
    /// `isScriptLoaded` 的返回值：置 false 即模拟「脚本失效」。
    loaded: Arc<AtomicBool>,
    /// `loadScript` 的返回值：负数（除 -1）模拟真失败。
    next_id: Arc<AtomicI32>,
    loads: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
    /// `isScriptLoaded` 被问过的 pluginName（用于断言双名探测）。
    probes: Arc<Mutex<Vec<String>>>,
    /// `loadScript` 收到的 (filePath, pluginName)。
    load_args: Arc<Mutex<Vec<(String, String)>>>,
}

struct MockScripting {
    st: MockState,
}

#[interface(name = "org.kde.kwin.Scripting")]
impl MockScripting {
    #[zbus(name = "isScriptLoaded")]
    fn is_script_loaded(&self, plugin_name: &str) -> bool {
        self.st.probes.lock().expect("probes 锁").push(plugin_name.to_string());
        self.st.loaded.load(Ordering::SeqCst)
    }

    #[zbus(name = "loadScript")]
    fn load_script(&self, file_path: &str, plugin_name: &str) -> i32 {
        self.st
            .load_args
            .lock()
            .expect("load_args 锁")
            .push((file_path.to_string(), plugin_name.to_string()));
        self.st.loads.fetch_add(1, Ordering::SeqCst);
        let id = self.st.next_id.load(Ordering::SeqCst);
        // 真 KWin：非负 id 表示装载成功，此后 isScriptLoaded 为真。
        if id >= 0 {
            self.st.loaded.store(true, Ordering::SeqCst);
        }
        id
    }
}

struct MockScript {
    runs: Arc<AtomicUsize>,
}

#[interface(name = "org.kde.kwin.Script")]
impl MockScript {
    #[zbus(name = "run")]
    fn run(&self) {
        self.runs.fetch_add(1, Ordering::SeqCst);
    }
}

fn start_mock(st: &MockState) -> blocking::Connection {
    blocking::connection::Builder::session()
        .expect("mock 连接会话总线失败（是否在 dbus-run-session 内？）")
        .name(KWIN_SERVICE)
        .expect("mock 请求 org.kde.KWin 失败")
        .serve_at("/Scripting", MockScripting { st: st.clone() })
        .expect("serve /Scripting 失败")
        .serve_at(MOCK_SCRIPT_PATH, MockScript { runs: Arc::clone(&st.runs) })
        .expect("serve Script 对象失败")
        .build()
        .expect("mock 服务启动失败")
}

// ---------------------------------------------------------------------------
// 日志捕获：warn 文案是给真机用户的可操作指引，必须可断言
// ---------------------------------------------------------------------------

struct CaptureLogger {
    records: Arc<Mutex<Vec<(log::Level, String)>>>,
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

fn capture_logs() -> Arc<Mutex<Vec<(log::Level, String)>>> {
    static CELL: std::sync::OnceLock<Arc<Mutex<Vec<(log::Level, String)>>>> =
        std::sync::OnceLock::new();
    Arc::clone(CELL.get_or_init(|| {
        let rec = Arc::new(Mutex::new(Vec::new()));
        // 只装一次；若被人抢先则沿用其记录器（本二进制内无其它 logger）。
        let _ = log::set_boxed_logger(Box::new(CaptureLogger { records: Arc::clone(&rec) }));
        log::set_max_level(log::LevelFilter::Debug);
        rec
    }))
}

/// 轮询等待一条匹配级别与关键字的日志出现。
fn wait_log(
    rec: &Arc<Mutex<Vec<(log::Level, String)>>>,
    level: log::Level,
    needle: &str,
    secs: u64,
) -> bool {
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

/// 轮询等待条件成立。
fn wait_until<F: Fn() -> bool>(f: F, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() || Instant::now() >= deadline {
            return f();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// 契约测试
// ---------------------------------------------------------------------------

#[test]
fn kwin_健康检测_已加载不重复加载_失效自动重注册_消失重现_重载失败降级() {
    // 缩短健康检测周期（生产默认 10s），让整条流程在 CI 时间预算内跑完。
    std::env::set_var("FOCUSD_KWIN_HEALTH_MS", "1000");
    let rec = capture_logs();

    let st = MockState::default();
    st.loaded.store(true, Ordering::SeqCst);
    st.next_id.store(MOCK_SCRIPT_ID, Ordering::SeqCst);
    let kwin = start_mock(&st);

    // ---- ① 已加载：`load_script` 必须幂等，且双名探测 ----
    rec.lock().expect("records 锁").clear();
    KdeBackend.load_script().expect("已加载场景 load_script 应成功");
    assert_eq!(
        st.loads.load(Ordering::SeqCst),
        0,
        "isScriptLoaded=true 时不应再调 loadScript（旧实现会对 kpackagetool6 装过的实例误报失败）"
    );
    let probes = st.probes.lock().expect("probes 锁").clone();
    assert!(probes.contains(&PLUGIN_NAME.to_string()), "应探测 pluginName `{PLUGIN_NAME}`");
    assert!(
        probes.contains(&PKG_PLUGIN_ID.to_string()),
        "应双名探测 `{PKG_PLUGIN_ID}`（kpackagetool6 的 KPlugin.Id，见架构 U2）"
    );

    // ---- ② 起健康循环：先占住 org.focusd.Focus1，让 run_with 走 serve 分支 ----
    let (tx, rx) = mpsc::channel();
    let state: Arc<RwLock<Option<Focus>>> = Arc::new(RwLock::new(None));
    let _serve = dbus::start_serve(Arc::clone(&state), tx.clone()).expect("start_serve 失败");
    let ctl = KwinCtl::connect().expect("KwinCtl::connect 失败");
    std::thread::spawn(move || {
        let _ = KdeBackend.run_with(tx, ctl);
    });
    assert_eq!(st.loads.load(Ordering::SeqCst), 0, "启动期已加载则不应重复加载");

    // ---- ③ 脚本失效 → 自动重新注册（核心：无需重启 focusd）----
    rec.lock().expect("records 锁").clear();
    st.loaded.store(false, Ordering::SeqCst);
    assert!(
        wait_until(|| st.loads.load(Ordering::SeqCst) >= 1, 20),
        "脚本失效后健康循环应自动 loadScript 重新注册"
    );
    assert!(
        st.runs.load(Ordering::SeqCst) >= 1,
        "重新注册后必须 run()：脚本重跑 main.js 才会补推当前焦点"
    );
    let args = st.load_args.lock().expect("load_args 锁").clone();
    assert_eq!(
        args.last().map(|a| a.1.as_str()),
        Some(PLUGIN_NAME),
        "loadScript 的 pluginName 必须是 `{PLUGIN_NAME}`"
    );
    // D2：失效瞬间推无焦点，绝不保留陈旧值
    let got = rx.recv_timeout(Duration::from_secs(5)).expect("失效瞬间应推无焦点快照");
    assert_eq!(got, Focus::default(), "失效期间必须对外上报无焦点（D2）");
    assert!(
        wait_log(&rec, log::Level::Info, "自动重新注册", 10),
        "重新注册成功应打 INFO 日志（含‘无需重启 focusd’）"
    );
    // 等健康循环把脚本确认为已加载（degraded 复位），否则下一阶段不会再推无焦点
    assert!(wait_log(&rec, log::Level::Info, "已恢复在线", 10), "重载后应回到健康态");

    // ---- ④ org.kde.KWin bus name 消失 → 推无焦点并等待，不重试加载 ----
    rec.lock().expect("records 锁").clear();
    let loads_before = st.loads.load(Ordering::SeqCst);
    drop(kwin);
    let got = rx.recv_timeout(Duration::from_secs(20)).expect("KWin 不在线应推无焦点快照");
    assert_eq!(got, Focus::default(), "KWin 不在线不能保留陈旧值（D2）");
    // 不在线时只等待，不该疯狂重试 loadScript
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        st.loads.load(Ordering::SeqCst),
        loads_before,
        "KWin 不在线时不应重试 loadScript（加载必然失败，只该等）"
    );
    assert!(
        wait_log(&rec, log::Level::Warn, "org.kde.KWin 不在线", 10),
        "KWin 不在线应有 WARN 日志（真机排障的第一现场）"
    );

    // ---- ⑤ bus name 重现 → 无需重启 focusd 即恢复，且有 INFO 可区分 ----
    rec.lock().expect("records 锁").clear();
    let _kwin2 = start_mock(&st);
    assert!(
        wait_log(&rec, log::Level::Info, "已恢复在线", 20),
        "KWin 重现后应打 INFO 恢复日志（让用户能区分‘恢复了’与‘一直没恢复’）"
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(rx.try_recv().is_err(), "恢复后不应再推无焦点快照");

    // ---- ⑥ 重载失败（loadScript 返回 -2）→ 持续上报无焦点 + 可操作 WARN ----
    rec.lock().expect("records 锁").clear();
    st.loaded.store(false, Ordering::SeqCst);
    st.next_id.store(-2, Ordering::SeqCst); // -2 = 真失败（区别于 -1 = 已加载）
    rec.lock().expect("records 锁").clear();
    let got = rx.recv_timeout(Duration::from_secs(20)).expect("重载失败应上报无焦点快照");
    assert_eq!(got, Focus::default(), "重载失败期间 GetFocus 必须返回空串（D2）");
    assert!(
        wait_log(&rec, log::Level::Warn, "kpackagetool6", 15),
        "重载失败 WARN 必须含 kpackagetool6 重装指引"
    );
    assert!(
        wait_log(&rec, log::Level::Warn, "系统设置", 5),
        "重载失败 WARN 必须含 系统设置 → 窗口管理 → KWin 脚本 指引"
    );
    assert!(
        wait_log(&rec, log::Level::Warn, "无需重启 focusd", 5),
        "降级提示必须明确‘无需重启 focusd’"
    );
}
