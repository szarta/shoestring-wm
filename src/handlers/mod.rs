mod compositor;
mod dmabuf;
mod ext_workspace;
mod foreign_toplevel_mgmt;
#[cfg(feature = "tty")]
mod gamma_control;
mod layer_shell;
mod output_management;
mod screencopy;
pub(crate) mod session_lock;
mod virtual_pointer;
mod xdg_activation;
mod xdg_decoration;
mod xdg_foreign;
mod xdg_shell;

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::{Focus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{get_parent, with_states};
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelListHandler, ForeignToplevelListState,
};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler};
use smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;

use crate::state::ShoestringWm;

impl ForeignToplevelListHandler for ShoestringWm {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list
    }
}

impl smithay::wayland::idle_notify::IdleNotifierHandler for ShoestringWm {
    fn idle_notifier_state(
        &mut self,
    ) -> &mut smithay::wayland::idle_notify::IdleNotifierState<Self> {
        // Only reached when a client holds an ext_idle_notify resource, which
        // requires the global — and the global only exists when the option is
        // on, i.e. when `idle_notifier` is `Some`. See [`ShoestringWm::new`].
        self.idle_notifier
            .as_mut()
            .expect("ext_idle_notify dispatched without an IdleNotifierState")
    }
}

impl smithay::wayland::idle_inhibit::IdleInhibitHandler for ShoestringWm {
    fn inhibit(&mut self, surface: WlSurface) {
        // A client asked to inhibit idle for this surface. Track it and
        // recompute — it only actually inhibits while the surface is visible.
        self.idle_inhibitors.push(surface);
        self.refresh_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        // Remove one inhibitor for this surface (Vec, so duplicates are
        // reference-counted by position). Smithay calls this only on an
        // explicit destroy; client-death is handled by the dead-surface
        // prune in refresh_idle_inhibit.
        if let Some(pos) = self.idle_inhibitors.iter().position(|s| *s == surface) {
            self.idle_inhibitors.remove(pos);
        }
        self.refresh_idle_inhibit();
    }
}

impl smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitHandler
    for ShoestringWm
{
    fn keyboard_shortcuts_inhibit_state(
        &mut self,
    ) -> &mut smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(
        &mut self,
        inhibitor: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor,
    ) {
        // Grant every request (single-user WM — no privilege model), but only
        // arm it if its surface already holds keyboard focus. Otherwise it
        // stays dormant until `focus_changed` activates it on focus entry.
        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        if focused.as_ref() == Some(inhibitor.wl_surface()) {
            inhibitor.activate();
            self.active_shortcuts_inhibitor = Some(inhibitor);
        }
    }

    fn inhibitor_destroyed(
        &mut self,
        inhibitor: smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor,
    ) {
        // The client tore down (or died with) its inhibitor; smithay has
        // already dropped it from the seat list. Forget our cached handle so we
        // don't try to deactivate a dead object on the next focus change.
        if self.active_shortcuts_inhibitor.as_ref() == Some(&inhibitor) {
            self.active_shortcuts_inhibitor = None;
        }
    }
}

impl smithay::wayland::input_method::InputMethodHandler for ShoestringWm {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        // The IME candidate window. Tracking it in the PopupManager is all it
        // takes to draw — smithay's window render walks `popups_for_surface`
        // for the parent toplevel, and this popup is parented to it.
        if let Err(err) = self
            .popups
            .track_popup(smithay::desktop::PopupKind::from(surface))
        {
            tracing::warn!(?err, "failed to track input-method popup");
        }
    }

    fn popup_repositioned(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = smithay::desktop::PopupManager::dismiss_popup(
                &parent,
                &smithay::desktop::PopupKind::from(surface),
            );
        }
    }

    fn parent_geometry(
        &self,
        parent: &WlSurface,
    ) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        // Geometry of the toplevel the popup hangs off, so smithay can place
        // the candidate box against the client's reported text-cursor rect.
        self.space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(parent)).then(|| window.geometry())
            })
            .unwrap_or_default()
    }
}

impl SeatHandler for ShoestringWm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image.clone();
        self.pointer_element.set_status(image);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);

        // Keyboard-shortcuts inhibit follows focus: at most one inhibitor is
        // active at a time, always the one on the focused surface. Deactivate
        // whatever we had armed, then arm the newly-focused surface's inhibitor
        // (if any). The `active`/`inactive` events tell the client whether its
        // shortcuts are actually being passed through right now.
        let next = focused.and_then(|s| seat.keyboard_shortcuts_inhibitor_for_surface(s));
        if self.active_shortcuts_inhibitor != next {
            if let Some(prev) = self.active_shortcuts_inhibitor.take() {
                prev.inactivate();
            }
            if let Some(inhibitor) = &next {
                inhibitor.activate();
            }
            self.active_shortcuts_inhibitor = next;
        }
    }
}

impl smithay::wayland::tablet_manager::TabletSeatHandler for ShoestringWm {
    fn tablet_tool_image(
        &mut self,
        _tool: &smithay::backend::input::TabletToolDescriptor,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        // A stylus can carry its own cursor sprite. Route it through the same
        // pointer-cursor element the mouse uses — a tablet tool drives the one
        // shared pointer, so one cursor is correct.
        self.cursor_status = image.clone();
        self.pointer_element.set_status(image);
    }
}

impl PointerConstraintsHandler for ShoestringWm {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // A constraint takes effect only while the cursor is over the surface
        // that requested it. If the pointer is already there, activate now;
        // otherwise the input motion handler activates it on entry. (Smithay
        // auto-deactivates on pointer leave — see its seat::pointer impl — so
        // there's no matching teardown here.)
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn remove_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // When the surface's last constraint is gone, drop the cursor back to
        // wherever the (formerly locked, hence hidden) client said it was
        // drawing one, so it doesn't reappear at a stale location.
        if with_pointer_constraint(surface, pointer, |c| c.is_none()) {
            if let Some((hint_surface, hint_loc)) = self.pointer_constraint_cursor_hint.take() {
                if &hint_surface == surface {
                    if let Some(origin) = self.surface_global_origin(surface) {
                        pointer.set_location(origin + hint_loc);
                    }
                }
            }
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        // Only meaningful while the constraint is active (a locked pointer
        // hides the cursor); stash it for `remove_constraint` to restore.
        if with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active())) {
            self.pointer_constraint_cursor_hint = Some((surface.clone(), location));
        }
    }
}

impl SelectionHandler for ShoestringWm {
    type SelectionUserData = ();
}

impl DataDeviceHandler for ShoestringWm {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PrimarySelectionHandler for ShoestringWm {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl DndGrabHandler for ShoestringWm {}
impl WaylandDndGrabHandler for ShoestringWm {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => source.cancel(),
        }
    }
}

impl OutputHandler for ShoestringWm {
    fn output_bound(
        &mut self,
        _output: smithay::output::Output,
        wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        // Late `wl_output` bind: tell any ext-workspace group of this client
        // about the now-bound output (see [`crate::ext_workspace::notify_output_bound`]).
        crate::ext_workspace::notify_output_bound(self, &wl_output);
    }
}

impl FractionalScaleHandler for ShoestringWm {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // A client just bound wp_fractional_scale for this surface. Seed it
        // with the scale of the output it lives on so it can render natively
        // from the first frame; [`crate::scale::send_preferred_scale`] keeps it
        // current afterwards. Walk to the root so subsurfaces inherit the
        // toplevel's output, and fall back to the first output when the surface
        // isn't mapped yet (the value is still correct on a single-scale setup).
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        let output = self
            .space
            .elements()
            .find(|w| crate::window_ext::matches_surface(w, &root))
            .and_then(|w| self.space.outputs_for_element(w).first().cloned())
            .or_else(|| self.space.outputs().next().cloned());
        if let Some(output) = output {
            let scale = output.current_scale().fractional_scale();
            with_states(&surface, |states| {
                with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(scale);
                });
            });
        }
    }
}

smithay::delegate_dispatch2!(ShoestringWm);
