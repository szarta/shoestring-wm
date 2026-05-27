//! Wayland plumbing for shoestring-notify.
//!
//! Owns the live `State` (globals, surfaces, font, queue, rustbus
//! connection) and all `Dispatch` impls. Surfaces are layer-shell windows
//! anchored top-right; one per live notification. Stack margins are
//! recomputed any time the set of surfaces changes.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use rustbus::message_builder::MarshalledMessage;
use rustbus::RpcConn;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_output::WlOutput,
    wl_pointer::{self, ButtonState, WlPointer},
    wl_registry::WlRegistry,
    wl_seat::{self, Capability, WlSeat},
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

use crate::config::{Config, FONT_CANDIDATES};
use crate::dbus_iface::notification_closed_signal;
use crate::render;
use crate::state::{CloseReason, Queue};

/// One layer-shell surface per live notification. The owning HashMap is
/// keyed by notification id, so the struct does not duplicate it.
pub struct NotifySurface {
    pub wl_surface: WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    /// Size we asked the compositor for in `set_size`.
    pub requested_size: (u32, u32),
    /// `true` once the first `Configure` has arrived from the compositor;
    /// gates the first paint.
    pub configured: bool,
    /// Highest `Notification::revision` we have committed a buffer for.
    /// Drives "draw on first configure, redraw on replace".
    pub drawn_revision: Option<u64>,
    /// Margin from the top anchor edge. Recomputed on every restack.
    pub margin_top: u32,
}

pub struct State {
    pub cfg: Config,

    pub compositor: WlCompositor,
    pub shm: WlShm,
    pub layer_shell: ZwlrLayerShellV1,

    pub font: Font,

    pub queue: Queue,

    /// Per-notification surfaces keyed by notification id.
    pub surfaces: HashMap<u32, NotifySurface>,
    /// `wl_surface.id()` → notification id, so pointer Enter events can
    /// route to the right notification.
    pub surface_id_to_notif: HashMap<ObjectId, u32>,

    /// `wl_seat` kept alive for capability events.
    #[allow(dead_code)]
    pub seat: Option<WlSeat>,
    pub pointer: Option<WlPointer>,
    /// Notification id currently under the pointer, set by Enter/Leave.
    pub pointer_focus: Option<u32>,

    pub rpc: RpcConn,

    /// Set when the stack (set of surfaces) changes — main loop calls
    /// [`restack`] before the next wayland flush.
    pub stack_dirty: bool,

    pub running: bool,
}

pub fn init(cfg: Config) -> Result<(Connection, EventQueue<State>, QueueHandle<State>, State)> {
    let font = load_font(cfg.font.as_deref()).context("could not load any system font")?;

    let conn =
        Connection::connect_to_env().context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositor (>=v4) missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=5, ())
        .context("zwlr_layer_shell_v1 missing (compositor doesn't support layer-shell?)")?;
    let seat: Option<WlSeat> = match globals.bind(&qh, 1..=7, ()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = ?e, "no wl_seat global; click-to-dismiss disabled");
            None
        }
    };

    let rpc = crate::dbus_iface::acquire_bus()?;

    let state = State {
        cfg,
        compositor,
        shm,
        layer_shell,
        font,
        queue: Queue::new(),
        surfaces: HashMap::new(),
        surface_id_to_notif: HashMap::new(),
        seat,
        pointer: None,
        pointer_focus: None,
        rpc,
        stack_dirty: false,
        running: true,
    };
    Ok((conn, queue, qh, state))
}

/// Create surfaces for any notifications without one, destroy surfaces
/// whose notification disappeared, and recompute stack margins. Idempotent;
/// safe to call on every iteration.
pub fn sync_surfaces(state: &mut State, qh: &QueueHandle<State>) {
    let mut needs_restack = false;

    // Add: new notifications without a surface.
    let ids: Vec<u32> = state.queue.iter().map(|n| n.id).collect();
    for id in &ids {
        if state.surfaces.contains_key(id) {
            continue;
        }
        create_surface_for(state, qh, *id);
        needs_restack = true;
    }

    // Remove: surfaces whose notification was closed.
    let stale: Vec<u32> = state
        .surfaces
        .keys()
        .copied()
        .filter(|id| !ids.contains(id))
        .collect();
    for id in stale {
        destroy_surface(state, id);
        needs_restack = true;
    }

    if needs_restack || state.stack_dirty {
        restack(state);
        state.stack_dirty = false;
    }

    // After possible margin updates, see if any surface needs a paint.
    paint_if_needed(state, qh);
}

fn create_surface_for(state: &mut State, qh: &QueueHandle<State>, notif_id: u32) {
    let Some(notif) = state.queue.get(notif_id) else {
        return;
    };

    let layout = render::layout(
        &state.font,
        state.cfg.summary_size,
        state.cfg.body_size,
        state.cfg.padding,
        state.cfg.max_width,
        &notif.summary,
        &notif.body,
    );
    let size = (layout.width, layout.height);

    let wl_surface = state.compositor.create_surface(qh, ());
    let layer_surface = state.layer_shell.get_layer_surface(
        &wl_surface,
        None,
        Layer::Overlay,
        "shoestring-notify".to_owned(),
        qh,
        notif_id,
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Right);
    layer_surface.set_size(size.0, size.1);
    // Margin is computed properly in restack; set a placeholder so the
    // first commit doesn't paint at (0,0) before restack fires.
    layer_surface.set_margin(state.cfg.gap as i32, state.cfg.gap as i32, 0, 0);
    wl_surface.commit();

    state.surface_id_to_notif.insert(wl_surface.id(), notif_id);
    state.surfaces.insert(
        notif_id,
        NotifySurface {
            wl_surface,
            layer_surface,
            requested_size: size,
            configured: false,
            drawn_revision: None,
            margin_top: state.cfg.gap,
        },
    );
    tracing::debug!(id = notif_id, w = size.0, h = size.1, "created surface");
}

fn destroy_surface(state: &mut State, notif_id: u32) {
    if let Some(s) = state.surfaces.remove(&notif_id) {
        state.surface_id_to_notif.remove(&s.wl_surface.id());
        if state.pointer_focus == Some(notif_id) {
            state.pointer_focus = None;
        }
        s.layer_surface.destroy();
        s.wl_surface.destroy();
        tracing::debug!(id = notif_id, "destroyed surface");
    }
}

fn restack(state: &mut State) {
    let gap = state.cfg.gap;
    let mut next_top = gap;
    // Iterate in queue order (insertion order): oldest closest to the
    // anchor (top), newest below.
    for notif in state.queue.iter() {
        if let Some(s) = state.surfaces.get_mut(&notif.id) {
            if s.margin_top != next_top {
                s.margin_top = next_top;
                s.layer_surface
                    .set_margin(next_top as i32, gap as i32, 0, 0);
                s.wl_surface.commit();
            }
            next_top += s.requested_size.1 + gap;
        }
    }
}

fn paint_if_needed(state: &mut State, qh: &QueueHandle<State>) {
    let ids: Vec<u32> = state.surfaces.keys().copied().collect();
    for id in ids {
        let needs_paint = {
            let Some(s) = state.surfaces.get(&id) else {
                continue;
            };
            let Some(notif) = state.queue.get(id) else {
                continue;
            };
            s.configured && s.drawn_revision.is_none_or(|r| r < notif.revision)
        };
        if needs_paint {
            if let Err(e) = paint(state, qh, id) {
                tracing::error!(id, error = ?e, "paint failed");
            }
        }
    }
}

fn paint(state: &mut State, qh: &QueueHandle<State>, notif_id: u32) -> Result<()> {
    let notif = state
        .queue
        .get(notif_id)
        .ok_or_else(|| anyhow!("notification {notif_id} vanished mid-paint"))?
        .clone();
    let cfg = state.cfg.clone();
    let font = &state.font;

    let layout = render::layout(
        font,
        cfg.summary_size,
        cfg.body_size,
        cfg.padding,
        cfg.max_width,
        &notif.summary,
        &notif.body,
    );

    let w = layout.width;
    let h = layout.height;
    let stride = w as i32 * 4;
    let size = (stride as usize) * h as usize;

    let mut tmp = tempfile::tempfile().context("tempfile for wl_shm pool")?;
    tmp.set_len(size as u64)?;
    // Seed the file so the buffer is never observed as garbage.
    let row_bytes = cfg.background.to_ne_bytes().repeat(w as usize);
    for _ in 0..h {
        tmp.write_all(&row_bytes)?;
    }
    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };

    render::paint(
        &mut mmap,
        font,
        &layout,
        cfg.padding,
        cfg.summary_size,
        cfg.body_size,
        cfg.background,
        cfg.foreground,
    );

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer =
        pool.create_buffer(0, w as i32, h as i32, stride, Format::Argb8888, qh, ());
    pool.destroy();
    drop(mmap);

    let s = state
        .surfaces
        .get_mut(&notif_id)
        .ok_or_else(|| anyhow!("surface gone mid-paint"))?;
    s.wl_surface.attach(Some(&buffer), 0, 0);
    s.wl_surface.damage_buffer(0, 0, w as i32, h as i32);
    s.wl_surface.commit();
    s.drawn_revision = Some(notif.revision);
    // tmp drops here; the compositor holds its own dup'd fd via the pool.
    Ok(())
}

/// Close a notification: queue.close + signal + surface teardown.
/// Caller is responsible for calling sync_surfaces afterwards so any
/// restacking happens.
pub fn dismiss(state: &mut State, id: u32, reason: CloseReason) -> Result<()> {
    if state.queue.close(id) {
        let mut sig = notification_closed_signal(id, reason);
        send_message(&mut state.rpc, &mut sig)?;
        tracing::info!(id, ?reason, "dismissed");
    }
    Ok(())
}

fn send_message(rpc: &mut RpcConn, msg: &mut MarshalledMessage) -> Result<()> {
    use rustbus::connection::ll_conn::force_finish_on_error;
    rpc.send_message(msg)
        .map_err(|e| anyhow!("queue message: {e:?}"))?
        .write_all()
        .map_err(force_finish_on_error)
        .map_err(|e| anyhow!("write message: {e:?}"))?;
    Ok(())
}

fn load_font(cfg_path: Option<&Path>) -> Result<Font> {
    if let Some(p) = cfg_path {
        let bytes =
            fs::read(p).with_context(|| format!("config font: cannot read {}", p.display()))?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("font parse: {e}"));
    }
    if let Some(path) = std::env::var_os("SHOESTRING_NOTIFY_FONT") {
        let bytes = fs::read(&path).context("$SHOESTRING_NOTIFY_FONT unreadable")?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("font parse: {e}"));
    }
    for path in FONT_CANDIDATES {
        let p = std::path::PathBuf::from(path);
        if p.exists() {
            let bytes = fs::read(&p)?;
            return Font::from_bytes(bytes, FontSettings::default())
                .map_err(|e| anyhow!("font parse: {e}"));
        }
    }
    Err(anyhow!("no font found in candidates: {FONT_CANDIDATES:?}"))
}

// ---- Wayland dispatch impls ---------------------------------------------

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

impl Dispatch<ZwlrLayerSurfaceV1, u32> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as Proxy>::Event,
        udata: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let notif_id = *udata;
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width: _,
                height: _,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(s) = state.surfaces.get_mut(&notif_id) {
                    s.configured = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                tracing::debug!(id = notif_id, "layer surface closed by compositor");
                // Treat compositor close as a dismiss-by-user; the
                // notification is no longer visible regardless.
                if let Err(e) = dismiss(state, notif_id, CloseReason::DismissedByUser) {
                    tracing::error!(id = notif_id, error = ?e, "dismiss after compositor close failed");
                }
            }
            _ => {}
        }
    }
}

macro_rules! noop_dispatch {
    ($($proxy:ty),* $(,)?) => {
        $(impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as Proxy>::Event,
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
    WlOutput,
    ZwlrLayerShellV1,
);

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
            let has_pointer = caps.contains(Capability::Pointer);
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
                tracing::debug!("seat advertised pointer; bound");
            } else if !has_pointer {
                if let Some(p) = state.pointer.take() {
                    p.release();
                    state.pointer_focus = None;
                }
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _pointer: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => {
                state.pointer_focus = state.surface_id_to_notif.get(&surface.id()).copied();
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_focus = None;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Pressed),
                ..
            } => {
                // BTN_LEFT == 0x110 (Linux input event code). Right/middle
                // are currently ignored; could be wired to "close all" or
                // similar later.
                const BTN_LEFT: u32 = 0x110;
                if button != BTN_LEFT {
                    return;
                }
                let Some(id) = state.pointer_focus else {
                    return;
                };
                if let Err(e) = dismiss(state, id, CloseReason::DismissedByUser) {
                    tracing::error!(id, error = ?e, "click-dismiss failed");
                }
                state.stack_dirty = true;
            }
            _ => {}
        }
    }
}
