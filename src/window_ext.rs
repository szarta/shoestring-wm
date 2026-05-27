//! Polymorphic helpers for [`smithay::desktop::Window`] that hide the
//! xdg / xwayland branch.
//!
//! [`Window`] internally wraps either an xdg `ToplevelSurface` or an
//! [`X11Surface`]. Most operations on Window are already polymorphic
//! (mapping, geometry, activation), but the wayland-protocol-shaped
//! ones — sending close, sending pending configures, fetching the
//! surface that should receive keyboard focus — go down different
//! code paths. These free functions let the rest of the WM stay
//! ignorant of which kind of window it's holding.

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
    wayland::seat::WaylandFocus,
};

/// The [`WlSurface`] that should receive keyboard focus for `window`.
///
/// - xdg toplevels: the toplevel's own `wl_surface`.
/// - X11 toplevels: the wayland surface XWayland binds via
///   `xwayland_shell_v1`. May be `None` if the X11 client hasn't
///   committed its surface yet — caller should skip focus updates
///   rather than panic.
pub fn focus_surface(window: &Window) -> Option<WlSurface> {
    window.wl_surface().map(|s| s.into_owned())
}

/// Push a pending configure to `window` (xdg-only). X11 surfaces ack
/// their configure synchronously inside the XwmHandler request, so
/// there's nothing to flush here.
pub fn send_pending_configure(window: &Window) {
    if let Some(t) = window.toplevel() {
        t.send_pending_configure();
    }
}

/// Ask the client to close `window`. xdg sends `xdg_toplevel.close`;
/// X11 sends `WM_DELETE_WINDOW` (or terminates the client if the
/// window opts out of the protocol).
pub fn send_close(window: &Window) {
    if let Some(t) = window.toplevel() {
        t.send_close();
    } else if let Some(x) = window.x11_surface() {
        x.close().ok();
    }
}

/// Compare a [`Window`] to a [`WlSurface`] for equality. Cheap to call
/// in `find()` predicates; works for both window kinds.
pub fn matches_surface(window: &Window, surface: &WlSurface) -> bool {
    window.wl_surface().is_some_and(|s| &*s == surface)
}

/// Push `loc` (compositor-space coordinates) to the X11 client as its
/// root-window position. No-op for xdg windows — wayland clients don't
/// know their on-screen coordinates and don't need to.
///
/// X11 popup menus (GIMP's File menu, etc.) compute their on-screen
/// position by asking the X server where their parent toplevel lives in
/// root coords; if we move the window via `space.map_element` without
/// also calling `x11.configure`, that root-coord cache stays stale and
/// the popup spawns at the wrong place until something else (a resize,
/// a tile) re-syncs it.
pub fn sync_x11_location(window: &Window, loc: Point<i32, Logical>) {
    let Some(x11) = window.x11_surface() else {
        return;
    };
    let size = window.geometry().size;
    let _ = x11.configure(Rectangle::new(loc, size));
}
