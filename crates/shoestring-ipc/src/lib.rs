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
    /// List every connected input device (keyboards, pointers, touchpads,
    /// tablets, …) with its libinput identity and capabilities. The input
    /// analogue of [`Request::Outputs`]; reply is [`Response::Inputs`].
    /// Read-only and not gated by automation. Only the TTY/udev backend has
    /// real libinput devices — the nested winit backend reports an empty list.
    Inputs,
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
        /// Number of press/release cycles at the same spot (`2` = double-click).
        /// Absent / `null` means 1. All cycles fire microseconds apart, well
        /// inside any double-click threshold.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    /// Like [`Request::InjectClick`], but the click lands at coordinates
    /// **relative to a specific toplevel's origin** rather than in
    /// compositor-global space. `id` is a [`WindowSummary::id`]; `wx` / `wy`
    /// are window-local logical pixels (`0,0` = the window's top-left). The WM
    /// translates them by the window's current on-screen position, so callers
    /// stay immune to where the compositor placed the window — the key win for
    /// automating apps whose dialogs spawn at variable positions. Runs the same
    /// click-to-focus + press/release as [`Request::InjectClick`]. Errors if the
    /// window is unknown or not currently mapped. Gated by `set_automation`.
    /// Reply is [`Response::Ok`].
    InjectClickToWindow {
        id: String,
        button: String,
        wx: f64,
        wy: f64,
        /// Press/release cycles (`2` = double-click); absent means 1.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    /// Press a button at `from`, move to `to` (with interpolated motion so
    /// apps tracking pointer deltas register the drag), then release — the
    /// press/move/release the space map and other drag surfaces need, without
    /// the caller hand-composing it from `move_mouse` + raw button events.
    /// Coordinates are compositor-global logical pixels, unless `id` is set — a
    /// [`WindowSummary::id`] — in which case `from` / `to` are window-local and
    /// the WM translates them by the window's origin (placement-immune, like
    /// [`Request::InjectClickToWindow`]). `button` as in [`Request::InjectClick`]
    /// (default `"left"`). Errors if `id` is given but unknown/unmapped, or the
    /// button name is bad. Gated by `set_automation`. Reply is [`Response::Ok`].
    Drag {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default = "default_drag_button")]
        button: String,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
    },
    /// Move the pointer to compositor-space `(x, y)` without clicking.
    /// Parity with `xdotool mousemove`; useful for hover-only tests and
    /// for setting up a drag (move → press → move → release). Does not
    /// change keyboard focus — pointer focus follows the motion the same
    /// way it would for a real `wl_pointer.motion` event. Gated by
    /// `set_automation`. Reply is [`Response::Ok`].
    MoveMouse { x: f64, y: f64 },
    /// Inject a single **raw input transition** straight into the seat —
    /// the KVM-passthrough primitive behind remote-desktop serve mode. Unlike
    /// [`Request::InjectKey`] (keysym, atomic tap) and [`Request::InjectClick`]
    /// (atomic press+release), each [`RawInput`] is one low-level event — a key
    /// or button *down or up*, an absolute motion, or a scroll — so a held key,
    /// a drag, or a chord can be reproduced exactly as the remote client sent
    /// it. Keys carry an evdev keycode and are forwarded past the WM's binding
    /// table (the *served* machine's own keymap/binds apply natively). Gated by
    /// `set_automation`. Reply is [`Response::Ok`].
    InjectInput { event: RawInput },
    /// Read the WM's current selection (clipboard, or primary with
    /// `primary: true`). The WM picks the best text mime the owner offers, reads
    /// the bytes out of band, and replies with [`Response::Clipboard`] (a
    /// **deferred** reply — the WM holds the connection until the read drains).
    /// The viewer-box half of cross-machine copy/paste: the remote stack reads a
    /// machine's selection to ship it over the tunnel. Gated by `set_automation`.
    GetClipboard {
        #[serde(default)]
        primary: bool,
    },
    /// Set the WM's selection (clipboard, or primary with `primary: true`) to
    /// `data` under `mime` — the WM becomes the selection owner and serves these
    /// bytes to anything that pastes (back-filling the standard text aliases).
    /// The receiving half: the remote stack writes a buffer pulled over the
    /// tunnel into the local selection. Gated by `set_automation`. Reply is
    /// [`Response::Ok`].
    SetClipboard {
        #[serde(default)]
        primary: bool,
        mime: String,
        data: Vec<u8>,
    },
    /// Enter text into the focused surface **via the clipboard**: if `text` is
    /// given, set the selection to it (UTF-8 `text/plain`, like `set_clipboard`),
    /// then synthesize the paste chord (`modifiers` + `keysym`, default
    /// `Ctrl` + `v`) to the focused window. Unlike [`Request::InjectText`] — which
    /// only handles ASCII letters/digits/space through the keymap — this carries
    /// arbitrary Unicode, punctuation, and long strings, because the bytes travel
    /// through the selection rather than per-keystroke. With `text` omitted it
    /// just pastes whatever the selection already holds. Override `keysym` /
    /// `modifiers` for apps whose paste isn't `Ctrl+V` (terminals often use
    /// `Ctrl`+`Shift`+`v` or `Shift`+`Insert`). Gated by `set_automation`. Reply
    /// is [`Response::Ok`] (or [`Response::Error`] if the paste chord can't be
    /// resolved in the current keymap).
    Paste {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default = "default_paste_keysym")]
        keysym: String,
        #[serde(default = "default_paste_modifiers")]
        modifiers: Vec<String>,
    },
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
    /// Power off the machine. Like [`Quit`](Self::Quit) this pops the
    /// `confirm_action` dialog and only acts on the user's *Yes*; the reply
    /// is [`Response::Ok`] once the dialog is shown. The WM shells out to a
    /// fallback chain (`systemctl poweroff` → `loginctl poweroff` →
    /// `shutdown(8)`) so systemd, elogind, and bare-init/FreeBSD systems all
    /// work (see the systemd-is-optional policy). **Ungated** for the same
    /// reason as `Quit`: the dialog is the authorization. Lets the bar's
    /// control menu offer Shut down without the automation gate.
    PowerOff,
    /// Reboot the machine. Confirm-gated and ungated exactly like
    /// [`PowerOff`](Self::PowerOff); fallback chain is `systemctl reboot` →
    /// `loginctl reboot` → `shutdown -r now`.
    Reboot,
    /// Suspend the machine (sleep / S3). Confirm-gated and ungated exactly
    /// like [`PowerOff`](Self::PowerOff); fallback chain is
    /// `systemctl suspend` → `loginctl suspend` → `zzz` (FreeBSD).
    Suspend,
    /// Quit the **window manager** cleanly for scripted teardown (not the
    /// machine — that's [`PowerOff`](Self::PowerOff)). Signals the WM's process
    /// group so its children (autostart apps, the `-C` command, XWayland, and
    /// anything else that stayed in the group) exit with it, then stops the
    /// event loop — which drops the IPC and Wayland listeners and unlinks their
    /// sockets, so no stale `…-wayland-N.sock` / `.lock` lingers. Unlike the
    /// `Quit` keybind it does not prompt for confirmation. The process-group
    /// signal only fires when the WM is its own group leader (nested backends
    /// make themselves one at startup), so it can never reach the parent
    /// session. Gated by `set_automation`. Reply is [`Response::Ok`], written
    /// just before the loop stops.
    Shutdown,
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
    /// Register the connection as *the* remote-desktop server
    /// (`shoestring-remote-server`). Sent once at the server's startup so the
    /// WM knows a server is available — which is what makes the **remote gate**
    /// selectable (greyed-out until then). The registration lives for the
    /// connection's lifetime: when the server disconnects the WM clears it and
    /// forces the remote gate off. The WM replies [`Response::Remote`] and then
    /// pushes [`Event::RemoteChanged`] to this connection so the server learns
    /// when the gate flips (open / close its ssh-tunneled listener). At most one
    /// server registers at a time; a second registration replaces the first.
    RegisterRemoteServer,
    /// Flip the runtime **remote gate** — the single consent switch for
    /// remote-desktop serve mode. Enabling *couples* the capture gate (so the
    /// served output can be streamed) and the automation gate (so client input
    /// can be injected); disabling turns both back off. Refused with
    /// [`Response::Error`] unless a server has registered
    /// ([`Request::RegisterRemoteServer`]). Reply is [`Response::Remote`];
    /// [`Event::RemoteChanged`] is broadcast when the value changes. Runtime
    /// only — never read from disk, so remote access can't be on at login
    /// (the user enables it explicitly after logging into the served box).
    SetRemote { enabled: bool },
    /// Read the remote-gate state without changing it: whether the gate is on,
    /// whether a server has registered, and the current viewer count. Reply is
    /// [`Response::Remote`].
    RemoteStatus,
    /// Report the number of connected remote viewers/controllers. Sent by the
    /// registered `shoestring-remote-server` as clients connect and disconnect.
    /// The WM caches it and broadcasts [`Event::RemoteChanged`] so the bar can
    /// light a "being viewed / controlled" chip (privacy: never silent). Reply
    /// is [`Response::Ok`].
    ReportRemoteViewers { viewers: u32 },
    /// Register this connection as a **viewable machine** on the local
    /// machine-axis — the viewer-box half of remote desktop. A
    /// `shoestring-remote-client` (or a stand-in) that has tunnelled out to a
    /// remote box registers here so the user can `Super+J/K` to it; index 0 is
    /// always the local machine, registrations take index 1.. in arrival order.
    /// Unlike [`Request::RegisterRemoteServer`] (one served box, single
    /// registration) **many** clients may register. `width`/`height` are the
    /// remote's negotiated pixel size, used to clamp the forwarded virtual
    /// pointer. The connection becomes a subscriber and is added to the axis for
    /// its lifetime: disconnecting removes the machine and, if it was the active
    /// view, returns input to local. While this client is the active view the WM
    /// pushes every local input event to it as [`Event::CapturedInput`]. Reply
    /// is [`Response::RemoteClients`].
    RegisterRemoteClient {
        label: String,
        width: u32,
        height: u32,
    },
    /// Read the machine-axis state without changing it: the registered remote
    /// machines (index 1..) and which index is the active view (0 = local).
    /// Reply is [`Response::RemoteClients`].
    RemoteClientStatus,
    /// Switch the active view on the machine-axis to `index` (0 = local, 1.. =
    /// a registered remote machine). Clamped to the current machine count. The
    /// same internal path the `Super+J/K` / break-out keybinds drive, exposed so
    /// a client (or a test) can switch programmatically. Entering a remote view
    /// starts input capture; leaving it (or `index` 0) stops. Broadcasts
    /// [`Event::ViewChanged`]. Reply is [`Response::RemoteClients`].
    SetView { index: u8 },
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
        /// Write the PNG here instead of the auto-generated
        /// `$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png`. Parent dirs are
        /// created. `shoestring-ctl screenshot --stdout` uses this with a
        /// tmpfs path it then streams, so no large blob crosses the IPC socket.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Capture a **single toplevel**, cropped to just that window (its own
    /// surface tree rendered to an offscreen buffer — correct even when the
    /// window is occluded or on another workspace), and write it as a PNG. `id`
    /// is a [`WindowSummary::id`]. Replies with [`Response::Screenshot`] carrying
    /// the file path, or [`Response::Error`] if the window is unknown or the
    /// active backend can't render offscreen (udev — winit and headless can).
    /// Gated by `set_automation`. `path` behaves as on [`Request::Screenshot`].
    ScreenshotWindow {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// Spawn a child process under the WM's environment (inherits
    /// `WAYLAND_DISPLAY`, `SHOESTRING_WM_SOCKET`, etc.) and return its
    /// captured output once it exits. `argv[0]` is the executable; the
    /// remainder are arguments. `argv` must be non-empty.
    ///
    /// `timeout_ms`: if set, the child is sent `SIGKILL` after this many
    /// milliseconds. The reply still includes whatever output was
    /// captured up to that point. Ignored when `detach` is set.
    ///
    /// `cwd`: run the child in this working directory instead of the WM's
    /// (Stars!'s save dialog, for one, defaults to the process cwd). `env`:
    /// extra `KEY`/`VALUE` pairs layered onto the inherited environment.
    ///
    /// `detach`: spawn the child in its own session (`setsid`, stdio to
    /// `/dev/null`) and reply immediately with [`Response::CommandStarted`]
    /// carrying its PID — the WM neither waits for it nor captures output. Lets
    /// one long-lived WM launch many independent jobs (e.g. an oracle game per
    /// output dir) without blocking the connection. Without it the reply is the
    /// blocking [`Response::CommandResult`].
    ///
    /// Output (non-detached) is capped at [`RUN_COMMAND_OUTPUT_CAP`] bytes per
    /// stream; further bytes are drained from the pipe (so the child does not
    /// block on a full pipe buffer) but discarded, and the response's
    /// `truncated` field is set.
    ///
    /// Gated by `set_automation`: returns [`Response::Error`] when the
    /// runtime automation gate is off.
    RunCommand {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        detach: bool,
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
    /// Force-kill the toplevel matching `id` — the SIGKILL to
    /// [`CloseWindow`]'s polite request. Instead of asking the client to
    /// close, the WM terminates the owning process outright (peer-credential
    /// pid for Wayland clients; the real X client pid via XRes for X11
    /// windows, *not* XWayland's). Use for windows that ignore a close
    /// request (mid-session games, hung apps) — `shoestring-kill -f`. Reply
    /// is [`Response::Ok`] once the kill is issued, [`Response::Error`] if no
    /// window matches or the owning pid can't be resolved.
    KillWindow { id: String },
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
    /// Move the toplevel matching `id` (an `ext-foreign-toplevel-list-v1`
    /// identifier, as carried by [`WindowSummary::id`]) to workspace `index`
    /// (1-based). The per-id, focus-independent counterpart to the
    /// `move-window-to-workspace` [`Request::DispatchAction`] action, which
    /// only ever moves the *focused* window: this targets an arbitrary window
    /// and does **not** switch the active workspace or steal focus, so a TUI /
    /// taskbar can shuffle a background window without disturbing the user's
    /// view.
    ///
    /// Moving a window *onto* the active workspace maps it into view; moving it
    /// *off* the active workspace unmaps it (refocusing the next window if the
    /// moved one held focus). A no-op when the window is already on `index`.
    /// Broadcasts [`Event::WindowMovedToWorkspace`]. Reply is [`Response::Ok`]
    /// on success, [`Response::Error`] if no window matches, `index` is out of
    /// range, or the window is sticky (shown on all workspaces — un-stick it
    /// first) or minimized (restore it first). Not gated by automation (a
    /// benign window-management tweak, like [`Request::FocusWindow`]).
    MoveWindowToWorkspace { id: String, index: u8 },
    /// Minimize (hide) or restore the toplevel matching `id`. The per-id,
    /// focus-independent counterpart to the `minimize` [`Request::DispatchAction`]
    /// action (which toggles the focused window): `minimized: true` hides the
    /// window regardless of focus, `false` restores a hidden one to the active
    /// workspace. Idempotent — minimizing an already-minimized window (or
    /// restoring a visible one) is a no-op. Reply is [`Response::Ok`] on
    /// success, [`Response::Error`] if no window matches. Not gated by
    /// automation (a benign window-management tweak, like [`Request::FocusWindow`]).
    SetWindowMinimized { id: String, minimized: bool },
    /// Maximize or unmaximize the toplevel matching `id`. The per-id,
    /// focus-independent counterpart to the `maximize` [`Request::DispatchAction`]
    /// action (which toggles the focused window): `maximized: true` fills the
    /// work area, `false` restores the saved floating rectangle. Idempotent.
    /// Reply is [`Response::Ok`] on success, [`Response::Error`] if no window
    /// matches. Not gated by automation (a benign window-management tweak, like
    /// [`Request::FocusWindow`]).
    SetWindowMaximized { id: String, maximized: bool },
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
    /// Subscribe to a **streaming damage capture** of an output — the native
    /// damage-push primitive behind remote-desktop serve mode. The WM pushes
    /// only the *damaged tiles* of the chosen output on each render, so an idle
    /// desktop produces no traffic. Consumed by `shoestring-remote-server`,
    /// which relays the frames over an ssh tunnel.
    ///
    /// `output: None` selects the first/default output; `Some(name)` a
    /// specific one. Gated by the **screen-capture gate**
    /// (`[general].screen_capture_enabled` / `set_screen_capture`): returns
    /// [`Response::Error`] when the gate is off, and an active stream is torn
    /// down (a `Bye` frame, then disconnect) if the gate is flipped off.
    ///
    /// Unlike every other request, the reply is **not** pure newline-JSON: the
    /// server writes one [`Response::Ok`] line, then *upgrades the connection*
    /// to a binary stream of length-prefixed `shoestring_remote::ServerMessage`
    /// frames (`Ready`, then `Frame`/`Resize`/`Bye`). The subscriber reads the
    /// `ok` line, then switches to the binary framing.
    CaptureStream {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Subscribe to a damage-tile stream of a **single window** — the capture
    /// primitive behind remote-desktop *graft mode* (pull one remote window into
    /// a local workspace). `id` is a foreign-toplevel identifier (as in
    /// [`WindowSummary::id`]). Unlike [`Request::CaptureStream`], the window is
    /// rendered to its own offscreen buffer, so the stream is correct even when
    /// the window is occluded, minimized, or on a non-active workspace, and its
    /// tiles carry window-local coordinates.
    ///
    /// Gated by the **screen-capture gate** exactly like [`Request::CaptureStream`],
    /// and the reply uses the same **connection upgrade**: one [`Response::Ok`]
    /// line, then a binary stream of `shoestring_remote::ServerMessage` frames
    /// (`Ready`, optional `Meta`, then `Frame`/`Resize`/`Bye`). A `Bye` is sent
    /// when the window closes.
    CaptureWindow { id: String },
    /// Inject a single raw input transition into a **specific window** — the
    /// input primitive behind remote-desktop *graft mode*. Like
    /// [`Request::InjectInput`] but the event is delivered to the window named by
    /// `id` (a foreign-toplevel identifier) rather than wherever the global seat
    /// happens to point: keyboard focus is pinned to that window and pointer
    /// motion is interpreted in **window-local** pixel coordinates, so input
    /// lands correctly even when the window is occluded or off-workspace. Gated
    /// by `set_automation`. Reply is [`Response::Ok`].
    InjectInputToWindow { id: String, event: RawInput },
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
    /// Block the connection until a toplevel matching `title` / `app_id`
    /// **maps** (or, with `unmap: true`, until every currently-matching one
    /// **unmaps**), then reply with [`Response::WaitWindow`]. Both filters are
    /// regexes exactly like [`Request::FindWindows`] — each independent,
    /// AND-ed, an unset filter matches anything.
    ///
    /// The synchronous replacement for sleep-polling [`Request::Windows`] in a
    /// launch script: fire the app, then `wait_window` on its title/app_id and
    /// act the instant it appears. If the condition already holds when the
    /// request arrives (a match is already mapped; or, for `unmap`, none
    /// matches) the reply is immediate. A map wait resolves when the window is
    /// actually usable — for wayland clients that is the first commit carrying
    /// a real title/app_id, not the bare toplevel creation.
    ///
    /// `timeout_ms` bounds the wait; on expiry the reply carries
    /// `timed_out: true` and no window. Omit it to wait forever (until
    /// satisfied or the client disconnects). Read-only observation — **not**
    /// gated by automation (like [`Request::FindWindows`]).
    WaitWindow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        /// Wait for matching windows to unmap (close) instead of to map.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unmap: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u32>,
    },
    /// Block the connection until the compositor is ready, then reply with
    /// [`Response::WaitReady`]. With `xwayland: true` the reply waits until
    /// XWayland has started and `$DISPLAY` is exported — closing the boot race
    /// where an X client launched at startup dies with
    /// `nodrv_CreateWindow: no driver could be loaded` before XWayland (`:N`)
    /// is up (previously worked around by grepping the WM log for
    /// `xwayland ready`). Without `xwayland` the reply is immediate: the socket
    /// having accepted this request is itself proof the compositor loop runs.
    ///
    /// `timeout_ms` bounds the XWayland wait; on expiry the reply carries
    /// `timed_out: true` and `ready: false` (e.g. if XWayland failed to spawn).
    /// Read-only coordination — **not** gated by automation.
    WaitReady {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        xwayland: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u32>,
    },
    /// Read a single pixel's colour from an output — a cheap UI-state probe
    /// that skips the screenshot → PNG → decode round-trip. `output` selects the
    /// output (default: the first); `x` / `y` are that output's **logical**
    /// pixel coordinates (top-left origin), scaled to the physical pixel the WM
    /// reads back. Reply is [`Response::Pixel`] with the channels 0–255, or
    /// [`Response::Error`] if the point falls outside the output. Answers the
    /// oracle's "is this indicator lit / this checkbox ticked?" without OCR.
    /// Gated by `set_automation` (a capture primitive, like [`Request::Screenshot`]).
    GetPixel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        x: i32,
        y: i32,
    },
    /// Hash the pixels of an output region — a cheap "did this change?" probe.
    /// `output` selects the output (default: the first); `region` (in that
    /// output's **logical** coords) defaults to the whole output. Reply is
    /// [`Response::RegionHash`] carrying a 64-bit hash of the captured pixels
    /// plus the region's size in physical pixels. Two probes with equal hashes
    /// are the same image; a changed hash means the region changed — so the
    /// oracle can poll a dialog for "settled / repainted" without shipping or
    /// diffing a full screenshot. Gated by `set_automation`.
    HashRegion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<ScreenshotRegion>,
    },
}

/// One raw input transition for [`Request::InjectInput`] — the low-level events
/// a remote-desktop client forwards (KVM passthrough). Each is a single event,
/// not an atomic gesture, so held keys/buttons and drags reproduce exactly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RawInput {
    /// A key transition. `keycode` is a Linux/evdev keycode (the value *before*
    /// XKB's `+8` offset, as libinput reports it); the WM adds the offset. The
    /// key is forwarded past the binding table — the served machine's own keymap
    /// and keybinds apply.
    Key { keycode: u32, pressed: bool },
    /// A pointer button transition. `button` is an evdev button code
    /// (`BTN_LEFT` = `0x110`, `BTN_RIGHT` = `0x111`, `BTN_MIDDLE` = `0x112`).
    Button { button: u32, pressed: bool },
    /// Absolute pointer motion to compositor-space `(x, y)` — same coordinate
    /// system as [`Request::MoveMouse`].
    Motion { x: f64, y: f64 },
    /// Scroll. `horizontal` / `vertical` are continuous deltas (surface-local
    /// units); either may be zero.
    Axis { horizontal: f64, vertical: f64 },
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

/// One registered remote machine on the machine-axis, for
/// [`Response::RemoteClients`]. `index` is its axis position (1.. ; index 0 is
/// the implicit local machine); `label` is the human name the client registered
/// with (typically the remote host).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineEntry {
    pub index: u8,
    pub label: String,
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
    /// Reply to [`Request::Inputs`]: every connected input device. Empty on
    /// the nested winit backend, which has no real libinput devices.
    Inputs {
        inputs: Vec<InputSummary>,
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
    /// Remote-gate state. Returned for [`Request::RegisterRemoteServer`],
    /// [`Request::SetRemote`], and [`Request::RemoteStatus`]. `enabled` is the
    /// gate; `server_available` is whether a `shoestring-remote-server` has
    /// registered (the bar greys the toggle until then); `viewers` is the
    /// number of connected clients (the "being viewed" chip lights when > 0).
    Remote {
        enabled: bool,
        server_available: bool,
        viewers: u32,
    },
    /// Machine-axis state. Returned for [`Request::RegisterRemoteClient`],
    /// [`Request::RemoteClientStatus`], and [`Request::SetView`]. `machines`
    /// are the registered remote machines (index 1.. ; the local machine is the
    /// implicit index 0 and is not listed); `active` is the current view index
    /// (0 = local).
    RemoteClients {
        machines: Vec<MachineEntry>,
        active: u8,
    },
    /// The selection bytes for [`Request::GetClipboard`]. `mime` is the type the
    /// data is in (`None` with empty `data` = the selection was empty / had no
    /// text the WM could read).
    Clipboard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        data: Vec<u8>,
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
    /// Reply to a `run_command` with `detach: true`: the child was started in
    /// its own session with stdio nulled, and `pid` is its process id. The WM
    /// does not wait for it or capture output — the caller owns its lifetime.
    CommandStarted {
        pid: i32,
    },
    /// Result of [`Request::WaitWindow`]. `window` is the toplevel that
    /// satisfied a **map** wait (`None` for an `unmap` wait — the window is
    /// gone — or on timeout). `timed_out` is `true` only when the wait hit its
    /// `timeout_ms` deadline.
    WaitWindow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<WindowSummary>,
        timed_out: bool,
    },
    /// Result of [`Request::WaitReady`]. `ready` is `true` once the compositor
    /// (and XWayland, if requested) is up; `display` is the X display string
    /// (`":1"`) when an XWayland wait was satisfied, else `None`. `timed_out`
    /// is `true` when the wait hit its deadline (and then `ready` is `false`).
    WaitReady {
        ready: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        timed_out: bool,
    },
    /// Reply to [`Request::GetPixel`]: the probed pixel's channels, 0–255, in
    /// R/G/B/A order regardless of the compositor's internal buffer layout.
    /// Alpha is the compositor's premultiplied value (opaque UI reads `255`).
    Pixel {
        r: u8,
        g: u8,
        b: u8,
        a: u8,
    },
    /// Reply to [`Request::HashRegion`]: a 64-bit hash of the region's pixels
    /// (stable for identical pixels, changes when any pixel does) plus the
    /// region's size in physical pixels. Compare two hashes to detect UI change.
    RegionHash {
        hash: u64,
        width: u32,
        height: u32,
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
    /// Operating-system process id of the window's owning client, when the
    /// compositor can resolve it. `None` for clients whose peer credentials
    /// don't carry a pid (e.g. FreeBSD's `LOCAL_PEERCRED`) and on older WM
    /// builds. For X11 windows this is XWayland's pid, not the X app's, since
    /// the wayland surface belongs to the XWayland connection. Lets a script
    /// resolve a window (matched by `title` / `app_id`) to a pid — e.g. to
    /// find a specific nested `shoestring-wm`'s process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
}

/// `skip_serializing_if` helper for `bool` fields that default to `false`,
/// so the common case stays off the wire (matching the `Option::is_none`
/// pattern the optional fields use).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Default drag button for [`Request::Drag`] — the left mouse button.
fn default_drag_button() -> String {
    "left".to_string()
}

/// Default paste keysym for [`Request::Paste`] — the `v` of `Ctrl+V`.
fn default_paste_keysym() -> String {
    "v".to_string()
}

/// Default paste modifiers for [`Request::Paste`] — `Ctrl` (of `Ctrl+V`).
fn default_paste_modifiers() -> Vec<String> {
    vec!["Ctrl".to_string()]
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

/// A connected input device, as reported by [`Request::Inputs`]. Mirrors the
/// libinput device identity so callers can tell a touchpad from a wired mouse
/// without scraping `/dev/input`. Properties come straight off the live
/// libinput handle the WM holds for `[input]` config application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSummary {
    /// libinput device name (e.g. `"SynPS/2 Synaptics TouchPad"`).
    pub name: String,
    /// Kernel sysfs name, the `eventN`-style node (e.g. `"event5"`).
    pub sysname: String,
    /// USB/Bluetooth vendor id, or `0` when the bus exposes none.
    pub vendor: u32,
    /// USB/Bluetooth product id, or `0` when the bus exposes none.
    pub product: u32,
    /// libinput capabilities the device advertises, as lowercase names:
    /// `"keyboard"`, `"pointer"`, `"touch"`, `"tablet_tool"`, `"tablet_pad"`,
    /// `"gesture"`, `"switch"`. A single device can hold several.
    pub capabilities: Vec<String>,
    /// Physical size `(width, height)` in millimetres, for devices that report
    /// it (touchpads, touchscreens, tablets). `None` for keyboards/mice and
    /// anything libinput can't measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mm: Option<(f64, f64)>,
    /// Name of the output this device is bound to, when libinput knows the
    /// mapping (typically touchscreens/tablets pinned to a screen). Usually
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

fn default_transform() -> String {
    "normal".to_string()
}

/// Which way the clipboard moves for [`Event::RemoteClipboard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOp {
    /// Send the local selection to the active remote.
    Push,
    /// Fetch the active remote's selection into the local one.
    Pull,
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
    /// Fired when any remote-desktop state changes: the gate flipping, a server
    /// (un)registering, or the viewer count moving. Carries the full state so
    /// the bar can light a "being viewed / controlled" chip (when `viewers >
    /// 0`) and grey the gate toggle (until `server_available`), and the
    /// registered server learns when to open/close its listener (`enabled`),
    /// all without polling. Mirrors the [`ScreenCaptureChanged`] privacy-signal
    /// pattern. See [`Request::SetRemote`].
    RemoteChanged {
        enabled: bool,
        server_available: bool,
        viewers: u32,
    },
    /// Fired when the active machine-axis view changes — the user switched which
    /// machine they're driving (`Super+J/K`, the break-out hotkey, a
    /// [`Request::SetView`], or a registration/disconnect that shifted the
    /// active index). `index` is the new view (0 = local, 1.. = a registered
    /// remote); `label` is that machine's name, or `None` for local. The bar
    /// lights a "driving <machine>" chip while `index != 0`; a
    /// `shoestring-remote-client` uses it to reveal/hide its own surface when it
    /// becomes (or stops being) the active view. Broadcast, so non-active
    /// clients see it too. Re-query [`Request::RemoteClientStatus`] for the full
    /// machine list.
    ViewChanged {
        index: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// One captured local input transition, pushed **only to the connection that
    /// is the active machine-axis view** — the viewer-box mirror of
    /// [`Request::InjectInput`]. While a remote machine is the active view the
    /// WM swallows every local input event and forwards it here as the same
    /// [`RawInput`] currency the served side injects, so the client can relay it
    /// over its tunnel for the remote's own keymap/binds to apply (raw KVM
    /// passthrough). Keys carry an evdev keycode; motion is absolute in the
    /// remote's pixel space (clamped to the size from
    /// [`Request::RegisterRemoteClient`]). The axis keys (`Super+J/K`) and the
    /// break-out hotkey are handled locally and never forwarded.
    CapturedInput {
        event: RawInput,
    },
    /// A clipboard bridge action fired locally (`Super+Shift+C` / `+V`) while a
    /// remote is the active view, pushed **only to that active client** so it
    /// can carry the buffer over its tunnel. [`ClipboardOp::Push`] = send this
    /// machine's selection to the remote; [`ClipboardOp::Pull`] = fetch the
    /// remote's selection into this machine. The client services it with
    /// [`Request::GetClipboard`] / [`Request::SetClipboard`] against its local WM
    /// and the matching message on its wire. Like [`Event::CapturedInput`], this
    /// is targeted, never broadcast.
    RemoteClipboard {
        op: ClipboardOp,
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

        // Shutdown is a bare unit variant, distinct from the machine PowerOff.
        assert_eq!(
            serde_json::to_string(&Request::Shutdown).unwrap(),
            r#"{"type":"shutdown"}"#
        );
        let back: Request = serde_json::from_str(r#"{"type":"shutdown"}"#).unwrap();
        assert!(matches!(back, Request::Shutdown));

        // wait_window: optional filters/unmap/timeout all skip when absent, so
        // the minimal form is just the tag; and it round-trips with fields set.
        assert_eq!(
            serde_json::to_string(&Request::WaitWindow {
                title: None,
                app_id: None,
                unmap: false,
                timeout_ms: None,
            })
            .unwrap(),
            r#"{"type":"wait_window"}"#
        );
        let w: Request = serde_json::from_str(
            r#"{"type":"wait_window","app_id":"^Stars$","unmap":true,"timeout_ms":5000}"#,
        )
        .unwrap();
        assert!(matches!(
            w,
            Request::WaitWindow { app_id: Some(ref a), unmap: true, timeout_ms: Some(5000), title: None }
                if a == "^Stars$"
        ));

        // wait_ready: bare form, and the xwayland variant.
        assert_eq!(
            serde_json::to_string(&Request::WaitReady {
                xwayland: false,
                timeout_ms: None,
            })
            .unwrap(),
            r#"{"type":"wait_ready"}"#
        );
        let r: Request =
            serde_json::from_str(r#"{"type":"wait_ready","xwayland":true,"timeout_ms":30000}"#)
                .unwrap();
        assert!(matches!(
            r,
            Request::WaitReady {
                xwayland: true,
                timeout_ms: Some(30000)
            }
        ));

        // get_pixel: output skips when absent; round-trips with coords.
        assert_eq!(
            serde_json::to_string(&Request::GetPixel {
                output: None,
                x: 10,
                y: 20,
            })
            .unwrap(),
            r#"{"type":"get_pixel","x":10,"y":20}"#
        );
        let p: Request =
            serde_json::from_str(r#"{"type":"get_pixel","output":"HEADLESS-1","x":1,"y":2}"#)
                .unwrap();
        assert!(matches!(
            p,
            Request::GetPixel { output: Some(ref o), x: 1, y: 2 } if o == "HEADLESS-1"
        ));

        // hash_region: both output and region optional (whole-output default).
        assert_eq!(
            serde_json::to_string(&Request::HashRegion {
                output: None,
                region: None,
            })
            .unwrap(),
            r#"{"type":"hash_region"}"#
        );
        let h: Request =
            serde_json::from_str(r#"{"type":"hash_region","region":{"x":0,"y":0,"w":64,"h":64}}"#)
                .unwrap();
        assert!(matches!(
            h,
            Request::HashRegion {
                output: None,
                region: Some(ScreenshotRegion { w: 64, h: 64, .. })
            }
        ));
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
            count: None,
        };
        // x/y/count are skipped when absent so simple cases stay terse, and a
        // legacy payload (no count) still deserializes.
        assert_eq!(
            serde_json::to_string(&click_no_xy).unwrap(),
            r#"{"type":"inject_click","button":"left"}"#
        );
        let legacy: Request =
            serde_json::from_str(r#"{"type":"inject_click","button":"left"}"#).unwrap();
        assert!(matches!(legacy, Request::InjectClick { count: None, .. }));
        // A double-click carries count.
        let dbl = Request::InjectClick {
            button: "left".into(),
            x: Some(100.5),
            y: Some(200.0),
            count: Some(2),
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&dbl).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::InjectClick { ref button, x: Some(_), y: Some(_), count: Some(2) }
                if button == "left"
        ));
        let wclick = Request::InjectClickToWindow {
            id: "wl-3".into(),
            button: "left".into(),
            wx: 12.0,
            wy: 34.5,
            count: None,
        };
        assert_eq!(
            serde_json::to_string(&wclick).unwrap(),
            r#"{"type":"inject_click_to_window","id":"wl-3","button":"left","wx":12.0,"wy":34.5}"#
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&wclick).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::InjectClickToWindow { ref id, ref button, wx, wy, count: None }
                if id == "wl-3" && button == "left" && wx == 12.0 && wy == 34.5
        ));
        // Drag: default button fills in, id skips when absent, round-trips.
        let d: Request = serde_json::from_str(
            r#"{"type":"drag","from_x":1.0,"from_y":2.0,"to_x":3.0,"to_y":4.0}"#,
        )
        .unwrap();
        assert!(matches!(
            d,
            Request::Drag { id: None, ref button, from_x, from_y, to_x, to_y }
                if button == "left" && from_x == 1.0 && from_y == 2.0 && to_x == 3.0 && to_y == 4.0
        ));
        let dw = Request::Drag {
            id: Some("wl-9".into()),
            button: "middle".into(),
            from_x: 5.0,
            from_y: 6.0,
            to_x: 7.0,
            to_y: 8.0,
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&dw).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::Drag { id: Some(ref id), ref button, .. }
                if id == "wl-9" && button == "middle"
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
    fn inject_input_raw_shapes() {
        let key = Request::InjectInput {
            event: RawInput::Key {
                keycode: 30,
                pressed: true,
            },
        };
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            r#"{"type":"inject_input","event":{"kind":"key","keycode":30,"pressed":true}}"#
        );

        // Each variant round-trips through the tagged enum.
        for ev in [
            RawInput::Key {
                keycode: 1,
                pressed: false,
            },
            RawInput::Button {
                button: 0x110,
                pressed: true,
            },
            RawInput::Motion { x: 4.5, y: -2.0 },
            RawInput::Axis {
                horizontal: 0.0,
                vertical: 15.0,
            },
        ] {
            let s = serde_json::to_string(&Request::InjectInput { event: ev }).unwrap();
            let back: Request = serde_json::from_str(&s).unwrap();
            assert!(matches!(back, Request::InjectInput { event } if event == ev));
        }
    }

    #[test]
    fn machine_axis_shapes() {
        // Register carries label + remote pixel size.
        let reg = Request::RegisterRemoteClient {
            label: "dev-107".into(),
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            serde_json::to_string(&reg).unwrap(),
            r#"{"type":"register_remote_client","label":"dev-107","width":1920,"height":1080}"#
        );

        let sv = Request::SetView { index: 1 };
        assert_eq!(
            serde_json::to_string(&sv).unwrap(),
            r#"{"type":"set_view","index":1}"#
        );
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"type":"remote_client_status"}"#).unwrap(),
            Request::RemoteClientStatus
        ));

        // Response lists remotes (index 1..); local is implicit.
        let resp = Response::RemoteClients {
            machines: vec![MachineEntry {
                index: 1,
                label: "dev-107".into(),
            }],
            active: 1,
        };
        let back: Response = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        assert!(matches!(
            back,
            Response::RemoteClients { machines, active }
                if active == 1 && machines == vec![MachineEntry { index: 1, label: "dev-107".into() }]
        ));

        // ViewChanged: label present off-local, skipped at local.
        assert_eq!(
            serde_json::to_string(&Event::ViewChanged {
                index: 0,
                label: None,
            })
            .unwrap(),
            r#"{"type":"view_changed","index":0}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::ViewChanged {
                index: 1,
                label: Some("dev-107".into()),
            })
            .unwrap(),
            r#"{"type":"view_changed","index":1,"label":"dev-107"}"#
        );

        // CapturedInput reuses RawInput verbatim.
        let cap = Event::CapturedInput {
            event: RawInput::Motion { x: 12.0, y: 34.0 },
        };
        let back: Event = serde_json::from_str(&serde_json::to_string(&cap).unwrap()).unwrap();
        assert!(matches!(
            back,
            Event::CapturedInput { event: RawInput::Motion { x, y } } if x == 12.0 && y == 34.0
        ));
    }

    #[test]
    fn clipboard_shapes() {
        // GetClipboard: primary defaults false and is omittable.
        let g: Request = serde_json::from_str(r#"{"type":"get_clipboard"}"#).unwrap();
        assert!(matches!(g, Request::GetClipboard { primary: false }));
        let gp: Request =
            serde_json::from_str(r#"{"type":"get_clipboard","primary":true}"#).unwrap();
        assert!(matches!(gp, Request::GetClipboard { primary: true }));

        // SetClipboard carries mime + bytes; round-trips.
        let s = Request::SetClipboard {
            primary: false,
            mime: "text/plain;charset=utf-8".into(),
            data: b"hi".to_vec(),
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::SetClipboard { primary: false, mime, data }
                if mime == "text/plain;charset=utf-8" && data == b"hi"
        ));

        // Clipboard response: empty selection skips the mime field.
        assert_eq!(
            serde_json::to_string(&Response::Clipboard {
                mime: None,
                data: vec![],
            })
            .unwrap(),
            r#"{"type":"clipboard","data":[]}"#
        );
        let r: Response =
            serde_json::from_str(r#"{"type":"clipboard","mime":"text/plain","data":[104,105]}"#)
                .unwrap();
        assert!(
            matches!(r, Response::Clipboard { mime, data } if mime.as_deref() == Some("text/plain") && data == b"hi")
        );

        // RemoteClipboard event op round-trips snake_case.
        assert_eq!(
            serde_json::to_string(&Event::RemoteClipboard {
                op: ClipboardOp::Push,
            })
            .unwrap(),
            r#"{"type":"remote_clipboard","op":"push"}"#
        );
        let pull: Event =
            serde_json::from_str(r#"{"type":"remote_clipboard","op":"pull"}"#).unwrap();
        assert!(matches!(
            pull,
            Event::RemoteClipboard {
                op: ClipboardOp::Pull
            }
        ));
    }

    #[test]
    fn paste_request_shapes() {
        // A bare paste defaults to Ctrl+V of the current selection.
        let bare: Request = serde_json::from_str(r#"{"type":"paste"}"#).unwrap();
        assert!(matches!(
            bare,
            Request::Paste { text: None, ref keysym, ref modifiers }
                if keysym == "v" && modifiers == &["Ctrl"]
        ));
        // Set-and-paste with an overridden chord round-trips.
        let p = Request::Paste {
            text: Some("~/Stars! Games/α.m1".into()),
            keysym: "Insert".into(),
            modifiers: vec!["Shift".into()],
        };
        let back: Request = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::Paste { text: Some(ref t), ref keysym, ref modifiers }
                if t == "~/Stars! Games/α.m1" && keysym == "Insert" && modifiers == &["Shift"]
        ));
    }

    #[test]
    fn inputs_request_response_shapes() {
        let q = Request::Inputs;
        assert_eq!(serde_json::to_string(&q).unwrap(), r#"{"type":"inputs"}"#);

        // A plain pointer: no physical size, no output mapping — both optional
        // fields are skipped so the common case stays terse.
        let mouse = InputSummary {
            name: "Logitech USB Mouse".into(),
            sysname: "event2".into(),
            vendor: 0x046d,
            product: 0xc077,
            capabilities: vec!["pointer".into()],
            size_mm: None,
            output: None,
        };
        assert_eq!(
            serde_json::to_string(&mouse).unwrap(),
            r#"{"name":"Logitech USB Mouse","sysname":"event2","vendor":1133,"product":49271,"capabilities":["pointer"]}"#
        );

        // A touchpad reports a physical size; that field appears.
        let touchpad = InputSummary {
            name: "SynPS/2 Synaptics TouchPad".into(),
            sysname: "event5".into(),
            vendor: 2,
            product: 7,
            capabilities: vec!["pointer".into(), "gesture".into()],
            size_mm: Some((97.33, 66.86)),
            output: None,
        };
        let s = serde_json::to_string(&touchpad).unwrap();
        assert!(s.contains(r#""size_mm":[97.33,66.86]"#), "got {s}");
        let resp = Response::Inputs {
            inputs: vec![touchpad],
        };
        let back: Response = serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
        match back {
            Response::Inputs { inputs } => {
                assert_eq!(inputs.len(), 1);
                assert_eq!(inputs[0].capabilities, ["pointer", "gesture"]);
                assert_eq!(inputs[0].size_mm, Some((97.33, 66.86)));
            }
            _ => panic!("expected Inputs"),
        }
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
    fn remote_request_response_event_shapes() {
        assert_eq!(
            serde_json::to_string(&Request::RegisterRemoteServer).unwrap(),
            r#"{"type":"register_remote_server"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetRemote { enabled: true }).unwrap(),
            r#"{"type":"set_remote","enabled":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::RemoteStatus).unwrap(),
            r#"{"type":"remote_status"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ReportRemoteViewers { viewers: 2 }).unwrap(),
            r#"{"type":"report_remote_viewers","viewers":2}"#
        );
        let resp = Response::Remote {
            enabled: true,
            server_available: true,
            viewers: 1,
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"remote","enabled":true,"server_available":true,"viewers":1}"#
        );
        let ev = Event::RemoteChanged {
            enabled: false,
            server_available: true,
            viewers: 0,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"remote_changed","enabled":false,"server_available":true,"viewers":0}"#
        );
        // Round-trips back to the same value.
        let back: Request =
            serde_json::from_str(r#"{"type":"set_remote","enabled":false}"#).unwrap();
        assert!(matches!(back, Request::SetRemote { enabled: false }));
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
    fn per_id_window_action_shapes() {
        assert_eq!(
            serde_json::to_string(&Request::MoveWindowToWorkspace {
                id: "win-7".into(),
                index: 4,
            })
            .unwrap(),
            r#"{"type":"move_window_to_workspace","id":"win-7","index":4}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetWindowMinimized {
                id: "win-7".into(),
                minimized: true,
            })
            .unwrap(),
            r#"{"type":"set_window_minimized","id":"win-7","minimized":true}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::SetWindowMaximized {
                id: "win-7".into(),
                maximized: false,
            })
            .unwrap(),
            r#"{"type":"set_window_maximized","id":"win-7","maximized":false}"#
        );
        // Round-trip back into the same request.
        let back: Request =
            serde_json::from_str(r#"{"type":"move_window_to_workspace","id":"abc","index":16}"#)
                .unwrap();
        assert!(matches!(
            back,
            Request::MoveWindowToWorkspace { ref id, index: 16 } if id == "abc"
        ));
    }

    #[test]
    fn screenshot_request_shapes() {
        let bare = Request::Screenshot {
            output: None,
            region: None,
            path: None,
        };
        // path default-skips so the simple shape is unchanged on the wire.
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
            path: Some("/tmp/x.png".into()),
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
                path: Some(ref p),
            } if n == "eDP-1" && p == "/tmp/x.png"
        ));
        // A legacy client without `path` still deserializes.
        let legacy: Request =
            serde_json::from_str(r#"{"type":"screenshot","output":"eDP-1"}"#).unwrap();
        assert!(matches!(legacy, Request::Screenshot { path: None, .. }));
        let resp = Response::Screenshot {
            path: "/tmp/foo.png".into(),
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"screenshot","path":"/tmp/foo.png"}"#
        );
        // Per-window capture, with a path override and the default.
        let win = Request::ScreenshotWindow {
            id: "wl-7".into(),
            path: Some("/tmp/w.png".into()),
        };
        assert_eq!(
            serde_json::to_string(&win).unwrap(),
            r#"{"type":"screenshot_window","id":"wl-7","path":"/tmp/w.png"}"#
        );
        let bare_win = Request::ScreenshotWindow {
            id: "wl-8".into(),
            path: None,
        };
        assert_eq!(
            serde_json::to_string(&bare_win).unwrap(),
            r#"{"type":"screenshot_window","id":"wl-8"}"#
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&win).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::ScreenshotWindow { ref id, path: Some(ref p) } if id == "wl-7" && p == "/tmp/w.png"
        ));
    }

    #[test]
    fn run_command_request_response_shapes() {
        let bare = Request::RunCommand {
            argv: vec!["echo".into(), "hi".into()],
            timeout_ms: None,
            cwd: None,
            env: vec![],
            detach: false,
        };
        // cwd/env/detach default-skip so the simple shape is unchanged, and a
        // legacy client (no cwd/env/detach) still deserializes.
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"run_command","argv":["echo","hi"]}"#
        );
        let legacy: Request =
            serde_json::from_str(r#"{"type":"run_command","argv":["echo"]}"#).unwrap();
        assert!(matches!(
            legacy,
            Request::RunCommand { detach: false, cwd: None, ref env, .. } if env.is_empty()
        ));
        let full = Request::RunCommand {
            argv: vec!["wine".into(), "stars.exe".into()],
            timeout_ms: Some(250),
            cwd: Some("/games/g1".into()),
            env: vec![("WINEPREFIX".into(), "/p1".into())],
            detach: true,
        };
        let s = serde_json::to_string(&full).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::RunCommand {
                ref argv,
                timeout_ms: Some(250),
                cwd: Some(ref d),
                ref env,
                detach: true,
            } if argv == &["wine", "stars.exe"] && d == "/games/g1"
                && env == &[("WINEPREFIX".to_string(), "/p1".to_string())]
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
        assert_eq!(
            serde_json::to_string(&Response::CommandStarted { pid: 4321 }).unwrap(),
            r#"{"type":"command_started","pid":4321}"#
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

        // capture_stream: output is skipped when None.
        let bare = Request::CaptureStream { output: None };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"capture_stream"}"#
        );
        let named = Request::CaptureStream {
            output: Some("HEADLESS-1".into()),
        };
        assert_eq!(
            serde_json::to_string(&named).unwrap(),
            r#"{"type":"capture_stream","output":"HEADLESS-1"}"#
        );
        let back: Request = serde_json::from_str(r#"{"type":"capture_stream"}"#).unwrap();
        assert!(matches!(back, Request::CaptureStream { output: None }));

        // capture_window: window id is required.
        let cw = Request::CaptureWindow {
            id: "toplevel-7".into(),
        };
        assert_eq!(
            serde_json::to_string(&cw).unwrap(),
            r#"{"type":"capture_window","id":"toplevel-7"}"#
        );
        let back: Request = serde_json::from_str(r#"{"type":"capture_window","id":"x"}"#).unwrap();
        assert!(matches!(back, Request::CaptureWindow { id } if id == "x"));

        // inject_input_to_window: id + raw event.
        let iitw = Request::InjectInputToWindow {
            id: "toplevel-7".into(),
            event: RawInput::Button {
                button: 0x110,
                pressed: true,
            },
        };
        assert_eq!(
            serde_json::to_string(&iitw).unwrap(),
            r#"{"type":"inject_input_to_window","id":"toplevel-7","event":{"kind":"button","button":272,"pressed":true}}"#
        );
        let back: Request = serde_json::from_str(&serde_json::to_string(&iitw).unwrap()).unwrap();
        assert!(matches!(
            back,
            Request::InjectInputToWindow {
                id,
                event: RawInput::Button { button: 0x110, pressed: true }
            } if id == "toplevel-7"
        ));
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
            pid: Some(4242),
        };
        let s = serde_json::to_string(&with_geo).unwrap();
        assert!(s.contains(r#""geometry":{"x":0,"y":0,"w":640,"h":480}"#));
        assert!(s.contains(r#""z":2"#));
        assert!(s.contains(r#""sticky":true"#));
        assert!(s.contains(r#""always_on_top":true"#));
        assert!(s.contains(r#""pid":4242"#));
        // A plain summary omits the optional/flag fields entirely.
        let plain = WindowSummary {
            sticky: false,
            always_on_top: false,
            pid: None,
            ..with_geo.clone()
        };
        let plain_s = serde_json::to_string(&plain).unwrap();
        assert!(!plain_s.contains("sticky"));
        assert!(!plain_s.contains("always_on_top"));
        assert!(!plain_s.contains("pid"));
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

        let kill = Request::KillWindow { id: "abc".into() };
        let s = serde_json::to_string(&kill).unwrap();
        assert_eq!(s, r#"{"type":"kill_window","id":"abc"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::KillWindow { id } if id == "abc"));

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
                pid: Some(99),
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
