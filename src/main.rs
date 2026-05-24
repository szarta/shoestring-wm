//! shoestring-menu: dmenu-style launcher for shoestring-wm.
//!
//! Top-anchored layer-shell surface with an editable single-line input.
//! Keyboard input via wl_seat + xkbcommon (Exclusive interactivity, so the
//! compositor routes all key events to the menu while it's up). Command
//! mode (PATH scan + spawn) arrives in M2; bookmarks mode in M3.

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
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

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
/// Prefix drawn before the user's query — gives a visual hint that this is
/// an input field, mirroring dmenu's "$ " prompt.
const PROMPT: &str = "> ";

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
    /// xkb context shared across keymaps (cheap to keep around).
    xkb_ctx: xkb::Context,
    /// Live xkb state after the keyboard sends us its keymap. Until then,
    /// keys are ignored — we don't try to guess a layout.
    xkb_state: Option<xkb::State>,
    /// Editable query buffer (UTF-8). cursor_byte is a byte offset, always
    /// landing on a char boundary.
    query: String,
    cursor_byte: usize,
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
    // Bind the seat so we can request a keyboard. v5+ gives us name/capabilities
    // events; we only care about capabilities here.
    let _seat: WlSeat = globals
        .bind(&qh, 5..=9, ())
        .context("wl_seat (>=v5) missing")?;

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
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
        query: String::new(),
        cursor_byte: 0,
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
    // Exclusive keyboard focus: the compositor routes all key events to us
    // while we're up, so users can type into the field without their focused
    // app stealing input. The compositor releases focus when we destroy the
    // surface (on Esc / Enter / Closed).
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
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

    // Draw prompt + query as one string so the caret math stays simple.
    let line = format!("{}{}", PROMPT, state.query);
    draw_text(&mut mmap, w, h, &state.font, FONT_PX, PADDING_X, &line, FG);
    // Caret: vertical bar at the pixel offset of (PROMPT + query[..cursor]).
    let pre_caret = format!("{}{}", PROMPT, &state.query[..state.cursor_byte]);
    let caret_x = PADDING_X + measure_text(&state.font, FONT_PX, &pre_caret);
    draw_caret(&mut mmap, w, h, caret_x, FG);

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

/// Total horizontal advance for `text` at `size_px`. No kerning — neither
/// does our renderer, so the values line up.
fn measure_text(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as i32
}

/// Draw a 1px vertical caret. Height is the font's ascent/descent band,
/// vertically centered in the strip.
fn draw_caret(mmap: &mut MmapMut, w: u32, h: u32, x: i32, color: u32) {
    if x < 0 || x >= w as i32 {
        return;
    }
    // Match draw_text's vertical placement: a band of (ascent + |descent|)
    // pixels, centered. We don't have the font here, so reuse FONT_PX as
    // a stand-in for the band height — close enough at the sizes we use.
    let band = FONT_PX.ceil() as i32;
    let top = ((h as i32 - band) / 2).max(0);
    let stride = w as usize * 4;
    let [fb, fg, fr, _fa] = color.to_le_bytes();
    for y in top..(top + band).min(h as i32) {
        let off = y as usize * stride + x as usize * 4;
        mmap[off] = fb;
        mmap[off + 1] = fg;
        mmap[off + 2] = fr;
        mmap[off + 3] = 0xFF;
    }
}

// ---- Input editing -------------------------------------------------------

/// Apply a single keysym (already translated through xkb) plus optional
/// printable UTF-8 text to the query buffer. Returns true if the redraw
/// flag should be raised.
fn handle_key(state: &mut State, sym: xkb::Keysym, utf8: &str, ctrl: bool) -> bool {
    use xkb::keysyms::*;
    let raw = sym.raw();
    if raw == KEY_Escape {
        state.running = false;
        return false;
    }
    if raw == KEY_BackSpace {
        if state.cursor_byte == 0 {
            return false;
        }
        let prev = state.query[..state.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        state.query.replace_range(prev..state.cursor_byte, "");
        state.cursor_byte = prev;
        return true;
    }
    if raw == KEY_Left {
        let prev = state.query[..state.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(state.cursor_byte);
        state.cursor_byte = prev;
        return true;
    }
    if raw == KEY_Right {
        if state.cursor_byte >= state.query.len() {
            return false;
        }
        let ch = state.query[state.cursor_byte..].chars().next().unwrap();
        state.cursor_byte += ch.len_utf8();
        return true;
    }
    if raw == KEY_Home {
        state.cursor_byte = 0;
        return true;
    }
    if raw == KEY_End {
        state.cursor_byte = state.query.len();
        return true;
    }
    // Ctrl+U: clear the line (dmenu convention).
    if ctrl && (raw == KEY_u || raw == KEY_U) {
        state.query.clear();
        state.cursor_byte = 0;
        return true;
    }
    // Insert any printable text. xkb's get_utf8 returns "" for non-character
    // keys (arrows, function keys, etc). Filter control chars — Ctrl+letter
    // would otherwise insert \x01..\x1a.
    if utf8.is_empty() || utf8.chars().any(|c| c.is_control()) {
        return false;
    }
    state.query.insert_str(state.cursor_byte, utf8);
    state.cursor_byte += utf8.len();
    true
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

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) {
                // Request a keyboard. We never destroy it — the seat keeps
                // it alive for the lifetime of the menu process.
                let _kbd = seat.get_keyboard(qh, ());
            } else {
                tracing::warn!(
                    "seat advertises no keyboard capability; menu will be uninteractive"
                );
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(format),
                fd,
                size,
            } => {
                if format != KeymapFormat::XkbV1 {
                    tracing::warn!(?format, "unsupported keymap format; ignoring");
                    return;
                }
                // Read the keymap string out of the compositor-supplied fd.
                // We map shared (PROT_READ + MAP_PRIVATE-equivalent via memmap2)
                // because the spec permits the client to write back; doing
                // it private is safer and we drop the mmap before returning.
                let mmap = unsafe { memmap2::MmapOptions::new().len(size as usize).map(&fd) };
                let mmap = match mmap {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(error = ?e, "mmap keymap fd failed");
                        return;
                    }
                };
                // The buffer is NUL-terminated per spec; trim trailing NULs
                // before treating it as a string.
                let bytes = match mmap.split(|b| *b == 0).next() {
                    Some(b) => b,
                    None => &mmap[..],
                };
                let keymap_str = match std::str::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = ?e, "keymap not valid utf-8");
                        return;
                    }
                };
                let keymap = xkb::Keymap::new_from_string(
                    &state.xkb_ctx,
                    keymap_str.to_string(),
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                );
                match keymap {
                    Some(km) => {
                        state.xkb_state = Some(xkb::State::new(&km));
                        tracing::info!("xkb keymap loaded");
                    }
                    None => tracing::error!("xkb keymap parse failed"),
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb_state) = state.xkb_state.as_mut() {
                    xkb_state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                if key_state != KeyState::Pressed {
                    return;
                }
                let Some(xkb_state) = state.xkb_state.as_ref() else {
                    return;
                };
                // Wayland delivers evdev keycodes; xkb expects them offset by 8.
                let keycode: xkb::Keycode = (key + 8).into();
                let sym = xkb_state.key_get_one_sym(keycode);
                let utf8 = xkb_state.key_get_utf8(keycode);
                let ctrl =
                    xkb_state.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
                if handle_key(state, sym, &utf8, ctrl) {
                    state.dirty = true;
                }
            }
            // Enter/Leave/RepeatInfo aren't needed for M1.
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
