//! Thin client for the WM's newline-JSON IPC socket.
//!
//! Every helper here speaks to the *local* shoestring-wm over its unix socket
//! (`$SHOESTRING_WM_SOCKET`). The server uses several short-lived connections —
//! one-shot request/reply for things like `report_viewers` and `inject` — plus
//! two long-lived ones: the **registration** connection (a pure event
//! subscriber, kept open for the process lifetime so its drop forces the gate
//! off) and a per-client **capture-stream** connection (upgraded to a binary
//! `ServerMessage` frame stream).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{anyhow, bail, Context, Result};
use shoestring_ipc::{client_socket_path, RawInput, Request, Response};

/// Open a fresh connection to the WM IPC socket.
pub fn connect() -> Result<UnixStream> {
    let path = client_socket_path().context(
        "could not resolve WM socket; is shoestring-wm running and exporting \
         $SHOESTRING_WM_SOCKET?",
    )?;
    UnixStream::connect(&path).with_context(|| format!("connect to WM socket {}", path.display()))
}

/// Serialize a request as one newline-terminated JSON line.
pub fn write_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let mut line = serde_json::to_vec(req).context("encode request")?;
    line.push(b'\n');
    stream.write_all(&line).context("write request")?;
    Ok(())
}

/// Read one newline-delimited JSON line, returning `None` on clean EOF.
pub fn read_line(reader: &mut BufReader<UnixStream>) -> Result<Option<String>> {
    let mut buf = String::new();
    let n = reader.read_line(&mut buf).context("read line")?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// Send a one-shot request on a throwaway connection and return the first
/// response line, parsed. Used for `inject`, `report_viewers`, etc.
pub fn request(req: &Request) -> Result<Response> {
    let mut stream = connect()?;
    write_request(&mut stream, req)?;
    let mut reader = BufReader::new(stream);
    let line = read_line(&mut reader)?.context("WM closed before responding")?;
    serde_json::from_str(&line).context("parse WM response")
}

/// Send a one-shot request and require a bare `Ok`, surfacing a server error.
pub fn request_ok(req: &Request) -> Result<()> {
    match request(req)? {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("WM error: {message}"),
        other => bail!("unexpected WM response: {other:?}"),
    }
}

/// Report the live viewer count to the WM so the "being viewed" chip tracks it.
pub fn report_viewers(viewers: u32) -> Result<()> {
    request_ok(&Request::ReportRemoteViewers { viewers })
}

/// Replay one raw input transition into the WM. The WM IPC is one
/// request-per-connection for one-shot requests (it hangs up after replying),
/// so this opens a fresh connection per event — cheap on a localhost unix
/// socket, and the same pattern the bar uses. The automation gate is coupled
/// on while the remote gate is enabled, so this only fails if the gate flipped
/// off mid-session (in which case the session is ending anyway).
pub fn inject(event: RawInput) -> Result<()> {
    request_ok(&Request::InjectInput { event })
}

/// Register this process as *the* remote server and return the connection (now
/// a long-lived event subscriber) plus the gate's initial enabled state. The
/// connection must be kept alive for the whole process: its drop tells the WM
/// the server went away and forces the remote gate off.
pub fn register() -> Result<(BufReader<UnixStream>, bool)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::RegisterRemoteServer)?;
    let mut reader = BufReader::new(stream);
    let line = read_line(&mut reader)?.context("WM closed before register reply")?;
    let resp: Response = serde_json::from_str(&line).context("parse register reply")?;
    match resp {
        Response::Remote {
            enabled,
            server_available,
            ..
        } => {
            if !server_available {
                bail!("WM did not accept the server registration");
            }
            Ok((reader, enabled))
        }
        Response::Error { message } => bail!("WM refused registration: {message}"),
        other => bail!("unexpected register reply: {other:?}"),
    }
}

/// Open a capture-stream connection for `output` (None = the WM's first
/// output). Sends `capture_stream`, consumes the `Ok` ack line, and returns the
/// connection positioned at the start of the binary `ServerMessage` frames.
/// Any buffered bytes the ack-reader pulled past the newline are returned too,
/// so the caller can prepend them before reading frames.
pub fn open_capture_stream(output: Option<String>) -> Result<(UnixStream, Vec<u8>)> {
    let mut stream = connect()?;
    write_request(&mut stream, &Request::CaptureStream { output })?;

    // The ack is a single newline-JSON line; everything after the newline is
    // already binary frame data, so read byte-by-byte to avoid a BufReader
    // swallowing frame bytes we can't push back.
    let ack = read_one_json_line(&mut stream)?;
    let resp: Response = serde_json::from_str(&ack).context("parse capture_stream ack")?;
    match resp {
        Response::Ok => Ok((stream, Vec::new())),
        Response::Error { message } => bail!("capture_stream refused: {message}"),
        other => bail!("unexpected capture_stream ack: {other:?}"),
    }
}

/// Read a single newline-terminated line from a raw stream one byte at a time,
/// so we stop exactly at the `\n` and leave any following bytes in the socket.
fn read_one_json_line(stream: &mut UnixStream) -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).context("read ack byte")?;
        if n == 0 {
            return Err(anyhow!("WM closed before capture_stream ack"));
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
    }
    String::from_utf8(buf).context("ack line not UTF-8")
}
