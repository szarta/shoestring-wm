//! A touch grab that dismisses popups on touch-outside, mirroring smithay's
//! [`PopupPointerGrab`](smithay::desktop::PopupPointerGrab).
//!
//! Smithay ships `PopupKeyboardGrab` + `PopupPointerGrab` but no touch
//! equivalent, so an open menu (tray dbusmenu, app context menu) only
//! dismisses on a *pointer* press outside it — a touch tap elsewhere left it
//! stuck open. This grab closes that gap: while it is active, a touch-down on
//! a surface belonging to a different client than the grabbed popup calls
//! [`PopupGrab::ungrab`], which sends `xdg_popup.popup_done` (the same signal
//! the pointer path uses, and what the bar listens for to tear its menu down).
//!
//! Set alongside the pointer/keyboard popup grabs in
//! [`XdgShellHandler::grab`](crate::handlers); see `handlers/xdg_shell.rs`.

use smithay::{
    desktop::{PopupGrab, PopupUngrabStrategy},
    input::touch::{
        DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent, TouchGrab,
        TouchInnerHandle, UpEvent,
    },
    reexports::wayland_server::Resource,
    utils::{Logical, Point, Serial, SERIAL_COUNTER},
    wayland::seat::WaylandFocus,
};

use crate::state::ShoestringWm;

/// Touch counterpart to [`smithay::desktop::PopupPointerGrab`]. Wraps the same
/// shared [`PopupGrab`] so dismissing through touch also ends the keyboard and
/// pointer popup grabs.
pub struct PopupTouchGrab {
    popup_grab: PopupGrab<ShoestringWm>,
    /// The trait requires a `start_data`; touch popup grabs never have it read
    /// by smithay core, but we keep a valid one (seeded from the pointer grab's
    /// start focus/location) rather than risk an `unreachable!`.
    start_data: GrabStartData<ShoestringWm>,
}

impl PopupTouchGrab {
    pub fn new(popup_grab: &PopupGrab<ShoestringWm>) -> Self {
        let p = popup_grab.pointer_grab_start_data();
        let start_data = GrabStartData {
            focus: p.focus.clone(),
            slot: Default::default(),
            location: p.location,
        };
        PopupTouchGrab {
            popup_grab: popup_grab.clone(),
            start_data,
        }
    }

    /// True when `focus` is a surface of the same client as the grabbed popup —
    /// i.e. the touch landed inside the menu (or a sibling popup), so it should
    /// be delivered, not treated as a dismiss.
    fn focus_is_grab_client(
        &self,
        focus: &Option<(
            smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
            Point<f64, Logical>,
        )>,
    ) -> bool {
        focus
            .as_ref()
            .and_then(|(surf, _)| {
                self.popup_grab
                    .current_grab()
                    .map(|g| g.same_client_as(&surf.id()))
            })
            .unwrap_or(false)
    }
}

impl TouchGrab<ShoestringWm> for PopupTouchGrab {
    fn down(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        focus: Option<(
            smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
            Point<f64, Logical>,
        )>,
        event: &DownEvent,
        seq: Serial,
    ) {
        if self.popup_grab.has_ended() {
            handle.unset_grab(self, data);
            handle.down(data, focus, event, seq);
            return;
        }

        // Touch outside the grabbed popup's client → dismiss the whole popup
        // stack (sends popup_done) and end the grab, then still deliver the
        // down to whatever is underneath, matching the pointer grab.
        if !self.focus_is_grab_client(&focus) {
            let _ = self.popup_grab.ungrab(PopupUngrabStrategy::All);
            handle.unset_grab(self, data);
            handle.down(data, focus, event, seq);
            return;
        }

        handle.down(data, focus, event, seq);
    }

    fn up(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        event: &UpEvent,
        seq: Serial,
    ) {
        if self.popup_grab.has_ended() {
            handle.unset_grab(self, data);
        }
        handle.up(data, event, seq);
    }

    fn motion(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        focus: Option<(
            smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
            Point<f64, Logical>,
        )>,
        event: &MotionEvent,
        seq: Serial,
    ) {
        if self.popup_grab.has_ended() {
            handle.unset_grab(self, data);
            handle.motion(data, focus, event, seq);
            return;
        }
        // Keep motion within the grabbed client, mirroring the pointer grab's
        // owner-events behavior.
        if self.focus_is_grab_client(&focus) {
            handle.motion(data, focus, event, seq);
        } else {
            handle.motion(data, None, event, seq);
        }
    }

    fn frame(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        seq: Serial,
    ) {
        handle.frame(data, seq);
    }

    fn cancel(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        seq: Serial,
    ) {
        handle.cancel(data, seq);
    }

    fn shape(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        event: &ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut ShoestringWm,
        handle: &mut TouchInnerHandle<'_, ShoestringWm>,
        event: &OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &GrabStartData<ShoestringWm> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut ShoestringWm) {
        // Mirror PopupGrab::unset_keyboard_grab (private upstream): when this
        // grab ends, drop a matching keyboard popup grab and restore keyboard
        // focus to the popup root, so the keyboard isn't left captured by the
        // dismissed menu.
        let serial = SERIAL_COUNTER.next_serial();
        let grab_serial = self.popup_grab.serial();
        let prev_serial = self.popup_grab.previous_serial().unwrap_or(grab_serial);
        let Some(keyboard) = data.seat.get_keyboard() else {
            return;
        };
        if keyboard.is_grabbed()
            && (keyboard.has_grab(grab_serial) || keyboard.has_grab(prev_serial))
        {
            keyboard.unset_grab(data);
            keyboard.set_focus(data, self.popup_grab.current_grab(), serial);
        }
    }
}
