//! An optional lease on the remote gate (`set_remote` with `for_secs` and/or
//! `idle_secs`): the gate turns itself off when the time limit runs out, or
//! once nobody has been connected for the idle period. Without one the gate
//! stays on until someone turns it off, which is what containers and
//! always-on serve boxes want.
//!
//! The idle clock runs while the server reports no viewers, including before
//! the first one connects, and stops while anyone is connected. Expiry takes
//! the same path as turning the gate off over IPC, so sessions end, the
//! coupled capture and automation gates close, held input is released, and
//! subscribers hear about it. One calloop timer is due at the earlier of the
//! two deadlines and is re-armed whenever the viewer count changes. Nothing
//! else watches the deadline, so a lease whose timer can't be registered
//! fails closed: the gate turns off rather than stay on unbounded.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::ShoestringWm;

/// The limits a `set_remote` asked for, and the idle clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The time limit's end.
    until: Option<Instant>,
    /// How long the gate may stay on with nobody connected.
    idle: Option<Duration>,
    /// When the last viewer left, or the lease began with none; `None` while
    /// someone is connected.
    idle_since: Option<Instant>,
}

impl Lease {
    /// `None` when neither limit is given: the gate stays on.
    pub fn new(
        now: Instant,
        limit: Option<Duration>,
        idle: Option<Duration>,
        viewers: u32,
    ) -> Option<Lease> {
        if limit.is_none() && idle.is_none() {
            return None;
        }
        // `set_remote` refuses longer leases; clamping here as well keeps
        // `Instant` arithmetic from overflowing, which would panic.
        let max = Duration::from_secs(shoestring_ipc::REMOTE_LEASE_MAX_SECS);
        let (limit, idle) = (limit.map(|d| d.min(max)), idle.map(|d| d.min(max)));
        Some(Lease {
            until: limit.map(|d| now + d),
            idle,
            idle_since: (viewers == 0).then_some(now),
        })
    }

    /// Start the idle clock when the last viewer leaves, and stop it when one
    /// connects.
    pub fn viewers_changed(&mut self, now: Instant, viewers: u32) {
        match (viewers, self.idle_since) {
            (0, None) => self.idle_since = Some(now),
            (1.., Some(_)) => self.idle_since = None,
            _ => {}
        }
    }

    /// When the gate turns off, as things stand.
    pub fn deadline(&self) -> Option<Instant> {
        let idle = self.idle_since.zip(self.idle).map(|(since, d)| since + d);
        match (self.until, idle) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The lease as `remote_status` reports it.
    pub fn status(&self, now: Instant) -> shoestring_ipc::RemoteLease {
        let left = |at: Instant| secs_ceil(at.saturating_duration_since(now));
        shoestring_ipc::RemoteLease {
            expires_in_secs: self.until.map(left),
            idle_secs: self.idle.map(|d| d.as_secs()),
            idle_expires_in_secs: self.idle_since.zip(self.idle).map(|(s, d)| left(s + d)),
        }
    }
}

/// Whole seconds, rounded up, so a lease never reads as 0 before it ends.
fn secs_ceil(d: Duration) -> u64 {
    d.as_secs() + u64::from(d.subsec_nanos() > 0)
}

impl ShoestringWm {
    /// Replace the lease (`None` keeps the gate on) and arm its timer.
    /// Returns false if the timer couldn't be registered; the caller must
    /// then turn the gate off, since nothing would end the lease.
    #[must_use]
    pub fn set_remote_lease(&mut self, lease: Option<Lease>) -> bool {
        self.remote_lease = lease;
        self.arm_remote_lease()
    }

    /// The lease as `remote_status` and `remote_changed` report it.
    pub fn remote_lease_status(&self) -> Option<shoestring_ipc::RemoteLease> {
        let now = Instant::now();
        self.remote_lease.as_ref().map(|lease| lease.status(now))
    }

    /// Follow a new viewer count with the idle clock. Returns true if the
    /// timer couldn't be re-armed and the gate was turned off (and announced)
    /// instead.
    pub fn remote_lease_viewers_changed(&mut self) -> bool {
        let Some(lease) = self.remote_lease.as_mut() else {
            return false;
        };
        lease.viewers_changed(Instant::now(), self.remote_viewers);
        if self.arm_remote_lease() {
            return false;
        }
        self.remote_lease_fail_closed();
        true
    }

    /// Set the timer for the lease's current deadline, or remove it. Returns
    /// false only when a deadline needed a timer and registering it failed.
    fn arm_remote_lease(&mut self) -> bool {
        if let Some(token) = self.remote_lease_timer.take() {
            self.loop_handle.remove(token);
        }
        let Some(deadline) = self.remote_lease.as_ref().and_then(Lease::deadline) else {
            return true;
        };
        let inserted =
            self.loop_handle
                .insert_source(Timer::from_deadline(deadline), |_, _, state| {
                    state.remote_lease_timer = None;
                    state.remote_lease_tick();
                    TimeoutAction::Drop
                });
        match inserted {
            Ok(token) => {
                self.remote_lease_timer = Some(token);
                true
            }
            Err(e) => {
                tracing::error!(error = %e, "remote lease: timer insert failed");
                false
            }
        }
    }

    /// Turn the gate off because its lease can't be enforced.
    fn remote_lease_fail_closed(&mut self) {
        tracing::error!("remote lease has no timer; turning the remote gate off");
        self.turn_remote(false);
    }

    fn remote_lease_tick(&mut self) {
        let Some(deadline) = self.remote_lease.as_ref().and_then(Lease::deadline) else {
            return;
        };
        if Instant::now() < deadline {
            if !self.arm_remote_lease() {
                self.remote_lease_fail_closed();
            }
            return;
        }
        tracing::info!("remote lease ended; turning the remote gate off");
        self.turn_remote(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn no_limits_means_no_lease() {
        assert_eq!(Lease::new(Instant::now(), None, None, 0), None);
    }

    #[test]
    fn a_time_limit_ends_regardless_of_viewers() {
        let now = Instant::now();
        let mut lease = Lease::new(now, Some(10 * MIN), None, 0).unwrap();
        assert_eq!(lease.deadline(), Some(now + 10 * MIN));
        lease.viewers_changed(now + MIN, 2);
        assert_eq!(lease.deadline(), Some(now + 10 * MIN));
        assert_eq!(lease.status(now + MIN).expires_in_secs, Some(9 * 60));
    }

    #[test]
    fn the_idle_clock_runs_only_with_nobody_connected() {
        let now = Instant::now();
        let mut lease = Lease::new(now, None, Some(5 * MIN), 0).unwrap();
        assert_eq!(lease.deadline(), Some(now + 5 * MIN), "idle from the start");
        lease.viewers_changed(now + MIN, 1);
        assert_eq!(lease.deadline(), None, "someone is connected");
        lease.viewers_changed(now + 2 * MIN, 2);
        assert_eq!(lease.deadline(), None);
        lease.viewers_changed(now + 3 * MIN, 0);
        assert_eq!(
            lease.deadline(),
            Some(now + 8 * MIN),
            "restarts when they go"
        );
        lease.viewers_changed(now + 4 * MIN, 0);
        assert_eq!(lease.deadline(), Some(now + 8 * MIN), "a repeat 0 keeps it");
    }

    #[test]
    fn a_lease_begun_with_a_viewer_waits_for_them_to_leave() {
        let now = Instant::now();
        let lease = Lease::new(now, None, Some(MIN), 1).unwrap();
        assert_eq!(lease.deadline(), None);
        assert_eq!(lease.status(now).idle_expires_in_secs, None);
        assert_eq!(lease.status(now).idle_secs, Some(60));
    }

    #[test]
    fn both_limits_end_at_the_earlier() {
        let now = Instant::now();
        let mut lease = Lease::new(now, Some(10 * MIN), Some(MIN), 0).unwrap();
        assert_eq!(lease.deadline(), Some(now + MIN));
        lease.viewers_changed(now, 1);
        assert_eq!(lease.deadline(), Some(now + 10 * MIN));
    }

    #[test]
    fn an_overlong_lease_is_clamped_not_a_panic() {
        let now = Instant::now();
        let huge = Some(Duration::from_secs(u64::MAX));
        let mut lease = Lease::new(now, huge, huge, 0).unwrap();
        let max = Duration::from_secs(shoestring_ipc::REMOTE_LEASE_MAX_SECS);
        assert_eq!(lease.deadline(), Some(now + max));
        lease.viewers_changed(now + MIN, 1);
        lease.viewers_changed(now + 2 * MIN, 0);
        assert_eq!(lease.deadline(), Some(now + max));
    }

    #[test]
    fn time_left_rounds_up() {
        let now = Instant::now();
        let lease = Lease::new(now, Some(Duration::from_millis(1500)), None, 1).unwrap();
        assert_eq!(lease.status(now).expires_in_secs, Some(2));
        assert_eq!(lease.status(now + MIN).expires_in_secs, Some(0));
    }
}
