mod backend;

use anyhow::Result;
use backend::wlroots::WlrootsBackend;
use backend::Backend;
use clap::Parser;

#[derive(Parser)]
#[command(name = "focusd", about = "Wayland 焦点应用探测（MVP：wlroots 后端）")]
enum Cli {
    /// 持续监听焦点变化并打印
    Watch {
        /// 输出格式：plain（app_id<TAB>title）或 json
        #[arg(long, default_value = "plain")]
        format: String,
    },
    /// 打印当前后端能力探测结果后退出（用于确认环境是否支持）
    Probe,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    match cli {
        Cli::Probe => {
            println!("后端: {}", WlrootsBackend.name());
            match std::env::var("WAYLAND_DISPLAY") {
                Ok(v) => println!("WAYLAND_DISPLAY = {}（已设置）", v),
                Err(_) => {
                    println!("WAYLAND_DISPLAY 未设置 —— 你不在 Wayland 会话中。");
                    println!("提示：X11 会话下本工具无意义，请登录 Wayland 会话后重试。");
                    return Ok(());
                }
            }
            println!("XDG_SESSION_TYPE = {:?}", std::env::var("XDG_SESSION_TYPE").ok());
            println!("\n运行 `focusd watch` 可验证后端是否可用。");
            Ok(())
        }

        Cli::Watch { format } => {
            let (tx, rx) = std::sync::mpsc::channel();

            std::thread::spawn(move || {
                if let Err(e) = WlrootsBackend.run(tx) {
                    log::error!("后端退出: {:#}", e);
                    std::process::exit(1);
                }
            });

            for focus in rx {
                match format.as_str() {
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
    }
}

/// 极简 JSON 字符串转义，避免为 MVP 引入 serde 依赖。
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
