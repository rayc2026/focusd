use std::collections::HashMap;
use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::reconnect::{
    no_endpoint, ConnectMode, Connector, RealSleeper, ReconnectConfig, Session, Supervisor,
};
use super::selector::{probe_wlroots, RealProbe};
use super::{Backend, Focus};

/// 后端人可读名（`Backend::name()` 与日志共用）。
const NAME: &str = "wlroots (zwlr-foreign-toplevel-management-unstable-v1)";

/// 首次连接失败的错误文案（D1：配置错误就该明确报错退出）。
///
/// 与 v0.2.0 逐字一致——CI「无 compositor 时 serve 明确报错退出」的断言
/// 与用户的既有排障习惯都依赖它，**不得改写**。
const INIT_ERR: &str = "无法连接 Wayland display。请确认 WAYLAND_DISPLAY 已设置且在 Wayland 会话中";

/// zwlr_foreign_toplevel_handle_v1.state 里 activated 的枚举值。
/// 协议定义：maximized=0、minimized=1、activated=2。之前误写成 1
/// （恰好是 minimized），导致 activated 永远检测不到——而旧 CI 断言
/// 又因 RUST_LOG=debug 的日志行里恰好含 app_id 字样而假绿；
/// gate 转正 + serve 的 GetFocus 断言把这个 bug 暴露了出来。
const STATE_ACTIVATED: u32 = 2;

#[derive(Default)]
struct Toplevel {
    app_id: Option<String>,
    title: Option<String>,
    activated: bool,
}

struct State {
    toplevels: HashMap<ObjectId, Toplevel>,
    tx: Sender<Focus>,
    last: Option<Focus>,
}

impl State {
    /// 新建会话状态。
    ///
    /// **每次 `connect` 都新建一个 `State`**：toplevel 表与 `last` 天然重置，
    /// 保证重连后不会拿旧会话的窗口表去算焦点（旧表里全是已消失的窗口）。
    fn new(tx: Sender<Focus>) -> Self {
        Self { toplevels: HashMap::new(), tx, last: None }
    }

    /// 找出当前处于 activated 状态的窗口。
    ///
    /// wlroots 协议允许多个 toplevel 同时上报，但任一时刻通常只有一个
    /// 被激活。这里取第一个匹配项，与 sway/hyprland 的实际行为一致。
    fn current(&self) -> Option<Focus> {
        self.toplevels
            .values()
            .find(|t| t.activated)
            .map(|t| Focus {
                app_id: t.app_id.clone(),
                title: t.title.clone(),
            })
    }

    /// 只在焦点真正发生变化时推送，避免刷屏。
    ///
    /// 注意：焦点消失（最后一个窗口关闭、current() 返回 `None`）**同样要
    /// 推送**——`Focus` 的 app_id/title 均为 `None`，下游 serve 的
    /// GetFocus 会返回空串（见 docs/dbus.md 契约）。若在此拦截 None，
    /// 消费方将永远读到陈旧的焦点值（迭代二 QA 发现的 L1 缺陷）。
    fn emit_if_changed(&mut self) {
        let cur = self.current();
        if cur == self.last {
            return;
        }
        let _ = self.tx.send(cur.clone().unwrap_or_default());
        self.last = cur;
    }
}

// ---------------------------------------------------------------------------
// 会话化：WlConnector（连接工厂） + WlSession（事件泵）
// ---------------------------------------------------------------------------

/// 一条已建立的 wlroots 会话：连接产物 + 事件泵。
///
/// 实现 [`Session`]，交给 [`Supervisor`] 驱动；`pump()` 返回 `Err` 即
/// 「会话已断，请重连」。
pub struct WlSession {
    /// **必须持有 manager 对象**：一旦 drop，客户端会向服务端发 release，
    /// 之后再也收不到 toplevel 事件（v0.2.0 里它靠 `run()` 的局部变量存活，
    /// 语义等价）。字段名带下划线前缀：它只用于保活，不被读取。
    _manager: ZwlrForeignToplevelManagerV1,
    event_queue: EventQueue<State>,
    state: State,
}

impl Session for WlSession {
    /// 阻塞派发一批事件。
    ///
    /// U1：compositor 被 SIGKILL 后，对端 socket 关闭 → 本端读到 EOF，
    /// `blocking_dispatch` 返回 `Err`（IO 错误），`Supervisor` 随即进入重连。
    /// 该假设由 CI 的 L1 断言端到端证伪（kill 后 serve 存活 + GetFocus 空串）；
    /// 若将来观察到「进程存活但焦点永久陈旧」——即 pump 永久阻塞——
    /// 这里要换成 `prepare_read()` + `libc::poll` 检测 `POLLHUP` 的备选实现
    /// （`libc` 已在依赖里，不算新增）。
    fn pump(&mut self) -> Result<()> {
        self.event_queue.blocking_dispatch(&mut self.state)?;
        Ok(())
    }
}

/// wlroots 后端的连接工厂。
///
/// 首次连接**完全复用** `Connection::connect_to_env()`（零回归）；
/// 重连时重解析候选 socket 并逐个用「建 registry + bind manager」校验。
pub struct WlConnector {
    /// 每次 `connect` 都要把一个 `Sender` 交给新的 `State`。
    /// `mpsc::Sender` 不保证 `Sync`，而 [`Connector`] 要求 `Send + Sync`，
    /// 因此加一层 `Mutex`（只在 connect 时短暂取锁克隆一次）。
    tx: Mutex<Sender<Focus>>,
}

impl WlConnector {
    pub fn new(tx: Sender<Focus>) -> Self {
        Self { tx: Mutex::new(tx) }
    }

    /// 取一份 `Sender` 克隆。锁中毒时取回内部值（通道本身仍然可用）。
    fn sender(&self) -> Sender<Focus> {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 拿到 `Connection` 之后的公共部分：初始化 registry、校验协议、组装会话。
    ///
    /// **bind 校验就是「这个 socket 背后是不是 wlroots」的判据**：非 wlroots 系
    /// （GNOME / KDE / 裸 Weston 等）拿不到 `zwlr_foreign_toplevel_manager_v1`，
    /// bind 直接失败 → 调用方换下一个候选（U4 的主要缓解手段）。
    fn init(&self, conn: Connection) -> Result<Box<dyn Session>> {
        let (globals, event_queue) =
            registry_queue_init(&conn).context("无法初始化 Wayland registry")?;
        let qh = event_queue.handle();

        // TODO(协议版本协商)：固定绑定 3..=3，未做协商（主理人裁定 4 登记，
        // 不在本次迭代范围）。遇到只支持 v1/v2 的旧 compositor 会绑定失败；
        // 若将来要做，应按 globals 列表里该 interface 的实际 version 放宽区间。
        //
        // 版本 3 是 wlroots 各 compositor 普遍支持的版本。
        // 若绑定失败，说明当前 compositor 不是 wlroots 系（如 GNOME / KDE），
        // 需要走其它后端。
        let manager: ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .context("当前 compositor 未实现 zwlr_foreign_toplevel_manager_v1。\
                     本后端仅支持 wlroots 系（Sway / Hyprland / river / labwc 等）。\
                     GNOME / KDE 支持尚在实现中")?;

        log::info!("已连接 {NAME}");

        Ok(Box::new(WlSession {
            _manager: manager,
            event_queue,
            state: State::new(self.sender()),
        }))
    }

    /// 连接一个具体路径的 socket（重连候选）。
    fn connect_path(&self, path: &Path) -> Result<Box<dyn Session>> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("连接 Wayland socket {} 失败", path.display()))?;
        let conn = Connection::from_socket(stream)
            .with_context(|| format!("从 socket {} 建立 Wayland 连接失败", path.display()))?;
        self.init(conn)
    }
}

impl Connector for WlConnector {
    fn connect(&self, mode: ConnectMode) -> Result<Box<dyn Session>> {
        match mode {
            // 首次：与 v0.2.0 逐字节一致——不扫描、不重解析。
            // 扫描只在「已经连上过、现在断了」时启用，避免首次就误连到
            // WSLg 的 Weston / 另一个 wlroots 实例，把错误文案搞乱。
            ConnectMode::Initial => {
                let conn = Connection::connect_to_env().context(INIT_ERR)?;
                self.init(conn)
            }
            // 重连：重解析候选，逐个用「建 registry + bind manager」校验。
            ConnectMode::Reconnect => {
                let candidates = candidate_sockets();
                let mut last: Option<anyhow::Error> = None;
                for path in &candidates {
                    match self.connect_path(path) {
                        Ok(session) => {
                            log::debug!("重连候选 {} 可用", path.display());
                            return Ok(session);
                        }
                        Err(e) => {
                            log::debug!("重连候选 {} 不可用: {e:#}", path.display());
                            last = Some(e);
                        }
                    }
                }
                let listed = if candidates.is_empty() {
                    "<无>".to_string()
                } else {
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let err = match last {
                    Some(e) => anyhow!(
                        "已尝试全部候选 socket（{listed}）均无可用 \
                         zwlr_foreign_toplevel_manager_v1；最后错误: {e:#}"
                    ),
                    None => anyhow!("未找到任何候选 socket（XDG_RUNTIME_DIR 缺失或已清空）"),
                };
                // 「无候选 / 候选全废」是极廉价的探测（一次 readdir + 若干 connect），
                // 标记成探测类失败 → Supervisor 按 discover_max 封顶退避，
                // 避免 30s 上限拖慢恢复（PRD 要求 ≤30s 内恢复）。
                Err(no_endpoint(err))
            }
        }
    }

    fn label(&self) -> &'static str {
        "wlroots"
    }

    fn hint(&self) -> &'static str {
        "请检查 WAYLAND_DISPLAY 是否仍指向有效的 compositor，\
         以及该 compositor 是否支持 zwlr-foreign-toplevel-management-unstable-v1\
         （Sway / Hyprland / river / labwc 等）"
    }
}

// ---------------------------------------------------------------------------
// socket 重解析（Q5 结论，架构 §3.3）
// ---------------------------------------------------------------------------

/// 重连时的候选 socket 列表（按优先级排序，已去重）。
///
/// 顺序：
/// 1. 重读 `WAYLAND_DISPLAY`（绝对路径直接用；相对名拼到 runtime dir 下）；
/// 2. 扫描 runtime dir 下 `wayland-*`，按 **mtime 倒序**
///    （SIGKILL 会留下残留 socket 文件，新 compositor 必然换号，
///    `wayland-1` 残留 → 新实例只能用 `wayland-2`，故必须扫描而非只认 env）。
///
/// 不用 inotify 监听目录：常驻进程不值得为此背一个监听子系统，
/// 2s 级探测已够（且 CPU 开销可忽略）。
pub fn candidate_sockets() -> Vec<PathBuf> {
    candidate_sockets_from(&|k: &str| std::env::var(k).ok())
}

/// 可注入环境的 [`candidate_sockets`] 主体（单测入口）。
fn candidate_sockets_from<F>(get: &F) -> Vec<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out: Vec<PathBuf> = Vec::new();

    // 1) env 候选永远排第一：用户显式指定的端点优先于扫描结果（U4 缓解）。
    if let Some(display) = get("WAYLAND_DISPLAY").filter(|v| !v.trim().is_empty()) {
        let p = PathBuf::from(display);
        if p.is_absolute() {
            push_unique(&mut out, p);
        } else if let Some(rt) = runtime_dir(get) {
            push_unique(&mut out, rt.join(p));
        }
    }

    // 2) 扫描：mtime 倒序，新 compositor 的 socket 更新。
    if let Some(rt) = runtime_dir(get) {
        for p in scan_wayland_sockets(&rt) {
            push_unique(&mut out, p);
        }
    }

    out
}

fn push_unique(out: &mut Vec<PathBuf>, p: PathBuf) {
    if !out.contains(&p) {
        out.push(p);
    }
}

/// 列出 runtime dir 下所有 `wayland-*` 文件，按 mtime 倒序。
fn scan_wayland_sockets(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("wayland-"));
        if !named {
            continue;
        }
        // 读不到 mtime 就当最旧（UNIX_EPOCH），排序仍然稳定。
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((mtime, path));
    }
    // Reverse：mtime 倒序，新 compositor 的 socket 更新。
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// runtime dir：`XDG_RUNTIME_DIR`，缺失时兜底 `/run/user/<uid>`。
///
/// 兜底用 `libc::getuid()`——`libc` 已在依赖里（架构 §6：依赖零增删）。
fn runtime_dir<F>(get: &F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(dir) = get("XDG_RUNTIME_DIR").filter(|v| !v.trim().is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let uid = unsafe { libc::getuid() };
    let fallback = PathBuf::from(format!("/run/user/{uid}"));
    if fallback.is_dir() {
        Some(fallback)
    } else {
        None
    }
}

pub struct WlrootsBackend;

impl Backend for WlrootsBackend {
    fn id(&self) -> &'static str {
        "wlroots"
    }

    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self) -> Result<()> {
        // 探测逻辑抽成纯函数放 selector（环境可注入），这里只是转发。
        probe_wlroots(&RealProbe)
    }

    /// 阻塞事件循环：委托给 [`Supervisor`]。
    ///
    /// 首次连接失败（D1）由 `Supervisor` 原样上抛，错误信息不被包装，
    /// 上层 `main` 的 `log::error! + exit(1)` 保持既有行为；
    /// 运行中断连则由 `Supervisor` 无限重试并对外上报「无焦点」。
    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        let cfg = ReconnectConfig::from_env();
        log::debug!("wlroots 重连参数: {cfg:?}");
        let connector = WlConnector::new(tx.clone());
        Supervisor::new(connector, RealSleeper, cfg, tx).run()
    }
}

// ---- registry：不做处理，global 绑定已在 run() 中通过 globals.bind 完成 ----
// 注意：UserData 必须是 GlobalListContents 而非 ()，
// 这是 registry_queue_init 的 trait bound 要求，与自行 bind 的 global 不同。
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// ---- manager：只处理新窗口出现 ----
impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                log::debug!("toplevel 事件: 新窗口 {:?}，加入跟踪", toplevel.id());
                state.toplevels.insert(toplevel.id(), Toplevel::default());
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                log::warn!("toplevel manager 已结束，不再接收新窗口事件");
            }
            _ => {}
        }
    }

    // toplevel 事件（opcode 0）会创建新的 ZwlrForeignToplevelHandleV1 对象，
    // 必须在这里声明它的 UserData，否则 wayland-client 会 panic：
    // "Missing event_created_child specialization for event opcode 0"
    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

// ---- handle：窗口属性与激活状态 ----
impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                log::debug!("handle {:?} app_id={:?}", id, app_id);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                log::debug!("handle {:?} title={:?}", id, title);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                // state 是 array<uint32>，Rust 绑定给的是原始字节。
                // as_chunks::<4>() 切出完整的 4 字节块，不足一块的尾段归入 remainder
                // （正常情况为空，协议保证按 uint32 对齐）。
                log::debug!("handle {:?} state 原始字节: {:02x?}", id, raw);
                let (chunks, _remainder) = raw.as_chunks::<4>();
                let activated = chunks
                    .iter()
                    .any(|&c| u32::from_ne_bytes(c) == STATE_ACTIVATED);
                log::debug!("handle {:?} activated={}", id, activated);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.activated = activated;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                log::debug!("handle {:?} closed", id);
                state.toplevels.remove(&id);
                state.emit_if_changed();
            }
            // done 表示这批属性已发送完毕，是统一的提交点。
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                log::debug!(
                    "handle {:?} done; current={:?}",
                    id,
                    state.current().map(|f| f.app_id)
                );
                state.emit_if_changed();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Focus;
    use std::time::Duration;

    /// 建一个隔离的临时 runtime dir（带 tag，避免同进程内用例互相踩）。
    fn temp_runtime(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("focusd-cand-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时 runtime dir 失败");
        dir
    }

    /// 造一个候选文件并设定 mtime（秒级，避开文件系统时间粒度差异）。
    fn touch(path: &Path, secs: u64) {
        let f = fs::File::create(path).expect("创建候选文件失败");
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("设置 mtime 失败");
    }

    /// 可注入的环境表（用 `String` 而非 `&'static str`，便于传运行时路径）。
    fn env(pairs: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn 候选socket_env优先且去重() {
        let rt = temp_runtime("env-first");
        touch(&rt.join("wayland-1"), 100); // 旧
        touch(&rt.join("wayland-2"), 200); // 新
        let get = env(vec![
            ("XDG_RUNTIME_DIR".into(), rt.display().to_string()),
            ("WAYLAND_DISPLAY".into(), "wayland-1".into()),
        ]);

        let got = candidate_sockets_from(&get);

        // env 指定的 wayland-1 必须排第一（即便它更旧），且只出现一次。
        assert_eq!(got, vec![rt.join("wayland-1"), rt.join("wayland-2")]);
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn 候选socket无env时按mtime倒序() {
        let rt = temp_runtime("scan-order");
        touch(&rt.join("wayland-1"), 100);
        touch(&rt.join("wayland-2"), 300);
        touch(&rt.join("wayland-3"), 200);
        let get = env(vec![("XDG_RUNTIME_DIR".into(), rt.display().to_string())]);

        let got = candidate_sockets_from(&get);

        // 新 compositor 的 socket mtime 最新 → 排最前（SIGKILL 残留场景）
        assert_eq!(
            got,
            vec![rt.join("wayland-2"), rt.join("wayland-3"), rt.join("wayland-1")]
        );
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn 候选socket只认wayland前缀() {
        let rt = temp_runtime("prefix");
        touch(&rt.join("wayland-1"), 100);
        touch(&rt.join("sway-ipc.1.sock"), 999);
        touch(&rt.join("not-wayland"), 999);
        let get = env(vec![("XDG_RUNTIME_DIR".into(), rt.display().to_string())]);

        let got = candidate_sockets_from(&get);

        assert_eq!(got, vec![rt.join("wayland-1")]);
        let _ = fs::remove_dir_all(&rt);
    }

    #[test]
    fn 候选socket_绝对路径的wayland_display直接使用() {
        let rt = temp_runtime("abs-runtime");
        let custom = temp_runtime("abs-custom");
        touch(&rt.join("wayland-2"), 200);
        let abs = custom.join("my-socket");
        touch(&abs, 300);
        let get = env(vec![
            ("XDG_RUNTIME_DIR".into(), rt.display().to_string()),
            ("WAYLAND_DISPLAY".into(), abs.display().to_string()),
        ]);

        let got = candidate_sockets_from(&get);

        assert_eq!(got[0], abs, "绝对路径必须原样使用，不拼 runtime dir");
        assert!(got.contains(&rt.join("wayland-2")), "扫描结果仍应作为兜底");
        let _ = fs::remove_dir_all(&rt);
        let _ = fs::remove_dir_all(&custom);
    }

    #[test]
    fn 候选socket_空runtime目录返回空表() {
        let rt = temp_runtime("empty");
        let get = env(vec![("XDG_RUNTIME_DIR".into(), rt.display().to_string())]);

        assert!(candidate_sockets_from(&get).is_empty());
        let _ = fs::remove_dir_all(&rt);
    }

    /// L1 回归测试：焦点消失（最后一个窗口关闭）时必须推送 None 快照。
    /// 此前 emit_if_changed 用 `if let Some` 拦截了 None，导致 serve 的
    /// GetFocus 永远返回陈旧值（迭代二 QA 报告遗留问题 L1）。
    #[test]
    fn 焦点消失时也推送none快照() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = State {
            toplevels: HashMap::new(), // 空表 → current() 为 None
            tx,
            last: Some(Focus {
                app_id: Some("focusd.win1".into()),
                title: Some("one".into()),
            }),
        };

        state.emit_if_changed();

        let received = rx
            .try_recv()
            .expect("焦点消失时必须推送快照（L1 回归）");
        assert_eq!(received, Focus::default());
        assert_eq!(state.last, None);
    }

    #[test]
    fn 无变化时不推送() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = State {
            toplevels: HashMap::new(),
            tx,
            last: None, // 与 current()（None）相同 → 无变化
        };

        state.emit_if_changed();

        assert!(
            rx.try_recv().is_err(),
            "无变化时不应推送"
        );
    }
}
