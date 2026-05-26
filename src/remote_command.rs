//! WM-side helper for `Request::RunCommand`: spawn a child under the
//! WM's environment, capture bounded stdout/stderr non-blocking via
//! calloop pipe sources, optionally SIGKILL on a calloop `Timer`, and
//! send [`Response::CommandResult`] once the child has been reaped.
//!
//! The shape and lifecycle mirror [`crate::remote_screenshot`] — the
//! difference is configurable `argv`, the optional timeout, and a hard
//! per-stream output cap. Past the cap we keep draining the pipe (so
//! the child does not block on a full pipe buffer) but discard the
//! bytes and set the response's `truncated` flag.

use std::{
    cell::RefCell,
    io::Read,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    process::{Child, Stdio},
    rc::Rc,
    time::Duration,
};

use anyhow::{Context, Result};
use shoestring_ipc::{Response, RUN_COMMAND_OUTPUT_CAP};
use smithay::reexports::calloop::{
    generic::Generic,
    timer::{TimeoutAction, Timer},
    Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};

use crate::state::ShoestringWm;

pub struct Pending {
    pub client: Rc<RefCell<crate::ipc::Client>>,
    pub client_id: crate::ipc::ClientId,
    child: Child,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_open: bool,
    stderr_open: bool,
    /// Set to `true` once either stream has exceeded
    /// [`RUN_COMMAND_OUTPUT_CAP`]; surfaced as `truncated` in the
    /// response so callers can distinguish bounded output from a real
    /// short result.
    truncated: bool,
    stdout_token: Option<RegistrationToken>,
    stderr_token: Option<RegistrationToken>,
    /// Set when a timeout was configured. Removed when the response is
    /// finalised (whether the timer fired or the child finished first).
    timer_token: Option<RegistrationToken>,
}

struct PipeFd<T: AsRawFd> {
    inner: T,
}

impl<T: AsRawFd> AsFd for PipeFd<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: see remote_screenshot::PipeFd — calloop owns the
        // Generic which owns this wrapper which owns the ChildStdout /
        // ChildStderr that backs the fd.
        unsafe { BorrowedFd::borrow_raw(self.inner.as_raw_fd()) }
    }
}

impl ShoestringWm {
    /// Kick off the child and register the deferred-response machinery.
    /// On spawn failure the caller is expected to reply with
    /// `Response::Error` directly — no `Pending` is recorded.
    pub fn spawn_remote_command(
        &mut self,
        client_id: crate::ipc::ClientId,
        client: Rc<RefCell<crate::ipc::Client>>,
        argv: &[String],
        timeout_ms: Option<u32>,
    ) -> Result<()> {
        anyhow::ensure!(!argv.is_empty(), "run_command: argv must be non-empty");

        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", &argv[0]))?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout must exist on spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr must exist on spawn");

        let pending_id = self.next_remote_command_id();
        let pending = Pending {
            client,
            client_id,
            child,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            stdout_open: true,
            stderr_open: true,
            truncated: false,
            stdout_token: None,
            stderr_token: None,
            timer_token: None,
        };
        self.pending_commands.insert(pending_id, pending);

        let stdout_token = register_pipe(&self.loop_handle, stdout, pending_id, PipeKind::Stdout)?;
        let stderr_token = register_pipe(&self.loop_handle, stderr, pending_id, PipeKind::Stderr)?;
        if let Some(p) = self.pending_commands.get_mut(&pending_id) {
            p.stdout_token = Some(stdout_token);
            p.stderr_token = Some(stderr_token);
        }

        if let Some(ms) = timeout_ms {
            let timer = Timer::from_duration(Duration::from_millis(ms as u64));
            let timer_token = self
                .loop_handle
                .insert_source(timer, move |_, _, state| {
                    state.on_command_timeout(pending_id);
                    TimeoutAction::Drop
                })
                .map_err(|e| anyhow::anyhow!("insert run_command timer: {e}"))?;
            if let Some(p) = self.pending_commands.get_mut(&pending_id) {
                p.timer_token = Some(timer_token);
            }
        }

        tracing::info!(?argv, ?timeout_ms, "remote command started");
        Ok(())
    }

    fn next_remote_command_id(&mut self) -> u64 {
        let id = self.next_command_id;
        self.next_command_id = self.next_command_id.wrapping_add(1);
        id
    }

    /// Pipe source callback: one of stdout/stderr hit EOF. When both
    /// are drained, reap the child and reply.
    fn on_command_pipe_closed(&mut self, pending_id: u64, kind: PipeKind) {
        let done = match self.pending_commands.get_mut(&pending_id) {
            Some(p) => {
                match kind {
                    PipeKind::Stdout => p.stdout_open = false,
                    PipeKind::Stderr => p.stderr_open = false,
                }
                !p.stdout_open && !p.stderr_open
            }
            None => return,
        };
        if done {
            self.finalize_remote_command(pending_id);
        }
    }

    /// Timer fired before the child exited. SIGKILL it; the pipe
    /// sources will then drain the remaining buffered output and EOF,
    /// at which point `finalize_remote_command` runs as normal.
    fn on_command_timeout(&mut self, pending_id: u64) {
        let Some(p) = self.pending_commands.get_mut(&pending_id) else {
            return;
        };
        // Std's Child::kill sends SIGKILL on Unix. We log but ignore
        // errors: ESRCH just means the child already exited between
        // the timer firing and us reaching here.
        if let Err(e) = p.child.kill() {
            tracing::debug!(error = %e, "run_command kill (child already exited?)");
        } else {
            tracing::info!(?pending_id, "run_command timed out → SIGKILL");
        }
        p.timer_token = None;
    }

    fn finalize_remote_command(&mut self, pending_id: u64) {
        let Some(mut pending) = self.pending_commands.remove(&pending_id) else {
            return;
        };
        if let Some(t) = pending.stdout_token.take() {
            self.loop_handle.remove(t);
        }
        if let Some(t) = pending.stderr_token.take() {
            self.loop_handle.remove(t);
        }
        if let Some(t) = pending.timer_token.take() {
            self.loop_handle.remove(t);
        }

        let exit_code = match pending.child.wait() {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) => {
                tracing::warn!(error = %e, "wait run_command failed");
                let _ = crate::ipc::write_response(
                    &pending.client,
                    &Response::Error {
                        message: format!("run_command: wait failed: {e}"),
                    },
                );
                self.drop_ipc_client(pending.client_id);
                return;
            }
        };

        let stdout = String::from_utf8_lossy(&pending.stdout_buf).into_owned();
        let stderr = String::from_utf8_lossy(&pending.stderr_buf).into_owned();
        let response = Response::CommandResult {
            exit_code,
            stdout,
            stderr,
            truncated: pending.truncated,
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
            // SAFETY: see remote_screenshot::register_pipe.
            let pipe = unsafe { pipe.get_mut() };
            let mut buf = [0u8; 4096];
            loop {
                match pipe.inner.read(&mut buf) {
                    Ok(0) => {
                        state.on_command_pipe_closed(pending_id, kind);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => {
                        let Some(p) = state.pending_commands.get_mut(&pending_id) else {
                            // The pending entry can disappear if we
                            // already finalised (e.g. the other pipe
                            // closed first and the child exited
                            // synchronously) — keep draining the fd
                            // so the kernel can close it, then bail.
                            continue;
                        };
                        let target = match kind {
                            PipeKind::Stdout => &mut p.stdout_buf,
                            PipeKind::Stderr => &mut p.stderr_buf,
                        };
                        let room = RUN_COMMAND_OUTPUT_CAP.saturating_sub(target.len());
                        if room == 0 {
                            // Already at cap. Drop the bytes but flag.
                            p.truncated = true;
                        } else if n <= room {
                            target.extend_from_slice(&buf[..n]);
                        } else {
                            target.extend_from_slice(&buf[..room]);
                            p.truncated = true;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, ?kind, "run_command pipe read failed");
                        state.on_command_pipe_closed(pending_id, kind);
                        return Ok(PostAction::Remove);
                    }
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("insert run_command pipe source: {e}"))
}
