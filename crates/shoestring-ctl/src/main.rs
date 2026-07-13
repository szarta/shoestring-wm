//! Reference CLI client for shoestring-wm's IPC socket.
//!
//! Connects to `$SHOESTRING_WM_SOCKET` (or the default
//! `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`), sends one
//! [`Request`], and either pretty-prints the [`Response`] or streams
//! [`Event`]s. Stdout is line-oriented JSON unless `--pretty` is passed.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use shoestring_ipc::{client_socket_path, Event, Request, Response, ScreenshotRegion};

#[derive(Debug, Parser)]
#[command(name = "shoestring-ctl", version, about)]
struct Cli {
    /// Override the socket path (otherwise $SHOESTRING_WM_SOCKET or the default).
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// Print JSON output indented for human reading.
    #[arg(short, long)]
    pretty: bool,

    #[command(subcommand)]
    cmd: Command,
}

// `RunCommand` happens to end with the enum's name; renaming would
// change the user-facing CLI subcommand and IPC protocol, so allow it.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Print the active workspace and total workspace count.
    Workspaces,
    /// List every mapped window with title, app_id, workspace, focused flag.
    Windows,
    /// List windows whose title and/or app_id match the given regexes.
    /// Each filter is independent and AND-ed; an omitted filter matches
    /// everything. Patterns are not anchored — `firefox` matches anywhere
    /// in the string; use `^firefox$` for exact match. Output shape is
    /// the same as `windows`. Useful for find-then-focus scripting:
    /// `shoestring-ctl find-windows --app-id '^Alacritty$' | jq ...`.
    FindWindows {
        /// Regex matched against the window title.
        #[arg(short, long)]
        title: Option<String>,
        /// Regex matched against the window app_id.
        #[arg(short, long)]
        app_id: Option<String>,
    },
    /// List every connected output with its mode and scale.
    Outputs,
    /// List every connected input device (keyboards, pointers, touchpads,
    /// tablets) with its libinput identity and capabilities — the input
    /// analogue of `outputs`. Read-only and not gated by automation. Reports
    /// nothing under the nested winit backend, which has no libinput devices.
    Inputs,
    /// Dump the full window tree: outputs with their logical placement, plus
    /// each workspace and the windows on it (geometry, stacking order, and
    /// the output each window sits on). The `swaymsg -t get_tree` analogue —
    /// the go-to query for layout scripting. Pair with `-p` for indented
    /// output, or pipe to `jq`. Read-only and not gated by automation.
    #[command(alias = "get-tree")]
    Tree,
    /// Print the WM's diagnostics metrics: process resource gauges
    /// (`process.open_fds`, `process.rss_kb`, `process.fd_limit`) plus WM
    /// counts. Read-only and not gated by automation. A one-shot snapshot
    /// by default; with `--watch` it subscribes and prints a metrics line
    /// per sample until the WM closes the socket (Ctrl-C to stop). Pair
    /// with `-p` for indented output.
    Metrics {
        /// Stream samples instead of printing a single snapshot.
        #[arg(short, long)]
        watch: bool,
        /// Desired push interval in milliseconds while watching. Clamped
        /// up to the WM's `[diagnostics].sample_interval_ms`. Ignored
        /// without `--watch`.
        #[arg(long, value_name = "MS")]
        interval: Option<u32>,
    },
    /// Stream events forever (one JSON line per event). Exits on socket
    /// close (typically when the WM quits).
    EventStream,
    /// Synthesize a keypress (press + release) targeting the focused
    /// surface. KEYSYM is an X keysym name like "Return", "F5", or "q",
    /// optionally prefixed with modifiers in `+`-syntax
    /// (`super+shift+q`, xdotool-compatible). Pass `--mod` (repeatable)
    /// for the same effect without parsing. Modifier aliases:
    /// `super`/`logo`/`mod4`/`win`, `ctrl`/`control`, `alt`/`mod1`,
    /// `shift` (case-insensitive). Chords the WM consumes
    /// (e.g. `super+shift+q`) won't reach the focused surface — use
    /// `dispatch-action` for those.
    Key {
        /// X keysym name or `mod+mod+keysym` chord. The keysym is the
        /// last `+`-separated token; everything before it becomes a
        /// modifier (merged with `--mod`).
        keysym: String,
        /// Modifier to hold while the keysym is pressed. Repeatable.
        /// Released in reverse order after the keysym.
        #[arg(short = 'm', long = "mod", value_name = "NAME")]
        modifiers: Vec<String>,
    },
    /// Type a literal string into the focused surface. Per-keystroke synthesis
    /// supports ASCII letters, digits, and space; other characters return an
    /// error — use --via-clipboard for arbitrary text (Unicode, punctuation,
    /// long strings).
    Type {
        /// Text to type.
        text: String,
        /// Enter the text via the clipboard + a paste chord instead of
        /// per-keystroke synthesis. Sets the selection to TEXT, then pastes it
        /// into the focused surface — handles anything plain typing can't.
        #[arg(long)]
        via_clipboard: bool,
        /// Paste chord for --via-clipboard (e.g. "Ctrl+Shift+v",
        /// "Shift+Insert"). Defaults to Ctrl+V.
        #[arg(long, default_value = "Ctrl+v", requires = "via_clipboard")]
        paste_key: String,
    },
    /// Paste the current selection into the focused surface by synthesizing the
    /// paste chord (default Ctrl+V). Pair with `set-clipboard` for arbitrary
    /// text entry, or use `type --via-clipboard` to set + paste in one call.
    /// Requires the automation gate.
    Paste {
        /// Paste chord (e.g. "Ctrl+Shift+v", "Shift+Insert"). Defaults to Ctrl+V.
        #[arg(long, default_value = "Ctrl+v")]
        key: String,
    },
    /// Synthesize a single mouse click. BUTTON is "left", "right",
    /// "middle", or a numeric BTN_* code. Pass --x/--y together to move
    /// the pointer to those compositor-space coordinates first, or
    /// --window with --wx/--wy to click at coordinates relative to a
    /// window's origin (immune to where the window was placed).
    Click {
        /// Button name or numeric BTN_* code.
        button: String,
        /// X coordinate to move the pointer to before clicking. Requires --y.
        #[arg(long, requires = "y", conflicts_with = "window")]
        x: Option<f64>,
        /// Y coordinate to move the pointer to before clicking. Requires --x.
        #[arg(long, requires = "x", conflicts_with = "window")]
        y: Option<f64>,
        /// Toplevel id (from `windows`) to click relative to. Requires
        /// --wx and --wy; mutually exclusive with --x/--y.
        #[arg(long, requires_all = ["wx", "wy"])]
        window: Option<String>,
        /// Window-local X (logical px, 0 = left edge). Requires --window.
        #[arg(long, requires = "window")]
        wx: Option<f64>,
        /// Window-local Y (logical px, 0 = top edge). Requires --window.
        #[arg(long, requires = "window")]
        wy: Option<f64>,
        /// Press/release cycles at the same spot. 2 = double-click. Default 1.
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Drag with a button held: press at --from, move to --to, release.
    /// Coordinates are compositor-space, unless --window is given, in which case
    /// --from/--to are window-local (immune to window placement). Requires the
    /// automation gate.
    Drag {
        /// Start point `X,Y`.
        #[arg(long, value_name = "X,Y")]
        from: String,
        /// End point `X,Y`.
        #[arg(long, value_name = "X,Y")]
        to: String,
        /// Button to hold: left/right/middle or a numeric BTN_* code.
        #[arg(long, default_value = "left")]
        button: String,
        /// Toplevel id (from `windows`); makes --from/--to window-local.
        #[arg(long)]
        window: Option<String>,
    },
    /// Move the pointer to compositor-space (X, Y) without clicking. Same
    /// coordinate system as `click --x --y`. Useful for hover-only tests
    /// and for composing drags. Requires the automation gate.
    MoveMouse {
        /// X coordinate to move the pointer to.
        x: f64,
        /// Y coordinate to move the pointer to.
        y: f64,
    },
    /// Print the current pointer location as
    /// `{"type":"pointer_position","x":...,"y":...}`. Read-only and not
    /// gated by automation.
    PointerPosition,
    /// Block until a toplevel matching --title / --app-id maps (or, with
    /// --unmap, until every currently-matching one closes), then print it.
    /// Filters are regexes like `find-windows` (unset matches anything). The
    /// synchronous replacement for sleeping and re-running `windows`: launch an
    /// app, then `wait-window --app-id '^Stars$'`. Exits 2 (not 1) on timeout so
    /// scripts can tell "timed out" from a real error. Read-only, not gated.
    WaitWindow {
        /// Regex matched against the window title.
        #[arg(short, long)]
        title: Option<String>,
        /// Regex matched against the window app_id.
        #[arg(short, long)]
        app_id: Option<String>,
        /// Wait for matching windows to close instead of to appear.
        #[arg(long)]
        unmap: bool,
        /// Give up after MS milliseconds (exit 2). 0 = wait forever.
        #[arg(long, value_name = "MS", default_value_t = 10_000)]
        timeout: u32,
    },
    /// Block until the compositor is ready. With --xwayland, wait until XWayland
    /// is up and $DISPLAY is exported — the fix for launching X11 apps (wine,
    /// Stars!) at startup without racing XWayland. Without it, returns as soon
    /// as the WM answers. Exits 2 on timeout. Read-only, not gated.
    WaitReady {
        /// Also wait for XWayland ($DISPLAY exported).
        #[arg(long)]
        xwayland: bool,
        /// Give up after MS milliseconds (exit 2). 0 = wait forever.
        #[arg(long, value_name = "MS", default_value_t = 10_000)]
        timeout: u32,
    },
    /// Lock the session. Spawns the WM's configured lock binary
    /// (`general.lock_command` in the WM config, default
    /// `shoestring-lock`).
    Lock,
    /// Quit the window manager cleanly (not the machine — that's `power off`).
    /// Signals the WM's process group so its children exit too, then stops the
    /// compositor, unlinking the IPC and Wayland sockets. Unlike the Quit
    /// keybind there's no confirmation prompt — the deterministic teardown for
    /// scripted / batch sessions. Requires the automation gate.
    Shutdown,
    /// Read or toggle the runtime automation gate. Affects future
    /// inject_* / remote-automation IPC calls. Not persisted to disk;
    /// the WM's config file is the source of truth at next start.
    Automation {
        #[command(subcommand)]
        action: AutomationAction,
    },
    /// Read or toggle the runtime screen-capture gate. Controls whether the
    /// `zwlr_screencopy` protocol is advertised, i.e. whether tools like OBS,
    /// grim, or the screen-share portal can read the screen. Off by default;
    /// not persisted to disk — the WM's config file is the source of truth at
    /// next start.
    ScreenCapture {
        #[command(subcommand)]
        action: ScreenCaptureAction,
    },
    /// Read the media-privacy snapshot, or mute/unmute the default audio
    /// output or microphone. Mute control is delegated to `shoestring-mediad`
    /// (PipeWire); the WM only reflects live state. Camera is status-only —
    /// reported in `media status`, with no off-switch.
    Media {
        #[command(subcommand)]
        action: MediaAction,
    },
    /// Run a command under the WM's environment (inherits
    /// WAYLAND_DISPLAY, SHOESTRING_WM_SOCKET, etc.) and print the
    /// captured stdout/stderr + exit code as JSON. Requires the
    /// automation gate to be on. Pass argv after `--`, e.g.
    /// `shoestring-ctl run-command -- alacritty --version`.
    RunCommand {
        /// Kill the child with SIGKILL after this many milliseconds.
        /// The reply still includes any output captured before the
        /// kill. Ignored with --detach.
        #[arg(long, value_name = "MS", conflicts_with = "detach")]
        timeout_ms: Option<u32>,
        /// Run the child in this working directory instead of the WM's.
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Extra environment variable as KEY=VALUE, layered onto the
        /// inherited environment. Repeatable.
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Spawn the child detached (own session, stdio to /dev/null) and
        /// return its PID immediately instead of waiting for it to exit.
        #[arg(long)]
        detach: bool,
        /// Command and arguments. The first value is the executable.
        #[arg(required = true, trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Re-read the WM's config file, recompile binds, and broadcast a
    /// `config_reloaded` event. Equivalent to the `reload-config`
    /// keybind. Useful in scripts and as a manual trigger when the
    /// filesystem watcher's auto-reload isn't enough (e.g. the WM was
    /// launched without --config and a file was placed later).
    ReloadConfig,
    /// Enter interactive window-picker mode and print the chosen window
    /// (or `{"window": null}` on cancel). Blocks until the user clicks
    /// or presses Escape. Useful for scripting custom kill / focus / move
    /// flows on top of the picker primitive.
    PickWindow,
    /// Close a window by its `ext-foreign-toplevel-list-v1` identifier
    /// (the `id` field of `windows` / `event_stream` records). The WM
    /// sends `xdg_toplevel.close`; the client may surface a save-prompt
    /// rather than exiting immediately.
    CloseWindow {
        /// FT identifier of the window to close.
        id: String,
    },
    /// Force-kill a window by its `ext-foreign-toplevel-list-v1` identifier.
    /// Unlike `close-window`, the WM terminates the owning process (SIGKILL)
    /// instead of asking it to close — for windows that ignore a close
    /// request. Backs `shoestring-kill -f`.
    KillWindow {
        /// FT identifier of the window to kill.
        id: String,
    },
    /// Focus a window by its `ext-foreign-toplevel-list-v1` identifier.
    /// Unminimizes if needed, switches workspaces if needed.
    FocusWindow {
        /// FT identifier of the window to focus.
        id: String,
    },
    /// Raise a window to the top of the stacking order by its
    /// `ext-foreign-toplevel-list-v1` identifier. A pure restack — keyboard
    /// focus and the active workspace are unchanged (unlike `focus-window`).
    RaiseWindow {
        /// FT identifier of the window to raise.
        id: String,
    },
    /// Lower a window to the bottom of the stacking order by its
    /// `ext-foreign-toplevel-list-v1` identifier. The complement of
    /// `raise-window`.
    LowerWindow {
        /// FT identifier of the window to lower.
        id: String,
    },
    /// Set or clear the sticky flag (show on all workspaces) on a window by
    /// its `ext-foreign-toplevel-list-v1` identifier.
    SetSticky {
        /// FT identifier of the window.
        id: String,
        /// `true` to pin to all workspaces, `false` to release.
        #[arg(action = clap::ArgAction::Set)]
        sticky: bool,
    },
    /// Set or clear the always-on-top flag on a window by its
    /// `ext-foreign-toplevel-list-v1` identifier.
    SetAlwaysOnTop {
        /// FT identifier of the window.
        id: String,
        /// `true` to keep above other windows, `false` to release.
        #[arg(action = clap::ArgAction::Set)]
        always_on_top: bool,
    },
    /// Override a window's display name, keyed by its
    /// `ext-foreign-toplevel-list-v1` identifier. The override wins over the
    /// client's own title everywhere the WM reports a title (`windows`,
    /// `get_tree`, `find-windows`, the `window_title_changed` event), so bars
    /// and window-jump menus show and match on it. Pass an empty NAME to clear
    /// the override and revert to the client's title.
    SetName {
        /// FT identifier of the window to rename.
        id: String,
        /// New display name; empty string clears the override.
        name: String,
    },
    /// Move a window to a workspace by its `ext-foreign-toplevel-list-v1`
    /// identifier. Unlike the `move-window-to-workspace` action (focused
    /// window only), this targets an arbitrary window and leaves the active
    /// workspace and focus untouched. INDEX is 1-based.
    MoveWindow {
        /// FT identifier of the window to move.
        id: String,
        /// 1-based destination workspace.
        index: u8,
    },
    /// Minimize or restore a window by its `ext-foreign-toplevel-list-v1`
    /// identifier, regardless of which window has focus.
    SetMinimized {
        /// FT identifier of the window.
        id: String,
        /// `true` to hide, `false` to restore.
        #[arg(action = clap::ArgAction::Set)]
        minimized: bool,
    },
    /// Maximize or unmaximize a window by its `ext-foreign-toplevel-list-v1`
    /// identifier, regardless of which window has focus.
    SetMaximized {
        /// FT identifier of the window.
        id: String,
        /// `true` to fill the work area, `false` to restore the floating rect.
        #[arg(action = clap::ArgAction::Set)]
        maximized: bool,
    },
    /// Capture a PNG screenshot via the WM and print the resulting
    /// path. Requires the automation gate to be on. Path is
    /// auto-generated as `$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png`.
    Screenshot {
        /// Output name (e.g. `eDP-1`). Defaults to the first output the
        /// compositor advertises. Required when `--region` is set.
        #[arg(short, long)]
        output: Option<String>,
        /// Capture only this rectangle in the named output's logical
        /// coords. Format: `X,Y,W,H`. Implies `--output` is required.
        #[arg(long, value_name = "X,Y,W,H", requires = "output")]
        region: Option<String>,
        /// Capture just this toplevel (id from `windows`), cropped to the
        /// window. Renders the window's own surface tree, so it works even
        /// when occluded or off-screen. Mutually exclusive with
        /// --output/--region; winit and headless backends only.
        #[arg(long, conflicts_with_all = ["output", "region"])]
        window: Option<String>,
        /// Write the PNG here instead of the default
        /// `$XDG_PICTURES_DIR/Screenshot-AUTO-<ts>.png`. Parent dirs are
        /// created. Works with --window too.
        #[arg(long, value_name = "FILE", conflicts_with = "stdout")]
        path: Option<String>,
        /// Stream the raw PNG bytes to stdout instead of writing a file —
        /// for piping captures (`... screenshot --stdout > shot.png`).
        #[arg(long)]
        stdout: bool,
    },
    /// Read the WM's current selection and write the raw bytes to stdout.
    /// The WM picks the best text mime the owner offers. Requires the
    /// automation gate to be on. The viewer-box half of cross-machine
    /// copy/paste; locally, a focus-free `wl-paste`.
    GetClipboard {
        /// Read the primary selection (middle-click) instead of the clipboard.
        #[arg(long)]
        primary: bool,
    },
    /// Set the WM's selection to TEXT (or stdin if omitted). The WM becomes the
    /// selection owner and serves these bytes to anything that pastes. Requires
    /// the automation gate. A focus-free `wl-copy`.
    SetClipboard {
        /// The text to set. If omitted, the bytes are read from stdin.
        text: Option<String>,
        /// Set the primary selection instead of the clipboard.
        #[arg(long)]
        primary: bool,
        /// Mime type to advertise. Defaults to UTF-8 text; text mimes fan out
        /// to the standard aliases server-side.
        #[arg(long, default_value = "text/plain;charset=utf-8")]
        mime: String,
    },
    /// Run a bind `Action` server-side as if a keybind had fired. Unlike
    /// `key`, this does not need an external key chord to land on a
    /// focused surface — Super+Shift+Q is consumed by the WM, but
    /// `dispatch-action quit` fires the same Action::Quit path. Requires
    /// the automation gate to be on.
    ///
    /// ACTION is either a bare kebab-case name for a no-arg action
    /// (`quit`, `tile-left`, `maximize`, `reload-config`, `lock`, ...)
    /// or a JSON object for parametric actions (`{"type":"focus-workspace",
    /// "index":3}`).
    DispatchAction {
        /// Bare name (e.g. `quit`) or JSON object literal.
        action: String,
    },
    /// Set a per-output gamma ramp from a color temperature, server-side —
    /// the WM drives the CRTC directly, so no night-light daemon is needed.
    /// KMS-only (udev/TTY backend). Takes over any wlr-gamma-control client
    /// on the affected output. Example: `set-gamma --temperature 3000`.
    SetGamma {
        /// Output name (e.g. `eDP-1`). Defaults to every gamma-capable output.
        #[arg(short, long)]
        output: Option<String>,
        /// Color temperature in Kelvin (1000–25000). ~6500 is neutral, lower
        /// is warmer.
        #[arg(short, long)]
        temperature: u32,
        /// Overall brightness multiplier (0.1–1.0). Defaults to 1.0.
        #[arg(short, long)]
        brightness: Option<f64>,
        /// Gamma exponent (0.1–10.0). Defaults to 1.0.
        #[arg(short, long)]
        gamma: Option<f64>,
    },
    /// Clear the WM's own (IPC-set) gamma and restore the output(s) to their
    /// original ramp. Leaves wlr-gamma-control clients alone.
    ResetGamma {
        /// Output name. Defaults to every output the WM set.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Read or toggle the runtime remote-desktop gate. Turning it on couples
    /// the capture + automation gates so `shoestring-remote-server` can stream
    /// the output and replay client input. Refused unless a server has
    /// registered. Not persisted to disk — the headless/container entrypoint
    /// opens it at boot; a desktop session toggles it from the bar chip.
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
    /// List the machine-axis: the remote machines registered as viewable
    /// (index 1..) and which index is the active view (0 = local). Read-only.
    RemoteClients,
    /// Switch the active machine-axis view to INDEX (0 = local, 1.. = a
    /// registered remote machine). Clamped to the machine count. The
    /// programmatic equivalent of the Super+J/K binds; while a remote is the
    /// active view the WM forwards all input to it.
    SetView {
        /// 0 = local; 1.. selects a registered remote machine.
        index: u8,
    },
}

#[derive(Debug, Subcommand)]
enum AutomationAction {
    /// Turn the gate ON. Inject_* IPC and future remote-automation
    /// methods will be allowed.
    On,
    /// Turn the gate OFF.
    Off,
    /// Print the current state.
    Status,
}

#[derive(Debug, Subcommand)]
enum RemoteAction {
    /// Turn the gate ON: couple the capture + automation gates and let the
    /// registered server stream + replay input. Refused if no server has
    /// registered yet.
    On,
    /// Turn the gate OFF: the server's listener closes and no port stays open.
    Off,
    /// Print the current state (gate, whether a server is registered, viewers).
    Status,
}

#[derive(Debug, Subcommand)]
enum ScreenCaptureAction {
    /// Turn the gate ON. The `zwlr_screencopy` global is advertised and
    /// capture tools can read the screen.
    On,
    /// Turn the gate OFF. The global is withdrawn and captures are refused.
    Off,
    /// Print the current state.
    Status,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Debug, Subcommand)]
enum MediaAction {
    /// Print the current media-privacy snapshot (sink/mic mute + camera-in-use),
    /// or `{"type":"media","state":null}` if no monitor has reported yet.
    Status,
    /// Mute (`on`) or unmute (`off`) the default audio output.
    AudioMute { state: OnOff },
    /// Mute (`on`) or unmute (`off`) the default microphone. Stream-mute only —
    /// it does not prevent a device open (same as a hardware mic key).
    MicMute { state: OnOff },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let socket_path = cli
        .socket
        .or_else(client_socket_path)
        .context("could not resolve socket path: set $SHOESTRING_WM_SOCKET or pass --socket")?;

    let stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("connect to {}", socket_path.display()))?;

    // Capture before moving cli.cmd into the match — only EventStream and
    // `metrics --watch` keep the connection open afterward.
    let is_stream = matches!(
        cli.cmd,
        Command::EventStream | Command::Metrics { watch: true, .. }
    );

    // Set by `screenshot --stdout`: the tmpfs file the WM writes, which we then
    // stream to our stdout and unlink. Routing the bytes through a file (rather
    // than an IPC response) keeps the non-blocking IPC socket clear of a
    // send-buffer-sized PNG blob.
    let mut screenshot_stream_temp: Option<PathBuf> = None;

    let request = match cli.cmd {
        Command::Workspaces => Request::Workspaces,
        Command::Windows => Request::Windows,
        Command::FindWindows { title, app_id } => Request::FindWindows { title, app_id },
        Command::Outputs => Request::Outputs,
        Command::Inputs => Request::Inputs,
        Command::Tree => Request::GetTree,
        Command::Metrics { watch, interval } => {
            if watch {
                Request::MetricsStream {
                    interval_ms: interval,
                }
            } else {
                Request::Metrics
            }
        }
        Command::EventStream => Request::EventStream,
        Command::Key { keysym, modifiers } => {
            let (keysym, modifiers) = split_chord(&keysym, modifiers);
            Request::InjectKey { keysym, modifiers }
        }
        Command::Type {
            text,
            via_clipboard,
            paste_key,
        } => {
            if via_clipboard {
                let (keysym, modifiers) = split_chord(&paste_key, vec![]);
                Request::Paste {
                    text: Some(text),
                    keysym,
                    modifiers,
                }
            } else {
                Request::InjectText { text }
            }
        }
        Command::Paste { key } => {
            let (keysym, modifiers) = split_chord(&key, vec![]);
            Request::Paste {
                text: None,
                keysym,
                modifiers,
            }
        }
        Command::Click {
            button,
            x,
            y,
            window,
            wx,
            wy,
            count,
        } => match window {
            // requires_all guarantees wx/wy are present when --window is.
            Some(id) => Request::InjectClickToWindow {
                id,
                button,
                wx: wx.unwrap(),
                wy: wy.unwrap(),
                count: Some(count),
            },
            None => Request::InjectClick {
                button,
                x,
                y,
                count: Some(count),
            },
        },
        Command::Drag {
            from,
            to,
            button,
            window,
        } => {
            let (from_x, from_y) = parse_xy(&from)?;
            let (to_x, to_y) = parse_xy(&to)?;
            Request::Drag {
                id: window,
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            }
        }
        Command::MoveMouse { x, y } => Request::MoveMouse { x, y },
        Command::PointerPosition => Request::PointerPosition,
        Command::WaitWindow {
            title,
            app_id,
            unmap,
            timeout,
        } => Request::WaitWindow {
            title,
            app_id,
            unmap,
            // 0 = wait forever (protocol treats a missing timeout that way).
            timeout_ms: (timeout != 0).then_some(timeout),
        },
        Command::WaitReady { xwayland, timeout } => Request::WaitReady {
            xwayland,
            timeout_ms: (timeout != 0).then_some(timeout),
        },
        Command::Lock => Request::Lock,
        Command::Shutdown => Request::Shutdown,
        Command::Automation { action } => match action {
            AutomationAction::On => Request::SetAutomation { enabled: true },
            AutomationAction::Off => Request::SetAutomation { enabled: false },
            AutomationAction::Status => Request::AutomationStatus,
        },
        Command::ScreenCapture { action } => match action {
            ScreenCaptureAction::On => Request::SetScreenCapture { enabled: true },
            ScreenCaptureAction::Off => Request::SetScreenCapture { enabled: false },
            ScreenCaptureAction::Status => Request::ScreenCaptureStatus,
        },
        Command::Media { action } => match action {
            MediaAction::Status => Request::MediaStatus,
            MediaAction::AudioMute { state } => Request::SetAudioMute {
                enabled: matches!(state, OnOff::On),
            },
            MediaAction::MicMute { state } => Request::SetMicMute {
                enabled: matches!(state, OnOff::On),
            },
        },
        Command::Screenshot {
            output,
            region,
            window,
            path,
            stdout,
        } => {
            // --stdout: have the WM write to a tmpfs file we stream and unlink.
            let dest = if stdout {
                let tmp = screenshot_stdout_temp();
                screenshot_stream_temp = Some(tmp.clone());
                Some(tmp.to_string_lossy().into_owned())
            } else {
                path
            };
            match window {
                Some(id) => Request::ScreenshotWindow { id, path: dest },
                None => {
                    let region = region.as_deref().map(parse_region).transpose()?;
                    Request::Screenshot {
                        output,
                        region,
                        path: dest,
                    }
                }
            }
        }
        Command::RunCommand {
            argv,
            timeout_ms,
            cwd,
            env,
            detach,
        } => {
            let env = env
                .iter()
                .map(|kv| parse_env_kv(kv))
                .collect::<Result<Vec<_>>>()?;
            Request::RunCommand {
                argv,
                timeout_ms,
                cwd,
                env,
                detach,
            }
        }
        Command::ReloadConfig => Request::ReloadConfig,
        Command::PickWindow => Request::PickWindow,
        Command::CloseWindow { id } => Request::CloseWindow { id },
        Command::KillWindow { id } => Request::KillWindow { id },
        Command::FocusWindow { id } => Request::FocusWindow { id },
        Command::RaiseWindow { id } => Request::RaiseWindow { id },
        Command::LowerWindow { id } => Request::LowerWindow { id },
        Command::SetSticky { id, sticky } => Request::SetWindowSticky { id, sticky },
        Command::SetAlwaysOnTop { id, always_on_top } => {
            Request::SetWindowAlwaysOnTop { id, always_on_top }
        }
        Command::SetName { id, name } => Request::SetWindowName { id, name },
        Command::MoveWindow { id, index } => Request::MoveWindowToWorkspace { id, index },
        Command::SetMinimized { id, minimized } => Request::SetWindowMinimized { id, minimized },
        Command::SetMaximized { id, maximized } => Request::SetWindowMaximized { id, maximized },
        Command::GetClipboard { primary } => Request::GetClipboard { primary },
        Command::SetClipboard {
            text,
            primary,
            mime,
        } => {
            let data = match text {
                Some(t) => t.into_bytes(),
                None => {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)
                        .context("read clipboard data from stdin")?;
                    buf
                }
            };
            Request::SetClipboard {
                primary,
                mime,
                data,
            }
        }
        Command::DispatchAction { action } => Request::DispatchAction {
            action: parse_action(&action)?,
        },
        Command::SetGamma {
            output,
            temperature,
            brightness,
            gamma,
        } => Request::SetGamma {
            output,
            temperature,
            brightness,
            gamma,
        },
        Command::ResetGamma { output } => Request::ResetGamma { output },
        Command::Remote { action } => match action {
            RemoteAction::On => Request::SetRemote { enabled: true },
            RemoteAction::Off => Request::SetRemote { enabled: false },
            RemoteAction::Status => Request::RemoteStatus,
        },
        Command::RemoteClients => Request::RemoteClientStatus,
        Command::SetView { index } => Request::SetView { index },
    };

    let mut writer = stream.try_clone()?;
    let req_line = serde_json::to_string(&request)?;
    writeln!(writer, "{req_line}")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);

    // Every request gets at least one response line.
    let mut first = String::new();
    let n = reader.read_line(&mut first)?;
    if n == 0 {
        anyhow::bail!("server closed connection before responding");
    }
    let response: Response = serde_json::from_str(first.trim_end()).context("parse response")?;

    if let Response::Error { message } = &response {
        eprintln!("server error: {message}");
        std::process::exit(1);
    }

    if is_stream {
        // After the Response::Ok ack, the rest is a stream of Events.
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader.read_line(&mut buf)?;
            if n == 0 {
                break;
            }
            let event: Event = match serde_json::from_str(buf.trim_end()) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("invalid event from server: {e}: {}", buf.trim_end());
                    continue;
                }
            };
            print_value(&event, cli.pretty)?;
        }
    } else if let Response::Clipboard { mime, data } = &response {
        // Selection bytes go to stdout verbatim so `get-clipboard` pipes like
        // `wl-paste` (the mime is on stderr for visibility, off the data path).
        if let Some(m) = mime {
            eprintln!("mime: {m}");
        }
        use std::io::Write as _;
        std::io::stdout().write_all(data)?;
    } else if let (Some(tmp), Response::Screenshot { .. }) = (&screenshot_stream_temp, &response) {
        // `screenshot --stdout`: the WM wrote the PNG to our tmpfs file; stream
        // it to stdout so it pipes like a file, then remove it.
        use std::io::Write as _;
        let bytes = std::fs::read(tmp).with_context(|| format!("read {}", tmp.display()))?;
        std::io::stdout().write_all(&bytes)?;
        let _ = std::fs::remove_file(tmp);
    } else {
        print_value(&response, cli.pretty)?;
        // A satisfied wait exits 0; a timed-out one exits 2 so scripts can tell
        // "the thing never happened" from a genuine server error (exit 1).
        if matches!(
            &response,
            Response::WaitWindow {
                timed_out: true,
                ..
            } | Response::WaitReady {
                timed_out: true,
                ..
            }
        ) {
            std::process::exit(2);
        }
    }

    Ok(())
}

/// Split an xdotool-style chord arg (`super+shift+q`) into the trailing
/// keysym and the leading modifier list, appended to any `--mod` the
/// user already passed. A keysym with no `+` is returned unchanged.
/// `--mod`s are listed first (preserving the user's order) and chord
/// tokens follow, since the IPC presses modifiers in vector order.
fn split_chord(keysym: &str, mut mods: Vec<String>) -> (String, Vec<String>) {
    if !keysym.contains('+') {
        return (keysym.to_string(), mods);
    }
    let mut parts: Vec<&str> = keysym.split('+').collect();
    let keysym = parts.pop().expect("split always yields >=1 part");
    for p in parts {
        mods.push(p.to_string());
    }
    (keysym.to_string(), mods)
}

/// Expand a `dispatch-action` argument into the JSON Value the IPC
/// expects. A bare kebab-case name like `quit` becomes `{"type":"quit"}`;
/// anything starting with `{` is passed through as already-formed JSON.
fn parse_action(s: &str) -> Result<serde_json::Value> {
    let trimmed = s.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed)
            .with_context(|| format!("dispatch-action: not valid JSON: {trimmed}"));
    }
    Ok(serde_json::json!({ "type": trimmed }))
}

/// A tmpfs destination for `screenshot --stdout`: the WM writes the PNG here
/// and we stream + unlink it. `$XDG_RUNTIME_DIR` (RAM-backed) when set, else the
/// system temp dir. One capture per `ctl` process, so the pid disambiguates.
fn screenshot_stdout_temp() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("shoestring-shot-{}.png", std::process::id()))
}

/// Split a `KEY=VALUE` argument for `run-command --env`. The value may itself
/// contain `=`; only the first `=` splits. An empty key is rejected.
fn parse_env_kv(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .with_context(|| format!("--env expected KEY=VALUE (got {s:?})"))?;
    anyhow::ensure!(!k.is_empty(), "--env has an empty key in {s:?}");
    Ok((k.to_string(), v.to_string()))
}

/// Parse an `X,Y` coordinate pair (floats) for `drag --from/--to`.
fn parse_xy(s: &str) -> Result<(f64, f64)> {
    let (x, y) = s
        .split_once(',')
        .with_context(|| format!("expected X,Y (got {s:?})"))?;
    let x: f64 = x
        .trim()
        .parse()
        .with_context(|| format!("bad X in {s:?}"))?;
    let y: f64 = y
        .trim()
        .parse()
        .with_context(|| format!("bad Y in {s:?}"))?;
    Ok((x, y))
}

fn parse_region(s: &str) -> Result<ScreenshotRegion> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        anyhow::bail!("--region expected X,Y,W,H (got {s:?})");
    }
    let mut nums = [0i32; 4];
    for (i, p) in parts.iter().enumerate() {
        nums[i] = p
            .trim()
            .parse()
            .with_context(|| format!("--region field {} not an int: {p:?}", i + 1))?;
    }
    let [x, y, w, h] = nums;
    if w <= 0 || h <= 0 {
        anyhow::bail!("--region size must be positive (got {w}x{h})");
    }
    Ok(ScreenshotRegion { x, y, w, h })
}

fn print_value<T: serde::Serialize>(value: &T, pretty: bool) -> Result<()> {
    let mut out = std::io::stdout().lock();
    if pretty {
        serde_json::to_writer_pretty(&mut out, value)?;
    } else {
        serde_json::to_writer(&mut out, value)?;
    }
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_chord;

    #[test]
    fn split_chord_passes_through_bare_keysym() {
        let (k, m) = split_chord("Return", vec![]);
        assert_eq!(k, "Return");
        assert!(m.is_empty());
    }

    #[test]
    fn split_chord_extracts_modifiers() {
        let (k, m) = split_chord("super+shift+q", vec![]);
        assert_eq!(k, "q");
        assert_eq!(m, vec!["super", "shift"]);
    }

    #[test]
    fn split_chord_appends_to_existing_mods() {
        // --mod values are pressed before chord-derived ones.
        let (k, m) = split_chord("alt+x", vec!["ctrl".into()]);
        assert_eq!(k, "x");
        assert_eq!(m, vec!["ctrl", "alt"]);
    }
}
