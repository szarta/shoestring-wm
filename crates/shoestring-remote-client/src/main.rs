//! `shoestring-remote-client` — the remote-desktop **viewer**.
//!
//! Runs on the machine you sit at. Connects out to a remote box's
//! `shoestring-remote-server` (over an `ssh -L` tunnel or plain loopback),
//! presents that desktop fullscreen as a `wlr-layer-shell` surface, and — once
//! it is the active view on the local WM's machine-axis (A7) — forwards every
//! local input event the WM hands it to the remote. Three fds in one
//! single-threaded `poll(2)` loop (mirrors `shoestring-bar`):
//!
//! - **wayland** — presentation only (the WM owns input in capture mode);
//! - **WM IPC** — `register_remote_client`, then `view_changed` (map/unmap) +
//!   `captured_input` (the local input to relay);
//! - **network** — `ServerMessage` frames in, `ClientMessage` input out.
//!
//! See `docs/ipc.rst` (machine axis) and the `shoestring-remote` crate (wire).

mod cursor;
mod net;
mod wm;

use std::net::TcpStream;
use std::os::fd::{AsFd, AsRawFd, RawFd};

use anyhow::{Context, Result};
use memmap2::MmapMut;
use shoestring_ipc::{Event as IpcEvent, RawInput};
use shoestring_remote::{apply_tile, ClientMessage, CursorUpdate, ServerMessage};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_registry::WlRegistry,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::cursor::Cursor;
use crate::net::Net;
use crate::wm::Wm;

fn main() -> Result<()> {
    let cli = match parse_cli()? {
        CliAction::Run(c) => c,
        CliAction::Help => {
            print!("{}", help_text());
            return Ok(());
        }
        CliAction::Version => {
            println!("shoestring-remote-client {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    };
    init_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), connect = %cli.connect, label = %cli.label, "shoestring-remote-client starting");
    run(cli)
}

// ---- CLI -----------------------------------------------------------------

struct Cli {
    /// `host:port` of the remote server (reached over an ssh -L tunnel or loopback).
    connect: String,
    /// Machine name shown on the local machine-axis (defaults to the host part).
    label: String,
}

enum CliAction {
    Run(Cli),
    Help,
    Version,
}

fn parse_cli() -> Result<CliAction> {
    let mut connect: Option<String> = None;
    let mut label: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--connect" => {
                connect = Some(
                    args.next()
                        .context("--connect needs a host:port argument")?,
                )
            }
            "--label" => label = Some(args.next().context("--label needs a name argument")?),
            other => anyhow::bail!("unknown argument: {other} (try --help)"),
        }
    }
    let connect = connect.context("--connect <host:port> is required (try --help)")?;
    // Default the label to the host part of the connect target.
    let label = label.unwrap_or_else(|| {
        connect
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_else(|| connect.clone())
    });
    Ok(CliAction::Run(Cli { connect, label }))
}

fn help_text() -> String {
    format!(
        "shoestring-remote-client {}\n\
         View and drive a remote shoestring-wm desktop.\n\n\
         USAGE:\n  shoestring-remote-client --connect <host:port> [--label <name>]\n\n\
         OPTIONS:\n\
         \x20 --connect <host:port>  Remote shoestring-remote-server endpoint (e.g. an\n\
         \x20                        `ssh -L 7355:127.0.0.1:7355` tunnel: 127.0.0.1:7355).\n\
         \x20 --label <name>         Name shown on the local machine-axis (default: host).\n\
         \x20 -h, --help             Print this help.\n\
         \x20 -V, --version          Print version.\n\n\
         Switch to the remote with Super+J/K on the local WM; Super+Escape returns local.\n",
        env!("CARGO_PKG_VERSION")
    )
}

fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = match std::env::var_os("SHOESTRING_REMOTE_CLIENT_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_REMOTE_CLIENT_LOG path");
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_filter(env)
                .boxed()
        }
        None => tracing_subscriber::fmt::layer().with_filter(env).boxed(),
    };
    tracing_subscriber::registry().with(fmt_layer).init();
}

// ---- presentation buffer -------------------------------------------------

/// One reused shm buffer (tempfile-backed), the EMFILE-safe pattern from
/// `shoestring-bar`: the `wl_buffer` is destroyed only when the frame size
/// changes, never per-present.
struct Buf {
    tmp: std::fs::File,
    buffer: WlBuffer,
    dims: (u32, u32),
}

// ---- state ---------------------------------------------------------------

struct State {
    // Wayland globals + our surface.
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    viewporter: Option<WpViewporter>,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    viewport: Option<WpViewport>,
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,

    /// Logical surface size from the layer-surface `configure`.
    surface_size: Option<(u32, u32)>,
    /// Fractional scale (1.0 until a `preferred_scale` arrives).
    scale: f64,
    running: bool,

    // Presentation.
    /// The reconstructed remote frame, tightly-packed ARGB8888, `fb_w`×`fb_h`.
    fb: Vec<u8>,
    fb_w: u32,
    fb_h: u32,
    buffer: Option<Buf>,
    /// `true` while this client is the active machine-axis view (mapped + relaying).
    active: bool,
    /// `true` once a configure has been acked (we know the surface size).
    configured: bool,
    /// Latest remote cursor: `Some((x,y,icon))` to draw, `None` = hidden.
    cursor_pos: Option<(i32, i32, String)>,
    cursor: Cursor,
    /// A frame changed since the last present.
    dirty: bool,
    /// Count of frames applied (for a one-time "frames flowing" log).
    frames: u64,
}

impl State {
    /// (Re)allocate `fb` to the remote's pixel size from `Ready`/`Resize`.
    fn resize_fb(&mut self, w: u32, h: u32) {
        self.fb_w = w;
        self.fb_h = h;
        self.fb = vec![0u8; w as usize * h as usize * 4];
        self.dirty = true;
    }

    /// Present the current `fb` (with the cursor composited on top) to the
    /// surface, scaling it to the surface via the viewport. No-op until we have
    /// a surface, a configure, and a non-empty frame.
    fn present(&mut self, qh: &QueueHandle<State>) {
        if !self.active || self.fb_w == 0 || self.fb_h == 0 {
            return;
        }
        let Some((surf_w, surf_h)) = self.surface_size else {
            return;
        };
        let Some(surface) = self.surface.clone() else {
            return;
        };
        let (pw, ph) = (self.fb_w, self.fb_h);
        let stride = pw as i32 * 4;
        let size = (stride as usize) * ph as usize;

        // (Re)allocate the shm buffer only when the frame size changes.
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

        // Copy the frame into the mapping, then composite the cursor on top
        // (kept out of `fb` so it doesn't smear across damage updates).
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
            if let Some((cx, cy, icon)) = &self.cursor_pos {
                self.cursor
                    .draw(&mut mmap, pw, ph, (*cx, *cy), icon, self.scale);
            }
        }

        let buf = self.buffer.as_ref().expect("buffer set above");
        surface.attach(Some(&buf.buffer), 0, 0);
        if let Some(vp) = self.viewport.as_ref() {
            // Scale the remote-sized frame to the logical surface size.
            vp.set_destination(surf_w as i32, surf_h as i32);
        }
        surface.damage_buffer(0, 0, pw as i32, ph as i32);
        surface.commit();
    }

    /// Reveal the surface (next `present` attaches a buffer).
    fn map(&mut self) {
        self.active = true;
        self.dirty = true;
    }

    /// Hide the surface by attaching a null buffer.
    fn unmap(&mut self) {
        self.active = false;
        if let Some(surface) = self.surface.as_ref() {
            surface.attach(None, 0, 0);
            surface.commit();
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // ---- Wayland boot ----
    let conn =
        Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositor (>=v4) missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=5, ())
        .context("zwlr_layer_shell_v1 missing (no layer-shell support?)")?;
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    let mut state = State {
        compositor,
        shm,
        layer_shell,
        viewporter,
        surface: None,
        layer_surface: None,
        viewport: None,
        fractional: None,
        surface_size: None,
        scale: 1.0,
        running: true,
        fb: Vec::new(),
        fb_w: 0,
        fb_h: 0,
        buffer: None,
        active: false,
        configured: false,
        cursor_pos: None,
        cursor: Cursor::load(),
        dirty: false,
        frames: 0,
    };

    // Fullscreen Overlay layer surface: anchored to all four edges, exclusive
    // zone -1 so it ignores the bar's reserved strip and covers the whole
    // output. No keyboard interactivity — in capture mode the WM delivers input
    // over IPC, not through this surface. We don't attach a buffer until we're
    // the active view (map/unmap on view_changed).
    let surface = state.compositor.create_surface(&qh, ());
    let layer_surface = state.layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "shoestring-remote-client".to_string(),
        &qh,
        (),
    );
    layer_surface.set_size(0, 0);
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    if let (Some(vp), Some(mgr)) = (&state.viewporter, &fractional_mgr) {
        state.viewport = Some(vp.get_viewport(&surface, &qh, ()));
        state.fractional = Some(mgr.get_fractional_scale(&surface, &qh, ()));
    }
    surface.commit();
    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    // Round-trip so the compositor sends the configure → we learn the local
    // output size for the Hello (advisory: the server serves its own size today).
    queue
        .roundtrip(&mut state)
        .context("initial wayland roundtrip")?;
    let (hello_w, hello_h) = state.surface_size.unwrap_or((1920, 1080));
    let hello_scale = state.scale;

    // ---- Network handshake (blocking) ----
    tracing::info!(addr = %cli.connect, "connecting to remote server");
    let mut stream = TcpStream::connect(&cli.connect)
        .with_context(|| format!("connect to remote server {}", cli.connect))?;
    stream.set_nodelay(true).ok();
    shoestring_remote::write_framed(
        &mut stream,
        &ClientMessage::Hello {
            width: hello_w,
            height: hello_h,
            scale: hello_scale,
        },
    )
    .map_err(|e| anyhow::anyhow!("send Hello: {e}"))?;

    // Read until Ready (a well-behaved server sends it first).
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
            ServerMessage::Bye => anyhow::bail!("server closed before Ready"),
            other => tracing::debug!(?other, "ignoring pre-Ready message"),
        }
    };
    tracing::info!(rw, rh, rscale, "remote ready");
    state.resize_fb(rw, rh);

    stream
        .set_nonblocking(true)
        .context("set remote stream non-blocking")?;
    let mut net = Net::from_stream(stream);

    // ---- Register on the local machine-axis ----
    // Use the remote's pixel size so the WM clamps the forwarded pointer to it.
    let mut wm = Wm::register(&cli.label, rw, rh).context("register on local WM machine-axis")?;
    let my_index = wm.my_index();

    tracing::info!("entering poll loop");
    event_loop(
        &conn, &mut queue, &qh, &mut state, &mut net, &mut wm, my_index,
    )
}

#[allow(clippy::too_many_arguments)]
fn event_loop(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
    net: &mut Net,
    wm: &mut Wm,
    my_index: u8,
) -> Result<()> {
    while state.running {
        // Present a fresh frame if one arrived and we're the active view.
        if state.dirty && state.active {
            state.dirty = false;
            state.present(qh);
        }

        conn.flush()?;
        let read_guard = conn.prepare_read();
        let wl_fd = read_guard.as_ref().map(|g| g.connection_fd().as_raw_fd());

        // Build the pollfd set: wayland, WM IPC, network.
        let mut pfds: [libc::pollfd; 3] = unsafe { std::mem::zeroed() };
        let mut nfds = 0usize;
        let mut wl_idx = None;
        if let Some(fd) = wl_fd {
            pfds[nfds] = pollfd(fd, libc::POLLIN);
            wl_idx = Some(nfds);
            nfds += 1;
        }
        let wm_idx = nfds;
        pfds[nfds] = pollfd(wm.as_raw_fd(), libc::POLLIN);
        nfds += 1;
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

        // Wayland.
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

        // Network: writable first (drain staged input), then readable (frames).
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

        // Local WM IPC: view changes + captured input to relay.
        if pfds[wm_idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match wm.drain_events() {
                Ok(events) => {
                    for ev in events {
                        handle_wm_event(state, net, my_index, ev);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "local WM connection lost; exiting");
                    break;
                }
            }
        }

        queue.dispatch_pending(state)?;
    }

    // Best-effort orderly disconnect.
    let _ = net.send(&ClientMessage::Bye);
    let _ = net.flush_out();
    Ok(())
}

/// Apply every buffered `ServerMessage`. Returns `true` if the server said `Bye`.
fn drain_server_messages(state: &mut State, net: &mut Net) -> Result<bool> {
    while let Some(msg) = net.next_message()? {
        match msg {
            ServerMessage::Ready { width, height, .. } => {
                // A late Ready (shouldn't happen post-handshake) just resizes.
                if (width, height) != (state.fb_w, state.fb_h) {
                    state.resize_fb(width, height);
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
                    tracing::info!(seq, tiles = tiles.len(), "first frame received");
                }
                state.dirty = true;
            }
            ServerMessage::Cursor(update) => {
                state.cursor_pos = match update {
                    CursorUpdate::Hidden => None,
                    CursorUpdate::Shape { x, y, icon } => Some((x, y, icon)),
                };
                state.dirty = true;
            }
            ServerMessage::Resize {
                width,
                height,
                scale,
            } => {
                tracing::info!(width, height, scale, "remote resized");
                state.resize_fb(width, height);
            }
            ServerMessage::Bye => return Ok(true),
        }
    }
    Ok(false)
}

/// React to a local-WM event: reveal/hide on view changes, relay captured input.
fn handle_wm_event(state: &mut State, net: &mut Net, my_index: u8, ev: IpcEvent) {
    match ev {
        IpcEvent::ViewChanged { index, .. } => {
            let now_active = index == my_index;
            if now_active && !state.active {
                tracing::info!("became the active view; mapping");
                state.map();
            } else if !now_active && state.active {
                tracing::info!("no longer the active view; unmapping");
                state.unmap();
            }
        }
        IpcEvent::CapturedInput { event } => {
            if !state.active {
                return; // only relay while we're the view (defensive)
            }
            let msg = match event {
                RawInput::Key { keycode, pressed } => ClientMessage::Key { keycode, pressed },
                RawInput::Button { button, pressed } => {
                    ClientMessage::PointerButton { button, pressed }
                }
                RawInput::Motion { x, y } => ClientMessage::PointerMotion { x, y },
                RawInput::Axis {
                    horizontal,
                    vertical,
                } => ClientMessage::PointerAxis {
                    horizontal,
                    vertical,
                },
            };
            if let Err(e) = net.send(&msg) {
                tracing::warn!(error = %e, "relaying input to server failed");
            }
        }
        _ => {}
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
    ZwlrLayerShellV1,
    WpViewporter,
    WpViewport,
    WpFractionalScaleManagerV1,
);

// registry_queue_init handles wl_registry; this satisfies the Dispatch bound.
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                let w = width.max(1);
                let h = height.max(1);
                state.surface_size = Some((w, h));
                state.configured = true;
                if state.active {
                    state.dirty = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                tracing::info!("layer surface closed by compositor; exiting");
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

#[cfg(test)]
mod tests {
    use super::*;

    // The captured_input -> ClientMessage mapping is the load-bearing relay; lock
    // each variant so a wire/RawInput drift is caught here, not at runtime.
    fn relay(ev: RawInput) -> ClientMessage {
        match ev {
            RawInput::Key { keycode, pressed } => ClientMessage::Key { keycode, pressed },
            RawInput::Button { button, pressed } => {
                ClientMessage::PointerButton { button, pressed }
            }
            RawInput::Motion { x, y } => ClientMessage::PointerMotion { x, y },
            RawInput::Axis {
                horizontal,
                vertical,
            } => ClientMessage::PointerAxis {
                horizontal,
                vertical,
            },
        }
    }

    #[test]
    fn captured_input_maps_to_client_message() {
        assert_eq!(
            relay(RawInput::Key {
                keycode: 30,
                pressed: true
            }),
            ClientMessage::Key {
                keycode: 30,
                pressed: true
            }
        );
        assert_eq!(
            relay(RawInput::Button {
                button: 0x110,
                pressed: false
            }),
            ClientMessage::PointerButton {
                button: 0x110,
                pressed: false
            }
        );
        assert_eq!(
            relay(RawInput::Motion { x: 4.0, y: 8.0 }),
            ClientMessage::PointerMotion { x: 4.0, y: 8.0 }
        );
        assert_eq!(
            relay(RawInput::Axis {
                horizontal: 1.0,
                vertical: -2.0
            }),
            ClientMessage::PointerAxis {
                horizontal: 1.0,
                vertical: -2.0
            }
        );
    }
}
