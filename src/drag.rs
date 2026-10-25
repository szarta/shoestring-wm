//! Synthesized drags (`drag` over IPC) that take real time.
//!
//! A drag source often starts its own loop on the press and then reads the
//! button's state: wine's drag detection and OLE `DoDragDrop` do, as do
//! toolkits that wait out a drag threshold. Sent all at once, the press, the
//! motions and the release arrive with one timestamp, the source finds the
//! button already up, and the gesture reads as a click. So a drag runs on a
//! calloop timer instead: press, a hold, motions spread over a duration (each
//! its own frame, with rising timestamps), a hold at the target, then the
//! release, after which the IPC reply goes out. The loop keeps running
//! throughout.
//!
//! One drag runs at a time. It stops early, releasing the button, if its
//! connection closes, the automation gate closes, the session locks after it
//! began, or the pressed window goes away; the reply then says why.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point};

use crate::ipc::{write_response, Client, ClientId};
use crate::state::ShoestringWm;

/// Where a drag goes: resolved once, when it starts.
pub(crate) struct DragPath {
    /// The evdev button code to hold.
    pub button: u32,
    /// The surface every motion is sent to (see
    /// [`ShoestringWm::resolve_drag`]).
    pub focus: Option<(WlSurface, Point<f64, Logical>)>,
    /// Global logical coordinates of the press and the release.
    pub from: (f64, f64),
    pub to: (f64, f64),
}

/// How long a drag takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DragTiming {
    /// After the press before the first motion, and at the target before the
    /// release.
    hold: Duration,
    /// From the first motion to the last.
    duration: Duration,
    /// How many motions, evenly spaced from `from` to `to`.
    steps: u32,
}

impl DragTiming {
    /// The request's timings, with its defaults; refuses out-of-range values.
    pub fn from_request(
        hold_ms: Option<u32>,
        duration_ms: Option<u32>,
        steps: Option<u32>,
    ) -> Result<DragTiming, String> {
        use shoestring_ipc::{
            DRAG_DEFAULT_DURATION_MS, DRAG_DEFAULT_HOLD_MS, DRAG_DEFAULT_STEPS, DRAG_MAX_MS,
            DRAG_MAX_STEPS,
        };
        let hold_ms = hold_ms.unwrap_or(DRAG_DEFAULT_HOLD_MS);
        let duration_ms = duration_ms.unwrap_or(DRAG_DEFAULT_DURATION_MS);
        let steps = steps.unwrap_or(DRAG_DEFAULT_STEPS);
        if hold_ms > DRAG_MAX_MS || duration_ms > DRAG_MAX_MS {
            return Err(format!(
                "drag: hold_ms and duration_ms must be at most {DRAG_MAX_MS}"
            ));
        }
        if !(1..=DRAG_MAX_STEPS).contains(&steps) {
            return Err(format!("drag: steps must be 1 to {DRAG_MAX_STEPS}"));
        }
        Ok(DragTiming {
            hold: Duration::from_millis(hold_ms.into()),
            duration: Duration::from_millis(duration_ms.into()),
            steps,
        })
    }

    /// How long event `k` waits after the one before it: motions are
    /// `1..=steps`, and `steps + 1` is the release. Measured from when the
    /// previous event was actually sent, so a busy loop stretches the drag
    /// rather than cutting the holds short.
    fn gap(&self, k: u32) -> Duration {
        let interval = self.duration / self.steps;
        match k {
            1 => self.hold + interval,
            k if k <= self.steps => interval,
            _ => self.hold,
        }
    }
}

/// The drag in progress, and the connection waiting for its reply.
pub(crate) struct PendingDrag {
    client_id: ClientId,
    client: Rc<RefCell<Client>>,
    path: DragPath,
    timing: DragTiming,
    /// When the last event was sent.
    last: Instant,
    /// The next event to send; see [`DragTiming::gap`].
    next: u32,
    /// A drag begun on a locked session meets the lock screen like real
    /// input; one begun unlocked stops if the session locks.
    locked_at_start: bool,
    timer: Option<RegistrationToken>,
}

impl ShoestringWm {
    /// Press now and run the rest of the drag on a timer; the reply goes to
    /// `client` once the button is released. Errors (and changes nothing) if
    /// another drag is running.
    pub(crate) fn start_drag(
        &mut self,
        client_id: ClientId,
        client: &Rc<RefCell<Client>>,
        path: DragPath,
        timing: DragTiming,
    ) -> Result<(), String> {
        if self.pending_drag.is_some() {
            return Err("drag: another drag is in progress".into());
        }
        self.drag_motion(&path.focus, path.from);
        self.inject_raw_button(path.button, true, false);
        self.pending_drag = Some(PendingDrag {
            client_id,
            client: Rc::clone(client),
            path,
            timing,
            last: Instant::now(),
            next: 1,
            locked_at_start: self.is_locked(),
            timer: None,
        });
        self.arm_drag();
        Ok(())
    }

    /// Stop the drag owned by a closing connection, releasing its button.
    pub(crate) fn cancel_drag_owned_by(&mut self, id: ClientId) {
        if self
            .pending_drag
            .as_ref()
            .is_some_and(|d| d.client_id == id)
        {
            let drag = self.pending_drag.take().unwrap();
            if let Some(token) = drag.timer {
                self.loop_handle.remove(token);
            }
            self.inject_raw_button(drag.path.button, false, false);
        }
    }

    /// Set the timer for the next event. A drag whose timer can't be set
    /// releases now rather than leave the button held.
    fn arm_drag(&mut self) {
        let Some(drag) = self.pending_drag.as_mut() else {
            return;
        };
        let due = drag.last + drag.timing.gap(drag.next);
        let inserted = self
            .loop_handle
            .insert_source(Timer::from_deadline(due), |_, _, state| {
                if let Some(drag) = state.pending_drag.as_mut() {
                    drag.timer = None;
                }
                state.drag_tick();
                TimeoutAction::Drop
            });
        match inserted {
            Ok(token) => drag.timer = Some(token),
            Err(e) => {
                tracing::error!(error = %e, "drag: timer insert failed");
                let drag = self.pending_drag.take().unwrap();
                self.inject_raw_button(drag.path.button, false, false);
                self.finish_drag(drag, Err("drag: could not schedule the drag".into()));
            }
        }
    }

    fn drag_tick(&mut self) {
        let Some(mut drag) = self.pending_drag.take() else {
            return;
        };
        // Closing the gate has already released the button.
        if !self.automation_enabled {
            let message = "automation disabled: the drag was stopped".into();
            return self.finish_drag(drag, Err(message));
        }
        let stop = if self.is_locked() && !drag.locked_at_start {
            Some("session locked: the drag was stopped")
        } else if drag.path.focus.as_ref().is_some_and(|(s, _)| !s.alive()) {
            Some("drag: the window went away")
        } else {
            None
        };
        if let Some(message) = stop {
            self.inject_raw_button(drag.path.button, false, false);
            return self.finish_drag(drag, Err(message.into()));
        }

        let steps = drag.timing.steps;
        if drag.next <= steps {
            let t = f64::from(drag.next) / f64::from(steps);
            let (fx, fy) = drag.path.from;
            let (tx, ty) = drag.path.to;
            self.drag_motion(&drag.path.focus, (fx + (tx - fx) * t, fy + (ty - fy) * t));
            drag.last = Instant::now();
            drag.next += 1;
            self.pending_drag = Some(drag);
            self.arm_drag();
        } else {
            self.inject_raw_button(drag.path.button, false, false);
            self.finish_drag(drag, Ok(()));
        }
    }

    /// Reply to the drag's connection and close it.
    fn finish_drag(&mut self, drag: PendingDrag, result: Result<(), String>) {
        let response = match result {
            Ok(()) => shoestring_ipc::Response::Ok,
            Err(message) => shoestring_ipc::Response::Error { message },
        };
        let _ = write_response(&drag.client, &response);
        self.drop_ipc_client(drag.client_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_bounds() {
        let t = DragTiming::from_request(None, None, None).unwrap();
        assert_eq!(t.steps, shoestring_ipc::DRAG_DEFAULT_STEPS);
        assert!(t.hold > Duration::ZERO && t.duration > Duration::ZERO);
        assert!(DragTiming::from_request(Some(0), Some(0), Some(1)).is_ok());
        let max = shoestring_ipc::DRAG_MAX_MS;
        assert!(DragTiming::from_request(Some(max + 1), None, None).is_err());
        assert!(DragTiming::from_request(None, Some(max + 1), None).is_err());
        assert!(DragTiming::from_request(None, None, Some(0)).is_err());
        let steps = shoestring_ipc::DRAG_MAX_STEPS;
        assert!(DragTiming::from_request(None, None, Some(steps + 1)).is_err());
    }

    #[test]
    fn events_are_spread_over_the_hold_and_duration() {
        let t = DragTiming::from_request(Some(100), Some(400), Some(4)).unwrap();
        let ms = |k| t.gap(k).as_millis();
        assert_eq!(
            (1..=5).map(ms).collect::<Vec<_>>(),
            [200, 100, 100, 100, 100],
            "a 100 ms hold, four motions 100 ms apart, a hold, release"
        );
    }
}
