//! shoestring-menu: dmenu-style launcher for shoestring-wm.
//!
//! Top-anchored layer-shell surface with an editable single-line input and
//! a fuzzy-filtered dropdown. Keyboard input via wl_seat + xkbcommon
//! (Exclusive interactivity routes every key to the menu while it's up).
//!
//! Mimics the user's existing dmenu wrappers (launch_ui_selection.sh and
//! launch_bookmark_selection.sh): candidates come from a curated file
//! rather than a $PATH scan, and bookmark dispatch extracts the URL from
//! a markdown `[title](url)` segment and opens it via xdg-open.
//!
//! Defaults (overridable with --source PATH):
//!   commands  $XDG_CONFIG_HOME/shoestring-wm/executables
//!   bookmarks $XDG_CONFIG_HOME/shoestring-wm/bookmarks

use std::{
    fs,
    io::Write,
    os::fd::{AsFd, AsRawFd},
    path::{Path, PathBuf},
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

/// Input strip height in logical pixels.
const INPUT_HEIGHT: u32 = 24;
/// Dropdown row height. One row per match.
const ROW_HEIGHT: u32 = 22;
/// Maximum dropdown rows. Matches dmenu's `-l 32`.
const MAX_ROWS: usize = 32;
/// Background fill, ARGB8888. Matches dmenu `-nb black`.
const BG: u32 = 0xFF_00_00_00;
/// Foreground (text) color. Matches dmenu `-nf '#66cccc'`.
const FG: u32 = 0xFF_66_CC_CC;
/// Selected-row highlight. Matches dmenu `-sb white`.
const SEL_BG: u32 = 0xFF_FF_FF_FF;
/// Selected-row text color. Matches dmenu `-sf black`.
const SEL_FG: u32 = 0xFF_00_00_00;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Commands,
    Bookmarks,
}

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
    /// Current mode (decides candidate source and how Enter dispatches).
    mode: Mode,
    /// All candidates, in file order, populated once at startup.
    candidates: Vec<Candidate>,
    /// Indices into `candidates` for the current query, best-first. Capped
    /// at MAX_ROWS — we never render more than that.
    matches: Vec<usize>,
    /// Selected row in `matches`. Clamped to matches.len()-1.
    selected: usize,
    /// Shared fuzzy matcher. Created once.
    matcher: fuzzy_matcher::skim::SkimMatcherV2,
    /// Set true on the first wl_keyboard::Enter we receive. Used by Leave
    /// to decide whether to exit: a Leave before we ever had focus would
    /// be spurious (we haven't even shown ourselves yet), but a Leave
    /// after Enter means another surface stole focus and we should quit.
    ever_focused: bool,
}

/// One entry in the candidate list.
/// - Commands mode: `display == invoke` (the file line, whitespace-split at exec).
/// - Bookmarks mode: `display` is the original markdown line; `invoke` is the URL
///   extracted from `](...)`. xdg-open opens the URL on selection.
#[derive(Clone, Debug)]
struct Candidate {
    display: String,
    invoke: String,
}

/// Initialise tracing. Writes to stderr by default; if `SHOESTRING_MENU_LOG`
/// is set, appends to that file instead (ANSI disabled). Mirrors the wm's
/// pattern — the menu is spawned by the wm so stdio is unreachable from a
/// TTY; the file route is the only way to debug what we received.
fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match std::env::var_os("SHOESTRING_MENU_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_MENU_LOG path");
            tracing_subscriber::fmt()
                .with_env_filter(env)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(env).init();
        }
    }
}

fn main() -> Result<()> {
    init_tracing();

    let cli = parse_args()?;
    let source = cli.source.unwrap_or_else(|| default_source(cli.mode));
    let candidates = match cli.mode {
        Mode::Commands => load_command_list(&source),
        Mode::Bookmarks => load_bookmarks_md(&source),
    };
    tracing::info!(
        mode = ?cli.mode,
        source = %source.display(),
        count = candidates.len(),
        "candidates loaded"
    );
    let mode = cli.mode;

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
        mode,
        candidates,
        matches: Vec::new(),
        selected: 0,
        matcher: fuzzy_matcher::skim::SkimMatcherV2::default(),
        ever_focused: false,
    };
    // Seed the visible matches with the empty-query result (top alphabetical).
    recompute_matches(&mut state);

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
    // Fixed total height: input strip plus the dropdown's full slot capacity.
    // Empty rows render as solid BG, so the surface always looks "right".
    let total_h = INPUT_HEIGHT + (MAX_ROWS as u32) * ROW_HEIGHT;
    layer_surface.set_size(0, total_h);
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

// ---- CLI + candidate sources --------------------------------------------

struct Cli {
    mode: Mode,
    /// Override the default source file. None → use default_source(mode).
    source: Option<PathBuf>,
}

/// Hand-rolled CLI: `--mode commands|bookmarks` (default commands) and
/// `--source <path>` to override the default candidate file. Two flags;
/// not worth pulling in clap.
fn parse_args() -> Result<Cli> {
    let mut mode = Mode::Commands;
    let mut source: Option<PathBuf> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!(
                    "shoestring-menu [--mode commands|bookmarks] [--source PATH]\n\
                     \n\
                     Defaults (alongside the wm config):\n\
                       commands  $XDG_CONFIG_HOME/shoestring-wm/executables\n\
                       bookmarks $XDG_CONFIG_HOME/shoestring-wm/bookmarks"
                );
                std::process::exit(0);
            }
            "--mode" => {
                let v = args.get(i + 1).context("--mode needs a value")?;
                mode = match v.as_str() {
                    "commands" => Mode::Commands,
                    "bookmarks" => Mode::Bookmarks,
                    other => anyhow::bail!("unknown mode {other:?} (expected commands|bookmarks)"),
                };
                i += 2;
            }
            "--source" => {
                let v = args.get(i + 1).context("--source needs a path")?;
                source = Some(PathBuf::from(v));
                i += 2;
            }
            other => anyhow::bail!("unknown arg {other:?}"),
        }
    }
    Ok(Cli { mode, source })
}

/// Default candidate file per mode. Lives alongside the wm's `config.toml`
/// under `$XDG_CONFIG_HOME/shoestring-wm/` (or `$HOME/.config/...`).
fn default_source(mode: Mode) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    let dir = base.join("shoestring-wm");
    match mode {
        Mode::Commands => dir.join("executables"),
        Mode::Bookmarks => dir.join("bookmarks"),
    }
}

/// Load the curated command list. One command per line; lines may contain
/// spaces (args are kept and whitespace-split at exec time). Blank lines
/// and `#` comments are skipped. Mirrors `launch_ui_selection.sh`.
fn load_command_list(path: &Path) -> Vec<Candidate> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = ?e, "command list unreadable");
            return Vec::new();
        }
    };
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| Candidate {
            display: l.to_string(),
            invoke: l.to_string(),
        })
        .collect()
}

/// Load a markdown bookmarks file. Each entry is expected to look like
///   `- [Title](URL) <!-- TAGS: ... -->`
/// We display the full line (so tags and URL are fuzzy-searchable) but
/// extract URL between `](` and the matching `)` for dispatch. Lines that
/// don't contain a parseable URL are skipped. Mirrors
/// `launch_bookmark_selection.sh`.
fn load_bookmarks_md(path: &Path) -> Vec<Candidate> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = ?e, "bookmarks file unreadable");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(url) = extract_md_url(trimmed) else {
            continue;
        };
        out.push(Candidate {
            display: trimmed.to_string(),
            invoke: url,
        });
    }
    out
}

/// Find the URL inside a markdown `[label](url)` segment. Tracks paren
/// depth so URLs containing `(...)` survive. Returns None if no parseable
/// link is present.
fn extract_md_url(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let tail = &line[start..];
    let mut depth = 1_i32;
    for (i, ch) in tail.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(tail[..i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Recompute `state.matches` from `state.query`. Empty query shows the top
/// alphabetical entries; non-empty query fuzzy-scores everything and keeps
/// the MAX_ROWS best.
fn recompute_matches(state: &mut State) {
    use fuzzy_matcher::FuzzyMatcher;

    state.matches.clear();
    if state.query.is_empty() {
        state
            .matches
            .extend(0..state.candidates.len().min(MAX_ROWS));
    } else {
        let mut scored: Vec<(i64, usize)> = state
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                state
                    .matcher
                    .fuzzy_match(&c.display, &state.query)
                    .map(|s| (s, i))
            })
            .collect();
        // Higher score first; break ties by alphabetical order (stable input).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        state
            .matches
            .extend(scored.into_iter().take(MAX_ROWS).map(|(_, i)| i));
    }
    if state.selected >= state.matches.len() {
        state.selected = state.matches.len().saturating_sub(1);
    }
}

/// Spawn a child fully detached from this process: setsid so SIGHUP from
/// our exit doesn't kill it, stdio nulled, no waiting (orphan reaps to PID 1).
fn spawn_detached(argv: &[&str]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (prog, rest) = argv.split_first().context("empty argv")?;
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach: new session means we don't get killed by the terminal/parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("spawn {prog}"))?;
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

    // --- Input strip (top INPUT_HEIGHT pixels) ---
    let line = format!("{}{}", PROMPT, state.query);
    draw_text_at(
        &mut mmap,
        w,
        h,
        0,
        INPUT_HEIGHT,
        &state.font,
        FONT_PX,
        PADDING_X,
        &line,
        FG,
    );
    let pre_caret = format!("{}{}", PROMPT, &state.query[..state.cursor_byte]);
    let caret_x = PADDING_X + measure_text(&state.font, FONT_PX, &pre_caret);
    draw_caret(&mut mmap, w, INPUT_HEIGHT, caret_x, FG);

    // --- Dropdown rows ---
    for (row_idx, cand_idx) in state.matches.iter().copied().enumerate() {
        let row_top = INPUT_HEIGHT as i32 + (row_idx as i32) * ROW_HEIGHT as i32;
        let selected = row_idx == state.selected;
        if selected {
            fill_rect(
                &mut mmap,
                w,
                h,
                0,
                row_top,
                w as i32,
                ROW_HEIGHT as i32,
                SEL_BG,
            );
        }
        let label = &state.candidates[cand_idx].display;
        draw_text_at(
            &mut mmap,
            w,
            h,
            row_top,
            ROW_HEIGHT,
            &state.font,
            FONT_PX,
            PADDING_X,
            label,
            if selected { SEL_FG } else { FG },
        );
    }

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

/// Draw one line of text vertically centered inside [band_top, band_top+band_h]
/// on a surface of size (w, surface_h). Glyphs are clipped to the full surface
/// — band_h only affects baseline placement, not clipping, so glyphs near the
/// font's ascent/descent extremes don't lose pixels at row boundaries.
#[allow(clippy::too_many_arguments)]
fn draw_text_at(
    mmap: &mut MmapMut,
    w: u32,
    surface_h: u32,
    band_top: i32,
    band_h: u32,
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
    let baseline_y = band_top + ((band_h as f32 - band) / 2.0 + line_metrics.ascent).round() as i32;

    let mut pen_x = x_start as f32;
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size_px);
        let gx = (pen_x + metrics.xmin as f32).round() as i32;
        let gy = baseline_y - (metrics.ymin + metrics.height as i32);
        blit_alpha(
            mmap,
            w,
            surface_h,
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

/// Solid-color rect blit. Used for the selected-row highlight.
#[allow(clippy::too_many_arguments)]
fn fill_rect(
    mmap: &mut MmapMut,
    w: u32,
    h: u32,
    x: i32,
    y: i32,
    rect_w: i32,
    rect_h: i32,
    color: u32,
) {
    let bytes = color.to_ne_bytes();
    let stride = w as usize * 4;
    let x0 = x.max(0) as usize;
    let x1 = (x + rect_w).clamp(0, w as i32) as usize;
    let y0 = y.max(0) as usize;
    let y1 = (y + rect_h).clamp(0, h as i32) as usize;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pixel = bytes;
    for row in y0..y1 {
        let row_off = row * stride;
        for col in x0..x1 {
            let off = row_off + col * 4;
            mmap[off..off + 4].copy_from_slice(&pixel);
        }
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
fn handle_key(state: &mut State, sym: xkb::Keysym, utf8: &str, ctrl: bool, shift: bool) -> bool {
    use xkb::keysyms::*;
    let raw = sym.raw();
    tracing::debug!(
        keysym = raw,
        keysym_name = %xkb::keysym_get_name(sym),
        utf8 = ?utf8,
        ctrl,
        shift,
        "menu key"
    );

    // -- Exit / dispatch --
    if raw == KEY_Escape {
        tracing::debug!("Escape: setting running=false");
        state.running = false;
        return false;
    }
    if raw == KEY_Return || raw == KEY_KP_Enter {
        dispatch_selection(state);
        state.running = false;
        return false;
    }

    // -- Selection navigation --
    if raw == KEY_Down || (raw == KEY_Tab && !shift) {
        if !state.matches.is_empty() {
            state.selected = (state.selected + 1) % state.matches.len();
        }
        return true;
    }
    if raw == KEY_Up || (raw == KEY_ISO_Left_Tab) || (raw == KEY_Tab && shift) {
        if !state.matches.is_empty() {
            state.selected = if state.selected == 0 {
                state.matches.len() - 1
            } else {
                state.selected - 1
            };
        }
        return true;
    }

    // -- Caret motion --
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

    // -- Query edits (must recompute_matches) --
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
        state.selected = 0;
        recompute_matches(state);
        return true;
    }
    if ctrl && (raw == KEY_u || raw == KEY_U) {
        state.query.clear();
        state.cursor_byte = 0;
        state.selected = 0;
        recompute_matches(state);
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
    state.selected = 0;
    recompute_matches(state);
    true
}

/// Spawn the currently-selected match, or the literal query if no match is
/// highlighted. M2 only handles Mode::Commands; M3 wires Mode::Bookmarks.
fn dispatch_selection(state: &State) {
    let cmd: String = if let Some(idx) = state.matches.get(state.selected).copied() {
        state.candidates[idx].invoke.clone()
    } else if !state.query.is_empty() {
        state.query.clone()
    } else {
        return;
    };
    match state.mode {
        Mode::Commands => {
            // Whitespace-split so curated entries like `code --some-flag`
            // exec with args. No shell quoting — users who need that can
            // wrap their entry in a script.
            let tokens: Vec<&str> = cmd.split_whitespace().collect();
            if tokens.is_empty() {
                return;
            }
            if let Err(e) = spawn_detached(&tokens) {
                tracing::error!(error = ?e, "spawn failed");
            }
        }
        Mode::Bookmarks => {
            // `cmd` is the URL we extracted at load time. xdg-open routes to
            // the user's configured default browser.
            if let Err(e) = spawn_detached(&["xdg-open", &cmd]) {
                tracing::error!(error = ?e, "xdg-open failed");
            }
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
                let default_h = INPUT_HEIGHT + (MAX_ROWS as u32) * ROW_HEIGHT;
                let h = if height == 0 { default_h } else { height };
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
                let shift =
                    xkb_state.mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);
                if handle_key(state, sym, &utf8, ctrl, shift) {
                    state.dirty = true;
                }
            }
            wl_keyboard::Event::Enter { .. } => {
                state.ever_focused = true;
                tracing::debug!("keyboard focus entered");
            }
            // The only way we lose focus while alive is another surface
            // stealing it (e.g. user pressed Super+P while a menu is
            // already up, spawning a second instance). Exit so we don't
            // become an invisible background process.
            wl_keyboard::Event::Leave { .. } if state.ever_focused => {
                tracing::debug!("keyboard focus left; exiting");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_url_basic() {
        assert_eq!(
            extract_md_url("- [Foo](https://example.com)").as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn extract_url_with_tag_comment() {
        let line = "- [Foo](https://example.com) <!-- TAGS: a,b -->";
        assert_eq!(extract_md_url(line).as_deref(), Some("https://example.com"));
    }

    #[test]
    fn extract_url_with_inner_parens() {
        // Wikipedia-style URLs with disambiguation parens.
        let line = "- [Bar](https://en.wikipedia.org/wiki/Foo_(bar)) <!-- T -->";
        assert_eq!(
            extract_md_url(line).as_deref(),
            Some("https://en.wikipedia.org/wiki/Foo_(bar)")
        );
    }

    #[test]
    fn extract_url_missing_returns_none() {
        assert_eq!(extract_md_url("- plain text, no link").as_deref(), None);
        assert_eq!(
            extract_md_url("- [unterminated](https://x").as_deref(),
            None
        );
    }
}
