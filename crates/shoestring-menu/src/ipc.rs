//! Minimal IPC client for the `windows` mode.
//!
//! The WM speaks newline-delimited JSON over a unix socket. We need exactly
//! two one-shot round-trips: list the mapped windows ([`fetch_windows`]) and
//! focus the chosen one ([`focus_window`]). This mirrors shoestring-bar's
//! `ipc_client.rs` — kept as its own tiny copy rather than a shared crate so
//! the menu stays a self-contained binary.
//!
//! Neither request is behind the automation gate, so the picker works in a
//! normal desktop session.

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
};

use anyhow::{Context, Result};
use shoestring_ipc::{client_socket_path, Request, Response, WindowSummary};

fn connect() -> Result<UnixStream> {
    let path = client_socket_path().context(
        "could not resolve socket path; is shoestring-wm running and exporting \
         $SHOESTRING_WM_SOCKET?",
    )?;
    UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))
}

fn write_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let line = serde_json::to_string(req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Read exactly one newline-delimited line. `None` on EOF before any byte.
fn read_line(stream: &mut UnixStream) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(String::from_utf8(buf)?))
                };
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(String::from_utf8(buf)?));
                }
                buf.push(byte[0]);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Send `Request::Windows`; return every mapped window across all workspaces
/// (including minimized ones — `FocusWindow` unminimizes on the way in).
pub fn fetch_windows() -> Result<Vec<WindowSummary>> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::Windows)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse windows response")?;
    match resp {
        Response::Windows { windows } => Ok(windows),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::FocusWindow { id }`. The WM unminimizes the window if
/// needed, switches to its workspace, and gives it keyboard focus.
pub fn focus_window(id: &str) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::FocusWindow { id: id.into() })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse focus_window response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}
