//! Opt-in auto-lock after inactivity (`[general] idle_lock_secs`).
//!
//! One calloop timer, due when the configured idle period would end if no
//! input arrived. When it fires it compares against the last input: if input
//! came since, it re-arms for the remainder; otherwise it runs `lock_command`.
//! So an idle box wakes once per period, not on a polling interval. Input
//! itself never touches the timer, only [`ShoestringWm::note_user_activity`]'s
//! timestamp.
//!
//! Local input and injected input both count as activity (a remote viewer
//! drives the served box through injection). The lock is skipped while already
//! locked and while a visible client inhibits idle.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::ShoestringWm;

impl ShoestringWm {
    /// Record user input: resets the idle-lock clock and any
    /// `ext_idle_notify_v1` timers.
    pub fn note_user_activity(&mut self) {
        self.last_user_activity = Instant::now();
        self.notify_idle_activity();
    }

    /// Start, restart, or stop the idle-lock timer to match the config. Call at
    /// startup and after a config reload.
    pub fn sync_idle_lock(&mut self) {
        if let Some(token) = self.idle_lock_timer.take() {
            self.loop_handle.remove(token);
        }
        let secs = self.config.general.idle_lock_secs;
        if secs == 0 {
            return;
        }
        let due = Duration::from_secs(secs).saturating_sub(self.last_user_activity.elapsed());
        let inserted = self
            .loop_handle
            .insert_source(Timer::from_duration(due), |_, _, state| {
                state.idle_lock_tick()
            });
        match inserted {
            Ok(token) => {
                self.idle_lock_timer = Some(token);
                tracing::info!(secs, "idle lock armed");
            }
            Err(e) => tracing::warn!(error = %e, "idle lock: timer insert failed"),
        }
    }

    fn idle_lock_tick(&mut self) -> TimeoutAction {
        let period = Duration::from_secs(self.config.general.idle_lock_secs);
        let idle = self.last_user_activity.elapsed();
        if idle < period {
            return TimeoutAction::ToDuration(period - idle);
        }
        if self.is_locked() {
            tracing::debug!("idle lock: already locked");
        } else if self.idle_inhibit_active() {
            tracing::debug!("idle lock: a visible client inhibits idle");
        } else {
            tracing::info!(secs = period.as_secs(), "idle lock: no input; locking");
            self.spawn_lock();
        }
        TimeoutAction::ToDuration(period)
    }
}
