//! Graft mode — present ONE remote window as a local `xdg_toplevel`.
//!
//! Where the default (serve) mode in `main.rs` shows a whole remote desktop
//! fullscreen on a `wlr-layer-shell` surface driven by the machine-axis, graft
//! mode pulls a single remote window into the local workspace as an ordinary
//! window: the local WM tiles/moves/closes it like any app, and — because it is a
//! real focusable surface — keyboard and pointer input arrive *naturally* from
//! the local seat, which we forward to the grafted window over the tunnel. No
//! machine-axis, no capture mode.
//!
//! On the wire this reuses the whole `shoestring-remote` protocol: the client's
//! first message is `Graft { selector }` (instead of `Hello`), and the server
//! then streams that window exactly as if it were an output — `Ready` + damage
//! `Frame`s + `Resize`/`Bye`, plus a `Meta` carrying the window's app_id/title so
//! we can label the toplevel. Pixels are decoded with the shared `apply_tile`
//! into a reused shm buffer and scaled to the toplevel via `wp_viewport`, the
//! same present path serve mode uses.
//!
//! Three fds in one `poll(2)` loop: wayland (presentation + local input) and the
//! network. Unlike serve mode there is no WM-IPC fd — graft never registers.

use std::net::TcpStream;
use std::os::fd::{AsFd, AsRawFd, RawFd};

use anyhow::{Context, Result};
use memmap2::MmapMut;
use shoestring_remote::{apply_tile, ClientMessage, ServerMessage};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::net::Net;
use crate::Cli;

/// One reused shm buffer (tempfile-backed), destroyed only on a frame-size
/// change — the EMFILE-safe pattern shared with serve mode and `shoestring-bar`.
struct Buf {
    tmp: std::fs::File,
    buffer: WlBuffer,
    dims: (u32, u32),
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,

    surface: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    toplevel: Option<XdgToplevel>,
    viewport: Option<WpViewport>,
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,

    /// Logical toplevel size from the latest `xdg_toplevel` configure.
    surface_size: (u32, u32),
    /// Fractional scale of the *local* surface (1.0 until told otherwise).
    scale: f64,
    running: bool,

    /// Reconstructed remote-window frame, tightly-packed ARGB8888, `fb_w`×`fb_h`
    /// **physical** pixels.
    fb: Vec<u8>,
    fb_w: u32,
    fb_h: u32,
    /// The remote window's scale (from `Ready`/`Resize`): fb is physical, so the
    /// remote *logical* size is `fb_w/remote_scale` — the space local pointer
    /// coordinates map into.
    remote_scale: f64,
    buffer: Option<Buf>,
    configured: bool,
    dirty: bool,
    frames: u64,

    // Input: forwarded to the grafted window over the network.
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    /// Staged client messages produced by wayland dispatch, flushed to the net
    /// after each dispatch pass (dispatch can't borrow `Net`).
    outbox: Vec<ClientMessage>,
    /// Toplevel title/app_id last applied, so we only re-set on change.
    title: String,
}

impl State {
    fn resize_fb(&mut self, w: u32, h: u32, scale: f64) {
        self.fb_w = w;
        self.fb_h = h;
        self.remote_scale = if scale > 0.0 { scale } else { 1.0 };
        self.fb = vec![0u8; w as usize * h as usize * 4];
        self.dirty = true;
    }

    /// Present the current frame scaled to the toplevel via the viewport.
    fn present(&mut self, qh: &QueueHandle<State>) {
        if !self.configured || self.fb_w == 0 || self.fb_h == 0 {
            return;
        }
        let (surf_w, surf_h) = self.surface_size;
        if surf_w == 0 || surf_h == 0 {
            return;
        }
        let Some(surface) = self.surface.clone() else {
            return;
        };
        let (pw, ph) = (self.fb_w, self.fb_h);
        let stride = pw as i32 * 4;
        let size = (stride as usize) * ph as usize;

        if self.buffer.as_ref().is_none_or(|b| b.dims != (pw, ph)) {
            if let Some(old) = self.buffer.take() {
                old.buffer.destroy();
            }
            let tmp = match tempfile::tempfile().and_then(|f| {
                f.set_len(size as u64)?;
                Ok(f)
            }) {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(error = %e, "shm tempfile alloc failed");
                    return;
                }
            };
            let pool = self.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
            let buffer =
                pool.create_buffer(0, pw as i32, ph as i32, stride, Format::Argb8888, qh, ());
            pool.destroy();
            self.buffer = Some(Buf {
                tmp,
                buffer,
                dims: (pw, ph),
            });
        }

        {
            let buf = self.buffer.as_ref().expect("buffer set above");
            let mut mmap = match unsafe { MmapMut::map_mut(&buf.tmp) } {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "mmap shm buffer failed");
                    return;
                }
            };
            mmap[..self.fb.len()].copy_from_slice(&self.fb);
        }

        let buf = self.buffer.as_ref().expect("buffer set above");
        surface.attach(Some(&buf.buffer), 0, 0);
        if let Some(vp) = self.viewport.as_ref() {
            vp.set_destination(surf_w as i32, surf_h as i32);
        }
        surface.damage_buffer(0, 0, pw as i32, ph as i32);
        surface.commit();
    }

    /// Convert a surface-local (logical) pointer position to remote-window
    /// *logical* coordinates the WM injects window-local: scale by the ratio of
    /// the remote logical size to the local toplevel size.
    fn to_remote_pointer(&self, sx: f64, sy: f64) -> (f64, f64) {
        let (surf_w, surf_h) = self.surface_size;
        if surf_w == 0 || surf_h == 0 {
            return (sx, sy);
        }
        let rem_logical_w = self.fb_w as f64 / self.remote_scale;
        let rem_logical_h = self.fb_h as f64 / self.remote_scale;
        (
            sx * rem_logical_w / surf_w as f64,
            sy * rem_logical_h / surf_h as f64,
        )
    }
}

pub fn run(cli: Cli, selector: String) -> Result<()> {
    // ---- Wayland boot ----
    let conn =
        Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositor (>=v4) missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let xdg_wm_base: XdgWmBase = globals
        .bind(&qh, 1..=6, ())
        .context("xdg_wm_base missing (no xdg-shell support?)")?;
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=8, ()).ok();

    let mut state = State {
        compositor,
        shm,
        viewporter,
        fractional_mgr,
        surface: None,
        xdg_surface: None,
        toplevel: None,
        viewport: None,
        fractional: None,
        surface_size: (0, 0),
        scale: 1.0,
        running: true,
        fb: Vec::new(),
        fb_w: 0,
        fb_h: 0,
        remote_scale: 1.0,
        buffer: None,
        configured: false,
        dirty: false,
        frames: 0,
        seat,
        keyboard: None,
        pointer: None,
        outbox: Vec::new(),
        title: String::new(),
    };

    // Bind seat keyboard/pointer up front (capabilities arrive via events too,
    // but most compositors advertise both immediately).
    if let Some(seat) = state.seat.clone() {
        state.keyboard = Some(seat.get_keyboard(&qh, ()));
        state.pointer = Some(seat.get_pointer(&qh, ()));
    }

    // ---- Network handshake: Graft first, then await Ready ----
    tracing::info!(addr = %cli.connect, %selector, "connecting to remote server (graft)");
    let mut stream = TcpStream::connect(&cli.connect)
        .with_context(|| format!("connect to remote server {}", cli.connect))?;
    stream.set_nodelay(true).ok();
    shoestring_remote::write_framed(
        &mut stream,
        &ClientMessage::Graft {
            selector: selector.clone(),
        },
    )
    .map_err(|e| anyhow::anyhow!("send Graft: {e}"))?;

    let mut init_title = selector.clone();
    let (rw, rh, rscale) = loop {
        match shoestring_remote::read_framed::<_, ServerMessage>(&mut stream)
            .map_err(|e| anyhow::anyhow!("awaiting Ready: {e}"))?
        {
            ServerMessage::Ready {
                width,
                height,
                scale,
                ..
            } => break (width, height, scale),
            ServerMessage::Meta { app_id, title } => {
                init_title = pick_title(&title, &app_id, &selector);
            }
            ServerMessage::Bye => anyhow::bail!("server closed before Ready (no such window?)"),
            other => tracing::debug!(?other, "ignoring pre-Ready message"),
        }
    };
    tracing::info!(rw, rh, rscale, "remote window ready");
    state.resize_fb(rw, rh, rscale);

    // ---- Create the local toplevel now that we know the window ----
    let surface = state.compositor.create_surface(&qh, ());
    let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_app_id("shoestring-remote-client".to_string());
    toplevel.set_title(init_title.clone());
    state.title = init_title;
    if let Some(vp) = &state.viewporter {
        state.viewport = Some(vp.get_viewport(&surface, &qh, ()));
    }
    if let Some(mgr) = &state.fractional_mgr {
        state.fractional = Some(mgr.get_fractional_scale(&surface, &qh, ()));
    }
    // Suggest the remote window's logical size as the initial size.
    let init_w = (rw as f64 / rscale).round().max(1.0) as u32;
    let init_h = (rh as f64 / rscale).round().max(1.0) as u32;
    state.surface_size = (init_w, init_h);
    surface.commit();
    state.surface = Some(surface);
    state.xdg_surface = Some(xdg_surface);
    state.toplevel = Some(toplevel);

    // Round-trip so the initial xdg configure lands before we present.
    queue
        .roundtrip(&mut state)
        .context("initial wayland roundtrip")?;

    stream
        .set_nonblocking(true)
        .context("set remote stream non-blocking")?;
    let mut net = Net::from_stream(stream);

    tracing::info!("entering graft poll loop");
    event_loop(&conn, &mut queue, &qh, &mut state, &mut net)
}

fn event_loop(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
    net: &mut Net,
) -> Result<()> {
    while state.running {
        if state.dirty {
            state.dirty = false;
            state.present(qh);
        }

        // Flush any input staged during the previous dispatch.
        drain_outbox(state, net);

        conn.flush()?;
        let read_guard = conn.prepare_read();
        let wl_fd = read_guard.as_ref().map(|g| g.connection_fd().as_raw_fd());

        let mut pfds: [libc::pollfd; 2] = unsafe { std::mem::zeroed() };
        let mut nfds = 0usize;
        let mut wl_idx = None;
        if let Some(fd) = wl_fd {
            pfds[nfds] = pollfd(fd, libc::POLLIN);
            wl_idx = Some(nfds);
            nfds += 1;
        }
        let net_idx = nfds;
        let net_events = libc::POLLIN | if net.wants_write() { libc::POLLOUT } else { 0 };
        pfds[nfds] = pollfd(net.as_raw_fd(), net_events);
        nfds += 1;

        let n = unsafe { libc::poll(pfds.as_mut_ptr(), nfds as libc::nfds_t, -1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                if let Some(g) = read_guard {
                    drop(g);
                }
                continue;
            }
            return Err(err.into());
        }

        if let (Some(idx), Some(guard)) = (wl_idx, read_guard) {
            if pfds[idx].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    if !is_wouldblock(&e) {
                        tracing::warn!(error = ?e, "wayland read failed");
                    }
                }
            } else {
                drop(guard);
            }
        }

        if pfds[net_idx].revents & libc::POLLOUT != 0 {
            if let Err(e) = net.flush_out() {
                tracing::warn!(error = %e, "remote write failed; exiting");
                break;
            }
        }
        if pfds[net_idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match net.fill() {
                Ok(true) => {
                    if drain_server_messages(state, net)? {
                        tracing::info!("server ended the session");
                        break;
                    }
                }
                Ok(false) => {
                    tracing::info!("remote server hung up");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "remote read failed; exiting");
                    break;
                }
            }
        }

        queue.dispatch_pending(state)?;
    }

    let _ = net.send(&ClientMessage::Bye);
    let _ = net.flush_out();
    Ok(())
}

/// Flush queued input `ClientMessage`s to the network.
fn drain_outbox(state: &mut State, net: &mut Net) {
    for msg in state.outbox.drain(..) {
        if let Err(e) = net.send(&msg) {
            tracing::warn!(error = %e, "forwarding input to server failed");
        }
    }
}

/// Apply buffered `ServerMessage`s. Returns `true` on `Bye`.
fn drain_server_messages(state: &mut State, net: &mut Net) -> Result<bool> {
    while let Some(msg) = net.next_message()? {
        match msg {
            ServerMessage::Ready {
                width,
                height,
                scale,
                ..
            } => {
                if (width, height) != (state.fb_w, state.fb_h) {
                    state.resize_fb(width, height, scale);
                }
            }
            ServerMessage::Frame { seq, tiles } => {
                for tile in &tiles {
                    if let Err(e) = apply_tile(&mut state.fb, state.fb_w, state.fb_h, tile) {
                        tracing::warn!(error = %e, "dropping malformed tile");
                    }
                }
                state.frames += 1;
                if state.frames == 1 {
                    tracing::info!(seq, tiles = tiles.len(), "first grafted frame received");
                }
                state.dirty = true;
            }
            ServerMessage::Meta { app_id, title } => {
                let new_title = pick_title(&title, &app_id, &state.title);
                if new_title != state.title {
                    state.title = new_title.clone();
                    if let Some(tl) = state.toplevel.as_ref() {
                        tl.set_title(new_title);
                    }
                }
            }
            ServerMessage::Resize {
                width,
                height,
                scale,
            } => {
                tracing::info!(width, height, scale, "remote window resized");
                state.resize_fb(width, height, scale);
            }
            // Graft draws the local cursor over the toplevel naturally; the
            // structural remote cursor is ignored (a v1 simplification).
            ServerMessage::Cursor(_) => {}
            // Clipboard brokering isn't wired for graft in v1.
            ServerMessage::Clipboard { .. } => {}
            ServerMessage::Bye => return Ok(true),
        }
    }
    Ok(false)
}

/// Prefer the window title, then app_id, then a fallback (selector).
fn pick_title(title: &str, app_id: &str, fallback: &str) -> String {
    if !title.is_empty() {
        title.to_string()
    } else if !app_id.is_empty() {
        app_id.to_string()
    } else {
        fallback.to_string()
    }
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn is_wouldblock(e: &wayland_client::backend::WaylandError) -> bool {
    matches!(e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
}

// ---- Wayland dispatch ----------------------------------------------------

macro_rules! noop_dispatch {
    ($($proxy:ty),* $(,)?) => {
        $(impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {}
        })*
    };
}
noop_dispatch!(
    WlCompositor,
    WlShm,
    WlShmPool,
    WlBuffer,
    WlSurface,
    WpViewporter,
    WpViewport,
    WpFractionalScaleManagerV1,
);

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: <XdgWmBase as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Must pong or the compositor considers us unresponsive and kills us.
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: <XdgSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
            state.dirty = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: <XdgToplevel as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // 0×0 means "pick your own size" — keep our current suggestion.
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                state.surface_size = (width as u32, height as u32);
                state.dirty = true;
            }
            xdg_toplevel::Event::Close => {
                tracing::info!("toplevel closed by compositor/user; exiting");
                state.outbox.push(ClientMessage::Bye);
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let s = scale as f64 / 120.0;
            if (state.scale - s).abs() > f64::EPSILON {
                state.scale = s;
                state.dirty = true;
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_keyboard delivers keys only while we hold focus, so forwarding every
        // Key event is correct. `key` is a raw evdev keycode — exactly what the
        // wire (and the WM's inject_raw_keycode, which re-adds XKB's +8) expects.
        if let wl_keyboard::Event::Key { key, state: st, .. } = event {
            let pressed = matches!(st, WEnum::Value(wl_keyboard::KeyState::Pressed));
            state.outbox.push(ClientMessage::Key {
                keycode: key,
                pressed,
            });
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                let (x, y) = state.to_remote_pointer(surface_x, surface_y);
                state.outbox.push(ClientMessage::PointerMotion { x, y });
            }
            wl_pointer::Event::Button {
                button, state: st, ..
            } => {
                let pressed = matches!(st, WEnum::Value(wl_pointer::ButtonState::Pressed));
                state
                    .outbox
                    .push(ClientMessage::PointerButton { button, pressed });
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let (h, v) = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (value, 0.0),
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => (0.0, value),
                    _ => (0.0, 0.0),
                };
                if h != 0.0 || v != 0.0 {
                    state.outbox.push(ClientMessage::PointerAxis {
                        horizontal: h,
                        vertical: v,
                    });
                }
            }
            _ => {}
        }
    }
}
