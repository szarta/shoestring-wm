//! State + per-frame event batching for `zwlr_virtual_pointer_v1` — lets
//! clients emulate a physical pointer (wlrctl, ydotool-on-wayland, remote
//! desktop). Dispatch lives in [`crate::handlers::virtual_pointer`].
//!
//! This is a standard client-facing Wayland protocol, advertised
//! unconditionally the way wlroots does — unlike the WM's own IPC `inject_*`
//! path (which sits behind the runtime automation gate), gating it would
//! defeat the tools it exists for. The events still go through the same seat
//! pointer as real input, so the focused client can't tell them apart.
//!
//! Frame semantics mirror `wl_pointer`: motion/button/axis requests are
//! buffered per virtual-pointer resource and only delivered when the client
//! sends `frame`. A client that never sends `frame` produces no events at all.

use std::sync::Mutex;

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState},
    input::pointer::{AxisFrame, ButtonEvent, MotionEvent},
    reexports::wayland_server::backend::GlobalId,
    utils::{Logical, Point, SERIAL_COUNTER},
};

use crate::{inject::monotonic_msec, state::ShoestringWm};

/// Held so the manager global stays registered for the WM's lifetime.
pub struct VirtualPointerState {
    #[allow(dead_code)]
    pub manager_global: GlobalId,
}

/// Global marker for `zwlr_virtual_pointer_manager_v1`.
pub struct VirtualPointerManagerData;

/// Per-`zwlr_virtual_pointer_v1` data: the optional output it's mapped to (for
/// `motion_absolute` coordinate mapping) plus the events accumulated since the
/// last `frame`.
pub struct VirtualPointerData {
    /// Output name this pointer is mapped to, if created via
    /// `create_virtual_pointer_with_output` with a non-null output. Drives the
    /// `motion_absolute` extent→logical mapping; `None` maps against the first
    /// output.
    pub output_name: Option<String>,
    pub pending: Mutex<PendingFrame>,
}

impl VirtualPointerData {
    pub fn new(output_name: Option<String>) -> Self {
        Self {
            output_name,
            pending: Mutex::new(PendingFrame::default()),
        }
    }
}

/// One frame's worth of buffered pointer events, flushed by `frame`.
#[derive(Default)]
pub struct PendingFrame {
    /// Resolved absolute target location after applying every motion /
    /// motion_absolute request in this frame. `None` if the frame had no
    /// motion. Resolved as requests arrive (see the dispatch handler) so
    /// relative deltas accumulate correctly.
    pub location: Option<Point<f64, Logical>>,
    /// Buttons pressed/released this frame, in request order.
    pub buttons: Vec<(u32, ButtonState)>,
    pub axis_source: Option<AxisSource>,
    pub axis_value: (f64, f64),
    pub axis_v120: (Option<i32>, Option<i32>),
    pub axis_stop: (bool, bool),
    /// Any axis-related request arrived, so an axis frame should be emitted.
    pub has_axis: bool,
}

impl ShoestringWm {
    /// Map a `motion_absolute` request's `(x, y)` in `[0, *_extent]` to a
    /// logical compositor-space point, against the pointer's mapped output (or
    /// the first output if unmapped). Returns `None` if there's no output to
    /// map against or an extent is zero (which would divide by zero).
    pub fn virtual_pointer_absolute(
        &self,
        output_name: Option<&str>,
        x: u32,
        y: u32,
        x_extent: u32,
        y_extent: u32,
    ) -> Option<Point<f64, Logical>> {
        if x_extent == 0 || y_extent == 0 {
            return None;
        }
        let output = match output_name {
            Some(name) => self
                .space
                .outputs()
                .find(|o| o.name() == name)
                .or_else(|| self.space.outputs().next())?,
            None => self.space.outputs().next()?,
        };
        let geo = self.space.output_geometry(output)?;
        let px = geo.loc.x as f64 + (x as f64 / x_extent as f64) * geo.size.w as f64;
        let py = geo.loc.y as f64 + (y as f64 / y_extent as f64) * geo.size.h as f64;
        Some((px, py).into())
    }

    /// Replay a virtual pointer's buffered frame through the seat pointer and
    /// close it with a `wl_pointer.frame`. Mirrors the real input handlers:
    /// motion updates pointer focus, a button press does click-to-focus (minus
    /// the super-drag / picker carve-outs, which depend on real modifier and
    /// picker state), and axis is delivered as one accumulated frame. A frame
    /// with nothing buffered emits nothing (so an empty `frame` is a no-op).
    pub fn flush_virtual_pointer_frame(&mut self, data: &VirtualPointerData) {
        let frame = std::mem::take(&mut *data.pending.lock().unwrap());
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let mut sent = false;

        if let Some(location) = frame.location {
            let serial = SERIAL_COUNTER.next_serial();
            let under = self.surface_under(location);
            pointer.motion(
                self,
                under.clone(),
                &MotionEvent {
                    location,
                    serial,
                    time: monotonic_msec(),
                },
            );
            self.last_pointer_focus = under;
            sent = true;
        }

        for (button, state) in frame.buttons {
            // Click-to-focus on press, matching the real PointerButton handler:
            // a window under the pointer takes focus unless an Overlay/Top layer
            // surface covers it; clicking empty desktop clears focus.
            if state == ButtonState::Pressed && !pointer.is_grabbed() && !self.is_locked() {
                let pos = pointer.current_location();
                match self.space.element_under(pos).map(|(w, _)| w.clone()) {
                    Some(window) if !self.overlay_layer_under(pos) => self.focus_window(&window),
                    Some(_) => {}
                    None => {
                        if self.surface_under(pos).is_none() {
                            self.clear_focus();
                        }
                    }
                }
            }
            let serial = SERIAL_COUNTER.next_serial();
            pointer.button(
                self,
                &ButtonEvent {
                    button,
                    state,
                    serial,
                    time: monotonic_msec(),
                },
            );
            sent = true;
        }

        if frame.has_axis {
            let mut af = AxisFrame::new(monotonic_msec());
            if let Some(source) = frame.axis_source {
                af = af.source(source);
            }
            if frame.axis_value.0 != 0.0 {
                af = af.value(Axis::Horizontal, frame.axis_value.0);
            }
            if frame.axis_value.1 != 0.0 {
                af = af.value(Axis::Vertical, frame.axis_value.1);
            }
            if let Some(steps) = frame.axis_v120.0 {
                af = af.v120(Axis::Horizontal, steps);
            }
            if let Some(steps) = frame.axis_v120.1 {
                af = af.v120(Axis::Vertical, steps);
            }
            if frame.axis_stop.0 {
                af = af.stop(Axis::Horizontal);
            }
            if frame.axis_stop.1 {
                af = af.stop(Axis::Vertical);
            }
            pointer.axis(self, af);
            sent = true;
        }

        if sent {
            pointer.frame(self);
        }
    }
}
