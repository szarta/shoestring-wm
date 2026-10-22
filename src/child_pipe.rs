//! Calloop sources for a child process's stdout/stderr, shared by
//! [`crate::remote_command`] and [`crate::remote_screenshot`].
//!
//! Two rules keep a child's output from stalling the compositor:
//!
//! - The read end is set `O_NONBLOCK` before it is registered. The
//!   callback drains until `WouldBlock`; on a blocking pipe that last read
//!   would instead park the whole event loop (no rendering, no IPC, no
//!   timers) until the child wrote again or exited.
//! - Each wakeup reads at most [`READ_BUDGET`] bytes. The source is
//!   level-triggered, so whatever is left wakes it again on the next
//!   dispatch, and a child that prints continuously shares the loop with
//!   everything else instead of monopolising it.

use std::{
    io::Read,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
};

use anyhow::Result;
use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};

use crate::state::ShoestringWm;

/// Most bytes one wakeup reads from one pipe before yielding to the loop.
pub(crate) const READ_BUDGET: usize = 64 * 1024;

/// What a pipe source reports to its owner.
pub(crate) enum PipeEvent<'a> {
    /// A chunk of output, in order.
    Data(&'a [u8]),
    /// EOF, or a read error (logged here); the source is removed after this.
    Closed,
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

/// Set `stream` nonblocking and register it on the loop. `label` names the
/// pipe in logs; `on_event` receives its output and then a single
/// [`PipeEvent::Closed`].
pub(crate) fn register<T, F>(
    loop_handle: &LoopHandle<'static, ShoestringWm>,
    stream: T,
    label: &'static str,
    mut on_event: F,
) -> Result<RegistrationToken>
where
    T: AsRawFd + Read + 'static,
    F: FnMut(&mut ShoestringWm, PipeEvent<'_>) + 'static,
{
    set_nonblocking(&stream)?;
    let source = Generic::new(PipeFd { inner: stream }, Interest::READ, Mode::Level);
    loop_handle
        .insert_source(source, move |_, pipe, state| {
            // SAFETY: the calloop source owns the PipeFd; we never drop
            // the FD out from under it. get_mut gives us the inner T
            // through the NoIoDrop wrapper so we can call Read::read.
            let pipe = unsafe { pipe.get_mut() };
            let mut buf = [0u8; 4096];
            let mut read = 0;
            while read < READ_BUDGET {
                match pipe.inner.read(&mut buf) {
                    Ok(0) => {
                        on_event(state, PipeEvent::Closed);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => {
                        read += n;
                        on_event(state, PipeEvent::Data(&buf[..n]));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        tracing::debug!(error = %e, label, "child pipe read failed");
                        on_event(state, PipeEvent::Closed);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| anyhow::anyhow!("insert {label} source: {e}"))
}

/// Set `O_NONBLOCK` on the read end. Only our end of the pipe changes: the
/// child's write end is a separate open file description.
fn set_nonblocking<T: AsRawFd>(stream: &T) -> std::io::Result<()> {
    let raw = stream.as_raw_fd();
    // SAFETY: `raw` is a valid open fd owned by `stream` for the call's duration.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
