//! `shoestring-kill` — xkill-equivalent click-to-close picker.
//!
//! Sends [`Request::PickWindow`] to the WM, waits for the user's click,
//! then closes the chosen toplevel — politely via [`Request::CloseWindow`]
//! by default, or with [`Request::KillWindow`] (SIGKILL the owning process)
//! under `-f` / `--force` for windows that ignore a close.
//! Prints a single human-readable line on stderr describing what
//! happened, and exits 0 on a successful close, 1 on cancel, 2 on
//! protocol/IO error.
//!
//! The two requests run on separate connections — the WM allows only one
//! in-flight request per connection, and the picker reply closes the first
//! connection on resolution.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    process::ExitCode,
};

use anyhow::{Context, Result};
use shoestring_ipc::{client_socket_path, Request, Response};

const EXIT_OK: u8 = 0;
const EXIT_CANCELLED: u8 = 1;
const EXIT_ERROR: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("shoestring-kill: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run() -> Result<u8> {
    // `-f` / `--force` switches from a polite close (the client may show a
    // save-prompt or ignore it) to a force-kill that SIGKILLs the owning
    // process — for windows that won't die otherwise (mid-session games,
    // hung apps). `-h` / `--help` prints usage.
    let mut force = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-f" | "--force" => force = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: shoestring-kill [-f|--force]\n  \
                     click a window to close it; -f force-kills (SIGKILL) instead"
                );
                return Ok(EXIT_OK);
            }
            other => anyhow::bail!("unknown argument {other:?} (try --help)"),
        }
    }

    let socket =
        client_socket_path().context("could not resolve socket path: set $SHOESTRING_WM_SOCKET")?;

    // The picker grabs the keyboard until the user resolves it. Print a
    // hint so anyone who runs the binary blind from a terminal knows how
    // to get back out without killing the process.
    eprintln!(
        "shoestring-kill: click a window to {} (Escape or right-click to cancel)",
        if force { "force-kill it" } else { "close it" }
    );

    let picked = match send(&socket, &Request::PickWindow)? {
        Response::PickedWindow { window } => window,
        Response::Error { message } => {
            anyhow::bail!("pick_window: {message}");
        }
        other => anyhow::bail!("pick_window: unexpected reply {other:?}"),
    };

    let Some(window) = picked else {
        eprintln!("shoestring-kill: cancelled");
        return Ok(EXIT_CANCELLED);
    };

    let label = if window.title.is_empty() {
        window.app_id.clone()
    } else {
        format!("{} ({})", window.title, window.app_id)
    };

    let (request, verb) = if force {
        (
            Request::KillWindow {
                id: window.id.clone(),
            },
            "force-killed",
        )
    } else {
        (
            Request::CloseWindow {
                id: window.id.clone(),
            },
            "closed",
        )
    };
    match send(&socket, &request)? {
        Response::Ok => {
            eprintln!("shoestring-kill: {verb} {label}");
            Ok(EXIT_OK)
        }
        Response::Error { message } => {
            anyhow::bail!("{:?}: {message}", window.id);
        }
        other => anyhow::bail!("unexpected reply {other:?}"),
    }
}

fn send(socket: &std::path::Path, request: &Request) -> Result<Response> {
    let stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    let mut writer = stream.try_clone().context("clone socket for write half")?;
    let line = serde_json::to_string(request)?;
    writeln!(writer, "{line}")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let n = reader.read_line(&mut buf)?;
    if n == 0 {
        anyhow::bail!("server closed connection before responding");
    }
    let response: Response = serde_json::from_str(buf.trim_end()).context("parse response")?;
    Ok(response)
}
