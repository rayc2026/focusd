mod backend;
mod dbus;

use anyhow::{Context, Result};
use std::sync::{Arc, RwLock};
use std::sync::mpsc;
use backend::Dedup;
use backend::selector;
use clap::Parser;

#[derive(Parser)]
#[command(name = "focusd", about = "Wayland 焦点应用探测守护进程")]
enum Cli {
    /// 持续监听焦点变化并打印
    Watch {
        /// 输出格式：plain（app_id<TAB>title）或 json
        #[arg(long, default_value = "plain")]
        format: String,
        /// 手动指定后端（省略则自动探测：wlroots → kde → gnome）
        #[arg(long)]
        backend: Option<String>,
    },
    /// 以 D-Bus 服务模式常驻：org.focusd.Focus1（GetFocus / FocusChanged）
    Serve {
        /// 手动指定后端（省略则自动探测：wlroots → kde → gnome）
        #[arg(long)]
        backend: Option<String>,
    },
    /// 打印后端探测结果后退出（用于确认环境是否支持）
    Probe {
        /// 只探测指定后端（如 --backend wlroots）
        #[arg(long)]
        backend: Option<String>,
    },
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli {
        Cli::Probe { backend } => cmd_probe(backend.as_deref()),
        Cli::Watch { format, backend } => cmd_watch(&format, backend.as_deref()),
        Cli::Serve { backend } => cmd_serve(backend.as_deref()),
    }
}

/// probe：列出全部后端的探测结果（顺序 = 自动选择优先级），
/// 或用 --backend 只看某一个。始终正常退出，让用户看到完整诊断。
fn cmd_probe(hint: Option<&str>) -> Result<()> {
    let env = selector::RealProbe;
    match hint {
        Some(id) => {
            let b = selector::select_with(&env, Some(id))?;
            println!("后端 `{}` 探测通过：{}", b.id(), b.name());
        }
        None => {
            println!("后端探测结果（顺序 = 自动选择优先级）：");
            let mut any_ok = false;
            // 用 backends() + Backend::probe() 走后端自身的探测入口，
            // 与 select_with（selector 纯函数）互为对照，两条路径都不会成为死代码
            for b in selector::backends() {
                match b.probe() {
                    Ok(()) => {
                        println!("  [✓] {:<8} 可用", b.id());
                        any_ok = true;
                    }
                    Err(err) => println!("  [✗] {:<8} {:#}", b.id(), err),
                }
            }
            println!();
            if any_ok {
                let b = selector::select_with(&env, None)?;
                println!("自动选择: {}（{}）", b.id(), b.name());
                println!("运行 `focusd watch` 开始监听焦点变化，或 `focusd serve` 启动 D-Bus 服务。");
            } else {
                println!("没有可用后端。提示：X11 会话下本工具无意义，请登录 Wayland 会话后重试。");
            }
        }
    }
    Ok(())
}

/// watch：选定后端 → 后端线程跑事件循环 → 主线程打印。
/// 输出格式与 MVP 完全一致，避免破坏已有消费者。
fn cmd_watch(format: &str, backend_hint: Option<&str>) -> Result<()> {
    let backend = selector::select(backend_hint)?;
    log::info!("使用后端: {} ({})", backend.id(), backend.name());

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        if let Err(e) = backend.run(tx) {
            log::error!("后端退出: {:#}", e);
            std::process::exit(1);
        }
    });

    // 去重收敛到主循环一处（Dedup）：后端内部去重只是优化，
    // 对外语义（不重复输出同一快照）由这里保证。
    let mut dedup = Dedup::new();
    for focus in rx {
        let Some(focus) = dedup.install(focus) else {
            continue;
        };
        match format {
            "json" => println!(
                "{{\"app_id\":{}, \"title\":{}}}",
                serde_escape(focus.app_id.as_deref()),
                serde_escape(focus.title.as_deref())
            ),
            _ => println!(
                "{}\t{}",
                focus.app_id.unwrap_or_else(|| "-".into()),
                focus.title.unwrap_or_else(|| "-".into())
            ),
        }
    }
    Ok(())
}

/// serve：D-Bus 服务模式。
///
/// 线程模型（架构文档 §4 时序图）：
/// 后端线程 --mpsc--> serve 主线程（去重 + 写 Arc<RwLock> 状态 + 发信号）
/// zbus ObjectServer 在自己的内部线程上应答 GetFocus / Kwin.Report。
fn cmd_serve(backend_hint: Option<&str>) -> Result<()> {
    let backend = selector::select(backend_hint)?;
    log::info!("使用后端: {} ({})", backend.id(), backend.name());

    let state = Arc::new(RwLock::new(None::<backend::Focus>));
    let (tx, rx) = mpsc::channel();

    // 先起 D-Bus：KWin 推送入口必须赶在后端事件流之前就绪。
    // 失败（无会话总线等）即退出——PRD 验收要求 daemon 不挂死。
    let conn = dbus::start_serve(Arc::clone(&state), tx.clone())
        .context("D-Bus 服务启动失败（是否有会话总线？请检查 DBUS_SESSION_BUS_ADDRESS）")?;
    log::info!("D-Bus 服务就绪: {} @ {}", dbus::BUS_NAME, dbus::PATH);

    std::thread::spawn(move || {
        if let Err(e) = backend.run(tx) {
            log::error!("后端退出: {:#}", e);
            std::process::exit(1);
        }
    });

    let mut dedup = Dedup::new();
    for focus in rx {
        let Some(focus) = dedup.install(focus) else {
            continue;
        };
        // 先更新状态、后发信号：保证信号订阅者随后调 GetFocus 一定看到新值。
        *state.write().expect("state RwLock 中毒") = Some(focus.clone());
        let (app_id, title) = (
            focus.app_id.unwrap_or_default(),
            focus.title.unwrap_or_default(),
        );
        // blocking::Connection::emit_signal 是 zbus 5 的同步发射路径，
        // 与架构图的 SignalContext + emit 等效（同一 signal 消息）。
        if let Err(e) = conn.emit_signal(
            None::<&str>,
            dbus::PATH,
            dbus::IFACE_FOCUS,
            "FocusChanged",
            &(app_id, title),
        ) {
            log::warn!("FocusChanged 信号发射失败: {e}");
        } else {
            log::debug!("serve: FocusChanged 已发射: {app_id} / {title}");
        }
    }
    Ok(())
}

/// 极简 JSON 字符串转义，避免引入 serde 依赖。
fn serde_escape(s: Option<&str>) -> String {
    match s {
        None => "null".to_string(),
        Some(v) => {
            let escaped = v
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            format!("\"{}\"", escaped)
        }
    }
}
