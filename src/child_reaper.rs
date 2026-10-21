//! Reaping of the WM's own helper children.
//!
//! The WM spawns fire-and-forget helpers (autostart clients, binding spawns,
//! the lock command, `shoestring-mediad`, power tools, remote commands and
//! screenshots) and keeps no one around to `wait()` on them. Something has to,
//! or they sit as `<defunct>` in `ps` for the life of the session. This module
//! does that for exactly the children handed to [`track`], and no others.
//!
//! It must not reap any other child. Several waiters in the process expect to
//! collect their own child: `Command::spawn` waits on a child whose `exec`
//! failed and asserts that the wait succeeds, smithay waits on Xwayland, and
//! `Command::status`/`output` wait on theirs. A reaper that calls
//! `waitpid(-1)` steals those children and makes the waits fail with `ECHILD`.
//! In `spawn` that is a panic. On a machine without an `Xwayland` binary, it
//! took the WM down at startup about once in 200 starts.
//!
//! So the SIGCHLD handler only writes a byte to a self-pipe. The calloop
//! source added by [`register`] wakes on it and, on the main thread, calls
//! `try_wait` (a `waitpid` on that one pid) for each tracked child, routing
//! each exit status to [`ShoestringWm::note_child_reaped`].

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

use smithay::reexports::calloop::{generic::Generic, Interest, LoopHandle, Mode, PostAction};

use crate::state::ShoestringWm;

/// Write end of the SIGCHLD self-pipe; `-1` until [`install`] sets it.
static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Children the WM spawned and dropped, waiting to be reaped. Never touched
/// from the signal handler.
static TRACKED: Mutex<Vec<Child>> = Mutex::new(Vec::new());

fn tracked() -> MutexGuard<'static, Vec<Child>> {
    // A panic while holding the lock leaves the list itself intact.
    TRACKED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Hand a spawned child to the reaper and return its pid. Use this for any
/// child the caller will not `wait()` on itself; a child the caller waits on
/// must not be tracked.
pub fn track(child: Child) -> i32 {
    let pid = child.id() as i32;
    tracked().push(child);
    pid
}

/// SIGKILL a tracked child. Fails with `ESRCH` once the child has been
/// reaped, so a recycled pid is never signalled.
pub fn kill(pid: i32) -> std::io::Result<()> {
    match tracked().iter_mut().find(|c| c.id() as i32 == pid) {
        Some(child) => child.kill(),
        None => Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
    }
}

/// Reap every tracked child that has exited, returning `(pid, status)` for
/// each. Children still running stay tracked.
pub fn sweep() -> Vec<(i32, ExitStatus)> {
    let mut reaped = Vec::new();
    tracked().retain_mut(|child| match child.try_wait() {
        Ok(Some(status)) => {
            reaped.push((child.id() as i32, status));
            false
        }
        Ok(None) => true,
        Err(e) => {
            // Only ECHILD is possible here, and it means the child is gone.
            tracing::warn!(pid = child.id(), error = %e, "reaper: try_wait failed; dropping child");
            false
        }
    });
    reaped
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "freebsd")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

/// Install the SIGCHLD handler and create its self-pipe. Returns the pipe's
/// read end for [`register`], or `None` if the pipe could not be created (the
/// handler is then not installed and tracked children are never reaped).
/// Call once, before spawning anything.
pub fn install() -> Option<OwnedFd> {
    // O_NONBLOCK so a full pipe never blocks the handler (a full pipe already
    // holds a pending wakeup); O_CLOEXEC so children don't inherit it.
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2 writes two fds into the array we pass.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        let err = std::io::Error::last_os_error();
        tracing::error!(error = %err, "SIGCHLD self-pipe creation failed; expect <defunct> in ps");
        return None;
    }
    WAKE_WRITE_FD.store(fds[1], Ordering::SeqCst);

    // SAFETY: the handler only calls async-signal-safe functions (write, an
    // atomic load, errno access) and reaps nothing.
    unsafe extern "C" fn sigchld_handler(_: libc::c_int) {
        let wfd = WAKE_WRITE_FD.load(Ordering::SeqCst);
        if wfd >= 0 {
            unsafe {
                // write(2) may clobber errno under whatever code we interrupted.
                let saved = *errno_location();
                let byte = 0u8;
                libc::write(wfd, &byte as *const u8 as *const libc::c_void, 1);
                *errno_location() = saved;
            }
        }
    }

    // SAFETY: a zeroed sigaction is valid; we fill in the handler and flags.
    unsafe {
        let mut sa: libc::sigaction = std::mem::MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = sigchld_handler as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut()) != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(error = %err, "SIGCHLD handler install failed; expect <defunct> in ps");
        }
    }

    // SAFETY: fds[0] is the freshly created read end, owned by no one else.
    Some(unsafe { OwnedFd::from_raw_fd(fds[0]) })
}

/// Watch the self-pipe from [`install`] on the event loop. Each wakeup empties
/// the pipe, reaps exited tracked children, and routes their statuses.
pub fn register(
    handle: &LoopHandle<'static, ShoestringWm>,
    read_fd: Option<OwnedFd>,
) -> anyhow::Result<()> {
    let Some(read_fd) = read_fd else {
        return Ok(());
    };
    let source = Generic::new(read_fd, Interest::READ, Mode::Level);
    handle
        .insert_source(source, |_, fd, state| {
            let raw = fd.as_raw_fd();
            let mut buf = [0u8; 64];
            // SAFETY: reading into a local buffer from our own nonblocking fd.
            while unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) } > 0 {
            }
            for (pid, status) in sweep() {
                state.note_child_reaped(pid, status);
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| anyhow::anyhow!("insert child reaper source: {e}"))?;
    Ok(())
}
