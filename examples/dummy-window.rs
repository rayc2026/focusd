//! 极简 Wayland toplevel 客户端，专用于无显示环境（headless CI）的集成测试。
//!
//! 只用 wl_shm 提交一个纯色 buffer——不依赖 EGL / GPU / 字体 / dbus，
//! 因此在 GitHub Actions 这类无显卡的 runner 上也能保证 map 出窗口。
//! （实测 foot / weston-flower / zenity 在该环境下进程存活但永远不 map。）
//!
//! 用法：
//! ```text
//! dummy-window --app-id focusd.win1 [--title 标题]
//! ```

use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;

use anyhow::{Context, Result};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const W: i32 = 64;
const H: i32 = 64;

struct State {
    running: bool,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    configured_once: bool,
    app_id: String,
    title: String,
    /// create_pool 传的是 BorrowedFd，libwayland 要在 flush 时才把 fd
    /// 写进 socket，所以 File 必须保持存活到进程结束。
    keep_alive: Vec<File>,
}

impl State {
    /// 建一个 SHM buffer：/dev/shm 下临时文件写入纯色像素后立即 unlink，
    /// fd 依然有效（tmpfs），wl_shm 可正常 mmap。
    fn create_buffer(&mut self, qh: &QueueHandle<Self>) -> Option<wl_buffer::WlBuffer> {
        let shm = self.shm.as_ref()?;
        let path = format!("/dev/shm/focusd-dummy-{}.shm", std::process::id());
        let mut f = File::create(&path).ok()?;
        let pixel: u32 = 0xFF_AA_66_44; // XRGB8888
        let mut data = Vec::with_capacity((W * H * 4) as usize);
        for _ in 0..(W * H) {
            data.extend_from_slice(&pixel.to_ne_bytes());
        }
        f.write_all(&data).ok()?;
        let _ = std::fs::remove_file(&path);
        self.keep_alive.push(f.try_clone().ok()?);
        let pool = shm.create_pool(f.as_fd(), W * H * 4, qh, ());
        Some(pool.create_buffer(
            0,
            W,
            H,
            W * 4,
            wl_shm::Format::Xrgb8888,
            qh,
            (),
        ))
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut app_id = String::from("focusd.dummy");
    let mut title = String::from("dummy");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--app-id" => app_id = args.next().unwrap_or_default(),
            "--title" => title = args.next().unwrap_or_default(),
            _ => {}
        }
    }

    eprintln!("dummy: connecting...");
    let conn = Connection::connect_to_env().context("无法连接 Wayland display")?;
    eprintln!("dummy: connected");
    let (_globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    eprintln!("dummy: initial roundtrip done");

    let mut state = State {
        running: true,
        compositor: None,
        shm: None,
        wm_base: None,
        surface: None,
        xdg_surface: None,
        toplevel: None,
        configured_once: false,
        app_id,
        title,
        keep_alive: Vec::new(),
    };

    eprintln!(
        "dummy: globals after roundtrip: compositor={} shm={} wm_base={}",
        state.compositor.is_some(),
        state.shm.is_some(),
        state.wm_base.is_some(),
    );

    // registry_queue_init 已做过一次 roundtrip，globals 已进入 state
    let compositor = state
        .compositor
        .clone()
        .context("compositor 未提供 wl_compositor")?;
    let wm_base = state
        .wm_base
        .clone()
        .context("compositor 未提供 xdg_wm_base")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    eprintln!("dummy: surface / xdg_surface / toplevel created");

    // app_id 必须在首次 commit 前设置，否则 compositor 会用空值
    toplevel.set_app_id(state.app_id.clone());
    toplevel.set_title(state.title.clone());
    surface.commit();
    eprintln!("dummy: committed, entering dispatch loop (waiting for configure)");

    state.surface = Some(surface);
    state.xdg_surface = Some(xdg_surface);
    state.toplevel = Some(toplevel);

    while state.running {
        event_queue
            .blocking_dispatch(&mut state)
            .context("事件循环异常退出")?;
    }
    Ok(())
}

// ---- registry: 收集 globals ----
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            eprintln!("dummy: global {} v{} (name {})", interface, version, name);
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// ---- xdg_wm_base: 必须应答 ping，否则会被 compositor 判定无响应 ----
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _state: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

// ---- xdg_surface: 首次 configure 时 ack + attach buffer + commit ----
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            eprintln!("dummy: got configure (serial {}), acking", serial);
            xdg_surface.ack_configure(serial);
            if !state.configured_once {
                state.configured_once = true;
                if let Some(surface) = state.surface.clone() {
                    if let Some(buf) = state.create_buffer(qh) {
                        surface.attach(Some(&buf), 0, 0);
                    }
                    surface.commit();
                    eprintln!(
                        "dummy-window: mapped (app_id={})",
                        state.app_id
                    );
                }
            }
        }
    }
}

// ---- toplevel: 收到 Close 就退出，其余忽略 ----
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, xdg_toplevel::Event::Close) {
            state.running = false;
        }
    }
}

// ---- 其余对象的空实现 ----
impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
