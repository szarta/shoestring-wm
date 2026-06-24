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
use shoestring_ipc::{client_socket_path, Event, MediaState, Request, Response, WindowSummary};

fn connect() -> Result<UnixStream> {
    let path = client_socket_path()
        .context("could not resolve socket path; is shoestring-wm running and exporting $SHOESTRING_WM_SOCKET?")?;
    // Log the resolved socket so the live-vs-test `$SHOESTRING_WM_SOCKET`
    // confusion (a bar pointed at the wrong WM shows stale data) is visible in
    // the log. Debug-level: per-request, so it stays out of the default `info`.
    tracing::debug!(socket = %path.display(), "ipc connect");
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

/// Send `Request::Workspaces`, parse the response, return
/// `(active, count, names)`. `names` is length `count` (or empty when
/// talking to a pre-naming WM build); empty strings mean "no name set,
/// render the 1-based number instead". Connection is closed when the
/// function returns.
pub fn query_workspaces() -> Result<(u8, u8, Vec<String>)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::Workspaces)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse workspaces response")?;
    match resp {
        Response::Workspaces {
            active,
            count,
            names,
        } => Ok((active, count, names)),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::AutomationStatus`, return the current gate state.
/// Used at startup so the AUTO indicator reflects reality before any
/// `automation_changed` event would arrive.
pub fn query_automation() -> Result<bool> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::AutomationStatus)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse automation response")?;
    match resp {
        Response::Automation { enabled } => Ok(enabled),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::ScreenCaptureStatus`, return the current gate state.
/// Used at startup so the capture indicator reflects reality before any
/// `screen_capture_changed` event would arrive.
pub fn query_screen_capture() -> Result<bool> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::ScreenCaptureStatus)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse screen-capture response")?;
    match resp {
        Response::ScreenCapture { enabled } => Ok(enabled),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::MediaStatus`, return the current media-privacy snapshot
/// (sink/mic mute + camera-in-use), or `None` when no `shoestring-mediad`
/// monitor has reported yet. Used at startup to seed the MUTE/MIC/CAM chips
/// before the first `media_changed` event would arrive.
pub fn query_media() -> Result<Option<MediaState>> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::MediaStatus)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse media response")?;
    match resp {
        Response::Media { state } => Ok(state),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::Windows`, return the summary list. Used at startup to
/// bootstrap per-window state (currently: workspace assignment) for any
/// windows that already existed when the bar attached — `window_opened`
/// events are not replayed on subscribe.
pub fn query_windows() -> Result<Vec<WindowSummary>> {
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

/// Fire-and-forget `Request::FocusWindow { id }` on a fresh one-shot
/// connection. Returns `Ok(())` when the WM acks with `Response::Ok`.
/// Surfaces server `Error` (e.g. window already gone) as an `Err` so
/// the caller can log it.
pub fn request_focus_window(id: &str) -> Result<()> {
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

/// Set the runtime screen-capture gate (`Request::SetScreenCapture`). Ungated.
/// The bar doesn't use the returned state — an `Event::ScreenCaptureChanged`
/// follows when it actually flips and the event loop updates the indicator.
pub fn set_screen_capture(enabled: bool) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::SetScreenCapture { enabled })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response =
        serde_json::from_str(&line).context("parse set_screen_capture response")?;
    match resp {
        Response::ScreenCapture { .. } | Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Set the runtime automation gate (`Request::SetAutomation`). Ungated.
/// `Event::AutomationChanged` (and possibly `ScreenCaptureChanged`, since
/// automation is a capture superset) follows when it flips.
pub fn set_automation(enabled: bool) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::SetAutomation { enabled })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse set_automation response")?;
    match resp {
        Response::Automation { .. } | Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Send `Request::RemoteStatus`, return `(enabled, server_available, viewers)`.
/// Used at startup so the remote toggle/chip reflect reality before the first
/// `remote_changed` event.
pub fn query_remote() -> Result<(bool, bool, u32)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::RemoteStatus)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse remote response")?;
    match resp {
        Response::Remote {
            enabled,
            server_available,
            viewers,
        } => Ok((enabled, server_available, viewers)),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Query the machine-axis view (`Request::RemoteClientStatus`). Returns the
/// active view index (0 = local) and that machine's label (`None` for local),
/// seeding the "driving <machine>" chip; a `view_changed` event corrects us.
pub fn query_remote_clients() -> Result<(u8, Option<String>)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::RemoteClientStatus)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse remote_clients response")?;
    match resp {
        Response::RemoteClients { machines, active } => {
            let label = active
                .checked_sub(1)
                .and_then(|i| machines.get(i as usize))
                .map(|m| m.label.clone());
            Ok((active, label))
        }
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Set the runtime remote gate (`Request::SetRemote`). A `remote_changed` event
/// (and coupled `automation_changed` / `screen_capture_changed`) follows when
/// it flips; enabling is refused by the WM unless a server has registered.
pub fn set_remote(enabled: bool) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::SetRemote { enabled })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse set_remote response")?;
    match resp {
        Response::Remote { .. } | Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Mute/unmute the default audio output (`Request::SetAudioMute`). The WM
/// delegates to `shoestring-mediad`; the resulting live state arrives later as
/// an `Event::MediaChanged`, which the event loop applies to the chips.
pub fn set_audio_mute(enabled: bool) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::SetAudioMute { enabled })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse set_audio_mute response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Mute/unmute the default microphone (`Request::SetMicMute`). Mic analogue of
/// [`set_audio_mute`].
pub fn set_mic_mute(enabled: bool) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::SetMicMute { enabled })?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse set_mic_mute response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Lock the session (`Request::Lock`). Ungated; spawns the WM's lock binary.
pub fn lock() -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::Lock)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse lock response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Quit the session (`Request::Quit`). Ungated; pops the WM's confirm dialog,
/// so this returns once the dialog is shown — the user still has to accept it.
pub fn quit() -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::Quit)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response = serde_json::from_str(&line).context("parse quit response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("server error: {message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

/// Power off the machine (`Request::PowerOff`). Ungated; pops the WM's
/// confirm dialog, so this returns once the dialog is shown — the user still
/// has to accept it. The WM owns the actual `systemctl`/`loginctl`/`shutdown`
/// fallback chain.
pub fn power_off() -> Result<()> {
    one_shot_ok(&Request::PowerOff, "power-off")
}

/// Reboot the machine (`Request::Reboot`). Same confirm-then-act flow as
/// [`power_off`].
pub fn reboot() -> Result<()> {
    one_shot_ok(&Request::Reboot, "reboot")
}

/// Suspend the machine (`Request::Suspend`). Same confirm-then-act flow as
/// [`power_off`].
pub fn suspend() -> Result<()> {
    one_shot_ok(&Request::Suspend, "suspend")
}

/// Send a one-shot request that expects a bare [`Response::Ok`], mapping a
/// server-side error to an `Err`. Shared by the fire-and-confirm controls
/// (`PowerOff`/`Reboot`/`Suspend`); `what` names the request for context.
fn one_shot_ok(req: &Request, what: &str) -> Result<()> {
    let mut stream = connect()?;
    write_request(&mut stream, req)?;
    let line = read_line(&mut stream)?.context("server closed before responding")?;
    let resp: Response =
        serde_json::from_str(&line).with_context(|| format!("parse {what} response"))?;
    match resp {
        Response::Ok => Ok(()),
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
    if let Some(path) = client_socket_path() {
        tracing::info!(socket = %path.display(), "ipc event stream open");
    }
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
