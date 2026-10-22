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
//!
//! ## What a command is
//!
//! An attached command is its whole **process group**: the child runs in a
//! new group of its own, which the processes it starts join unless they leave
//! on purpose. The timeout kills the group, not just the child, so a shell
//! and what it started go together. The reply waits for the child to exit and
//! its output to reach EOF; a process it started that still holds the pipes
//! after it exits gets [`OUTPUT_GRACE`] to finish, then the group is killed
//! and the reply goes out with the output so far, instead of the request
//! hanging for as long as that process runs. Processes that don't hold the
//! output (redirected elsewhere) are left alone; `detach` is for anything
//! meant to outlive the request.

use std::{cell::RefCell, io::Read, os::fd::AsRawFd, process::Stdio, rc::Rc, time::Duration};

use anyhow::{Context, Result};
use shoestring_ipc::{Response, RUN_COMMAND_OUTPUT_CAP};
use smithay::reexports::calloop::{
    timer::{TimeoutAction, Timer},
    LoopHandle, RegistrationToken,
};

use crate::{
    child_pipe::{self, PipeEvent},
    state::ShoestringWm,
};

/// How long the reply waits for output to reach EOF after the child exits,
/// before killing what is left of its process group.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);

pub struct Pending {
    pub(crate) client: Rc<RefCell<crate::ipc::Client>>,
    pub client_id: crate::ipc::ClientId,
    /// PID of the child, which [`crate::child_reaper`] tracks: it reaps the
    /// child, matches its status back here, and lets a timeout kill it.
    pid: i32,
    /// Exit status, once the reaper has delivered it. `None` until then.
    exit_status: Option<std::process::ExitStatus>,
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
    /// Set once the child has exited with its pipes still open: the
    /// [`OUTPUT_GRACE`] timer that ends the wait.
    grace_token: Option<RegistrationToken>,
}

impl ShoestringWm {
    /// Kick off the child and register the deferred-response machinery.
    /// On spawn failure the caller is expected to reply with
    /// `Response::Error` directly — no `Pending` is recorded.
    pub(crate) fn spawn_remote_command(
        &mut self,
        client_id: crate::ipc::ClientId,
        client: Rc<RefCell<crate::ipc::Client>>,
        argv: &[String],
        timeout_ms: Option<u32>,
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<()> {
        anyhow::ensure!(!argv.is_empty(), "run_command: argv must be non-empty");

        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A group of its own (pgid = its pid), so the command can be
            // killed as a whole; see the module docs.
            .process_group(0);
        apply_cwd_env(&mut cmd, cwd, env);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", argv[0]))?;
        let stdout = child
            .stdout
            .take()
            .expect("piped stdout must exist on spawn");
        let stderr = child
            .stderr
            .take()
            .expect("piped stderr must exist on spawn");
        let pid = crate::child_reaper::track(child);

        let pending_id = self.next_remote_command_id();
        let pending = Pending {
            client,
            client_id,
            pid,
            exit_status: None,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            stdout_open: true,
            stderr_open: true,
            truncated: false,
            stdout_token: None,
            stderr_token: None,
            timer_token: None,
            grace_token: None,
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

    /// Spawn a `run_command` with `detach: true`: its own session (`setsid`,
    /// so it outlives the request and isn't in the WM's process group), stdio
    /// to `/dev/null`, no pipes and no pending-command bookkeeping. Returns the
    /// child PID. [`crate::child_reaper`] reaps it when it exits (and
    /// [`ShoestringWm::note_child_reaped`] lets its unmatched pid go), so it
    /// never zombies while the WM lives.
    pub(crate) fn spawn_detached_command(
        &mut self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<i32> {
        use std::os::unix::process::CommandExt;
        anyhow::ensure!(!argv.is_empty(), "run_command: argv must be non-empty");

        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_cwd_env(&mut cmd, cwd, env);
        // New session: detaches from the WM's controlling terminal / process
        // group so a signal aimed at the WM doesn't take the child with it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {:?}", argv[0]))?;
        // We deliberately do not wait on it; the reaper does.
        let pid = crate::child_reaper::track(child);
        tracing::info!(?argv, pid, "detached command started");
        Ok(pid)
    }

    fn next_remote_command_id(&mut self) -> u64 {
        let id = self.next_command_id;
        self.next_command_id = self.next_command_id.wrapping_add(1);
        id
    }

    /// Pipe source callback: one of stdout/stderr hit EOF. Marks the
    /// stream closed, then attempts to finalize.
    fn on_command_pipe_closed(&mut self, pending_id: u64, kind: PipeKind) {
        match self.pending_commands.get_mut(&pending_id) {
            Some(p) => match kind {
                PipeKind::Stdout => p.stdout_open = false,
                PipeKind::Stderr => p.stderr_open = false,
            },
            None => return,
        }
        self.maybe_finalize_remote_command(pending_id);
    }

    /// Called by the SIGCHLD drain (`note_child_reaped`) when a reaped pid
    /// matches an in-flight command. Records the status, then attempts to
    /// finalize. Returns `true` if the pid matched one of our children.
    pub fn note_command_reaped(&mut self, pid: i32, status: std::process::ExitStatus) -> bool {
        let Some((&pending_id, _)) = self.pending_commands.iter().find(|(_, p)| p.pid == pid)
        else {
            return false;
        };
        if let Some(p) = self.pending_commands.get_mut(&pending_id) {
            p.exit_status = Some(status);
        }
        self.maybe_finalize_remote_command(pending_id);
        // Still pending: the child is gone but something it started holds the
        // pipes. Give the output a moment, then stop waiting.
        if self.pending_commands.contains_key(&pending_id) {
            self.arm_output_grace(pending_id);
        }
        true
    }

    fn arm_output_grace(&mut self, pending_id: u64) {
        let token = self.loop_handle.insert_source(
            Timer::from_duration(OUTPUT_GRACE),
            move |_, _, state| {
                state.on_output_grace_over(pending_id);
                TimeoutAction::Drop
            },
        );
        match token {
            Ok(token) => {
                if let Some(p) = self.pending_commands.get_mut(&pending_id) {
                    p.grace_token = Some(token);
                }
            }
            // Without the timer the reply waits for EOF, as it always did.
            Err(e) => tracing::warn!(error = %e, "insert run_command grace timer"),
        }
    }

    /// The child exited [`OUTPUT_GRACE`] ago and its pipes are still open:
    /// kill what is left of its process group and reply with what we have.
    fn on_output_grace_over(&mut self, pending_id: u64) {
        let Some(p) = self.pending_commands.get_mut(&pending_id) else {
            return;
        };
        p.grace_token = None;
        tracing::info!(
            ?pending_id,
            "run_command exited but its output is still held open; killing its process group"
        );
        kill_group(p.pid);
        self.finalize_remote_command(pending_id);
    }

    /// Append a chunk of child output, up to [`RUN_COMMAND_OUTPUT_CAP`] per
    /// stream. Past the cap the bytes are dropped (the pipe keeps draining so
    /// the child never blocks on a full buffer) and `truncated` is set. The
    /// pending entry can already be gone if we finalised early; the bytes
    /// are discarded then.
    fn on_command_output(&mut self, pending_id: u64, kind: PipeKind, bytes: &[u8]) {
        let Some(p) = self.pending_commands.get_mut(&pending_id) else {
            return;
        };
        let target = match kind {
            PipeKind::Stdout => &mut p.stdout_buf,
            PipeKind::Stderr => &mut p.stderr_buf,
        };
        let room = RUN_COMMAND_OUTPUT_CAP.saturating_sub(target.len());
        if bytes.len() > room {
            p.truncated = true;
        }
        target.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// Timer fired before the reply went out. SIGKILL the command's whole
    /// process group; the pipe sources then drain the remaining buffered
    /// output and EOF, and the reply goes out as normal.
    fn on_command_timeout(&mut self, pending_id: u64) {
        let Some(p) = self.pending_commands.get_mut(&pending_id) else {
            return;
        };
        p.timer_token = None;
        tracing::info!(
            ?pending_id,
            "run_command timed out → SIGKILL its process group"
        );
        kill_group(p.pid);
    }

    /// Reply only once both pipes have drained *and* the exit status has
    /// arrived from the reaper. Whichever of `on_command_pipe_closed` /
    /// `note_command_reaped` lands last is the one that finalizes.
    fn maybe_finalize_remote_command(&mut self, pending_id: u64) {
        let ready = match self.pending_commands.get(&pending_id) {
            Some(p) => !p.stdout_open && !p.stderr_open && p.exit_status.is_some(),
            None => return,
        };
        if ready {
            self.finalize_remote_command(pending_id);
        }
    }

    /// Send the reply with whatever output has arrived, and stop watching the
    /// command: pipe sources, timers and all.
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
        if let Some(t) = pending.grace_token.take() {
            self.loop_handle.remove(t);
        }

        // SIGKILL on timeout (and other signal deaths) yield no exit code;
        // -1 preserves the prior sentinel for "killed / no code".
        let exit_code = pending
            .exit_status
            .and_then(|status| status.code())
            .unwrap_or(-1);

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

/// SIGKILL every process in the group `pgid` (the command's child pid, see
/// `process_group(0)`). ESRCH — the group is already empty — is expected and
/// ignored. Safe to call after the child is reaped: a pid isn't handed out
/// again while a process group of that id still has members, and an empty
/// group just gets ESRCH.
fn kill_group(pgid: i32) {
    // SAFETY: plain syscall; a positive pgid names exactly one group.
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            tracing::warn!(error = %e, pgid, "run_command: killpg failed");
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

/// Apply a `run_command`'s optional `cwd` and extra `env` pairs to a
/// [`Command`](std::process::Command). The env pairs layer onto the WM's
/// inherited environment (they don't clear it), so `WAYLAND_DISPLAY` etc.
/// survive; a repeated key takes its last value.
fn apply_cwd_env(cmd: &mut std::process::Command, cwd: Option<&str>, env: &[(String, String)]) {
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
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
    let label = match kind {
        PipeKind::Stdout => "run_command stdout",
        PipeKind::Stderr => "run_command stderr",
    };
    child_pipe::register(
        loop_handle,
        stream,
        label,
        move |state, event| match event {
            PipeEvent::Data(bytes) => state.on_command_output(pending_id, kind, bytes),
            PipeEvent::Closed => state.on_command_pipe_closed(pending_id, kind),
        },
    )
}
