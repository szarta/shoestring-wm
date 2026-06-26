//! The local-WM IPC connection — the viewer-box half of the A7 machine axis.
//!
//! We open one connection, send [`Request::RegisterRemoteClient`] (which makes
//! this a viewable machine *and* an event subscriber), and keep it open. From
//! then on the WM pushes [`Event::ViewChanged`] (so we reveal/hide our surface
//! and learn when we're the active view) and [`Event::CapturedInput`] (the local
//! input to relay to the server while we're active). Mirrors the partial-line,
//! non-blocking drain in `shoestring-bar`'s `ipc_client`.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use shoestring_ipc::{client_socket_path, Event, Request, Response};

pub struct Wm {
    stream: UnixStream,
    /// Partial line(s) not yet terminated by `\n`.
    buf: Vec<u8>,
    /// This client's index on the machine axis (1-based), assigned at register.
    my_index: u8,
}

impl Wm {
    /// Register on the local WM's machine-axis as a viewable machine named
    /// `label`, sized to the remote's pixel dimensions (so the WM clamps the
    /// forwarded virtual pointer correctly). Returns the open subscriber
    /// connection; our assigned axis index is [`Wm::my_index`].
    pub fn register(label: &str, width: u32, height: u32) -> Result<Self> {
        let path = client_socket_path().context(
            "could not resolve $SHOESTRING_WM_SOCKET; is the local shoestring-wm running?",
        )?;
        tracing::debug!(socket = %path.display(), "local WM ipc connect");
        let mut stream =
            UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))?;

        let req = Request::RegisterRemoteClient {
            label: label.to_string(),
            width,
            height,
        };
        let line = serde_json::to_string(&req)?;
        stream.write_all(line.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        // Read the one-line RemoteClients reply. Our entry was just appended, so
        // we are the last machine; its index is ours.
        let reply = read_line(&mut stream)?.context("WM closed before register reply")?;
        let my_index = match serde_json::from_str::<Response>(&reply)
            .context("parse register_remote_client reply")?
        {
            Response::RemoteClients { machines, .. } => machines
                .last()
                .map(|m| m.index)
                .context("WM returned an empty machine list after register")?,
            Response::Error { message } => anyhow::bail!("WM refused registration: {message}"),
            other => anyhow::bail!("unexpected register reply: {other:?}"),
        };

        stream
            .set_nonblocking(true)
            .context("set WM ipc stream non-blocking")?;
        tracing::info!(label, my_index, "registered on local machine-axis");
        Ok(Wm {
            stream,
            buf: Vec::new(),
            my_index,
        })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    pub fn my_index(&self) -> u8 {
        self.my_index
    }

    /// Drain every complete event line currently buffered on the connection.
    /// Returns an empty vec on `WouldBlock`; errors on a closed/garbled socket.
    pub fn drain_events(&mut self) -> Result<Vec<Event>> {
        let mut tmp = [0u8; 4096];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => anyhow::bail!("local WM closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("read local WM events"),
            }
        }
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let trimmed = &line[..line.len() - 1];
            if trimmed.is_empty() {
                continue;
            }
            match std::str::from_utf8(trimmed) {
                Ok(s) => match serde_json::from_str::<Event>(s) {
                    Ok(e) => events.push(e),
                    Err(e) => tracing::warn!(error = ?e, line = s, "ignoring malformed WM event"),
                },
                Err(_) => tracing::warn!("WM event line not valid utf-8; skipping"),
            }
        }
        Ok(events)
    }
}

/// Send a one-shot request to the local WM on a throwaway blocking connection
/// and return the parsed reply. Used for clipboard brokering — separate from the
/// long-lived non-blocking subscriber so a request/reply can't tangle with the
/// event drain.
fn one_shot(req: &Request) -> Result<Response> {
    let path = client_socket_path()
        .context("could not resolve $SHOESTRING_WM_SOCKET for clipboard request")?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))?;
    let line = serde_json::to_string(req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let reply = read_line(&mut stream)?.context("WM closed before clipboard reply")?;
    serde_json::from_str(&reply).context("parse clipboard reply")
}

/// Read the local WM's selection (clipboard, or primary). Returns the chosen
/// mime + bytes (`None` mime = empty selection). The viewer half of a **Push**.
pub fn get_clipboard(primary: bool) -> Result<(Option<String>, Vec<u8>)> {
    match one_shot(&Request::GetClipboard { primary })? {
        Response::Clipboard { mime, data } => Ok((mime, data)),
        Response::Error { message } => anyhow::bail!("WM error: {message}"),
        other => anyhow::bail!("unexpected get_clipboard reply: {other:?}"),
    }
}

/// Set the local WM's selection to `data` under `mime`. The viewer half of a
/// **Pull** (installing what the remote sent us).
pub fn set_clipboard(primary: bool, mime: String, data: Vec<u8>) -> Result<()> {
    match one_shot(&Request::SetClipboard {
        primary,
        mime,
        data,
    })? {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("WM error: {message}"),
        other => anyhow::bail!("unexpected set_clipboard reply: {other:?}"),
    }
}

/// Read one newline-delimited line (blocking — used once, before the stream is
/// switched to non-blocking). `None` on EOF before any byte.
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
                }
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(String::from_utf8(buf)?));
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
