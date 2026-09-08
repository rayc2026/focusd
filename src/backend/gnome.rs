//! GNOME Shell Extension 后端。
//!
//! 链路（架构文档 §5.2）：Shell 扩展（packaging/gnome/focusd@rayc2026.github.io）
//! 用 Gio 在会话总线上 own `org.focusd.Gnome1` 并导出
//! `GetFocus() -> (s wm_class, s title)`；本后端作为 client 按
//! `FOCUSD_POLL_MS`（默认 250ms，可配置）轮询，推入主循环同一通道，
//! 同值重发由主循环的 Dedup 过滤。
//!
//! 语义差异（文档化于 README / docs/dbus.md）：GNOME 拿到的是 **WM_CLASS**
//! （X11 语义），与 wlroots 的 app_id 不严格相等——大多数应用两者一致。

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use zbus::blocking;

use super::selector::{probe_gnome, RealProbe};
use super::{Backend, Focus, empty_to_none};

/// focusd Shell 扩展的 D-Bus 契约（extension.js 导出）。
pub const GNOME_BUS_NAME: &str = "org.focusd.Gnome1";
pub const GNOME_PATH: &str = "/org/focusd/Gnome";
pub const GNOME_IFACE: &str = "org.focusd.Gnome1";

pub struct GnomeBackend;

impl Backend for GnomeBackend {
    fn id(&self) -> &'static str {
        "gnome"
    }

    fn name(&self) -> &'static str {
        "GNOME Shell (Extension → D-Bus 轮询)"
    }

    fn probe(&self) -> Result<()> {
        // 探测逻辑抽成纯函数放 selector（环境可注入），这里只是转发。
        probe_gnome(&RealProbe)
    }

    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        let conn = blocking::Connection::session()
            .context("无法连接会话总线（GNOME 后端需要 D-Bus）")?;
        let proxy = blocking::Proxy::new(&conn, GNOME_BUS_NAME, GNOME_PATH, GNOME_IFACE)
            .with_context(|| {
                format!(
                    "无法创建 Shell 扩展代理。请安装扩展：\
                     将 packaging/gnome/focusd@rayc2026.github.io/ 复制到 \
                     ~/.local/share/gnome-shell/extensions/，重载 Shell 后 \
                     gnome-extensions enable focusd@rayc2026.github.io"
                )
            })?;

        let poll = poll_interval();
        log::info!("GNOME 扩展轮询已启动（间隔 {poll:?}）: {GNOME_BUS_NAME}");

        loop {
            // GetFocus() -> (s wm_class, s title)；空串 = 无焦点窗口
            match proxy.call::<_, _, (String, String)>("GetFocus", &()) {
                Ok((wm_class, title)) => {
                    // 同值会重复推送（250ms 一次），去重由主循环保证
                    let focus = Focus {
                        app_id: empty_to_none(&wm_class),
                        title: empty_to_none(&title),
                    };
                    let _ = tx.send(focus);
                }
                Err(e) => {
                    // 扩展被禁用 / Shell 重载中：不退出，降级继续轮询，
                    // 否则一次瞬时失败就把整个 daemon 打死。
                    log::debug!("GetFocus 轮询失败: {e}");
                }
            }
            std::thread::sleep(poll);
        }
    }
}

/// 轮询间隔：`FOCUSD_POLL_MS`（默认 250ms）。
/// 下限 20ms 防止误配置成 0 打成 busy loop，上限 5s 防止形同关闭。
fn poll_interval() -> Duration {
    let ms = std::env::var("FOCUSD_POLL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250)
        .clamp(20, 5000);
    Duration::from_millis(ms)
}
