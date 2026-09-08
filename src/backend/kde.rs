//! KDE KWin Script 后端（迭代二 T03 完整实现）。
//!
//! 设计要点（见 docs/ARCHITECTURE-increment.md §5.1）：
//! KWin Scripting API 只能单向 `callDBus` 对外推送（无法注册 bus name、
//! 无法导出方法、没有定时器），因此链路是「KWin 脚本订阅
//! windowActivated / captionChanged → callDBus 推送到 focusd 注册的
//! `org.focusd.Focus1` 上的 `org.focusd.Focus1.Kwin.Report` 方法」，
//! 本后端接收推送后汇入主循环同一通道，去重由主循环保证。

use std::sync::mpsc::Sender;

use anyhow::{bail, Result};

use super::{Backend, Focus};

pub struct KdeBackend;

impl Backend for KdeBackend {
    fn id(&self) -> &'static str {
        "kde"
    }

    fn name(&self) -> &'static str {
        "KDE Plasma (KWin Script → D-Bus 推送)"
    }

    fn probe(&self) -> Result<()> {
        // T01 阶段占位：真实探测（KDE_SESSION_VERSION + org.kde.KWin 在线）
        // 由 selector::probe_kde 承担，本方法随 T03 loadScript 实现一并补齐。
        bail!("kde 后端尚未实现：完整实现将在迭代二后续批次提供")
    }

    fn run(&self, _tx: Sender<Focus>) -> Result<()> {
        bail!("kde 后端尚未实现：完整实现将在迭代二后续批次提供")
    }
}
