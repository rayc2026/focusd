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
pub trait Backend {
    fn name(&self) -> &'static str;
    fn run(&self, tx: std::sync::mpsc::Sender<Focus>) -> anyhow::Result<()>;
}
