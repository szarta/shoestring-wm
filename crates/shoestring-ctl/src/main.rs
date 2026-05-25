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
use shoestring_ipc::{client_socket_path, Event, Request, Response};

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
