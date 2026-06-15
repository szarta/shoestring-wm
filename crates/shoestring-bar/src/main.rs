//! shoestring-bar: a lightweight Wayland status bar.
//!
//! Layer-shell anchored to the bottom of the focused output, drawing into a
//! single `wl_shm` buffer. Renders a workspace indicator on the left, a
//! window list (with focused-entry highlight) in the middle, and a clock
//! on the right. Live state comes from shoestring-wm's IPC socket.

// Drawing primitives intentionally take (mmap, w, h, x, y, w, h, color)
// in positional form — wrapping them in a struct buys nothing readable.
#![allow(clippy::too_many_arguments)]

mod battery;
mod config;
mod icons;
mod ipc_client;
mod menu;
mod tray;

use std::{
    collections::HashMap,
    ffi::CString,
    fs,
    os::fd::{AsFd, AsRawFd},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{anyhow, bail, Context, Result};
use config::{default_config_path, default_config_toml, Config, Position};
use fontdue::{Font, FontSettings};
use memmap2::MmapMut;
use shoestring_ipc::Event as IpcEvent;
use tracing_subscriber::EnvFilter;
use wayland_client::{
    backend::ObjectId,
    event_created_child,
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_keyboard::{self, KeyState, WlKeyboard},
        wl_output::WlOutput,
        wl_pointer::{self, ButtonState, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1, EVT_TOPLEVEL_OPCODE},
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
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

/// Dim color for inactive workspace boxes and unfocused entry backgrounds.
/// Not currently user-configurable (the M24 config spec only exposes bg/fg).
pub(crate) const DIM: u32 = 0xFF_55_55_55;
/// Accent: focused workspace box and focused window's highlight backdrop.
/// Not currently user-configurable.
pub(crate) const ACCENT: u32 = 0xFF_55_77_BB;
/// Unfocused window-list entry chip background. Matches DIM so it uses the
/// same visual grammar as the inactive workspace boxes on the left.
const ENTRY_BG: u32 = DIM;
/// Battery indicator color at or below `battery_low_threshold`. Orange.
const BAT_LOW: u32 = 0xFF_FF_AA_55;
/// Battery indicator color at or below `battery_critical_threshold`. Red.
const BAT_CRITICAL: u32 = 0xFF_FF_55_55;
/// Fill for the capture chip while a frame was *just* read (live "your screen
/// is being captured right now" signal). Red, matching the recording-light
/// convention; distinct from the calmer accent used when capture is merely
/// armed.
const CAPTURE_ACTIVE: u32 = 0xFF_FF_33_33;
/// How long the capture chip stays in its red "active" state after the last
/// `screen_captured` event. Longer than the WM's ~400ms event throttle so a
/// steady cast holds the light solid; the ~1Hz redraw clears it after.
const CAPTURE_DOT_TTL: Duration = Duration::from_millis(1500);
/// Horizontal text inset from the bar edges.
const PADDING_X: i32 = 8;

/// Workspace box layout (left side). Count comes from the WM at startup
/// via [`ipc_client::query_workspaces`]; geometry is fixed.
const WS_BOX_W: i32 = 8;
const WS_BOX_H: i32 = 8;
const WS_GAP: i32 = 4;
/// Gap between the workspace cluster and the start of the window list.
const WS_LIST_GAP: i32 = 12;
/// Pixel gap between adjacent window-list entries (separator-less; the
/// focused-entry highlight is the visual divider).
const ENTRY_GAP: i32 = 12;

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
    cfg: Config,
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    font: Font,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    size: Option<(u32, u32)>,
    /// Fractional scale for the surface (1.0 until the compositor sends a
    /// `wp_fractional_scale.preferred_scale`). The buffer is rendered at
    /// `logical * scale` px and mapped back to logical via `viewport`.
    scale: f64,
    viewport: Option<WpViewport>,
    #[allow(dead_code)]
    fractional: Option<WpFractionalScaleV1>,
    /// Reused `wl_shm` buffer. Allocated lazily and only reallocated when
    /// the bar's pixel dimensions change; every redraw maps it afresh and
    /// re-attaches the same `wl_buffer`. Allocating a new pool+buffer per
    /// redraw (the previous behaviour) leaked one compositor-side fd each
    /// frame — the pool's mapping is held until its `wl_buffer` is
    /// destroyed, which the per-frame path never did.
    buffer: Option<BarBuffer>,
    /// Live ext-foreign-toplevel-list entries, keyed by the handle's
    /// wayland object id. Updated incrementally on each `done` event;
    /// removed on `closed`. Activation state comes from the [`focused_id`]
    /// field below (sourced via M9 IPC), since ext-FT itself doesn't
    /// expose it.
    toplevels: HashMap<ObjectId, Toplevel>,
    /// 1-based active workspace; sourced from a `Workspaces` query at
    /// startup and updated by `workspace_changed` events.
    active_workspace: u8,
    /// Total workspace count (cached from the same startup query).
    workspace_count: u8,
    /// Display name per workspace (1-based positionally). Length ==
    /// `workspace_count` once seeded from the IPC `Workspaces`
    /// response; entries are empty strings for unnamed slots. The
    /// active workspace's non-empty name is rendered to the right of
    /// the box cluster.
    workspace_names: Vec<String>,
    /// `ext-foreign-toplevel-list` identifier of the currently focused
    /// window, sourced from `window_focused` events. `None` means no
    /// window holds keyboard focus.
    focused_id: Option<String>,
    /// Runtime state of the WM's automation gate. Sourced from
    /// [`ipc_client::query_automation`] at startup and updated by
    /// `automation_changed` events. When `true` the bar shows an
    /// "AUTO" chip next to the clock so the user can always tell at
    /// a glance that injected input / remote commands are allowed.
    automation_enabled: bool,
    /// Runtime state of the WM's screen-capture gate. Sourced from
    /// [`ipc_client::query_screen_capture`] at startup and updated by
    /// `screen_capture_changed` events. When `true` the bar shows a "CAP"
    /// chip so the user always knows screen capture is *possible*.
    screen_capture_enabled: bool,
    /// Wall-clock instant of the most recent `screen_captured` event, i.e.
    /// the last time a frame was actually read. `Some` lights a live
    /// "capturing now" dot that decays a short while after the events stop
    /// (the ~1Hz redraw tick clears it). The strongest privacy signal.
    last_capture: Option<Instant>,
    /// 1-based workspace for each window, keyed by ext-FT identifier.
    /// Populated from `window_opened` and updated by
    /// `window_moved_to_workspace`; ext-FT itself does not expose
    /// workspace info. Entries without a mapping (haven't seen the
    /// matching IPC event yet) are treated as "unknown" and hidden
    /// from the bar list — the WM emits `window_opened` synchronously
    /// with the ext-FT announce so this only matters momentarily.
    window_workspaces: HashMap<String, u8>,
    /// Set whenever any input that affects the rendered output changes
    /// (toplevel arrival/departure, clock tick, etc). Cleared after the
    /// next paint.
    dirty: bool,
    /// Last rendered clock string; we re-paint when this changes, which
    /// makes sub-minute formats (e.g. `iso` → seconds) work without any
    /// extra config plumbing.
    last_clock: String,
    /// `wl_seat`, kept alive so its `Capabilities` events keep arriving
    /// and we can hand out a pointer when the compositor advertises one.
    /// `None` when the compositor doesn't expose a seat at all (e.g.
    /// headless test rig). Read in the WlSeat dispatch via the proxy
    /// passed there, so the field is only here for its drop lifetime.
    #[allow(dead_code)]
    seat: Option<WlSeat>,
    /// Bar-side pointer; created when the seat first advertises the
    /// `pointer` capability and destroyed if it goes away (laptop lid
    /// closed, etc.).
    pointer: Option<WlPointer>,
    /// Surface-local pointer x in pixels. `None` while the pointer is
    /// not over the bar (we get `enter` / `leave` events).
    pointer_x: Option<f64>,
    /// (x_start, width, FT identifier) for each window-list entry from
    /// the last [`draw_window_list`] pass. Hit-tested by the pointer
    /// button handler; cleared and rebuilt on every redraw.
    window_entry_rects: Vec<(i32, i32, String)>,
    /// `wp_viewporter` manager, kept so the tray menu can give its own
    /// surface a viewport for HiDPI mapping. `None` if unsupported.
    viewporter: Option<WpViewporter>,
    /// Bar-side keyboard, bound when the seat advertises the capability.
    /// Used only to drive + dismiss the tray menu (the bar itself takes
    /// no keyboard focus).
    keyboard: Option<WlKeyboard>,
    /// Surface-local pointer y in pixels; companion to `pointer_x`, needed
    /// for vertical hit-testing inside the tray menu.
    pointer_y: Option<f64>,
    /// True while the pointer is over the open tray menu (vs the bar), so
    /// the button handler routes the click correctly.
    pointer_on_menu: bool,
    /// (x_start, width, tray item index) for each rendered tray icon from
    /// the last redraw, in logical coords. Hit-tested to open a menu.
    tray_icon_rects: Vec<(i32, i32, usize)>,
    /// The open tray context menu, if any.
    menu: Option<menu::Menu>,
    /// Set when the menu needs a repaint (hover/nav/configure change).
    menu_dirty: bool,
    /// Tray item index whose menu the loop should fetch + open (set by the
    /// pointer handler; consumed in the loop where the D-Bus conn lives).
    pending_open_menu: Option<usize>,
    /// dbusmenu id the loop should send a `clicked` event for, then dismiss.
    pending_menu_click: Option<i32>,
    /// Auto-detected battery source. `None` on systems with no
    /// recognised battery — the indicator is hidden entirely in that
    /// case so the bar layout doesn't reserve dead space.
    battery_source: Option<Box<dyn battery::BatterySource>>,
    /// Most recent reading. `None` until the first successful poll
    /// (or if the source temporarily fails to read).
    battery_reading: Option<battery::BatteryReading>,
    /// Wall-clock time of the last battery poll. Polling cadence is
    /// 1s, matching the existing event-loop tick — no extra fds.
    last_battery_poll: SystemTime,
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
    let cli = parse_cli().context("parsing CLI arguments")?;
    match cli {
        CliAction::Run => {}
        CliAction::Help => {
            print!("{}", help_text());
            return Ok(());
        }
        CliAction::Version => {
            println!("shoestring-bar {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        CliAction::WriteDefaultConfig { force } => {
            return write_default_config_to_disk(force);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    run_with_reconnect()
}

/// Reconnect supervisor. A Wayland client cannot resurrect a severed
/// `wl_display`, so when the compositor drops our connection — GPU reset,
/// modeset on an AC<->battery power transition, compositor restart, VT
/// switch — the only recovery is to tear everything down and connect
/// afresh. We retry with capped exponential backoff, resetting the backoff
/// once a session has stayed up long enough to count as stable so a brief
/// glitch doesn't permanently inflate the delay. Errors that are *not* a
/// lost connection (missing globals, no `$WAYLAND_DISPLAY`, a protocol
/// violation) propagate out and stop the bar — reconnecting would only
/// reproduce them.
fn run_with_reconnect() -> Result<()> {
    const BASE_DELAY: Duration = Duration::from_millis(200);
    const MAX_DELAY: Duration = Duration::from_secs(5);
    const STABLE_RUN: Duration = Duration::from_secs(30);

    let mut delay = BASE_DELAY;
    loop {
        let started = Instant::now();
        match run_session() {
            Ok(()) => return Ok(()),
            Err(e) if is_disconnect(&e) => {
                if started.elapsed() >= STABLE_RUN {
                    delay = BASE_DELAY;
                }
                tracing::warn!(
                    error = ?e,
                    backoff_ms = delay.as_millis() as u64,
                    "wayland connection lost; reconnecting"
                );
                std::thread::sleep(delay);
                delay = (delay * 2).min(MAX_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// True when `err` (anywhere in its cause chain) is a lost Wayland
/// connection — an I/O-level failure such as `ConnectionReset` from the
/// socket peer, or a raw `poll(2)` error. Protocol errors deliberately do
/// *not* count: those signal a client bug that a reconnect would only
/// reproduce, so they should surface loudly instead of flapping.
fn is_disconnect(err: &anyhow::Error) -> bool {
    use wayland_client::backend::WaylandError;
    use wayland_client::DispatchError;
    err.chain().any(|cause| {
        if let Some(w) = cause.downcast_ref::<WaylandError>() {
            return matches!(w, WaylandError::Io(_));
        }
        if let Some(d) = cause.downcast_ref::<DispatchError>() {
            return matches!(d, DispatchError::Backend(WaylandError::Io(_)));
        }
        cause.downcast_ref::<std::io::Error>().is_some()
    })
}

/// Establish and run one Wayland session: connect, bind globals, build
/// state, create the layer surface, then drive the event loop until either
/// a clean shutdown (`Ok`) or a connection loss / fatal error (`Err`).
/// Called once per (re)connect by [`run_with_reconnect`]; everything it
/// allocates is dropped when it returns, so a reconnect starts from a clean
/// slate (fresh `Connection`, surfaces, IPC stream, and battery source).
fn run_session() -> Result<()> {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = ?e, "config load failed; using defaults");
            Config::default()
        }
    };

    let font = load_font(cfg.font.as_deref()).context("could not load any system font")?;

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
    // Optional HiDPI globals: present under shoestring-wm.
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional_mgr: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
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

    // Bind wl_seat so we can receive pointer button events. Missing seat
    // is non-fatal — the bar still renders, the user just can't click
    // entries. Version range matches what mainstream compositors ship.
    let seat: Option<WlSeat> = match globals.bind(&qh, 1..=7, ()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = ?e, "no wl_seat global; bar will be non-interactive");
            None
        }
    };

    // Bootstrap initial workspace from the WM via a one-shot IPC query.
    // Failure is non-fatal — we default to ws=1/count=16 and rely on
    // subsequent event updates.
    let (active_workspace, workspace_count, workspace_names) = ipc_client::query_workspaces()
        .unwrap_or_else(|e| {
            tracing::warn!(error = ?e, "ipc workspace query failed; using defaults");
            (1, 16, Vec::new())
        });

    // Bootstrap per-window workspace mappings. ext-FT will replay every
    // pre-existing toplevel to us, but the WM's `window_opened` IPC
    // events are not replayed on subscribe, so without this the bar
    // would treat every pre-existing window as "workspace unknown" and
    // hide it.
    let initial_windows = ipc_client::query_windows().unwrap_or_else(|e| {
        tracing::warn!(error = ?e, "ipc windows query failed; window list will populate as events arrive");
        Vec::new()
    });
    let window_workspaces: HashMap<String, u8> = initial_windows
        .into_iter()
        .map(|w| (w.id, w.workspace))
        .collect();

    // Bootstrap the automation gate state. Non-fatal: defaulting to
    // `false` matches the WM's default gate position, and any subsequent
    // `automation_changed` event will correct us if we guessed wrong.
    let automation_enabled = ipc_client::query_automation().unwrap_or_else(|e| {
        tracing::warn!(error = ?e, "ipc automation query failed; assuming off");
        false
    });

    // Same bootstrap for the screen-capture gate.
    let screen_capture_enabled = ipc_client::query_screen_capture().unwrap_or_else(|e| {
        tracing::warn!(error = ?e, "ipc screen-capture query failed; assuming off");
        false
    });

    // Detect the battery source once at startup. If `battery_show` is
    // off, skip detection entirely so we don't even open the sysfs
    // dir on platforms where the user has opted out.
    let battery_source = if cfg.battery_show {
        battery::auto_detect()
    } else {
        None
    };
    if battery_source.is_some() {
        tracing::info!("battery indicator: source detected");
    } else if cfg.battery_show {
        tracing::debug!("battery indicator: no battery detected; hidden");
    }
    let battery_reading = battery_source.as_ref().and_then(|s| s.read());

    let mut state = State {
        cfg,
        compositor,
        shm,
        layer_shell,
        font,
        surface: None,
        layer_surface: None,
        size: None,
        scale: 1.0,
        viewport: None,
        fractional: None,
        buffer: None,
        toplevels: HashMap::new(),
        active_workspace,
        workspace_count,
        workspace_names,
        focused_id: None,
        automation_enabled,
        screen_capture_enabled,
        last_capture: None,
        window_workspaces,
        dirty: false,
        last_clock: String::new(),
        seat,
        pointer: None,
        pointer_x: None,
        window_entry_rects: Vec::new(),
        viewporter,
        keyboard: None,
        pointer_y: None,
        pointer_on_menu: false,
        tray_icon_rects: Vec::new(),
        menu: None,
        menu_dirty: false,
        pending_open_menu: None,
        pending_menu_click: None,
        battery_source,
        battery_reading,
        last_battery_poll: SystemTime::now(),
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
    let edge_anchor = match state.cfg.position {
        Position::Bottom => Anchor::Bottom,
        Position::Top => Anchor::Top,
    };
    layer_surface.set_size(0, state.cfg.height);
    layer_surface.set_anchor(edge_anchor | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(state.cfg.height as i32);
    // Opt into fractional scaling so the bar renders crisp on HiDPI outputs.
    if let (Some(vp), Some(mgr)) = (&state.viewporter, &fractional_mgr) {
        state.viewport = Some(vp.get_viewport(&surface, &qh, ()));
        state.fractional = Some(mgr.get_fractional_scale(&surface, &qh, ()));
    }
    surface.commit();

    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    // Subscribe to WM events. None means IPC is unavailable — we still run
    // with whatever defaults we have; the bar just won't update on focus
    // or workspace changes.
    let mut event_stream = match ipc_client::open_event_stream() {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = ?e, "ipc event stream unavailable; running without live updates");
            None
        }
    };

    tracing::info!("shoestring-bar ready, waiting for configure");

    event_loop(conn, &mut queue, &qh, &mut state, event_stream.as_mut())
}

// ---- CLI -----------------------------------------------------------------

enum CliAction {
    Run,
    Help,
    Version,
    WriteDefaultConfig { force: bool },
}

/// Hand-parses argv. Avoids pulling clap for two flags + help/version —
/// staying lean is one of the project's stated goals (see
/// [[project-shoestring-wm-dependencies]] in author memory).
fn parse_cli() -> Result<CliAction> {
    let mut write_config = false;
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--write-default-config" => write_config = true,
            "--force" => force = true,
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            other => bail!("unknown argument: {other:?} (try --help)"),
        }
    }
    if force && !write_config {
        bail!("--force only makes sense with --write-default-config");
    }
    if write_config {
        Ok(CliAction::WriteDefaultConfig { force })
    } else {
        Ok(CliAction::Run)
    }
}

fn help_text() -> String {
    format!(
        "shoestring-bar {ver} — lightweight Wayland status bar.

Usage: shoestring-bar [OPTIONS]

Options:
      --write-default-config   Write the bundled default config to the
                               user's config path and exit. Refuses to
                               overwrite unless --force is also passed.
      --force                  Allow --write-default-config to overwrite.
  -h, --help                   Print this help and exit.
  -V, --version                Print version and exit.

Config file: $XDG_CONFIG_HOME/shoestring-bar/config.toml (defaults to
~/.config/shoestring-bar/config.toml). Run without flags to start the bar.

Environment: WAYLAND_DISPLAY (required), SHOESTRING_WM_SOCKET (optional,
enables live workspace/focus updates), SHOESTRING_BAR_FONT (font path
override), RUST_LOG (tracing filter).
",
        ver = env!("CARGO_PKG_VERSION"),
    )
}

fn write_default_config_to_disk(force: bool) -> Result<()> {
    let target = default_config_path()
        .ok_or_else(|| anyhow!("no config path resolvable: set $HOME or $XDG_CONFIG_HOME"))?;
    if target.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&target, default_config_toml())
        .with_context(|| format!("writing {}", target.display()))?;
    println!("wrote default config to {}", target.display());
    Ok(())
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
    mut event_stream: Option<&mut ipc_client::EventStream>,
) -> Result<()> {
    // System-tray (StatusNotifier) host. `None` if there's no session bus or
    // another tray already owns the watcher — the bar just runs trayless then.
    let mut tray = tray::Tray::new();

    while state.running {
        // 1. Repaint if the clock string has changed since the last paint.
        //    Comparing the rendered string (rather than a minute counter)
        //    means sub-minute formats like `iso` redraw every second
        //    without any extra plumbing, while minute formats stay cheap
        //    (string equality is fast, and we still only allocate one
        //    String per tick).
        let now_clock = format_clock_now(&state.cfg.clock_format);
        if state.last_clock != now_clock {
            state.last_clock = now_clock;
            state.dirty = true;
        }

        // Decay the live capture indicator: once no `screen_captured` event
        // has arrived for CAPTURE_DOT_TTL, drop back from the red "active"
        // state. The ~1Hz poll tick below bounds how long the stale red
        // lingers after a cast ends.
        if state
            .last_capture
            .is_some_and(|t| t.elapsed() >= CAPTURE_DOT_TTL)
        {
            state.last_capture = None;
            state.dirty = true;
        }

        // Re-poll the battery at most once per second. The poll() call
        // below caps at 1000ms, so this naturally rate-limits to ~1Hz
        // without any timer plumbing. Only mark dirty when the reading
        // actually changed — otherwise a static 85% battery would
        // force a full redraw every tick.
        if let Some(src) = state.battery_source.as_ref() {
            let due = state
                .last_battery_poll
                .elapsed()
                .map(|e| e.as_secs() >= 1)
                .unwrap_or(true);
            if due {
                state.last_battery_poll = SystemTime::now();
                let new_reading = src.read();
                if state.battery_reading != new_reading {
                    state.battery_reading = new_reading;
                    state.dirty = true;
                }
            }
        }
        if state.dirty {
            if let Some((w, h)) = state.size {
                state.dirty = false;
                if let Err(e) = redraw(state, qh, w, h, tray.as_mut()) {
                    tracing::error!(error = ?e, "redraw failed");
                }
            }
        }

        // 2. Flush any queued requests, then prepare to read.
        conn.flush()?;
        let guard = conn.prepare_read();
        let wayland_fd = guard.as_ref().map(|g| g.connection_fd().as_raw_fd());
        let ipc_fd = event_stream.as_ref().map(|s| s.as_raw_fd());

        // Build a pollfd array over whichever of wayland / ipc / dbus-tray
        // are present this iteration.
        let tray_fd = tray.as_ref().map(|t| t.fd());
        let mut pfds: [libc::pollfd; 3] = unsafe { std::mem::zeroed() };
        let mut nfds: libc::nfds_t = 0;
        let mut wayland_idx: Option<usize> = None;
        let mut ipc_idx: Option<usize> = None;
        let mut tray_idx: Option<usize> = None;
        if let Some(fd) = wayland_fd {
            pfds[nfds as usize] = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            wayland_idx = Some(nfds as usize);
            nfds += 1;
        }
        if let Some(fd) = ipc_fd {
            pfds[nfds as usize] = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            ipc_idx = Some(nfds as usize);
            nfds += 1;
        }
        if let Some(fd) = tray_fd {
            pfds[nfds as usize] = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            tray_idx = Some(nfds as usize);
            nfds += 1;
        }

        // Sleep until any fd is readable or the next second ticks.
        // 1s granularity is fine for minute-resolution clock.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, 1000) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }

        if let Some(guard) = guard {
            let idx = wayland_idx.expect("wayland_idx must be set if guard exists");
            if n > 0 && pfds[idx].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    // WouldBlock here is benign — POLLIN can fire after the
                    // wayland-rs library already drained the fd inside
                    // prepare_read.
                    if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        tracing::warn!(error = ?e, "wayland read failed");
                    }
                }
            } else {
                drop(guard);
            }
        }

        if let (Some(idx), Some(stream)) = (ipc_idx, event_stream.as_deref_mut()) {
            if pfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                match stream.drain_events() {
                    Ok(events) => {
                        for ev in events {
                            apply_ipc_event(state, ev);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "ipc stream read failed; closing");
                        event_stream = None;
                    }
                }
            }
        }

        // Drain the D-Bus tray socket on readability (or unconditionally if we
        // couldn't poll its fd this round). refill_all is nonblocking.
        if let (Some(idx), Some(t)) = (tray_idx, tray.as_mut()) {
            if pfds[idx].revents & (libc::POLLIN | libc::POLLHUP) != 0 && t.dispatch() {
                state.dirty = true;
            }
        }

        // 3. Dispatch any events we got from `read()` above OR events
        //    that wayland-rs already had buffered (prepare_read returned None).
        queue.dispatch_pending(state)?;

        // 4. Tray-menu commands queued by the input handlers, executed here
        //    where the D-Bus connection (`tray`) is in scope.
        if let Some(idx) = state.pending_open_menu.take() {
            open_tray_menu(state, qh, tray.as_mut(), idx);
        }
        if let Some(id) = state.pending_menu_click.take() {
            if let (Some(t), Some(m)) = (tray.as_mut(), state.menu.as_ref()) {
                let ti = m.tray_idx;
                t.menu_clicked(ti, id);
            }
            if let Some(m) = state.menu.take() {
                m.destroy();
            }
        }
        if state.menu_dirty {
            state.menu_dirty = false;
            render_menu(state, qh);
        }
    }
    Ok(())
}

fn apply_ipc_event(state: &mut State, event: IpcEvent) {
    match event {
        IpcEvent::WorkspaceChanged { active } => {
            if state.active_workspace != active {
                state.active_workspace = active;
                state.dirty = true;
            }
        }
        IpcEvent::WindowFocused { id } => {
            if state.focused_id != id {
                state.focused_id = id;
                state.dirty = true;
            }
        }
        // ext-foreign-toplevel-list carries title/app_id and open/close
        // already, but it does *not* expose workspace. Track that
        // separately so the visible window list can be filtered to the
        // active workspace.
        IpcEvent::WindowOpened { id, workspace, .. } => {
            state.window_workspaces.insert(id, workspace);
            state.dirty = true;
        }
        IpcEvent::WindowClosed { id } => {
            state.window_workspaces.remove(&id);
            // ext-FT's `closed` already pulled the Toplevel entry and
            // marked dirty; nothing extra to do for the visible list.
        }
        IpcEvent::WindowMovedToWorkspace { id, workspace } => {
            state.window_workspaces.insert(id, workspace);
            state.dirty = true;
        }
        IpcEvent::AutomationChanged { enabled } => {
            if state.automation_enabled != enabled {
                state.automation_enabled = enabled;
                state.dirty = true;
            }
        }
        IpcEvent::ScreenCaptureChanged { enabled } => {
            if state.screen_capture_enabled != enabled {
                state.screen_capture_enabled = enabled;
                state.dirty = true;
            }
        }
        IpcEvent::ScreenCaptured { .. } => {
            // Refresh the live "capturing now" indicator; the event-loop tick
            // clears it once captures stop (see CAPTURE_DOT_TTL).
            state.last_capture = Some(Instant::now());
            state.dirty = true;
        }
        IpcEvent::WindowTitleChanged { .. }
        | IpcEvent::WindowRestacked { .. }
        | IpcEvent::WindowStickyChanged { .. }
        | IpcEvent::WindowAlwaysOnTopChanged { .. }
        | IpcEvent::OutputAdded(_)
        | IpcEvent::OutputRemoved { .. }
        | IpcEvent::ConfigReloaded
        | IpcEvent::Metrics { .. } => {}
    }
}

/// Resolve a font in priority order: explicit config path, then
/// $SHOESTRING_BAR_FONT, then the built-in candidate list. Returning the
/// first one that successfully loads.
fn load_font(cfg_path: Option<&Path>) -> Result<Font> {
    if let Some(p) = cfg_path {
        let bytes =
            fs::read(p).with_context(|| format!("bar.font: cannot read {}", p.display()))?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("font parse: {e}"));
    }
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

/// A reused `wl_shm` buffer: a tempfile-backed pool whose `wl_buffer` we
/// re-attach every frame. Recreated only when the bar's dimensions change.
pub(crate) struct BarBuffer {
    /// Backing tempfile, mapped afresh each redraw. The compositor holds
    /// its own dup of this fd via the pool, so it stays valid for the
    /// buffer's lifetime.
    pub(crate) tmp: std::fs::File,
    /// The attachable buffer. Destroying it releases the compositor-side
    /// pool mapping; we only do that when reallocating for a new size.
    pub(crate) buffer: WlBuffer,
    /// Pixel dimensions this buffer was allocated for.
    pub(crate) dims: (u32, u32),
}

/// Compose the bar into its reused wl_buffer and attach it.
/// Re-runs on every configure event and on state changes.
fn redraw(
    state: &mut State,
    qh: &QueueHandle<State>,
    w: u32,
    h: u32,
    tray: Option<&mut tray::Tray>,
) -> Result<()> {
    // `(w, h)` is the logical surface size from the layer-shell configure. The
    // buffer is rendered at physical resolution `(pw, ph)` and mapped back to
    // logical via the viewport, so every layout dimension below is scaled into
    // physical px. `s(v)` scales a logical length; click hit-test rects are
    // converted back to logical before storing (pointer coords stay logical).
    let scale = state.scale;
    let pw = ((w as f64) * scale).round().max(1.0) as u32;
    let ph = ((h as f64) * scale).round().max(1.0) as u32;
    let s = |v: i32| ((v as f64) * scale).round() as i32;

    let stride = pw as i32 * 4;
    let size = (stride as usize) * ph as usize;

    // (Re)allocate the shm buffer only when missing or the size changed.
    // Destroying the old wl_buffer releases the compositor's pool fd —
    // skipping this per frame is the entire fix (see `State::buffer`).
    if state.buffer.as_ref().is_none_or(|b| b.dims != (pw, ph)) {
        if let Some(old) = state.buffer.take() {
            old.buffer.destroy();
        }
        let tmp = tempfile::tempfile()?;
        tmp.set_len(size as u64)?;
        let pool = state.shm.create_pool(tmp.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, pw as i32, ph as i32, stride, Format::Argb8888, qh, ());
        // The wl_buffer keeps the pool's mapping alive, so we can drop the
        // pool object now and just hold onto the buffer.
        pool.destroy();
        state.buffer = Some(BarBuffer {
            tmp,
            buffer,
            dims: (pw, ph),
        });
    }

    let bg = state.cfg.background;
    let fg = state.cfg.foreground;
    let font_px = state.cfg.font_size * scale as f32;

    // Map the persisted backing file for this frame. `map_mut` only borrows
    // the file for the call; the returned mapping is independent, so the
    // rest of the draw can freely borrow `state`.
    let mut mmap =
        unsafe { MmapMut::map_mut(&state.buffer.as_ref().expect("buffer ensured above").tmp)? };

    // We re-fill via the mmap (faster than seek+write) so subsequent draws
    // can mutate in place. Background first, then text on top. All coords are
    // physical px from here on (`pw`/`ph` and `s(..)`-scaled constants).
    fill_bg(&mut mmap, pw, ph, bg);

    // Right: clock. Drawn first so we know how much horizontal space
    // remains for the (truncatable) window list in the middle.
    let clock = format_clock_now(&state.cfg.clock_format);
    let clock_w = measure_text(&state.font, font_px, &clock);
    let clock_x = pw as i32 - s(PADDING_X) - clock_w;
    draw_text(&mut mmap, pw, ph, &state.font, font_px, clock_x, &clock, fg);

    // Automation indicator chip: drawn immediately left of the clock when
    // the WM's automation gate is ON. Hidden when off — the user should
    // always be able to tell at a glance that injected input / remote
    // commands are currently allowed. Same accent-fill + fg-text shape
    // as the focused-entry highlight in the window list, so the visual
    // grammar matches across the bar.
    let auto_chip_left = if state.automation_enabled {
        const AUTO_LABEL: &str = "AUTO";
        let auto_pad = s(4);
        let auto_clock_gap = s(8);
        let label_w = measure_text(&state.font, font_px, AUTO_LABEL);
        let chip_w = label_w + auto_pad * 2;
        let chip_right = clock_x - auto_clock_gap;
        let chip_left = chip_right - chip_w;
        fill_rect(
            &mut mmap,
            pw,
            ph,
            chip_left,
            s(2),
            chip_w,
            ph as i32 - s(4),
            ACCENT,
        );
        draw_text(
            &mut mmap,
            pw,
            ph,
            &state.font,
            font_px,
            chip_left + auto_pad,
            AUTO_LABEL,
            fg,
        );
        Some(chip_left)
    } else {
        None
    };

    // Screen-capture indicator chip: drawn immediately left of the AUTO chip
    // (or left of the clock when AUTO is hidden) whenever the WM's capture
    // gate is ON, so the user can always tell screen capture is *possible*.
    // It turns red while a frame is actually being read (the live "your
    // screen is being captured right now" signal); otherwise it sits in the
    // calmer accent color to mean "armed but idle".
    let cap_chip_left = if state.screen_capture_enabled {
        const CAP_LABEL: &str = "CAP";
        let cap_pad = s(4);
        let cap_gap = s(8);
        let active = state
            .last_capture
            .is_some_and(|t| t.elapsed() < CAPTURE_DOT_TTL);
        let label_w = measure_text(&state.font, font_px, CAP_LABEL);
        let chip_w = label_w + cap_pad * 2;
        let chip_right = auto_chip_left.unwrap_or(clock_x) - cap_gap;
        let chip_left = chip_right - chip_w;
        fill_rect(
            &mut mmap,
            pw,
            ph,
            chip_left,
            s(2),
            chip_w,
            ph as i32 - s(4),
            if active { CAPTURE_ACTIVE } else { ACCENT },
        );
        draw_text(
            &mut mmap,
            pw,
            ph,
            &state.font,
            font_px,
            chip_left + cap_pad,
            CAP_LABEL,
            fg,
        );
        Some(chip_left)
    } else {
        None
    };

    // Battery indicator: drawn immediately left of the capture/AUTO chips (or
    // left of the clock when both are hidden). Skipped when no source
    // was detected or `battery_show = false` (which short-circuits
    // detection up in main).
    let battery_left = match (state.battery_source.as_ref(), state.battery_reading) {
        (Some(_), Some(reading)) => {
            let bat_gap = s(8);
            let text = battery::format_reading(&reading, &state.cfg.battery_format);
            let text_w = measure_text(&state.font, font_px, &text);
            let right_anchor = cap_chip_left.or(auto_chip_left).unwrap_or(clock_x);
            let bat_x = right_anchor - bat_gap - text_w;
            let color = battery::pick_color(
                &reading,
                fg,
                BAT_LOW,
                state.cfg.battery_low_threshold,
                BAT_CRITICAL,
                state.cfg.battery_critical_threshold,
            );
            draw_text(&mut mmap, pw, ph, &state.font, font_px, bat_x, &text, color);
            Some(bat_x)
        }
        _ => None,
    };

    // Tray: StatusNotifier icons, drawn left of the battery/chips cluster and
    // sized to the bar height. Decode is lazy + cached inside the tray.
    let tray_left = match tray {
        Some(t) => {
            let pad = s(3);
            let icon_px = (ph as i32 - pad * 2).clamp(8, 64) as u16;
            t.ensure_icons(icon_px);
            let gap = s(6);
            let right_anchor = battery_left
                .or(cap_chip_left)
                .or(auto_chip_left)
                .unwrap_or(clock_x);
            let tray_items: Vec<(usize, &icons::Icon)> = t.visible_items().collect();
            if tray_items.is_empty() {
                state.tray_icon_rects.clear();
                None
            } else {
                // Right-to-left so the cluster hugs the battery/clock. Capture
                // each icon's logical rect + item index so the pointer handler
                // can open the matching menu on click.
                let mut rects: Vec<(i32, i32, usize)> = Vec::with_capacity(tray_items.len());
                let mut x = right_anchor - gap;
                for (idx, icon) in tray_items.iter().rev() {
                    x -= icon.width as i32;
                    let y = (ph as i32 - icon.height as i32) / 2;
                    blit_icon(&mut mmap, pw, ph, x, y, icon);
                    let lx = ((x as f64) / scale).round() as i32;
                    let lw = ((icon.width as f64) / scale).round() as i32;
                    rects.push((lx, lw, *idx));
                    x -= gap;
                }
                state.tray_icon_rects = rects;
                Some(x)
            }
        }
        None => {
            state.tray_icon_rects.clear();
            None
        }
    };

    // Far left: workspace cluster. Active box in accent color, the rest
    // dimmed. Suppressed entirely when `cfg.show_workspaces` is false,
    // in which case the window list slides to the left edge.
    let list_start_x = if state.cfg.show_workspaces {
        let ws_box_w = s(WS_BOX_W);
        let ws_box_h = s(WS_BOX_H);
        let ws_gap = s(WS_GAP);
        let ws_cluster_w =
            state.workspace_count as i32 * ws_box_w + (state.workspace_count as i32 - 1) * ws_gap;
        let ws_y = (ph as i32 - ws_box_h) / 2;
        for i in 0..state.workspace_count {
            let bx = s(PADDING_X) + i as i32 * (ws_box_w + ws_gap);
            let color = if i + 1 == state.active_workspace {
                ACCENT
            } else {
                DIM
            };
            fill_rect(&mut mmap, pw, ph, bx, ws_y, ws_box_w, ws_box_h, color);
        }
        // Active workspace's display name, drawn immediately right of
        // the box cluster. Skipped when the user hasn't named the
        // active slot — boxes alone are enough in that case.
        // WS_LIST_GAP separates the name from the window list.
        let mut after_ws_x = s(PADDING_X) + ws_cluster_w;
        let active_name = state
            .workspace_names
            .get(state.active_workspace.saturating_sub(1) as usize)
            .map(|s| s.as_str())
            .unwrap_or("");
        if !active_name.is_empty() {
            let name_x = after_ws_x + s(8);
            let name_w = measure_text(&state.font, font_px, active_name);
            draw_text(
                &mut mmap,
                pw,
                ph,
                &state.font,
                font_px,
                name_x,
                active_name,
                fg,
            );
            after_ws_x = name_x + name_w;
        }
        after_ws_x + s(WS_LIST_GAP)
    } else {
        s(PADDING_X)
    };

    // Middle: window list, drawn per-entry so we can paint a background
    // accent behind the focused entry. Entries are deterministically
    // ordered by FT identifier (stable across renders). When no window
    // exists on the active workspace the slot stays empty — `--version`
    // is the supported channel for reading the build version.
    let list_end_x = tray_left
        .or(battery_left)
        .or(cap_chip_left)
        .or(auto_chip_left)
        .unwrap_or(clock_x)
        - s(PADDING_X);
    let list_budget = (list_end_x - list_start_x).max(0);
    // draw_window_list also fills `entry_rects`, which the pointer
    // button handler reads on the next click to hit-test entries.
    let mut entry_rects: Vec<(i32, i32, String)> = Vec::new();
    draw_window_list(
        &mut mmap,
        pw,
        ph,
        scale,
        state,
        list_start_x,
        list_budget,
        font_px,
        fg,
        &mut entry_rects,
    );
    // Stored hit-test rects must be logical: pointer events arrive in logical
    // surface coords, so divide the physical x/width back down by `scale`.
    for r in &mut entry_rects {
        r.0 = ((r.0 as f64) / scale).round() as i32;
        r.1 = ((r.1 as f64) / scale).round() as i32;
    }
    state.window_entry_rects = entry_rects;

    // Flush our writes and unmap before committing; the compositor reads
    // the pixels through its own dup of the pool fd, not our mapping.
    drop(mmap);

    let buffer = &state.buffer.as_ref().expect("buffer ensured above").buffer;
    let surface = state.surface.as_ref().expect("surface missing in redraw");
    surface.attach(Some(buffer), 0, 0);
    // Map the physical buffer back onto the logical surface size.
    if let Some(vp) = state.viewport.as_ref() {
        vp.set_destination(w as i32, h as i32);
    }
    surface.damage_buffer(0, 0, pw as i32, ph as i32);
    surface.commit();
    Ok(())
}

/// Total horizontal advance for rendering `text` at `size_px`. Doesn't
/// account for kerning, but neither does our renderer — values match.
pub(crate) fn measure_text(font: &Font, size_px: f32, text: &str) -> i32 {
    let mut w = 0.0_f32;
    for ch in text.chars() {
        w += font.metrics(ch, size_px).advance_width;
    }
    w.ceil() as i32
}

/// Local-time clock formatted via `libc::strftime`. `format` is a
/// strftime(3) pattern (aliases like `iso` / `24h-short` are pre-expanded
/// at config-load time). Uses libc to avoid pulling in chrono/time —
/// already in our tree via tempfile, so this is zero added deps.
fn format_clock_now(format: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r is thread-safe and reentrant; the version of glibc we
    // care about supports it unconditionally.
    unsafe { libc::localtime_r(&secs, &mut tm) };
    // strftime needs a NUL-terminated format. An embedded NUL in the
    // user's config is a config bug; surface a recognizable placeholder
    // rather than panicking out of the render loop.
    let Ok(fmt) = CString::new(format) else {
        return "[bad clock format]".into();
    };
    let mut buf = [0u8; 256];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            &tm,
        )
    };
    // n == 0 means either the result was empty (legitimate, e.g. empty
    // format string) OR the buffer was too small. 256 bytes is plenty for
    // any reasonable clock format, so we treat both cases the same.
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Draw the window list entries, painting an accent backdrop behind the
/// focused entry. Truncates the visible set with ".." if the cumulative
/// width exceeds `budget_px`. The focused entry, when present, is drawn
/// first in width-budget order so it always survives truncation.
#[allow(clippy::too_many_arguments)]
fn draw_window_list(
    mmap: &mut MmapMut,
    w: u32,
    h: u32,
    scale: f64,
    state: &State,
    start_x: i32,
    budget_px: i32,
    font_px: f32,
    fg: u32,
    entry_rects: &mut Vec<(i32, i32, String)>,
) {
    let s = |v: i32| ((v as f64) * scale).round() as i32;
    let entry_gap = s(ENTRY_GAP);
    let font = &state.font;
    // Filter to windows on the active workspace. Entries without a
    // mapping yet (announced via ext-FT before the matching
    // `window_opened` IPC arrived) stay hidden until the workspace is
    // known — the two arrive close enough in practice that the gap
    // is invisible.
    let mut entries: Vec<&Toplevel> = state
        .toplevels
        .values()
        .filter(|t| {
            state.window_workspaces.get(&t.identifier).copied() == Some(state.active_workspace)
        })
        .collect();
    entries.sort_by(|a, b| a.identifier.cmp(&b.identifier));

    let label_of = |t: &Toplevel| -> String {
        if !t.title.is_empty() {
            t.title.clone()
        } else if !t.app_id.is_empty() {
            t.app_id.clone()
        } else {
            "(untitled)".to_string()
        }
    };

    // First pass: compute widths and decide which entries fit. `id` is
    // `Some` for real entries (clickable) and `None` for the truncation
    // ".." marker (not clickable, no FT identifier to route a click to).
    let mut visible: Vec<(String, i32, bool, Option<String>)> = Vec::new();
    let mut used = 0;
    for t in &entries {
        let label = label_of(t);
        let lw = measure_text(font, font_px, &label);
        let needed = if visible.is_empty() {
            lw
        } else {
            lw + entry_gap
        };
        if used + needed > budget_px {
            // Out of room; signal truncation with a trailing ".." if it fits.
            let ellipsis_w = measure_text(font, font_px, "..");
            let ellipsis_needed = if visible.is_empty() {
                ellipsis_w
            } else {
                ellipsis_w + entry_gap
            };
            if used + ellipsis_needed <= budget_px {
                visible.push(("..".into(), ellipsis_w, false, None));
            }
            break;
        }
        let focused = state.focused_id.as_deref() == Some(t.identifier.as_str());
        visible.push((label, lw, focused, Some(t.identifier.clone())));
        used += needed;
    }

    let mut x = start_x;
    for (i, (label, lw, focused, id)) in visible.iter().enumerate() {
        if i > 0 {
            x += entry_gap;
        }
        // Every real entry (not the ".." overflow marker) gets a chip
        // background so entries are visually separated. Focused = accent,
        // unfocused = dim dark bg.
        if id.is_some() {
            let pad = s(4);
            let chip_color = if *focused { ACCENT } else { ENTRY_BG };
            fill_rect(
                mmap,
                w,
                h,
                x - pad,
                s(2),
                lw + pad * 2,
                h as i32 - s(4),
                chip_color,
            );
        }
        draw_text(mmap, w, h, font, font_px, x, label, fg);
        if let Some(id) = id {
            entry_rects.push((x, *lw, id.clone()));
        }
        x += lw;
    }
}

/// Solid-color rectangle (no alpha blending). Clamped to the buffer.
pub(crate) fn fill_rect(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) {
    let bytes = color.to_ne_bytes();
    let stride = dst_w as usize * 4;
    let x0 = x.max(0) as usize;
    let x1 = (x + w).min(dst_w as i32).max(0) as usize;
    let y0 = y.max(0) as usize;
    let y1 = (y + h).min(dst_h as i32).max(0) as usize;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let row: Vec<u8> = bytes.repeat(x1 - x0);
    for yy in y0..y1 {
        let off = yy * stride + x0 * 4;
        mmap[off..off + row.len()].copy_from_slice(&row);
    }
}

pub(crate) fn fill_bg(mmap: &mut MmapMut, w: u32, h: u32, color: u32) {
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

/// Blit a premultiplied-BGRA icon onto the buffer, source-over an opaque
/// background (`out = src + dst*(1 - a)`). Clipped to the buffer.
fn blit_icon(
    mmap: &mut MmapMut,
    dst_w: u32,
    dst_h: u32,
    dst_x: i32,
    dst_y: i32,
    icon: &icons::Icon,
) {
    let (iw, ih) = (icon.width as i32, icon.height as i32);
    let stride = dst_w as usize * 4;
    for sy in 0..ih {
        let dy = dst_y + sy;
        if dy < 0 || dy >= dst_h as i32 {
            continue;
        }
        for sx in 0..iw {
            let dx = dst_x + sx;
            if dx < 0 || dx >= dst_w as i32 {
                continue;
            }
            let si = ((sy * iw + sx) * 4) as usize;
            let (sb, sg, sr, sa) = (
                icon.bgra[si],
                icon.bgra[si + 1],
                icon.bgra[si + 2],
                icon.bgra[si + 3],
            );
            if sa == 0 {
                continue;
            }
            let off = dy as usize * stride + dx as usize * 4;
            if sa == 255 {
                mmap[off] = sb;
                mmap[off + 1] = sg;
                mmap[off + 2] = sr;
                mmap[off + 3] = 255;
            } else {
                let inv = 255 - sa as u32;
                mmap[off] = (sb as u32 + (mmap[off] as u32 * inv + 127) / 255) as u8;
                mmap[off + 1] = (sg as u32 + (mmap[off + 1] as u32 * inv + 127) / 255) as u8;
                mmap[off + 2] = (sr as u32 + (mmap[off + 2] as u32 * inv + 127) / 255) as u8;
                mmap[off + 3] = 255;
            }
        }
    }
}

/// Blit a single-channel coverage bitmap as `color`, alpha-blending against
/// whatever ARGB is already in the destination.
pub(crate) fn blit_alpha(
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
        // Route strictly by proxy identity. A configure/closed can arrive for a
        // menu surface we just destroyed; acking a dead object is a protocol
        // error that severs the whole bar, so events for neither the live bar
        // nor the current menu are ignored (NOT acked).
        let is_bar = state.layer_surface.as_ref() == Some(layer_surface);
        let is_menu = state
            .menu
            .as_ref()
            .is_some_and(|m| &m.layer_surface == layer_surface);
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                if is_menu {
                    layer_surface.ack_configure(serial);
                    if let Some(m) = state.menu.as_mut() {
                        let (ww, wh) = m.want_size();
                        let w = if width == 0 { ww } else { width };
                        let h = if height == 0 { wh } else { height };
                        m.set_size(w, h);
                    }
                    state.menu_dirty = true;
                } else if is_bar {
                    layer_surface.ack_configure(serial);
                    let w = if width == 0 { 1 } else { width };
                    let h = if height == 0 {
                        state.cfg.height
                    } else {
                        height
                    };
                    state.size = Some((w, h));
                    state.dirty = true;
                }
                // else: stale surface (destroyed menu) — ignore, don't ack.
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if is_menu {
                    if let Some(m) = state.menu.take() {
                        m.destroy();
                    }
                } else if is_bar {
                    tracing::info!("layer surface closed by compositor; exiting");
                    state.running = false;
                }
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
    // Request-only HiDPI globals — they never send events.
    WpViewporter,
    WpViewport,
    WpFractionalScaleManagerV1,
);

impl Dispatch<WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: <WpFractionalScaleV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            // `scale` is the ratio in 1/120ths (e.g. 180 == 1.5x).
            let s = scale as f64 / 120.0;
            if state.scale != s {
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
            let has_pointer = caps.contains(Capability::Pointer);
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
                tracing::debug!("seat advertised pointer; bound");
            } else if !has_pointer {
                if let Some(p) = state.pointer.take() {
                    p.release();
                    state.pointer_x = None;
                    tracing::debug!("seat dropped pointer capability");
                }
            }

            // Keyboard: bound only so the tray menu can receive key nav and,
            // crucially, a `Leave` when focus moves away (= click-elsewhere
            // dismissal). The bar itself never requests keyboard focus.
            let has_kbd = caps.contains(Capability::Keyboard);
            if has_kbd && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
                tracing::debug!("seat advertised keyboard; bound");
            } else if !has_kbd {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                    tracing::debug!("seat dropped keyboard capability");
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
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = Some(surface_x);
                state.pointer_y = Some(surface_y);
                state.pointer_on_menu = state.menu.as_ref().is_some_and(|m| m.owns(&surface));
                if state.pointer_on_menu {
                    if let Some(m) = state.menu.as_mut() {
                        if m.hover_at(surface_y) {
                            state.menu_dirty = true;
                        }
                    }
                }
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = Some(surface_x);
                state.pointer_y = Some(surface_y);
                if state.pointer_on_menu {
                    if let Some(m) = state.menu.as_mut() {
                        if m.hover_at(surface_y) {
                            state.menu_dirty = true;
                        }
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_x = None;
                state.pointer_y = None;
                state.pointer_on_menu = false;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Pressed),
                ..
            } => {
                // BTN_LEFT == 0x110 (Linux input event code).
                const BTN_LEFT: u32 = 0x110;
                if button != BTN_LEFT {
                    return;
                }

                // Click inside the open tray menu: resolve the hovered row.
                if state.pointer_on_menu {
                    let action = state.menu.as_ref().map(|m| m.activate_hover());
                    if let Some(action) = action {
                        apply_menu_action(state, action);
                    }
                    return;
                }

                let Some(x) = state.pointer_x else {
                    return;
                };
                let xi = x.floor() as i32;

                // Tray icons sit on the right; a hit opens that item's menu.
                if let Some((_, _, idx)) = state
                    .tray_icon_rects
                    .iter()
                    .find(|(rx, rw, _)| xi >= *rx && xi < *rx + *rw)
                    .copied()
                {
                    // Clicking the icon whose menu is already open toggles it
                    // shut; otherwise (re)open for this item.
                    if state.menu.as_ref().is_some_and(|m| m.tray_idx == idx) {
                        if let Some(m) = state.menu.take() {
                            m.destroy();
                        }
                    } else {
                        state.pending_open_menu = Some(idx);
                    }
                    return;
                }

                // Otherwise hit-test the window list (left/middle) → focus.
                let hit = state
                    .window_entry_rects
                    .iter()
                    .find(|(rx, rw, _)| xi >= *rx && xi < *rx + *rw)
                    .map(|(_, _, id)| id.clone());
                if let Some(id) = hit {
                    if let Err(e) = ipc_client::request_focus_window(&id) {
                        tracing::warn!(error = ?e, %id, "focus_window ipc failed");
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _kbd: &WlKeyboard,
        event: <WlKeyboard as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(KeyState::Pressed),
                ..
            } => {
                if let Some(action) = state.menu.as_mut().map(|m| m.key(key)) {
                    apply_menu_action(state, action);
                    state.menu_dirty = true;
                }
            }
            // Focus moved off the menu (the user clicked elsewhere): dismiss.
            // The guard ensures a stale Leave for an already-replaced menu
            // surface can't tear down the current one.
            wl_keyboard::Event::Leave { surface, .. }
                if state.menu.as_ref().is_some_and(|m| m.owns(&surface)) =>
            {
                if let Some(m) = state.menu.take() {
                    m.destroy();
                }
            }
            _ => {}
        }
    }
}

/// Apply a resolved menu [`Action`]. UI-only variants mutate `State.menu`
/// directly; `Activate` is deferred to the loop (it needs the D-Bus conn).
fn apply_menu_action(state: &mut State, action: menu::Action) {
    match action {
        menu::Action::None => {}
        menu::Action::Close => {
            if let Some(m) = state.menu.take() {
                m.destroy();
            }
        }
        menu::Action::Activate(id) => {
            state.pending_menu_click = Some(id);
        }
        menu::Action::NavInto(id) => {
            let fs = state.cfg.font_size;
            if let Some(m) = state.menu.as_mut() {
                m.nav_into(id);
                m.renavigate(&state.font, fs);
            }
            state.menu_dirty = true;
        }
        menu::Action::NavBack => {
            let fs = state.cfg.font_size;
            let mut close = false;
            if let Some(m) = state.menu.as_mut() {
                if m.nav_back() {
                    m.renavigate(&state.font, fs);
                } else {
                    close = true;
                }
            }
            if close {
                if let Some(m) = state.menu.take() {
                    m.destroy();
                }
            }
            state.menu_dirty = true;
        }
    }
}

/// Render the open tray menu into its buffer. Split-borrow over `State`:
/// `state.menu` (mut) and the read-only draw inputs are disjoint fields.
fn render_menu(state: &mut State, qh: &QueueHandle<State>) {
    let scale = state.scale;
    let font_size = state.cfg.font_size;
    let bg = state.cfg.background;
    let fg = state.cfg.foreground;
    let shm = state.shm.clone();
    if let Some(menu) = state.menu.as_mut() {
        if let Err(e) = menu.render(qh, &shm, &state.font, scale, font_size, bg, fg) {
            tracing::warn!(error = ?e, "tray menu render failed");
        }
    }
}

/// Fetch + open the context menu for tray item `idx` (or fall back to an
/// `Activate` for menu-less items). Runs in the loop where `tray` is in scope.
fn open_tray_menu(
    state: &mut State,
    qh: &QueueHandle<State>,
    tray: Option<&mut tray::Tray>,
    idx: usize,
) {
    if let Some(m) = state.menu.take() {
        m.destroy();
    }
    let Some(t) = tray else {
        return;
    };
    let anchor_x = state
        .tray_icon_rects
        .iter()
        .find(|(_, _, i)| *i == idx)
        .map(|(x, _, _)| *x)
        .unwrap_or(0);
    let (output_w, bar_h) = state.size.unwrap_or((0, state.cfg.height));

    if t.has_menu(idx) {
        if let Some(root) = t.fetch_menu(idx) {
            let m = menu::Menu::open(
                &state.compositor,
                &state.layer_shell,
                state.viewporter.as_ref(),
                qh,
                root,
                idx,
                anchor_x,
                output_w,
                bar_h,
                state.cfg.position,
                &state.font,
                state.cfg.font_size,
            );
            tracing::debug!(idx, anchor_x, "tray menu opened");
            state.menu = Some(m);
            state.menu_dirty = true;
            return;
        }
    }
    // No usable dbusmenu — give the item an Activate so left-click does
    // *something* (some items toggle visibility / raise a window).
    t.activate(idx, 0, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strftime_short_24h_produces_hhmm() {
        let s = format_clock_now("%H:%M");
        // Five characters, ASCII digits separated by ':'.
        assert_eq!(s.len(), 5, "expected HH:MM, got {s:?}");
        let bytes = s.as_bytes();
        assert!(bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit());
        assert_eq!(bytes[2], b':');
        assert!(bytes[3].is_ascii_digit() && bytes[4].is_ascii_digit());
    }

    #[test]
    fn strftime_iso_includes_dashes_and_colons() {
        let s = format_clock_now("%Y-%m-%d %H:%M:%S");
        assert_eq!(s.len(), 19, "expected YYYY-MM-DD HH:MM:SS, got {s:?}");
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        assert_eq!(s.as_bytes()[13], b':');
    }

    #[test]
    fn strftime_handles_embedded_nul_without_panicking() {
        let s = format_clock_now("%H\0:%M");
        assert_eq!(s, "[bad clock format]");
    }
}
