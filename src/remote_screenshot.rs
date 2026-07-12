//! WM-side helper for `Request::Screenshot`: spawns `shoestring-screenshot`
//! as a child process, captures its stdout/stderr asynchronously via
//! calloop pipe sources, and sends the deferred IPC response when the
//! child exits.
//!
//! ## Why a subprocess
//!
//! shoestring-screenshot already implements the full wlr-screencopy
//! client (output binding, format negotiation, ARGB → RGBA, PNG encode).
//! Re-running that logic inside the WM would duplicate ~300 lines for
//! the rare case of IPC-initiated capture; spawning the binary keeps a
//! single source of truth.
//!
//! ## Why non-blocking
//!
//! The child connects back to the *same* WM that spawned it and sends
//! screencopy requests. If the WM blocked on `wait()`, the child would
//! sit forever waiting for a compositor response that never comes —
//! classic self-deadlock. We register stdout/stderr as calloop `Generic`
//! READ sources so the WM keeps servicing wayland events while the child
//! is alive, and only finalise the IPC response when the pipes close.

use std::{
    cell::RefCell,
    io::Read,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    path::PathBuf,
    process::Stdio,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use shoestring_ipc::{Response, ScreenshotRegion};
use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};

use crate::state::ShoestringWm;

/// In-flight subprocess. Completion needs two independent signals: both
/// pipes reaching EOF (output fully drained) *and* the child's exit status
/// arriving from the global SIGCHLD reaper (see `main::install_sigchld_autoreap`).
/// Whichever lands last triggers `maybe_finalize`. We do not `wait()` the
/// child ourselves — the reaper already collected it, so a direct wait would
/// race to ECHILD; the status is delivered via `note_screenshot_reaped`.
pub struct Pending {
    /// IPC client whose `Request::Screenshot` we're answering. Held by
    /// `Rc` so the connection stays alive across calloop iterations
    /// without being re-borrowed from the IPC server.
    pub(crate) client: Rc<RefCell<crate::ipc::Client>>,
    pub client_id: crate::ipc::ClientId,
    /// PID of the spawned `shoestring-screenshot`, used to match the
    /// reaped-status record from the SIGCHLD drain.
    pid: i32,
    /// Exit status, once the reaper has delivered it. `None` until then.
    exit_status: Option<std::process::ExitStatus>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_open: bool,
    stderr_open: bool,
    stdout_token: Option<RegistrationToken>,
    stderr_token: Option<RegistrationToken>,
    /// Destination the child was told to `--file` to; the finalized reply
    /// reports it directly rather than re-parsing the child's printed path.
    dest: PathBuf,
}

/// Generic<T> wrapper that satisfies AsFd on a stdout/stderr pipe. We
/// can't use ChildStdout/ChildStderr directly because they don't
/// implement AsFd in the way calloop expects across versions.
struct PipeFd<T: AsRawFd> {
    inner: T,
}

impl<T: AsRawFd> AsFd for PipeFd<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: ChildStdout/ChildStderr own the underlying pipe fd
        // and outlive the source (calloop owns the Generic, which owns
        // the PipeFd, which owns the Child*). The raw fd is valid for
        // the lifetime of `self`.
        unsafe { BorrowedFd::borrow_raw(self.inner.as_raw_fd()) }
    }
}

impl ShoestringWm {
    /// Build the auto-generated PNG path. `Screenshot-AUTO-<ts>.png`
    /// under `$XDG_PICTURES_DIR` (or `$HOME/Pictures`, or `.` as a last
    /// resort). The AUTO infix is per the task spec so user-triggered
    /// and agent-triggered captures don't visually collide in a file
    /// browser.
    pub(crate) fn auto_screenshot_path() -> PathBuf {
        let dir = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Pictures")))
            .unwrap_or_else(|| PathBuf::from("."));
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (y, m, day, h, min, s) = unix_to_civil(secs as i64);
        let stamp = format!("{y:04}{m:02}{day:02}-{h:02}{min:02}{s:02}");
        dir.join(format!("Screenshot-AUTO-{stamp}.png"))
    }

    /// Kick off `shoestring-screenshot` and register the deferred-response
    /// machinery. Returns `Ok(path)` once spawn succeeds; the path is the
    /// destination we asked the child to write to. The eventual IPC reply
    /// reports the same path on success.
    pub(crate) fn spawn_remote_screenshot(
        &mut self,
        client_id: crate::ipc::ClientId,
        client: Rc<RefCell<crate::ipc::Client>>,
        output: Option<&str>,
        region: Option<ScreenshotRegion>,
        path: Option<PathBuf>,
    ) -> Result<()> {
        // Write to the requested path (or the auto-generated one), creating
        // parent dirs first. `screenshot --stdout` supplies a tmpfs path here
        // and streams it client-side, so no PNG blob crosses the IPC socket.
        let dest = path.unwrap_or_else(Self::auto_screenshot_path);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create screenshot dir {}", parent.display()))?;
            }
        }
        let mut cmd = std::process::Command::new("shoestring-screenshot");
        cmd.arg("--file").arg(&dest);
        if let Some(name) = output {
            cmd.arg("--output").arg(name);
        }
        if let Some(r) = region {
            cmd.arg("--region-rect")
                .arg(format!("{},{},{},{}", r.x, r.y, r.w, r.h));
        }
        // shoestring-screenshot needs WAYLAND_DISPLAY; the WM exports it
        // in main.rs before this point, so the inherited env is fine.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("spawn shoestring-screenshot")?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout must exist on spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr must exist on spawn");
        let pid = child.id() as i32;
        // The global SIGCHLD reaper owns reaping from here on; we only need
        // the pid to match its status record. Dropping `child` without
        // waiting is safe (std's Child::drop neither waits nor kills).
        drop(child);

        let pending_id = self.next_remote_screenshot_id();

        let pending = Pending {
            client,
            client_id,
            pid,
            exit_status: None,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            stdout_open: true,
            stderr_open: true,
            stdout_token: None,
            stderr_token: None,
            dest: dest.clone(),
        };
        self.pending_screenshots.insert(pending_id, pending);

        let stdout_token = register_pipe(&self.loop_handle, stdout, pending_id, PipeKind::Stdout)?;
        let stderr_token = register_pipe(&self.loop_handle, stderr, pending_id, PipeKind::Stderr)?;
        if let Some(p) = self.pending_screenshots.get_mut(&pending_id) {
            p.stdout_token = Some(stdout_token);
            p.stderr_token = Some(stderr_token);
        }

        tracing::info!(?dest, "remote screenshot started");
        Ok(())
    }

    fn next_remote_screenshot_id(&mut self) -> u64 {
        let id = self.next_screenshot_id;
        self.next_screenshot_id = self.next_screenshot_id.wrapping_add(1);
        id
    }

    /// Called by the pipe sources when one of stdout/stderr hits EOF.
    /// Marks the stream closed, then attempts to finalize.
    fn on_pipe_closed(&mut self, pending_id: u64, kind: PipeKind) {
        match self.pending_screenshots.get_mut(&pending_id) {
            Some(p) => match kind {
                PipeKind::Stdout => p.stdout_open = false,
                PipeKind::Stderr => p.stderr_open = false,
            },
            None => return,
        }
        self.maybe_finalize_remote_screenshot(pending_id);
    }

    /// Called by the SIGCHLD drain (`note_child_reaped`) when a reaped pid
    /// matches an in-flight screenshot. Records the status, then attempts to
    /// finalize. Returns `true` if the pid matched one of our children.
    pub fn note_screenshot_reaped(&mut self, pid: i32, status: std::process::ExitStatus) -> bool {
        let Some((&pending_id, _)) = self.pending_screenshots.iter().find(|(_, p)| p.pid == pid)
        else {
            return false;
        };
        if let Some(p) = self.pending_screenshots.get_mut(&pending_id) {
            p.exit_status = Some(status);
        }
        self.maybe_finalize_remote_screenshot(pending_id);
        true
    }

    /// Reply only once both pipes have drained *and* the exit status has
    /// arrived from the reaper. Whichever of `on_pipe_closed` /
    /// `note_screenshot_reaped` lands last is the one that finalizes.
    fn maybe_finalize_remote_screenshot(&mut self, pending_id: u64) {
        let ready = match self.pending_screenshots.get(&pending_id) {
            Some(p) => !p.stdout_open && !p.stderr_open && p.exit_status.is_some(),
            None => return,
        };
        if !ready {
            return;
        }
        let mut pending = self.pending_screenshots.remove(&pending_id).unwrap();
        if let Some(t) = pending.stdout_token.take() {
            self.loop_handle.remove(t);
        }
        if let Some(t) = pending.stderr_token.take() {
            self.loop_handle.remove(t);
        }

        let status = pending.exit_status.expect("ready implies status present");

        let response = if status.success() {
            // We know the destination we passed via --file; report it directly
            // rather than re-parsing the child's printed path.
            Response::Screenshot {
                path: pending.dest.to_string_lossy().into_owned(),
            }
        } else {
            let stderr = String::from_utf8_lossy(&pending.stderr_buf)
                .trim()
                .to_string();
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "<signal>".into());
            let msg = if stderr.is_empty() {
                format!("screenshot: child exited {code}")
            } else {
                format!("screenshot: child exited {code}: {stderr}")
            };
            Response::Error { message: msg }
        };

        let _ = crate::ipc::write_response(&pending.client, &response);
        self.drop_ipc_client(pending.client_id);
    }
}

#[derive(Debug, Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

fn register_pipe<T>(
    loop_handle: &LoopHandle<'static, ShoestringWm>,
    stream: T,
    pending_id: u64,
    kind: PipeKind,
) -> Result<RegistrationToken>
where
    T: AsRawFd + Read + 'static,
{
    let source = Generic::new(PipeFd { inner: stream }, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(source, move |_, pipe, state| {
            // SAFETY: the calloop source owns the PipeFd; we never drop
            // the FD out from under it. get_mut gives us the inner T
            // through the NoIoDrop wrapper so we can call Read::read.
            let pipe = unsafe { pipe.get_mut() };
            let mut buf = [0u8; 4096];
            loop {
                match pipe.inner.read(&mut buf) {
                    Ok(0) => {
                        state.on_pipe_closed(pending_id, kind);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => {
                        if let Some(p) = state.pending_screenshots.get_mut(&pending_id) {
                            match kind {
                                PipeKind::Stdout => p.stdout_buf.extend_from_slice(&buf[..n]),
                                PipeKind::Stderr => p.stderr_buf.extend_from_slice(&buf[..n]),
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, ?kind, "screenshot pipe read failed");
                        state.on_pipe_closed(pending_id, kind);
                        return Ok(PostAction::Remove);
                    }
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("insert screenshot pipe source: {e}"))
}

// Howard Hinnant's date algorithm — duplicated from shoestring-screenshot
// to avoid spinning up a shared crate for a single fn. UTC; we only need a
// sortable filename, not timezone-correct labels.
fn unix_to_civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let h = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i32) + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, min, s)
}
