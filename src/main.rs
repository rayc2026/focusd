mod backend;

use anyhow::Result;
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
                println!("运行 `focusd watch` 开始监听焦点变化。");
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

    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        if let Err(e) = backend.run(tx) {
            log::error!("后端退出: {:#}", e);
            std::process::exit(1);
        }
    });

    // 去重收敛到主循环一处（Dedup）：后端内部去重只是优化，
    // 对外语义（不重复输出同一快照）由这里保证。
    let mut dedup = backend::Dedup::new();
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
