//! XWayland startup glue + XwmHandler implementation.
//!
//! ## Lifecycle
//!
//! - [`ShoestringWm::start_xwayland`] spawns the Xwayland subprocess. The
//!   actual X11 window manager state isn't created until Xwayland signals
//!   `Ready`; until then `state.xwm` is `None` and X11 clients fail with
//!   "Cannot open display" (same as before this module existed).
//! - On `Ready` we:
//!     1. Construct the [`X11Wm`] and stash it on the state.
//!     2. Set `$DISPLAY` so children spawned via [`std::process::Command`]
//!        inherit it. Apps launched *before* Xwayland is up don't get
//!        retroactive `$DISPLAY` — they need to be re-launched.
//!     3. Upload the WM's xcursor sprite to the X server so X11 apps see
//!        the user's themed cursor on first hover.
//!
//! ## Window mapping
//!
//! X11 toplevels and override-redirect popups both land in our regular
//! [`Space`] alongside xdg toplevels. The Space already speaks both
//! polymorphically via [`smithay::desktop::Window`]; the few places we
//! reach into `window.toplevel()` are guarded by [`crate::window_ext`].

use std::{os::unix::io::OwnedFd, process::Stdio};

use smithay::{
    desktop::Window,
    input::pointer::Focus,
    reexports::wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle},
    utils::{Logical, Point, Rectangle, Size, SERIAL_COUNTER},
    wayland::{
        seat::WaylandFocus,
        selection::{
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
            SelectionTarget,
        },
        xwayland_keyboard_grab::{XWaylandKeyboardGrabHandler, XWaylandKeyboardGrabState},
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    },
};

use crate::layout::LayoutState;

use crate::state::ShoestringWm;

impl ShoestringWm {
    /// Spawn Xwayland as a subprocess. Returns silently on failure — the
    /// compositor still functions, just without X11 client support.
    /// Invoked once at startup after the wayland socket is listening.
    pub fn start_xwayland(&mut self) {
        let (xwayland, client) = match XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            Stdio::null(),
            |_| (),
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "xwayland disabled: spawn failed");
                return;
            }
        };

        let display_handle = self.display_handle.clone();
        let ret = self
            .loop_handle
            .insert_source(xwayland, move |event, _, state| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    let wm = match X11Wm::start_wm(
                        state.loop_handle.clone(),
                        &display_handle,
                        x11_socket,
                        client.clone(),
                    ) {
                        Ok(wm) => wm,
                        Err(e) => {
                            tracing::warn!(error = %e, "xwayland: failed to attach X11 WM");
                            return;
                        }
                    };
                    state.xwm = Some(wm);
                    state.xdisplay = Some(display_number);
                    let display_str = format!(":{}", display_number);
                    // Apps spawned by the WM (autostart, dispatch-action
                    // spawn, run_command, etc.) inherit our env; setting
                    // $DISPLAY makes X11 clients find Xwayland.
                    std::env::set_var("DISPLAY", &display_str);
                    // Only the real session pushes DISPLAY into systemd; a
                    // nested instance would retarget the host session's
                    // D-Bus-activated services at this throwaway Xwayland.
                    if state.session_integration {
                        crate::import_systemd_env(&["DISPLAY"]);
                    }
                    tracing::info!(display = %display_str, "xwayland ready");
                    state.refresh_xwayland_cursor();
                }
                XWaylandEvent::Error => {
                    tracing::warn!("xwayland crashed on startup");
                }
            });
        if let Err(e) = ret {
            tracing::warn!(error = %e, "xwayland disabled: loop insert failed");
        }
    }

    /// Push the current xcursor sprite into the X11 WM so X11 apps see
    /// the user's themed cursor on first hover. Called once on Ready.
    /// Best-effort: failures log at debug level and the X server falls
    /// back to its own default.
    pub fn refresh_xwayland_cursor(&mut self) {
        let Some(wm) = self.xwm.as_mut() else { return };
        let elapsed = self.start_time.elapsed();
        let Some(frame) = self
            .cursor
            .current_frame(smithay::input::pointer::CursorIcon::Default, 1, elapsed)
            .cloned()
        else {
            return;
        };
        if let Err(e) = wm.set_cursor(
            &frame.pixels_rgba,
            Size::from((frame.width as u16, frame.height as u16)),
            Point::from((frame.xhot as u16, frame.yhot as u16)),
        ) {
            tracing::debug!(error = %e, "xwayland: set_cursor failed");
        }
    }

    /// Locate the [`Window`] currently wrapping `surface` in the Space.
    fn space_element_for_x11(&self, surface: &X11Surface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.x11_surface().is_some_and(|s| s == surface))
            .cloned()
    }
}

impl XWaylandShellHandler for ShoestringWm {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl XWaylandKeyboardGrabHandler for ShoestringWm {
    fn keyboard_focus_for_xsurface(
        &self,
        surface: &WlSurface,
    ) -> Option<crate::focus::KeyboardFocusTarget> {
        // X11 apps that request a keyboard grab on a child surface (popup
        // menus, etc.) need focus to follow the grab. Match the toplevel
        // window owning `surface` and return its keyboard-focus target (the
        // X11 variant, so X input focus is driven correctly).
        self.space
            .elements()
            .find(|w| w.wl_surface().is_some_and(|s| &*s == surface))
            .and_then(crate::window_ext::keyboard_focus)
    }
}

impl XwmHandler for ShoestringWm {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm_state called without xwm")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(e) = surface.set_mapped(true) {
            tracing::warn!(error = %e, "xwayland: set_mapped(true) failed");
            return;
        }
        let window = Window::new_x11_window(surface.clone());
        let pos = self
            .seat
            .get_pointer()
            .map(|p| p.current_location().to_i32_round())
            .unwrap_or_default();
        let active_ws = self.workspaces.active();
        self.space.map_element(window.clone(), pos, false);
        self.workspaces.assign(window.clone(), active_ws, pos);
        self.pending_initial_center.insert(window.clone());

        // Tell the X client what bbox the compositor expects so it can
        // draw at the right size from the first frame.
        let bbox = self
            .space
            .element_bbox(&window)
            .unwrap_or_else(|| Rectangle::new(pos, surface.geometry().size));
        let _ = surface.configure(Some(bbox));

        // Register with ext-foreign-toplevel-list so bars / IPC see the
        // window alongside xdg toplevels. X11 title/class don't arrive
        // via wayland commits — seed them here from what the X server
        // already knows.
        let title = surface.title();
        let app_id = surface.class();
        let handle = self
            .foreign_toplevel_list
            .new_toplevel::<crate::state::ShoestringWm>(&title, &app_id);
        let id = handle.identifier();
        self.foreign_toplevels.insert(window.clone(), handle);
        // Announce to wlr-foreign-toplevel-management taskbars before focusing.
        crate::foreign_toplevel_mgmt::announce_to_all_managers(self, &window);
        self.emit_ipc(shoestring_ipc::Event::WindowOpened {
            id,
            title,
            app_id,
            workspace: active_ws.one_based(),
        });

        self.try_apply_window_rules(&window);
        self.focus_window(&window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        // Override-redirect windows position themselves (popups, tooltips,
        // menus). Honor the geometry the client gave us; do not run the
        // tiling / window-rules pipeline.
        let location = surface.geometry().loc;
        let window = Window::new_x11_window(surface);
        self.space.map_element(window, location, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.space_element_for_x11(&surface) {
            // Tell wlr-foreign-toplevel-management taskbars the window is gone.
            crate::foreign_toplevel_mgmt::close_toplevel(self, &window);
            self.space.unmap_elem(&window);
            self.layout.forget(&window);
            self.workspaces.forget(&window);
            self.pending_initial_center.remove(&window);
            self.rules_applied.remove(&window);
            self.sticky.remove(&window);
            self.always_on_top.remove(&window);
            self.window_name_overrides.remove(&window);
            // Same FT cleanup the xdg destroy path runs — see
            // shoestring_wm_ft_drop memory note: dropping our Arc on the
            // handle is not enough, must explicitly remove_toplevel.
            if let Some(handle) = self.foreign_toplevels.remove(&window) {
                let id = handle.identifier();
                self.foreign_toplevel_list.remove_toplevel(&handle);
                self.emit_ipc(shoestring_ipc::Event::WindowClosed { id });
            }
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Honor size hints but ignore self-move — managed windows don't
        // get to pick their own position.
        let mut geo = surface.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = surface.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let Some(window) = self.space_element_for_x11(&surface) else {
            return;
        };
        // Mirror the X-side geometry back into the Space so input hits
        // the right area. Override-redirect popups land here when they
        // move themselves.
        self.space.map_element(window, geometry.loc, false);
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.space_element_for_x11(&surface) {
            self.focus_window(&window);
            self.window_layout_action(LayoutState::Maximized);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.space_element_for_x11(&surface) {
            self.focus_window(&window);
            // window_layout_action toggles when the focused window is
            // already in the requested state, restoring the saved floating
            // rect — same path the keybind takes.
            self.window_layout_action(LayoutState::Maximized);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        // fullscreen_window → apply_geometry sets the X-side fullscreen property
        // and configures the surface, so no manual set_fullscreen here. X11 has
        // no per-output hint in this request, so fullscreen on the current one.
        if let Some(window) = self.space_element_for_x11(&surface) {
            self.focus_window(&window);
            self.fullscreen_window(&window, None);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.space_element_for_x11(&surface) {
            self.focus_window(&window);
            self.unfullscreen_window(&window);
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _button: u32,
        edges: X11ResizeEdge,
    ) {
        let Some(window) = self.space_element_for_x11(&surface) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(loc) = self.space.element_location(&window) else {
            return;
        };
        let initial = Rectangle::new(loc, window.geometry().size);
        let grab = crate::grabs::ResizeSurfaceGrab::start(
            start_data,
            window.clone(),
            edges.into(),
            initial,
        );
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        let Some(window) = self.space_element_for_x11(&surface) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(initial_location) = self.space.element_location(&window) else {
            return;
        };
        let grab = crate::grabs::MoveSurfaceGrab::new(start_data, window.clone(), initial_location);
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Only forward when an X11 window owns keyboard focus, so a
        // wayland-native app reading the clipboard doesn't accidentally
        // pull the X-side selection.
        let Some(kb) = self.seat.get_keyboard() else {
            return false;
        };
        let Some(focused) = kb.current_focus().and_then(|f| f.surface()) else {
            return false;
        };
        self.space.elements().any(|w| {
            let same_surface = w.wl_surface().is_some_and(|s| *s == focused);
            same_surface
                && w.x11_surface()
                    .and_then(|x| x.xwm_id())
                    .is_some_and(|id| id == xwm)
        })
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(e) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    tracing::warn!(?e, "xwayland: clipboard fetch failed");
                }
            }
            SelectionTarget::Primary => {
                if let Err(e) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    tracing::warn!(?e, "xwayland: primary fetch failed");
                }
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ())
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat)
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat)
                }
            }
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.xdisplay = None;
    }
}

/// Initialise the wayland-side globals XWayland depends on. Must be
/// called once during state construction, *before* [`ShoestringWm::start_xwayland`].
pub fn init_xwayland_globals(dh: &DisplayHandle) -> XWaylandShellState {
    XWaylandKeyboardGrabState::new::<ShoestringWm>(dh);
    XWaylandShellState::new::<ShoestringWm>(dh)
}
