use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Instant};

use shoestring_config::Config;
use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
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
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

pub struct ShoestringWm {
    pub start_time: Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, Self>,

    pub config: Config,
    pub config_path: Option<std::path::PathBuf>,
    pub bindings: crate::binds::BindingTable,

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

    pub seat: Seat<Self>,

    pub ipc: Option<crate::ipc::Server>,
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

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");
        seat.add_keyboard(
            Default::default(),
            config.general.repeat_delay,
            config.general.repeat_rate,
        )
        .unwrap();
        seat.add_pointer();

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
            seat,
            ipc: None,
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
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
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

    /// Move the focused window to `target` workspace. If `target` differs from
    /// the active workspace, the window is unmapped immediately; focus falls
    /// back to whatever's MRU-top on the current workspace.
    pub fn move_focused_to_workspace(&mut self, target: crate::workspace::WorkspaceId) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("move_focused: no focused window — nothing to move");
            return;
        };
        let active = self.workspaces.active();
        if target == active {
            tracing::debug!(
                ws = active.one_based(),
                "move_focused: target == active, skip"
            );
            return;
        }
        tracing::debug!(
            from = active.one_based(),
            to = target.one_based(),
            "move_focused"
        );
        // Capture position before unmapping so the window comes back at the
        // same spot when its workspace is activated.
        if let Some(loc) = self.space.element_location(&window) {
            self.workspaces.record_location(&window, loc);
        }
        self.workspaces.reassign(&window, target);
        // Push the moved window onto the target's MRU so switching there
        // brings it straight back into focus.
        self.workspaces.record_focus(target, &window);
        self.workspaces.remove_from_active_focus(active, &window);
        self.space.unmap_elem(&window);

        // Refocus whatever's next on the active workspace.
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
