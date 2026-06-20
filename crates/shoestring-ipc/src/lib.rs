//! Wire types for the shoestring-wm IPC protocol.
//!
//! Types-only crate: depends on serde + serde_json so a future companion bar
//! can link them without pulling in Smithay or a runtime.
//!
//! ## Protocol
//!
//! Newline-delimited JSON over a `SOCK_STREAM` unix socket. The socket path
//! defaults to `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`; the WM
//! exports it as `$SHOESTRING_WM_SOCKET` so children can find it without
//! knowing the formula.
//!
//! A client opens the socket, sends exactly one [`Request`] (one JSON
//! object terminated by `\n`), then reads the [`Response`]. For
//! [`Request::EventStream`], the server keeps the connection open and
//! pushes [`Event`]s forever (one JSON line per event) until the client
//! disconnects.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Environment variable the WM exports so children (notably `shoestring-ctl`
/// and the bar) can find the socket without re-deriving the path formula.
pub const SOCKET_ENV: &str = "SHOESTRING_WM_SOCKET";

/// Resolve the conventional socket path:
/// `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`. Returns `None`
/// if either env var is unset — callers should prefer `$SHOESTRING_WM_SOCKET`
/// when present.
pub fn default_socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")?;
    let display = display.to_string_lossy();
    Some(PathBuf::from(runtime).join(format!("shoestring-wm-{display}.sock")))
}

/// Resolve the socket path for a *client*: prefer `$SHOESTRING_WM_SOCKET`,
/// fall back to [`default_socket_path`].
pub fn client_socket_path() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .or_else(default_socket_path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// List workspaces and which one is currently active.
    Workspaces,
    /// List every mapped window (across all workspaces).
    Windows,
    /// List every connected output.
    Outputs,
    /// Snapshot the full window tree: every output with its logical
    /// placement, plus each workspace and the windows on it (with geometry,
    /// stacking order, and the output each window sits on). The
    /// `swaymsg -t get_tree` analogue — the canonical query for layout
    /// scripting. Reply is [`Response::Tree`]. Read-only and not gated by
    /// automation (pure observation, like [`Request::Windows`]).
    GetTree,
    /// Switch the connection into streaming mode: server sends one
    /// [`Response::Ok`] then a stream of [`Event`]s, one per line, forever.
    EventStream,
    /// Synthesize a keypress (press + release) targeting whichever surface
    /// currently holds keyboard focus. `keysym` is an X keysym name as
    /// understood by `xkb_keysym_from_name` (e.g. `"Return"`, `"F5"`,
    /// `"q"`, `"BackSpace"`).
    ///
    /// When `modifiers` is non-empty each modifier is pressed before the
    /// main keysym and released after (in reverse order) — the focused
    /// surface sees the key arrive with the right modifier mask, the same
    /// way `xdotool key super+shift+q` works. Names are case-insensitive
    /// and follow the WM's keybind alias table: `super` / `logo` / `mod4`
    /// / `win`, `ctrl` / `control`, `alt` / `mod1`, `shift`. Anything
    /// else is passed straight to `xkb_keysym_from_name`, so raw names
    /// like `Hyper_L` or `Mode_switch` work too.
    ///
    /// Chords that the WM itself consumes (e.g. `Super+Shift+Q`) won't
    /// reach the focused surface — use [`Request::DispatchAction`] for
    /// those.
    InjectKey {
        keysym: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        modifiers: Vec<String>,
    },
    /// Synthesize a sequence of keypresses that types `text`. v1 supports
    /// ASCII letters, digits, and space; other codepoints fall back to a
    /// server-side error so the caller knows to break the input up.
    InjectText { text: String },
    /// Synthesize a single mouse click. `button` is one of `"left"`,
    /// `"right"`, `"middle"`, or a numeric Linux `BTN_*` code as a string
    /// (`"272"` etc). Optional `x` / `y` move the pointer to the given
    /// compositor-space coordinates first (both must be present together).
    InjectClick {
        button: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
    },
    /// Move the pointer to compositor-space `(x, y)` without clicking.
    /// Parity with `xdotool mousemove`; useful for hover-only tests and
    /// for setting up a drag (move → press → move → release). Does not
    /// change keyboard focus — pointer focus follows the motion the same
    /// way it would for a real `wl_pointer.motion` event. Gated by
    /// `set_automation`. Reply is [`Response::Ok`].
    MoveMouse { x: f64, y: f64 },
    /// Read the current pointer location in compositor-space logical
    /// coords. Reply is [`Response::PointerPosition`]. Read-only and not
    /// gated by automation — pure observation, useful for verifying that
    /// an earlier `move_mouse` / `inject_click` landed where it should.
    PointerPosition,
    /// Lock the session. Spawns the WM's configured lock binary
    /// (`general.lock_command`); the binary itself drives the
    /// `ext-session-lock-v1` handshake. Returns immediately with
    /// [`Response::Ok`] — no wait for the lock to confirm.
    Lock,
    /// Quit the session — the same path as the `Action::Quit` keybind, so it
    /// pops the `confirm_action` dialog (`shoestring-confirm`) and only exits
    /// the WM on the user's *Yes*. Reply is [`Response::Ok`] (the dialog has
    /// been shown, not that the user accepted). **Ungated** despite being
    /// destructive: the human confirmation at the dialog is the authorization,
    /// so a socket client can at worst pop a dialog the user dismisses. Lets a
    /// mouse-driven UI (the bar control menu) offer log-out without requiring
    /// the automation gate that `dispatch_action {"type":"quit"}` needs.
    Quit,
    /// Toggle the runtime automation gate (see
    /// `general.automation_enabled`). Reply is [`Response::Automation`]
    /// with the new state; an [`Event::AutomationChanged`] is broadcast
    /// to subscribers when the value actually flips. Not persisted to
    /// disk — the config file is the source of truth at next start.
    ///
    /// Automation is a superset of screen capture: flipping this gate also
    /// flips the screen-capture gate the same way (an agent that can drive
    /// the session can also observe it — the `Screenshot` request needs the
    /// capture global). So enabling automation alone is enough to
    /// screenshot, and disabling it returns the session to no-capture. When
    /// the capture gate follows, an [`Event::ScreenCaptureChanged`] is
    /// broadcast too.
    SetAutomation { enabled: bool },
    /// Read the current automation gate state without changing it. Reply
    /// is [`Response::Automation`].
    AutomationStatus,
    /// Toggle the runtime screen-capture gate (see
    /// `general.screen_capture_enabled`). When enabled the WM advertises the
    /// `zwlr_screencopy_manager_v1` global so capture tools (OBS, grim, the
    /// portal screencast backend) can read the screen; when disabled the
    /// global is withdrawn and any capture is refused. Reply is
    /// [`Response::ScreenCapture`] with the new state; an
    /// [`Event::ScreenCaptureChanged`] is broadcast when the value actually
    /// flips. Not persisted to disk — the config file is the source of truth
    /// at next start.
    SetScreenCapture { enabled: bool },
    /// Read the current screen-capture gate state without changing it. Reply
    /// is [`Response::ScreenCapture`].
    ScreenCaptureStatus,
    /// Capture a PNG screenshot via the WM's wlr-screencopy server. The
    /// WM spawns `shoestring-screenshot` on the user's behalf and replies
    /// with the resulting [`Response::Screenshot`] once the file is
    /// written.
    ///
    /// - `output: None` → first advertised output (full-screen).
    /// - `output: Some(name)` → capture that output. Required when
    ///   `region` is set, since region coordinates are output-relative.
    /// - `region: Some(...)` → capture only that rectangle in the named
    ///   output's logical coords.
    ///
    /// Gated by `set_automation`: returns [`Response::Error`] when the
    /// runtime automation gate is off.
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<ScreenshotRegion>,
    },
    /// Spawn a child process under the WM's environment (inherits
    /// `WAYLAND_DISPLAY`, `SHOESTRING_WM_SOCKET`, etc.) and return its
    /// captured output once it exits. `argv[0]` is the executable; the
    /// remainder are arguments. `argv` must be non-empty.
    ///
    /// `timeout_ms`: if set, the child is sent `SIGKILL` after this many
    /// milliseconds. The reply still includes whatever output was
    /// captured up to that point.
    ///
    /// Output is capped at [`RUN_COMMAND_OUTPUT_CAP`] bytes per stream;
    /// further bytes are drained from the pipe (so the child does not
    /// block on a full pipe buffer) but discarded, and the response's
    /// `truncated` field is set.
    ///
    /// Gated by `set_automation`: returns [`Response::Error`] when the
    /// runtime automation gate is off.
    RunCommand {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u32>,
    },
    /// Re-read the TOML config file the WM was launched with, recompile
    /// the binding table, and swap both. Equivalent to the
    /// `Action::ReloadConfig` keybind path and the filesystem watcher's
    /// trigger; broadcasts [`Event::ConfigReloaded`] on success. Reply
    /// is [`Response::Ok`] or [`Response::Error`] (no config loaded /
    /// parse failure / I/O failure).
    ReloadConfig,
    /// Enter interactive window-picker mode: the WM waits for the user's
    /// next click and replies with [`Response::PickedWindow`] carrying
    /// the targeted window (or `None` if the user cancels with Escape
    /// or right-click, or clicks somewhere that isn't a toplevel).
    ///
    /// Only one picker may be active at a time; a concurrent request
    /// returns [`Response::Error`]. If the client disconnects while
    /// the picker is up, the picker is cancelled.
    ///
    /// Not gated by `set_automation` — the reply is contingent on a
    /// physical user click, so this is not a remote-control surface.
    PickWindow,
    /// Close the toplevel matching `id` (an
    /// `ext-foreign-toplevel-list-v1` identifier, the same one carried
    /// by [`WindowSummary::id`]). Sends `xdg_toplevel.close` to the
    /// client; the client decides whether to honor it (a typical app
    /// surfaces a save-prompt before exiting). Reply is
    /// [`Response::Ok`] on a successful send, [`Response::Error`] if
    /// no window matches.
    CloseWindow { id: String },
    /// Focus the toplevel matching `id`. The WM unminimizes the window
    /// if needed, switches to its workspace if it lives elsewhere, and
    /// then takes the same focus/raise/activate path as a click. Reply
    /// is [`Response::Ok`] on success, [`Response::Error`] if no
    /// window matches.
    FocusWindow { id: String },
    /// Raise the toplevel matching `id` to the top of the stacking order
    /// without changing keyboard focus or switching workspaces (a pure
    /// restack — contrast [`Request::FocusWindow`], which raises *and*
    /// focuses). No-op if the window is minimized or on a non-active
    /// workspace (it isn't in the mapped stack). Emits
    /// [`Event::WindowRestacked`]. Reply is [`Response::Ok`] on success,
    /// [`Response::Error`] if no window matches. Not gated by automation
    /// (a benign window-management tweak, like [`Request::FocusWindow`]).
    RaiseWindow { id: String },
    /// Lower the toplevel matching `id` to the bottom of the stacking order.
    /// The complement of [`Request::RaiseWindow`]; same focus/gating/no-op
    /// semantics.
    LowerWindow { id: String },
    /// Set or clear the sticky flag (show on all workspaces) on the toplevel
    /// matching `id`. Broadcasts a [`Event::WindowStickyChanged`]. Reply is
    /// [`Response::Ok`] on success, [`Response::Error`] if no window matches.
    /// Not gated by automation (a benign window-management tweak, like
    /// [`Request::FocusWindow`]).
    SetWindowSticky { id: String, sticky: bool },
    /// Set or clear the always-on-top flag on the toplevel matching `id`.
    /// Broadcasts a [`Event::WindowAlwaysOnTopChanged`]. Reply is
    /// [`Response::Ok`] on success, [`Response::Error`] if no window matches.
    /// Not gated by automation (a benign window-management tweak, like
    /// [`Request::FocusWindow`]).
    SetWindowAlwaysOnTop { id: String, always_on_top: bool },
    /// Override the display name of the toplevel matching `id` (an
    /// `ext-foreign-toplevel-list-v1` identifier, as carried by
    /// [`WindowSummary::id`]). The override takes precedence over the
    /// client's `xdg_toplevel` title everywhere the WM reports a title:
    /// the `windows` / `get_tree` snapshots, the `find_windows` match
    /// surface, and the [`Event::WindowTitleChanged`] stream — so a bar
    /// or window-jump menu shows (and matches on) the override rather
    /// than the client-supplied title.
    ///
    /// An empty `name` clears the override, reverting to the client's
    /// own title. The override never touches `app_id`, and it is *not*
    /// forwarded to wlr-foreign-toplevel-management taskbars (those keep
    /// seeing the raw client title). The override is keyed by the live
    /// window and is dropped when the window closes — it does not persist.
    ///
    /// Setting (or clearing) the name broadcasts an
    /// [`Event::WindowTitleChanged`] carrying the new effective title.
    /// Reply is [`Response::Ok`] on success, [`Response::Error`] if no
    /// window matches. Not gated by automation (a benign display tweak,
    /// like [`Request::FocusWindow`]).
    SetWindowName { id: String, name: String },
    /// Run a named bind `Action` server-side, exactly as if a keybind
    /// had fired. Unlike [`Request::InjectKey`] this does *not* route
    /// through the focused surface — Super+Shift+Q won't fire because
    /// the WM consumes the chord, but `dispatch_action {"type":"quit"}`
    /// will. This is the canonical entry point for driving WM actions
    /// from automation.
    ///
    /// `action` is a `shoestring_config::Action` serialised the same
    /// way `[[bindings]]` entries in the config TOML are (i.e. the
    /// JSON form of the same `#[serde(tag="type", rename_all="kebab-case")]`
    /// enum). Carried as a raw JSON value so this crate doesn't have
    /// to depend on `shoestring-config` and pull its dep tree into
    /// every IPC client.
    ///
    /// Examples:
    /// - `{"type":"quit"}`
    /// - `{"type":"focus-workspace","index":3}`
    /// - `{"type":"spawn","command":"alacritty","args":[]}`
    ///
    /// Gated by `set_automation`: returns [`Response::Error`] when the
    /// runtime automation gate is off. Reply is [`Response::Ok`] on
    /// success, [`Response::Error`] on malformed `action`.
    DispatchAction { action: serde_json::Value },
    /// List every mapped window whose `title` and `app_id` match the
    /// supplied regular expressions. Each filter is independent; an
    /// unset filter matches everything. Both filters are AND-ed:
    /// `{title: Some("foo"), app_id: Some("bar")}` matches only windows
    /// where title matches `foo` *and* app_id matches `bar`.
    ///
    /// Regexes use Rust's `regex` crate syntax (Perl-like, no
    /// backreferences). They are not anchored — `firefox` matches
    /// anywhere in the string; use `^firefox$` to require an exact
    /// match. Reply reuses [`Response::Windows`]; an empty list means
    /// no matches. Malformed regex yields [`Response::Error`].
    FindWindows {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
    },
    /// Snapshot the diagnostics metrics registry — process resource gauges
    /// (open fds, RSS, fd limit), WM counts, and any counters. Reply is
    /// [`Response::Metrics`]. Read-only and *not* gated by automation
    /// (pure observation, like [`Request::Windows`] / [`Request::Outputs`]).
    /// Answered on demand even when `[diagnostics].enabled = false`.
    Metrics,
    /// Subscribe to a stream of [`Event::Metrics`] samples: the server
    /// replies [`Response::Ok`], then pushes one metrics line per sample
    /// until the client disconnects. This is the turn-on/off diagnostics
    /// pipe — subscribe to start, hang up to stop.
    ///
    /// `interval_ms` is the desired push interval; it is clamped *up* to
    /// the WM's `[diagnostics].sample_interval_ms` (v1 can't push faster
    /// than it samples). Omit to push on every sample tick.
    ///
    /// Requires `[diagnostics].enabled = true` (the background sampler is
    /// the source of the stream); returns [`Response::Error`] when
    /// diagnostics are disabled.
    MetricsStream {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_ms: Option<u32>,
    },
    /// Set a per-output gamma ramp from a color temperature, server-side —
    /// the WM computes the ramp and drives the CRTC, so no external
    /// night-light daemon (wlsunset/gammastep) is needed. Shares the same
    /// exclusivity as `zwlr_gamma_control_v1`: this takes over any client
    /// currently controlling the output (that client is `failed`), and a
    /// later client likewise takes over from this.
    ///
    /// - `output: None` → every gamma-capable output; `Some(name)` → just
    ///   that one.
    /// - `temperature` is in Kelvin (1000–25000); ~6500 is neutral, lower is
    ///   warmer.
    /// - `brightness` (0.1–1.0, default 1.0) scales all channels.
    /// - `gamma` (0.1–10.0, default 1.0) is the ramp exponent.
    ///
    /// KMS-only: works on the udev/TTY backend. Not gated by automation (a
    /// benign display tweak, like [`Request::ReloadConfig`]). Reply is
    /// [`Response::Ok`] or [`Response::Error`] (bad range / no capable
    /// output / DRM rejection / non-TTY build).
    SetGamma {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        temperature: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brightness: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gamma: Option<f64>,
    },
    /// Clear the WM's own (IPC-set) gamma and restore the output(s) to their
    /// original ramp. `output: None` clears every output the WM set;
    /// `Some(name)` clears just that one. Leaves wlr-gamma-control *clients*
    /// untouched. Reply is [`Response::Ok`] (with the count) or
    /// [`Response::Error`]. Not gated by automation.
    ResetGamma {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Read the last media-privacy snapshot the WM holds (default-sink mute,
    /// default-source/mic mute, camera-in-use). Reply is [`Response::Media`],
    /// whose `state` is `None` when no monitor (`shoestring-mediad`) has
    /// reported yet — distinct from "reported, all false". Read-only and not
    /// gated by automation (pure observation). Used by the bar at startup to
    /// seed its chips before the first [`Event::MediaChanged`] arrives.
    MediaStatus,
    /// Toggle the *default audio sink* mute. The WM does not mute anything
    /// itself — PipeWire owns that state — so this spawns the
    /// `shoestring-mediad audio-mute <on|off>` oneshot, which flips the real
    /// default-sink mute. The live state then flows back as a
    /// [`Request::ReportMedia`] from the monitor and the WM broadcasts
    /// [`Event::MediaChanged`]. Reply is [`Response::Ok`] once the helper is
    /// spawned (not when the mute actually flips). Ungated, like [`Request::Lock`]
    /// (a benign local-session control surface a bar/keybind drives).
    SetAudioMute { enabled: bool },
    /// Toggle the *default audio source* (microphone) mute, the mic analogue of
    /// [`Request::SetAudioMute`]. Spawns `shoestring-mediad mic-mute <on|off>`.
    /// Honest caveat: this stream-mutes the source (capturing apps get silence);
    /// it does **not** prevent a device open — the same guarantee as a hardware
    /// mic-mute key. Ungated. Reply is [`Response::Ok`].
    SetMicMute { enabled: bool },
    /// Push a fresh media-privacy snapshot to the WM. Sent **by the trusted
    /// `shoestring-mediad` monitor**, not by ordinary clients: it links
    /// libpipewire, watches the real default sink/source mute and active camera
    /// nodes, and reports here whenever they change. The WM caches the snapshot
    /// (answering [`Request::MediaStatus`]) and, if it changed, broadcasts
    /// [`Event::MediaChanged`] to subscribers. Ungated — the socket is the
    /// user's own session socket and the payload is cosmetic (it drives privacy
    /// indicators, not capability). Reply is [`Response::Ok`].
    ReportMedia {
        audio_muted: bool,
        mic_muted: bool,
        camera_active: bool,
    },
}

/// A media-privacy snapshot: the live mute state of the default audio sink and
/// source, plus whether a camera is actively streaming. Reflects PipeWire's
/// truth as observed by `shoestring-mediad` — the WM never authors these, it
/// only caches what the monitor reports (mute can also change via media keys,
/// `pavucontrol`, `wpctl`, or the apps themselves). `camera_active` is
/// status-only: there is no compositor-side camera off-switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaState {
    /// Default audio sink (output) is muted.
    pub audio_muted: bool,
    /// Default audio source (microphone) is muted.
    pub mic_muted: bool,
    /// A camera node is actively being streamed (detected via PipeWire; raw
    /// `/dev/video*` opens that bypass PipeWire are not visible here).
    pub camera_active: bool,
}

/// One sampled metric. `gauge` is an instantaneous reading that can go up
/// or down (open fds, RSS); `counter` is monotonic since WM start (frames
/// rendered, IPC requests). The `kind` tag makes the wire self-describing
/// so consumers can render/aggregate without an out-of-band schema.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricValue {
    Gauge { value: i64 },
    Counter { value: u64 },
}

/// Per-stream byte cap for [`Request::RunCommand`]. Keeps IPC frames
/// bounded — pathological commands (`yes`, accidentally tailed logs)
/// can't OOM the WM or wedge it on JSON serialisation.
pub const RUN_COMMAND_OUTPUT_CAP: usize = 64 * 1024;

/// Rectangle for [`Request::Screenshot`], in the target output's logical
/// pixel coords. All fields are positive; `w` and `h` must be > 0.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRegion {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Generic success acknowledgement (no payload). Sent in response to
    /// [`Request::EventStream`] before the event stream starts.
    Ok,
    Workspaces {
        /// 1-based index of the active workspace.
        active: u8,
        /// Total workspace count. Sourced from `[workspaces].count`
        /// in the WM config; default 16. Range 1..=`MAX_WORKSPACE_COUNT`.
        count: u8,
        /// Display name per workspace in 1-based order, length =
        /// `count`. Empty strings mean "no name set, render the
        /// number". Added in a later build so older clients that
        /// pre-dated names still deserialize; new field, no
        /// `deny_unknown_fields`.
        #[serde(default)]
        names: Vec<String>,
    },
    Windows {
        windows: Vec<WindowSummary>,
    },
    Outputs {
        outputs: Vec<OutputSummary>,
    },
    /// Full window tree for [`Request::GetTree`]: outputs with their logical
    /// placement, every workspace and the windows on it, plus any minimized
    /// windows (which belong to no workspace — minimizing drops a window's
    /// workspace assignment, and unminimizing re-assigns it to whatever
    /// workspace is active at restore time). A get_tree-style snapshot for
    /// layout scripting.
    Tree {
        outputs: Vec<OutputNode>,
        workspaces: Vec<WorkspaceNode>,
        /// Minimized (workspace-less) windows. Each has `minimized: true`
        /// and no `geometry` / `output` / `z` (unmapped). Empty when nothing
        /// is minimized.
        minimized: Vec<WindowNode>,
    },
    /// Current state of the automation gate. Returned for both
    /// [`Request::SetAutomation`] and [`Request::AutomationStatus`].
    Automation {
        enabled: bool,
    },
    /// Current state of the screen-capture gate. Returned for both
    /// [`Request::SetScreenCapture`] and [`Request::ScreenCaptureStatus`].
    ScreenCapture {
        enabled: bool,
    },
    /// Path of the PNG written by [`Request::Screenshot`]. Absolute,
    /// usually under `$XDG_PICTURES_DIR`.
    Screenshot {
        path: String,
    },
    /// Result of [`Request::RunCommand`]. `exit_code` is the child's
    /// real exit code; `-1` means killed by a signal (typically
    /// `SIGKILL` from the timeout path). `truncated` is true if either
    /// stdout or stderr exceeded [`RUN_COMMAND_OUTPUT_CAP`].
    CommandResult {
        exit_code: i32,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    /// Result of [`Request::PickWindow`]. `window` is `Some` when the
    /// user clicked a toplevel, `None` if they cancelled (Escape /
    /// right-click) or clicked something that wasn't a toplevel.
    PickedWindow {
        window: Option<WindowSummary>,
    },
    /// Result of [`Request::PointerPosition`]. Compositor-space logical
    /// coords — same coordinate system that [`Request::MoveMouse`] and
    /// [`Request::InjectClick`] take.
    PointerPosition {
        x: f64,
        y: f64,
    },
    /// Snapshot of the diagnostics registry for [`Request::Metrics`].
    /// `ts_ms` is the sample's wall-clock time (ms since the Unix epoch);
    /// `metrics` maps a dotted metric name (`process.open_fds`,
    /// `wm.windows`, …) to its [`MetricValue`]. A `BTreeMap` so key order
    /// is stable on the wire.
    Metrics {
        ts_ms: u64,
        metrics: BTreeMap<String, MetricValue>,
    },
    /// Media-privacy snapshot for [`Request::MediaStatus`]. `state` is `None`
    /// when no `shoestring-mediad` monitor has reported yet (so the bar shows
    /// no media chips), `Some` once a live snapshot exists.
    Media {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<MediaState>,
    },
    /// Server-side error; the client should print and exit non-zero.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    /// Stable identifier matching `ext-foreign-toplevel-list-v1`'s
    /// `identifier` event so a bar can cross-reference.
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// 1-based workspace.
    pub workspace: u8,
    /// `true` for the currently keyboard-focused window.
    pub focused: bool,
    /// On-screen rectangle in compositor-global logical coords. `None` for
    /// minimized windows (unmapped, so no rect) and windows on a non-active
    /// workspace (only the active workspace is mapped). New field; older
    /// payloads without it deserialize via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<WindowGeometry>,
    /// Stacking order within the currently mapped stack: `0` is bottom-most,
    /// higher is closer to the top. `None` for windows that aren't mapped
    /// (minimized, or on a non-active workspace — only the active workspace
    /// is mapped, so only its windows have a Z position). Matches the `z`
    /// field on [`WindowNode`] in the [`Request::GetTree`] snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<u32>,
    /// `true` when the window is sticky — shown on every workspace. Sticky
    /// windows stay mapped across workspace switches and report the
    /// currently-active workspace. Omitted (defaults to `false`) on older WM
    /// builds and for non-sticky windows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sticky: bool,
    /// `true` when the window is always-on-top — kept above ordinary windows
    /// in the stacking order. Omitted (defaults to `false`) on older WM
    /// builds and for ordinary windows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub always_on_top: bool,
}

/// `skip_serializing_if` helper for `bool` fields that default to `false`,
/// so the common case stays off the wire (matching the `Option::is_none`
/// pattern the optional fields use).
fn is_false(b: &bool) -> bool {
    !*b
}

/// A window's on-screen rectangle in compositor-global logical coords:
/// `(x, y)` is the top-left, `(w, h)` the size. Same coordinate system as
/// [`Request::MoveMouse`] / [`Response::PointerPosition`], so a script can
/// aim the pointer at a window from its geometry directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A window node in the tree returned by [`Request::GetTree`] — one mapped
/// or minimized toplevel with everything a layout script needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowNode {
    /// FT identifier, matching [`WindowSummary::id`].
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// On-screen rectangle in global logical coords; `None` when unmapped
    /// (minimized or on a non-active workspace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<WindowGeometry>,
    /// Name of the output the window's center sits on; `None` when unmapped
    /// or off every output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Stacking order within the currently mapped stack: `0` is bottom-most,
    /// higher is closer to the top. `None` for windows that aren't mapped
    /// (minimized, or on a non-active workspace — only the active workspace
    /// is mapped, so only its windows have a Z position).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z: Option<u32>,
    /// `true` for the currently keyboard-focused window.
    pub focused: bool,
    pub minimized: bool,
    /// `true` when the window is sticky (shown on every workspace). Omitted
    /// (defaults to `false`) for ordinary windows. See [`WindowSummary::sticky`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub sticky: bool,
    /// `true` when the window is always-on-top. Omitted (defaults to `false`)
    /// for ordinary windows. See [`WindowSummary::always_on_top`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub always_on_top: bool,
    /// Tile/maximize state: `floating`, `tiled_left`, `tiled_right`, or
    /// `maximized`.
    pub layout: String,
}

/// A workspace and its windows in the tree returned by [`Request::GetTree`].
/// shoestring-wm workspaces are global (not nested under an output), so they
/// form a flat sibling list. Only workspaces that have windows — plus the
/// active one — are reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceNode {
    /// 1-based index.
    pub index: u8,
    /// Display name, or empty when unset.
    pub name: String,
    /// `true` for the active (mapped/visible) workspace.
    pub focused: bool,
    /// Windows on this workspace. For the active workspace, ordered bottom
    /// to top by stacking order; for others, order is unspecified.
    pub windows: Vec<WindowNode>,
}

/// An output and its logical placement in the tree returned by
/// [`Request::GetTree`]. Unlike [`OutputSummary`] this carries the output's
/// position in the global logical coordinate space, so window rectangles can
/// be related to the output they land on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputNode {
    pub name: String,
    /// Logical rectangle: top-left + size in compositor-global logical coords.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Logical scale, matching [`OutputSummary::scale`].
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Logical scale; matches what's advertised on `wl_output.scale` /
    /// `wp_fractional_scale_v1`.
    pub scale: f64,
    /// Whether variable refresh rate (VRR / adaptive sync) is enabled on this
    /// output. `true` only when the output opted in via config *and* the
    /// connector advertised support. Always `false` under the nested winit
    /// backend, which cannot drive VRR.
    #[serde(default)]
    pub adaptive_sync: bool,
    /// Output orientation as a `wlr-randr`-style name (`"normal"`, `"90"`,
    /// `"180"`, `"270"`, `"flipped"`, `"flipped-90"`, …). `width`/`height`
    /// above are the raw mode dimensions; a `"90"`/`"270"` transform swaps the
    /// logical usable area.
    #[serde(default = "default_transform")]
    pub transform: String,
}

fn default_transform() -> String {
    "normal".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    WorkspaceChanged {
        /// 1-based.
        active: u8,
    },
    WindowOpened {
        id: String,
        title: String,
        app_id: String,
        workspace: u8,
    },
    WindowClosed {
        id: String,
    },
    /// `id` is `None` when no window holds keyboard focus.
    WindowFocused {
        id: Option<String>,
    },
    /// Window's title or app_id changed.
    WindowTitleChanged {
        id: String,
        title: String,
        app_id: String,
    },
    /// Window was reassigned to a different workspace (via the
    /// move-to-workspace keybinds or a `[[window_rules]]` workspace
    /// action). Fires regardless of whether the user's view follows.
    /// `workspace` is 1-based.
    WindowMovedToWorkspace {
        id: String,
        workspace: u8,
    },
    /// Window's stacking order changed — it was raised to the top or lowered
    /// to the bottom (via the `raise`/`lower` actions or the
    /// [`Request::RaiseWindow`] / [`Request::LowerWindow`] requests). A raise
    /// or lower shifts the Z index of every other mapped window too, so this
    /// only names the window that moved; re-query [`Request::Windows`] or
    /// [`Request::GetTree`] for the updated `z` of the whole stack.
    WindowRestacked {
        id: String,
    },
    /// Window's sticky flag changed — it was pinned to (or released from)
    /// "show on all workspaces" via the `toggle-sticky` action, a
    /// `[[window_rules]]` `sticky` action, or the [`Request::SetWindowSticky`]
    /// request. See [`WindowSummary::sticky`].
    WindowStickyChanged {
        id: String,
        sticky: bool,
    },
    /// Window's always-on-top flag changed — it was pinned to (or released
    /// from) the always-on-top layer via the `toggle-always-on-top` action, a
    /// `[[window_rules]]` `always_on_top` action, or the
    /// [`Request::SetWindowAlwaysOnTop`] request. See
    /// [`WindowSummary::always_on_top`].
    WindowAlwaysOnTopChanged {
        id: String,
        always_on_top: bool,
    },
    /// A client used `xdg_activation_v1` to ask that one of its surfaces be
    /// focused — typically an app launched by another app (a link opened
    /// from a chat client, a file opened from a file manager) asking to come
    /// to the front. `granted` is the WM's focus-stealing-prevention verdict:
    /// `true` when the request was honored (the window was activated and
    /// focused, so a [`Self::WindowFocused`] event follows), `false` when it
    /// was suppressed because the request wasn't user-driven — focus stayed
    /// put and a bar may flag the window as demanding attention. Emitted only
    /// for a window the WM tracks (one with an `id`).
    WindowActivationRequested {
        id: String,
        granted: bool,
    },
    OutputAdded(OutputSummary),
    OutputRemoved {
        name: String,
    },
    /// Fired when the runtime automation gate flips. Subscribers can use
    /// this to surface a status indicator (e.g. bar widget) without
    /// polling.
    AutomationChanged {
        enabled: bool,
    },
    /// Fired when the runtime screen-capture gate flips. Subscribers (e.g. a
    /// bar widget) surface whether capture is *possible* without polling.
    ScreenCaptureChanged {
        enabled: bool,
    },
    /// Fired when a screen-capture frame is actually delivered to a client —
    /// the live "your screen is being read right now" signal, distinct from
    /// the gate merely being enabled. Rate-limited by the WM (at most a few
    /// per second) so a 30fps cast doesn't flood subscribers; a bar can light
    /// a "recording" dot and let it decay after the events stop. `output` is
    /// the captured output's name.
    ScreenCaptured {
        output: String,
    },
    /// Fired after the WM re-reads its TOML config from disk — either via
    /// the [`Action::ReloadConfig`] keybind path or the file-watcher
    /// triggered on a successful edit. Subscribers can use this to
    /// re-render anything derived from the config (e.g. a bar widget that
    /// mirrors the active keybind set). The event carries no payload; a
    /// subscriber that wants the new state should re-query.
    ConfigReloaded,
    /// One diagnostics sample, pushed to [`Request::MetricsStream`]
    /// subscribers. Same payload shape as [`Response::Metrics`].
    Metrics {
        ts_ms: u64,
        metrics: BTreeMap<String, MetricValue>,
    },
    /// Fired when the media-privacy snapshot changes — the default sink/source
    /// mute or camera-in-use state moved, as reported by `shoestring-mediad`
    /// via [`Request::ReportMedia`]. Subscribers (the bar) update their
    /// MUTE/MIC/CAM indicators without polling. Carries the full snapshot so a
    /// subscriber needs no follow-up [`Request::MediaStatus`].
    MediaChanged {
        audio_muted: bool,
        mic_muted: bool,
        camera_active: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let r = Request::EventStream;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"type":"event_stream"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::EventStream));
    }

    #[test]
    fn response_workspaces_shape() {
        let r = Response::Workspaces {
            active: 3,
            count: 16,
            names: vec![String::new(); 16],
        };
        let s = serde_json::to_string(&r).unwrap();
        // names defaults to empty Vec on older clients; we emit it
        // unconditionally so the wire shape is unambiguous on the WM side.
        assert!(s.starts_with(r#"{"type":"workspaces","active":3,"count":16,"names":["#));
    }

    #[test]
    fn response_workspaces_deserializes_without_names() {
        // Older WM builds didn't emit `names`. The field's `#[serde(default)]`
        // means new client code can read a legacy payload without erroring.
        let legacy = r#"{"type":"workspaces","active":1,"count":8}"#;
        let r: Response = serde_json::from_str(legacy).unwrap();
        match r {
            Response::Workspaces {
                active,
                count,
                names,
            } => {
                assert_eq!(active, 1);
                assert_eq!(count, 8);
                assert!(names.is_empty());
            }
            _ => panic!("expected Workspaces"),
        }
    }

    #[test]
    fn inject_request_shapes() {
        let key = Request::InjectKey {
            keysym: "Return".into(),
            modifiers: vec![],
        };
        // Empty modifiers is skipped on the wire so the no-chord shape
        // is unchanged from the pre-chord protocol.
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            r#"{"type":"inject_key","keysym":"Return"}"#
        );
        // Legacy payloads without `modifiers` still deserialize.
        let legacy: Request =
            serde_json::from_str(r#"{"type":"inject_key","keysym":"Return"}"#).unwrap();
        assert!(matches!(
            legacy,
            Request::InjectKey { ref keysym, ref modifiers }
                if keysym == "Return" && modifiers.is_empty()
        ));
        let chord = Request::InjectKey {
            keysym: "q".into(),
            modifiers: vec!["Super".into(), "Shift".into()],
        };
        assert_eq!(
            serde_json::to_string(&chord).unwrap(),
            r#"{"type":"inject_key","keysym":"q","modifiers":["Super","Shift"]}"#
        );
        let s = serde_json::to_string(&chord).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::InjectKey { ref keysym, ref modifiers }
                if keysym == "q" && modifiers == &["Super", "Shift"]
        ));
        let typ = Request::InjectText { text: "hi".into() };
        assert_eq!(
            serde_json::to_string(&typ).unwrap(),
            r#"{"type":"inject_text","text":"hi"}"#
        );
        let click_no_xy = Request::InjectClick {
            button: "left".into(),
            x: None,
            y: None,
        };
        // x/y are skipped when None so simple cases stay terse.
        assert_eq!(
            serde_json::to_string(&click_no_xy).unwrap(),
            r#"{"type":"inject_click","button":"left"}"#
        );
        let click_xy = Request::InjectClick {
            button: "right".into(),
            x: Some(100.5),
            y: Some(200.0),
        };
        let s = serde_json::to_string(&click_xy).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::InjectClick {
                ref button,
                x: Some(_),
                y: Some(_)
            } if button == "right"
        ));
    }

    #[test]
    fn move_mouse_and_pointer_position_shapes() {
        let mv = Request::MoveMouse { x: 100.0, y: 200.5 };
        assert_eq!(
            serde_json::to_string(&mv).unwrap(),
            r#"{"type":"move_mouse","x":100.0,"y":200.5}"#
        );
        let back: Request =
            serde_json::from_str(r#"{"type":"move_mouse","x":1.0,"y":2.0}"#).unwrap();
        assert!(matches!(back, Request::MoveMouse { x, y } if x == 1.0 && y == 2.0));

        let q = Request::PointerPosition;
        assert_eq!(
            serde_json::to_string(&q).unwrap(),
            r#"{"type":"pointer_position"}"#
        );

        let resp = Response::PointerPosition { x: 10.0, y: 20.0 };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"pointer_position","x":10.0,"y":20.0}"#
        );
    }

    #[test]
    fn automation_request_response_event_shapes() {
        let set = Request::SetAutomation { enabled: true };
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"type":"set_automation","enabled":true}"#
        );
        let status = Request::AutomationStatus;
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"type":"automation_status"}"#
        );
        let resp = Response::Automation { enabled: false };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"automation","enabled":false}"#
        );
        let ev = Event::AutomationChanged { enabled: true };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"automation_changed","enabled":true}"#
        );
    }

    #[test]
    fn screen_capture_request_response_event_shapes() {
        let set = Request::SetScreenCapture { enabled: true };
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"type":"set_screen_capture","enabled":true}"#
        );
        let status = Request::ScreenCaptureStatus;
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"type":"screen_capture_status"}"#
        );
        let resp = Response::ScreenCapture { enabled: false };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"screen_capture","enabled":false}"#
        );
        let ev = Event::ScreenCaptureChanged { enabled: true };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"screen_capture_changed","enabled":true}"#
        );
        let cap = Event::ScreenCaptured {
            output: "eDP-1".into(),
        };
        assert_eq!(
            serde_json::to_string(&cap).unwrap(),
            r#"{"type":"screen_captured","output":"eDP-1"}"#
        );
    }

    #[test]
    fn media_request_response_event_shapes() {
        assert_eq!(
            serde_json::to_string(&Request::MediaStatus).unwrap(),
            r#"{"type":"media_status"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetAudioMute { enabled: true }).unwrap(),
            r#"{"type":"set_audio_mute","enabled":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetMicMute { enabled: false }).unwrap(),
            r#"{"type":"set_mic_mute","enabled":false}"#
        );
        let report = Request::ReportMedia {
            audio_muted: true,
            mic_muted: false,
            camera_active: true,
        };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"type":"report_media","audio_muted":true,"mic_muted":false,"camera_active":true}"#
        );

        // `state: None` is skipped on the wire (no monitor reporting yet).
        assert_eq!(
            serde_json::to_string(&Response::Media { state: None }).unwrap(),
            r#"{"type":"media"}"#
        );
        let some = Response::Media {
            state: Some(MediaState {
                audio_muted: false,
                mic_muted: true,
                camera_active: false,
            }),
        };
        assert_eq!(
            serde_json::to_string(&some).unwrap(),
            r#"{"type":"media","state":{"audio_muted":false,"mic_muted":true,"camera_active":false}}"#
        );

        let ev = Event::MediaChanged {
            audio_muted: true,
            mic_muted: true,
            camera_active: false,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"media_changed","audio_muted":true,"mic_muted":true,"camera_active":false}"#
        );
        // Round-trips back to the same request.
        let back: Request = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::ReportMedia {
                audio_muted: true,
                mic_muted: false,
                camera_active: true
            }
        ));
    }

    #[test]
    fn screenshot_request_shapes() {
        let bare = Request::Screenshot {
            output: None,
            region: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"screenshot"}"#
        );
        let with_region = Request::Screenshot {
            output: Some("eDP-1".into()),
            region: Some(ScreenshotRegion {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            }),
        };
        let s = serde_json::to_string(&with_region).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::Screenshot {
                output: Some(ref n),
                region: Some(ScreenshotRegion {
                    x: 10, y: 20, w: 800, h: 600,
                }),
            } if n == "eDP-1"
        ));
        let resp = Response::Screenshot {
            path: "/tmp/foo.png".into(),
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"screenshot","path":"/tmp/foo.png"}"#
        );
    }

    #[test]
    fn run_command_request_response_shapes() {
        let bare = Request::RunCommand {
            argv: vec!["echo".into(), "hi".into()],
            timeout_ms: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"run_command","argv":["echo","hi"]}"#
        );
        let with_timeout = Request::RunCommand {
            argv: vec!["sleep".into(), "5".into()],
            timeout_ms: Some(250),
        };
        let s = serde_json::to_string(&with_timeout).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::RunCommand {
                ref argv,
                timeout_ms: Some(250),
            } if argv == &["sleep", "5"]
        ));
        let resp = Response::CommandResult {
            exit_code: 0,
            stdout: "hi\n".into(),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"command_result","exit_code":0,"stdout":"hi\n","stderr":"","truncated":false}"#
        );
    }

    #[test]
    fn event_window_moved_to_workspace_shape() {
        let e = Event::WindowMovedToWorkspace {
            id: "abc".into(),
            workspace: 4,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(
            s,
            r#"{"type":"window_moved_to_workspace","id":"abc","workspace":4}"#
        );
    }

    #[test]
    fn reload_config_request_shape() {
        let r = Request::ReloadConfig;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"type":"reload_config"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::ReloadConfig));
    }

    #[test]
    fn event_config_reloaded_shape() {
        let e = Event::ConfigReloaded;
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"type":"config_reloaded"}"#);
    }

    #[test]
    fn metrics_request_shapes() {
        let snap = Request::Metrics;
        assert_eq!(
            serde_json::to_string(&snap).unwrap(),
            r#"{"type":"metrics"}"#
        );
        let back: Request = serde_json::from_str(r#"{"type":"metrics"}"#).unwrap();
        assert!(matches!(back, Request::Metrics));

        // interval_ms is skipped when None.
        let stream_bare = Request::MetricsStream { interval_ms: None };
        assert_eq!(
            serde_json::to_string(&stream_bare).unwrap(),
            r#"{"type":"metrics_stream"}"#
        );
        let stream = Request::MetricsStream {
            interval_ms: Some(2000),
        };
        assert_eq!(
            serde_json::to_string(&stream).unwrap(),
            r#"{"type":"metrics_stream","interval_ms":2000}"#
        );
        let back: Request = serde_json::from_str(r#"{"type":"metrics_stream"}"#).unwrap();
        assert!(matches!(back, Request::MetricsStream { interval_ms: None }));
    }

    #[test]
    fn metric_value_and_metrics_payload_shapes() {
        // Self-describing kind tag.
        assert_eq!(
            serde_json::to_string(&MetricValue::Gauge { value: 142 }).unwrap(),
            r#"{"kind":"gauge","value":142}"#
        );
        assert_eq!(
            serde_json::to_string(&MetricValue::Counter { value: 918273 }).unwrap(),
            r#"{"kind":"counter","value":918273}"#
        );

        let mut metrics = BTreeMap::new();
        metrics.insert(
            "process.open_fds".to_string(),
            MetricValue::Gauge { value: 142 },
        );
        metrics.insert(
            "render.frames_total".to_string(),
            MetricValue::Counter { value: 5 },
        );
        let resp = Response::Metrics {
            ts_ms: 1_733_800_000_000,
            metrics: metrics.clone(),
        };
        let s = serde_json::to_string(&resp).unwrap();
        // BTreeMap keeps keys sorted: open_fds before render.frames_total.
        assert_eq!(
            s,
            r#"{"type":"metrics","ts_ms":1733800000000,"metrics":{"process.open_fds":{"kind":"gauge","value":142},"render.frames_total":{"kind":"counter","value":5}}}"#
        );
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Response::Metrics { ts_ms, .. } if ts_ms == 1_733_800_000_000));

        // Event variant shares the payload shape.
        let ev = Event::Metrics { ts_ms: 1, metrics };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.starts_with(r#"{"type":"metrics","ts_ms":1,"metrics":{"#));
    }

    #[test]
    fn event_window_focused_none() {
        let e = Event::WindowFocused { id: None };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"type":"window_focused","id":null}"#);
    }

    #[test]
    fn event_window_activation_requested_shape() {
        let e = Event::WindowActivationRequested {
            id: "ft-7".into(),
            granted: false,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(
            s,
            r#"{"type":"window_activation_requested","id":"ft-7","granted":false}"#
        );
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Event::WindowActivationRequested { id, granted } if id == "ft-7" && !granted
        ));
    }

    #[test]
    fn find_windows_request_shape() {
        // Bare {} (no filters) serializes terse — both fields skipped.
        let bare = Request::FindWindows {
            title: None,
            app_id: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"find_windows"}"#
        );
        let with_both = Request::FindWindows {
            title: Some("^foo".into()),
            app_id: Some("bar$".into()),
        };
        let s = serde_json::to_string(&with_both).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::FindWindows {
                title: Some(ref t),
                app_id: Some(ref a),
            } if t == "^foo" && a == "bar$"
        ));
    }

    #[test]
    fn get_tree_request_shape() {
        let r = Request::GetTree;
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"type":"get_tree"}"#);
        let back: Request = serde_json::from_str(r#"{"type":"get_tree"}"#).unwrap();
        assert!(matches!(back, Request::GetTree));
    }

    #[test]
    fn window_summary_geometry_is_optional_and_back_compat() {
        // A legacy payload with no `geometry` still deserializes (field is
        // `#[serde(default)]`), and a None geometry is skipped on the wire so
        // the minimal shape is unchanged from pre-geometry clients.
        let legacy = r#"{"id":"a","title":"t","app_id":"x","workspace":1,"focused":false}"#;
        let w: WindowSummary = serde_json::from_str(legacy).unwrap();
        assert!(w.geometry.is_none());
        assert_eq!(serde_json::to_string(&w).unwrap(), legacy);

        let with_geo = WindowSummary {
            id: "a".into(),
            title: "t".into(),
            app_id: "x".into(),
            workspace: 1,
            focused: false,
            geometry: Some(WindowGeometry {
                x: 0,
                y: 0,
                w: 640,
                h: 480,
            }),
            z: Some(2),
            sticky: true,
            always_on_top: true,
        };
        let s = serde_json::to_string(&with_geo).unwrap();
        assert!(s.contains(r#""geometry":{"x":0,"y":0,"w":640,"h":480}"#));
        assert!(s.contains(r#""z":2"#));
        assert!(s.contains(r#""sticky":true"#));
        assert!(s.contains(r#""always_on_top":true"#));
        // A plain summary omits both flag fields entirely (default false).
        let plain = WindowSummary {
            sticky: false,
            always_on_top: false,
            ..with_geo.clone()
        };
        let plain_s = serde_json::to_string(&plain).unwrap();
        assert!(!plain_s.contains("sticky"));
        assert!(!plain_s.contains("always_on_top"));
    }

    #[test]
    fn tree_response_shape() {
        let resp = Response::Tree {
            outputs: vec![OutputNode {
                name: "eDP-1".into(),
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                scale: 1.0,
            }],
            workspaces: vec![WorkspaceNode {
                index: 1,
                name: String::new(),
                focused: true,
                windows: vec![WindowNode {
                    id: "abc".into(),
                    title: "t".into(),
                    app_id: "a".into(),
                    geometry: Some(WindowGeometry {
                        x: 0,
                        y: 0,
                        w: 960,
                        h: 1080,
                    }),
                    output: Some("eDP-1".into()),
                    z: Some(0),
                    focused: true,
                    minimized: false,
                    sticky: true,
                    always_on_top: true,
                    layout: "tiled_left".into(),
                }],
            }],
            minimized: vec![WindowNode {
                id: "m".into(),
                title: "min".into(),
                app_id: "a".into(),
                geometry: None,
                output: None,
                z: None,
                focused: false,
                minimized: true,
                sticky: false,
                always_on_top: false,
                layout: "floating".into(),
            }],
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        match back {
            Response::Tree {
                outputs,
                workspaces,
                minimized,
            } => {
                assert_eq!(outputs.len(), 1);
                assert_eq!(outputs[0].name, "eDP-1");
                assert_eq!(workspaces[0].windows[0].z, Some(0));
                assert_eq!(workspaces[0].windows[0].layout, "tiled_left");
                assert!(workspaces[0].windows[0].sticky);
                assert!(workspaces[0].windows[0].always_on_top);
                assert_eq!(minimized.len(), 1);
                assert!(minimized[0].minimized);
                assert!(!minimized[0].sticky);
                assert!(!minimized[0].always_on_top);
                assert_eq!(minimized[0].z, None);
            }
            _ => panic!("expected Tree"),
        }

        // Unmapped window: geometry / output / z all skipped on the wire.
        let node = WindowNode {
            id: "m".into(),
            title: String::new(),
            app_id: String::new(),
            geometry: None,
            output: None,
            z: None,
            focused: false,
            minimized: true,
            sticky: false,
            always_on_top: false,
            layout: "floating".into(),
        };
        let s = serde_json::to_string(&node).unwrap();
        assert!(!s.contains("geometry"));
        assert!(!s.contains("\"z\""));
        assert!(!s.contains("output"));
        assert!(!s.contains("sticky"));
        assert!(!s.contains("always_on_top"));
        assert!(s.contains(r#""minimized":true"#));
    }

    #[test]
    fn pick_and_close_window_shapes() {
        let pick = Request::PickWindow;
        let s = serde_json::to_string(&pick).unwrap();
        assert_eq!(s, r#"{"type":"pick_window"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::PickWindow));

        let close = Request::CloseWindow { id: "abc".into() };
        let s = serde_json::to_string(&close).unwrap();
        assert_eq!(s, r#"{"type":"close_window","id":"abc"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::CloseWindow { id } if id == "abc"));

        let focus = Request::FocusWindow { id: "abc".into() };
        let s = serde_json::to_string(&focus).unwrap();
        assert_eq!(s, r#"{"type":"focus_window","id":"abc"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::FocusWindow { id } if id == "abc"));

        let raise = Request::RaiseWindow { id: "abc".into() };
        let s = serde_json::to_string(&raise).unwrap();
        assert_eq!(s, r#"{"type":"raise_window","id":"abc"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::RaiseWindow { id } if id == "abc"));

        let lower = Request::LowerWindow { id: "abc".into() };
        let s = serde_json::to_string(&lower).unwrap();
        assert_eq!(s, r#"{"type":"lower_window","id":"abc"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::LowerWindow { id } if id == "abc"));

        let restacked = Event::WindowRestacked { id: "abc".into() };
        let s = serde_json::to_string(&restacked).unwrap();
        assert_eq!(s, r#"{"type":"window_restacked","id":"abc"}"#);
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Event::WindowRestacked { id } if id == "abc"));

        let set_sticky = Request::SetWindowSticky {
            id: "abc".into(),
            sticky: true,
        };
        let s = serde_json::to_string(&set_sticky).unwrap();
        assert_eq!(
            s,
            r#"{"type":"set_window_sticky","id":"abc","sticky":true}"#
        );
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::SetWindowSticky { id, sticky } if id == "abc" && sticky));

        let sticky_evt = Event::WindowStickyChanged {
            id: "abc".into(),
            sticky: false,
        };
        let s = serde_json::to_string(&sticky_evt).unwrap();
        assert_eq!(
            s,
            r#"{"type":"window_sticky_changed","id":"abc","sticky":false}"#
        );
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Event::WindowStickyChanged { id, sticky } if id == "abc" && !sticky)
        );

        let set_aot = Request::SetWindowAlwaysOnTop {
            id: "abc".into(),
            always_on_top: true,
        };
        let s = serde_json::to_string(&set_aot).unwrap();
        assert_eq!(
            s,
            r#"{"type":"set_window_always_on_top","id":"abc","always_on_top":true}"#
        );
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Request::SetWindowAlwaysOnTop { id, always_on_top } if id == "abc" && always_on_top)
        );

        let aot_evt = Event::WindowAlwaysOnTopChanged {
            id: "abc".into(),
            always_on_top: true,
        };
        let s = serde_json::to_string(&aot_evt).unwrap();
        assert_eq!(
            s,
            r#"{"type":"window_always_on_top_changed","id":"abc","always_on_top":true}"#
        );
        let back: Event = serde_json::from_str(&s).unwrap();
        assert!(
            matches!(back, Event::WindowAlwaysOnTopChanged { id, always_on_top } if id == "abc" && always_on_top)
        );

        let set_name = Request::SetWindowName {
            id: "abc".into(),
            name: "Build log".into(),
        };
        let s = serde_json::to_string(&set_name).unwrap();
        assert_eq!(
            s,
            r#"{"type":"set_window_name","id":"abc","name":"Build log"}"#
        );
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::SetWindowName { id, name } if id == "abc" && name == "Build log"
        ));

        let cancelled = Response::PickedWindow { window: None };
        assert_eq!(
            serde_json::to_string(&cancelled).unwrap(),
            r#"{"type":"picked_window","window":null}"#
        );
        let picked = Response::PickedWindow {
            window: Some(WindowSummary {
                id: "abc".into(),
                title: "t".into(),
                app_id: "a".into(),
                workspace: 2,
                focused: true,
                geometry: Some(WindowGeometry {
                    x: 10,
                    y: 20,
                    w: 800,
                    h: 600,
                }),
                z: Some(0),
                sticky: false,
                always_on_top: false,
            }),
        };
        let s = serde_json::to_string(&picked).unwrap();
        let back: Response = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Response::PickedWindow {
                window: Some(WindowSummary {
                    workspace: 2,
                    focused: true,
                    ..
                })
            }
        ));
    }
}
