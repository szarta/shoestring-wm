//! Thin IPC client helpers over the shoestring-wm unix socket.
//!
//! Two shapes, mirroring `shoestring-ctl`: a blocking one-shot
//! request/response for queries and commands, and a background event-stream
//! subscriber that nudges the UI to refresh whenever the WM's state changes —
//! so the view stays live without polling.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{anyhow, Context, Result};
use shoestring_ipc::{Request, Response};

/// Connect, send exactly one `req`, and read exactly one response line.
pub fn request(socket: &Path, req: &Request) -> Result<Response> {
    let stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{}", serde_json::to_string(req)?)?;
    writer.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(anyhow!("server closed connection before responding"));
    }
    serde_json::from_str(line.trim_end()).context("parse response")
}

/// Send a command-style request and require a [`Response::Ok`], surfacing a
/// [`Response::Error`] as `Err(message)`.
pub fn request_ok(socket: &Path, req: &Request) -> Result<()> {
    match request(socket, req)? {
        Response::Ok => Ok(()),
        Response::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected response: {other:?}")),
    }
}

/// Spawn a detached thread that subscribes to the WM event stream and sends a
/// `()` on `tx` for every event received (and once more when the stream ends).
/// The UI treats each nudge as "something changed, re-fetch the snapshot",
/// which keeps the protocol coupling trivial: we never have to interpret event
/// payloads, just react to their arrival. The thread exits on its own when the
/// receiver is dropped (process exit) or the socket closes.
pub fn spawn_event_thread(socket: PathBuf, tx: Sender<()>) {
    std::thread::spawn(move || {
        let Ok(stream) = UnixStream::connect(&socket) else {
            return;
        };
        let Ok(mut writer) = stream.try_clone() else {
            return;
        };
        let req = serde_json::to_string(&Request::EventStream).expect("serialize EventStream");
        if writeln!(writer, "{req}").is_err() || writer.flush().is_err() {
            return;
        }
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            // One nudge per line, whether it parses or not — the UI only cares
            // that the world moved. A read error ends the stream; nudge once
            // so the UI can reflect the disconnect on its next refresh.
            if line.is_err() {
                let _ = tx.send(());
                return;
            }
            if tx.send(()).is_err() {
                return;
            }
        }
        let _ = tx.send(());
    });
}
