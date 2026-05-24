//! shoestring-bar: a lightweight Wayland status bar.
//!
//! Layer-shell anchored to the bottom of the focused output, drawing into a
//! single `wl_shm` buffer. Window list / clock / workspace indicator land in
//! follow-up commits — this milestone just proves text rendering works.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::fd::{AsFd, AsRawFd},
    path::PathBuf,
    time::SystemTime,
};

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use tracing_subscriber::EnvFilter;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, event_created_child,
    backend::ObjectId,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1, EVT_TOPLEVEL_OPCODE},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

/// Bar height in logical pixels. Eventually configurable via TOML.
const BAR_HEIGHT: u32 = 24;
/// Background fill, ARGB8888.
const BG: u32 = 0xFF_22_22_22;
/// Foreground (text) color.
const FG: u32 = 0xFF_FF_FF_FF;
/// Font size in pixels. Picked to leave a couple of px of padding inside
/// a 24px bar; revisit when DPI/scale handling lands.
const FONT_PX: f32 = 14.0;
/// Horizontal text inset from the bar edges.
const PADDING_X: i32 = 8;

/// Search paths for a default sans font. We deliberately don't pull in
/// fontconfig — picking the first hit keeps the dep surface tiny. User
/// override via $SHOESTRING_BAR_FONT is the universal escape hatch.
const FONT_CANDIDATES: &[&str] = &[
    // Debian / Ubuntu
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    // Arch / generic Linux
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    // Fedora / RHEL
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    // FreeBSD (pkg install dejavu / liberation-fonts-ttf)
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/local/share/fonts/liberation-fonts-ttf/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/noto/NotoSans-Regular.ttf",
];

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    font: Font,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    size: Option<(u32, u32)>,
    /// Live ext-foreign-toplevel-list entries, keyed by the handle's
    /// wayland object id. Updated incrementally on each `done` event;
    /// removed on `closed`. NOTE: this protocol exposes title/app_id but
    /// NOT activation state — focus highlight needs M9 IPC (see #10).
    toplevels: HashMap<ObjectId, Toplevel>,
    /// Set whenever any input that affects the rendered output changes
    /// (toplevel arrival/departure, clock tick, etc). Cleared after the
    /// next paint.
    dirty: bool,
    /// Minute-of-epoch when we last painted; used to detect clock ticks.
    last_minute: u32,
    running: bool,
}

#[derive(Default, Clone)]
struct Toplevel {
    /// Compositor-assigned stable string id. Will be used to match the
    /// focused-window event from M9 IPC.
    identifier: String,
    title: String,
    app_id: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let font = load_font().context("could not load any system font")?;

    let conn = Connection::connect_to_env()
        .context("connect to wayland (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .context("wl_compositor (>=v4) missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=5, ())
        .context("zwlr_layer_shell_v1 missing (compositor doesn't support layer-shell?)")?;
    // Subscribe to the window list. Missing global is not fatal — bar
    // will just render without a window list (clock + future workspaces
    // still useful).
    let _toplevel_list: Option<ExtForeignToplevelListV1> = match globals.bind(&qh, 1..=1, ()) {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(error = ?e, "no ext-foreign-toplevel-list global; window list disabled");
            None
        }
    };

    let mut state = State {
        compositor,
        shm,
        layer_shell,
        font,
        surface: None,
        layer_surface: None,
        size: None,
        toplevels: HashMap::new(),
        dirty: false,
        last_minute: 0,
        running: true,
    };

    let surface = state.compositor.create_surface(&qh, ());
    let layer_surface = state.layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Bottom,
        "shoestring-bar".to_string(),
        &qh,
        (),
    );
    layer_surface.set_size(0, BAR_HEIGHT);
    layer_surface.set_anchor(Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(BAR_HEIGHT as i32);
    surface.commit();

    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    tracing::info!("shoestring-bar ready, waiting for configure");

    event_loop(conn, &mut queue, &qh, &mut state)
}

/// Hand-rolled event loop: wake on wayland fd readability OR a 1-second
/// poll timeout (whichever first), then dispatch + repaint if needed.
/// Using poll(2) keeps the bar's dep count at zero added crates for
/// timing — pulling calloop would add ~6 transitive deps for what
/// amounts to a 30-line loop.
fn event_loop(
    conn: Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
) -> Result<()> {
    while state.running {
        // 1. Repaint if the clock minute has rolled over since the last
        //    paint. Toplevel events already set state.dirty in their
        //    handlers, so we just need to add the time signal here.
        let now_minute = current_minute_id();
        if state.last_minute != now_minute {
            state.last_minute = now_minute;
            state.dirty = true;
        }
        if state.dirty {
            if let Some((w, h)) = state.size {
                state.dirty = false;
                if let Err(e) = redraw(state, qh, w, h) {
                    tracing::error!(error = ?e, "redraw failed");
                }
            }
        }

        // 2. Flush any queued requests, then prepare to read.
        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            // Sleep until either an event arrives or the next second
            // ticks. 1s granularity is fine for minute-resolution clock.
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pfd, 1, 1000) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if n > 0 && pfd.revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    tracing::warn!(error = ?e, "wayland read failed");
                }
            } else {
                // Timed out; drop the guard so the next prepare_read works.
                drop(guard);
            }
        }
        // 3. Dispatch any events we got from `read()` above OR events
        //    that wayland-rs already had buffered (prepare_read returned None).
        queue.dispatch_pending(state)?;
    }
    Ok(())
}

/// A monotonically-ish increasing identifier for the current minute,
/// derived from UNIX epoch seconds / 60. Wraps every ~136 years.
fn current_minute_id() -> u32 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 60) as u32)
        .unwrap_or(0)
}

/// Try each candidate path in order; return the first that loads.
fn load_font() -> Result<Font> {
    // Allow an explicit override via env var — handy for testing.
    if let Some(path) = std::env::var_os("SHOESTRING_BAR_FONT") {
        let bytes = fs::read(&path).context("$SHOESTRING_BAR_FONT unreadable")?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("font parse: {e}"));
    }
    for path in FONT_CANDIDATES {
        let p = PathBuf::from(path);
        if p.exists() {
            let bytes = fs::read(&p)?;
            return Font::from_bytes(bytes, FontSettings::default())
                .map_err(|e| anyhow::anyhow!("font parse: {e}"));
        }
    }
    anyhow::bail!("no font found in candidates: {FONT_CANDIDATES:?}")
}

// ---- Drawing -------------------------------------------------------------

/// Compose the bar into a freshly-allocated wl_buffer and attach it.
/// Re-runs on every configure event (and, later, on state changes).
fn redraw(state: &State, qh: &QueueHandle<State>, w: u32, h: u32) -> Result<()> {
    let surface = state.surface.as_ref().expect("surface missing in redraw");

    let stride = w as i32 * 4;
    let size = (stride as usize) * h as usize;

    let mut tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    // Pre-fill the file so the wl_shm buffer is valid even if the compositor
    // peeks at it before our mmap stamp.
    let row_bytes = BG.to_ne_bytes().repeat(w as usize);
    for _ in 0..h {
        tmp.write_all(&row_bytes)?;
    }

    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };

    // We re-fill via the mmap (faster than seek+write) so subsequent draws
    // can mutate in place. Background first, then text on top.
    fill_bg(&mut mmap, w, h, BG);

    // Right: clock. Drawn first so we know how much horizontal space
    // remains for the (truncatable) window list on the left.
    let clock = format_clock_now();
    let clock_w = measure_text(&state.font, FONT_PX, &clock);
    let clock_x = w as i32 - PADDING_X - clock_w;
    draw_text(&mut mmap, w, h, &state.font, FONT_PX, clock_x, &clock, FG);

    // Left: window list, "  |  " separated, truncated with ".." if it'd
    // collide with the clock. Empty state shows the version placeholder.
    let mut entries: Vec<&Toplevel> = state.toplevels.values().collect();
    entries.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    let raw = if entries.is_empty() {
        format!("shoestring-bar v{}", env!("CARGO_PKG_VERSION"))
    } else {
        entries
            .iter()
            .map(|t| {
                if !t.title.is_empty() {
                    t.title.clone()
                } else if !t.app_id.is_empty() {
                    t.app_id.clone()
                } else {
                    "(untitled)".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("  |  ")
    };
    // Available width = bar width − right padding − clock width − gap.
    let left_budget = (clock_x - PADDING_X - 12).max(0);
    let label = truncate_to_fit(&raw, &state.font, FONT_PX, left_budget);
    draw_text(&mut mmap, w, h, &state.font, FONT_PX, PADDING_X, &label, FG);

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride,
        Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    // The mmap-backed file lives until tmp drops at end of scope; the
    // compositor reads pixels via the fd-backed pool's wl_buffer above.
    drop(mmap);

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    Ok(())
}

/// Total horizontal advance for rendering `text` at `size_px`. Doesn't
/// account for kerning, but neither does our renderer — values match.
fn measure_text(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as i32
}

/// If `text` fits in `budget_px`, return it. Otherwise drop characters
/// from the right and append ".." until what's left fits. Returns at
/// minimum ".." (or "" if even that doesn't fit).
fn truncate_to_fit(text: &str, font: &Font, size_px: f32, budget_px: i32) -> String {
    if budget_px <= 0 {
        return String::new();
    }
    if measure_text(font, size_px, text) <= budget_px {
        return text.to_string();
    }
    let ellipsis = "..";
    let ellipsis_w = measure_text(font, size_px, ellipsis);
    if ellipsis_w > budget_px {
        return String::new();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + ellipsis;
        if measure_text(font, size_px, &candidate) <= budget_px {
            return candidate;
        }
    }
    ellipsis.to_string()
}

/// Local-time clock formatted as `Day Mon DD  HH:MM`. Uses libc to avoid
/// pulling in chrono/time — already in our tree via tempfile, so this is
/// zero added deps.
fn format_clock_now() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r is thread-safe and reentrant; the version of glibc we
    // care about supports it unconditionally.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    const WDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun",
        "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {} {:02}  {:02}:{:02}",
        WDAY.get(tm.tm_wday as usize).copied().unwrap_or("???"),
        MON.get(tm.tm_mon as usize).copied().unwrap_or("???"),
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
    )
}

fn fill_bg(mmap: &mut MmapMut, w: u32, h: u32, color: u32) {
    let bytes = color.to_ne_bytes();
    let stride = w as usize * 4;
    let row = bytes.repeat(w as usize);
    for y in 0..h as usize {
        let off = y * stride;
        mmap[off..off + stride].copy_from_slice(&row);
    }
}

/// Vertically-centered single-line text. `x_start` is the left edge (in
/// pixels). Glyphs are alpha-blended onto whatever's already in the buffer.
fn draw_text(
    mmap: &mut MmapMut,
    w: u32,
    h: u32,
    font: &Font,
    size_px: f32,
    x_start: i32,
    text: &str,
    color: u32,
) {
    // Use the font's line metrics to baseline-align: ascent above baseline,
    // descent below. Vertical-center the (ascent+descent) band inside the bar.
    let line_metrics = font.horizontal_line_metrics(size_px).unwrap_or_else(|| {
        // Fallback if the font doesn't expose horizontal metrics.
        fontdue::LineMetrics {
            ascent: size_px * 0.8,
            descent: -size_px * 0.2,
            line_gap: 0.0,
            new_line_size: size_px,
        }
    });
    let band = line_metrics.ascent - line_metrics.descent;
    let baseline_y = ((h as f32 - band) / 2.0 + line_metrics.ascent).round() as i32;

    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + metrics.xmin as f32).round() as i32;
        // fontdue's ymin is the *bottom* of the glyph relative to baseline,
        // measured upward (positive = above baseline). The top-edge y for
        // blitting is therefore baseline_y - (ymin + height).
        let gy = baseline_y - (metrics.ymin + metrics.height as i32);
        blit_alpha(mmap, w, h, gx, gy, metrics.width as u32, metrics.height as u32, &bitmap, color);
        pen_x += metrics.advance_width;
    }
}

/// Blit a single-channel coverage bitmap as `color`, alpha-blending against
/// whatever ARGB is already in the destination.
fn blit_alpha(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    dst_x: i32,
    dst_y: i32,
    src_w: u32,
    src_h: u32,
    src: &[u8],
    color: u32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    let [fb, fg, fr, _fa] = color.to_le_bytes();
    let stride = dst_w as usize * 4;
    for sy in 0..src_h as i32 {
        let dy = dst_y + sy;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..src_w as i32 {
            let dx = dst_x + sx;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let coverage = src[(sy as u32 * src_w + sx as u32) as usize];
            if coverage == 0 {
                continue;
            }
            let off = dy as usize * stride + dx as usize * 4;
            let a = coverage as i32; // 0..=255
            // out = bg + a/255 * (fg - bg) for each channel. Signed math
            // so darker-foreground-on-lighter-background still works.
            for (i, fg_chan) in [fb, fg, fr].iter().enumerate() {
                let bg = mmap[off + i] as i32;
                let out = bg + (a * (*fg_chan as i32 - bg)) / 255;
                mmap[off + i] = out.clamp(0, 255) as u8;
            }
            mmap[off + 3] = 0xFF; // keep the bar fully opaque
        }
    }
}

// ---- Wayland dispatch impls ---------------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
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
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer_surface.ack_configure(serial);
                let w = if width == 0 { 1 } else { width };
                let h = if height == 0 { BAR_HEIGHT } else { height };
                state.size = Some((w, h));
                state.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                tracing::info!("layer surface closed by compositor; exiting");
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { .. } => {
                // The new ExtForeignToplevelHandleV1 child is created
                // automatically via event_created_child! below. We don't
                // need to do anything else here — handle events arrive
                // on the child's own dispatch.
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                // Compositor stopped sending toplevels. We could drop the
                // list, but it's harmless to leave it bound.
            }
            _ => {}
        }
    }

    // Tell wayland-rs how to construct child resources announced via the
    // `toplevel` event so they land on State's Dispatch impl below.
    event_created_child!(State, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id();
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.toplevels.entry(id).or_default().title = title;
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.toplevels.entry(id).or_default().app_id = app_id;
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                state.toplevels.entry(id).or_default().identifier = identifier;
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                // `done` marks a consistent snapshot per the protocol.
                // The main loop polls dirty and repaints on next wakeup.
                state.dirty = true;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                state.dirty = true;
                handle.destroy();
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
    WlOutput,
    ZwlrLayerShellV1,
);
