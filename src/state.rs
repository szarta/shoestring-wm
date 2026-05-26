use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::Instant,
};

use shoestring_config::Config;
use smithay::{
    desktop::{layer_map_for_output, PopupManager, Space, Window, WindowSurfaceType},
    input::{pointer::CursorImageStatus, Seat, SeatState},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        session_lock::SessionLockManagerState,
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

/// `(pointer_element_snapshot, pointer_location, (hotspot_x, hotspot_y))`.
pub type CursorSnapshot = (
    crate::drawing::PointerElement,
    smithay::utils::Point<f64, smithay::utils::Logical>,
    (i32, i32),
);

pub struct ShoestringWm {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, Self>,

    pub config: Config,
    pub config_path: Option<std::path::PathBuf>,
    pub bindings: crate::binds::BindingTable,
    /// `Some` once [`Self::start_config_watcher`] succeeds. Held purely
    /// for its `Drop` — the calloop source forwarding filesystem events
    /// is registered separately and is what actually drives reload.
    pub config_watcher: Option<crate::config_watcher::ConfigWatcher>,
    /// Token for an in-flight debounce `Timer`. Removed and re-inserted
    /// on every filesystem event so a continuous edit burst defers the
    /// reload to the trailing edge.
    pub pending_reload_token: Option<smithay::reexports::calloop::RegistrationToken>,

    pub space: Space<Window>,
    pub popups: PopupManager,
    pub layout: crate::layout::LayoutManager,
    pub workspaces: crate::workspace::WorkspaceManager,

    #[cfg(feature = "tty")]
    pub udev: Option<crate::backend::udev::UdevData>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub foreign_toplevel_list: ForeignToplevelListState,
    /// FT handle per window. Drop sends `closed`, so removing the entry on
    /// destroy is sufficient cleanup. Title/app_id changes are pushed in
    /// [`crate::handlers::compositor`]'s commit hook.
    pub foreign_toplevels: HashMap<Window, ForeignToplevelHandle>,
    pub shm_state: ShmState,
    // Held for the lifetime of the WM so its globals stay registered with the display.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub screencopy: crate::screencopy::ScreencopyState,
    pub session_lock_state: SessionLockManagerState,
    /// `Some` while a session lock is active. See
    /// [`crate::handlers::session_lock`] for the protocol wiring.
    pub lock_session: Option<crate::handlers::session_lock::LockState>,

    pub seat: Seat<Self>,

    pub ipc: Option<crate::ipc::Server>,
    /// Runtime gate for remote-automation IPC methods (inject_key/text/click,
    /// future remote screenshot + command exec). Initialised from
    /// `general.automation_enabled` and overridable at runtime via
    /// `Request::SetAutomation` and at startup via `--enable-automation`.
    /// Never written back to disk; the config file stays the source of
    /// truth at next start.
    pub automation_enabled: bool,

    /// In-flight `Request::Screenshot` subprocesses, keyed by an
    /// opaque counter. Entries are removed once the child has exited
    /// and the deferred IPC response has been written.
    pub pending_screenshots: HashMap<u64, crate::remote_screenshot::Pending>,
    pub next_screenshot_id: u64,

    /// In-flight `Request::RunCommand` subprocesses; same lifecycle as
    /// `pending_screenshots` but with optional timer-based SIGKILL.
    pub pending_commands: HashMap<u64, crate::remote_command::Pending>,
    pub next_command_id: u64,

    /// Windows that have already had `[[window_rules]]` evaluated.
    /// Evaluation runs once per window — on the first commit after
    /// map. Entries are removed in `toplevel_destroyed`.
    pub rules_applied: HashSet<Window>,

    pub cursor: crate::cursor::Cursor,
    pub cursor_status: CursorImageStatus,
    pub pointer_element: crate::drawing::PointerElement,
    /// Last image we uploaded as a `MemoryRenderBuffer`. We re-use the buffer
    /// when the chosen xcursor frame is unchanged across renders (the common
    /// case — static cursors stay on a single frame indefinitely).
    pub pointer_image: Option<xcursor::parser::Image>,
}

impl ShoestringWm {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
        config_path: Option<std::path::PathBuf>,
    ) -> Self {
        let dh = display.handle();

        let (bindings, bind_warnings) = crate::binds::BindingTable::compile(&config);
        for w in bind_warnings {
            tracing::warn!(target: "shoestring_wm::config", "{w}");
        }
        bindings.log_compiled();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let foreign_toplevel_list = ForeignToplevelListState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let screencopy = crate::screencopy::ScreencopyState {
            manager_global: dh
                .create_global::<Self, _, _>(3, crate::screencopy::ScreencopyManagerData),
            pending: Vec::new(),
        };
        // Filter: every client may see ext-session-lock — clients without
        // permission to actually lock just receive `finished` when they try.
        // Our own gating (single locker at a time) lives in the handler.
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");
        seat.add_keyboard(
            Default::default(),
            config.general.repeat_delay,
            config.general.repeat_rate,
        )
        .unwrap();
        seat.add_pointer();

        let automation_enabled = config.general.automation_enabled;

        let space = Space::default();
        let popups = PopupManager::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();
        let loop_handle = event_loop.handle();

        Self {
            start_time: Instant::now(),
            socket_name,
            display_handle: dh,
            loop_signal,
            loop_handle,
            config,
            config_path,
            bindings,
            config_watcher: None,
            pending_reload_token: None,
            space,
            popups,
            layout: crate::layout::LayoutManager::default(),
            workspaces: crate::workspace::WorkspaceManager::default(),
            #[cfg(feature = "tty")]
            udev: None,
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            foreign_toplevel_list,
            foreign_toplevels: HashMap::new(),
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            screencopy,
            session_lock_state,
            lock_session: None,
            seat,
            ipc: None,
            automation_enabled,
            pending_screenshots: HashMap::new(),
            next_screenshot_id: 0,
            pending_commands: HashMap::new(),
            next_command_id: 0,
            rules_applied: HashSet::new(),
            cursor: crate::cursor::Cursor::load(),
            cursor_status: CursorImageStatus::default_named(),
            pointer_element: crate::drawing::PointerElement::default(),
            pointer_image: None,
        }
    }

    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
    ) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                    .unwrap();
            })
            .expect("failed to insert wayland listening socket");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // SAFETY: we never drop the display while the source is registered.
                    unsafe {
                        let d = display.get_mut();
                        d.dispatch_clients(state).unwrap();
                        // Flush eagerly so clients receive replies even when no
                        // redraw is firing (e.g. nested winit window not visible).
                        let _ = d.flush_clients();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .expect("failed to insert wayland display source");

        socket_name
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

        // Z-order: Overlay > Top > toplevel windows > Bottom > Background.
        // We have to consult layer-shell surfaces explicitly because
        // `space.element_under` only walks the toplevel window stack;
        // without this an Overlay layer surface (e.g. shoestring-region's
        // picker) covering the screen would be invisible to pointer input.
        let output_geo = self.space.outputs().find_map(|o| {
            self.space
                .output_geometry(o)
                .filter(|g| g.to_f64().contains(pos))
                .map(|g| (o.clone(), g))
        });

        if let Some((output, geo)) = output_geo.as_ref() {
            let local = pos - geo.loc.to_f64();
            let map = layer_map_for_output(output);
            for layer in [WlrLayer::Overlay, WlrLayer::Top] {
                if let Some(ls) = map.layer_under(layer, local) {
                    let layer_loc = map.layer_geometry(ls).map(|g| g.loc).unwrap_or_default();
                    let inner = local - layer_loc.to_f64();
                    if let Some((surface, sp)) = ls.surface_under(inner, WindowSurfaceType::ALL) {
                        return Some((surface, (sp + layer_loc).to_f64() + geo.loc.to_f64()));
                    }
                }
            }
        }

        if let Some((surface, p)) = self
            .space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
        {
            return Some((surface, p));
        }

        if let Some((output, geo)) = output_geo {
            let local = pos - geo.loc.to_f64();
            let map = layer_map_for_output(&output);
            for layer in [WlrLayer::Bottom, WlrLayer::Background] {
                if let Some(ls) = map.layer_under(layer, local) {
                    let layer_loc = map.layer_geometry(ls).map(|g| g.loc).unwrap_or_default();
                    let inner = local - layer_loc.to_f64();
                    if let Some((surface, sp)) = ls.surface_under(inner, WindowSurfaceType::ALL) {
                        return Some((surface, (sp + layer_loc).to_f64() + geo.loc.to_f64()));
                    }
                }
            }
        }

        None
    }

    /// The window whose toplevel surface currently holds keyboard focus.
    pub fn focused_window(&self) -> Option<Window> {
        let focused = self.seat.get_keyboard()?.current_focus()?;
        self.space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| *t.wl_surface() == focused)
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Raise + keyboard-focus + activate `window`, deactivating every other
    /// element. Mirrors what click-to-focus does in [`input::process_input_event`].
    pub fn focus_window(&mut self, window: &Window) {
        use smithay::utils::SERIAL_COUNTER;
        self.space.raise_element(window, true);
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kb) = self.seat.get_keyboard() {
            let surface = window.toplevel().unwrap().wl_surface().clone();
            kb.set_focus(self, Some(surface), serial);
        }
        let target = window.clone();
        self.space.elements().for_each(|w| {
            w.set_activated(w == &target);
            w.toplevel().unwrap().send_pending_configure();
        });
        let active = self.workspaces.active();
        self.workspaces.record_focus(active, window);

        let id = self.foreign_toplevels.get(window).map(|h| h.identifier());
        self.emit_ipc(shoestring_ipc::Event::WindowFocused { id });
    }

    /// Clear keyboard focus and deactivate every mapped window. Used when
    /// switching to an empty workspace or minimizing the last window.
    pub fn clear_focus(&mut self) {
        use smithay::utils::SERIAL_COUNTER;
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kb) = self.seat.get_keyboard() {
            kb.set_focus(self, Option::<WlSurface>::None, serial);
        }
        self.space.elements().for_each(|w| {
            w.set_activated(false);
            w.toplevel().unwrap().send_pending_configure();
        });
        self.emit_ipc(shoestring_ipc::Event::WindowFocused { id: None });
    }

    /// Find a tracked window by its xdg toplevel surface. Considers both
    /// currently-mapped windows in the space and off-workspace / minimized
    /// windows held in the workspace manager.
    pub fn find_window(
        &self,
        surface: &smithay::wayland::shell::xdg::ToplevelSurface,
    ) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().map(|t| t == surface).unwrap_or(false))
            .cloned()
            .or_else(|| self.workspaces.find_by_toplevel(surface))
    }

    /// Switch to workspace `target`. Unmaps windows on the current workspace,
    /// remaps windows assigned to the target at their saved locations, and
    /// restores focus from the target's MRU stack.
    pub fn focus_workspace_id(&mut self, target: crate::workspace::WorkspaceId) {
        let current = self.workspaces.active();
        if current == target {
            return;
        }
        self.workspaces.prune_dead();

        // Save current focus into the leaving workspace's history.
        if let Some(focused) = self.focused_window() {
            self.workspaces.record_focus(current, &focused);
        }

        // Snapshot positions then unmap everything currently on screen.
        let to_unmap: Vec<Window> = self.space.elements().cloned().collect();
        for w in &to_unmap {
            if let Some(loc) = self.space.element_location(w) {
                self.workspaces.record_location(w, loc);
            }
        }
        for w in &to_unmap {
            self.space.unmap_elem(w);
        }

        self.workspaces.set_active(target);

        // Re-map every window assigned to the target workspace, skipping
        // those that are still minimized.
        for w in self.workspaces.windows_on(target) {
            if self.layout.is_minimized(&w) {
                continue;
            }
            let loc = self.workspaces.saved_location(&w).unwrap_or((0, 0).into());
            self.space.map_element(w, loc, false);
        }

        // Restore focus to the new workspace's MRU top (skipping windows
        // that aren't currently mapped, e.g. ones the user minimized while
        // it was the active workspace).
        let pick = loop {
            let Some(w) = self.workspaces.last_focused(target) else {
                break None;
            };
            if self.space.elements().any(|el| el == &w) {
                break Some(w);
            }
            self.workspaces.discard_top_focus(target);
        };
        match pick {
            Some(w) => self.focus_window(&w),
            None => self.clear_focus(),
        }
        tracing::debug!(
            from = current.one_based(),
            to = target.one_based(),
            "workspace switched",
        );
        self.emit_ipc(shoestring_ipc::Event::WorkspaceChanged {
            active: target.one_based(),
        });
    }

    /// Refresh `pointer_element`'s memory buffer for the next render. Picks
    /// the right xcursor frame for `scale` + elapsed time; only uploads a new
    /// `MemoryRenderBuffer` when the chosen frame differs from the previous
    /// (true for every render on static cursors). The buffer's reported scale
    /// matches the output's so smithay maps the HiDPI sprite back to its
    /// nominal logical size — otherwise a 48px frame at output-scale 2 would
    /// render at 96 physical pixels.
    pub fn refresh_cursor_buffer(&mut self, scale: u32) {
        if self.cursor.is_empty() {
            return;
        }
        let elapsed = self.start_time.elapsed();
        let Some(frame) = self.cursor.current_frame(scale, elapsed).cloned() else {
            return;
        };
        let same_as_last = self
            .pointer_image
            .as_ref()
            .map(|prev| prev.size == frame.size && prev.pixels_rgba == frame.pixels_rgba)
            .unwrap_or(false);
        if same_as_last {
            return;
        }
        let buffer = smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
            &frame.pixels_rgba,
            smithay::backend::allocator::Fourcc::Argb8888,
            (frame.width as i32, frame.height as i32),
            scale.max(1) as i32,
            smithay::utils::Transform::Normal,
            None,
        );
        self.pointer_element.set_buffer(buffer);
        self.pointer_image = Some(frame);
    }

    /// Snapshot the data needed to render the cursor this frame. Cheap to
    /// clone (the buffer is `Arc`-backed), and separating this from the
    /// renderer call lets backends release a `&mut self` borrow before
    /// calling into the renderer (which itself borrows `self.udev`).
    pub fn cursor_render_snapshot(&self) -> Option<CursorSnapshot> {
        use smithay::input::pointer::CursorImageStatus;
        let pointer = self.seat.get_pointer()?;
        let location = pointer.current_location();
        let hotspot = match &self.cursor_status {
            CursorImageStatus::Hidden => return None,
            CursorImageStatus::Named(_) => self
                .pointer_image
                .as_ref()
                .map(|i| (i.xhot as i32, i.yhot as i32))
                .unwrap_or((0, 0)),
            CursorImageStatus::Surface(surface) => {
                use smithay::wayland::compositor::with_states;
                with_states(surface, |states| {
                    let attrs = states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>();
                    attrs
                        .map(|a| {
                            let h = a.lock().unwrap().hotspot;
                            (h.x, h.y)
                        })
                        .unwrap_or((0, 0))
                })
            }
        };
        Some((self.pointer_element.clone(), location, hotspot))
    }

    /// Schedule an immediate render of the output a pending screencopy frame
    /// targets, so the capture happens promptly instead of waiting for the
    /// next damage-driven render. In the winit backend rendering is already
    /// continuous so this is a no-op; the udev backend wakes the matching
    /// CRTC via an idle callback.
    pub fn kick_render_for_screencopy(&mut self, output: &smithay::output::Output) {
        let _ = output;
        #[cfg(feature = "tty")]
        {
            if self.udev.is_some() {
                if let Some(udev_id) = output
                    .user_data()
                    .get::<crate::backend::udev::UdevOutputId>()
                {
                    let node = udev_id.device_id;
                    let crtc = udev_id.crtc;
                    self.loop_handle.insert_idle(move |state| {
                        state.render_surface(node, crtc);
                    });
                }
            }
        }
    }

    /// Move the focused window to `target` workspace. If `target` differs from
    /// the active workspace, the window is unmapped immediately; focus falls
    /// back to whatever's MRU-top on the current workspace.
    pub fn move_focused_to_workspace(&mut self, target: crate::workspace::WorkspaceId) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("move_focused: no focused window — nothing to move");
            return;
        };
        self.move_window_to_workspace(&window, target);
    }

    /// Move an arbitrary tracked window to `target`. Shared by the
    /// keybind path ([`Self::move_focused_to_workspace`]) and the
    /// `[[window_rules]]` path. When the window happens to be focused
    /// and the move takes it off the active workspace, focus falls
    /// back to the next MRU-top window.
    pub fn move_window_to_workspace(
        &mut self,
        window: &smithay::desktop::Window,
        target: crate::workspace::WorkspaceId,
    ) {
        let active = self.workspaces.active();
        if target == active {
            tracing::debug!(
                ws = active.one_based(),
                "move_window: target == active, skip"
            );
            return;
        }
        tracing::debug!(
            from = active.one_based(),
            to = target.one_based(),
            "move_window"
        );
        let was_focused = self.focused_window().as_ref() == Some(window);
        if let Some(loc) = self.space.element_location(window) {
            self.workspaces.record_location(window, loc);
        }
        self.workspaces.reassign(window, target);
        // Push onto the target's MRU so switching there brings it
        // straight back into focus.
        self.workspaces.record_focus(target, window);
        self.workspaces.remove_from_active_focus(active, window);
        self.space.unmap_elem(window);

        if let Some(handle) = self.foreign_toplevels.get(window) {
            self.emit_ipc(shoestring_ipc::Event::WindowMovedToWorkspace {
                id: handle.identifier(),
                workspace: target.one_based(),
            });
        }

        if !was_focused {
            // Nothing to refocus — keybind / pointer focus stays put.
            return;
        }
        let pick = loop {
            let Some(w) = self.workspaces.last_focused(active) else {
                break None;
            };
            if self.space.elements().any(|el| el == &w) {
                break Some(w);
            }
            self.workspaces.discard_top_focus(active);
        };
        match pick {
            Some(w) => self.focus_window(&w),
            None => self.clear_focus(),
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
