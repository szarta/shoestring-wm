use smithay::{
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager, Space, Window,
    },
    reexports::wayland_server::protocol::{wl_seat, wl_surface::WlSurface},
    utils::Serial,
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
        let location = center_for_new_window(&self.space, &window);
        let active_ws = self.workspaces.active();
        tracing::info!(
            ?location,
            workspace = active_ws.one_based(),
            "mapping new toplevel"
        );
        self.space.map_element(window.clone(), location, false);
        self.workspaces.assign(window.clone(), active_ws, location);
        // Register with ext-foreign-toplevel-list so bars see this window.
        // Title/app_id arrive on later commits; sync_foreign_toplevel pushes
        // them when they change.
        let handle = self
            .foreign_toplevel_list
            .new_toplevel::<crate::state::ShoestringWm>("", "");
        self.foreign_toplevels.insert(window.clone(), handle);
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
        self.space.unmap_elem(&window);
        self.layout.forget(&window);
        self.workspaces.forget(&window);
        // FT handle's Drop sends `closed`; just removing the entry suffices.
        self.foreign_toplevels.remove(&window);
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
    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
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
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
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
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &root)
        else {
            return;
        };
        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
