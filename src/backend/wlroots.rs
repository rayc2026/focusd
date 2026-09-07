use std::collections::HashMap;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use wayland_client::{
    backend::ObjectId,
    globals::registry_queue_init,
    protocol::wl_registry,
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::{Backend, Focus};

/// zwlr_foreign_toplevel_handle_v1.state 里 activated 的枚举值。
const STATE_ACTIVATED: u32 = 1;

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
    fn emit_if_changed(&mut self) {
        let cur = self.current();
        if cur == self.last {
            return;
        }
        if let Some(f) = &cur {
            let _ = self.tx.send(f.clone());
        }
        self.last = cur;
    }
}

pub struct WlrootsBackend;

impl Backend for WlrootsBackend {
    fn name(&self) -> &'static str {
        "wlroots (zwlr-foreign-toplevel-management-unstable-v1)"
    }

    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        let conn = Connection::connect_to_env()
            .context("无法连接 Wayland display。请确认 WAYLAND_DISPLAY 已设置且在 Wayland 会话中")?;

        let (globals, mut event_queue) = registry_queue_init(&conn)
            .context("无法初始化 Wayland registry")?;
        let qh = event_queue.handle();

        // 版本 3 是 wlroots 各 compositor 普遍支持的版本。
        // 若绑定失败，说明当前 compositor 不是 wlroots 系（如 GNOME / KDE），
        // 需要走其它后端。
        let _manager: ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .context("当前 compositor 未实现 zwlr_foreign_toplevel_manager_v1。\
                     本后端仅支持 wlroots 系（Sway / Hyprland / river / labwc 等）。\
                     GNOME / KDE 支持尚在实现中")?;

        let mut state = State {
            toplevels: HashMap::new(),
            tx,
            last: None,
        };

        log::info!("已连接 {}", self.name());

        loop {
            event_queue.blocking_dispatch(&mut state)?;
        }
    }
}

// ---- registry：不做处理，global 绑定已在 run() 中通过 globals.bind 完成 ----
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &(),
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
                state.toplevels.insert(toplevel.id(), Toplevel::default());
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                log::warn!("toplevel manager 已结束，不再接收新窗口事件");
            }
            _ => {}
        }
    }
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
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                // state 是 array<uint32>，Rust 绑定给的是原始字节。
                let activated = raw
                    .chunks_exact(4)
                    .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == STATE_ACTIVATED);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.activated = activated;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                state.emit_if_changed();
            }
            // done 表示这批属性已发送完毕，是统一的提交点。
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                state.emit_if_changed();
            }
            _ => {}
        }
    }
}
