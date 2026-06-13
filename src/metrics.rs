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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shoestring_ipc::MetricValue;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::Client;

use crate::state::{ClientState, ShoestringWm};

/// How many recent `open_fds` readings the growth detector keeps. At the
/// default 1s cadence this is ~30s of history — long enough to tell a
/// steady leak from a transient spike.
const FD_HISTORY: usize = 30;

/// Minimum net rise across a full [`FD_HISTORY`] window (every sample
/// non-decreasing) before we call it a leak. Below this, normal churn
/// (a client mapping a few buffers) shouldn't trip the warning.
const FD_GROWTH_FLOOR: u64 = 64;

/// Per-client live census of a single Wayland resource kind, keyed by the
/// resource object so create/destroy can be matched exactly.
///
/// `K` is the per-object key — [`ObjectId`] in production, a plain integer
/// in tests (object ids can't be fabricated outside a live backend). The
/// owner is a human-readable client name (`comm-pid`); because that name
/// embeds the pid it's unique per client, so it doubles as the count key —
/// two clients that share a `comm` still get distinct rows.
#[derive(Debug)]
struct ResourceCensus<K: Eq + Hash> {
    /// Owner name → live object count. An entry exists iff the count is > 0.
    counts: HashMap<String, u32>,
    /// Object key → owner name, so a destroy decrements the right client
    /// even when the resource's own `client()` no longer resolves.
    owner: HashMap<K, String>,
}

// Manual `Default` — the derive would wrongly demand `K: Default` even
// though both fields are maps that default empty regardless of `K`.
impl<K: Eq + Hash> Default for ResourceCensus<K> {
    fn default() -> Self {
        Self {
            counts: HashMap::new(),
            owner: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash> ResourceCensus<K> {
    /// Record a newly created object owned by `owner`. Idempotent on the
    /// key: a duplicate create (shouldn't happen) replaces the old owner.
    fn insert(&mut self, key: K, owner: String) {
        if let Some(prev) = self.owner.insert(key, owner.clone()) {
            decrement(&mut self.counts, &prev);
        }
        *self.counts.entry(owner).or_insert(0) += 1;
    }

    /// Record an object's destruction. No-op if we never saw its creation.
    fn remove(&mut self, key: &K) {
        if let Some(owner) = self.owner.remove(key) {
            decrement(&mut self.counts, &owner);
        }
    }

    /// `(owner, live_count)` for every client with at least one live object.
    fn iter(&self) -> impl Iterator<Item = (&String, u32)> {
        self.counts.iter().map(|(name, &n)| (name, n))
    }
}

/// Decrement a name's count, dropping the entry when it reaches zero so a
/// disconnected client leaves no stale row.
fn decrement(counts: &mut HashMap<String, u32>, name: &str) {
    if let Some(n) = counts.get_mut(name) {
        *n -= 1;
        if *n == 0 {
            counts.remove(name);
        }
    }
}

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
    /// Per-client live `wl_surface` census — the attribution that turns a
    /// process-wide fd-growth warning into "client X holds N surfaces".
    surfaces: ResourceCensus<ObjectId>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn set_gauge(&mut self, name: &str, value: i64) {
        self.values
            .insert(name.to_string(), MetricValue::Gauge { value });
    }

    /// Record a client's newly created `wl_surface`. `owner` is the
    /// resolved client name (`comm-pid`); `surface` is the object's id.
    pub fn track_surface(&mut self, surface: ObjectId, owner: String) {
        self.surfaces.insert(surface, owner);
    }

    /// Record a `wl_surface` destruction. No-op if its creation went
    /// unseen (e.g. it was created before diagnostics tracking existed).
    pub fn untrack_surface(&mut self, surface: &ObjectId) {
        self.surfaces.remove(surface);
    }

    /// Re-derive the `client.<name>.surfaces` gauges from the live census.
    /// Stale `client.*` keys are cleared first so a client that just
    /// dropped its last surface disappears from the snapshot.
    fn refresh_client_gauges(&mut self) {
        self.values.retain(|k, _| !k.starts_with("client."));
        // Collect first to avoid borrowing `self.surfaces` and `self.values`
        // at once.
        let rows: Vec<(String, i64)> = self
            .surfaces
            .iter()
            .map(|(name, n)| (format!("client.{name}.surfaces"), n as i64))
            .collect();
        for (key, value) in rows {
            self.values.insert(key, MetricValue::Gauge { value });
        }
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
    /// The cached metrics name (`comm-pid`) for a Wayland client,
    /// resolving it on first call and memoising it in the client's
    /// [`ClientState`] so the `/proc` lookup runs at most once per client.
    /// Clients without our `ClientState` (notably Xwayland, which carries
    /// `XWaylandClientData`) are resolved fresh each call — rare enough not
    /// to matter.
    pub fn client_metrics_name(&self, client: &Client) -> String {
        let resolve = || {
            // pid 0 is never a real client (Linux's swapper); FreeBSD's
            // peer-cred lookup also reports 0 because its `LOCAL_PEERCRED`
            // carries no pid. Treat ≤0 as "no pid" and fall back to a stable
            // per-client serial so distinct clients stay distinct rows
            // rather than all collapsing into one.
            let pid = client
                .get_credentials(&self.display_handle)
                .ok()
                .map(|creds| creds.pid)
                .filter(|&pid| pid > 0);
            match pid {
                Some(pid) => client_name(Some(pid), read_comm(pid)),
                None => format!("client-{}", next_client_serial()),
            }
        };
        if let Some(state) = client.get_data::<ClientState>() {
            return state.metrics_name.get_or_init(resolve).clone();
        }
        resolve()
    }

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

        // Per-client resource attribution: surface gauges, freshly derived
        // from the live census maintained on surface create/destroy.
        self.metrics.refresh_client_gauges();

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

/// A stable, human-readable name for a Wayland client: its process
/// `comm` joined with its pid (`shoestring-bar-1234`). The pid keeps the
/// name unique even when two instances share a `comm`. Falls back to
/// `pid-<n>` when `/proc` is unreadable (non-Linux) and `unknown` when even
/// the pid is unavailable. Non-`[A-Za-z0-9_-]` bytes in `comm` are squashed
/// to `_` so the name stays a clean dotted-metric segment.
pub fn client_name(pid: Option<i32>, comm: Option<String>) -> String {
    match (comm, pid) {
        (Some(comm), Some(pid)) => format!("{}-{pid}", sanitize_segment(&comm)),
        (None, Some(pid)) => format!("pid-{pid}"),
        _ => "unknown".to_string(),
    }
}

/// Hands out a monotonic id for clients whose pid we can't determine (no
/// peer-cred pid — e.g. FreeBSD). Caching the result in [`ClientState`]
/// makes it stable for the life of that client, so its census rows balance.
fn next_client_serial() -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Squash a string to the `[A-Za-z0-9_-]` charset used by metric-name
/// segments, collapsing everything else (dots, spaces, slashes) to `_`.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_".to_string()
    } else {
        cleaned
    }
}

/// Read a process's `comm` (the executable's basename, ≤15 chars) from
/// `/proc/<pid>/comm`, trimmed of its trailing newline. `None` on any I/O
/// error — notably every non-Linux platform, where there's no procfs.
pub fn read_comm(pid: i32) -> Option<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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
        // getrlimit is portable (Linux + the BSDs), so the limit always
        // resolves — assert it unconditionally.
        let limit = read_fd_limit().expect("getrlimit");
        assert!(limit > 0);
        // The fd count comes from /proc/self/fd, which only exists on Linux
        // with procfs mounted. Where present it must be a sane positive count
        // within the limit; where absent (e.g. FreeBSD, which mounts no
        // procfs by default) read_open_fds is None and the gauge is simply
        // omitted — that's graceful degradation, not a test failure.
        if let Some(fds) = read_open_fds() {
            assert!(fds > 0);
            assert!(limit >= fds);
        }
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

    // The census is generic over the object key; tests key by `u64` because
    // a real `ObjectId` can't be built outside a live wayland backend.
    fn count(c: &ResourceCensus<u64>, owner: &str) -> Option<u32> {
        c.counts.get(owner).copied()
    }

    #[test]
    fn census_counts_per_owner_and_prunes_at_zero() {
        let mut c: ResourceCensus<u64> = ResourceCensus::default();
        c.insert(1, "bar-10".into());
        c.insert(2, "bar-10".into());
        c.insert(3, "term-20".into());
        assert_eq!(count(&c, "bar-10"), Some(2));
        assert_eq!(count(&c, "term-20"), Some(1));

        // Destroying one of bar's surfaces decrements; the row stays.
        c.remove(&1);
        assert_eq!(count(&c, "bar-10"), Some(1));
        // Its last surface gone → the row disappears entirely.
        c.remove(&2);
        assert_eq!(count(&c, "bar-10"), None);
        assert_eq!(count(&c, "term-20"), Some(1));
    }

    #[test]
    fn census_remove_unknown_key_is_noop() {
        let mut c: ResourceCensus<u64> = ResourceCensus::default();
        c.insert(1, "bar-10".into());
        c.remove(&999); // never inserted
        assert_eq!(count(&c, "bar-10"), Some(1));
    }

    #[test]
    fn census_same_comm_distinct_pids_are_separate_rows() {
        // Two clients share a `comm` but differ by pid → the pid-suffixed
        // names keep them apart, which is the whole point of embedding pid.
        let mut c: ResourceCensus<u64> = ResourceCensus::default();
        c.insert(1, "alacritty-10".into());
        c.insert(2, "alacritty-20".into());
        assert_eq!(count(&c, "alacritty-10"), Some(1));
        assert_eq!(count(&c, "alacritty-20"), Some(1));
    }

    #[test]
    fn client_name_formats_and_falls_back() {
        assert_eq!(
            client_name(Some(1234), Some("shoestring-bar".into())),
            "shoestring-bar-1234"
        );
        // No comm (e.g. non-Linux) → pid-only form.
        assert_eq!(client_name(Some(77), None), "pid-77");
        // Nothing at all → a stable sentinel rather than an empty key.
        assert_eq!(client_name(None, None), "unknown");
    }

    #[test]
    fn sanitize_segment_squashes_metric_unsafe_chars() {
        // Dots would split the dotted metric path; spaces/slashes are noise.
        assert_eq!(sanitize_segment("a.b c/d"), "a_b_c_d");
        assert_eq!(sanitize_segment("keep-_09"), "keep-_09");
        assert_eq!(sanitize_segment(""), "_");
    }

    #[test]
    fn refresh_client_gauges_reflects_live_census() {
        let mut m = Metrics::new();
        // The ObjectId-keyed census can't be driven from a unit test, so we
        // verify the projection's contract directly: it clears any prior
        // `client.*` keys and leaves unrelated gauges untouched.
        m.values.insert(
            "client.stale-1.surfaces".into(),
            MetricValue::Gauge { value: 9 },
        );
        m.values
            .insert("process.open_fds".into(), MetricValue::Gauge { value: 5 });
        m.refresh_client_gauges();
        // Stale per-client key is gone; unrelated keys survive.
        assert!(!m.values.contains_key("client.stale-1.surfaces"));
        assert!(m.values.contains_key("process.open_fds"));
    }
}
