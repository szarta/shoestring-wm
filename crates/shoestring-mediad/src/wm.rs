//! Push a media snapshot to the WM over its newline-JSON IPC socket. One
//! request per connection (the WM IPC is one-shot per connection, like every
//! other `shoestring-ctl` mutation), so each report opens, sends, drains the
//! `Ok`, and closes — reports are infrequent (a mute/camera change), so this
//! is cheap.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use shoestring_ipc::{client_socket_path, Request};

use crate::pw::Snapshot;

/// Send one `Request::ReportMedia`. Returns `Err` if the WM socket can't be
/// resolved or reached (the monitor logs and carries on — the WM may simply
/// not be up yet).
pub fn report(snapshot: Snapshot) -> Result<()> {
    let path = client_socket_path().context("resolve WM socket path")?;
    let mut stream =
        UnixStream::connect(&path).with_context(|| format!("connect {}", path.display()))?;
    let req = Request::ReportMedia {
        audio_muted: snapshot.audio_muted,
        mic_muted: snapshot.mic_muted,
        camera_active: snapshot.camera_active,
    };
    let line = serde_json::to_string(&req)?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    // Best-effort drain of the single `Response::Ok` so the WM's write doesn't
    // block; we don't parse it.
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    Ok(())
}
