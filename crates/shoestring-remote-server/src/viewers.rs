//! Shared count of connected viewers, reported to the WM on every change.
//!
//! The WM caches the number and broadcasts it so the bar can light a "being
//! viewed / controlled" chip — a privacy signal that must never be silent, so
//! every add/remove pushes the new total (best-effort: a failed report is
//! logged, not fatal, since the session itself is still live).

use std::sync::Mutex;

use crate::wm;

#[derive(Default)]
pub struct Viewers {
    count: Mutex<u32>,
}

impl Viewers {
    pub fn new() -> Self {
        Viewers {
            count: Mutex::new(0),
        }
    }

    /// A viewer connected: bump the count and report the new total.
    pub fn add(&self) {
        self.set(|n| n + 1);
    }

    /// A viewer disconnected: drop the count (saturating) and report it.
    pub fn remove(&self) {
        self.set(|n| n.saturating_sub(1));
    }

    /// Force the count to zero and report it — used when the gate flips off so
    /// the chip clears even if a session teardown raced the gate change.
    pub fn reset(&self) {
        self.set(|_| 0);
    }

    fn set(&self, f: impl FnOnce(u32) -> u32) {
        let total = {
            let mut guard = self.count.lock().unwrap();
            *guard = f(*guard);
            *guard
        };
        if let Err(e) = wm::report_viewers(total) {
            tracing::warn!(error = %e, total, "failed to report viewer count to WM");
        }
    }
}
