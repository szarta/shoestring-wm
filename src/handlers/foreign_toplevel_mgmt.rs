//! Dispatch impls for `zwlr_foreign_toplevel_management_v1`.
//! State and broadcast helpers live in [`crate::foreign_toplevel_mgmt`].

use smithay::{
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
        },
        wayland_server::{backend::ClientId, Client, DataInit, DisplayHandle, New, Resource},
    },
    utils::IsAlive,
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{
    foreign_toplevel_mgmt::{ForeignToplevelHandleData, ForeignToplevelManagerData},
    layout::{self, LayoutState},
    state::ShoestringWm,
};

// ── Manager global ──────────────────────────────────────────────────────────────

impl GlobalDispatch2<ZwlrForeignToplevelManagerV1, ShoestringWm> for ForeignToplevelManagerData {
    fn bind(
        &self,
        state: &mut ShoestringWm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        let manager = data_init.init(resource, ForeignToplevelManagerData);
        crate::foreign_toplevel_mgmt::announce_existing_to_manager(state, &manager);
        state.foreign_toplevel_mgmt.managers.push(manager);
    }
}

impl Dispatch2<ZwlrForeignToplevelManagerV1, ShoestringWm> for ForeignToplevelManagerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            // `finished` is a destructor event — the resource is dead after it.
            resource.finished();
            state
                .foreign_toplevel_mgmt
                .managers
                .retain(|m| m != resource);
        }
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
    ) {
        state
            .foreign_toplevel_mgmt
            .managers
            .retain(|m| m != resource);
    }
}

// ── Handle (one per window × manager) ────────────────────────────────────────────

impl Dispatch2<ZwlrForeignToplevelHandleV1, ShoestringWm> for ForeignToplevelHandleData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;

        // `destroy` is valid even after the toplevel is gone (inert handle); all
        // other requests are ignored once the window has been destroyed.
        if let Request::Destroy = request {
            return;
        }
        let window = self.window.clone();
        if !window.alive() {
            return;
        }

        match request {
            Request::Activate { .. } => state.activate_window(&window),
            Request::Close => crate::window_ext::send_close(&window),
            Request::SetMinimized => state.minimize_window(&window),
            Request::UnsetMinimized => state.unminimize_window(&window),
            // set_layout toggles, so guard on the current state: only act when a
            // real change is needed (already-correct requests fall through).
            Request::SetMaximized
                if state.layout.layout_state(&window) != LayoutState::Maximized =>
            {
                layout::set_layout(
                    &mut state.space,
                    &mut state.layout,
                    &window,
                    LayoutState::Maximized,
                );
                crate::foreign_toplevel_mgmt::sync_wlr_toplevel(state, &window);
            }
            Request::UnsetMaximized
                if state.layout.layout_state(&window) == LayoutState::Maximized =>
            {
                layout::set_layout(
                    &mut state.space,
                    &mut state.layout,
                    &window,
                    LayoutState::Maximized,
                );
                crate::foreign_toplevel_mgmt::sync_wlr_toplevel(state, &window);
            }
            // A minimize-animation hint the WM doesn't use; reject only an
            // invalid (negative) rectangle, otherwise ignore.
            Request::SetRectangle { width, height, .. } if width < 0 || height < 0 => {
                resource.post_error(
                    zwlr_foreign_toplevel_handle_v1::Error::InvalidRectangle,
                    "set_rectangle: width/height must be non-negative",
                );
            }
            // Everything else is a no-op: already-correct max/unmax, valid
            // set_rectangle, set/unset_fullscreen (no WM fullscreen concept),
            // and destroy (cleanup happens in `destroyed`).
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
    ) {
        let mgmt = &mut state.foreign_toplevel_mgmt;
        let mut now_empty = false;
        if let Some(v) = mgmt.handles.get_mut(&self.window) {
            v.retain(|h| h != resource);
            now_empty = v.is_empty();
        }
        if now_empty {
            mgmt.handles.remove(&self.window);
            mgmt.last_outputs.remove(&self.window);
        }
    }
}
