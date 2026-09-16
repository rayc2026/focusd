//! 重连内核（迭代三 · T01）。
//!
//! 目标：让「compositor 被杀 / 扩展失效」这类**运行中断连**能自动恢复，
//! 且这段逻辑**可以被单测**（PRD R3）。
//!
//! 设计要点（详见 `docs/ARCHITECTURE-iter3.md` §3.2）：
//!
//! - 抽出三个可注入 trait：`Session`（一条连接的事件泵）/ `Connector`（连接工厂）
//!   / `Sleeper`（退避时钟）。`Supervisor` 只依赖 trait，测试注入脚本化假实现，
//!   可在毫秒内跑完「断连 → N 次重连失败 → 恢复」完整状态机，**不碰真实时间与总线**。
//! - `ConnectMode::Initial`（首次）失败**直接上抛 Err**，不进退避循环（D1：
//!   配置错误就该明确报错退出，保住 CI 那条断言）；`ConnectMode::Reconnect`
//!   才允许重解析 / 扫描候选端点（Q5）。
//! - 断连瞬间立刻向通道推 `Focus::default()`（D2：失效即无焦点），
//!   主循环 `Dedup` 保证对外只发射一次 `FocusChanged("","")`。
//! - 日志分级（D4）：`LogThrottle` 统一实现「第 1 次 warn / 档位爬升 warn /
//!   每 10 次 warn / 其余 debug」，避免注销前后无限重连刷爆 journald。
//!
//! 本模块**通用**：不含任何 wlroots / KDE / GNOME  specifics，
//! 具体后端在各自文件里实现 `Connector` + `Session` 即可（T02/T03/T04）。

use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};

use super::Focus;

// ---------------------------------------------------------------------------
// 三个可注入抽象
// ---------------------------------------------------------------------------

/// 一次「会话」＝一条已建立的连接 + 它的事件泵。
///
/// 只暴露一个动作：阻塞等下一批事件。`Err` 即「会话已断，请重连」。
/// 不暴露「连接对象」本身，是为了让假实现能完全用脚本驱动。
pub trait Session: Send {
    /// 阻塞派发一批事件。`Err` 表示连接已不可用。
    fn pump(&mut self) -> Result<()>;
}

/// 连接工厂（**可测试性的注入点**）。
///
/// `&self` 而非 `&mut self`：真实实现是无状态工厂（重连所需的 socket 解析
/// 在方法内部完成），这样 `Supervisor` 不必持有可变引用，测试也能并发断言。
pub trait Connector: Send + Sync {
    /// 建立一条新连接。`ConnectMode::Initial` 失败必须原样上抛（D1）。
    fn connect(&self, mode: ConnectMode) -> Result<Box<dyn Session>>;

    /// 后端名，用于日志（如 "wlroots (zwlr-foreign-toplevel-management-unstable-v1)"）。
    fn label(&self) -> &'static str;

    /// 失效时给用户的可操作提示，拼在 warn 文案尾部（可为空）。
    fn hint(&self) -> &'static str {
        ""
    }
}

/// 连接场景。区别对待是为了同时满足两个互相冲突的产品决策：
/// 「配置错误要立刻报错」（首次）与「运行中断连要无限重试」（重连）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectMode {
    /// 进程启动后的首次连接：只走 `WAYLAND_DISPLAY`，失败即报错退出（D1）。
    Initial,
    /// 运行中断连后的重连：允许重解析 + 扫描候选（Q5）。
    Reconnect,
}

/// 退避时钟（可注入）。生产用 [`RealSleeper`]，测试用假时钟零等待。
pub trait Sleeper: Send + Sync {
    fn sleep(&self, d: Duration);
}

/// 生产时钟：就是 `thread::sleep`。
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

/// `FOCUSD_RECONNECT_MIN_MS`
const KEY_MIN: &str = "FOCUSD_RECONNECT_MIN_MS";
/// `FOCUSD_RECONNECT_MAX_MS`
const KEY_MAX: &str = "FOCUSD_RECONNECT_MAX_MS";
/// `FOCUSD_RECONNECT_FACTOR`
const KEY_FACTOR: &str = "FOCUSD_RECONNECT_FACTOR";
/// `FOCUSD_RECONNECT_JITTER_PCT`
const KEY_JITTER: &str = "FOCUSD_RECONNECT_JITTER_PCT";
/// `FOCUSD_RECONNECT_MAX_ATTEMPTS`
const KEY_ATTEMPTS: &str = "FOCUSD_RECONNECT_MAX_ATTEMPTS";
/// `FOCUSD_RECONNECT_DISCOVER_MAX_MS`
const KEY_DISCOVER: &str = "FOCUSD_RECONNECT_DISCOVER_MAX_MS";

/// 重连参数（全部 `FOCUSD_*` env 可配，解析后 clamp，见架构 §9.3）。
///
/// 之所以做成 `Copy`：它会同时被 `Supervisor` 与 `Backoff` 持有，
/// 拷贝 6 个标量的成本远低于到处传引用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectConfig {
    /// 初始退避间隔。
    pub min: Duration,
    /// 退避上限（普通失败）。
    pub max: Duration,
    /// 指数退避倍率。
    pub factor: f64,
    /// 抖动百分比（`0` = 关闭，CI 单测与确定性场景必须设 `0`）。
    pub jitter_pct: u32,
    /// 最大重连次数（`0` = 无限，常驻语义的默认值）。
    pub max_attempts: u32,
    /// 「无候选端点」这类廉价探测失败的退避封顶。
    pub discover_max: Duration,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(500),
            max: Duration::from_millis(30_000),
            factor: 2.0,
            jitter_pct: 20,
            max_attempts: 0,
            discover_max: Duration::from_millis(2_000),
        }
    }
}

impl ReconnectConfig {
    /// 从进程环境解析（生产入口）。
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// 从任意键值对解析。
    ///
    /// 抽出这个入口是为了**可测**：进程级 env 是全局可变状态，
    /// 同一测试二进制内的用例并行执行时互相污染，也无法断言「非法值回落默认」。
    /// 生产走 [`Self::from_env`]，测试注入一个 map 即可。
    pub fn from_lookup<F>(get: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let d = Self::default();
        let min_ms = clamp_u64(env_u64(&get, KEY_MIN, as_ms(d.min)), 10, 60_000);
        let mut max_ms = clamp_u64(env_u64(&get, KEY_MAX, as_ms(d.max)), 10, 600_000);
        // max < min 会让退避永远停在下界、日志里还写着「已封顶」——直接抬平。
        if max_ms < min_ms {
            max_ms = min_ms;
        }
        let factor = clamp_f64(env_f64(&get, KEY_FACTOR, d.factor), 1.0, 10.0, d.factor);
        let jitter_pct = clamp_u64(
            env_u64(&get, KEY_JITTER, u64::from(d.jitter_pct)),
            0,
            50,
        ) as u32;
        let max_attempts = clamp_u64(
            env_u64(&get, KEY_ATTEMPTS, u64::from(d.max_attempts)),
            0,
            u64::from(u32::MAX),
        ) as u32;
        let discover_ms = clamp_u64(env_u64(&get, KEY_DISCOVER, as_ms(d.discover_max)), 100, 30_000);

        Self {
            min: Duration::from_millis(min_ms),
            max: Duration::from_millis(max_ms),
            factor,
            jitter_pct,
            max_attempts,
            discover_max: Duration::from_millis(discover_ms),
        }
    }
}

fn as_ms(d: Duration) -> u64 {
    d.as_millis().min(u128::from(u64::MAX)) as u64
}

fn clamp_u64(v: u64, lo: u64, hi: u64) -> u64 {
    v.clamp(lo, hi)
}

/// `f64` 的 clamp：`NaN` / 无穷会让 `f64::clamp` panic，先挡掉再回落默认。
fn clamp_f64(v: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        fallback
    }
}

fn env_u64<F>(get: &F, key: &str, default: u64) -> u64
where
    F: Fn(&str) -> Option<String>,
{
    match get(key) {
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                log::warn!("{key}={raw:?} 非法（应为非负整数），回落默认 {default}");
                default
            }
        },
        None => default,
    }
}

fn env_f64<F>(get: &F, key: &str, default: f64) -> f64
where
    F: Fn(&str) -> Option<String>,
{
    match get(key) {
        Some(raw) => match raw.trim().parse::<f64>() {
            Ok(v) => v,
            Err(_) => {
                log::warn!("{key}={raw:?} 非法（应为浮点数），回落默认 {default}");
                default
            }
        },
        None => default,
    }
}

// ---------------------------------------------------------------------------
// 退避
// ---------------------------------------------------------------------------

/// 标记「没有可连接的候选端点」这一类失败。
///
/// 这类失败是极廉价的探测（一次 readdir + 若干 connect），等 30s 再试既拖慢
/// 恢复（PRD 要求 ≤30s 内恢复）又毫无必要，因此 `Supervisor` 会把它单独封顶到
/// `discover_max`（默认 2s）。用类型标记而非字符串匹配，是为了让假实现也能
/// 精确构造这条路径（见本文件单测）。
#[derive(Debug)]
pub struct DiscoveryFailure(String);

impl std::fmt::Display for DiscoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DiscoveryFailure {}

/// 把一次失败包装成 [`DiscoveryFailure`]（保留完整错误链文本）。
pub fn no_endpoint(inner: anyhow::Error) -> anyhow::Error {
    anyhow!(DiscoveryFailure(format!("{inner:#}")))
}

/// 判断一次失败是否属于「无候选端点」。
pub fn is_discovery_failure(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DiscoveryFailure>().is_some()
}

/// 手写线性同余发生器（MMIX / Knuth 常数）。
///
/// 抖动的目的只是避免多实例同时重连造成惊群，不值得为此把 `rand` 拉进依赖树
/// （架构 §6 明确：本迭代依赖零增删）。
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new() -> Self {
        // 种子只需「每次进程启动不同」；取纳秒子秒位，避免依赖 `rand`。
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos()))
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self { state: seed | 1 }
    }

    /// 固定种子，仅供单测断言确定性。
    #[cfg(test)]
    fn seeded(state: u64) -> Self {
        Self { state: state | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    /// 返回 `[0, n)` 内的伪随机数。取模带来的轻微偏斜对抖动无关紧要。
    fn below(&mut self, n: u128) -> u128 {
        if n <= 1 {
            return 0;
        }
        let hi = u128::from(self.next_u64());
        let lo = u128::from(self.next_u64());
        ((hi << 64) | lo) % n
    }
}

/// 指数退避：`min * factor^step`，封顶 `max`；
/// 若最近一次失败是「无候选端点」则再封顶 `discover_max`。
///
/// 抖动在最后一步施加（±`jitter_pct`%），`jitter_pct = 0` 时函数完全确定
/// ——这是单测能断言精确退避序列的前提。
pub struct Backoff {
    cfg: ReconnectConfig,
    step: u32,
    rng: Lcg,
}

impl Backoff {
    pub fn new(cfg: ReconnectConfig) -> Self {
        Self { cfg, step: 0, rng: Lcg::new() }
    }

    /// 当前档位（已自增后的 step），供 `LogThrottle` 判断「档位是否提升」。
    pub fn step(&self) -> u32 {
        self.step
    }

    /// 重连成功后调用：回到初始档位。
    pub fn reset(&mut self) {
        self.step = 0;
    }

    /// 取出下一次退避时长并自增档位。
    pub fn next(&mut self, discover_only: bool) -> Duration {
        let cap_ms = if discover_only {
            self.cfg.max.as_millis().min(self.cfg.discover_max.as_millis())
        } else {
            self.cfg.max.as_millis()
        };
        let base_ms = self.tier_ms(cap_ms);
        let delay = Duration::from_millis(base_ms.min(u128::from(u64::MAX)) as u64);
        self.step = self.step.saturating_add(1);
        apply_jitter(delay, self.cfg.jitter_pct, &mut self.rng)
    }

    /// 用**逐步乘法**而不是 `min * factor^step`：
    /// 后者在 step 较大时必然溢出 f64，而封顶之后 step 还会一直自增
    /// （默认 `max_attempts = 0` = 无限重试）。这里一旦触顶就提前退出，
    /// 循环次数被档位数（约 7 次）天然限住。
    fn tier_ms(&self, cap_ms: u128) -> u128 {
        let mut ms = self.cfg.min.as_millis().min(cap_ms);
        let mut i: u32 = 0;
        while i < self.step {
            let grown = (ms as f64 * self.cfg.factor).round();
            // factor 被 clamp 到 [1.0, 10.0]，但 `ms` 可能已触顶：
            // 一旦不再增长（或算出非有限值）就停，避免死循环。
            if !grown.is_finite() || grown <= ms as f64 {
                break;
            }
            ms = grown as u128;
            if ms >= cap_ms {
                ms = cap_ms;
                break;
            }
            i += 1;
        }
        ms
    }
}

/// 施加 ±`pct`% 抖动。`pct == 0` 时原样返回（保证确定性）。
fn apply_jitter(d: Duration, pct: u32, rng: &mut Lcg) -> Duration {
    if pct == 0 {
        return d;
    }
    let ms = d.as_millis();
    if ms == 0 {
        return d;
    }
    let delta = (ms as f64 * f64::from(pct) / 100.0).round();
    if !delta.is_finite() || delta <= 0.0 {
        return d;
    }
    let delta = delta as u128;
    // 在 [ms - delta, ms + delta] 内均匀取值。
    let span = delta.saturating_mul(2).saturating_add(1);
    let shifted = ms as i128 + rng.below(span) as i128 - delta as i128;
    let out = if shifted <= 0 { 0u128 } else { shifted as u128 };
    Duration::from_millis(out.min(u128::from(u64::MAX)) as u64)
}

/// 退避档位的「爬升期」上限（含）。
///
/// 默认参数下档位只有 7 档（500/1k/2k/4k/8k/16k/30k）。
/// 超过之后延迟已被 `max` 封顶，`step` 仍会自增但**延迟不再变化**——
/// 若继续把每次自增都算作「档位提升」，D4 的日志抑制就形同虚设（每次都 warn）。
const TIER_CLIMB_MAX_STEP: u32 = 6;

/// D4 日志分级抑制。
///
/// 规则（架构 §9.2）：
/// - 第 1 次失败 → `Warn`（真机排障的第一现场，必须看得见）；
/// - 退避**档位仍在爬升期**且发生提升 → `Warn`（说明故障在持续恶化）；
/// - 每 10 次 → `Warn`（兜底：长故障期间也要留下心跳证据）；
/// - 其余 → `Debug`。
#[derive(Debug, Default)]
pub struct LogThrottle {
    attempts: u32,
    last_step: u32,
    last_warn: u32,
}

impl LogThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次失败，返回本次该用的日志级别。
    ///
    /// `step` 传 [`Backoff::step`]；KDE / GNOME 这类没有指数退避的循环传 `0`，
    /// 此时只按「首次 + 每 10 次」分级。
    pub fn on_failure(&mut self, step: u32) -> log::Level {
        self.attempts = self.attempts.saturating_add(1);
        let first = self.attempts == 1;
        let tier_climb = step > self.last_step && step <= TIER_CLIMB_MAX_STEP;
        let heartbeat = self.attempts % 10 == 0;
        self.last_step = step;

        if first || tier_climb || heartbeat {
            self.last_warn = self.attempts;
            log::Level::Warn
        } else {
            log::Level::Debug
        }
    }

    /// 重连成功后调用：重新回到「下一次失败即首次」。
    pub fn reset(&mut self) {
        self.attempts = 0;
        self.last_step = 0;
        self.last_warn = 0;
    }

    /// 已记录的失败次数。
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// 最近一次升级为 `Warn` 的是第几次失败。
    pub fn last_warn(&self) -> u32 {
        self.last_warn
    }
}

// ---------------------------------------------------------------------------
// 监督器
// ---------------------------------------------------------------------------

/// 重连状态机的驱动器。
///
/// 精确语义（架构 §3.2）：
///
/// ```text
/// session = connector.connect(Initial)?    // ← 失败：Err 上抛（D1，不重试）
/// loop:
///     match session.pump():
///         Ok(())  => continue
///         Err(e)  => 推 Focus::default() → 退避 → connect(Reconnect) → 循环
/// ```
///
/// `C` / `S` 为泛型而非 `dyn`：真实后端在编译期确定，省掉一层动态分发，
/// 也让测试能直接注入假类型。
pub struct Supervisor<C: Connector, S: Sleeper> {
    connector: C,
    sleeper: S,
    cfg: ReconnectConfig,
    tx: Sender<Focus>,
}

impl<C: Connector, S: Sleeper> Supervisor<C, S> {
    pub fn new(connector: C, sleeper: S, cfg: ReconnectConfig, tx: Sender<Focus>) -> Self {
        Self { connector, sleeper, cfg, tx }
    }

    /// 阻塞运行，只在两种情况下返回 `Err`：
    /// ① 首次连接失败（D1：配置错误，明确报错退出）；
    /// ② 重连次数耗尽（`max_attempts > 0` 且已用完；默认 0 = 永不放弃）。
    pub fn run(self) -> Result<()> {
        let label = self.connector.label();
        let hint = self.connector.hint();
        let tail = if hint.is_empty() {
            String::new()
        } else {
            format!(" {hint}")
        };

        // D1：首次连接失败**不**进退避循环，原样上抛。
        // main 侧已有的 `log::error! + exit(1)` 会把它变成明确的报错退出，
        // 这正是「无 compositor 时 serve 明确报错退出」这条 CI 断言依赖的行为。
        let mut session = self.connector.connect(ConnectMode::Initial)?;
        log::debug!("{label} 首次连接成功，进入事件派发循环");

        let mut backoff = Backoff::new(self.cfg);
        let mut throttle = LogThrottle::new();
        let mut attempts: u32 = 0;
        // U7：`generation` 预留给将来的 `GetStatus`，本迭代不对外，只在 debug 日志留线索。
        let mut generation: u32 = 0;

        loop {
            let mut err = match session.pump() {
                Ok(()) => continue,
                Err(e) => e,
            };

            // D2：无法确认焦点 → 立刻对外上报「无焦点」。
            // 主循环 `Dedup` 保证 `FocusChanged("","")` 恰好一次；
            // 重连循环绝不触碰 `state`，也绝不保留最后已知值。
            let _ = self.tx.send(Focus::default());

            loop {
                attempts = attempts.saturating_add(1);
                let limit = self.cfg.max_attempts;
                if limit > 0 && attempts > limit {
                    bail!("{label} 重连 {limit} 次仍未恢复，放弃（最后错误: {err}）。{tail}");
                }

                let delay = backoff.next(is_discovery_failure(&err));
                let step = backoff.step();
                let level = throttle.on_failure(step);
                log_at(level, label, attempts, &err, delay, &tail);
                self.sleeper.sleep(delay);

                match self.connector.connect(ConnectMode::Reconnect) {
                    Ok(next) => {
                        log::info!("{label} 已恢复（第 {attempts} 次重连成功）");
                        session = next;
                        attempts = 0;
                        backoff.reset();
                        throttle.reset();
                        generation = generation.saturating_add(1);
                        log::debug!("{label} 会话代次 -> {generation}");
                        break;
                    }
                    Err(ce) => err = ce,
                }
            }
        }
    }
}

/// 按级别输出一条断连日志。文案模板见架构 §9.4：
/// `<后端> <故障>（第 N 次重试，{err}）；<delay> 后重试。<可操作提示>`
fn log_at(
    level: log::Level,
    label: &str,
    attempts: u32,
    err: &anyhow::Error,
    delay: Duration,
    tail: &str,
) {
    let msg = format!("{label} 连接断开（第 {attempts} 次重试，{err}）；{delay:?} 后重试。{tail}");
    match level {
        log::Level::Warn => log::warn!("{msg}"),
        _ => log::debug!("{msg}"),
    }
}

// ---------------------------------------------------------------------------
// 单测：全部用假实现，不碰真实时间与总线，总耗时 < 1s
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{mpsc, Arc, Mutex};

    // ---- 假实现 ----

    /// 共享记录器。`Supervisor::run` 会消费掉 connector / sleeper，
    /// 测试靠事先克隆这份记录来断言。
    #[derive(Clone, Default)]
    struct Recorder {
        calls: Arc<Mutex<Vec<ConnectMode>>>,
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    impl Recorder {
        fn calls(&self) -> Vec<ConnectMode> {
            self.calls.lock().unwrap().clone()
        }

        fn count(&self, mode: ConnectMode) -> usize {
            self.calls.lock().unwrap().iter().filter(|m| **m == mode).count()
        }

        fn sleeps_ms(&self) -> Vec<u128> {
            self.sleeps.lock().unwrap().iter().map(|d| d.as_millis()).collect()
        }
    }

    struct FakeSleeper {
        rec: Recorder,
    }

    impl FakeSleeper {
        fn new(rec: Recorder) -> Self {
            Self { rec }
        }
    }

    impl Sleeper for FakeSleeper {
        fn sleep(&self, d: Duration) {
            self.rec.sleeps.lock().unwrap().push(d);
        }
    }

    /// 一次 `connect` 的剧本。
    enum Plan {
        /// 连接成功；参数是该会话 `pump` 的返回值脚本
        /// （脚本耗尽后再 `pump` 视为「连接再次断开」，保证状态机可终止）。
        Ok(Vec<anyhow::Result<()>>),
        /// 普通失败。
        Err(&'static str),
        /// 「无候选端点」失败：退避按 `discover_max` 封顶。
        NoEndpoint(&'static str),
    }

    /// 生成 `n` 次成功的 pump 脚本。
    fn oks(n: usize) -> Vec<anyhow::Result<()>> {
        (0..n).map(|_| Ok(())).collect()
    }

    struct FakeSession {
        script: VecDeque<anyhow::Result<()>>,
    }

    impl Session for FakeSession {
        fn pump(&mut self) -> Result<()> {
            match self.script.pop_front() {
                Some(r) => r,
                None => Err(anyhow!("测试会话 pump 脚本耗尽：模拟连接再次断开")),
            }
        }
    }

    struct FakeConnector {
        rec: Recorder,
        initial: Mutex<VecDeque<Plan>>,
        reconnect: Mutex<VecDeque<Plan>>,
    }

    impl FakeConnector {
        fn new(rec: Recorder, initial: Vec<Plan>, reconnect: Vec<Plan>) -> Self {
            Self {
                rec,
                initial: Mutex::new(initial.into()),
                reconnect: Mutex::new(reconnect.into()),
            }
        }
    }

    impl Connector for FakeConnector {
        fn connect(&self, mode: ConnectMode) -> Result<Box<dyn Session>> {
            self.rec.calls.lock().unwrap().push(mode);
            let queue = match mode {
                ConnectMode::Initial => &self.initial,
                ConnectMode::Reconnect => &self.reconnect,
            };
            let plan = queue.lock().unwrap().pop_front();
            match plan {
                None => Err(anyhow!("测试剧本耗尽：connect({mode:?}) 已无计划")),
                Some(Plan::Ok(script)) => Ok(Box::new(FakeSession { script: script.into() })),
                Some(Plan::Err(m)) => Err(anyhow!(m)),
                Some(Plan::NoEndpoint(m)) => Err(no_endpoint(anyhow!(m))),
            }
        }

        fn label(&self) -> &'static str {
            "测试后端"
        }

        fn hint(&self) -> &'static str {
            "请检查测试环境"
        }
    }

    // ---- 测试夹具 ----

    /// 默认档位（500ms 起、×2、30s 封顶、2s discover 封顶）+ **无抖动**，
    /// 保证退避序列可精确断言。
    fn cfg_test(max_attempts: u32) -> ReconnectConfig {
        ReconnectConfig {
            min: Duration::from_millis(500),
            max: Duration::from_millis(30_000),
            factor: 2.0,
            jitter_pct: 0,
            max_attempts,
            discover_max: Duration::from_millis(2_000),
        }
    }

    fn lookup(pairs: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    // ---- 用例 ----

    #[test]
    fn 退避序列指数增长并封顶到max() {
        let mut b = Backoff::new(cfg_test(0));
        let seq: Vec<u128> = (0..9).map(|_| b.next(false).as_millis()).collect();
        assert_eq!(
            seq,
            vec![500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]
        );
    }

    #[test]
    fn 无候选端点时退避封顶到discover_max() {
        let mut b = Backoff::new(cfg_test(0));
        let seq: Vec<u128> = (0..6).map(|_| b.next(true).as_millis()).collect();
        assert_eq!(seq, vec![500, 1_000, 2_000, 2_000, 2_000, 2_000]);
    }

    #[test]
    fn 抖动为0时完全确定() {
        let mut b = Backoff::new(ReconnectConfig { jitter_pct: 0, ..cfg_test(0) });
        assert_eq!(b.next(false).as_millis(), 500);
        assert_eq!(b.next(false).as_millis(), 1_000);
        assert_eq!(b.next(false).as_millis(), 2_000);
    }

    #[test]
    fn 抖动开启时落在正负pct区间内() {
        // factor=1 且 min=max：每次基准都是 1000ms，便于只断言抖动范围。
        let cfg = ReconnectConfig {
            min: Duration::from_millis(1_000),
            max: Duration::from_millis(1_000),
            factor: 1.0,
            jitter_pct: 20,
            max_attempts: 0,
            discover_max: Duration::from_millis(2_000),
        };
        let mut b = Backoff::new(cfg);
        for _ in 0..64 {
            let ms = b.next(false).as_millis();
            assert!((800..=1_200).contains(&ms), "抖动越界: {ms}ms");
        }
    }

    #[test]
    fn 同种子抖动序列确定() {
        let cfg = ReconnectConfig { jitter_pct: 30, ..cfg_test(0) };
        let mut a = Backoff { cfg, step: 0, rng: Lcg::seeded(42) };
        let mut b = Backoff { cfg, step: 0, rng: Lcg::seeded(42) };
        for _ in 0..8 {
            assert_eq!(a.next(false), b.next(false));
        }
        // 不同种子应当产生不同序列（至少一次不同），证明种子真的生效。
        let mut c = Backoff { cfg, step: 0, rng: Lcg::seeded(7) };
        let mut d = Backoff { cfg, step: 0, rng: Lcg::seeded(99) };
        let differs = (0..8).any(|_| c.next(false) != d.next(false));
        assert!(differs, "不同种子应产生不同抖动序列");
    }

    #[test]
    fn 断连时立刻推送无焦点快照() {
        let (tx, rx) = mpsc::channel();
        let rec = Recorder::default();
        let conn = FakeConnector::new(
            rec.clone(),
            vec![Plan::Ok(vec![Ok(()), Ok(()), Err(anyhow!("compositor 已消失"))])],
            vec![], // 重连剧本为空 → 全部失败
        );
        let sup = Supervisor::new(conn, FakeSleeper::new(rec.clone()), cfg_test(2), tx);
        assert!(sup.run().is_err(), "重连耗尽后应返回 Err");

        let first = rx.try_recv().expect("断连时必须推送无焦点快照（D2）");
        assert_eq!(first, Focus::default());
    }

    #[test]
    fn 前三次重连失败后恢复并重置退避() {
        let (tx, _rx) = mpsc::channel();
        let rec = Recorder::default();
        let conn = FakeConnector::new(
            rec.clone(),
            vec![Plan::Ok(vec![Ok(()), Err(anyhow!("socket EOF"))])],
            vec![
                Plan::Err("重连失败 1"),
                Plan::Err("重连失败 2"),
                Plan::Err("重连失败 3"),
                Plan::Ok(oks(1)), // 第 4 次成功 → 恢复
            ],
        );
        let cfg = cfg_test(10);
        let sup = Supervisor::new(conn, FakeSleeper::new(rec.clone()), cfg, tx);
        // 恢复后会话脚本耗尽再次断连，最终因 max_attempts 耗尽返回 Err（保证可终止）。
        assert!(sup.run().is_err());

        assert_eq!(rec.count(ConnectMode::Initial), 1, "首次连接只应发生一次");
        assert_eq!(rec.count(ConnectMode::Reconnect), 14);
        // 前 4 次退避到 4000ms 后重连成功；恢复后 backoff 重置，序列从 500ms 重新爬升。
        assert_eq!(
            rec.sleeps_ms(),
            vec![
                500, 1_000, 2_000, 4_000, // 断连 → 4 次失败后恢复
                500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000, 30_000,
            ]
        );
    }

    #[test]
    fn 永远失败达max_attempts后返回err() {
        let (tx, _rx) = mpsc::channel();
        let rec = Recorder::default();
        let conn = FakeConnector::new(
            rec.clone(),
            vec![Plan::Ok(vec![Err(anyhow!("compositor 被 kill"))])],
            vec![], // 永远失败
        );
        let sup = Supervisor::new(conn, FakeSleeper::new(rec.clone()), cfg_test(5), tx);
        let err = sup.run().expect_err("重连次数耗尽后必须返回 Err");
        assert!(err.to_string().contains("放弃"), "错误信息应说明放弃了: {err}");

        assert_eq!(rec.count(ConnectMode::Reconnect), 5, "attempt 恰为 5");
        assert_eq!(rec.sleeps_ms(), vec![500, 1_000, 2_000, 4_000, 8_000]);
    }

    #[test]
    fn 首次连接失败不重连直接返回err() {
        let (tx, rx) = mpsc::channel();
        let rec = Recorder::default();
        let conn = FakeConnector::new(
            rec.clone(),
            vec![Plan::Err("无法连接 Wayland display")],
            vec![Plan::Ok(oks(3))], // 若错误地被重连，这里会被用掉
        );
        let sup = Supervisor::new(conn, FakeSleeper::new(rec.clone()), cfg_test(0), tx);
        let err = sup.run().expect_err("首次连接失败必须 Err（D1）");
        assert!(err.to_string().contains("无法连接 Wayland display"));

        // 只调了一次 connect，且是 Initial；没有退避、没有重连。
        assert_eq!(rec.calls(), vec![ConnectMode::Initial]);
        assert!(rec.sleeps_ms().is_empty(), "首次失败不应产生任何退避等待");
        assert!(rx.try_recv().is_err(), "首次失败不属于「运行中断连」，不推无焦点");
    }

    #[test]
    fn 无候选端点时supervisor按discover_max封顶() {
        let (tx, _rx) = mpsc::channel();
        let rec = Recorder::default();
        let conn = FakeConnector::new(
            rec.clone(),
            vec![Plan::Ok(vec![Err(anyhow!("socket EOF"))])],
            vec![
                Plan::NoEndpoint("无候选 socket 1"),
                Plan::NoEndpoint("无候选 socket 2"),
                Plan::NoEndpoint("无候选 socket 3"),
                Plan::NoEndpoint("无候选 socket 4"),
                Plan::NoEndpoint("无候选 socket 5"),
            ],
        );
        let sup = Supervisor::new(conn, FakeSleeper::new(rec.clone()), cfg_test(5), tx);
        assert!(sup.run().is_err());
        // 普通失败本应是 500/1000/2000/4000/8000，被 discover_max(2000) 截断。
        assert_eq!(rec.sleeps_ms(), vec![500, 1_000, 2_000, 2_000, 2_000]);
    }

    #[test]
    fn 日志分级首次与每10次warn其余debug() {
        let mut t = LogThrottle::new();
        let levels: Vec<log::Level> = (0..20).map(|_| t.on_failure(0)).collect();

        assert_eq!(levels[0], log::Level::Warn, "第 1 次必须 warn");
        for (i, l) in levels.iter().enumerate().take(9).skip(1) {
            assert_eq!(*l, log::Level::Debug, "第 {} 次应为 debug", i + 1);
        }
        assert_eq!(levels[9], log::Level::Warn, "第 10 次必须 warn");
        assert_eq!(levels[19], log::Level::Warn, "第 20 次必须 warn");
        assert_eq!(t.attempts(), 20);
        assert_eq!(t.last_warn(), 20);
    }

    #[test]
    fn 日志分级档位爬升时warn() {
        let mut t = LogThrottle::new();
        assert_eq!(t.on_failure(0), log::Level::Warn, "首次");
        assert_eq!(t.on_failure(3), log::Level::Warn, "档位提升且仍在爬升期");
        assert_eq!(t.on_failure(3), log::Level::Debug, "档位未变 → debug");
    }

    #[test]
    fn 日志分级封顶后档位提升不再warn() {
        let mut t = LogThrottle::new();
        assert_eq!(t.on_failure(0), log::Level::Warn, "首次");
        // step 越过爬升期上限：延迟已被 max 封顶，提升不再有信息量。
        assert_eq!(t.on_failure(7), log::Level::Debug);
    }

    #[test]
    fn 日志分级reset后重新算首次() {
        let mut t = LogThrottle::new();
        assert_eq!(t.on_failure(0), log::Level::Warn);
        t.reset();
        assert_eq!(t.on_failure(0), log::Level::Warn, "reset 后应重新视为首次");
        assert_eq!(t.attempts(), 1);
    }

    #[test]
    fn 参数解析默认值() {
        let cfg = ReconnectConfig::from_lookup(lookup(vec![]));
        assert_eq!(cfg, ReconnectConfig::default());
    }

    #[test]
    fn 参数解析clamp与非法值回落() {
        let cfg = ReconnectConfig::from_lookup(lookup(vec![
            (KEY_MIN, "1"),         // < 10 → clamp 到 10
            (KEY_MAX, "5"),         // < 10 → clamp 到 10，再被 max>=min 抬平
            (KEY_FACTOR, "99"),     // > 10 → clamp 到 10
            (KEY_JITTER, "80"),     // > 50 → clamp 到 50
            (KEY_ATTEMPTS, "7"),
            (KEY_DISCOVER, "abc"),  // 非法 → 回落默认 2000
        ]));
        assert_eq!(cfg.min.as_millis(), 10);
        assert_eq!(cfg.max.as_millis(), 10);
        assert!((cfg.factor - 10.0).abs() < 1e-9);
        assert_eq!(cfg.jitter_pct, 50);
        assert_eq!(cfg.max_attempts, 7);
        assert_eq!(cfg.discover_max.as_millis(), 2_000);
    }

    #[test]
    fn 参数解析max小于min时抬平() {
        let cfg = ReconnectConfig::from_lookup(lookup(vec![
            (KEY_MIN, "5_000"),
            (KEY_MAX, "1_000"),
        ]));
        // 两个值都非法（含下划线）→ 回落默认 500 / 30000，默认本身满足 max >= min。
        assert_eq!(cfg.min.as_millis(), 500);
        assert_eq!(cfg.max.as_millis(), 30_000);

        let cfg = ReconnectConfig::from_lookup(lookup(vec![
            (KEY_MIN, "5000"),
            (KEY_MAX, "1000"),
        ]));
        assert_eq!(cfg.min.as_millis(), 5_000);
        assert_eq!(cfg.max, cfg.min, "max 必须被抬到 min 以上");
    }

    #[test]
    fn no_endpoint可被识别为探测失败() {
        let e = no_endpoint(anyhow!("已尝试全部候选 socket 均无 manager"));
        assert!(is_discovery_failure(&e));
        assert!(!is_discovery_failure(&anyhow!("普通 IO 错误")));
    }
}
