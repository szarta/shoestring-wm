//! Long-running monitor: connect to PipeWire, report every default-sink/source
//! mute (and, in phase 3, camera) change to the WM, and survive PipeWire daemon
//! restarts via reconnect + backoff. This is the autostarted process.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::pw::{Pw, Snapshot};

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A session that ran at least this long is considered healthy, so the backoff
/// resets — a fast crash-loop keeps backing off, a daemon restart hours later
/// reconnects promptly.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// Run forever: each PipeWire session reports changes until the daemon drops,
/// then we back off and reconnect.
pub fn run() -> Result<()> {
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        match run_once() {
            Ok(()) => tracing::info!("pipewire session ended; reconnecting"),
            Err(e) => tracing::warn!(error = %e, "pipewire session error; reconnecting"),
        }
        if started.elapsed() >= HEALTHY_AFTER {
            backoff = BACKOFF_MIN;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One PipeWire connection: pump its loop, reporting snapshots, until the fd
/// hangs up (daemon gone) or poll errors.
fn run_once() -> Result<()> {
    let on_change: Rc<dyn Fn(Snapshot)> = Rc::new(|snap| match crate::wm::report(snap) {
        Ok(()) => tracing::debug!(?snap, "reported media snapshot"),
        Err(e) => tracing::debug!(error = %e, "media report failed (WM not up?)"),
    });
    let pw = Pw::connect(on_change)?;
    let fd = pw.loop_fd();
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, blocking poll.
        let r = unsafe { libc::poll(&mut pfd, 1, -1) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            anyhow::bail!("pipewire loop fd closed");
        }
        if pfd.revents & libc::POLLIN != 0 {
            pw.iterate();
        }
    }
}
