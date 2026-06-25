use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    sync::Arc,
    time::{Duration, Instant},
};

use shoestring_config::Config;
use smithay::{
    backend::allocator::{dmabuf::Dmabuf, format::FormatSet},
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
    utils::{IsAlive, Logical, Point},
    wayland::{
        compositor::{get_parent, CompositorClientState, CompositorState},
        dmabuf::DmabufState,
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
        xdg_activation::XdgActivationState,
        xdg_foreign::XdgForeignState,
    },
};

// Held only by the tty/udev backend (see `dmabuf_global`); unused under winit.
#[cfg(feature = "tty")]
use smithay::wayland::dmabuf::DmabufGlobal;

/// Smithay z-index for always-on-top windows. Sits between
/// [`RenderZindex::Shell`] (30, ordinary windows) and
/// [`RenderZindex::Top`] (40, layer-shell top surfaces — bars, overlay
/// menus), so an always-on-top window floats above other windows while bars
/// and pop-up menus stay reachable above it. See [`ShoestringWm::set_always_on_top`].
const ALWAYS_ON_TOP_ZINDEX: u8 = 35;

/// `(pointer_element_snapshot, pointer_location, (hotspot_x, hotspot_y))`.
pub type CursorSnapshot = (
    crate::drawing::PointerElement,
    smithay::utils::Point<f64, smithay::utils::Logical>,
    (i32, i32),
);

pub struct ShoestringWm {
    pub start_time: Instant,
    /// Monotonic clock backing `wp_presentation` timestamps. Its id is what
    /// the presentation global advertises so clients interpret the
    /// `presented` times against the right clock. Also the fallback time
    /// source when a backend can't supply a hardware vblank timestamp.
    pub clock: smithay::utils::Clock<smithay::utils::Monotonic>,
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
    /// Persistent solid-color buffers backing each window's server-side border
    /// (when `[decorations].border_width > 0`). Kept across frames so the
    /// damage tracker sees stable element ids; pruned to live windows each
    /// frame in [`crate::decorations::border_elements`]. Empty when borders are
    /// off, which is the default.
    pub window_borders: HashMap<Window, crate::decorations::WindowBorder>,

    /// F3-style on-screen diagnostics overlay (off by default; toggled by
    /// [`shoestring_config::Action::ToggleDiagnostics`]). Holds the persistent
    /// text buffer and its throttle/cache state; renders from `metrics`.
    pub diag_overlay: crate::diag_overlay::DiagOverlay,

    /// Desktop background / wallpaper (the `[background]` config section).
    /// Caches the composited output-sized canvas; rebuilt on output resize or
    /// config reload. The bottom-most element of every output's scene.
    pub wallpaper: crate::wallpaper::Wallpaper,

    #[cfg(feature = "tty")]
    pub udev: Option<crate::backend::udev::UdevData>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// `xdg_activation_v1`: cross-client focus handoff via activation tokens.
    /// A launcher/foreground app mints a token (ideally backed by a real
    /// input serial) and hands it to the app it spawns, which `activate`s
    /// itself with it. The focus-stealing-prevention policy lives in
    /// [`crate::handlers::xdg_activation`]. Mutable (not just held for the
    /// global) because the handler prunes consumed/stale tokens from it.
    pub xdg_activation_state: XdgActivationState,
    /// `xdg_foreign_v2` (zxdg_exporter/importer): lets one client export a
    /// toplevel under an opaque handle and another *different-process* client
    /// import it and set it as the parent of its own toplevel. The canonical
    /// consumer is the desktop portal — a file-chooser/print dialog runs in
    /// the portal process but wants to be parented to the app that summoned
    /// it. The whole protocol (export/import/set_parent_of, handle revocation)
    /// is driven by smithay through the blanket `delegate_dispatch2!`; we only
    /// hold the state and surface it via [`crate::handlers::xdg_foreign`].
    pub xdg_foreign_state: XdgForeignState,
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
    /// IPC-set display-name overrides, keyed by the live window. When present,
    /// the value supersedes the client's `xdg_toplevel` title everywhere the WM
    /// reports a title (the `windows`/`get_tree` snapshots, `find_windows`
    /// matching, the `window_title_changed` event). Set/cleared via
    /// [`Request::SetWindowName`](shoestring_ipc::Request::SetWindowName) and
    /// dropped when the window closes. Not forwarded to
    /// wlr-foreign-toplevel-management taskbars, which keep the raw client title.
    pub window_name_overrides: HashMap<Window, String>,
    /// wlr-foreign-toplevel-management: the *writable* sibling of
    /// `foreign_toplevels` (the read-only ext list). Lets waybar-style taskbars
    /// activate/close/minimize/maximize windows and read their state. Hand-wired
    /// (smithay ships no delegate); see [`crate::foreign_toplevel_mgmt`].
    pub foreign_toplevel_mgmt: crate::foreign_toplevel_mgmt::ForeignToplevelMgmtState,
    /// `ext-workspace-v1`: lets standard bars (waybar, Sway/Niri-style) read and
    /// switch workspaces without our custom IPC. Hand-wired (smithay ships no
    /// delegate); see [`crate::ext_workspace`].
    pub ext_workspace: crate::ext_workspace::ExtWorkspaceState,
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
    /// `ext-image-copy-capture-v1` + `ext-image-capture-source-v1` — the
    /// standardized successor to wlr-screencopy, advertised alongside it.
    /// Gated by the same screen-capture flag (`capture_gate`). See
    /// [`crate::ext_screencopy`].
    pub ext_screencopy: crate::ext_screencopy::ExtScreencopyState,
    /// wlr-virtual-pointer manager: lets clients (wlrctl, remote desktop)
    /// emulate a physical pointer. Always-on and backend-agnostic; the per
    /// resource event batching lives in [`crate::virtual_pointer`]. Held only
    /// to keep the manager global registered for the WM's lifetime.
    #[allow(dead_code)]
    pub virtual_pointer: crate::virtual_pointer::VirtualPointerState,
    /// `zwp_linux_dmabuf_v1` delegate. The global is stood up lazily once the
    /// first GPU (udev backend) reports its render formats — see
    /// [`crate::backend::udev`]. Unlike the screencopy manager this is *not*
    /// gated by the screen-capture gate: dmabuf import is a general facility
    /// GPU clients use for their own surface buffers. Only the screencopy
    /// `linux_dmabuf` *event* (which offers the compositor's framebuffer up
    /// for readback) respects the gate.
    pub dmabuf_state: DmabufState,
    /// `Some` once the dmabuf global has been created (held so it stays
    /// registered). Created at most once, on the first udev `device_added`,
    /// so it only exists on the tty backend.
    #[cfg(feature = "tty")]
    pub dmabuf_global: Option<DmabufGlobal>,
    /// dmabuf formats the primary GPU's renderer can import/render, captured
    /// when the global is created. Reused to advertise the screencopy
    /// `linux_dmabuf` event. `None` on the winit backend (no dmabuf export).
    pub dmabuf_formats: Option<FormatSet>,
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
    /// `Some` alongside [`Self::idle_notifier`] (same opt-in): holds the
    /// `zwp_idle_inhibit_manager_v1` global so video players and the like
    /// can suppress idle while visible. Inhibition is meaningless without a
    /// notifier to inhibit, so the two are advertised together. Held only to
    /// keep the global registered for the session's lifetime.
    #[allow(dead_code)]
    pub idle_inhibit_manager: Option<smithay::wayland::idle_inhibit::IdleInhibitManagerState>,
    /// Surfaces holding a live `zwp_idle_inhibitor_v1`. A `Vec` (not a set)
    /// so two inhibitors on one surface are reference-counted by position.
    /// Idle is actually inhibited only while at least one of these is
    /// *visible* (mapped in the space); see [`Self::refresh_idle_inhibit`].
    pub idle_inhibitors: Vec<WlSurface>,
    /// `Some` while a session lock is active. See
    /// [`crate::handlers::session_lock`] for the protocol wiring.
    pub lock_session: Option<crate::handlers::session_lock::LockState>,

    pub seat: Seat<Self>,
    /// Output association for touch devices, keyed by libinput device name and
    /// holding the connector name libinput reports for that device (set by a
    /// udev rule, e.g. `WL_OUTPUT`). Populated from the udev `DeviceAdded`/
    /// `DeviceRemoved` seam, where the concrete `libinput::Device` is available;
    /// empty on the winit backend. Consulted by touch projection when
    /// `[touch].map_to_output` is unset — see `touch_location` in
    /// [`crate::input`]. Usually empty (libinput rarely tags an output without
    /// an explicit udev rule), which is why the config override exists.
    pub touch_output_by_device: HashMap<String, String>,
    /// `zwp_pointer_constraints_v1`: lets a focused client lock the pointer in
    /// place or confine it to a region — pointer-lock for FPS games and
    /// RDP/VNC clients. Held only to keep the global registered; enforcement
    /// lives in [`crate::input`]'s motion handling and goes through
    /// `with_pointer_constraint` (keyed on surface data, not this state).
    #[allow(dead_code)]
    pub pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState,
    /// `zwp_relative_pointer_v1`: unaccelerated relative-motion deltas, the
    /// required companion to pointer constraints (a locked pointer reports
    /// motion solely through this). Delivered via the existing
    /// `PointerHandle::relative_motion` call on every libinput motion event;
    /// held only to keep the global registered.
    #[allow(dead_code)]
    pub relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// `zwp_tablet_manager_v2`: drawing-tablet / stylus support (Wacom et al.).
    /// Always advertised; the actual tablet/tool routing lives in
    /// [`crate::input`]'s `TabletToolAxis`/`Proximity`/`Tip`/`Button` handling,
    /// which registers tablets on the seat's tablet-seat on device hotplug.
    /// Held only to keep the global registered for the session's lifetime.
    #[allow(dead_code)]
    pub tablet_manager: smithay::wayland::tablet_manager::TabletManagerState,
    /// `zwp_pointer_gestures_v1`: forwards libinput touchpad swipe/pinch/hold
    /// gestures to clients via the seat's pointer. Always advertised; real
    /// gesture events arrive only from the libinput (TTY) backend and are
    /// routed in [`crate::input`]'s `Gesture*` handling. Held only to keep the
    /// global registered for the session's lifetime.
    #[allow(dead_code)]
    pub pointer_gestures: smithay::wayland::pointer_gestures::PointerGesturesState,
    /// `wp_cursor_shape_v1`: lets clients name a cursor (a [`CursorIcon`])
    /// instead of committing their own sprite. smithay translates a
    /// `set_shape` into `SeatHandler::cursor_image(Named(..))`, which our
    /// renderer rasterizes from the active xcursor theme — so themed cursors
    /// stay consistent without client-side xcursor handling. Always-on
    /// global; held only to keep it registered for the session's lifetime.
    /// Dispatched through the blanket `delegate_dispatch2!`, not this field.
    #[allow(dead_code)]
    pub cursor_shape_manager: smithay::wayland::cursor_shape::CursorShapeManagerState,
    /// `zwp_keyboard_shortcuts_inhibit_v1`: lets a focused client (a nested
    /// compositor, VM, remote-desktop viewer, or game) ask us to stop eating
    /// its keybinds so every key reaches it. Unlike the always-on globals
    /// above, this state is load-bearing — it owns the per-seat inhibitor list
    /// the `KeyboardShortcutsInhibitHandler` mutates, so it must be retained.
    /// See the handler impl in [`crate::handlers`] and the keybind filter in
    /// [`crate::input`].
    pub keyboard_shortcuts_inhibit_state:
        smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState,
    /// The inhibitor we have currently *activated* — at most one, always the
    /// one on the keyboard-focused surface. We grant every request but only
    /// activate the focused surface's inhibitor (and deactivate it on focus
    /// loss), so `Seat::keyboard_shortcuts_inhibited()` cleanly means "the
    /// focused client is grabbing all keys." Updated in `focus_changed` /
    /// `new_inhibitor` / `inhibitor_destroyed`.
    pub active_shortcuts_inhibitor:
        Option<smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitor>,
    /// `wp_presentation` global. Clients use it to request precise on-screen
    /// timestamps for the buffers they commit (video A/V sync, animation
    /// pacing). The udev/DRM backend fulfils them from hardware vblank
    /// timestamps; the winit backend marks them at submit time (best effort).
    /// Held purely to keep the global registered for the WM's lifetime — the
    /// protocol is driven through the blanket `delegate_dispatch2!`, not this
    /// field.
    #[allow(dead_code)]
    pub presentation_state: smithay::wayland::presentation::PresentationState,
    /// Latest cursor-position hint from a locked-pointer client: the surface
    /// it named plus a surface-local point. When the lock is released the
    /// visible cursor is dropped back here (it was hidden while locked).
    /// Cleared once no constraint remains on the surface. See the
    /// `PointerConstraintsHandler` impl in [`crate::handlers`].
    pub pointer_constraint_cursor_hint: Option<(WlSurface, Point<f64, Logical>)>,
    /// The surface (and its global origin offset) the pointer last *delivered*
    /// focus to. Tracked so [`Self::refresh_pointer_focus`] can tell whether
    /// the scene under a stationary pointer actually changed — a surface that
    /// maps, unmaps, or slides under the cursor keeps the cursor still but
    /// changes either the target surface or its offset, and the client must
    /// then see enter/leave/motion. Updated wherever we hand the pointer a new
    /// focus so the refresh pass doesn't re-send motion the client already saw.
    pub last_pointer_focus: Option<(WlSurface, Point<f64, Logical>)>,

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
    /// Shared live mirror of [`Self::screen_capture_enabled`] read by the
    /// `ext-image-copy-capture` manager globals' visibility filters (which
    /// outlive a single `&self` borrow). Updated in lockstep by
    /// [`Self::set_screen_capture`] so those globals appear/disappear with the
    /// gate just like the wlr manager does.
    pub capture_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Throttle for [`shoestring_ipc::Event::ScreenCaptured`]: the
    /// `Instant` of the last emitted live-capture event. `None` until the
    /// first capture. Keeps a high-FPS cast from flooding subscribers.
    pub last_screen_capture_event: Option<Instant>,
    /// Last media-privacy snapshot reported by the `shoestring-mediad` monitor
    /// (default-sink/source mute + camera-in-use). `None` until the monitor
    /// first reports — the WM never authors this, it only caches PipeWire's
    /// truth so the bar can render MUTE/MIC/CAM indicators. Updated via
    /// `Request::ReportMedia`; mutation is delegated to the helper.
    pub media: Option<shoestring_ipc::MediaState>,

    /// Runtime **remote gate** for remote-desktop serve mode. Off until the
    /// user explicitly enables it (never from disk, so remote access can't be
    /// live at login), and only enable-able while a server is registered. When
    /// it flips, the capture + automation gates follow (see
    /// [`Self::set_remote`]). Reported to the bar (toggle + "being viewed"
    /// chip) via [`shoestring_ipc::Event::RemoteChanged`].
    pub remote_enabled: bool,
    /// The IPC connection of the registered `shoestring-remote-server`, if one
    /// has registered ([`shoestring_ipc::Request::RegisterRemoteServer`]).
    /// `Some` ⇒ the remote gate is selectable. Cleared when that connection
    /// drops, which also forces the gate off.
    pub remote_server: Option<crate::ipc::ClientId>,
    /// Connected remote viewers/controllers, as last reported by the server
    /// ([`shoestring_ipc::Request::ReportRemoteViewers`]). Drives the bar's
    /// "being viewed / controlled" chip; `> 0` means someone is watching.
    pub remote_viewers: u32,

    /// The **machine-axis** — the viewer-box half of remote desktop. Each
    /// registered [`crate::remote::RemoteClient`] is a viewable machine
    /// (`Super+J/K` cycles them); index 0 is the implicit local machine, so
    /// `remote_clients[N-1]` is axis index `N`. Registered via
    /// [`shoestring_ipc::Request::RegisterRemoteClient`]; an entry's IPC
    /// connection dropping removes it (see [`Self::drop_remote_client`]).
    pub remote_clients: Vec<crate::remote::RemoteClient>,
    /// Active view on the machine-axis: 0 = local (normal input), `N` = driving
    /// `remote_clients[N-1]`. While non-zero the WM is in **capture mode** — all
    /// local input is swallowed and forwarded to that client as
    /// [`shoestring_ipc::Event::CapturedInput`] (see [`Self::captured_client`]).
    pub view_index: u8,
    /// The forwarded virtual pointer position in the *active remote's* pixel
    /// space, advanced by libinput deltas while in capture mode and clamped to
    /// that client's registered size. Recentred on every view switch. Only
    /// meaningful while `view_index != 0`.
    pub remote_pointer: (f64, f64),

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

    /// Sticky windows — shown on every workspace. A sticky window is
    /// exempt from the unmap/remap that a workspace switch does to every
    /// other window (see [`Self::focus_workspace_id`]); it stays mapped
    /// and is reassigned to whatever workspace becomes active so the
    /// "windows on the active workspace == mapped elements" invariant
    /// holds. Entries are removed in `toplevel_destroyed`.
    pub sticky: HashSet<Window>,

    /// Always-on-top windows — kept above all ordinary windows in the
    /// stacking order. Implemented by bumping the window's smithay z-index
    /// above [`RenderZindex::Shell`] (see [`Self::set_always_on_top`]); the
    /// `Space` keeps elements sorted by z-index, so this set is just for
    /// reporting + toggle bookkeeping. Entries are removed in
    /// `toplevel_destroyed`.
    pub always_on_top: HashSet<Window>,

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
    /// Cached kill-cursor (`×`) buffer for the active window picker, keyed by
    /// the scale it was rasterized at. Built on demand while a picker is armed
    /// and reused across frames; see [`crate::cursor::Cursor::kill_frame`].
    pub kill_cursor: Option<(
        u32,
        smithay::backend::renderer::element::memory::MemoryRenderBuffer,
    )>,

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
    /// `zwlr_data_control_manager_v1` — lets an out-of-focus client (the
    /// `shoestring-clipboard` bridge, cliphist, copyq) OBSERVE and SET the
    /// clipboard + primary selection without holding keyboard focus. Held for
    /// the global's lifetime. Drives the cross-machine clipboard sync (task
    /// 181); composes with the XWayland bridge via the shared `SelectionHandler`.
    pub data_control_state: smithay::wayland::selection::wlr_data_control::DataControlState,
}

impl ShoestringWm {
    /// Construct the full session compositor: opens a Wayland listening socket
    /// so external clients can connect. This is the path `main` takes for both
    /// real backends.
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
        config_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self::new_inner(event_loop, display, config, config_path, true)
    }

    /// Construct the compositor for the WLCS conformance harness: identical
    /// global/state setup, but **without** a listening socket. WLCS injects its
    /// clients directly via [`DisplayHandle::insert_client`] over socketpairs,
    /// so a public socket would be unused (and racy in CI). IPC/metrics/Xwayland
    /// are not started here — those are `main`'s job, not the constructor's.
    pub fn new_headless(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
    ) -> Self {
        Self::new_inner(event_loop, display, config, None, false)
    }

    fn new_inner(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        config: Config,
        config_path: Option<std::path::PathBuf>,
        open_socket: bool,
    ) -> Self {
        let dh = display.handle();

        let (bindings, bind_warnings) = crate::binds::BindingTable::compile(&config);
        for w in bind_warnings {
            tracing::warn!(target: "shoestring_wm::config", "{w}");
        }
        for e in config.window_rule_regex_errors() {
            tracing::warn!(target: "shoestring_wm::config", "{e}");
        }
        bindings.log_compiled();

        // Monotonic clock + the wp_presentation global, advertising that
        // clock's id so clients read `presented` timestamps correctly.
        let clock: smithay::utils::Clock<smithay::utils::Monotonic> = smithay::utils::Clock::new();
        let presentation_state =
            smithay::wayland::presentation::PresentationState::new::<Self>(&dh, clock.id() as u32);

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xdg_foreign_state = XdgForeignState::new::<Self>(&dh);
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
        // ext-workspace-v1: advertise the manager (v1) unconditionally so
        // standard bars see our workspaces. Backend-agnostic, like the foreign
        // toplevel globals above.
        let ext_workspace = crate::ext_workspace::ExtWorkspaceState {
            manager_global: dh
                .create_global::<Self, _, _>(1, crate::ext_workspace::ExtWorkspaceManagerData),
            instances: Vec::new(),
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
        // ext-image-copy-capture / ext-image-capture-source: the standardized
        // successor to wlr-screencopy, gated by the same capture flag. Its two
        // manager globals are registered once with a visibility filter reading
        // `capture_gate`, so they only appear to clients while the gate is on.
        let capture_gate =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(screen_capture_enabled));
        let ext_screencopy =
            crate::ext_screencopy::ExtScreencopyState::new::<Self>(&dh, capture_gate.clone());
        // wlr-virtual-pointer: advertise the manager (v2) unconditionally so
        // pointer-injection clients (wlrctl, remote desktop) work out of the
        // box, the way wlroots does. Unlike the WM's own IPC inject path, this
        // is a standard client protocol and isn't behind the automation gate.
        let virtual_pointer = crate::virtual_pointer::VirtualPointerState {
            manager_global: dh
                .create_global::<Self, _, _>(2, crate::virtual_pointer::VirtualPointerManagerData),
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
        // wlr-data-control: advertise the manager (v2) unconditionally so
        // clipboard managers and the cross-machine clipboard bridge can read +
        // write the selection out of focus. Passing the primary-selection state
        // enables primary over data-control too. `|_| true` = visible to every
        // client (the protocol is the standard wlroots clipboard-manager API).
        let data_control_state =
            smithay::wayland::selection::wlr_data_control::DataControlState::new::<Self, _>(
                &dh,
                Some(&primary_selection_state),
                |_| true,
            );

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");
        // Build the XKB keymap from [general].xkb_* (layouts, variants,
        // options, rules, model). Empty fields fall back to xkbcommon's own
        // defaults, so the common single-layout case needs no config.
        let xkb_config = smithay::input::keyboard::XkbConfig {
            rules: &config.general.xkb_rules,
            model: &config.general.xkb_model,
            layout: &config.general.xkb_layout,
            variant: &config.general.xkb_variant,
            options: config.general.xkb_options.clone(),
        };
        if seat
            .add_keyboard(
                xkb_config,
                config.general.repeat_delay,
                config.general.repeat_rate,
            )
            .is_err()
        {
            // A bad layout/variant/option must not wedge the whole session —
            // warn and come up with the default keymap instead.
            tracing::warn!(
                layout = %config.general.xkb_layout,
                variant = %config.general.xkb_variant,
                "invalid XKB config; falling back to the default keymap"
            );
            seat.add_keyboard(
                Default::default(),
                config.general.repeat_delay,
                config.general.repeat_rate,
            )
            .unwrap();
        }
        seat.add_pointer();

        // Pointer lock/confine (FPS games, RDP) plus its required companion,
        // relative-pointer. Both are plain always-on globals: constraint
        // enforcement lives in the input motion path, and relative motion is
        // already emitted on every libinput delta. Backend-agnostic.
        let pointer_constraints =
            smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer =
            smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(&dh);
        // Drawing tablets. Always-on global (no companion protocol to gate on,
        // unlike idle-inhibit); real tool events arrive only from the libinput
        // backend, but advertising costs nothing on winit.
        let tablet_manager = smithay::wayland::tablet_manager::TabletManagerState::new::<Self>(&dh);
        // Touchpad multi-finger gestures (swipe/pinch/hold). Always-on global
        // like the tablet/constraint managers; gesture events are emitted only
        // by the libinput backend and forwarded to the pointer in input.rs.
        let pointer_gestures =
            smithay::wayland::pointer_gestures::PointerGesturesState::new::<Self>(&dh);
        // Server-side cursor naming. Pairs with the pointer/tablet seat: a
        // client's `set_shape` becomes a `Named` cursor we draw from the
        // theme (see [`crate::cursor`]). Always advertised — costs nothing
        // when unused and lets toolkits drop client-side xcursor entirely.
        let cursor_shape_manager =
            smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(&dh);
        // Keyboard-shortcuts inhibit. A focused client that needs the raw key
        // stream — a nested compositor, a VM/SPICE window, a remote-desktop
        // viewer, a game — asks us to stop intercepting its keybinds. We grant
        // every request (single-user WM, no privilege model to enforce) but
        // only ever *activate* the inhibitor whose surface holds keyboard
        // focus; see the `KeyboardShortcutsInhibitHandler` impl.
        let keyboard_shortcuts_inhibit_state =
            smithay::wayland::keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitState::new::<Self>(
                &dh,
            );
        // Input-method support (CJK and other composed input via fcitx5/ibus).
        // Three cooperating globals form one subsystem:
        //   - zwp_text_input_v3: the application/toolkit side that wants text.
        //   - zwp_input_method_v2: the IME side that composes and commits it.
        //   - zwp_virtual_keyboard_v1: the IME re-injects keystrokes it doesn't
        //     consume (latin passthrough); without it fcitx5's "off" state eats
        //     keys, so the IME is only half-usable. (This is also task 116's
        //     protocol — an on-screen keyboard is just another vk client.)
        // smithay bridges text-input <-> input-method automatically on keyboard
        // focus (its `KeyboardTarget for WlSurface`) and installs the IME
        // keyboard grab; our keybind filter runs *before* that grab (see
        // `KeyboardHandle::input`), so Super-binds keep working while an IME is
        // active. Candidate popups ride the normal window-popup render path.
        // The manager states are inert after construction — the live handles
        // hang off the seat's user_data and the globals stay registered for the
        // display's lifetime — so, like anvil, we don't retain them. Allow every
        // client: an IME is an ordinary unprivileged process.
        smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh);
        smithay::wayland::input_method::InputMethodManagerState::new::<Self, _>(&dh, |_client| {
            true
        });
        smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<Self, _>(
            &dh,
            |_client| true,
        );

        let automation_enabled = config.general.automation_enabled;

        // Only advertise ext_idle_notify_v1 when the user opted in. Creating
        // the state also creates the global, so gating creation here is what
        // keeps the protocol entirely absent (not merely inert) by default.
        let idle_notifier = config.general.idle_notifications_enabled.then(|| {
            smithay::wayland::idle_notify::IdleNotifierState::new(&dh, event_loop.handle())
        });
        // Pair zwp_idle_inhibit_manager_v1 with the notifier: inhibiting idle
        // only has meaning when something advertises idle in the first place.
        let idle_inhibit_manager = config
            .general
            .idle_notifications_enabled
            .then(|| smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(&dh));

        let space = Space::default();
        let popups = PopupManager::default();
        let workspaces = build_workspace_manager(&config.workspaces);

        // Headless (WLCS) construction skips the listening socket; clients are
        // injected directly. Give the field a recognizable placeholder so any
        // accidental use surfaces obviously rather than as an empty string.
        let socket_name = Self::init_wayland_listener(display, event_loop, open_socket)
            .unwrap_or_else(|| OsString::from("wlcs-headless"));
        let loop_signal = event_loop.get_signal();
        let loop_handle = event_loop.handle();

        Self {
            start_time: Instant::now(),
            clock,
            presentation_state,
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
            window_borders: HashMap::new(),
            diag_overlay: crate::diag_overlay::DiagOverlay::default(),
            wallpaper: crate::wallpaper::Wallpaper::default(),
            #[cfg(feature = "tty")]
            udev: None,
            compositor_state,
            xdg_shell_state,
            xdg_activation_state,
            xdg_foreign_state,
            xdg_decoration_state,
            layer_shell_state,
            foreign_toplevel_list,
            foreign_toplevels: HashMap::new(),
            window_name_overrides: HashMap::new(),
            foreign_toplevel_mgmt,
            ext_workspace,
            shm_state,
            viewporter_state,
            fractional_scale_manager_state,
            output_manager_state,
            seat_state,
            data_device_state,
            output_management,
            screencopy,
            ext_screencopy,
            virtual_pointer,
            dmabuf_state: DmabufState::new(),
            #[cfg(feature = "tty")]
            dmabuf_global: None,
            dmabuf_formats: None,
            session_lock_state,
            #[cfg(feature = "tty")]
            gamma_control,
            idle_notifier,
            idle_inhibit_manager,
            idle_inhibitors: Vec::new(),
            lock_session: None,
            seat,
            touch_output_by_device: HashMap::new(),
            pointer_constraints,
            relative_pointer,
            tablet_manager,
            pointer_gestures,
            cursor_shape_manager,
            keyboard_shortcuts_inhibit_state,
            active_shortcuts_inhibitor: None,
            pointer_constraint_cursor_hint: None,
            last_pointer_focus: None,
            ipc: None,
            metrics: crate::metrics::Metrics::new(),
            // Default to the safe (non-integrating) stance; `main` flips this
            // on for the real session backend once the backend is chosen.
            session_integration: false,
            automation_enabled,
            screen_capture_enabled,
            capture_gate,
            last_screen_capture_event: None,
            media: None,
            // Remote-desktop serve mode: off and serverless until the user
            // enables it after a server registers (runtime-only consent).
            remote_enabled: false,
            remote_server: None,
            remote_viewers: 0,
            remote_clients: Vec::new(),
            view_index: 0,
            remote_pointer: (0.0, 0.0),
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
            sticky: HashSet::new(),
            always_on_top: HashSet::new(),
            pending_initial_center: HashSet::new(),
            cursor: crate::cursor::Cursor::load(),
            cursor_status: CursorImageStatus::default_named(),
            pointer_element: crate::drawing::PointerElement::default(),
            pointer_image: None,
            kill_cursor: None,
            xwm: None,
            xdisplay: None,
            xwayland_shell_state,
            primary_selection_state,
            data_control_state,
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

    /// Register the display dispatch source, and — when `open_socket` is set —
    /// a Wayland listening socket. Returns the socket name when one was opened
    /// (`None` in the headless/WLCS path, which injects clients directly). The
    /// display `Generic` source is always registered; only the listener is
    /// conditional.
    fn init_wayland_listener(
        display: Display<Self>,
        event_loop: &mut EventLoop<'static, Self>,
        open_socket: bool,
    ) -> Option<OsString> {
        let loop_handle = event_loop.handle();

        let socket_name = if open_socket {
            let listening_socket = ListeningSocketSource::new_auto().unwrap();
            let name = listening_socket.socket_name().to_os_string();
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
            Some(name)
        } else {
            None
        };

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                move |_, display, state| {
                    // SAFETY: we never drop the display while the source is registered.
                    unsafe {
                        let d = display.get_mut();
                        if let Err(e) = d.dispatch_clients(state) {
                            tracing::error!("dispatch_clients error: {e}");
                        }
                        // Real sessions flush eagerly so clients receive replies even
                        // when no redraw is firing (e.g. nested winit not visible).
                        //
                        // The headless WLCS harness (open_socket == false) must NOT:
                        // eager-flushing here sends a client's `wl_display.sync` reply
                        // the instant the sync request is dispatched — before a pending
                        // fake-input `WlcsEvent` (a separate calloop source) has been
                        // applied. WLCS injects input then roundtrips to observe it, so
                        // the premature reply makes the roundtrip see stale pointer
                        // focus and the assertion fails (often flakily). Deferring to
                        // the harness loop's single end-of-iteration `flush_clients`
                        // — exactly what smithay's `wlcs_anvil` does — keeps injected
                        // input ordered ahead of the sync that observes it.
                        if open_socket {
                            let _ = d.flush_clients();
                        }
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

    /// True if an **Overlay**- or **Top**-layer surface sits under `pos`, above
    /// the window stack. Such a click belongs to that layer surface (a tray
    /// menu, the region picker), so focusing a window it merely overlaps would
    /// wrongly steal the layer surface's keyboard focus — see the click-to-focus
    /// path in [`crate::input`].
    pub fn overlay_layer_under(&self, pos: Point<f64, Logical>) -> bool {
        use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

        let Some((output, geo)) = self.space.outputs().find_map(|o| {
            self.space
                .output_geometry(o)
                .filter(|g| g.to_f64().contains(pos))
                .map(|g| (o.clone(), g))
        }) else {
            return false;
        };
        let local = pos - geo.loc.to_f64();
        let map = layer_map_for_output(&output);
        for layer in [WlrLayer::Overlay, WlrLayer::Top] {
            if let Some(ls) = map.layer_under(layer, local) {
                let layer_loc = map.layer_geometry(ls).map(|g| g.loc).unwrap_or_default();
                let inner = local - layer_loc.to_f64();
                if ls.surface_under(inner, WindowSurfaceType::ALL).is_some() {
                    return true;
                }
            }
        }
        false
    }

    /// Re-evaluate the surface under the pointer at its *current* location and,
    /// if it changed, deliver pointer enter/leave/motion.
    ///
    /// Wayland requires pointer focus to track the scene, not just real motion:
    /// when a surface maps, unmaps, or slides under a stationary cursor the
    /// client must still see enter/leave/motion. The input handlers only
    /// refocus on actual motion, so this runs once per event-loop iteration
    /// (after client commits and any WM-driven map/move/unmap) to catch the
    /// stationary cases — the WLCS `ClientSurfaceEventsTest` /
    /// `SurfacePointerMotionTest` family.
    ///
    /// No-op while a pointer grab owns the focus (interactive move/resize,
    /// super-drag — the grab drives its own focus) and while the pointer is
    /// locked to a surface (a locked client must receive no absolute motion).
    /// Cheap in the common case: it returns early unless the surface *or* its
    /// global offset under the cursor differs from what was last delivered, so
    /// running it every loop iteration costs one [`Self::surface_under`] lookup.
    pub fn refresh_pointer_focus(&mut self) {
        use smithay::input::pointer::MotionEvent;
        use smithay::utils::SERIAL_COUNTER;
        use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint};

        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        if pointer.is_grabbed() {
            return;
        }

        let location = pointer.current_location();
        let under = self.surface_under(location);

        // Same surface at the same offset — nothing the client hasn't seen.
        if under == self.last_pointer_focus {
            return;
        }

        // A locked pointer must not receive absolute motion. The cursor can't
        // have moved under an active lock, so this only guards the degenerate
        // "locked surface itself moved" case; the confined case is fine —
        // motion already stayed inside the surface, so its region still holds.
        if let Some((surface, _)) = under.as_ref() {
            let locked = with_pointer_constraint(surface, &pointer, |c| {
                c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
            });
            if locked {
                return;
            }
        }

        self.last_pointer_focus = under.clone();
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location,
                serial,
                time: crate::inject::monotonic_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Global-space origin (top-left) of the mapped window backing `surface`,
    /// for translating a surface-local point into compositor-global coords.
    /// Used to restore the visible cursor from a locked-pointer client's
    /// cursor-position hint when its lock is released. `None` if no mapped
    /// window owns the surface.
    pub fn surface_global_origin(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        let window = self
            .space
            .elements()
            .find(|w| crate::window_ext::matches_surface(w, surface))?;
        self.space.element_location(window).map(|loc| loc.to_f64())
    }

    /// The window whose toplevel surface currently holds keyboard focus.
    /// Matches both xdg and X11 windows via [`crate::window_ext::matches_surface`].
    pub fn focused_window(&self) -> Option<Window> {
        let focused = self.seat.get_keyboard()?.current_focus()?;
        let surface = focused.surface()?;
        self.space
            .elements()
            .find(|w| crate::window_ext::matches_surface(w, &surface))
            .cloned()
    }

    /// The tracked window whose toplevel root surface is `surface`, searching
    /// both mapped (active-workspace) elements and windows parked on other
    /// workspaces or minimized. Used to resolve an `xdg_activation` target,
    /// which names a `wl_surface` that may belong to a window not currently
    /// in the space.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        if let Some(w) = self
            .space
            .elements()
            .find(|w| crate::window_ext::matches_surface(w, surface))
        {
            return Some(w.clone());
        }
        self.workspaces
            .windows_on_any()
            .find(|(w, _)| crate::window_ext::matches_surface(w, surface))
            .map(|(w, _)| w)
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
            if let Some(target) = crate::window_ext::keyboard_focus(window) {
                kb.set_focus(self, Some(target), serial);
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
            if let Some(target) = crate::window_ext::keyboard_focus(window) {
                kb.set_focus(self, Some(target), serial);
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

    /// Raise `window` to the top of the stacking order, leaving keyboard
    /// focus and the activated state untouched (unlike [`focus_window`],
    /// which raises *and* focuses). A no-op when the window isn't mapped —
    /// minimized windows are unmapped from the space, and off-workspace
    /// windows live only in the workspace manager. Emits `window_restacked`.
    pub fn raise_window(&mut self, window: &Window) {
        if self.space.element_location(window).is_none() {
            return;
        }
        // activate=false: a pure restack must not steal the activated bit
        // from whatever currently holds focus.
        self.space.raise_element(window, false);
        self.notify_restacked(window);
    }

    /// Lower `window` to the bottom of the stacking order, leaving keyboard
    /// focus untouched. The complement of [`raise_window`]; same unmapped
    /// no-op semantics.
    pub fn lower_window(&mut self, window: &Window) {
        if self.space.element_location(window).is_none() {
            return;
        }
        self.space.lower_element(window);
        self.notify_restacked(window);
    }

    /// Emit a `window_restacked` event for `window` after its Z position
    /// changed. The whole stack's Z indices shift on a raise/lower, so the
    /// event only names the window that moved; subscribers re-query
    /// `windows` / `get_tree` for the full order.
    fn notify_restacked(&mut self, window: &Window) {
        if let Some(id) = self.foreign_toplevels.get(window).map(|h| h.identifier()) {
            self.emit_ipc(shoestring_ipc::Event::WindowRestacked { id });
        }
    }

    /// Whether `window` is sticky (shown on every workspace).
    pub fn is_sticky(&self, window: &Window) -> bool {
        self.sticky.contains(window)
    }

    /// One-shot tile every window on the active workspace into `kind`, per
    /// output. Windows are grouped by the output they currently sit on (so each
    /// monitor is tiled independently inside its own usable rect) and ordered
    /// by their current top-left for a stable, reading-order placement.
    /// Minimized and fullscreen windows are left untouched. This holds no
    /// persistent layout state — afterwards the windows are ordinary floating
    /// windows. Bound to the Super+G cluster by default ([`Action::ArrangeGrid`]
    /// / `ArrangeSpiral` / `ArrangeBsp`).
    pub fn arrange_active_workspace(&mut self, kind: crate::layout::ArrangeKind) {
        use smithay::utils::Rectangle;
        let active = self.workspaces.active();
        // Tileable windows on the active workspace: alive, mapped, not
        // minimized, not app-driven-fullscreen.
        let tileable: Vec<Window> = self
            .workspaces
            .windows_on(active)
            .into_iter()
            .filter(|w| {
                w.alive()
                    && self.space.element_location(w).is_some()
                    && !self.layout.is_minimized(w)
                    && self.layout.layout_state(w) != crate::layout::LayoutState::Fullscreen
            })
            .collect();
        if tileable.is_empty() {
            tracing::debug!(?kind, "arrange: no tileable windows on active workspace");
            return;
        }
        // Group by output via each window's usable rect: an identical rect means
        // the same output (the output's position is baked into the rect's loc).
        // `Rectangle` isn't `Hash`, so key on its (x, y, w, h) tuple and carry
        // the rect along in the value.
        type RectKey = (i32, i32, i32, i32);
        let mut groups: HashMap<RectKey, (Rectangle<i32, Logical>, Vec<Window>)> = HashMap::new();
        for w in tileable {
            if let Some(usable) = crate::layout::usable_rect_for(&self.space, &w) {
                let key = (usable.loc.x, usable.loc.y, usable.size.w, usable.size.h);
                groups
                    .entry(key)
                    .or_insert_with(|| (usable, Vec::new()))
                    .1
                    .push(w);
            }
        }
        for (_, (usable, mut group)) in groups {
            // Stable, intuitive order: top-to-bottom, then left-to-right.
            group.sort_by_key(|w| {
                let loc = self.space.element_location(w).unwrap_or_default();
                (loc.y, loc.x)
            });
            crate::layout::arrange(&mut self.space, &mut self.layout, &group, usable, kind);
            // Persist the new positions so a workspace round-trip keeps them.
            for w in &group {
                if let Some(loc) = self.space.element_location(w) {
                    self.workspaces.record_location(w, loc);
                }
            }
        }
        tracing::debug!(?kind, workspace = active.one_based(), "arranged workspace");
    }

    /// Toggle the sticky flag on the focused window. No-op when nothing is
    /// focused. Bound to Super+S by default ([`Action::ToggleSticky`]).
    pub fn toggle_sticky_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("toggle_sticky: no focused window");
            return;
        };
        let now = !self.is_sticky(&window);
        self.set_sticky(&window, now);
    }

    /// Set (or clear) the sticky flag on `window`. A sticky window stays
    /// mapped across workspace switches and is reported as belonging to the
    /// active workspace. Clearing it leaves the window on the currently
    /// active workspace (where it is visible right now). Idempotent; emits
    /// `window_sticky_changed` only on an actual change.
    pub fn set_sticky(&mut self, window: &smithay::desktop::Window, sticky: bool) {
        let changed = if sticky {
            self.sticky.insert(window.clone())
        } else {
            self.sticky.remove(window)
        };
        if !changed {
            return;
        }
        // A window made sticky belongs to the workspace it's visible on
        // right now (the active one); clearing leaves it there too.
        self.workspaces.reassign(window, self.workspaces.active());
        tracing::debug!(sticky, "window sticky toggled");
        if let Some(id) = self.foreign_toplevels.get(window).map(|h| h.identifier()) {
            self.emit_ipc(shoestring_ipc::Event::WindowStickyChanged { id, sticky });
        }
    }

    /// Whether `window` is always-on-top.
    pub fn is_always_on_top(&self, window: &Window) -> bool {
        self.always_on_top.contains(window)
    }

    /// Toggle the always-on-top flag on the focused window. No-op when
    /// nothing is focused. Bound to Super+A by default
    /// ([`Action::ToggleAlwaysOnTop`]).
    pub fn toggle_always_on_top_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("toggle_always_on_top: no focused window");
            return;
        };
        let now = !self.is_always_on_top(&window);
        self.set_always_on_top(&window, now);
    }

    /// Set (or clear) the always-on-top flag on `window`. Works by bumping
    /// the window's smithay z-index above [`RenderZindex::Shell`] (ordinary
    /// windows) but below [`RenderZindex::Top`] (layer-shell bars/overlays),
    /// so it floats above other windows while bars and menus stay reachable.
    /// The `Space` only re-sorts on insert/raise, so we re-raise a mapped
    /// window to apply the new ordering immediately. Idempotent; emits
    /// `window_always_on_top_changed` only on an actual change.
    pub fn set_always_on_top(&mut self, window: &smithay::desktop::Window, on: bool) {
        let changed = if on {
            self.always_on_top.insert(window.clone())
        } else {
            self.always_on_top.remove(window)
        };
        if !changed {
            return;
        }
        let z = if on {
            ALWAYS_ON_TOP_ZINDEX
        } else {
            smithay::desktop::space::RenderZindex::Shell as u8
        };
        window.override_z_index(z);
        // The Space keeps its element list sorted by z-index but only
        // re-sorts when an element is (re-)inserted. Re-raise the window (no
        // activation change) so the new z-index takes effect now; skip it
        // when the window isn't mapped — the z-index persists for next map.
        if self.space.element_location(window).is_some() {
            self.space.raise_element(window, false);
        }
        tracing::debug!(on, "window always-on-top toggled");
        if let Some(id) = self.foreign_toplevels.get(window).map(|h| h.identifier()) {
            self.emit_ipc(shoestring_ipc::Event::WindowAlwaysOnTopChanged {
                id,
                always_on_top: on,
            });
        }
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
            kb.set_focus(
                self,
                Option::<crate::focus::KeyboardFocusTarget>::None,
                serial,
            );
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

    /// Recompute whether idle is inhibited and tell the idle notifier.
    ///
    /// Cheap and idempotent: a no-op when idle notifications are off, and an
    /// `is_empty()` check away from free in the common case (nothing
    /// inhibiting). Call it wherever an inhibitor is added/removed or a
    /// window's visibility changes (map, unmap, workspace switch, minimize).
    pub fn refresh_idle_inhibit(&mut self) {
        if self.idle_notifier.is_none() {
            return;
        }
        // Drop inhibitors whose surface died without an explicit destroy
        // request (a crashed client never sends one). The visibility test
        // below would already ignore them, but pruning keeps the Vec bounded.
        self.idle_inhibitors.retain(IsAlive::alive);
        let inhibited = self.idle_inhibit_active();
        if let Some(notifier) = self.idle_notifier.as_mut() {
            notifier.set_is_inhibited(inhibited);
        }
    }

    /// True iff at least one inhibiting surface is currently *visible* —
    /// mapped in the space, which (since off-workspace and minimized windows
    /// are unmapped) means on the active workspace and not minimized. Smithay
    /// explicitly leaves "ignore invisible/dead surfaces" to the compositor.
    pub(crate) fn idle_inhibit_active(&self) -> bool {
        if self.idle_inhibitors.is_empty() {
            return false;
        }
        self.idle_inhibitors
            .iter()
            .any(|surface| self.surface_is_mapped(surface))
    }

    /// Whether `surface` (walked up to its root) belongs to a window
    /// currently mapped in the space.
    fn surface_is_mapped(&self, surface: &WlSurface) -> bool {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.space
            .elements()
            .any(|w| crate::window_ext::matches_surface(w, &root))
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

        // Snapshot positions then unmap everything currently on screen —
        // except sticky windows, which stay mapped (and keep their spot)
        // so they appear on every workspace.
        let to_unmap: Vec<Window> = self.space.elements().cloned().collect();
        for w in &to_unmap {
            if let Some(loc) = self.space.element_location(w) {
                self.workspaces.record_location(w, loc);
            }
        }
        for w in &to_unmap {
            if self.sticky.contains(w) {
                continue;
            }
            self.space.unmap_elem(w);
        }

        self.workspaces.set_active(target);

        // Sticky windows follow the active workspace so the "windows on the
        // active workspace == mapped elements" invariant holds for the IPC
        // queries and focus history.
        for w in &to_unmap {
            if self.sticky.contains(w) {
                self.workspaces.reassign(w, target);
            }
        }

        // Re-map every window assigned to the target workspace, skipping
        // those that are still minimized or already mapped (a sticky window
        // that was never unmapped — re-mapping would disturb its stacking).
        for w in self.workspaces.windows_on(target) {
            if self.layout.is_minimized(&w) {
                continue;
            }
            if self.space.elements().any(|el| el == &w) {
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
        crate::ext_workspace::broadcast_active(self);
        // The set of mapped windows just changed wholesale; a video player
        // may have come into or gone out of view.
        self.refresh_idle_inhibit();
    }

    /// Refresh `pointer_element`'s memory buffer for the next render. Picks
    /// the right xcursor frame for `scale` + elapsed time; only uploads a new
    /// `MemoryRenderBuffer` when the chosen frame differs from the previous
    /// (true for every render on static cursors). The buffer's reported scale
    /// matches the output's so smithay maps the HiDPI sprite back to its
    /// nominal logical size — otherwise a 48px frame at output-scale 2 would
    /// render at 96 physical pixels.
    pub fn refresh_cursor_buffer(&mut self, scale: u32) {
        // While a window picker is armed (shoestring-kill), every output shows
        // the xkill-style kill cursor instead of the client's chosen sprite —
        // an unmistakable "click to close" `×`. Build it once per scale and
        // force the pointer element onto the buffer path so `drawing.rs` draws
        // it. The status is resynced from `cursor_status` when the picker
        // resolves (see `finish_picker`).
        if self.pending_picker.is_some() {
            let s = scale.max(1);
            if self.kill_cursor.as_ref().map(|(cs, _)| *cs) != Some(s) {
                let (size, pixels) = self.cursor.kill_frame(s);
                let buffer =
                    smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
                        &pixels,
                        smithay::backend::allocator::Fourcc::Argb8888,
                        (size as i32, size as i32),
                        s as i32,
                        smithay::utils::Transform::Normal,
                        None,
                    );
                self.kill_cursor = Some((s, buffer));
            }
            let buffer = self.kill_cursor.as_ref().unwrap().1.clone();
            self.pointer_element.set_buffer(buffer);
            self.pointer_element.set_status(CursorImageStatus::Named(
                smithay::input::pointer::CursorIcon::Default,
            ));
            return;
        }

        // Only the named-cursor path rasterizes a theme sprite. A client
        // that committed its own cursor surface is drawn from its buffer in
        // `drawing.rs`, and `Hidden` draws nothing — in both cases there is
        // no theme frame to upload.
        let icon = match &self.cursor_status {
            smithay::input::pointer::CursorImageStatus::Named(icon) => *icon,
            _ => return,
        };
        let elapsed = self.start_time.elapsed();
        let Some(frame) = self.cursor.current_frame(icon, scale, elapsed).cloned() else {
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
        // Picker active: draw the kill cursor regardless of what the client
        // under the pointer requested (Named, Surface, or even Hidden), with
        // the hotspot at its center (`size/2` logical px). The buffer was set
        // by `refresh_cursor_buffer`.
        if self.pending_picker.is_some() {
            let half = (self.cursor.size() / 2) as i32;
            return Some((self.pointer_element.clone(), location, (half, half)));
        }
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

    /// Schedule an immediate render of every output. Used when something that
    /// isn't tied to client damage changes the scene — e.g. the picker's kill
    /// cursor arming/resolving. winit renders continuously, so this is a no-op
    /// there; the udev backend wakes each output's CRTC via an idle callback.
    pub fn kick_render_all(&mut self) {
        #[cfg(feature = "tty")]
        {
            if self.udev.is_some() {
                let targets: Vec<_> = self
                    .space
                    .outputs()
                    .filter_map(|o| {
                        o.user_data()
                            .get::<crate::backend::udev::UdevOutputId>()
                            .map(|id| (id.device_id, id.crtc))
                    })
                    .collect();
                for (node, crtc) in targets {
                    self.loop_handle.insert_idle(move |state| {
                        state.render_surface(node, crtc);
                    });
                }
            }
        }
    }

    /// Apply the automation gate, coupling the screen-capture gate to follow
    /// it. Automation is a *superset* of screen capture: a session an agent can
    /// drive, it can also observe — the automation-gated `screenshot` request
    /// is implemented via the `zwlr_screencopy_manager_v1` global, which only
    /// the capture gate raises. Enabling automation therefore also opens the
    /// capture gate (so `automation on -> screenshot -> automation off` works
    /// without a second toggle), and disabling it closes the capture gate again
    /// so the session returns to a no-capture state. Coupling runs through
    /// [`Self::set_screen_capture`], so the bar's capture indicator stays lit —
    /// observation is never silent.
    ///
    /// Returns whether the screen-capture gate changed as a side effect, so the
    /// caller can emit the matching `ScreenCaptureChanged` event. The caller
    /// owns automation logging / `AutomationChanged` (mirrors set_screen_capture).
    pub fn set_automation(&mut self, enabled: bool) -> bool {
        self.automation_enabled = enabled;
        let capture_changed = self.screen_capture_enabled != enabled;
        if capture_changed {
            self.set_screen_capture(enabled);
        }
        capture_changed
    }

    /// Flip the **remote gate**, coupling the automation + capture gates:
    /// enabling turns both on (so the served output can stream and client input
    /// can inject), disabling turns both off (which tears down any capture
    /// stream). Returns `(automation_changed, capture_changed)` so the caller
    /// can emit the matching `AutomationChanged` / `ScreenCaptureChanged`
    /// events for the bar's AUTO/CAP chips. Does *not* emit `RemoteChanged`
    /// itself — the caller does that via [`Self::emit_remote_changed`] once it
    /// has decided whether the value actually moved.
    pub fn set_remote(&mut self, enabled: bool) -> (bool, bool) {
        self.remote_enabled = enabled;
        let auto_before = self.automation_enabled;
        let capture_changed = self.set_automation(enabled);
        let auto_changed = self.automation_enabled != auto_before;
        if !enabled {
            // No stream ⇒ no viewers; the server will also re-report 0.
            self.remote_viewers = 0;
        }
        (auto_changed, capture_changed)
    }

    /// Broadcast the current remote-desktop state to subscribers — the bar
    /// (toggle + "being viewed" chip) and the registered server (which reads
    /// `enabled` to open / close its listener).
    pub fn emit_remote_changed(&mut self) {
        let server_available = self.remote_server.is_some();
        self.emit_ipc(shoestring_ipc::Event::RemoteChanged {
            enabled: self.remote_enabled,
            server_available,
            viewers: self.remote_viewers,
        });
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
        // Mirror into the shared flag the ext-image-copy-capture globals' filters
        // read, so they appear/disappear from new clients' registries in step.
        self.capture_gate
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
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
            // ext-image-copy-capture: dropping the owned sessions sends `stopped`
            // (and fails their in-flight frames); also fail anything already
            // queued for a render pass so no client hangs waiting on a capture
            // the gate just revoked.
            self.ext_screencopy.sessions.clear();
            for p in self.ext_screencopy.pending.drain(..) {
                p.frame
                    .fail(crate::ext_screencopy::CaptureFailureReason::Stopped);
            }
            // Streaming capture subscribers (remote-desktop serve mode) must
            // stop the instant consent is revoked: send a final Bye and drop.
            self.teardown_capture_subscribers();
        }
    }

    /// Cache a media-privacy snapshot reported by the `shoestring-mediad`
    /// monitor and return whether it actually changed (so the caller can decide
    /// to broadcast [`shoestring_ipc::Event::MediaChanged`]). The WM is a pure
    /// cache here — it never authors mute state, only reflects PipeWire's truth.
    pub fn set_media(&mut self, snapshot: shoestring_ipc::MediaState) -> bool {
        let changed = self.media != Some(snapshot);
        self.media = Some(snapshot);
        changed
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

    /// Validate that a client's dmabuf can actually be imported by our
    /// renderer — the import test the `zwp_linux_dmabuf_v1` protocol expects
    /// before a buffer is created. Only the udev backend stands up the dmabuf
    /// global (winit has no dmabuf export), so the winit path is never reached
    /// in practice; we accept there rather than reject.
    pub fn import_dmabuf_test(&mut self, dmabuf: &Dmabuf) -> bool {
        #[cfg(feature = "tty")]
        if let Some(udev) = self.udev.as_mut() {
            return udev.import_dmabuf_test(dmabuf);
        }
        let _ = dmabuf;
        true
    }

    /// Pixel formats to advertise in the screencopy `linux_dmabuf` event, in
    /// client-preference order. Empty on the winit backend (no dmabuf global),
    /// so callers stay shm-only there. Opaque (`Xrgb8888`) is offered first
    /// since screencast streams are opaque; we only offer a format the
    /// renderer can actually import.
    pub fn screencopy_dmabuf_formats(&self) -> Vec<smithay::backend::allocator::Fourcc> {
        use smithay::backend::allocator::Fourcc;
        let Some(formats) = self.dmabuf_formats.as_ref() else {
            return Vec::new();
        };
        [Fourcc::Xrgb8888, Fourcc::Argb8888]
            .into_iter()
            .filter(|fourcc| formats.iter().any(|f| f.code == *fourcc))
            .collect()
    }

    /// Buffer constraints to advertise for an `ext-image-copy-capture` session
    /// targeting `output`: the output's full pixel size, the shm formats we can
    /// read back into, and — on KMS — the dmabuf formats/modifiers our renderer
    /// can scan the scene straight into. Returns `None` if the output has no
    /// current mode. Mirrors the buffer kinds offered by the wlr path.
    pub fn ext_capture_constraints_for_output(
        &self,
        output: &smithay::output::Output,
    ) -> Option<crate::ext_screencopy::BufferConstraints> {
        use smithay::reexports::wayland_server::protocol::wl_shm;
        let mode = output.current_mode()?;
        let size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (mode.size.w, mode.size.h).into();
        // Both layouts are byte-identical to our ARGB readback; offering Xrgb
        // too lets opaque consumers (screencast) avoid an alpha channel.
        let shm = vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888];
        Some(crate::ext_screencopy::BufferConstraints {
            size,
            shm,
            // smithay only carries the `dma` field when its `backend_drm` is
            // compiled, which both our `tty` and `headless` features pull in
            // (gbm → drm). The headless backend exports no dmabuf, so its arm
            // resolves to `None` inside `ext_dmabuf_constraints`.
            #[cfg(any(feature = "tty", feature = "headless"))]
            dma: self.ext_dmabuf_constraints(),
        })
    }

    /// dmabuf format/modifier constraints for ext-image-copy-capture, grouped by
    /// fourcc. `None` on winit/headless (no dmabuf export) or before the dmabuf
    /// global is up. Offers only formats the primary GPU's renderer can actually
    /// import, opaque first (screencast streams are opaque).
    #[cfg(any(feature = "tty", feature = "headless"))]
    pub fn ext_dmabuf_constraints(&self) -> Option<crate::ext_screencopy::DmabufConstraints> {
        // The headless backend compiles smithay's backend_drm (via gbm) so the
        // `dma` constraint type exists, but it exports no dmabuf — so there are
        // no constraints to advertise.
        #[cfg(not(feature = "tty"))]
        {
            None
        }
        #[cfg(feature = "tty")]
        {
            use smithay::backend::allocator::Fourcc;
            let node = self.udev.as_ref()?.primary_render_node();
            let formats = self.dmabuf_formats.as_ref()?;
            let mut grouped = Vec::new();
            for code in [Fourcc::Xrgb8888, Fourcc::Argb8888] {
                let modifiers: Vec<_> = formats
                    .iter()
                    .filter(|f| f.code == code)
                    .map(|f| f.modifier)
                    .collect();
                if !modifiers.is_empty() {
                    grouped.push((code, modifiers));
                }
            }
            if grouped.is_empty() {
                return None;
            }
            Some(crate::ext_screencopy::DmabufConstraints {
                node,
                formats: grouped,
            })
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
        // A sticky window is shown on every workspace, so moving it to one
        // particular workspace is meaningless — ignore the request and keep
        // it where it is. The user can un-stick it first if they want it to
        // live on a single workspace again.
        if self.is_sticky(window) {
            tracing::debug!("move_window: window is sticky, ignoring move");
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
    /// Cached metrics name (`comm-pid`) for this client, resolved once on
    /// first surface creation. Lives here so the `/proc` lookup happens at
    /// most once per client, not per surface. See [`crate::metrics`].
    pub metrics_name: std::sync::OnceLock<String>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
