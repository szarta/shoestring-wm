//! shoestring-lock — screen locker for shoestring-wm.
//!
//! Drives `ext-session-lock-v1`: locks the session, creates one black
//! surface per output, reads a password through `wl_keyboard`, verifies
//! it via PAM, and unlocks on success.
//!
//! The MVP grabs the seat keyboard the moment the compositor focuses
//! one of our lock surfaces. A maze screensaver runs on every output;
//! the password prompt stays hidden until the user starts typing and
//! then shows three lines centered on each output:
//!
//! ```text
//!     <hostname> · locked
//!     Password: ********
//!     (status / error)
//! ```
//!
//! The prompt auto-hides after [`PROMPT_TIMEOUT`] of inactivity, or
//! immediately on Escape, discarding any half-typed password. No
//! multi-output text positioning, no fade — just enough to keep an
//! unattended workstation usable on a single-user desktop. Spawned by
//! shoestring-wm via the `Action::Lock` / `shoestring-ctl lock` paths.

mod canvas;
mod maze;
mod pam;

use std::{
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use canvas::{Canvas, Rect, Target};
use clap::Parser;
use fontdue::Font;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard},
        wl_output::WlOutput,
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};
use xkbcommon::xkb;

// Color palette taken from xscreensaver's maze.c defaults (lines
// 1361-1376): black background, ~white walls, #00FF00 live path,
// #880000 dead/backtracked path. Argb8888 with opaque alpha so the
// compositor doesn't blend the lock surface with anything below.
const COLOR_BG: u32 = 0xFF00_0000;
const COLOR_WALL: u32 = 0xFFE0_E0E0;
const COLOR_LIVE: u32 = 0xFF00_FF00;
const COLOR_DEAD: u32 = 0xFF88_0000;
/// Solid backdrop drawn behind the prompt block so the text stays
/// legible over the busy maze. Dark gray, opaque.
const COLOR_PROMPT_BG: u32 = 0xFF20_2020;

/// Animation tick interval. ~30Hz keeps CPU light while staying
/// visually smooth for a screensaver.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Solver steps to advance per frame. Yields ~90 steps/sec — close
/// to xscreensaver's default --solve-delay=10000 µs (≈100/sec).
const STEPS_PER_FRAME: u32 = 3;
/// Pause showing the freshly-generated maze before the solver starts.
const PRE_SOLVE_DELAY: Duration = Duration::from_millis(800);
/// Pause showing the fully-solved maze before regenerating.
const POST_SOLVE_DELAY: Duration = Duration::from_millis(1800);
/// How long the password prompt stays up after the last keystroke
/// before fading back to the bare screensaver. Pressing Escape hides
/// it immediately; either way the half-typed password is discarded.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Target pixels per maze cell. Adjusted to whatever divides the
/// output cleanly enough to fit at least [`MIN_CELLS`] in each axis.
const TARGET_CELL_PX: u32 = 24;
const MIN_CELL_PX: u32 = 12;
const MIN_CELLS: u16 = 12;

/// Per-output animation state: a freshly-generated maze, a solver
/// chewing through it, and a small phase machine so we linger briefly
/// on each new maze before/after solving (matches xscreensaver's
/// `--pre-delay` / `--post-delay` knobs).
struct Animation {
    /// Logical-pixel size of one cell. Picked at construction to
    /// divide the output evenly enough to fit [`MIN_CELLS`] in each
    /// axis while staying close to [`TARGET_CELL_PX`].
    cell_px: u32,
    /// Top-left offset of the maze inside the output, in logical
    /// pixels. Centers the maze when `cell_px * dims` doesn't tile
    /// the surface exactly.
    offset: (i32, i32),
    maze: maze::Maze,
    solver: maze::Solver,
    phase: Phase,
    rng: maze::Rng,
}

/// What an animation tick changed.
enum Change {
    None,
    /// These cells changed state.
    Cells(Vec<(u16, u16)>),
    /// A new maze: repaint everything.
    All,
}

#[derive(Debug)]
enum Phase {
    /// Showing the freshly-generated maze, untouched, before the
    /// solver starts.
    PreSolve { until: Instant },
    /// Solver running. Steps advance per frame in the main loop.
    Solving,
    /// Solver finished. Maze stays on screen until `until`, then we
    /// regenerate.
    PostSolve { until: Instant },
}

impl Animation {
    fn new(surface_w: u32, surface_h: u32) -> Self {
        let cell_px = pick_cell_size(surface_w, surface_h);
        let cols = (surface_w / cell_px).max(MIN_CELLS as u32) as u16;
        let rows = (surface_h / cell_px).max(MIN_CELLS as u32) as u16;
        let offset = (
            ((surface_w as i32) - (cols as i32 * cell_px as i32)) / 2,
            ((surface_h as i32) - (rows as i32 * cell_px as i32)) / 2,
        );
        let mut rng = maze::Rng::from_clock();
        let maze = maze::Maze::generate_dfs(cols, rows, &mut rng);
        let solver = maze::Solver::new(&maze, (0, 0), (cols - 1, rows - 1), true);
        Self {
            cell_px,
            offset,
            maze,
            solver,
            phase: Phase::PreSolve {
                until: Instant::now() + PRE_SOLVE_DELAY,
            },
            rng,
        }
    }

    /// Tear down + rebuild the maze. Reuses the same cell size /
    /// offset (output didn't resize).
    fn regenerate(&mut self) {
        let cols = self.maze.width();
        let rows = self.maze.height();
        self.maze = maze::Maze::generate_dfs(cols, rows, &mut self.rng);
        self.solver = maze::Solver::new(&self.maze, (0, 0), (cols - 1, rows - 1), true);
        self.phase = Phase::PreSolve {
            until: Instant::now() + PRE_SOLVE_DELAY,
        };
    }

    /// Advance the animation and say what needs repainting.
    fn tick(&mut self) -> Change {
        let now = Instant::now();
        match self.phase {
            Phase::PreSolve { until } => {
                if now >= until {
                    self.phase = Phase::Solving;
                }
                Change::None
            }
            Phase::Solving => {
                let mut cells = Vec::new();
                for _ in 0..STEPS_PER_FRAME {
                    match self.solver.step(&self.maze) {
                        Some(maze::DrawEvent::EnteredLive(x, y))
                        | Some(maze::DrawEvent::MarkedDead(x, y)) => cells.push((x, y)),
                        Some(maze::DrawEvent::Solved) | None => {}
                    }
                    if self.solver.finished() {
                        self.phase = Phase::PostSolve {
                            until: now + POST_SOLVE_DELAY,
                        };
                        break;
                    }
                }
                if cells.is_empty() {
                    Change::None
                } else {
                    Change::Cells(cells)
                }
            }
            Phase::PostSolve { until } => {
                if now >= until {
                    self.regenerate();
                    return Change::All;
                }
                Change::None
            }
        }
    }
}

/// Pick a cell size that divides the surface into at least
/// [`MIN_CELLS`] in each axis, preferring [`TARGET_CELL_PX`].
fn pick_cell_size(surface_w: u32, surface_h: u32) -> u32 {
    let max_by_w = (surface_w / MIN_CELLS as u32).max(MIN_CELL_PX);
    let max_by_h = (surface_h / MIN_CELLS as u32).max(MIN_CELL_PX);
    TARGET_CELL_PX.min(max_by_w).min(max_by_h).max(MIN_CELL_PX)
}

#[derive(Debug, Parser)]
#[command(name = "shoestring-lock", version, about)]
struct Cli {
    /// PAM service name. Defaults to `shoestring-lock` — a dedicated policy
    /// shipped with the locker (see packaging). Override for a custom stack.
    #[arg(long)]
    pam_service: Option<String>,

    /// Optional path to a TTF font for the prompt. Falls back to
    /// $SHOESTRING_LOCK_FONT, then the common system font paths, then any
    /// font found under the system and user font directories.
    #[arg(long)]
    font: Option<PathBuf>,

    /// Pixel size for the prompt text.
    #[arg(long, default_value_t = 28.0)]
    font_size: f32,
}

struct OutputEntry {
    output: WlOutput,
    /// Stable registry name; kept for future logging.
    #[allow(dead_code)]
    name: u32,
    surface: Option<WlSurface>,
    /// Held to keep the lock surface alive — destroying it would
    /// unmap the only thing standing between the user and the
    /// unlocked desktop.
    #[allow(dead_code)]
    lock_surface: Option<ExtSessionLockSurfaceV1>,
    /// Last configure size from the compositor (logical pixels).
    size: Option<(u32, u32)>,
    /// Fractional scale (1.0 until the compositor reports a preferred scale).
    /// The buffer + maze render at `logical * scale` px, mapped back to logical
    /// via `viewport`.
    scale: f64,
    viewport: Option<WpViewport>,
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,
    /// True once we've drawn a buffer matching the latest configure.
    #[allow(dead_code)]
    drawn: bool,
    /// Per-output screensaver state. Built lazily on first configure (need the
    /// size), rebuilt on resize or scale change — it is sized in physical px.
    anim: Option<Animation>,
    /// The output's pixels and the buffers that show them; built on first draw.
    canvas: Option<Canvas>,
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    /// HiDPI globals — `Some` when the compositor advertises both.
    viewporter: Option<WpViewporter>,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    /// Held so the manager stays bound for the lifetime of the locker.
    #[allow(dead_code)]
    lock_manager: ExtSessionLockManagerV1,
    lock: Option<ExtSessionLockV1>,
    /// `true` after the compositor sent `locked` on the lock object.
    locked: bool,
    /// `true` after we've sent `unlock_and_destroy`; the main loop will
    /// exit on the next iteration so the destroy actually flushes.
    finished: bool,

    outputs: Vec<OutputEntry>,

    /// Kept alive so the seat proxy stays bound; we read events through
    /// the registered Dispatch impl, not by polling this field.
    #[allow(dead_code)]
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
    xkb_ctx: xkb::Context,
    xkb_keymap: Option<xkb::Keymap>,
    xkb_state: Option<xkb::State>,

    /// `None` when no font loaded: the lock still holds, without the prompt.
    font: Option<Font>,
    font_size: f32,
    /// What the user has typed so far. Held in memory until verify;
    /// zeroed on each verify attempt.
    password: String,
    /// Single-line message under the prompt (errors, "verifying…").
    status: String,
    /// Whether the password prompt is currently drawn. Hidden until the
    /// user starts typing; hidden again on Escape or after
    /// [`PROMPT_TIMEOUT`] of inactivity. The maze pauses while it shows.
    prompt_visible: bool,
    /// When the prompt should auto-hide. Only meaningful while
    /// `prompt_visible`; bumped forward on every keystroke.
    prompt_deadline: Instant,
    hostname: String,
    pam_service: String,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SHOESTRING_LOCK_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shoestring-lock: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // Lock even without a font: a lock screen with no prompt text beats
    // leaving the session open. Typing and unlocking work the same.
    let font =
        match shoestring_font::load(cli.font.as_deref(), "--font", Some("SHOESTRING_LOCK_FONT")) {
            Ok(font) => Some(font),
            Err(e) => {
                tracing::warn!(error = %e, "no usable font; locking without the prompt text");
                None
            }
        };
    let pam_service = cli
        .pam_service
        .unwrap_or_else(|| DEFAULT_PAM_SERVICE.to_string());
    warn_if_pam_service_missing(&pam_service);
    let hostname = read_hostname();

    let conn = Connection::connect_to_env().context("connect WAYLAND_DISPLAY")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry init")?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .context("wl_compositor missing")?;
    let shm: WlShm = globals.bind(&qh, 1..=2, ()).context("wl_shm missing")?;
    let lock_manager: ExtSessionLockManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("ext_session_lock_manager_v1 missing — compositor doesn't support locking")?;
    let seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();

    // Outputs: bind whatever the registry currently knows about. New
    // outputs after lock are out of scope for the MVP.
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
                    name: g.name,
                    surface: None,
                    lock_surface: None,
                    size: None,
                    scale: 1.0,
                    viewport: None,
                    fractional: None,
                    drawn: false,
                    anim: None,
                    canvas: None,
                }
            })
            .collect()
    });

    // Take the lock; compositor will respond with `locked` or `finished`.
    let lock = lock_manager.lock(&qh, ());

    let mut state = State {
        compositor,
        shm,
        viewporter,
        fractional_mgr,
        lock_manager,
        lock: Some(lock),
        locked: false,
        finished: false,
        outputs,
        seat,
        keyboard: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_keymap: None,
        xkb_state: None,
        font,
        font_size: cli.font_size,
        password: String::new(),
        status: String::new(),
        prompt_visible: false,
        prompt_deadline: Instant::now(),
        hostname,
        pam_service,
    };

    // Create one lock surface per output up front. The compositor will
    // send a configure for each which we ack and draw into.
    for i in 0..state.outputs.len() {
        create_lock_surface(&mut state, &qh, i);
    }

    // Frame-paced event loop. Wayland events are dispatched whenever
    // the fd is readable; between events we sleep up to
    // FRAME_INTERVAL, then tick each output's animation. The same
    // poll(2) pattern shoestring-bar uses — no calloop dep needed.
    let mut last_frame = Instant::now();
    while !state.finished {
        conn.flush().context("flush wayland")?;

        let timeout_ms = {
            let elapsed = last_frame.elapsed();
            if elapsed >= FRAME_INTERVAL {
                0
            } else {
                (FRAME_INTERVAL - elapsed).as_millis() as i32
            }
        };

        if let Some(guard) = conn.prepare_read() {
            let fd = guard.connection_fd().as_raw_fd();
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if n > 0 && pfd.revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    // WouldBlock can fire if wayland-rs already
                    // drained the fd inside prepare_read.
                    if !matches!(
                        &e,
                        wayland_client::backend::WaylandError::Io(io)
                            if io.kind() == std::io::ErrorKind::WouldBlock
                    ) {
                        tracing::warn!(error = ?e, "wayland read");
                    }
                }
            } else {
                drop(guard);
            }
        }
        queue
            .dispatch_pending(&mut state)
            .context("wayland dispatch")?;

        if last_frame.elapsed() >= FRAME_INTERVAL {
            last_frame = Instant::now();
            tick_animations(&mut state, &qh);
            // Auto-hide the prompt once it has been idle past its
            // deadline, falling back to the bare screensaver.
            if state.prompt_visible && Instant::now() >= state.prompt_deadline {
                state.hide_prompt();
                redraw_prompt(&mut state, &qh);
            }
        }
    }
    // Final roundtrip so the unlock_and_destroy actually reaches the
    // compositor before we exit and the connection is torn down.
    let _ = conn.roundtrip();
    Ok(())
}

/// Advance every output's screensaver animation and redraw the ones
/// that report dirty. Called once per frame.
fn tick_animations(state: &mut State, qh: &QueueHandle<State>) {
    present_leftovers(state, qh);
    // The maze holds still while the prompt is up, so typing repaints only
    // the prompt (and a remote viewer could watch it cheaply).
    if state.prompt_visible {
        return;
    }
    for i in 0..state.outputs.len() {
        if state.outputs[i].size.is_none() {
            continue;
        }
        let regions = match state.outputs[i].anim.as_mut().map(|a| a.tick()) {
            None | Some(Change::None) => continue,
            Some(Change::All) => None,
            Some(Change::Cells(cells)) => {
                let anim = state.outputs[i].anim.as_ref().expect("ticked above");
                Some(
                    cells
                        .iter()
                        .map(|&(x, y)| cell_region(anim, x, y))
                        .collect(),
                )
            }
        };
        if let Err(e) = redraw(state, qh, i, regions) {
            tracing::warn!(idx = i, error = %e, "redraw failed");
        }
    }
}

fn create_lock_surface(state: &mut State, qh: &QueueHandle<State>, idx: usize) {
    let Some(lock) = state.lock.as_ref() else {
        return;
    };
    let surface = state.compositor.create_surface(qh, ());
    // Opt into fractional scaling before the first configure so the maze and
    // prompt render crisp on HiDPI outputs.
    if let (Some(vp), Some(mgr)) = (&state.viewporter, &state.fractional_mgr) {
        state.outputs[idx].viewport = Some(vp.get_viewport(&surface, qh, ()));
        state.outputs[idx].fractional =
            Some(mgr.get_fractional_scale(&surface, qh, OutputSurfaceData { idx }));
    }
    let entry = &mut state.outputs[idx];
    let lock_surface =
        lock.get_lock_surface(&surface, &entry.output, qh, OutputSurfaceData { idx });
    entry.surface = Some(surface);
    entry.lock_surface = Some(lock_surface);
}

// ---- Wayland event handlers ---------------------------------------------

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

impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as wayland_client::Proxy>::Event,
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
        _: <WlSurface as wayland_client::Proxy>::Event,
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
        _: <WlShm as wayland_client::Proxy>::Event,
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
        _: <WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// wp_viewporter / wp_viewport / wp_fractional_scale_manager_v1 are request-only.
impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as wayland_client::Proxy>::Event,
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
        _: <WpViewport as wayland_client::Proxy>::Event,
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
        _: <WpFractionalScaleManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpFractionalScaleV1, OutputSurfaceData> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as wayland_client::Proxy>::Event,
        data: &OutputSurfaceData,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // `scale` is the ratio in 1/120ths (e.g. 180 == 1.5x).
            let s = scale as f64 / 120.0;
            let Some(entry) = state.outputs.get_mut(data.idx) else {
                return;
            };
            if entry.scale == s {
                return;
            }
            entry.scale = s;
            // The maze is sized in physical px, so a scale change is a resize:
            // rebuild it at the new resolution, then repaint.
            if let Some((w, h)) = entry.size {
                let pw = ((w as f64) * s).round().max(1.0) as u32;
                let ph = ((h as f64) * s).round().max(1.0) as u32;
                entry.anim = Some(Animation::new(pw, ph));
            }
            if let Err(e) = redraw_output(state, qh, data.idx) {
                tracing::warn!(error = %e, "redraw after scale change failed");
            }
        }
    }
}

// The user data is the buffer's busy flag (see [`canvas`]): the compositor
// releasing it makes it reusable.
impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtSessionLockManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtSessionLockManagerV1,
        _: <ExtSessionLockManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: <ExtSessionLockV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => {
                tracing::info!("compositor confirmed lock");
                state.locked = true;
            }
            ext_session_lock_v1::Event::Finished => {
                // Compositor refused the lock or yanked it (e.g. another
                // locker beat us / privileges). Quit without trying to
                // unlock — calling unlock_and_destroy now would be a
                // protocol error.
                tracing::warn!("session lock finished by compositor — exiting");
                state.lock = None;
                state.finished = true;
            }
            _ => {}
        }
    }
}

/// Per-lock-surface user data so we can map configure events back to
/// the originating output entry.
struct OutputSurfaceData {
    idx: usize,
}

impl Dispatch<ExtSessionLockSurfaceV1, OutputSurfaceData> for State {
    fn event(
        state: &mut Self,
        lock_surface: &ExtSessionLockSurfaceV1,
        event: <ExtSessionLockSurfaceV1 as wayland_client::Proxy>::Event,
        data: &OutputSurfaceData,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            lock_surface.ack_configure(serial);
            if data.idx >= state.outputs.len() {
                return;
            }
            let entry = &mut state.outputs[data.idx];
            let prev = entry.size;
            entry.size = Some((width, height));
            entry.drawn = false;
            // (Re)build the animation on first configure or genuine
            // resize. Same-size reconfigures preserve the running
            // solver so the maze doesn't restart on each focus
            // change / mode flap. The maze is sized in physical px so it
            // fills the (possibly HiDPI) buffer crisply.
            let need_rebuild = entry.anim.is_none() || prev != Some((width, height));
            if need_rebuild {
                let pw = ((width as f64) * entry.scale).round().max(1.0) as u32;
                let ph = ((height as f64) * entry.scale).round().max(1.0) as u32;
                entry.anim = Some(Animation::new(pw, ph));
            }
            if let Err(e) = redraw_output(state, qh, data.idx) {
                tracing::warn!(error = %e, "redraw failed");
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
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
            if caps.contains(Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl State {
    /// Register keyboard activity: reveal the prompt and push its
    /// auto-hide deadline out by [`PROMPT_TIMEOUT`].
    fn wake_prompt(&mut self) {
        self.prompt_visible = true;
        self.prompt_deadline = Instant::now() + PROMPT_TIMEOUT;
    }

    /// Hide the prompt and discard any half-typed password and status.
    fn hide_prompt(&mut self) {
        self.prompt_visible = false;
        self.password.clear();
        self.status.clear();
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(format, WEnum::Value(KeymapFormat::XkbV1)) {
                    tracing::warn!("non-xkbv1 keymap format; ignoring");
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
                match keymap {
                    Ok(Some(km)) => {
                        state.xkb_state = Some(xkb::State::new(&km));
                        state.xkb_keymap = Some(km);
                    }
                    Ok(None) => tracing::warn!("keymap parse returned None"),
                    Err(e) => tracing::warn!(error = %e, "keymap parse failed"),
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xs) = state.xkb_state.as_mut() {
                    xs.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                if !matches!(ks, WEnum::Value(KeyState::Pressed)) {
                    return;
                }
                // While the prompt is hidden, the first key is a pure wake
                // trigger: reveal the dialog but don't ingest the key. This
                // matches the usual "tap any key to wake" reflex — Enter
                // won't submit an empty password ("bad password"), printable
                // keys won't silently seed the buffer, and otherwise-inert
                // keys (arrows, Esc) still bring the prompt up.
                if !state.prompt_visible {
                    state.wake_prompt();
                    redraw_prompt(state, qh);
                    return;
                }
                // wayland keycode is evdev (offset 8 from xkb keycode).
                let xkb_code = key + 8;
                let mut dirty = false;
                let mut submit = false;
                // Pull the keysym + text out first so the xkb_state borrow
                // ends before we touch `state` through &mut self helpers.
                let parsed = state.xkb_state.as_ref().map(|xs| {
                    (
                        xs.key_get_one_sym(xkb_code.into()),
                        xs.key_get_utf8(xkb_code.into()),
                    )
                });
                if let Some((sym, utf8)) = parsed {
                    match sym {
                        xkb::Keysym::Return | xkb::Keysym::KP_Enter => submit = true,
                        xkb::Keysym::BackSpace => {
                            if state.password.pop().is_some() {
                                state.wake_prompt();
                                dirty = true;
                            }
                        }
                        xkb::Keysym::Escape => {
                            // Dismiss the prompt immediately, dropping any
                            // half-typed password. Always redraw to clear
                            // the box.
                            state.hide_prompt();
                            dirty = true;
                        }
                        _ => {
                            // Skip control chars (BS/Tab/etc already
                            // handled above) and modifier-only events.
                            if !utf8.is_empty() && !utf8.chars().all(|c| c.is_control()) {
                                state.password.push_str(&utf8);
                                state.wake_prompt();
                                dirty = true;
                            }
                        }
                    }
                }
                if submit {
                    verify_password(state);
                    // Keep the prompt up (with a fresh deadline) so a
                    // failure message stays visible; on success the loop
                    // is already tearing down.
                    if !state.finished {
                        state.wake_prompt();
                    }
                    dirty = true;
                }
                if dirty {
                    redraw_prompt(state, qh);
                }
            }
            _ => {}
        }
    }
}

// ---- PAM ----------------------------------------------------------------

/// Verify the typed password for `$USER` through the PAM service. The
/// conversation (see [`pam`]) answers echo-off prompts with the password and
/// refuses echo-on ones (username, 2FA), which a desktop unlock stack almost
/// never asks.
fn verify_password(state: &mut State) {
    let user = match std::env::var("USER") {
        Ok(u) => u,
        Err(_) => {
            state.status = "no $USER set".into();
            return;
        }
    };
    let password = pam::Secret::new(std::mem::take(&mut state.password));
    state.status = "verifying…".into();
    tracing::debug!(user, "attempting PAM auth");

    match pam::verify(&state.pam_service, &user, password) {
        Ok(()) => {
            tracing::info!("auth ok, unlocking");
            if let Some(lock) = state.lock.take() {
                lock.unlock_and_destroy();
            }
            state.finished = true;
        }
        Err(e @ (pam::Error::NulInPassword | pam::Error::NulInName)) => {
            state.status = e.to_string();
        }
        Err(e @ pam::Error::Start(_)) => {
            state.status = format!("pam init failed: {e}");
            tracing::warn!(service = %state.pam_service, error = %e, "pam init failed");
        }
        Err(e @ pam::Error::Auth(_)) => {
            tracing::info!(error = %e, "auth failed");
            state.status = "authentication failed".into();
        }
        Err(e @ pam::Error::Account(_)) => {
            tracing::info!(error = %e, "acct_mgmt rejected user");
            state.status = "account disabled".into();
        }
    }
}

/// Default PAM service. We ship a dedicated policy at
/// `/etc/pam.d/shoestring-lock` (FreeBSD: `/usr/local/etc/pam.d/shoestring-lock`)
/// so the locker verifies the *current* user's password through a stack we
/// control. This is deliberately NOT the generic `login` service: on
/// FreeBSD/OpenPAM `login` begins with `auth sufficient pam_self.so`, which
/// succeeds for the calling user with no password — a locker using it would
/// unlock on any input. The shipped policy uses the platform's real password
/// check (Linux: `pam_unix` via the setuid `unix_chkpwd`; FreeBSD: the setuid
/// `unix-selfauth-helper`), so the locker binary itself stays unprivileged.
const DEFAULT_PAM_SERVICE: &str = "shoestring-lock";

/// Warn if no policy file backs `service`. Without one, PAM resolves the
/// `other` fallback (deny, on a sane system) and every unlock attempt fails
/// closed — which presents as "the locker rejects my correct password".
/// Packaging installs the file; source installs must copy it (see docs).
fn warn_if_pam_service_missing(service: &str) {
    let candidates = [
        format!("/etc/pam.d/{service}"),
        format!("/usr/local/etc/pam.d/{service}"),
    ];
    if !candidates.iter().any(|p| Path::new(p).exists()) {
        tracing::warn!(
            service,
            "no PAM policy at /etc/pam.d/ or /usr/local/etc/pam.d/; unlock will \
             fail closed — install the shipped shoestring-lock service file"
        );
    }
}

// ---- Drawing ------------------------------------------------------------

/// Repaint all of output `idx`.
fn redraw_output(state: &mut State, qh: &QueueHandle<State>, idx: usize) -> Result<()> {
    redraw(state, qh, idx, None)
}

/// Repaint the prompt on every output: where it was (if it moved, shrank, or
/// was hidden) and where it is now.
fn redraw_prompt(state: &mut State, qh: &QueueHandle<State>) {
    for i in 0..state.outputs.len() {
        if state.outputs[i].size.is_none() {
            continue;
        }
        let Some(old) = state.outputs[i].canvas.as_ref().map(|c| c.prompt_box) else {
            // Nothing drawn yet: paint everything.
            if let Err(e) = redraw_output(state, qh, i) {
                tracing::warn!(idx = i, error = %e, "redraw failed");
            }
            continue;
        };
        let new = prompt_layout(state, i).map(|l| l.bbox);
        let regions: Vec<Rect> = old.into_iter().chain(new).collect();
        if let Err(e) = redraw(state, qh, i, Some(regions)) {
            tracing::warn!(idx = i, error = %e, "redraw failed");
        }
    }
}

/// Repaint `regions` of output `idx` (all of it when `None`) and show them.
/// The canvas keeps the rest of the screen from earlier frames.
fn redraw(
    state: &mut State,
    qh: &QueueHandle<State>,
    idx: usize,
    regions: Option<Vec<Rect>>,
) -> Result<()> {
    // `(w, h)` is the logical surface size; the buffer + maze render at physical
    // `(pw, ph)` and the viewport maps it back. The maze `anim` is already sized
    // in physical px (see configure / preferred_scale).
    let Some((w, h)) = state.outputs[idx].size else {
        return Ok(());
    };
    let Some(surface) = state.outputs[idx].surface.as_ref().cloned() else {
        return Ok(());
    };
    if w == 0 || h == 0 {
        return Ok(());
    }
    let scale = state.outputs[idx].scale;
    let pw = ((w as f64) * scale).round().max(1.0) as u32;
    let ph = ((h as f64) * scale).round().max(1.0) as u32;

    // A new or resized surface gets a fresh canvas, painted in full.
    let resized = state.outputs[idx]
        .canvas
        .as_ref()
        .is_none_or(|c| (c.w, c.h) != (pw, ph));
    if resized {
        state.outputs[idx].canvas = Some(Canvas::new(pw, ph));
    }
    let prompt = prompt_layout(state, idx);
    let entry = &mut state.outputs[idx];
    let canvas = entry.canvas.as_mut().expect("created above");
    let regions = match regions {
        Some(r) if !resized => r,
        _ => vec![canvas.full()],
    };

    for &region in &regions {
        let mut t = canvas.target(region);
        t.fill_rect(region, COLOR_BG);
        // Maze underlay, then the prompt over it.
        if let Some(anim) = entry.anim.as_ref() {
            draw_maze(&mut t, anim);
        }
        if let (Some(layout), Some(font)) = (prompt.as_ref(), state.font.as_ref()) {
            draw_prompt(&mut t, font, layout);
        }
    }
    canvas.prompt_box = prompt.map(|l| l.bbox);

    if let Some(vp) = entry.viewport.as_ref() {
        vp.set_destination(w as i32, h as i32);
    }
    canvas.present(&state.shm, qh, &surface, &regions)?;
    entry.drawn = true;
    Ok(())
}

/// Commit frames that found every buffer busy, once one is free again.
fn present_leftovers(state: &mut State, qh: &QueueHandle<State>) {
    for entry in &mut state.outputs {
        let (Some(canvas), Some(surface)) = (entry.canvas.as_mut(), entry.surface.as_ref()) else {
            continue;
        };
        if canvas.has_unpresented() {
            if let Err(e) = canvas.present(&state.shm, qh, surface, &[]) {
                tracing::warn!(error = %e, "present failed");
            }
        }
    }
}

/// Where the password prompt goes on output `idx`, or `None` while hidden or
/// when there is no font to draw it with.
struct PromptLayout {
    lines: [String; 3],
    font_px: f32,
    /// The backdrop box; everything the prompt draws lies inside it.
    bbox: Rect,
    line_h: f32,
    ascent: f32,
}

fn prompt_layout(state: &State, idx: usize) -> Option<PromptLayout> {
    if !state.prompt_visible {
        return None;
    }
    let font = state.font.as_ref()?;
    let entry = &state.outputs[idx];
    let (w, h) = entry.size?;
    let scale = entry.scale;
    let pw = ((w as f64) * scale).round().max(1.0) as i32;
    let ph = ((h as f64) * scale).round().max(1.0) as f32;

    // Centered 3-line block with a solid backdrop so the text stays readable
    // over the maze.
    let title = format!("{} · locked", state.hostname);
    let masked: String = "•".repeat(state.password.chars().count());
    let prompt = format!("Password: {masked}");
    let lines = [title, prompt, state.status.clone()];
    let font_px = state.font_size * scale as f32;
    let line_metrics = font
        .horizontal_line_metrics(font_px)
        .unwrap_or(fontdue::LineMetrics {
            ascent: font_px * 0.8,
            descent: -font_px * 0.2,
            line_gap: font_px * 0.2,
            new_line_size: font_px * 1.2,
        });
    let line_h = (line_metrics.ascent - line_metrics.descent + line_metrics.line_gap).max(font_px);
    let total_h = line_h * lines.len() as f32;
    let max_text_w = lines
        .iter()
        .map(|l| measure_text(font, font_px, l))
        .max()
        .unwrap_or(0);
    let pad_x = (font_px * 1.5).round() as i32;
    let pad_y = (font_px * 0.8).round() as i32;
    let box_w = max_text_w + pad_x * 2;
    let box_h = total_h.round() as i32 + pad_y * 2;
    let box_x = (pw - box_w) / 2;
    let box_y = ((ph - total_h) / 2.0).round() as i32 - pad_y;
    Some(PromptLayout {
        lines,
        font_px,
        bbox: Rect::new(box_x, box_y, box_w, box_h),
        line_h,
        ascent: line_metrics.ascent,
    })
}

fn draw_prompt(t: &mut Target, font: &Font, layout: &PromptLayout) {
    let b = layout.bbox;
    t.fill_rect(b, COLOR_PROMPT_BG);
    let pad_y = (layout.font_px * 0.8).round() as i32;
    let text_top = b.y + pad_y;
    for (i, line) in layout.lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let text_w = measure_text(font, layout.font_px, line);
        let x = b.x + (b.w - text_w) / 2;
        let baseline_y = text_top + (i as f32 * layout.line_h + layout.ascent).round() as i32;
        draw_text(t, font, layout.font_px, x, baseline_y, line, 0xFFE0E0E0);
    }
}

/// Render the current state of `anim` within `t`'s clip. Visited
/// cells go down first (live green / dead red), inset by `wp`
/// on every side so the wall pass that follows ends up with the
/// xscreensaver-style black border on both sides of every line.
/// Open doorways between two visited cells get a small color bridge
/// across the wall plane so the trail reads as continuous. Only the
/// cells overlapping the clip are visited.
fn draw_maze(t: &mut Target, anim: &Animation) {
    let cs = anim.cell_px as i32;
    let (ox, oy) = anim.offset;
    let cols = anim.maze.width();
    let rows = anim.maze.height();
    // Wall thickness scales with cell size so the look stays
    // proportional on hi-dpi displays.
    let wp = ((anim.cell_px / 12).max(1)) as i32;
    let inner = cs - 2 * wp;

    // The cells the clip overlaps.
    let clip = t.clip;
    if clip.is_empty() {
        return;
    }
    let cell_range = |lo: i32, len: i32, origin: i32, count: u16| {
        let first = ((lo - origin).div_euclid(cs)).clamp(0, count as i32);
        let last = ((lo + len - 1 - origin).div_euclid(cs) + 1).clamp(0, count as i32);
        (first as u16)..(last as u16)
    };
    let xs = cell_range(clip.x, clip.w, ox, cols);
    let ys = cell_range(clip.y, clip.h, oy, rows);

    let color_of = |x: u16, y: u16| -> Option<u32> {
        match anim.solver.cell_state(x, y) {
            maze::CellState::Untouched => None,
            maze::CellState::Live => Some(COLOR_LIVE),
            maze::CellState::Dead => Some(COLOR_DEAD),
        }
    };

    for y in ys.clone() {
        for x in xs.clone() {
            let Some(color) = color_of(x, y) else {
                continue;
            };
            let px = ox + x as i32 * cs;
            let py = oy + y as i32 * cs;
            if inner > 0 {
                t.fill_rect(Rect::new(px + wp, py + wp, inner, inner), color);
            }
            // Bridge the wall gap toward each visited neighbour we
            // share an open doorway with. N/W per cell is enough —
            // S/E end up drawn by the neighbour's pass.
            let walls = anim.maze.walls(x, y);
            if walls & maze::WALL_N == 0 && y > 0 && color_of(x, y - 1).is_some() && inner > 0 {
                t.fill_rect(Rect::new(px + wp, py, inner, wp), color);
            }
            if walls & maze::WALL_W == 0 && x > 0 && color_of(x - 1, y).is_some() && inner > 0 {
                t.fill_rect(Rect::new(px, py + wp, wp, inner), color);
            }
        }
    }

    // Walls. N + W per cell covers every interior edge exactly once;
    // the S row + E column passes finish the outer border.
    for y in ys.clone() {
        for x in xs.clone() {
            let walls = anim.maze.walls(x, y);
            let px = ox + x as i32 * cs;
            let py = oy + y as i32 * cs;
            if walls & maze::WALL_N != 0 {
                t.fill_rect(Rect::new(px, py, cs, wp), COLOR_WALL);
            }
            if walls & maze::WALL_W != 0 {
                t.fill_rect(Rect::new(px, py, wp, cs), COLOR_WALL);
            }
        }
    }
    if ys.end == rows {
        let bottom_y = oy + rows as i32 * cs - wp;
        for x in xs.clone() {
            if anim.maze.walls(x, rows - 1) & maze::WALL_S != 0 {
                t.fill_rect(Rect::new(ox + x as i32 * cs, bottom_y, cs, wp), COLOR_WALL);
            }
        }
    }
    if xs.end == cols {
        let right_x = ox + cols as i32 * cs - wp;
        for y in ys {
            if anim.maze.walls(cols - 1, y) & maze::WALL_E != 0 {
                t.fill_rect(Rect::new(right_x, oy + y as i32 * cs, wp, cs), COLOR_WALL);
            }
        }
    }
}

/// The region a change to cell `(x, y)` repaints: the cell itself plus its
/// east and south neighbours, whose doorway bridges take the cell's colour.
fn cell_region(anim: &Animation, x: u16, y: u16) -> Rect {
    let cs = anim.cell_px as i32;
    let (ox, oy) = anim.offset;
    Rect::new(ox + x as i32 * cs, oy + y as i32 * cs, 2 * cs, 2 * cs)
}

fn measure_text(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as i32
}

fn draw_text(
    t: &mut Target,
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
        t.blit_alpha(
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

/// The machine's name for the prompt title. gethostname(3) works everywhere;
/// `/etc/hostname` is a fallback only (FreeBSD keeps the name in rc.conf and
/// has no such file).
fn read_hostname() -> String {
    system_hostname()
        .or_else(|| {
            fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "host".into())
}

fn system_hostname() -> Option<String> {
    // 256 covers both HOST_NAME_MAX (64 on Linux) and MAXHOSTNAMELEN (256 on
    // FreeBSD). Passing one byte less keeps a NUL at the end even if the name
    // were truncated, where POSIX leaves termination unspecified.
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    let name = String::from_utf8_lossy(&buf[..len]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_comes_from_the_system() {
        let name = system_hostname().expect("gethostname");
        assert!(!name.is_empty() && !name.contains('\0'), "{name:?}");
        assert_eq!(read_hostname(), name);
    }

    /// Paint `region` of a `w`×`h` frame the way [`redraw`] does, minus the
    /// prompt.
    fn paint(px: &mut [u8], w: u32, h: u32, anim: &Animation, region: Rect) {
        let mut t = Target {
            px,
            w,
            clip: region.intersect(&Rect::new(0, 0, w as i32, h as i32)),
        };
        t.fill_rect(region, COLOR_BG);
        draw_maze(&mut t, anim);
    }

    /// Repainting only each changed cell's region, frame after frame, leaves
    /// exactly the pixels a full repaint would.
    #[test]
    fn cell_regions_cover_every_changed_pixel() {
        let (w, h) = (403, 301); // not a multiple of the cell size
        let mut anim = Animation::new(w, h);
        anim.phase = Phase::Solving;
        let full = Rect::new(0, 0, w as i32, h as i32);
        let mut incremental = vec![0u8; (w * h * 4) as usize];
        paint(&mut incremental, w, h, &anim, full);

        let mut frames = 0;
        while !anim.solver.finished() {
            if let Change::Cells(cells) = anim.tick() {
                for (x, y) in cells {
                    let region = cell_region(&anim, x, y);
                    paint(&mut incremental, w, h, &anim, region);
                }
            }
            frames += 1;
            if frames % 50 == 0 || anim.solver.finished() {
                let mut fresh = vec![0u8; (w * h * 4) as usize];
                paint(&mut fresh, w, h, &anim, full);
                assert!(
                    incremental == fresh,
                    "frame {frames} differs from a full repaint"
                );
            }
        }
        assert!(frames > 10, "the solver finished too fast to test anything");
    }
}
