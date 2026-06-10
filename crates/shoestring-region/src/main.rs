//! shoestring-region — interactive region picker for shoestring-wm.
//!
//! Covers every output with a translucent layer-shell overlay, tracks
//! pointer drag, draws a rubber-band rectangle, and on release prints
//!
//! ```text
//! OUTPUT_NAME X Y W H
//! ```
//!
//! to stdout (one line, space-separated, integers, logical coords on
//! the output the drag started on). Escape cancels with exit code 1.
//!
//! Intended consumer: `shoestring-screenshot --region`, which pipes the
//! coords to `zwlr_screencopy_v1::capture_output_region`. Roughly the
//! `slurp` equivalent — keyboard-only cancel, no fancy formats, just
//! "give me a rectangle on an output."

use std::{
    io::Write,
    os::fd::{AsFd, AsRawFd},
    process::ExitCode,
};

use anyhow::{anyhow, Context, Result};
use memmap2::MmapMut;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{self, WlShm},
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
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

/// Linux input event code for BTN_LEFT.
const BTN_LEFT: u32 = 0x110;

/// Dim overlay: ARGB8888 premultiplied. alpha=140 means roughly 55% black
/// covers the desktop — bright enough to read selection coords on the
/// screen, dark enough to make the clear "hole" pop.
const DIM_ALPHA: u8 = 140;

/// Selection border thickness (logical px).
const BORDER_PX: i32 = 2;
/// Border color, ARGB8888 premultiplied (opaque white).
const BORDER: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

struct OutputEntry {
    output: WlOutput,
    /// `wl_output.name` (v4+). None until the roundtrip arrives.
    name: Option<String>,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    /// Last configure size, surface-local (=logical px on this output).
    size: Option<(u32, u32)>,
    /// True until we've committed a buffer for the current size.
    dirty: bool,
    /// Fractional scale for this surface (1.0 until the compositor sends a
    /// `wp_fractional_scale.preferred_scale`). Buffers are rendered at
    /// `logical * scale` physical px and mapped back to logical via `viewport`.
    scale: f64,
    /// Per-surface viewport + fractional-scale objects, present only when the
    /// compositor advertises both globals (it does — see the WM's scale wiring).
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    /// HiDPI scaling globals. `Some` when the compositor advertises both
    /// wp_viewporter and wp_fractional_scale_manager_v1; without them the tool
    /// falls back to unscaled (scale 1.0) rendering.
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    outputs: Vec<OutputEntry>,

    // Seat / input.
    #[allow(dead_code)]
    seat: WlSeat,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,

    // Pointer state. `cur_surface_idx` tracks which surface the pointer is
    // currently over (Enter sets, Leave clears). `cur_pos` is in that
    // surface's local coords.
    cur_surface_idx: Option<usize>,
    cur_pos: (f64, f64),

    // Drag state. `start_idx` fixes the *output* the rectangle belongs
    // to; once a drag begins, motion on a different output is ignored
    // for the selection rect (the rendered rect stays at the last point
    // that was on the start surface).
    drag_start: Option<(usize, f64, f64)>,
    /// Anchor of the rectangle's other corner on the start surface.
    drag_cur: (f64, f64),

    /// True once we've emitted a result line; main loop exits next tick.
    finished: bool,
    /// Exit code if non-zero (Escape).
    exit_code: u8,

    /// wl_surface holding the crosshair sprite. `None` when the xcursor
    /// theme doesn't ship a crosshair-equivalent and we fall back to the
    /// compositor's default cursor. Attached / committed once at startup;
    /// reused on every Enter via `set_cursor`.
    cursor_surface: Option<WlSurface>,
    /// Cursor sprite hotspot in surface-local coords.
    cursor_hotspot: (i32, i32),
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("shoestring-region: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    let conn = Connection::connect_to_env().context("connect WAYLAND_DISPLAY")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositor (>=v4) missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=5, ())
        .context("zwlr_layer_shell_v1 missing (compositor doesn't support layer-shell?)")?;
    let seat: WlSeat = globals
        .bind(&qh, 5..=9, ())
        .context("wl_seat (>=v5) missing")?;
    // Optional: present under shoestring-wm, absent under bare compositors.
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    let outputs: Vec<OutputEntry> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| {
                let output =
                    globals
                        .registry()
                        .bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, ());
                OutputEntry {
                    output,
                    name: None,
                    surface: None,
                    layer_surface: None,
                    size: None,
                    dirty: false,
                    scale: 1.0,
                    viewport: None,
                    fractional: None,
                }
            })
            .collect()
    });
    if outputs.is_empty() {
        return Err(anyhow!("compositor advertised no outputs"));
    }

    let mut state = State {
        compositor,
        shm,
        layer_shell,
        viewporter,
        fractional_mgr,
        outputs,
        seat,
        pointer: None,
        keyboard: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
        cur_surface_idx: None,
        cur_pos: (0.0, 0.0),
        drag_start: None,
        drag_cur: (0.0, 0.0),
        finished: false,
        exit_code: 0,
        cursor_surface: None,
        cursor_hotspot: (0, 0),
    };

    // One layer surface per output, anchored to all four edges so the
    // compositor sizes it to the full output. exclusive_zone = -1 so we
    // overlay any reserved strips (bars). Layer::Overlay floats above
    // every regular window. Keyboard exclusive so Escape lands on us.
    for i in 0..state.outputs.len() {
        let surface = state.compositor.create_surface(&qh, ());
        let layer_surface = state.layer_shell.get_layer_surface(
            &surface,
            Some(&state.outputs[i].output),
            Layer::Overlay,
            "shoestring-region".to_string(),
            &qh,
            LayerSurfaceData { idx: i },
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        // Opt this surface into fractional scaling: a viewport lets us submit a
        // physical-resolution buffer and map it back to the logical surface
        // size, and the fractional-scale object delivers the preferred ratio.
        if let (Some(vp), Some(mgr)) = (&state.viewporter, &state.fractional_mgr) {
            state.outputs[i].viewport = Some(vp.get_viewport(&surface, &qh, ()));
            state.outputs[i].fractional =
                Some(mgr.get_fractional_scale(&surface, &qh, FractionalData { idx: i }));
        }
        surface.commit();
        state.outputs[i].surface = Some(surface);
        state.outputs[i].layer_surface = Some(layer_surface);
    }

    // Best-effort: load the crosshair sprite and commit it to a dedicated
    // surface so `set_cursor` on Enter can reuse the same buffer. Themes
    // without any crosshair-equivalent silently fall through and we
    // inherit whatever cursor the compositor was already showing.
    init_crosshair_cursor(&mut state, &qh);

    // First roundtrip: wl_output.name events arrive, layer-shell configures
    // start flowing. We don't strictly need names for drawing, but we need
    // them before emitting the final coord line.
    queue.roundtrip(&mut state).context("init roundtrip")?;

    event_loop(conn, &mut queue, &qh, &mut state)?;
    Ok(state.exit_code)
}

fn event_loop(
    conn: Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
) -> Result<()> {
    while !state.finished {
        // Redraw all dirty surfaces that have a size.
        for i in 0..state.outputs.len() {
            if state.outputs[i].dirty && state.outputs[i].size.is_some() {
                if let Err(e) = redraw_output(state, qh, i) {
                    eprintln!("shoestring-region: redraw idx={i}: {e:#}");
                }
            }
        }

        conn.flush().context("flush wayland")?;

        if let Some(guard) = conn.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // No timeout — drag updates and Escape are the only inputs we
            // care about; both arrive as wayland events.
            let n = unsafe { libc::poll(&mut pfd, 1, -1) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if n > 0 && pfd.revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    eprintln!("shoestring-region: wayland read: {e:#}");
                }
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(state).context("dispatch")?;
    }
    // Make sure the final destroy requests reach the compositor before
    // we exit (otherwise the overlay can stay on screen for a beat).
    for entry in state.outputs.iter_mut() {
        if let Some(ls) = entry.layer_surface.take() {
            ls.destroy();
        }
        if let Some(s) = entry.surface.take() {
            s.destroy();
        }
    }
    let _ = conn.roundtrip();
    Ok(())
}

/// Marks every surface dirty when shared selection state changes.
fn mark_all_dirty(state: &mut State) {
    for e in state.outputs.iter_mut() {
        e.dirty = true;
    }
}

/// Commit the current selection. Prints the result line and flips the
/// `finished` flag so the main loop exits.
fn commit_selection(state: &mut State) {
    let Some((idx, ax, ay)) = state.drag_start else {
        // No drag started — treat as cancel.
        state.exit_code = 1;
        state.finished = true;
        return;
    };
    let (bx, by) = state.drag_cur;
    let x = ax.min(bx).round() as i32;
    let y = ay.min(by).round() as i32;
    let w = (ax - bx).abs().round() as i32;
    let h = (ay - by).abs().round() as i32;
    // Clamp to surface bounds.
    let (sw, sh) = state.outputs[idx].size.unwrap_or((0, 0));
    let x = x.clamp(0, sw as i32);
    let y = y.clamp(0, sh as i32);
    let w = w.min(sw as i32 - x).max(0);
    let h = h.min(sh as i32 - y).max(0);
    if w < 1 || h < 1 {
        state.exit_code = 1;
        state.finished = true;
        return;
    }
    let name = state.outputs[idx]
        .name
        .clone()
        .unwrap_or_else(|| format!("output{idx}"));
    let line = format!("{name} {x} {y} {w} {h}");
    // Emit the result line. Stdout is the contract with consumers
    // (shoestring-screenshot --region). Newline-terminated, one shot.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    state.finished = true;
}

// ---- Drawing -------------------------------------------------------------

fn redraw_output(state: &mut State, qh: &QueueHandle<State>, idx: usize) -> Result<()> {
    // `(lw, lh)` is the logical surface size from the layer-shell configure;
    // the buffer is rendered at `(pw, ph)` physical px and mapped back down to
    // logical via the viewport (set below). Pointer/selection coords stay
    // logical throughout — only the pixels we stamp scale up.
    let (lw, lh) = match state.outputs[idx].size {
        Some(s) => s,
        None => return Ok(()),
    };
    if lw == 0 || lh == 0 {
        return Ok(());
    }
    let scale = state.outputs[idx].scale;
    let pw = ((lw as f64) * scale).round().max(1.0) as u32;
    let ph = ((lh as f64) * scale).round().max(1.0) as u32;

    let stride = pw as i32 * 4;
    let size = (stride as usize) * ph as usize;
    let tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };

    // Selection rect on *this* surface (logical px). Only the start-output
    // surface shows the rect; the rest are uniform dim.
    let rect = if matches!(state.drag_start, Some((sidx, ..)) if sidx == idx) {
        let (ax, ay) = match state.drag_start {
            Some((_, ax, ay)) => (ax, ay),
            None => unreachable!(),
        };
        let (bx, by) = state.drag_cur;
        let x0 = ax.min(bx).round() as i32;
        let y0 = ay.min(by).round() as i32;
        let x1 = ax.max(bx).round() as i32;
        let y1 = ay.max(by).round() as i32;
        Some((
            x0.clamp(0, lw as i32),
            y0.clamp(0, lh as i32),
            x1.clamp(0, lw as i32),
            y1.clamp(0, lh as i32),
        ))
    } else {
        None
    };

    paint(&mut mmap, pw, ph, scale, rect);

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        pw as i32,
        ph as i32,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    drop(mmap);

    let surface = match state.outputs[idx].surface.as_ref() {
        Some(s) => s.clone(),
        None => return Ok(()),
    };
    surface.attach(Some(&buffer), 0, 0);
    // Map the physical buffer back onto the logical surface size.
    if let Some(vp) = state.outputs[idx].viewport.as_ref() {
        vp.set_destination(lw as i32, lh as i32);
    }
    surface.damage_buffer(0, 0, pw as i32, ph as i32);
    surface.commit();
    state.outputs[idx].dirty = false;
    // The compositor takes ownership of the buffer until release; we drop
    // our handle, which only sends the destroy request — the compositor
    // queues it to take effect once the buffer's actually free.
    buffer.destroy();
    Ok(())
}

/// Paint the surface: solid dim everywhere, then (optionally) punch a
/// transparent hole and stamp a border around the selection rect. `w`/`h` are
/// physical (buffer) px; `rect` is in logical coords and is scaled here, so the
/// border stays a constant logical thickness across HiDPI scales.
fn paint(mmap: &mut MmapMut, w: u32, h: u32, scale: f64, rect: Option<(i32, i32, i32, i32)>) {
    let stride = w as usize * 4;
    let dim = [0u8, 0u8, 0u8, DIM_ALPHA];
    // Fill the whole surface with dim.
    {
        let mut row = vec![0u8; stride];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&dim);
        }
        for y in 0..h as usize {
            let off = y * stride;
            mmap[off..off + stride].copy_from_slice(&row);
        }
    }

    let Some((x0, y0, x1, y1)) = rect else {
        return;
    };
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // Logical -> physical.
    let px = |v: i32| ((v as f64) * scale).round() as i32;
    let (x0, y0, x1, y1) = (px(x0), px(y0), px(x1), px(y1));
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1 as usize).min(w as usize);
    let y1 = (y1 as usize).min(h as usize);

    // Clear inside (transparent).
    let clear = [0u8, 0u8, 0u8, 0u8];
    for y in y0..y1 {
        let off = y * stride + x0 * 4;
        let end = y * stride + x1 * 4;
        let row = &mut mmap[off..end];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&clear);
        }
    }

    // Border: outline drawn just *outside* the cleared rect when there's room
    // (so the drag region stays pixel-exact for the capture). Stamps as opaque
    // white in premultiplied ARGB8888. Thickness is BORDER_PX logical px.
    let bt = (((BORDER_PX as f64) * scale).round() as usize).max(1);
    let bx0 = x0.saturating_sub(bt);
    let by0 = y0.saturating_sub(bt);
    let bx1 = (x1 + bt).min(w as usize);
    let by1 = (y1 + bt).min(h as usize);

    let stamp_row = |mmap: &mut MmapMut, y: usize, lo: usize, hi: usize| {
        let off = y * stride + lo * 4;
        let end = y * stride + hi * 4;
        let row = &mut mmap[off..end];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&BORDER);
        }
    };

    // Top + bottom strips.
    for y in by0..y0 {
        stamp_row(mmap, y, bx0, bx1);
    }
    for y in y1..by1 {
        stamp_row(mmap, y, bx0, bx1);
    }
    // Left + right strips (only the interior rows, top/bottom already done).
    for y in y0..y1 {
        stamp_row(mmap, y, bx0, x0);
        stamp_row(mmap, y, x1, bx1);
    }
}

// ---- Cursor ---------------------------------------------------------------

/// Names a theme might advertise for a crosshair-style cursor. Picked in
/// order; the first one the theme has wins. Themes vary in what they
/// ship — "crosshair" is the freedesktop convention, "cross" / "tcross"
/// / "diamond_cross" are X11-era fallbacks.
const CROSSHAIR_NAMES: &[&str] = &["crosshair", "cross", "tcross", "diamond_cross"];

/// Default xcursor theme + size (mirrors the WM's loader so the picker
/// inherits the user's `XCURSOR_*` env).
const DEFAULT_THEME: &str = "default";
const DEFAULT_SIZE: u32 = 24;

/// Best-effort: load the crosshair sprite from the active xcursor theme,
/// allocate an shm buffer, and attach it to a dedicated cursor surface.
/// Failures (theme missing, no crosshair-equivalent, shm/IO trouble) just
/// leave `state.cursor_surface` as `None` — the picker still works, the
/// pointer just keeps whatever shape it had on Enter.
fn init_crosshair_cursor(state: &mut State, qh: &QueueHandle<State>) {
    let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_THEME.into());
    let size = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_SIZE);
    let theme = xcursor::CursorTheme::load(&theme_name);

    let Some(image) = load_crosshair_image(&theme, size) else {
        return;
    };

    let stride = image.width as i32 * 4;
    let buf_size = (stride as usize) * image.height as usize;
    let Ok(tmp) = tempfile::tempfile() else {
        return;
    };
    if tmp.set_len(buf_size as u64).is_err() {
        return;
    }
    let Ok(mut mmap) = (unsafe { MmapMut::map_mut(&tmp) }) else {
        return;
    };
    // xcursor's `pixels_rgba` is the file's byte order, which the X cursor
    // format stores as ARGB — same memory layout wl_shm's Argb8888 expects
    // on little-endian. Smithay uses this field the same way.
    mmap.copy_from_slice(&image.pixels_rgba);
    drop(mmap);

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), buf_size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        image.width as i32,
        image.height as i32,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();

    let surface = state.compositor.create_surface(qh, ());
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, image.width as i32, image.height as i32);
    surface.commit();
    // The compositor owns the buffer for as long as it's bound to the
    // surface; once we hand it over with attach() we can let go locally.
    buffer.destroy();

    state.cursor_surface = Some(surface);
    state.cursor_hotspot = (image.xhot as i32, image.yhot as i32);
}

fn load_crosshair_image(
    theme: &xcursor::CursorTheme,
    target_size: u32,
) -> Option<xcursor::parser::Image> {
    use std::io::Read;
    let path = CROSSHAIR_NAMES
        .iter()
        .find_map(|name| theme.load_icon(name))?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut bytes)
        .ok()?;
    let frames = xcursor::parser::parse_xcursor(&bytes)?;
    // Pick the frame whose nominal size is nearest the requested cursor
    // size — the crosshair shouldn't animate, but if a theme ships
    // multiple sizes we still want the closest match.
    frames
        .into_iter()
        .min_by_key(|f| (target_size as i32 - f.size as i32).abs())
}

// ---- Dispatch glue -------------------------------------------------------

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

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: <WlShm as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: <ZwlrLayerShellV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// wp_viewporter / wp_viewport / wp_fractional_scale_manager_v1 are all
// request-only — no events ever arrive, so their dispatch is a no-op.
impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpFractionalScaleManagerV1,
        _: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Identifies which output a `wp_fractional_scale_v1` object belongs to.
struct FractionalData {
    idx: usize,
}

impl Dispatch<WpFractionalScaleV1, FractionalData> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as Proxy>::Event,
        data: &FractionalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // `scale` is the ratio in 1/120ths (e.g. 180 == 1.5x).
            let s = scale as f64 / 120.0;
            if let Some(entry) = state.outputs.get_mut(data.idx) {
                if entry.scale != s {
                    entry.scale = s;
                    entry.dirty = true;
                }
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for entry in state.outputs.iter_mut() {
                if entry.output.id() == proxy.id() {
                    entry.name = Some(name);
                    break;
                }
            }
        }
    }
}

struct LayerSurfaceData {
    idx: usize,
}

impl Dispatch<ZwlrLayerSurfaceV1, LayerSurfaceData> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as Proxy>::Event,
        data: &LayerSurfaceData,
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
                if data.idx < state.outputs.len() {
                    state.outputs[data.idx].size = Some((width.max(1), height.max(1)));
                    state.outputs[data.idx].dirty = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // The compositor yanked the surface (output unplugged, etc).
                // Drop everything and exit as cancel.
                state.exit_code = 1;
                state.finished = true;
            }
            _ => {}
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
            if caps.contains(Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                let idx = state
                    .outputs
                    .iter()
                    .position(|o| o.surface.as_ref().map(|s| s.id()) == Some(surface.id()));
                state.cur_surface_idx = idx;
                if idx.is_some() {
                    state.cur_pos = (surface_x, surface_y);
                    if let Some((sidx, ..)) = state.drag_start {
                        if Some(sidx) == idx {
                            state.drag_cur = (surface_x, surface_y);
                            state.outputs[sidx].dirty = true;
                        }
                    }
                    // Switch the cursor to the crosshair sprite for as
                    // long as the pointer is over our overlay. The
                    // compositor reverts it automatically once the
                    // pointer leaves (no work needed on Leave).
                    if let Some(cursor) = state.cursor_surface.as_ref() {
                        let (hx, hy) = state.cursor_hotspot;
                        pointer.set_cursor(serial, Some(cursor), hx, hy);
                    }
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                let leaving = state
                    .outputs
                    .iter()
                    .position(|o| o.surface.as_ref().map(|s| s.id()) == Some(surface.id()));
                if leaving == state.cur_surface_idx {
                    state.cur_surface_idx = None;
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.cur_pos = (surface_x, surface_y);
                if let (Some((sidx, ..)), Some(cidx)) = (state.drag_start, state.cur_surface_idx) {
                    if sidx == cidx {
                        state.drag_cur = (surface_x, surface_y);
                        state.outputs[sidx].dirty = true;
                    }
                }
            }
            wl_pointer::Event::Button {
                button, state: bs, ..
            } => {
                if button != BTN_LEFT {
                    return;
                }
                match bs {
                    WEnum::Value(ButtonState::Pressed) => {
                        if let Some(cidx) = state.cur_surface_idx {
                            let (x, y) = state.cur_pos;
                            state.drag_start = Some((cidx, x, y));
                            state.drag_cur = (x, y);
                            mark_all_dirty(state);
                        }
                    }
                    WEnum::Value(ButtonState::Released) if state.drag_start.is_some() => {
                        commit_selection(state);
                    }
                    _ => {}
                }
            }
            _ => {}
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
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(format, WEnum::Value(KeymapFormat::XkbV1)) {
                    return;
                }
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &state.xkb_ctx,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                if let Ok(Some(km)) = keymap {
                    state.xkb_state = Some(xkb::State::new(&km));
                }
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                    return;
                }
                let xkb_code = key + 8;
                let sym = match state.xkb_state.as_ref() {
                    Some(xs) => xs.key_get_one_sym(xkb_code.into()),
                    None => return,
                };
                if sym == xkb::Keysym::Escape {
                    state.exit_code = 1;
                    state.finished = true;
                }
            }
            _ => {}
        }
    }
}
