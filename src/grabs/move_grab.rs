use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

use crate::state::ShoestringWm;

const BTN_LEFT: u32 = 0x110;

/// Margin in logical pixels the cursor must travel back inward before a
/// new edge-cross can fire. Keeps the drag from cascading through every
/// workspace while the cursor is parked against the clamp at the edge.
const EDGE_REARM_MARGIN: f64 = 16.0;

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
                    let target = active.shifted(d);
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

    fn unset(&mut self, _data: &mut ShoestringWm) {}
}
