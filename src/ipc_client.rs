//! Tiny IPC client glue: opens a unix-socket connection to the WM, sends a
//! single newline-terminated JSON [`Request`], and parses [`Event`] lines.
//!
//! Two-call shape:
//! - [`query_active_workspace`] is a one-shot used at startup to seed the
//!   bar with the current active workspace before the first
//!   `workspace_changed` event would arrive.
//! - [`open_event_stream`] returns an [`EventStream`] holding the open
//!   unix socket plus a partial-line buffer; the main loop polls its fd
//!   alongside the wayland fd and drains via [`EventStream::drain_events`].

use std::{
    io::{ErrorKind, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
};

use anyhow::{Context, Result};
use shoestring_ipc::{client_socket_path, Event, Request, Response};

fn connect() -> Result<UnixStream> {
    let path = client_socket_path()
        .context("could not resolve socket path; is shoestring-wm running and exporting $SHOESTRING_WM_SOCKET?")?;
    UnixStream::connect(&path).with_context(|| format!("connect to {}", path.display()))
}

fn write_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let line = serde_json::to_string(req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Read exactly one newline-delimited line. Returns `None` on EOF before any
/// byte was seen.
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

/// Send `Request::Workspaces`, parse the response, return `(active, count)`
/// (both 1-based-friendly: `count` is the total). Connection is closed
/// when the function returns.
pub fn query_workspaces() -> Result<(u8, u8)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::Workspaces)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse workspaces response")?;
    match resp {
        Response::Workspaces { active, count } => Ok((active, count)),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Open a streaming subscription. Sends `Request::EventStream`, reads the
/// initial `Response::Ok`, then flips the socket to non-blocking so the
/// caller can poll it.
pub fn open_event_stream() -> Result<EventStream> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::EventStream)?;
    let line = read_line(&mut stream)?.context("server closed before ack")?;
    let resp: Response = serde_json::from_str(&line).context("parse event-stream ack")?;
    match resp {
        Response::Ok => {}
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected ack: {other:?}"),
    }
    stream
        .set_nonblocking(true)
        .context("set event stream non-blocking")?;
    Ok(EventStream {
        stream,
        buf: Vec::new(),
    })
}

pub struct EventStream {
    stream: UnixStream,
    /// Partial line we haven't seen a `\n` for yet.
    buf: Vec<u8>,
}

impl EventStream {
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.as_raw_fd()
    }

    /// Read every byte currently available on the socket and return every
    /// fully-formed [`Event`] line. Returns `Err` on socket close — the
    /// caller should drop the stream.
    pub fn drain_events(&mut self) -> Result<Vec<Event>> {
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => anyhow::bail!("server closed the event stream"),
                Ok(n) => self.buf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut events = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            // Skip the trailing '\n' when parsing.
            let trimmed = &line[..line.len() - 1];
            if trimmed.is_empty() {
                continue;
            }
            let s = std::str::from_utf8(trimmed).context("event line is not valid utf-8")?;
            match serde_json::from_str::<Event>(s) {
                Ok(e) => events.push(e),
                Err(e) => tracing::warn!(error = ?e, line = s, "ignoring malformed event"),
            }
        }
        Ok(events)
    }
}
