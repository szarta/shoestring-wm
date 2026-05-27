use std::cell::RefCell;

use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
        MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor, shell::xdg::SurfaceCachedState},
};

use crate::state::ShoestringWm;

const BTN_RIGHT: u32 = 0x111;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP    = 0b0001;
        const BOTTOM = 0b0010;
        const LEFT   = 0b0100;
        const RIGHT  = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();
        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<smithay::xwayland::xwm::ResizeEdge> for ResizeEdge {
    fn from(edges: smithay::xwayland::xwm::ResizeEdge) -> Self {
        use smithay::xwayland::xwm::ResizeEdge as X;
        match edges {
            X::Top => Self::TOP,
            X::Bottom => Self::BOTTOM,
            X::Left => Self::LEFT,
            X::Right => Self::RIGHT,
            X::TopLeft => Self::TOP_LEFT,
            X::TopRight => Self::TOP_RIGHT,
            X::BottomLeft => Self::BOTTOM_LEFT,
            X::BottomRight => Self::BOTTOM_RIGHT,
        }
    }
}

pub struct ResizeSurfaceGrab {
    start_data: PointerGrabStartData<ShoestringWm>,
    window: Window,
    edges: ResizeEdge,
    initial_rect: Rectangle<i32, Logical>,
    last_window_size: Size<i32, Logical>,
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<ShoestringWm>,
        window: Window,
        edges: ResizeEdge,
        initial_window_rect: Rectangle<i32, Logical>,
    ) -> Self {
        // Only xdg toplevels run the ack/commit state machine; X11 surfaces
        // accept the new size synchronously via X11Surface::configure on
        // motion, so they have no "Resizing" / "WaitingForLastCommit"
        // bookkeeping to seed here.
        if let Some(t) = window.toplevel() {
            ResizeSurfaceState::with(t.wl_surface(), |state| {
                *state = ResizeSurfaceState::Resizing {
                    edges,
                    initial_rect: initial_window_rect,
                };
            });
        }

        Self {
            start_data,
            window,
            edges,
            initial_rect: initial_window_rect,
            last_window_size: initial_window_rect.size,
        }
    }
}

impl PointerGrab<ShoestringWm> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut PointerInnerHandle<'_, ShoestringWm>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        let mut delta = event.location - self.start_data.location;
        let mut new_w = self.initial_rect.size.w;
        let mut new_h = self.initial_rect.size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                delta.x = -delta.x;
            }
            new_w = (self.initial_rect.size.w as f64 + delta.x) as i32;
        }
        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) {
                delta.y = -delta.y;
            }
            new_h = (self.initial_rect.size.h as f64 + delta.y) as i32;
        }

        let (min_size, max_size) = self
            .window
            .toplevel()
            .map(|t| {
                compositor::with_states(t.wl_surface(), |states| {
                    let mut guard = states.cached_state.get::<SurfaceCachedState>();
                    let cur = guard.current();
                    (cur.min_size, cur.max_size)
                })
            })
            .unwrap_or_default();
        let min_w = min_size.w.max(1);
        let min_h = min_size.h.max(1);
        let max_w = if max_size.w == 0 {
            i32::MAX
        } else {
            max_size.w
        };
        let max_h = if max_size.h == 0 {
            i32::MAX
        } else {
            max_size.h
        };

        self.last_window_size = Size::from((new_w.clamp(min_w, max_w), new_h.clamp(min_h, max_h)));

        if let Some(xdg) = self.window.toplevel() {
            xdg.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
                state.size = Some(self.last_window_size);
            });
            xdg.send_pending_configure();
        } else if let Some(x11) = self.window.x11_surface() {
            // X11 has no ack/commit dance: tell the client the new geometry
            // synchronously. Position stays put, only size changes.
            let geo = Rectangle::new(self.initial_rect.loc, self.last_window_size);
            let _ = x11.configure(geo);
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
        if !handle.current_pressed().contains(&BTN_RIGHT) {
            handle.unset_grab(self, data, event.serial, event.time, true);

            if let Some(xdg) = self.window.toplevel() {
                xdg.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();

                ResizeSurfaceState::with(xdg.wl_surface(), |state| {
                    *state = ResizeSurfaceState::WaitingForLastCommit {
                        edges: self.edges,
                        initial_rect: self.initial_rect,
                    };
                });
            }
            // X11 already received the final size on the last motion event —
            // no terminal ack to send.
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

/// Per-surface resize state, kept on the surface's data map so commit-time
/// logic can reposition the window after the client acks a TOP/LEFT resize.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Resizing {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
    },
    WaitingForLastCommit {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();
            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing {
                edges,
                initial_rect,
            } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit {
                edges,
                initial_rect,
            } => {
                *self = Self::Idle;
                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }
}

pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        state
            .commit()
            .and_then(|(edges, initial_rect)| {
                edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                    let new_x = edges
                        .intersects(ResizeEdge::LEFT)
                        .then_some(initial_rect.loc.x + (initial_rect.size.w - geometry.size.w));
                    let new_y = edges
                        .intersects(ResizeEdge::TOP)
                        .then_some(initial_rect.loc.y + (initial_rect.size.h - geometry.size.h));
                    (new_x, new_y).into()
                })
            })
            .unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x {
        window_loc.x = new_x;
    }
    if let Some(new_y) = new_loc.y {
        window_loc.y = new_y;
    }
    if new_loc.x.is_some() || new_loc.y.is_some() {
        space.map_element(window, window_loc, false);
    }

    Some(())
}
