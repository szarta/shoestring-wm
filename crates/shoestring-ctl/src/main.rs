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

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the active workspace and total workspace count.
    Workspaces,
    /// List every mapped window with title, app_id, workspace, focused flag.
    Windows,
    /// List every connected output with its mode and scale.
    Outputs,
    /// Stream events forever (one JSON line per event). Exits on socket
    /// close (typically when the WM quits).
    EventStream,
    /// Synthesize a single keypress (press + release) targeting the
    /// focused surface. KEYSYM is an X keysym name like "Return", "F5",
    /// or "q".
    Key {
        /// X keysym name (e.g. "Return", "BackSpace", "Page_Up", "q").
        keysym: String,
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    let socket_path = cli
        .socket
        .or_else(client_socket_path)
        .context("could not resolve socket path: set $SHOESTRING_WM_SOCKET or pass --socket")?;

    let stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("connect to {}", socket_path.display()))?;

    // Capture before moving cli.cmd into the match — only EventStream
    // keeps the connection open afterward.
    let is_stream = matches!(cli.cmd, Command::EventStream);

    let request = match cli.cmd {
        Command::Workspaces => Request::Workspaces,
        Command::Windows => Request::Windows,
        Command::Outputs => Request::Outputs,
        Command::EventStream => Request::EventStream,
        Command::Key { keysym } => Request::InjectKey { keysym },
        Command::Type { text } => Request::InjectText { text },
        Command::Click { button, x, y } => Request::InjectClick { button, x, y },
        Command::Lock => Request::Lock,
        Command::Automation { action } => match action {
            AutomationAction::On => Request::SetAutomation { enabled: true },
            AutomationAction::Off => Request::SetAutomation { enabled: false },
            AutomationAction::Status => Request::AutomationStatus,
        },
        Command::Screenshot { output, region } => {
            let region = region.as_deref().map(parse_region).transpose()?;
            Request::Screenshot { output, region }
        }
        Command::RunCommand { argv, timeout_ms } => Request::RunCommand { argv, timeout_ms },
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
