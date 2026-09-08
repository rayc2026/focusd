use std::collections::HashMap;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::selector::{probe_wlroots, RealProbe};
use super::{Backend, Focus};

/// zwlr_foreign_toplevel_handle_v1.state 里 activated 的枚举值。
/// 协议定义：maximized=0、minimized=1、activated=2。之前误写成 1
/// （恰好是 minimized），导致 activated 永远检测不到——而旧 CI 断言
/// 又因 RUST_LOG=debug 的日志行里恰好含 app_id 字样而假绿；
/// gate 转正 + serve 的 GetFocus 断言把这个 bug 暴露了出来。
const STATE_ACTIVATED: u32 = 2;

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
    fn id(&self) -> &'static str {
        "wlroots"
    }

    fn name(&self) -> &'static str {
        "wlroots (zwlr-foreign-toplevel-management-unstable-v1)"
    }

    fn probe(&self) -> Result<()> {
        // 探测逻辑抽成纯函数放 selector（环境可注入），这里只是转发。
        probe_wlroots(&RealProbe)
    }

    fn run(&self, tx: Sender<Focus>) -> Result<()> {
        let conn = Connection::connect_to_env()
            .context("无法连接 Wayland display。请确认 WAYLAND_DISPLAY 已设置且在 Wayland 会话中")?;

        let (globals, mut event_queue) = registry_queue_init(&conn)
            .context("无法初始化 Wayland registry")?;
        let qh = event_queue.handle();

        // TODO(协议版本协商)：固定绑定 3..=3，未做协商（主理人裁定 4 登记，
        // 不在本次迭代范围）。遇到只支持 v1/v2 的旧 compositor 会绑定失败；
        // 若将来要做，应按 globals 列表里该 interface 的实际 version 放宽区间。
        //
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
// 注意：UserData 必须是 GlobalListContents 而非 ()，
// 这是 registry_queue_init 的 trait bound 要求，与自行 bind 的 global 不同。
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
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
                log::debug!("toplevel 事件: 新窗口 {:?}，加入跟踪", toplevel.id());
                state.toplevels.insert(toplevel.id(), Toplevel::default());
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                log::warn!("toplevel manager 已结束，不再接收新窗口事件");
            }
            _ => {}
        }
    }

    // toplevel 事件（opcode 0）会创建新的 ZwlrForeignToplevelHandleV1 对象，
    // 必须在这里声明它的 UserData，否则 wayland-client 会 panic：
    // "Missing event_created_child specialization for event opcode 0"
    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);
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
                log::debug!("handle {:?} app_id={:?}", id, app_id);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.app_id = Some(app_id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                log::debug!("handle {:?} title={:?}", id, title);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.title = Some(title);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                // state 是 array<uint32>，Rust 绑定给的是原始字节。
                // as_chunks::<4>() 切出完整的 4 字节块，不足一块的尾段归入 remainder
                // （正常情况为空，协议保证按 uint32 对齐）。
                log::debug!("handle {:?} state 原始字节: {:02x?}", id, raw);
                let (chunks, _remainder) = raw.as_chunks::<4>();
                let activated = chunks
                    .iter()
                    .any(|&c| u32::from_ne_bytes(c) == STATE_ACTIVATED);
                log::debug!("handle {:?} activated={}", id, activated);
                if let Some(t) = state.toplevels.get_mut(&id) {
                    t.activated = activated;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                log::debug!("handle {:?} closed", id);
                state.toplevels.remove(&id);
                state.emit_if_changed();
            }
            // done 表示这批属性已发送完毕，是统一的提交点。
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                log::debug!(
                    "handle {:?} done; current={:?}",
                    id,
                    state.current().map(|f| f.app_id)
                );
                state.emit_if_changed();
            }
            _ => {}
        }
    }
}
