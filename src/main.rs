//! shoestring-menu: dmenu-style launcher for shoestring-wm.
//!
//! M0 scaffold: top-anchored layer-shell surface across the focused output,
//! drawing a placeholder input strip. Keyboard input arrives in M1, command
//! mode in M2, bookmarks mode in M3.

use std::{
    fs,
    io::Write,
    os::fd::{AsFd, AsRawFd},
    path::PathBuf,
};

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use tracing_subscriber::EnvFilter;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

/// Input strip height in logical pixels. The dropdown (M2) will grow the
/// total surface height beyond this.
const INPUT_HEIGHT: u32 = 24;
/// Background fill, ARGB8888 (dark grey, matches the bar).
const BG: u32 = 0xFF_22_22_22;
/// Foreground (text) color.
const FG: u32 = 0xFF_FF_FF_FF;
/// Font size in pixels.
const FONT_PX: f32 = 14.0;
/// Horizontal text inset from the strip edges.
const PADDING_X: i32 = 8;
/// Prompt drawn at the left of the input field. Replaced with the live query
/// in M1.
const M0_PLACEHOLDER: &str = "shoestring-menu (M0 scaffold)";

/// Same font candidates list as the bar. The two binaries will eventually
/// share a tiny crate if a third project needs them too.
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
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
    dirty: bool,
    running: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let font = load_font().context("could not load any system font")?;

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
        .context("zwlr_layer_shell_v1 missing (compositor doesn't support layer-shell?)")?;

    let mut state = State {
        compositor,
        shm,
        layer_shell,
        font,
        surface: None,
        layer_surface: None,
        size: None,
        dirty: false,
        running: true,
    };

    let surface = state.compositor.create_surface(&qh, ());
    // Layer::Overlay so the menu floats above normal app windows. None for
    // output means "let the compositor pick" — usually the focused one.
    let layer_surface = state.layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "shoestring-menu".to_string(),
        &qh,
        (),
    );
    layer_surface.set_size(0, INPUT_HEIGHT);
    layer_surface.set_anchor(Anchor::Top | Anchor::Left | Anchor::Right);
    // Exclusive zone 0: we're an overlay, not a reserved strip — apps under
    // us keep their full window area.
    layer_surface.set_exclusive_zone(0);
    surface.commit();

    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    tracing::info!("shoestring-menu ready, waiting for configure");

    event_loop(conn, &mut queue, &qh, &mut state)
}

/// Hand-rolled event loop. M0 has no time-based wakeups, so we just block
/// on the wayland fd. M1 onward will repaint on keyboard input.
fn event_loop(
    conn: Connection,
    queue: &mut wayland_client::EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
) -> Result<()> {
    while state.running {
        if state.dirty {
            if let Some((w, h)) = state.size {
                state.dirty = false;
                if let Err(e) = redraw(state, qh, w, h) {
                    tracing::error!(error = ?e, "redraw failed");
                }
            }
        }

        conn.flush()?;
        if let Some(guard) = conn.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // -1 = block until an event arrives. M1 will switch to a short
            // timeout once we have user-visible animations (cursor blink).
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
                    tracing::warn!(error = ?e, "wayland read failed");
                }
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(state)?;
    }
    Ok(())
}

fn load_font() -> Result<Font> {
    if let Some(path) = std::env::var_os("SHOESTRING_MENU_FONT") {
        let bytes = fs::read(&path).context("$SHOESTRING_MENU_FONT unreadable")?;
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

fn redraw(state: &State, qh: &QueueHandle<State>, w: u32, h: u32) -> Result<()> {
    let surface = state.surface.as_ref().expect("surface missing in redraw");

    let stride = w as i32 * 4;
    let size = (stride as usize) * h as usize;

    let mut tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    let row_bytes = BG.to_ne_bytes().repeat(w as usize);
    for _ in 0..h {
        tmp.write_all(&row_bytes)?;
    }

    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };
    fill_bg(&mut mmap, w, h, BG);
    draw_text(
        &mut mmap,
        w,
        h,
        &state.font,
        FONT_PX,
        PADDING_X,
        M0_PLACEHOLDER,
        FG,
    );

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer =
        pool.create_buffer(0, w as i32, h as i32, stride, Format::Argb8888, qh, ());
    pool.destroy();
    drop(mmap);

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    Ok(())
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

#[allow(clippy::too_many_arguments)]
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
    let line_metrics =
        font.horizontal_line_metrics(size_px)
            .unwrap_or_else(|| fontdue::LineMetrics {
                ascent: size_px * 0.8,
                descent: -size_px * 0.2,
                line_gap: 0.0,
                new_line_size: size_px,
            });
    let band = line_metrics.ascent - line_metrics.descent;
    let baseline_y = ((h as f32 - band) / 2.0 + line_metrics.ascent).round() as i32;

    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + metrics.xmin as f32).round() as i32;
        let gy = baseline_y - (metrics.ymin + metrics.height as i32);
        blit_alpha(
            mmap,
            w,
            h,
            gx,
            gy,
            metrics.width as u32,
            metrics.height as u32,
            &bitmap,
            color,
        );
        pen_x += metrics.advance_width;
    }
}

#[allow(clippy::too_many_arguments)]
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
            let a = coverage as i32;
            for (i, fg_chan) in [fb, fg, fr].iter().enumerate() {
                let bg = mmap[off + i] as i32;
                let out = bg + (a * (*fg_chan as i32 - bg)) / 255;
                mmap[off + i] = out.clamp(0, 255) as u8;
            }
            mmap[off + 3] = 0xFF;
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
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                let w = if width == 0 { 1 } else { width };
                let h = if height == 0 { INPUT_HEIGHT } else { height };
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
