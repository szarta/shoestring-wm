use smithay::{
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, layer_map_for_output,
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space,
        Window, WindowSurfaceType,
    },
    input::{pointer::Focus, Seat},
    reexports::wayland_server::protocol::{wl_output, wl_seat, wl_surface::WlSurface},
    utils::{Logical, Serial, Size},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::state::ShoestringWm;

impl XdgShellHandler for ShoestringWm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        // Hint the available area to the client before the initial configure
        // so first-launch apps (e.g. firefox cold start) pick a sensible size
        // instead of falling back to their own hardcoded default. Mirrors
        // anvil's place_new_window. Uses the layer-shell non-exclusive zone
        // so a top bar / dock is excluded from the hint.
        if let Some(bounds) = usable_size_for_new_window(&self.space) {
            window.toplevel().unwrap().with_pending_state(|state| {
                state.bounds = Some(bounds);
            });
        }
        let location = center_for_new_window(&self.space, &window);
        let active_ws = self.workspaces.active();
        tracing::info!(
            ?location,
            workspace = active_ws.one_based(),
            "mapping new toplevel"
        );
        self.space.map_element(window.clone(), location, false);
        self.workspaces.assign(window.clone(), active_ws, location);
        // `location` here is a placeholder — the client hasn't picked a
        // size yet so we can't center properly. Mark for re-centering on
        // the first commit (see try_recenter_pending). Window rules that
        // set an explicit position will clear this flag.
        self.pending_initial_center.insert(window.clone());
        // Register with ext-foreign-toplevel-list so bars see this window.
        // Title/app_id arrive on later commits; sync_foreign_toplevel pushes
        // them when they change.
        let handle = self
            .foreign_toplevel_list
            .new_toplevel::<crate::state::ShoestringWm>("", "");
        let id = handle.identifier();
        self.foreign_toplevels.insert(window.clone(), handle);
        // Announce to wlr-foreign-toplevel-management taskbars before focusing,
        // so the new window has handles when focus_window broadcasts activation.
        crate::foreign_toplevel_mgmt::announce_to_all_managers(self, &window);
        self.emit_ipc(shoestring_ipc::Event::WindowOpened {
            id,
            title: String::new(),
            app_id: String::new(),
            workspace: active_ws.one_based(),
        });
        // Auto-focus newly mapped windows so the user doesn't have to click
        // them first. Matches the focusNew=yes Openbox behavior.
        self.focus_window(&window);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.find_window(&surface) else {
            // Already cleaned up somewhere — nothing to do.
            return;
        };
        // Tell wlr-foreign-toplevel-management taskbars the window is gone.
        crate::foreign_toplevel_mgmt::close_toplevel(self, &window);
        self.space.unmap_elem(&window);
        self.layout.forget(&window);
        self.workspaces.forget(&window);
        self.rules_applied.remove(&window);
        self.sticky.remove(&window);
        self.always_on_top.remove(&window);
        self.pending_initial_center.remove(&window);
        self.window_name_overrides.remove(&window);
        // Drop on our Arc clone is NOT enough: smithay's `new_toplevel`
        // hands each bound client (the bar, etc.) its own Arc clone via
        // the resource user-data, so `closed` is only sent once the LAST
        // clone goes away — which never happens until the client itself
        // disconnects. Explicitly route through `remove_toplevel`, which
        // calls `send_closed` and prunes the list state's Weak entry.
        let id = self.foreign_toplevels.remove(&window).map(|h| {
            let id = h.identifier();
            self.foreign_toplevel_list.remove_toplevel(&h);
            id
        });
        if let Some(id) = id {
            self.emit_ipc(shoestring_ipc::Event::WindowClosed { id });
        }
        // A closing player drops out of the space; recompute idle inhibition.
        // Its inhibitor surface dies too, so the dead-surface prune clears it.
        self.refresh_idle_inhibit();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    // App-driven fullscreen (games, mpv, fullscreen video). `output` is the
    // client's preferred monitor, if any. unset restores the prior layout.
    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        output: Option<wl_output::WlOutput>,
    ) {
        if let Some(window) = self.find_window(&surface) {
            self.fullscreen_window(&window, output);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.find_window(&surface) {
            self.unfullscreen_window(&window);
        }
    }

    // move/resize requests are wired up in M3 (Super+drag pointer grabs).
    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
    }
    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge,
    ) {
    }
    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        // Wire the popup grab so menus (tray dbusmenus, app context menus)
        // dismiss on click-outside and route keyboard/pointer correctly. Our
        // keyboard/pointer focus type is `WlSurface`, so the popup's root
        // surface (window or layer surface) IS the focus target — no enum
        // mapping needed. Mirrors anvil's XdgShellHandler::grab.
        let seat: Seat<Self> = Seat::from_resource(&seat).unwrap();
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
}

/// Size of the usable (non-exclusive) area on the output a freshly-mapped
/// window will land on. Used to populate `xdg_toplevel.bounds` before the
/// initial configure.
fn usable_size_for_new_window(space: &Space<Window>) -> Option<Size<i32, Logical>> {
    let output = space.outputs().next()?;
    Some(layer_map_for_output(output).non_exclusive_zone().size)
}

/// Center a freshly-mapped window on the first available output. The window's
/// own geometry is usually `(0, 0)` until its first configure round-trip
/// completes, so this picks a sensible spawn location for the common case.
fn center_for_new_window(
    space: &Space<Window>,
    window: &Window,
) -> smithay::utils::Point<i32, smithay::utils::Logical> {
    let Some(output) = space.outputs().next() else {
        return (0, 0).into();
    };
    let Some(output_geo) = space.output_geometry(output) else {
        return (0, 0).into();
    };
    let win_size = window.geometry().size;
    let x = output_geo.loc.x + (output_geo.size.w - win_size.w) / 2;
    let y = output_geo.loc.y + (output_geo.size.h - win_size.h) / 2;
    (x.max(output_geo.loc.x), y.max(output_geo.loc.y)).into()
}

pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    // X11 windows live in Space too; the predicate has to be safe for them
    // (returns false rather than panicking on a non-xdg window).
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    xdg.send_configure()
                        .expect("initial popup configure failed");
                }
            }
            PopupKind::InputMethod(_) => {}
        }
    }
}

impl ShoestringWm {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };

        // Window-parented popup (app context menus): constrain within the
        // output, relative to the owning window's position.
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        {
            let Some(output) = self.space.outputs().next() else {
                return;
            };
            let (Some(output_geo), Some(window_geo)) = (
                self.space.output_geometry(output),
                self.space.element_geometry(window),
            ) else {
                return;
            };
            let mut target = output_geo;
            target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
            target.loc -= window_geo.loc;
            popup.with_pending_state(|state| {
                state.geometry = state.positioner.get_unconstrained_geometry(target);
            });
            return;
        }

        // Layer-surface-parented popup (e.g. shoestring-bar's tray menu, via
        // zwlr_layer_surface_v1.get_popup): constrain within the output the
        // layer surface lives on, relative to the layer surface's position.
        for output in self.space.outputs() {
            let map = layer_map_for_output(output);
            if let Some(layer) = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL) {
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let layer_geo = map.layer_geometry(layer).unwrap_or_default();
                let mut target = output_geo;
                target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
                target.loc -= layer_geo.loc;
                popup.with_pending_state(|state| {
                    state.geometry = state.positioner.get_unconstrained_geometry(target);
                });
                return;
            }
        }
    }
}
