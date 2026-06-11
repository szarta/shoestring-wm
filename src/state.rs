use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
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
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        session_lock::SessionLockManagerState,
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, XdgShellState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
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
    /// Held for the global's lifetime. Every toplevel is forced into
    /// `ServerSide` mode; see [`crate::handlers::xdg_decoration`].
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub foreign_toplevel_list: ForeignToplevelListState,
    /// FT handle per window. Drop sends `closed`, so removing the entry on
    /// destroy is sufficient cleanup. Title/app_id changes are pushed in
    /// [`crate::handlers::compositor`]'s commit hook.
    pub foreign_toplevels: HashMap<Window, ForeignToplevelHandle>,
    /// wlr-foreign-toplevel-management: the *writable* sibling of
    /// `foreign_toplevels` (the read-only ext list). Lets waybar-style taskbars
    /// activate/close/minimize/maximize windows and read their state. Hand-wired
    /// (smithay ships no delegate); see [`crate::foreign_toplevel_mgmt`].
    pub foreign_toplevel_mgmt: crate::foreign_toplevel_mgmt::ForeignToplevelMgmtState,
    pub shm_state: ShmState,
    // Held for the lifetime of the WM so their globals stay registered with the
    // display. wp_viewporter + wp_fractional_scale_manager_v1 let HiDPI clients
    // render natively at the exact fractional output scale (see
    // [`crate::scale::send_preferred_scale`]).
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    // Held for the lifetime of the WM so its globals stay registered with the display.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub output_management: crate::output_management::OutputManagementState,
    pub screencopy: crate::screencopy::ScreencopyState,
    pub session_lock_state: SessionLockManagerState,
    /// wlr-gamma-control state: the manager global plus the live per-output
    /// controls. KMS-only (gamma ramps are a CRTC property), so it exists only
    /// in `tty`-feature builds; the DRM glue lives in [`crate::backend::udev`].
    #[cfg(feature = "tty")]
    pub gamma_control: crate::gamma_control::GammaControlState,
    /// `Some` only when `general.idle_notifications_enabled` is set, in which
    /// case the `ext_idle_notify_v1` global is advertised and this holds its
    /// state. When `None` the global was never created, so no client can bind
    /// it and the handler getter is never reached. Pinged on every input
    /// event via [`Self::notify_idle_activity`]. See
    /// [`crate::handlers`]'s `IdleNotifierHandler` impl.
    pub idle_notifier: Option<smithay::wayland::idle_notify::IdleNotifierState<Self>>,
    /// `Some` while a session lock is active. See
    /// [`crate::handlers::session_lock`] for the protocol wiring.
    pub lock_session: Option<crate::handlers::session_lock::LockState>,

    pub seat: Seat<Self>,

    pub ipc: Option<crate::ipc::Server>,
    /// Diagnostics registry: the latest sampled process/WM metrics plus
    /// the fd-leak detector's state. Fed by the sampler timer started in
    /// `main` (when `[diagnostics].enabled`) and read by the `metrics`
    /// IPC snapshot/stream. See [`crate::metrics`].
    pub metrics: crate::metrics::Metrics,
    /// Whether this WM is the session compositor and may integrate with the
    /// surrounding session — i.e. push `WAYLAND_DISPLAY` / `DISPLAY` into the
    /// systemd user manager so D-Bus-activated services find them. True for
    /// the TTY/udev backend; false for the nested winit backend, which runs
    /// *inside* another session and must not clobber that session's
    /// environment (or socket-activated services like ssh-tpm-agent). Set
    /// from the chosen backend in `main`.
    pub session_integration: bool,
    /// Runtime gate for remote-automation IPC methods (inject_key/text/click,
    /// future remote screenshot + command exec). Initialised from
    /// `general.automation_enabled` and overridable at runtime via
    /// `Request::SetAutomation` and at startup via `--enable-automation`.
    /// Never written back to disk; the config file stays the source of
    /// truth at next start.
    pub automation_enabled: bool,
    /// Runtime gate for screen capture via `zwlr_screencopy_v1`. Initialised
    /// from `general.screen_capture_enabled` and overridable at runtime via
    /// `Request::SetScreenCapture`. When this flips, the screencopy manager
    /// global is created/withdrawn ([`crate::screencopy::ScreencopyState`]).
    /// Never written back to disk; the config file is the source of truth at
    /// next start.
    pub screen_capture_enabled: bool,
    /// Throttle for [`shoestring_ipc::Event::ScreenCaptured`]: the
    /// `Instant` of the last emitted live-capture event. `None` until the
    /// first capture. Keeps a high-FPS cast from flooding subscribers.
    pub last_screen_capture_event: Option<Instant>,

    /// In-flight `Request::Screenshot` subprocesses, keyed by an
    /// opaque counter. Entries are removed once the child has exited
    /// and the deferred IPC response has been written.
    pub pending_screenshots: HashMap<u64, crate::remote_screenshot::Pending>,
    pub next_screenshot_id: u64,

    /// In-flight `Request::RunCommand` subprocesses; same lifecycle as
    /// `pending_screenshots` but with optional timer-based SIGKILL.
    pub pending_commands: HashMap<u64, crate::remote_command::Pending>,
    pub next_command_id: u64,

    /// `Some` while a [`shoestring_ipc::Request::PickWindow`] is awaiting
    /// the user's next click. Picker mode intercepts pointer/keyboard
    /// input — see [`crate::picker`] and [`crate::input`].
    pub pending_picker: Option<crate::picker::PendingPicker>,

    /// `Some` while a `shoestring-confirm` modal dialog is on screen
    /// awaiting Enter/Esc. Cleared in `finalize_confirm` once the helper
    /// exits. Only one confirm runs at a time — see [`crate::confirm`].
    pub pending_confirm: Option<crate::confirm::PendingConfirm>,

    /// Window currently being dragged via a Super+drag move grab.
    /// Cleared when the grab ends. The edge-drag repeat timer reads
    /// this to keep shifting the dragged window while the pointer is
    /// pinned to a workspace boundary.
    pub edge_drag_window: Option<Window>,
    /// Calloop timer token for the edge-drag repeat tick. Removed and
    /// re-inserted on every successful edge-cross so a sustained drag
    /// keeps stepping workspaces without the user having to release
    /// and re-push the cursor.
    pub edge_drag_repeat_token: Option<smithay::reexports::calloop::RegistrationToken>,

    /// `Some` while the user is holding a repeatable keybind (currently
    /// just relative-workspace navigation). Replaced wholesale on a new
    /// press; cleared on the matching release. See [`crate::input`].
    pub key_repeat: Option<crate::input::KeyRepeat>,

    /// Accumulated mouse-wheel travel (in v120 units, 120 per physical
    /// detent) while scrolling the bare desktop to switch workspaces.
    /// Lets high-resolution wheels — which emit several sub-detent events
    /// per notch — and the configurable `general.desktop_scroll_notches`
    /// threshold coexist without overshooting. Reset on direction reversal.
    /// See [`crate::input`].
    pub desktop_scroll_accum: f64,

    /// Windows that have already had `[[window_rules]]` evaluated.
    /// Evaluation runs once per window — on the first commit after
    /// map. Entries are removed in `toplevel_destroyed`.
    pub rules_applied: HashSet<Window>,

    /// Windows awaiting an initial centering pass. At `new_toplevel` the
    /// client hasn't picked a size yet (geometry is 0×0), so we can't
    /// center properly; the first commit with a non-zero size triggers
    /// the actual placement. Entries are removed when re-centered, when
    /// a window-rule overrides position, or when the toplevel dies.
    pub pending_initial_center: HashSet<Window>,

    pub cursor: crate::cursor::Cursor,
    pub cursor_status: CursorImageStatus,
    pub pointer_element: crate::drawing::PointerElement,
    /// Last image we uploaded as a `MemoryRenderBuffer`. We re-use the buffer
    /// when the chosen xcursor frame is unchanged across renders (the common
    /// case — static cursors stay on a single frame indefinitely).
    pub pointer_image: Option<xcursor::parser::Image>,

    /// X11 window-manager client. `None` until Xwayland sends `Ready`
    /// (and on Xwayland disconnect). See [`crate::xwayland`].
    pub xwm: Option<smithay::xwayland::X11Wm>,
    /// X display number Xwayland advertised; used to set `$DISPLAY` for
    /// child processes. `None` mirrors `xwm`.
    pub xdisplay: Option<u32>,
    /// Wayland-side state for the `xwayland_shell_v1` global Xwayland
    /// uses to publish its toplevels. Held for the global's lifetime.
    pub xwayland_shell_state: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// Primary selection (X11 middle-click clipboard) state. Held for
    /// the global's lifetime so wayland-native apps can forward primary
    /// to / from X11 apps via the XWayland selection bridge.
    pub primary_selection_state:
        smithay::wayland::selection::primary_selection::PrimarySelectionState,
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
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let foreign_toplevel_list = ForeignToplevelListState::new::<Self>(&dh);
        // wlr-foreign-toplevel-management: advertise v3 so taskbars can control
        // windows (output_enter/leave need v1+, the handle interface is v3).
        // Always-on (backend-agnostic, like the ext list above).
        let foreign_toplevel_mgmt = crate::foreign_toplevel_mgmt::ForeignToplevelMgmtState {
            manager_global: dh.create_global::<Self, _, _>(
                3,
                crate::foreign_toplevel_mgmt::ForeignToplevelManagerData,
            ),
            managers: Vec::new(),
            handles: HashMap::new(),
            last_outputs: HashMap::new(),
        };
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let output_management = crate::output_management::OutputManagementState {
            global: dh.create_global::<Self, _, _>(4, crate::output_management::OutputManagerData),
            serial: 0,
            managers: Vec::new(),
            heads: Vec::new(),
        };
        // Screen capture is opt-in: only advertise the zwlr_screencopy manager
        // global when the gate is on, so by default no client can even
        // discover the capability. Toggled at runtime by `set_screen_capture`.
        let screen_capture_enabled = config.general.screen_capture_enabled;
        let screencopy = crate::screencopy::ScreencopyState {
            manager_global: screen_capture_enabled.then(|| {
                dh.create_global::<Self, _, _>(3, crate::screencopy::ScreencopyManagerData)
            }),
            pending: Vec::new(),
        };
        // wlr-gamma-control: advertise the manager so night-light tools can
        // drive per-output gamma. Honored only for KMS outputs; binding it for
        // a non-udev output fails gracefully (see the handler). Version 1.
        #[cfg(feature = "tty")]
        let gamma_control = crate::gamma_control::GammaControlState {
            manager_global: dh
                .create_global::<Self, _, _>(1, crate::gamma_control::GammaControlManagerData),
            controls: std::collections::HashMap::new(),
        };

        // Filter: every client may see ext-session-lock — clients without
        // permission to actually lock just receive `finished` when they try.
        // Our own gating (single locker at a time) lives in the handler.
        let session_lock_state = SessionLockManagerState::new::<Self, _>(&dh, |_| true);

        let xwayland_shell_state = crate::xwayland::init_xwayland_globals(&dh);
        let primary_selection_state =
            smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<Self>(&dh);

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

        // Only advertise ext_idle_notify_v1 when the user opted in. Creating
        // the state also creates the global, so gating creation here is what
        // keeps the protocol entirely absent (not merely inert) by default.
        let idle_notifier = config.general.idle_notifications_enabled.then(|| {
            smithay::wayland::idle_notify::IdleNotifierState::new(&dh, event_loop.handle())
        });

        let space = Space::default();
        let popups = PopupManager::default();
        let workspaces = build_workspace_manager(&config.workspaces);

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
            workspaces,
            #[cfg(feature = "tty")]
            udev: None,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            layer_shell_state,
            foreign_toplevel_list,
            foreign_toplevels: HashMap::new(),
            foreign_toplevel_mgmt,
            shm_state,
            viewporter_state,
            fractional_scale_manager_state,
            output_manager_state,
            seat_state,
            data_device_state,
            output_management,
            screencopy,
            session_lock_state,
            #[cfg(feature = "tty")]
            gamma_control,
            idle_notifier,
            lock_session: None,
            seat,
            ipc: None,
            metrics: crate::metrics::Metrics::new(),
            // Default to the safe (non-integrating) stance; `main` flips this
            // on for the real session backend once the backend is chosen.
            session_integration: false,
            automation_enabled,
            screen_capture_enabled,
            last_screen_capture_event: None,
            pending_screenshots: HashMap::new(),
            next_screenshot_id: 0,
            pending_commands: HashMap::new(),
            next_command_id: 0,
            pending_picker: None,
            pending_confirm: None,
            edge_drag_window: None,
            edge_drag_repeat_token: None,
            key_repeat: None,
            desktop_scroll_accum: 0.0,
            rules_applied: HashSet::new(),
            pending_initial_center: HashSet::new(),
            cursor: crate::cursor::Cursor::load(),
            cursor_status: CursorImageStatus::default_named(),
            pointer_element: crate::drawing::PointerElement::default(),
            pointer_image: None,
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            primary_selection_state,
        }
    }

    /// Route a child reaped by the global SIGCHLD handler to whichever
    /// in-flight remote request spawned it. Children we don't track
    /// (autostart, bar, menus, XWayland, ...) match nothing and are simply
    /// dropped — they only needed reaping to avoid zombies.
    pub fn note_child_reaped(&mut self, pid: i32, status: std::process::ExitStatus) {
        // Short-circuit: try screenshots first, then commands; a child that
        // matches neither is a fire-and-forget helper we just let go.
        let _ = self.note_screenshot_reaped(pid, status) || self.note_command_reaped(pid, status);
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
                if let Err(e) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    tracing::warn!("failed to insert wayland client: {e}");
                }
            })
            .expect("failed to insert wayland listening socket");

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // SAFETY: we never drop the display while the source is registered.
                    unsafe {
                        let d = display.get_mut();
                        if let Err(e) = d.dispatch_clients(state) {
                            tracing::error!("dispatch_clients error: {e}");
                        }
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
    /// Matches both xdg and X11 windows via [`crate::window_ext::matches_surface`].
    pub fn focused_window(&self) -> Option<Window> {
        let focused = self.seat.get_keyboard()?.current_focus()?;
        self.space
            .elements()
            .find(|w| crate::window_ext::matches_surface(w, &focused))
            .cloned()
    }

    /// Raise + keyboard-focus + activate `window`, deactivating every other
    /// element. Mirrors what click-to-focus does in [`input::process_input_event`].
    pub fn focus_window(&mut self, window: &Window) {
        // While the session is locked, only a lock surface may hold
        // keyboard focus (see [`focus_lock_surface`]). Auto-focus-on-map
        // and other focus paths must not steal it, or the locker's
        // password field silently goes dead while the maze keeps running.
        if self.is_locked() {
            return;
        }
        use smithay::utils::SERIAL_COUNTER;
        self.space.raise_element(window, true);
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kb) = self.seat.get_keyboard() {
            if let Some(surface) = crate::window_ext::focus_surface(window) {
                kb.set_focus(self, Some(surface), serial);
            }
        }
        let target = window.clone();
        self.space.elements().for_each(|w| {
            w.set_activated(w == &target);
            crate::window_ext::send_pending_configure(w);
        });
        let active = self.workspaces.active();
        self.workspaces.record_focus(active, window);

        let id = self.foreign_toplevels.get(window).map(|h| h.identifier());
        self.emit_ipc(shoestring_ipc::Event::WindowFocused { id });
        // The activated state bit moved — refresh every taskbar handle.
        crate::foreign_toplevel_mgmt::broadcast_all(self);
    }

    /// Like [`focus_window`] but does not raise the window in the
    /// stacking order. Used by focus-follows-mouse / sloppy focus: the
    /// pointer-driven path moves keyboard focus and activation without
    /// reordering the stack, so passive hover doesn't reshuffle windows
    /// behind the user.
    pub fn focus_window_no_raise(&mut self, window: &Window) {
        // See [`focus_window`]: the lock surface owns focus while locked.
        if self.is_locked() {
            return;
        }
        use smithay::utils::SERIAL_COUNTER;
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kb) = self.seat.get_keyboard() {
            if let Some(surface) = crate::window_ext::focus_surface(window) {
                kb.set_focus(self, Some(surface), serial);
            }
        }
        let target = window.clone();
        self.space.elements().for_each(|w| {
            w.set_activated(w == &target);
            crate::window_ext::send_pending_configure(w);
        });
        let active = self.workspaces.active();
        self.workspaces.record_focus(active, window);

        let id = self.foreign_toplevels.get(window).map(|h| h.identifier());
        self.emit_ipc(shoestring_ipc::Event::WindowFocused { id });
        // The activated state bit moved — refresh every taskbar handle.
        crate::foreign_toplevel_mgmt::broadcast_all(self);
    }

    /// Clear keyboard focus and deactivate every mapped window. Used when
    /// switching to an empty workspace or minimizing the last window.
    pub fn clear_focus(&mut self) {
        // Don't drop the lock surface's focus while locked (see
        // [`focus_window`]); unlock restores focus explicitly.
        if self.is_locked() {
            return;
        }
        use smithay::utils::SERIAL_COUNTER;
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(kb) = self.seat.get_keyboard() {
            kb.set_focus(self, Option::<WlSurface>::None, serial);
        }
        self.space.elements().for_each(|w| {
            w.set_activated(false);
            crate::window_ext::send_pending_configure(w);
        });
        self.emit_ipc(shoestring_ipc::Event::WindowFocused { id: None });
        // Every window lost the activated bit — refresh all taskbar handles.
        crate::foreign_toplevel_mgmt::broadcast_all(self);
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
            self.space.map_element(w.clone(), loc, false);
            crate::window_ext::sync_x11_location(&w, loc);
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

    /// Apply the screen-capture gate. Idempotent: brings the
    /// `zwlr_screencopy_manager_v1` global into existence when `enabled` and
    /// withdraws it when not, and updates [`Self::screen_capture_enabled`].
    /// Withdrawing also fails any in-flight captures so a client doesn't hang,
    /// and — because a client that bound the manager before withdrawal keeps
    /// its proxy — the capture handler additionally refuses requests while the
    /// gate is off (see [`crate::handlers::screencopy`]). The caller owns the
    /// logging / `ScreenCaptureChanged` event (mirrors the automation gate).
    pub fn set_screen_capture(&mut self, enabled: bool) {
        self.screen_capture_enabled = enabled;
        let dh = self.display_handle.clone();
        if enabled {
            if self.screencopy.manager_global.is_none() {
                self.screencopy.manager_global = Some(
                    dh.create_global::<Self, _, _>(3, crate::screencopy::ScreencopyManagerData),
                );
            }
        } else {
            if let Some(global) = self.screencopy.manager_global.take() {
                dh.remove_global::<Self>(global);
            }
            for frame in self.screencopy.pending.drain(..) {
                frame.failed();
            }
        }
    }

    /// Record that a capture frame was just requested and, throttled to a few
    /// per second, broadcast [`shoestring_ipc::Event::ScreenCaptured`] so a
    /// bar can show a live "your screen is being read" indicator (distinct
    /// from the gate merely being enabled). The throttle keeps a 30/60-fps
    /// cast from flooding subscribers.
    pub fn note_screen_capture(&mut self, output_name: &str) {
        let now = Instant::now();
        let emit = self
            .last_screen_capture_event
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(400));
        if emit {
            self.last_screen_capture_event = Some(now);
            self.emit_ipc(shoestring_ipc::Event::ScreenCaptured {
                output: output_name.to_string(),
            });
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

    /// If `window` is awaiting its initial centering pass and now has a
    /// real geometry, recenter it on the non-exclusive zone of the
    /// output it currently sits on. Called from the commit handler so
    /// the very first frame the client paints lands in the correct
    /// spot. No-op once the window is no longer pending.
    pub fn try_recenter_pending(&mut self, window: &Window) {
        if !self.pending_initial_center.contains(window) {
            return;
        }
        let size = window.geometry().size;
        if size.w <= 0 || size.h <= 0 {
            return;
        }
        let cur = self.space.element_location(window);
        let output = cur
            .and_then(|loc| {
                self.space
                    .outputs()
                    .find(|o| {
                        self.space
                            .output_geometry(o)
                            .map(|g| {
                                loc.x >= g.loc.x
                                    && loc.x < g.loc.x + g.size.w
                                    && loc.y >= g.loc.y
                                    && loc.y < g.loc.y + g.size.h
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
            })
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let zone = layer_map_for_output(&output).non_exclusive_zone();
        let usable_loc = output_geo.loc + zone.loc;
        let x = usable_loc.x + (zone.size.w - size.w).max(0) / 2;
        let y = usable_loc.y + (zone.size.h - size.h).max(0) / 2;
        let new_loc: smithay::utils::Point<i32, smithay::utils::Logical> = (x, y).into();
        tracing::debug!(?new_loc, geo_size = ?size, "initial center pass");
        self.space.map_element(window.clone(), new_loc, false);
        crate::window_ext::sync_x11_location(window, new_loc);
        self.workspaces.record_location(window, new_loc);
        self.pending_initial_center.remove(window);
    }

    /// Reassign `window` to `target` workspace and switch the active
    /// workspace to `target`, taking the window along. Used by the
    /// edge-drag gesture in [`MoveSurfaceGrab`] so a Super+drag that
    /// crosses an output edge follows the window into the new workspace
    /// without breaking the pointer grab.
    pub fn move_window_to_workspace_following(
        &mut self,
        window: &smithay::desktop::Window,
        target: crate::workspace::WorkspaceId,
    ) {
        let active = self.workspaces.active();
        if target == active {
            return;
        }
        if let Some(loc) = self.space.element_location(window) {
            self.workspaces.record_location(window, loc);
        }
        self.workspaces.reassign(window, target);
        // Surface the dragged window as the target's MRU top so it
        // remains focused after the switch.
        self.workspaces.record_focus(target, window);
        self.workspaces.remove_from_active_focus(active, window);

        if let Some(handle) = self.foreign_toplevels.get(window) {
            self.emit_ipc(shoestring_ipc::Event::WindowMovedToWorkspace {
                id: handle.identifier(),
                workspace: target.one_based(),
            });
        }

        // Drop keyboard focus first so focus_workspace_id doesn't
        // record the now-moved window into the LEAVING workspace's MRU.
        self.clear_focus();
        self.focus_workspace_id(target);
    }
}

/// Build the runtime [`WorkspaceManager`] from the `[workspaces]`
/// config section. Clamps count to `1..=MAX_WORKSPACE_COUNT`, warns
/// on values outside that range, and discards name entries whose key
/// doesn't parse to a valid 1-based index for the resolved count.
fn build_workspace_manager(
    cfg: &shoestring_config::Workspaces,
) -> crate::workspace::WorkspaceManager {
    use shoestring_config::MAX_WORKSPACE_COUNT;
    let raw = cfg.count;
    let count = raw.clamp(1, MAX_WORKSPACE_COUNT);
    if raw != count {
        tracing::warn!(
            requested = raw,
            clamped = count,
            "[workspaces].count out of range; clamped"
        );
    }
    let mut names = vec![String::new(); count as usize];
    for (key, name) in &cfg.names {
        match key.parse::<u8>() {
            Ok(idx) if (1..=count).contains(&idx) => {
                names[idx as usize - 1] = name.clone();
            }
            Ok(idx) => tracing::warn!(
                index = idx,
                count,
                name = %name,
                "[workspaces.names] index out of range; ignored"
            ),
            Err(_) => tracing::warn!(
                key = %key,
                "[workspaces.names] non-numeric key; ignored"
            ),
        }
    }
    crate::workspace::WorkspaceManager::new(count, names)
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
