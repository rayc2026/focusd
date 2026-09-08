pub mod gnome;
pub mod kde;
pub mod selector;
pub mod wlroots;

/// 焦点应用快照。
///
/// 故意只保留两个字段：`app_id` 是 Wayland 原生标识（通常等于 desktop
/// 文件的文件名，如 `firefox`、`org.gimp.GIMP`），`title` 是窗口标题。
/// 不暴露窗口几何、PID 等额外信息——这是为了保持接口最小，
/// 也让后续新增 compositor 后端时不必承诺它们拿不到的能力。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Focus {
    pub app_id: Option<String>,
    pub title: Option<String>,
}

/// 各类 compositor 后端的统一接口。
///
/// `run` 是阻塞的：后端自己持有事件循环，通过 channel 把焦点变化推给调用方。
/// 这样上层（CLI / D-Bus 服务）不需要知道底层是 wlroots、GNOME 还是 KWin。
pub trait Backend: Send {
    /// 机器可读标识，取值 "wlroots" | "kde" | "gnome"。
    /// 供 `--backend <id>` 参数与自动探测顺序使用。
    fn id(&self) -> &'static str;

    /// 人可读名称（用于 CLI 输出与日志）。
    fn name(&self) -> &'static str;

    /// 轻量探测；`Err` 文案必须面向用户可操作（指明缺什么、怎么装）。
    fn probe(&self) -> anyhow::Result<()>;

    /// 阻塞事件循环；焦点变化通过 channel 推给主循环。
    fn run(&self, tx: std::sync::mpsc::Sender<Focus>) -> anyhow::Result<()>;
}

/// 主循环去重辅助（serve / watch 共用）。
///
/// 后端允许在内部做去重优化（如 wlroots 的 emit_if_changed），
/// 但对外的语义正确性由这里保证：只有与上一次快照真正不同才放行。
/// 把去重收敛到主循环一处，是为了 D-Bus 信号不重复发射——
/// KWin 推送与 GNOME 轮询路径都可能出现同值重发。
#[derive(Debug, Default)]
pub struct Dedup {
    last: Option<Focus>,
}

impl Dedup {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// 写入新快照；与上次相同则返回 `None`，变化则返回 `Some(新快照)`。
    pub fn install(&mut self, focus: Focus) -> Option<Focus> {
        if self.last.as_ref() == Some(&focus) {
            log::debug!("dedup: 与上次快照相同，忽略");
            return None;
        }
        log::debug!("dedup: 快照变化 -> {:?}", focus);
        self.last = Some(focus.clone());
        Some(focus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_只放行真正变化() {
        let mut d = Dedup::new();
        let f1 = Focus { app_id: Some("a".into()), title: Some("t".into()) };
        let f1_same = Focus { app_id: Some("a".into()), title: Some("t".into()) };
        let f2 = Focus { app_id: Some("b".into()), title: None };

        // 首次快照视为变化（初始状态要对外可见）
        assert_eq!(d.install(f1.clone()), Some(f1.clone()));
        // 同值重发被去重
        assert_eq!(d.install(f1_same), None);
        // 真正变化才放行
        assert_eq!(d.install(f2.clone()), Some(f2.clone()));
        // last 反映最新快照
        assert_eq!(d.last(), Some(&f2));
    }
}
