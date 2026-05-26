//! shoestring-confirm — modal yes/no confirmation dialog.
//!
//! Spawned by the WM (or any caller) to gate a destructive action behind
//! user confirmation. Renders a small centered prompt box over every
//! output, grabs the keyboard exclusively, and exits with
//!
//! - `0` if the user pressed Enter (confirm)
//! - `1` if the user pressed Escape, clicked outside, or the surface was
//!   closed by the compositor (cancel)
//! - `2` on protocol / IO error
//!
//! Usage:
//!
//! ```text
//! shoestring-confirm "Exit shoestring-wm?"
//! ```
//!
//! Font resolution mirrors `shoestring-bar`: `$SHOESTRING_CONFIRM_FONT`
//! first, then a small list of common system sans-serif paths.

use std::{
    env, fs,
    os::fd::{AsFd, AsRawFd},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{anyhow, Context, Result};
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_output::WlOutput,
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};
use xkbcommon::xkb;

const EXIT_OK: u8 = 0;
const EXIT_CANCELLED: u8 = 1;
const EXIT_ERROR: u8 = 2;

/// Dim overlay alpha. ~55% black.
const DIM_ALPHA: u8 = 140;

/// Dialog box dimensions (logical px). Picked to comfortably fit prompts
/// like "Exit shoestring-wm?" plus the help line.
const BOX_W: u32 = 460;
const BOX_H: u32 = 130;
/// Dialog border thickness.
const BOX_BORDER: u32 = 2;

/// ARGB8888 (premultiplied) colors.
const BOX_BG: u32 = 0xFF1E1E1E; // near-black
const BOX_BORDER_COLOR: u32 = 0xFFE0E0E0; // off-white
const TEXT_COLOR: u32 = 0xFFEEEEEE;
const HINT_COLOR: u32 = 0xFFAAAAAA;

const PROMPT_SIZE_PX: f32 = 18.0;
const HINT_SIZE_PX: f32 = 14.0;
const HINT_LINE: &str = "[Enter] confirm    [Esc] cancel";

/// Font search paths. Matches shoestring-bar's set so the two binaries
/// agree on which font shows up on a given distro.
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

struct OutputEntry {
    output: WlOutput,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    size: Option<(u32, u32)>,
    dirty: bool,
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    outputs: Vec<OutputEntry>,
    font: Font,
    prompt: String,

    #[allow(dead_code)]
    seat: WlSeat,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,

    finished: bool,
    exit_code: u8,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("shoestring-confirm: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn parse_args() -> Result<String> {
    let mut args = env::args().skip(1);
    let prompt = args
        .next()
        .ok_or_else(|| anyhow!("usage: shoestring-confirm <prompt>"))?;
    if args.next().is_some() {
        return Err(anyhow!("usage: shoestring-confirm <prompt>"));
    }
    Ok(prompt)
}

fn resolve_font_path() -> Result<PathBuf> {
    if let Ok(p) = env::var("SHOESTRING_CONFIRM_FONT") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!(
            "SHOESTRING_CONFIRM_FONT points at {} but it doesn't exist",
            path.display()
        ));
    }
    for cand in FONT_CANDIDATES {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "no system font found — set SHOESTRING_CONFIRM_FONT to a .ttf/.otf"
    ))
}

fn load_font() -> Result<Font> {
    let path = resolve_font_path()?;
    let bytes = fs::read(&path).with_context(|| format!("read font {}", path.display()))?;
    Font::from_bytes(bytes, FontSettings::default())
        .map_err(|e| anyhow!("parse font {}: {e}", path.display()))
}

fn run() -> Result<u8> {
    let prompt = parse_args()?;
    let font = load_font()?;

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
                    surface: None,
                    layer_surface: None,
                    size: None,
                    dirty: false,
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
        outputs,
        font,
        prompt,
        seat,
        pointer: None,
        keyboard: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
        finished: false,
        exit_code: 0,
    };

    // One full-output layer surface per output. Overlay layer + exclusive
    // keyboard means the prompt floats above every regular window and any
    // bar, and Enter/Esc land on us. exclusive_zone = -1 so we cover any
    // reserved strips (a bar) too.
    for i in 0..state.outputs.len() {
        let surface = state.compositor.create_surface(&qh, ());
        let layer_surface = state.layer_shell.get_layer_surface(
            &surface,
            Some(&state.outputs[i].output),
            Layer::Overlay,
            "shoestring-confirm".to_string(),
            &qh,
            LayerSurfaceData { idx: i },
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        surface.commit();
        state.outputs[i].surface = Some(surface);
        state.outputs[i].layer_surface = Some(layer_surface);
    }

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
        for i in 0..state.outputs.len() {
            if state.outputs[i].dirty && state.outputs[i].size.is_some() {
                if let Err(e) = redraw_output(state, qh, i) {
                    eprintln!("shoestring-confirm: redraw idx={i}: {e:#}");
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
                    eprintln!("shoestring-confirm: wayland read: {e:#}");
                }
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(state).context("dispatch")?;
    }
    // Make sure destroys reach the compositor before we exit so the
    // overlay doesn't linger past process exit.
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

// ---- Drawing -------------------------------------------------------------

fn redraw_output(state: &mut State, qh: &QueueHandle<State>, idx: usize) -> Result<()> {
    let (w, h) = match state.outputs[idx].size {
        Some(s) => s,
        None => return Ok(()),
    };
    if w == 0 || h == 0 {
        return Ok(());
    }

    let stride = w as i32 * 4;
    let size = (stride as usize) * h as usize;
    let tmp = tempfile::tempfile()?;
    tmp.set_len(size as u64)?;
    let mut mmap = unsafe { MmapMut::map_mut(&tmp)? };

    paint(&mut mmap, w, h, &state.prompt, &state.font);

    let pool: WlShmPool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
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
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    state.outputs[idx].dirty = false;
    buffer.destroy();
    Ok(())
}

fn paint(mmap: &mut MmapMut, w: u32, h: u32, prompt: &str, font: &Font) {
    let stride = w as usize * 4;
    // Dim background.
    let dim = [0u8, 0u8, 0u8, DIM_ALPHA];
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

    // Centered dialog box.
    let bw = BOX_W.min(w.saturating_sub(20));
    let bh = BOX_H.min(h.saturating_sub(20));
    let bx0 = ((w.saturating_sub(bw)) / 2) as i32;
    let by0 = ((h.saturating_sub(bh)) / 2) as i32;
    let bx1 = bx0 + bw as i32;
    let by1 = by0 + bh as i32;

    fill_rect(mmap, w, h, bx0, by0, bx1, by1, BOX_BG);
    stroke_rect(
        mmap,
        w,
        h,
        bx0,
        by0,
        bx1,
        by1,
        BOX_BORDER as i32,
        BOX_BORDER_COLOR,
    );

    // Prompt + hint, both centered horizontally inside the box.
    let prompt_metrics = measure_line(font, PROMPT_SIZE_PX, prompt);
    let hint_metrics = measure_line(font, HINT_SIZE_PX, HINT_LINE);

    let prompt_band = line_band(font, PROMPT_SIZE_PX);
    let hint_band = line_band(font, HINT_SIZE_PX);
    let gap = 12.0_f32;
    let total = prompt_band + gap + hint_band;
    let top = by0 as f32 + (bh as f32 - total) / 2.0;

    let prompt_baseline = (top + font_ascent(font, PROMPT_SIZE_PX)).round() as i32;
    let hint_baseline = (top + prompt_band + gap + font_ascent(font, HINT_SIZE_PX)).round() as i32;

    let prompt_x = bx0 + ((bw as i32 - prompt_metrics) / 2);
    let hint_x = bx0 + ((bw as i32 - hint_metrics) / 2);

    draw_text(
        mmap,
        w,
        h,
        font,
        PROMPT_SIZE_PX,
        prompt_x,
        prompt_baseline,
        prompt,
        TEXT_COLOR,
    );
    draw_text(
        mmap,
        w,
        h,
        font,
        HINT_SIZE_PX,
        hint_x,
        hint_baseline,
        HINT_LINE,
        HINT_COLOR,
    );
}

fn fill_rect(mmap: &mut MmapMut, w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    let stride = w as usize * 4;
    let x0 = x0.clamp(0, w as i32) as usize;
    let y0 = y0.clamp(0, h as i32) as usize;
    let x1 = x1.clamp(0, w as i32) as usize;
    let y1 = y1.clamp(0, h as i32) as usize;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let bytes = color.to_ne_bytes();
    let row: Vec<u8> = bytes.repeat(x1 - x0);
    for y in y0..y1 {
        let off = y * stride + x0 * 4;
        mmap[off..off + row.len()].copy_from_slice(&row);
    }
}

fn stroke_rect(
    mmap: &mut MmapMut,
    w: u32,
    h: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    thickness: i32,
    color: u32,
) {
    if thickness <= 0 {
        return;
    }
    // Top, bottom, left, right strips.
    fill_rect(mmap, w, h, x0, y0, x1, y0 + thickness, color);
    fill_rect(mmap, w, h, x0, y1 - thickness, x1, y1, color);
    fill_rect(mmap, w, h, x0, y0, x0 + thickness, y1, color);
    fill_rect(mmap, w, h, x1 - thickness, y0, x1, y1, color);
}

fn font_ascent(font: &Font, size_px: f32) -> f32 {
    font.horizontal_line_metrics(size_px)
        .map(|m| m.ascent)
        .unwrap_or(size_px * 0.8)
}

fn line_band(font: &Font, size_px: f32) -> f32 {
    let m = font
        .horizontal_line_metrics(size_px)
        .unwrap_or(fontdue::LineMetrics {
            ascent: size_px * 0.8,
            descent: -size_px * 0.2,
            line_gap: 0.0,
            new_line_size: size_px,
        });
    m.ascent - m.descent
}

fn measure_line(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut pen = 0.0_f32;
    for ch in text.chars() {
        let (metrics, _) = font.rasterize(ch, size_px);
        pen += metrics.advance_width;
    }
    pen.round() as i32
}

fn draw_text(
    mmap: &mut MmapMut,
    w: u32,
    h: u32,
    font: &Font,
    size_px: f32,
    x_start: i32,
    baseline_y: i32,
    text: &str,
    color: u32,
) {
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
            let bg_b = mmap[off];
            let bg_g = mmap[off + 1];
            let bg_r = mmap[off + 2];
            let bg_a = mmap[off + 3];
            let a = coverage as u16;
            let inv = 255 - a;
            // Premultiplied ARGB compositing: out = fg*a + bg*(1-a)
            mmap[off] = (((fb as u16 * a) + (bg_b as u16 * inv)) / 255) as u8;
            mmap[off + 1] = (((fg as u16 * a) + (bg_g as u16 * inv)) / 255) as u8;
            mmap[off + 2] = (((fr as u16 * a) + (bg_r as u16 * inv)) / 255) as u8;
            mmap[off + 3] = (((255_u16 * a) + (bg_a as u16 * inv)) / 255) as u8;
        }
    }
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

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
                // Compositor yanked us — treat as cancel.
                state.exit_code = EXIT_CANCELLED;
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
            if caps.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
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
        // Any pointer click cancels the dialog. The box is purely visual —
        // no clickable buttons. Matches "click outside to dismiss" UX.
        if let wl_pointer::Event::Button {
            state: WEnum::Value(ButtonState::Pressed),
            ..
        } = event
        {
            state.exit_code = EXIT_CANCELLED;
            state.finished = true;
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
                if sym == xkb::Keysym::Return || sym == xkb::Keysym::KP_Enter {
                    state.exit_code = EXIT_OK;
                    state.finished = true;
                } else if sym == xkb::Keysym::Escape {
                    state.exit_code = EXIT_CANCELLED;
                    state.finished = true;
                }
            }
            _ => {}
        }
    }
}
