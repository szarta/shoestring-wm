//! Diagnostics / metrics subsystem.
//!
//! A tiny in-process registry of named samples plus the machinery that
//! feeds it. The registry has two consumers (sinks):
//!
//! - **IPC** — [`shoestring_ipc::Request::Metrics`] snapshots it and
//!   [`shoestring_ipc::Request::MetricsStream`] tails it (wired in
//!   [`crate::ipc`]). This is the pipe an operator/agent turns on to watch
//!   the WM while debugging.
//! - **Leak detector** — [`Metrics::detect`] watches `process.open_fds`
//!   against the `RLIMIT_NOFILE` soft limit and warns *before* an
//!   unbounded fd leak repeats the 2026-06-09 crash (a per-redraw
//!   `wl_shm` leak in the bar drove the WM to EMFILE and dropped the
//!   session to GDM; fixed in 6a2e6cf, but nothing saw it coming).
//!
//! The values are sampled on a calloop timer at
//! `[diagnostics].sample_interval_ms`; see [`ShoestringWm::start_metrics`].
//! Everything here is `/proc` + `libc` — no Smithay involvement — so it
//! stays cheap and works headless.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shoestring_ipc::MetricValue;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::ShoestringWm;

/// How many recent `open_fds` readings the growth detector keeps. At the
/// default 1s cadence this is ~30s of history — long enough to tell a
/// steady leak from a transient spike.
const FD_HISTORY: usize = 30;

/// Minimum net rise across a full [`FD_HISTORY`] window (every sample
/// non-decreasing) before we call it a leak. Below this, normal churn
/// (a client mapping a few buffers) shouldn't trip the warning.
const FD_GROWTH_FLOOR: u64 = 64;

/// Registry of the latest sampled metrics plus the leak detector's state.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Latest reading per dotted metric name. Replaced wholesale each
    /// sample for gauges; counters would accumulate (none in v1).
    values: BTreeMap<String, MetricValue>,
    /// Recent `process.open_fds` readings, newest at the back.
    fd_history: VecDeque<u64>,
    /// Latched so the "approaching the fd limit" warning fires once per
    /// crossing, not every tick. Cleared with hysteresis on recovery.
    fd_threshold_warned: bool,
    /// Latched likewise for the "monotonic fd growth" warning.
    fd_growth_warned: bool,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn set_gauge(&mut self, name: &str, value: i64) {
        self.values
            .insert(name.to_string(), MetricValue::Gauge { value });
    }

    /// A copy of the current registry for an IPC reply.
    pub fn snapshot(&self) -> BTreeMap<String, MetricValue> {
        self.values.clone()
    }
}

/// Milliseconds since the Unix epoch, for stamping a sample. Saturates to
/// 0 if the clock is before the epoch (it never is).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ShoestringWm {
    /// Start the background diagnostics sampler: a calloop timer that, each
    /// tick, refreshes the registry, runs the leak detector, and pushes a
    /// sample to any `metrics` stream subscribers. No-op (logged) when
    /// `[diagnostics].enabled = false` — snapshot queries still answer on
    /// demand, but there's no background sampling or leak detection.
    ///
    /// The cadence is re-read from config each tick so a hot-reload that
    /// changes `sample_interval_ms` takes effect, and one that flips
    /// `enabled` off stops the timer. Re-enabling needs a WM restart (v1).
    pub fn start_metrics(&mut self) {
        if !self.config.diagnostics.enabled {
            tracing::info!("diagnostics disabled; metrics sampler not started");
            return;
        }
        let interval = Duration::from_millis(self.config.diagnostics.sample_interval_ms.max(1));
        let timer = Timer::from_duration(interval);
        let res = self.loop_handle.insert_source(timer, move |_, _, state| {
            if !state.config.diagnostics.enabled {
                tracing::info!("diagnostics disabled via reload; stopping metrics sampler");
                return TimeoutAction::Drop;
            }
            state.sample_metrics();
            state.push_metrics_to_subscribers();
            let ms = state.config.diagnostics.sample_interval_ms.max(1);
            TimeoutAction::ToDuration(Duration::from_millis(ms))
        });
        match res {
            Ok(_) => tracing::info!(
                interval_ms = interval.as_millis() as u64,
                "metrics sampler started"
            ),
            Err(e) => tracing::warn!(error = %e, "failed to start metrics sampler"),
        }
    }

    /// Refresh every sampled gauge in the registry, then run the leak
    /// detector. Called on the diagnostics timer and on demand by the
    /// `metrics` snapshot request (so a snapshot is fresh even when the
    /// background sampler is off).
    pub fn sample_metrics(&mut self) {
        let open_fds = read_open_fds();
        let fd_limit = read_fd_limit();
        let rss_kb = read_rss_kb();

        if let Some(fds) = open_fds {
            self.metrics.set_gauge("process.open_fds", fds as i64);
        }
        if let Some(limit) = fd_limit {
            self.metrics.set_gauge("process.fd_limit", limit as i64);
        }
        if let Some(rss) = rss_kb {
            self.metrics.set_gauge("process.rss_kb", rss as i64);
        }

        // WM-level gauges sourced from live state.
        self.metrics
            .set_gauge("wm.windows", self.foreign_toplevels.len() as i64);
        let (clients, subscribers) = self
            .ipc
            .as_ref()
            .map(|s| (s.client_count(), s.subscriber_count()))
            .unwrap_or((0, 0));
        self.metrics.set_gauge("wm.clients", clients as i64);
        self.metrics
            .set_gauge("ipc.subscribers", subscribers as i64);

        let fraction = self.config.diagnostics.fd_warn_fraction;
        if let (Some(fds), Some(limit)) = (open_fds, fd_limit) {
            self.metrics.detect(fds, limit, fraction);
        }
    }
}

impl Metrics {
    /// Inspect the latest fd reading against the soft limit and recent
    /// history; warn (latched) on either an absolute-threshold crossing or
    /// sustained monotonic growth. The leak that crashed us would trip the
    /// growth path within ~`FD_HISTORY` seconds and the threshold path
    /// minutes before the ceiling.
    fn detect(&mut self, open_fds: u64, fd_limit: u64, warn_fraction: f64) {
        // --- absolute threshold, with hysteresis on the latch ---
        let fraction = warn_fraction.clamp(f64::MIN_POSITIVE, 1.0);
        let threshold = (fd_limit as f64 * fraction) as u64;
        if open_fds > threshold {
            if !self.fd_threshold_warned {
                tracing::warn!(
                    open_fds,
                    fd_limit,
                    threshold,
                    "open file descriptors crossed {:.0}% of RLIMIT_NOFILE — possible fd leak; \
                     `shoestring-ctl metrics` to inspect",
                    fraction * 100.0,
                );
                self.fd_threshold_warned = true;
            }
        } else if open_fds < (threshold as f64 * 0.9) as u64 {
            // Dropped well back under the line; re-arm for the next crossing.
            self.fd_threshold_warned = false;
        }

        // --- monotonic-growth heuristic over the history window ---
        self.fd_history.push_back(open_fds);
        while self.fd_history.len() > FD_HISTORY {
            self.fd_history.pop_front();
        }
        if self.fd_history.len() == FD_HISTORY {
            let first = *self.fd_history.front().unwrap();
            let last = *self.fd_history.back().unwrap();
            let monotonic = self
                .fd_history
                .iter()
                .zip(self.fd_history.iter().skip(1))
                .all(|(a, b)| b >= a);
            let grew = last.saturating_sub(first) >= FD_GROWTH_FLOOR;
            if monotonic && grew {
                if !self.fd_growth_warned {
                    tracing::warn!(
                        first,
                        last,
                        window = FD_HISTORY,
                        "open file descriptors growing monotonically (+{} over {} samples) — \
                         likely fd leak; `shoestring-ctl metrics` to inspect",
                        last - first,
                        FD_HISTORY,
                    );
                    self.fd_growth_warned = true;
                }
            } else {
                // Plateaued or dropped: clear the latch so a fresh climb warns.
                self.fd_growth_warned = false;
            }
        }
    }
}

/// Count entries in `/proc/self/fd`. The directory read itself holds one
/// fd open for the duration, so the count includes a transient +1 — a
/// constant offset that doesn't affect leak (growth) detection. `None` on
/// any I/O error (e.g. non-Linux), which simply omits the gauge.
fn read_open_fds() -> Option<u64> {
    Some(std::fs::read_dir("/proc/self/fd").ok()?.count() as u64)
}

/// Resident set size in KiB from `/proc/self/statm` field 2 (resident
/// pages) times the page size. `None` if unreadable/unparseable.
fn read_rss_kb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf is a pure query with no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages.saturating_mul(page_size as u64) / 1024)
}

/// The `RLIMIT_NOFILE` soft limit — what we'd actually hit. `None` on the
/// (impossible in practice) getrlimit failure.
fn read_fd_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: lim is a valid, fully-initialized rlimit; getrlimit only writes it.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    if rc != 0 {
        return None;
    }
    // `rlim_t` is u64 on Linux (where this cast is a no-op clippy flags)
    // but i64 on FreeBSD, where the cast is load-bearing. Keep it.
    #[allow(clippy::unnecessary_cast)]
    Some(lim.rlim_cur as u64)
}

/// Raise the `RLIMIT_NOFILE` soft limit to the hard limit at startup as
/// defense-in-depth — a bigger ceiling buys more time for the detector to
/// warn before a leak crashes the session. Secondary to detection: this
/// only delays the wall, it doesn't find the leak. Logs the old/new soft
/// limit; failures are non-fatal.
pub fn raise_fd_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: same as read_fd_limit — lim is valid and only written.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        tracing::debug!("getrlimit(RLIMIT_NOFILE) failed; leaving fd limit untouched");
        return;
    }
    let old = lim.rlim_cur;
    if lim.rlim_cur >= lim.rlim_max {
        return; // already at the hard ceiling
    }
    lim.rlim_cur = lim.rlim_max;
    // SAFETY: lim is valid; setrlimit reads it.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } == 0 {
        tracing::info!(old, new = lim.rlim_cur, "raised RLIMIT_NOFILE soft limit");
    } else {
        tracing::debug!("setrlimit(RLIMIT_NOFILE) failed; leaving fd limit untouched");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fds_and_limit_are_readable() {
        // On Linux these always resolve; the test doubles as a smoke check
        // that the /proc + getrlimit plumbing works in the build env.
        let fds = read_open_fds().expect("read /proc/self/fd");
        assert!(fds > 0);
        let limit = read_fd_limit().expect("getrlimit");
        assert!(limit >= fds);
    }

    #[test]
    fn threshold_warning_latches_and_rearms() {
        let mut m = Metrics::new();
        // 800/1000 = 80% > 75% → warns, latches.
        m.detect(800, 1000, 0.75);
        assert!(m.fd_threshold_warned);
        // Still over: no state change (and, in practice, no second log).
        m.detect(810, 1000, 0.75);
        assert!(m.fd_threshold_warned);
        // Drop back under 90% of the threshold (675) → re-arm.
        m.detect(600, 1000, 0.75);
        assert!(!m.fd_threshold_warned);
    }

    #[test]
    fn monotonic_growth_trips_then_clears() {
        let mut m = Metrics::new();
        // A steady climb well under the absolute threshold still trips the
        // growth heuristic once the window fills.
        for i in 0..FD_HISTORY as u64 {
            m.detect(100 + i * 4, 100_000, 0.75);
        }
        assert!(m.fd_growth_warned);
        // A flat run clears the latch.
        for _ in 0..FD_HISTORY {
            m.detect(100, 100_000, 0.75);
        }
        assert!(!m.fd_growth_warned);
    }

    #[test]
    fn steady_state_does_not_warn() {
        let mut m = Metrics::new();
        for _ in 0..FD_HISTORY * 2 {
            m.detect(120, 1000, 0.75);
        }
        assert!(!m.fd_threshold_warned);
        assert!(!m.fd_growth_warned);
    }
}
