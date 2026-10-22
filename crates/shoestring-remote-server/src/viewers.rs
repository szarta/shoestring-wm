//! Shared count of connected viewers, reported to the WM on every change.
//!
//! The WM caches the number and broadcasts it so the bar can light a "being
//! viewed / controlled" chip — a privacy signal that must never be silent, so
//! every change pushes the new total (best-effort: a failed report is logged,
//! not fatal, since the session itself is still live).
//!
//! Three rules keep the WM's number right:
//!
//! - Each change is reported while the count is still locked, so reports reach
//!   the WM in the order the count changed: a stale `0` can't land after a
//!   newer `1`.
//! - A session counts for as long as it holds a [`ViewerGuard`], which drops
//!   its count however the session ends, early errors included.
//! - Closing the gate ([`Viewers::reset`]) starts a new generation. Sessions
//!   from before it are still tearing down; their guards no longer count, so
//!   they can't take a viewer who connected after the gate reopened off the
//!   count.

use std::sync::{Arc, Mutex};

use crate::wm;

struct Count {
    generation: u64,
    viewers: u32,
}

pub struct Viewers {
    count: Mutex<Count>,
    report: Box<dyn Fn(u32) + Send + Sync>,
}

/// One connected viewer's place in the count, released on drop.
pub struct ViewerGuard {
    viewers: Arc<Viewers>,
    generation: u64,
}

impl Default for Viewers {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewers {
    /// Report each change to the WM.
    pub fn new() -> Self {
        Self::with_reporter(|total| {
            if let Err(e) = wm::report_viewers(total) {
                tracing::warn!(error = %e, total, "failed to report viewer count to WM");
            }
        })
    }

    fn with_reporter(report: impl Fn(u32) + Send + Sync + 'static) -> Self {
        Viewers {
            count: Mutex::new(Count {
                generation: 0,
                viewers: 0,
            }),
            report: Box::new(report),
        }
    }

    /// A viewer connected: count it until the returned guard drops.
    pub fn join(self: &Arc<Self>) -> ViewerGuard {
        let mut count = self.count.lock().unwrap();
        count.viewers += 1;
        (self.report)(count.viewers);
        ViewerGuard {
            viewers: Arc::clone(self),
            generation: count.generation,
        }
    }

    /// The gate closed: zero the count, report it, and start a new generation
    /// so sessions still tearing down can't change the next one's count.
    pub fn reset(&self) {
        let mut count = self.count.lock().unwrap();
        count.generation += 1;
        count.viewers = 0;
        (self.report)(0);
    }

    fn leave(&self, generation: u64) {
        let mut count = self.count.lock().unwrap();
        if count.generation != generation {
            return;
        }
        count.viewers = count.viewers.saturating_sub(1);
        (self.report)(count.viewers);
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.viewers.leave(self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Viewers whose reports are recorded, in the order they were made.
    fn recorded() -> (Arc<Viewers>, Arc<Mutex<Vec<u32>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let viewers = Viewers::with_reporter(move |n| sink.lock().unwrap().push(n));
        (Arc::new(viewers), log)
    }

    #[test]
    fn guards_count_while_held() {
        let (viewers, log) = recorded();
        let a = viewers.join();
        let b = viewers.join();
        drop(a);
        drop(b);
        assert_eq!(*log.lock().unwrap(), vec![1, 2, 1, 0]);
    }

    #[test]
    fn concurrent_changes_are_reported_in_order() {
        let (viewers, log) = recorded();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..50 {
                        drop(viewers.join());
                    }
                });
            }
        });
        // Every report is one step from the one before, and the last is 0:
        // the WM, applying them in order, ends on the true count.
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 8 * 50 * 2);
        let mut previous = 0i64;
        for &n in log.iter() {
            assert_eq!(
                (n as i64 - previous).abs(),
                1,
                "reports out of order: {log:?}"
            );
            previous = n as i64;
        }
        assert_eq!(previous, 0);
    }

    #[test]
    fn a_session_from_before_a_reset_does_not_count() {
        let (viewers, log) = recorded();
        let old = viewers.join();
        viewers.reset();
        let new = viewers.join();
        drop(old);
        assert_eq!(
            *log.lock().unwrap(),
            vec![1, 0, 1],
            "old teardown changed the count"
        );
        drop(new);
        assert_eq!(log.lock().unwrap().last(), Some(&0));
    }
}
