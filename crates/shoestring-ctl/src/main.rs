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
use clap::{Parser, Subcommand};
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
    /// Type a literal string into the focused surface. v1 supports ASCII
    /// letters, digits, and space; other characters return an error.
    Type {
        /// Text to type.
        text: String,
    },
    /// Synthesize a single mouse click. BUTTON is "left", "right",
    /// "middle", or a numeric BTN_* code. Pass --x/--y together to move
    /// the pointer to those compositor-space coordinates first.
    Click {
        /// Button name or numeric BTN_* code.
        button: String,
        /// X coordinate to move the pointer to before clicking. Requires --y.
        #[arg(long, requires = "y")]
        x: Option<f64>,
        /// Y coordinate to move the pointer to before clicking. Requires --x.
        #[arg(long, requires = "x")]
        y: Option<f64>,
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
    /// Lock the session. Spawns the WM's configured lock binary
    /// (`general.lock_command` in the WM config, default
    /// `shoestring-lock`).
    Lock,
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
    /// Run a command under the WM's environment (inherits
    /// WAYLAND_DISPLAY, SHOESTRING_WM_SOCKET, etc.) and print the
    /// captured stdout/stderr + exit code as JSON. Requires the
    /// automation gate to be on. Pass argv after `--`, e.g.
    /// `shoestring-ctl run-command -- alacritty --version`.
    RunCommand {
        /// Kill the child with SIGKILL after this many milliseconds.
        /// The reply still includes any output captured before the
        /// kill.
        #[arg(long, value_name = "MS")]
        timeout_ms: Option<u32>,
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
enum ScreenCaptureAction {
    /// Turn the gate ON. The `zwlr_screencopy` global is advertised and
    /// capture tools can read the screen.
    On,
    /// Turn the gate OFF. The global is withdrawn and captures are refused.
    Off,
    /// Print the current state.
    Status,
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

    let request = match cli.cmd {
        Command::Workspaces => Request::Workspaces,
        Command::Windows => Request::Windows,
        Command::FindWindows { title, app_id } => Request::FindWindows { title, app_id },
        Command::Outputs => Request::Outputs,
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
        Command::Type { text } => Request::InjectText { text },
        Command::Click { button, x, y } => Request::InjectClick { button, x, y },
        Command::MoveMouse { x, y } => Request::MoveMouse { x, y },
        Command::PointerPosition => Request::PointerPosition,
        Command::Lock => Request::Lock,
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
        Command::Screenshot { output, region } => {
            let region = region.as_deref().map(parse_region).transpose()?;
            Request::Screenshot { output, region }
        }
        Command::RunCommand { argv, timeout_ms } => Request::RunCommand { argv, timeout_ms },
        Command::ReloadConfig => Request::ReloadConfig,
        Command::PickWindow => Request::PickWindow,
        Command::CloseWindow { id } => Request::CloseWindow { id },
        Command::FocusWindow { id } => Request::FocusWindow { id },
        Command::RaiseWindow { id } => Request::RaiseWindow { id },
        Command::LowerWindow { id } => Request::LowerWindow { id },
        Command::SetSticky { id, sticky } => Request::SetWindowSticky { id, sticky },
        Command::SetAlwaysOnTop { id, always_on_top } => {
            Request::SetWindowAlwaysOnTop { id, always_on_top }
        }
        Command::SetName { id, name } => Request::SetWindowName { id, name },
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
    } else {
        print_value(&response, cli.pretty)?;
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
