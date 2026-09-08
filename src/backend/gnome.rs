//! GNOME Shell Extension 后端（迭代二 T04 完整实现）。
//!
//! 设计要点（见 docs/ARCHITECTURE-increment.md §5.2）：
//! Shell 扩展用 Gio 在会话总线上 own `org.focusd.Gnome1` 并导出
//! `GetFocus() -> (s wm_class, s title)`；本后端作为 client 按
//! `FOCUSD_POLL_MS`（默认 250ms）轮询，推入主循环同一通道去重。
//! 注意：GNOME 拿到的是 WM_CLASS，与 wlroots 语义的 app_id 不严格相等。

use std::sync::mpsc::Sender;

use anyhow::{bail, Result};

use super::{Backend, Focus};

pub struct GnomeBackend;

impl Backend for GnomeBackend {
    fn id(&self) -> &'static str {
        "gnome"
    }

    fn name(&self) -> &'static str {
        "GNOME Shell (Extension → D-Bus 轮询)"
    }

    fn probe(&self) -> Result<()> {
        // T01 阶段占位：真实探测由 selector::probe_gnome 承担，
        // 本方法随 T04 轮询实现一并补齐。
        bail!("gnome 后端尚未实现：完整实现将在迭代二后续批次提供")
    }

    fn run(&self, _tx: Sender<Focus>) -> Result<()> {
        bail!("gnome 后端尚未实现：完整实现将在迭代二后续批次提供")
    }
}
