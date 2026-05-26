use std::time::Duration;

use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle},
};

use crate::state::ShoestringWm;

const BTN_LEFT: u32 = 0x110;

/// Margin in logical pixels the cursor must travel back inward before a
/// new edge-cross can fire. Keeps the drag from cascading through every
/// workspace while the cursor is parked against the clamp at the edge.
const EDGE_REARM_MARGIN: f64 = 16.0;

/// How long to wait at the edge before auto-shifting to the next workspace
/// during a sustained drag. The first shift fires on the inward → edge
/// transition (see [`MoveSurfaceGrab::motion`]); subsequent shifts fire
/// at this interval as long as the cursor stays at the edge.
const EDGE_REPEAT_INTERVAL: Duration = Duration::from_millis(800);

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<ShoestringWm>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    /// Previous pointer x; used to detect inward → edge transitions for
    /// the edge-drag workspace-switch gesture. Seeded from `start_data`.
    last_pointer_x: f64,
    /// Set after an edge-cross transfer; cleared when the cursor moves
    /// back inward past [`EDGE_REARM_MARGIN`].
    edge_disarmed: bool,
}

impl MoveSurfaceGrab {
    pub fn new(
        start_data: PointerGrabStartData<ShoestringWm>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
    ) -> Self {
        let last_pointer_x = start_data.location.x;
        Self {
            start_data,
            window,
            initial_window_location,
            last_pointer_x,
            edge_disarmed: false,
        }
    }
}

/// Outer rect that bounds the cursor across every mapped output. Same
/// shape `input.rs` uses for clamping; duplicated here so the grab can
/// detect "cursor is pinned at the edge" without re-borrowing the
/// pointer.
fn desktop_bounds(state: &ShoestringWm) -> Option<Rectangle<i32, Logical>> {
    let mut iter = state
        .space
        .outputs()
        .filter_map(|o| state.space.output_geometry(o));
    let first = iter.next()?;
    Some(iter.fold(first, |acc, r| {
        let x0 = acc.loc.x.min(r.loc.x);
        let y0 = acc.loc.y.min(r.loc.y);
        let x1 = (acc.loc.x + acc.size.w).max(r.loc.x + r.size.w);
        let y1 = (acc.loc.y + acc.size.h).max(r.loc.y + r.size.h);
        Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
    }))
}

impl ShoestringWm {
    /// (Re-)schedule the edge-drag repeat timer. Idempotent — any
    /// existing timer is cancelled first.
    pub fn arm_edge_repeat_timer(&mut self) {
        if let Some(t) = self.edge_drag_repeat_token.take() {
            self.loop_handle.remove(t);
        }
        let timer = Timer::from_duration(EDGE_REPEAT_INTERVAL);
        match self.loop_handle.insert_source(timer, |_, _, state| {
            state.edge_drag_repeat_token = None;
            state.try_edge_repeat_shift();
            TimeoutAction::Drop
        }) {
            Ok(token) => self.edge_drag_repeat_token = Some(token),
            Err(e) => tracing::warn!(error = %e, "insert edge-drag repeat timer failed"),
        }
    }

    /// Timer callback: if the drag is still alive and the pointer is
    /// still pinned to an edge, shift the dragged window onto the next
    /// workspace and re-arm. If the cursor has moved inward, do
    /// nothing — the regular inward-motion path will re-arm on the
    /// next edge crossing.
    pub fn try_edge_repeat_shift(&mut self) {
        let Some(window) = self.edge_drag_window.clone() else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(bounds) = desktop_bounds(self) else {
            return;
        };
        let cursor = pointer.current_location();
        let left = bounds.loc.x as f64;
        let right = (bounds.loc.x + bounds.size.w) as f64;
        let direction = if cursor.x >= right {
            1
        } else if cursor.x <= left {
            -1
        } else {
            // Cursor moved off the edge while the timer was queued.
            // Don't fire and don't reschedule — the grab's motion
            // handler owns re-arming.
            return;
        };
        let active = self.workspaces.active();
        let target = self.workspaces.shifted(active, direction);
        if target == active {
            return;
        }
        tracing::debug!(
            direction,
            target = target.one_based(),
            "edge-drag repeat shift"
        );
        self.move_window_to_workspace_following(&window, target);
        self.arm_edge_repeat_timer();
    }

    /// Tear down the edge-drag tracking when the move grab ends.
    pub fn end_edge_drag(&mut self) {
        self.edge_drag_window = None;
        if let Some(t) = self.edge_drag_repeat_token.take() {
            self.loop_handle.remove(t);
        }
    }
}

impl PointerGrab<ShoestringWm> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.space
            .map_element(self.window.clone(), new_location.to_i32_round(), true);

        let prev_x = self.last_pointer_x;
        let cur_x = event.location.x;
        self.last_pointer_x = cur_x;

        // Outer rect of every mapped output — the cursor is clamped to
        // this in input.rs, so cur_x == right means the user pushed
        // against the rightmost edge of the desktop.
        let bounds = {
            let mut iter = data
                .space
                .outputs()
                .filter_map(|o| data.space.output_geometry(o));
            iter.next().map(|first| {
                iter.fold(first, |acc, r| {
                    let x0 = acc.loc.x.min(r.loc.x);
                    let y0 = acc.loc.y.min(r.loc.y);
                    let x1 = (acc.loc.x + acc.size.w).max(r.loc.x + r.size.w);
                    let y1 = (acc.loc.y + acc.size.h).max(r.loc.y + r.size.h);
                    smithay::utils::Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
                })
            })
        };

        if let Some(bounds) = bounds {
            let left = bounds.loc.x as f64;
            let right = (bounds.loc.x + bounds.size.w) as f64;

            if self.edge_disarmed
                && cur_x < right - EDGE_REARM_MARGIN
                && cur_x > left + EDGE_REARM_MARGIN
            {
                self.edge_disarmed = false;
            }

            if !self.edge_disarmed {
                let direction = if cur_x >= right && prev_x < right {
                    Some(1_i32)
                } else if cur_x <= left && prev_x > left {
                    Some(-1_i32)
                } else {
                    None
                };
                if let Some(d) = direction {
                    let active = data.workspaces.active();
                    let target = data.workspaces.shifted(active, d);
                    if target != active {
                        let window = self.window.clone();
                        data.move_window_to_workspace_following(&window, target);
                        // Anchor subsequent motion to the window's current
                        // (post-switch) position with zero delta, so the
                        // window doesn't jump as the user keeps dragging.
                        if let Some(new_loc) = data.space.element_location(&window) {
                            self.initial_window_location = new_loc;
                        }
                        self.start_data.location = event.location;
                        self.edge_disarmed = true;
                        // Schedule the sustained-edge repeat. If the
                        // cursor stays pinned at the edge, the timer
                        // will fire another shift in EDGE_REPEAT_INTERVAL.
                        data.arm_edge_repeat_timer();
                    }
                }
            }
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().contains(&BTN_LEFT) {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event)
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event)
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event)
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event)
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event)
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event)
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event)
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event)
    }

    fn start_data(&self) -> &PointerGrabStartData<ShoestringWm> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut ShoestringWm) {
        data.end_edge_drag();
    }
}
