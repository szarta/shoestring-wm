use std::time::Duration;

use shoestring_config::Action;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device, DeviceCapability, Event,
        InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
        PointerMotionEvent, ProximityState, TabletToolButtonEvent, TabletToolEvent,
        TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState,
    },
    desktop::Window,
    input::{
        keyboard::FilterResult,
        pointer::{
            AxisFrame, ButtonEvent, Focus, GrabStartData as PointerGrabStartData, MotionEvent,
            RelativeMotionEvent,
        },
    },
    output::Output,
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            RegistrationToken,
        },
        wayland_server::protocol::wl_output::WlOutput,
    },
    utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER},
    wayland::{
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        tablet_manager::{TabletDescriptor, TabletSeatTrait},
    },
};

use crate::{
    binds::ModMask,
    grabs::{MoveSurfaceGrab, ResizeEdge, ResizeSurfaceGrab},
    layout::{self, LayoutState},
    state::ShoestringWm,
};

const BTN_LEFT: u32 = 0x110;

/// How long a repeatable keybind must be held before auto-repeat
/// begins. Matches the typical OS key-repeat delay so the gesture
/// feels native.
const KEY_REPEAT_DELAY: Duration = Duration::from_millis(600);
/// Interval between repeat firings once the initial delay has
/// elapsed. Tuned for workspace navigation (Super+H/L) — fast enough
/// to walk through 16 workspaces without feeling pokey, slow enough
/// that the user can stop on the right one.
const KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(250);

/// Currently-held repeatable keybind. At most one is tracked at a
/// time — pressing a different bound key replaces it, releasing the
/// tracked keycode clears it.
pub struct KeyRepeat {
    /// Physical keycode (as delivered by libinput / winit). Used to
    /// match the release event that ends the repeat.
    pub keycode: u32,
    /// Action to re-dispatch on each tick. Cloned per fire so the
    /// dispatch path can take it by value.
    pub action: Action,
    /// Calloop timer token; removed when the repeat ends.
    pub token: RegistrationToken,
}

/// Actions worth auto-repeating while held. Spawning a terminal 10x
/// because the user hesitated would be hostile; relative workspace
/// navigation is the canonical "I want to keep going" gesture.
fn is_repeatable(action: &Action) -> bool {
    matches!(
        action,
        Action::FocusWorkspaceRelative { .. } | Action::MoveWindowToWorkspaceRelative { .. }
    )
}
const BTN_RIGHT: u32 = 0x111;

impl ShoestringWm {
    /// Replace any current key-repeat with a fresh one for `keycode` /
    /// `action`. Cancels any existing repeat first — at most one
    /// keybind repeats at a time.
    pub fn start_key_repeat(&mut self, keycode: u32, action: Action) {
        self.cancel_key_repeat();
        let timer = Timer::from_duration(KEY_REPEAT_DELAY);
        let action_for_timer = action.clone();
        match self.loop_handle.insert_source(timer, move |_, _, state| {
            state.tick_key_repeat();
            TimeoutAction::ToDuration(KEY_REPEAT_INTERVAL)
        }) {
            Ok(token) => {
                self.key_repeat = Some(KeyRepeat {
                    keycode,
                    action: action_for_timer,
                    token,
                });
            }
            Err(e) => tracing::warn!(error = %e, "insert key-repeat timer failed"),
        }
    }

    /// Re-dispatch the held action. Called on every repeat tick after
    /// the initial delay. Re-clones the action so the dispatch path
    /// can take it by value; the clone is cheap (workspace deltas are
    /// `Copy`-sized).
    fn tick_key_repeat(&mut self) {
        let Some(action) = self.key_repeat.as_ref().map(|kr| kr.action.clone()) else {
            return;
        };
        self.dispatch_action(action);
    }

    /// Tear down the current key-repeat. Idempotent.
    pub fn cancel_key_repeat(&mut self) {
        if let Some(kr) = self.key_repeat.take() {
            self.loop_handle.remove(kr.token);
        }
    }

    pub fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Spawn { command, args } => {
                let mut cmd = std::process::Command::new(&command);
                cmd.args(&args);
                if let Some(socket) = self.socket_name.to_str() {
                    cmd.env("WAYLAND_DISPLAY", socket);
                }
                match cmd.spawn() {
                    Ok(child) => tracing::info!(
                        pid = child.id(), %command, ?args, "spawned via binding"
                    ),
                    Err(e) => tracing::warn!(%command, error = %e, "spawn failed"),
                }
            }
            Action::Quit => {
                tracing::debug!("Quit action received; prompting for confirmation");
                self.confirm_action("Exit shoestring-wm?", |state| {
                    tracing::info!("Quit confirmed; stopping event loop");
                    state.loop_signal.stop();
                });
            }
            Action::ReloadConfig => {
                let _ = self.reload_config_from_disk();
            }
            Action::TileLeft => self.window_layout_action(LayoutState::TiledLeft),
            Action::TileRight => self.window_layout_action(LayoutState::TiledRight),
            Action::Maximize => self.window_layout_action(LayoutState::Maximized),
            Action::Minimize => {
                // Toggle: if something has focus, hide it; if nothing does,
                // pop the last-hidden window back. Makes Super+D feel like
                // a single show/hide key instead of needing Super+Shift+D
                // to restore.
                if self.focused_window().is_some() {
                    self.minimize_focused();
                } else {
                    self.unminimize_last();
                }
            }
            Action::Unminimize => self.unminimize_last(),
            Action::Close => self.close_focused(),
            Action::CycleWindows => self.cycle_windows(),
            Action::Raise => self.restack_focused(true),
            Action::Lower => self.restack_focused(false),
            Action::ToggleSticky => self.toggle_sticky_focused(),
            Action::ToggleAlwaysOnTop => self.toggle_always_on_top_focused(),
            Action::FocusWorkspace { index } => {
                tracing::debug!(index, "FocusWorkspace");
                if let Some(ws) = self.workspaces.from_one_based(index) {
                    self.focus_workspace_id(ws);
                } else {
                    tracing::warn!(
                        index,
                        count = self.workspaces.count(),
                        "FocusWorkspace index out of range"
                    );
                }
            }
            Action::FocusWorkspaceRelative { delta } => {
                let target = self
                    .workspaces
                    .shifted(self.workspaces.active(), delta as i32);
                tracing::debug!(delta, target = target.one_based(), "FocusWorkspaceRelative");
                self.focus_workspace_id(target);
            }
            Action::MoveWindowToWorkspace { index } => {
                tracing::debug!(index, "MoveWindowToWorkspace");
                if let Some(ws) = self.workspaces.from_one_based(index) {
                    self.move_focused_to_workspace(ws);
                    self.focus_workspace_id(ws); // follow the window
                } else {
                    tracing::warn!(
                        index,
                        count = self.workspaces.count(),
                        "MoveWindowToWorkspace index out of range"
                    );
                }
            }
            Action::MoveWindowToWorkspaceRelative { delta } => {
                let target = self
                    .workspaces
                    .shifted(self.workspaces.active(), delta as i32);
                tracing::debug!(
                    delta,
                    target = target.one_based(),
                    "MoveWindowToWorkspaceRelative"
                );
                self.move_focused_to_workspace(target);
                self.focus_workspace_id(target); // follow the window
            }
            Action::ChangeVt { vt } => self.change_vt(vt),
            Action::InjectKey { keysym } => {
                if let Err(e) = self.inject_key(&keysym, &[]) {
                    tracing::warn!(keysym, error = %e, "inject_key failed");
                }
            }
            Action::InjectText { text } => {
                if let Err(e) = self.inject_text(&text) {
                    tracing::warn!(text, error = %e, "inject_text failed");
                }
            }
            Action::InjectClick { button } => {
                if let Err(e) = self.inject_click(&button, None) {
                    tracing::warn!(button, error = %e, "inject_click failed");
                }
            }
            Action::Lock => self.spawn_lock(),
            Action::CycleLayout => self.cycle_keyboard_layout(),
            Action::ToggleAudioMute => self.spawn_mediad(&["audio-mute", "toggle"]),
            Action::ToggleMicMute => self.spawn_mediad(&["mic-mute", "toggle"]),
        }
    }

    /// Advance the keyboard to the next XKB layout (Super+Space by default),
    /// wrapping at the end. A no-op when only one layout is configured. This
    /// only changes the active layout index — no keymap recompile — and
    /// smithay broadcasts the new state to the focused client automatically.
    fn cycle_keyboard_layout(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let name = keyboard.with_xkb_state(self, |mut ctx| {
            ctx.cycle_next_layout();
            let xkb = ctx.xkb().lock().unwrap();
            xkb.layout_name(xkb.active_layout()).to_string()
        });
        tracing::info!(layout = %name, "keyboard layout switched");
    }

    /// Spawn the configured `lock_command`. The child binds
    /// ext-session-lock-v1 itself; the WM doesn't wait for it. No-op
    /// when already locked (a second locker would just get `finished`
    /// from the manager filter anyway, but skipping the spawn keeps the
    /// process tree clean).
    pub fn spawn_lock(&mut self) {
        if self.is_locked() {
            tracing::debug!("lock: already locked, skipping spawn");
            return;
        }
        let cmd_line = self.config.general.lock_command.clone();
        let mut parts = cmd_line.split_whitespace();
        let Some(program) = parts.next() else {
            tracing::warn!("lock: lock_command is empty");
            return;
        };
        let args: Vec<&str> = parts.collect();
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args);
        if let Some(socket) = self.socket_name.to_str() {
            cmd.env("WAYLAND_DISPLAY", socket);
        }
        match cmd.spawn() {
            Ok(child) => tracing::info!(pid = child.id(), %cmd_line, "spawned lock"),
            Err(e) => tracing::warn!(%cmd_line, error = %e, "spawn lock failed"),
        }
    }

    /// Spawn the `shoestring-mediad` helper as a fire-and-forget oneshot with
    /// the given subcommand (`["audio-mute", "on"]`, `["mic-mute", "toggle"]`,
    /// …). The WM never touches PipeWire itself — this delegates the actual
    /// mute to the pipewire-linked helper, and the new state flows back via
    /// `Request::ReportMedia` from the long-running monitor. A missing binary
    /// (no-PipeWire build) is just a logged warning, never fatal — mirrors the
    /// systemd-optional / graceful-degradation posture. Inherits the WM env
    /// (incl. `WAYLAND_DISPLAY`) so the helper finds the same session.
    pub fn spawn_mediad(&mut self, args: &[&str]) {
        let mut cmd = std::process::Command::new("shoestring-mediad");
        cmd.args(args);
        if let Some(socket) = self.socket_name.to_str() {
            cmd.env("WAYLAND_DISPLAY", socket);
        }
        match cmd.spawn() {
            Ok(child) => tracing::info!(pid = child.id(), ?args, "spawned mediad"),
            Err(e) => tracing::warn!(?args, error = %e, "spawn mediad failed"),
        }
    }

    fn change_vt(&mut self, vt: u8) {
        #[cfg(feature = "tty")]
        {
            use smithay::backend::session::Session;
            let Some(udev) = self.udev.as_mut() else {
                tracing::warn!(vt, "ChangeVt requested but not running on tty backend");
                return;
            };
            match udev.session.change_vt(vt as i32) {
                Ok(()) => tracing::info!(vt, "switched to VT"),
                Err(e) => tracing::warn!(vt, error = ?e, "change_vt failed"),
            }
        }
        #[cfg(not(feature = "tty"))]
        {
            tracing::warn!(vt, "ChangeVt requested but tty feature not compiled in");
        }
    }

    pub(crate) fn window_layout_action(&mut self, target: LayoutState) {
        let Some(window) = self.focused_window() else {
            tracing::debug!(?target, "no focused window for layout action");
            return;
        };
        layout::set_layout(&mut self.space, &mut self.layout, &window, target);
    }

    /// Put `window` into app-driven fullscreen (xdg `set_fullscreen`, X11, or
    /// foreign-toplevel-management). `output`, when the client names one,
    /// selects which monitor to cover; otherwise the window's current output is
    /// used. Unlike Maximize, fullscreen covers the whole output edge-to-edge,
    /// ignoring layer-shell exclusive zones (bars/docks).
    pub fn fullscreen_window(&mut self, window: &Window, output: Option<WlOutput>) {
        let geo = output
            .and_then(|wl| Output::from_resource(&wl))
            .and_then(|o| self.space.output_geometry(&o))
            .or_else(|| layout::output_rect_for(&self.space, window));
        let Some(geo) = geo else {
            tracing::debug!("fullscreen: no output geometry, ignoring request");
            return;
        };
        layout::set_fullscreen(&mut self.space, &mut self.layout, window, geo);
        crate::foreign_toplevel_mgmt::sync_wlr_toplevel(self, window);
    }

    /// Leave app-driven fullscreen, restoring the pre-fullscreen layout.
    pub fn unfullscreen_window(&mut self, window: &Window) {
        layout::unset_fullscreen(&mut self.space, &mut self.layout, window);
        crate::foreign_toplevel_mgmt::sync_wlr_toplevel(self, window);
    }

    fn minimize_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("minimize: no focused window");
            return;
        };
        self.minimize_window(&window);
    }

    /// Hide `window` regardless of focus. Used by the foreign-toplevel-management
    /// protocol (a taskbar minimizing an arbitrary window) and by
    /// [`Self::minimize_focused`].
    pub fn minimize_window(&mut self, window: &Window) {
        if self.layout.is_minimized(window) {
            return;
        }
        let loc = self.space.element_location(window).unwrap_or_default();
        let size = window.geometry().size;
        let rect = Rectangle::new(loc, size);
        tracing::debug!(?loc, ?size, "minimize");

        self.space.unmap_elem(window);
        // Drop the window from its workspace's MRU history so a workspace
        // switch doesn't try to focus a hidden surface. Workspace assignment
        // is dropped too — Unminimize will re-assign to whatever workspace is
        // active at restore time.
        self.workspaces.forget(window);
        // Only surrender keyboard focus if we're hiding the focused window;
        // minimizing a background window (via a taskbar) must not steal focus.
        if self.focused_window().as_ref() == Some(window) {
            self.clear_focus();
        }
        self.layout.push_minimized(window.clone(), rect);
        crate::foreign_toplevel_mgmt::sync_wlr_toplevel(self, window);
        // A minimized window is unmapped, so it no longer inhibits idle.
        self.refresh_idle_inhibit();
    }

    /// Restore a specific minimized `window` (taskbar click on its entry),
    /// as opposed to the LIFO [`Self::unminimize_last`]. No-op if it isn't
    /// on the minimized stack.
    pub fn unminimize_window(&mut self, window: &Window) {
        let Some(rect) = self.layout.take_minimized(window) else {
            return;
        };
        let active = self.workspaces.active();
        self.space.map_element(window.clone(), rect.loc, true);
        self.workspaces.assign(window.clone(), active, rect.loc);
        self.focus_window(window);
        crate::foreign_toplevel_mgmt::sync_wlr_toplevel(self, window);
        // Restored into view — it may inhibit idle again.
        self.refresh_idle_inhibit();
    }

    fn unminimize_last(&mut self) {
        let Some((window, rect)) = self.layout.pop_live_minimized() else {
            tracing::debug!("unminimize: stack empty (or only dead windows)");
            return;
        };
        let active = self.workspaces.active();
        tracing::debug!(loc = ?rect.loc, ws = active.one_based(), "unminimize");
        self.space.map_element(window.clone(), rect.loc, true);
        self.workspaces.assign(window.clone(), active, rect.loc);
        // focus_window records MRU on the active workspace.
        self.focus_window(&window);
        // Restored into view — it may inhibit idle again.
        self.refresh_idle_inhibit();
    }

    fn close_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        crate::window_ext::send_close(&window);
    }

    /// Cycle keyboard focus to the next window on the active workspace,
    /// raising it (the Alt+Tab switcher). The bottom-most window in the
    /// stacking order — the one raised least recently — is focused and
    /// rises to the top, so repeated presses round-robin through every
    /// window and land back where they started. A no-op with fewer than
    /// two windows.
    ///
    /// Everything mapped in the space belongs to the active workspace
    /// (workspace switches map/unmap at the boundary) and minimized
    /// windows are unmapped, so the space elements are exactly the cycle
    /// candidates.
    fn cycle_windows(&mut self) {
        if self.space.elements().count() < 2 {
            return;
        }
        let focused = self.focused_window();
        // Bottom-to-top z-order: the first element that isn't already
        // focused is the least-recently-raised window — the next in the
        // round-robin. `focus_window` raises it back to the top.
        let next = self
            .space
            .elements()
            .find(|w| focused.as_ref() != Some(*w))
            .cloned();
        if let Some(next) = next {
            self.focus_window(&next);
        }
    }

    /// Raise (`up = true`) or lower (`up = false`) the focused window in the
    /// stacking order. Pure restack — keyboard focus stays put. A no-op when
    /// no window holds focus.
    fn restack_focused(&mut self, up: bool) {
        let Some(window) = self.focused_window() else {
            tracing::debug!(up, "restack: no focused window");
            return;
        };
        if up {
            self.raise_window(&window);
        } else {
            self.lower_window(&window);
        }
    }

    fn start_super_drag(
        &mut self,
        button: u32,
        window: &Window,
        pointer_pos: Point<f64, Logical>,
        serial: Serial,
    ) {
        let pointer = self.seat.get_pointer().unwrap();
        // Raise + focus so the dragged window comes to the front.
        self.space.raise_element(window, true);
        if let Some(kb) = self.seat.get_keyboard() {
            if let Some(surface) = crate::window_ext::focus_surface(window) {
                kb.set_focus(self, Some(surface), serial);
            }
        }
        self.space.elements().for_each(|w| {
            w.set_activated(w == window);
            crate::window_ext::send_pending_configure(w);
        });

        let Some(window_loc) = self.space.element_location(window) else {
            return;
        };
        let start_data = PointerGrabStartData {
            focus: None,
            button,
            location: pointer_pos,
        };

        if button == BTN_LEFT {
            // Stash the dragged window so the edge-drag repeat timer
            // (see [`grabs::move_grab`]) can keep shifting it while
            // the pointer is pinned to a workspace boundary. Cleared
            // on grab unset.
            self.edge_drag_window = Some(window.clone());
            let grab = MoveSurfaceGrab::new(start_data, window.clone(), window_loc);
            pointer.set_grab(self, grab, serial, Focus::Clear);
        } else {
            let geometry = window.geometry();
            let initial_rect = Rectangle::new(window_loc, geometry.size);
            let edges = edges_for_pointer(initial_rect, pointer_pos);
            let grab = ResizeSurfaceGrab::start(start_data, window.clone(), edges, initial_rect);
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    /// Apply focus-follows-mouse / sloppy focus on a pointer motion.
    /// No-op in click-to-focus mode, while a pointer grab is active, while
    /// the session is locked, while the window picker is up, and when a
    /// layer-shell surface currently owns keyboard focus (menu, picker,
    /// lock surface — those are modal w.r.t. their own scope).
    ///
    /// `FollowsMouse` clears focus when the pointer is over empty space;
    /// `Sloppy` keeps the previous focus until the pointer enters another
    /// window. Neither variant raises — see [`focus_window_no_raise`].
    fn maybe_pointer_focus(&mut self, pos: Point<f64, Logical>) {
        use shoestring_config::FocusMode;
        let mode = self.config.general.focus_mode;
        if matches!(mode, FocusMode::ClickToFocus) {
            return;
        }
        if self.is_locked() || self.pending_picker.is_some() {
            return;
        }
        let pointer = self.seat.get_pointer().expect("seat must have pointer");
        if pointer.is_grabbed() {
            return;
        }
        // If keyboard focus is on a non-window surface (layer-shell), leave
        // it. focused_window() returns None while kb.current_focus() is Some
        // exactly in that case.
        if self.focused_window().is_none() {
            if let Some(kb) = self.seat.get_keyboard() {
                if kb.current_focus().is_some() {
                    return;
                }
            }
        }
        let current = self.focused_window();
        let window = self.space.element_under(pos).map(|(w, _)| w.clone());
        match (window, current) {
            (Some(w), Some(cur)) if w == cur => {}
            (Some(w), _) => self.focus_window_no_raise(&w),
            (None, _) => {
                if matches!(mode, FocusMode::FollowsMouse) {
                    self.clear_focus();
                }
            }
        }
    }

    /// Reset every idle-notification timer for the seat. No-op unless
    /// `ext_idle_notify_v1` is advertised (`general.idle_notifications_enabled`).
    /// The seat clone is `Arc`-cheap and sidesteps the simultaneous
    /// `&mut self` / `&self.seat` borrow.
    fn notify_idle_activity(&mut self) {
        if self.idle_notifier.is_none() {
            return;
        }
        let seat = self.seat.clone();
        self.idle_notifier.as_mut().unwrap().notify_activity(&seat);
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        // Any real input is "activity" for idle purposes; device hotplug is
        // not. Mirrors the conventional compositor behaviour (cf. niri).
        if !matches!(
            event,
            InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
        ) {
            self.notify_idle_activity();
        }
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();
                let keycode: u32 = event.key_code().into();
                // Releasing the held repeat keycode stops the repeat.
                // Done before the bind lookup so a chord that resolves
                // to the same key (unusual but possible) still ends
                // the prior repeat cleanly.
                if key_state == KeyState::Released {
                    if let Some(kr) = self.key_repeat.as_ref() {
                        if kr.keycode == keycode {
                            self.cancel_key_repeat();
                        }
                    }
                }
                // Picker mode: intercept everything before it reaches the
                // focused surface. Escape cancels; every other key is
                // swallowed so the user can't accidentally type into the
                // window they're about to kill.
                if self.pending_picker.is_some() {
                    let cancel = self.seat.get_keyboard().unwrap().input::<bool, _>(
                        self,
                        event.key_code(),
                        key_state,
                        serial,
                        time,
                        |_state, _mods, handle| {
                            if key_state != KeyState::Pressed {
                                return FilterResult::Intercept(false);
                            }
                            const KEY_ESCAPE: u32 = 0xff1b;
                            let sym = handle
                                .raw_latin_sym_or_raw_current_sym()
                                .map(|s| s.raw())
                                .unwrap_or(0);
                            FilterResult::Intercept(sym == KEY_ESCAPE)
                        },
                    );
                    if cancel == Some(true) {
                        self.finish_picker(None);
                    }
                    return;
                }
                let action = self.seat.get_keyboard().unwrap().input::<Action, _>(
                    self,
                    event.key_code(),
                    key_state,
                    serial,
                    time,
                    |state, mods, handle| {
                        if key_state != KeyState::Pressed {
                            return FilterResult::Forward;
                        }
                        // Suppress binds while a session lock is up so the
                        // user can't Quit / Spawn out of the lock — with one
                        // exception: VT switching stays live as an escape
                        // hatch, so a misbehaving locker can never hard-lock
                        // the machine. Switching VT doesn't weaken the lock
                        // (the Wayland session stays locked; switching back
                        // shows the locker still up). Everything else is
                        // forwarded to the lock surface, which holds focus.
                        if state.is_locked() {
                            if let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() {
                                let mask = ModMask::from_state(mods);
                                if let Some(a) = state.bindings.lookup(mask, sym.raw()) {
                                    if matches!(a, Action::ChangeVt { .. }) {
                                        return FilterResult::Intercept(a.clone());
                                    }
                                }
                            }
                            return FilterResult::Forward;
                        }
                        // Use the keysym *before* shift/caps are applied so that
                        // a binding registered as "q" matches Super+Shift+q
                        // (which would otherwise produce keysym Q).
                        let Some(sym) = handle.raw_latin_sym_or_raw_current_sym() else {
                            tracing::debug!("keypress: no raw_latin/current sym");
                            return FilterResult::Forward;
                        };
                        let mask = ModMask::from_state(mods);
                        let matched = state.bindings.lookup(mask, sym.raw());
                        tracing::debug!(
                            keysym = sym.raw(),
                            keysym_name = %smithay::input::keyboard::xkb::keysym_get_name(sym),
                            ?mask,
                            matched = matched.is_some(),
                            "keypress"
                        );
                        match matched {
                            Some(a) => FilterResult::Intercept(a.clone()),
                            None => FilterResult::Forward,
                        }
                    },
                );
                if let Some(a) = action {
                    let repeatable = is_repeatable(&a);
                    self.dispatch_action(a.clone());
                    if repeatable {
                        self.start_key_repeat(keycode, a);
                    } else {
                        // A non-repeatable bind cancels any prior
                        // held repeat — e.g. Super+H is repeating, the
                        // user mashes Super+Q. The Quit fires once and
                        // the workspace walk stops.
                        self.cancel_key_repeat();
                    }
                }
            }
            InputEvent::PointerMotion { event, .. } => {
                // libinput (TTY backend) sends relative deltas. Add to the
                // pointer's current location and clamp to the union of all
                // mapped outputs so the cursor can't fly off the workspace.
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                let delta = event.delta();
                let current = pointer.current_location();

                // A pointer constraint on the surface under the cursor changes
                // how this motion is applied: a *locked* pointer receives no
                // absolute motion at all (only the relative deltas below —
                // what FPS games and RDP/VNC clients want), while a *confined*
                // pointer is clamped to its region/surface. An inactive or
                // out-of-region constraint doesn't apply.
                let under = self.surface_under(current);
                let mut pointer_locked = false;
                let mut pointer_confined = false;
                let mut confine_region = None;
                if let Some((surface, surface_loc)) = under.as_ref() {
                    with_pointer_constraint(surface, &pointer, |constraint| {
                        let Some(constraint) = constraint else {
                            return;
                        };
                        if !constraint.is_active() {
                            return;
                        }
                        // A region-scoped constraint only bites while the
                        // cursor is inside that surface-local region.
                        if !constraint
                            .region()
                            .is_none_or(|r| r.contains((current - *surface_loc).to_i32_round()))
                        {
                            return;
                        }
                        match &*constraint {
                            PointerConstraint::Locked(_) => pointer_locked = true,
                            PointerConstraint::Confined(c) => {
                                pointer_confined = true;
                                confine_region = c.region().cloned();
                            }
                        }
                    });
                }

                // Relative motion fires regardless of locking — for a locked
                // pointer it is the only motion the client ever sees.
                pointer.relative_motion(
                    self,
                    under.clone(),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );

                // Locked: the cursor stays put. Flush the frame and stop —
                // no absolute motion, no focus-follow.
                if pointer_locked {
                    pointer.frame(self);
                    return;
                }

                let mut new = current + delta;
                if let Some(bounds) = workspace_bounds(&self.space) {
                    new.x = new
                        .x
                        .clamp(bounds.loc.x as f64, (bounds.loc.x + bounds.size.w) as f64);
                    new.y = new
                        .y
                        .clamp(bounds.loc.y as f64, (bounds.loc.y + bounds.size.h) as f64);
                }

                let new_under = self.surface_under(new);

                // Confined: reject any motion that would leave the constrained
                // surface or step outside its confine region.
                if pointer_confined {
                    if let Some((surface, surface_loc)) = under.as_ref() {
                        if new_under.as_ref().map(|(s, _)| s) != Some(surface) {
                            pointer.frame(self);
                            return;
                        }
                        if let Some(region) = &confine_region {
                            if !region.contains((new - *surface_loc).to_i32_round()) {
                                pointer.frame(self);
                                return;
                            }
                        }
                    }
                }

                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location: new,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                // Record what the client just saw so the post-dispatch
                // refresh pass (see `refresh_pointer_focus`) doesn't re-send
                // this same focus as a phantom move.
                self.last_pointer_focus = new_under.clone();
                self.maybe_pointer_focus(new);

                // Activate a constraint once the cursor enters its region (the
                // client armed it earlier, but it only takes hold in-region).
                // A constraint created over the current location is activated
                // directly in `new_constraint`.
                if let Some((surface, surface_loc)) = new_under.as_ref() {
                    with_pointer_constraint(surface, &pointer, |constraint| {
                        if let Some(constraint) = constraint {
                            if !constraint.is_active()
                                && constraint
                                    .region()
                                    .is_none_or(|r| r.contains((new - *surface_loc).to_i32_round()))
                            {
                                constraint.activate();
                            }
                        }
                    });
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                let under = self.surface_under(pos);
                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                self.last_pointer_focus = under;
                self.maybe_pointer_focus(pos);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();

                // Picker mode: a press resolves (left) or cancels (right /
                // any other button). Releases are swallowed. The click is
                // never delivered to the surface beneath, so the window
                // about to be closed doesn't see a phantom click.
                if self.pending_picker.is_some() {
                    if button_state == ButtonState::Pressed {
                        let pos = pointer.current_location();
                        let target = if button == BTN_LEFT {
                            self.space.element_under(pos).map(|(w, _)| w.clone())
                        } else {
                            None
                        };
                        let summary = target.as_ref().and_then(|w| self.picker_summary(w));
                        self.finish_picker(summary);
                    }
                    return;
                }

                if ButtonState::Pressed == button_state
                    && !pointer.is_grabbed()
                    && !self.is_locked()
                {
                    let pos = pointer.current_location();
                    let target = self.space.element_under(pos).map(|(w, l)| (w.clone(), l));
                    let mods = keyboard.modifier_state();

                    if let Some((window, _)) = target
                        .as_ref()
                        .filter(|_| mods.logo && (button == BTN_LEFT || button == BTN_RIGHT))
                    {
                        let window = window.clone();
                        self.start_super_drag(button, &window, pos, serial);
                        // Don't deliver the press to the client — the grab owns the gesture.
                        return;
                    }

                    match target {
                        // A window geometrically under the pointer is only the
                        // real target if no Top/Overlay layer surface covers it.
                        // Clicking a layer surface (tray menu, picker) that sits
                        // above a window must not transfer focus to that window —
                        // doing so yanked the menu's keyboard focus and made it
                        // dismiss on its own clicks.
                        Some((window, _loc)) if !self.overlay_layer_under(pos) => {
                            self.focus_window(&window)
                        }
                        Some(_) => {}
                        None => {
                            // Only clear keyboard focus if nothing at all is
                            // under the pointer. A layer-shell surface (e.g.
                            // shoestring-region picker, menu) owns its own
                            // focus via the layer_shell commit handler;
                            // clicking it must not yank that focus away.
                            if self.surface_under(pos).is_none() {
                                self.clear_focus();
                            }
                        }
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_discrete = event.amount_v120(Axis::Vertical);

                // Desktop scroll: a mouse wheel over the bare desktop — no
                // window or layer-shell surface under the pointer — switches
                // workspaces (wheel up to the next, down to the previous),
                // the way Openbox scrolls the root window. Restricted to real
                // wheels so touchpad scrolling isn't hijacked, and skipped
                // mid-grab, mid-pick, or while locked. The event is then
                // consumed (there is no client under the pointer to deliver
                // it to anyway).
                if source == AxisSource::Wheel
                    && vertical_amount != 0.0
                    && self.pending_picker.is_none()
                    && !self.is_locked()
                {
                    let pointer = self.seat.get_pointer().unwrap();
                    if !pointer.is_grabbed()
                        && self.surface_under(pointer.current_location()).is_none()
                    {
                        // Accumulate wheel travel in v120 units (120 per
                        // physical detent) and only step once a full
                        // `desktop_scroll_notches` worth has built up. This
                        // collapses the several sub-detent events a high-res
                        // wheel emits per notch into one switch, and lets the
                        // user dial in a slower feel. Fall back to scaling the
                        // continuous amount if the backend gave no v120.
                        let v120 = vertical_discrete
                            .filter(|d| *d != 0.0)
                            .unwrap_or(vertical_amount * 120.0 / 15.0);
                        // Reset on direction reversal so a change of direction
                        // starts fresh rather than first cancelling leftover
                        // travel from the prior gesture.
                        if self.desktop_scroll_accum != 0.0
                            && (v120 > 0.0) != (self.desktop_scroll_accum > 0.0)
                        {
                            self.desktop_scroll_accum = 0.0;
                        }
                        self.desktop_scroll_accum += v120;

                        let threshold =
                            self.config.general.desktop_scroll_notches.max(1) as f64 * 120.0;
                        // Wayland axis convention: positive vertical is
                        // downward, so wheel up (negative accum) advances
                        // forward (delta +1), wheel down steps back (-1).
                        while self.desktop_scroll_accum.abs() >= threshold {
                            let delta = if self.desktop_scroll_accum < 0.0 {
                                self.desktop_scroll_accum += threshold;
                                1
                            } else {
                                self.desktop_scroll_accum -= threshold;
                                -1
                            };
                            let target = self.workspaces.shifted(self.workspaces.active(), delta);
                            if target == self.workspaces.active() {
                                // Clamped at an end — drop leftover travel so
                                // it doesn't spill into the next gesture.
                                self.desktop_scroll_accum = 0.0;
                                break;
                            }
                            tracing::debug!(
                                delta,
                                target = target.one_based(),
                                "desktop scroll → workspace"
                            );
                            self.focus_workspace_id(target);
                        }
                        return;
                    }
                }

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }
                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            // Graphics tablets (Wacom et al.) over libinput. A tablet must be
            // registered on the seat's tablet-seat before its tool events mean
            // anything, so add/remove track device hotplug.
            InputEvent::DeviceAdded { device }
                if device.has_capability(DeviceCapability::TabletTool) =>
            {
                self.seat
                    .tablet_seat()
                    .add_tablet::<Self>(&self.display_handle, &TabletDescriptor::from(&device));
            }
            InputEvent::DeviceRemoved { device }
                if device.has_capability(DeviceCapability::TabletTool) =>
            {
                let tablet_seat = self.seat.tablet_seat();
                tablet_seat.remove_tablet(&TabletDescriptor::from(&device));
                // With no tablets left, the tools they spawned are stale.
                if tablet_seat.count_tablets() == 0 {
                    tablet_seat.clear_tools();
                }
            }
            InputEvent::TabletToolAxis { event, .. } => self.on_tablet_tool_axis::<I>(&event),
            InputEvent::TabletToolProximity { event, .. } => {
                self.on_tablet_tool_proximity::<I>(&event)
            }
            InputEvent::TabletToolTip { event, .. } => self.on_tablet_tool_tip::<I>(&event),
            InputEvent::TabletToolButton { event, .. } => self.on_tablet_tool_button::<I>(&event),
            _ => {}
        }
    }

    /// Map a tablet tool's absolute position onto the desktop. Tablets report
    /// normalized coordinates; we project them onto the first output the same
    /// way `PointerMotionAbsolute` does, so the stylus and the mouse share one
    /// coordinate space.
    fn tablet_tool_position<I: InputBackend, E: AbsolutePositionEvent<I>>(
        &self,
        evt: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self.space.outputs().next()?;
        let output_geo = self.space.output_geometry(output)?;
        Some(evt.position_transformed(output_geo.size) + output_geo.loc.to_f64())
    }

    /// Stylus moved (and possibly changed pressure/tilt/etc.). Drives the
    /// pointer so the cursor tracks the tool, then forwards the high-resolution
    /// axes to the focused tool resource.
    fn on_tablet_tool_axis<I: InputBackend>(&mut self, evt: &I::TabletToolAxisEvent) {
        let Some(pos) = self.tablet_tool_position::<I, _>(evt) else {
            return;
        };
        let tablet_seat = self.seat.tablet_seat();
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&evt.device()));
        let tool = tablet_seat.get_tool(&evt.tool());

        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);

        if let (Some(tablet), Some(tool)) = (tablet, tool) {
            if evt.pressure_has_changed() {
                tool.pressure(evt.pressure());
            }
            if evt.distance_has_changed() {
                tool.distance(evt.distance());
            }
            if evt.tilt_has_changed() {
                tool.tilt(evt.tilt());
            }
            if evt.slider_has_changed() {
                tool.slider_position(evt.slider_position());
            }
            if evt.rotation_has_changed() {
                tool.rotation(evt.rotation());
            }
            if evt.wheel_has_changed() {
                tool.wheel(evt.wheel_delta(), evt.wheel_delta_discrete());
            }
            tool.motion(
                pos,
                under,
                &tablet,
                SERIAL_COUNTER.next_serial(),
                evt.time_msec(),
            );
        }
    }

    /// Stylus entered or left the tablet's hover range. Registers the tool on
    /// first sight and sends proximity in/out to whatever surface it is over.
    fn on_tablet_tool_proximity<I: InputBackend>(&mut self, evt: &I::TabletToolProximityEvent) {
        let Some(pos) = self.tablet_tool_position::<I, _>(evt) else {
            return;
        };
        let dh = self.display_handle.clone();
        let tablet_seat = self.seat.tablet_seat();
        let descriptor = evt.tool();
        tablet_seat.add_tool::<Self>(self, &dh, &descriptor);

        let tablet_seat = self.seat.tablet_seat();
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&evt.device()));
        let tool = tablet_seat.get_tool(&descriptor);

        let pointer = self.seat.get_pointer().unwrap();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: pos,
                serial: SERIAL_COUNTER.next_serial(),
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);

        if let (Some((surface, loc)), Some(tablet), Some(tool)) = (under, tablet, tool) {
            match evt.state() {
                ProximityState::In => tool.proximity_in(
                    pos,
                    (surface, loc),
                    &tablet,
                    SERIAL_COUNTER.next_serial(),
                    evt.time_msec(),
                ),
                ProximityState::Out => tool.proximity_out(evt.time_msec()),
            }
        }
    }

    /// Stylus touched down or lifted off. A tip-down behaves like a click: it
    /// moves keyboard focus to the window under the tool before reporting the
    /// contact to the client.
    fn on_tablet_tool_tip<I: InputBackend>(&mut self, evt: &I::TabletToolTipEvent) {
        let tool = self.seat.tablet_seat().get_tool(&evt.tool());
        let Some(tool) = tool else {
            return;
        };
        match evt.tip_state() {
            TabletToolTipState::Down => {
                let serial = SERIAL_COUNTER.next_serial();
                tool.tip_down(serial, evt.time_msec());
                if !self.is_locked() && self.pending_picker.is_none() {
                    let pos = self.seat.get_pointer().unwrap().current_location();
                    if let Some((window, _)) = self.space.element_under(pos) {
                        let window = window.clone();
                        self.focus_window(&window);
                    }
                }
            }
            TabletToolTipState::Up => tool.tip_up(evt.time_msec()),
        }
    }

    /// A button on the stylus barrel (or the tablet pad routed as a tool
    /// button) changed state; forward it verbatim to the tool's client.
    fn on_tablet_tool_button<I: InputBackend>(&mut self, evt: &I::TabletToolButtonEvent) {
        if let Some(tool) = self.seat.tablet_seat().get_tool(&evt.tool()) {
            tool.button(
                evt.button(),
                evt.button_state(),
                SERIAL_COUNTER.next_serial(),
                evt.time_msec(),
            );
        }
    }
}

/// Bounding rect that covers every mapped output. Used to clamp the cursor
/// so it can't be moved beyond the visible desktop.
fn workspace_bounds(space: &smithay::desktop::Space<Window>) -> Option<Rectangle<i32, Logical>> {
    let mut iter = space.outputs().filter_map(|o| space.output_geometry(o));
    let first = iter.next()?;
    Some(iter.fold(first, |acc, r| {
        let x0 = acc.loc.x.min(r.loc.x);
        let y0 = acc.loc.y.min(r.loc.y);
        let x1 = (acc.loc.x + acc.size.w).max(r.loc.x + r.size.w);
        let y1 = (acc.loc.y + acc.size.h).max(r.loc.y + r.size.h);
        Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())
    }))
}

/// Pick which edges to resize from based on the pointer's quadrant within the
/// window. Center bands resize only on one axis; corners on both.
fn edges_for_pointer(rect: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> ResizeEdge {
    let rel_x = pos.x - rect.loc.x as f64;
    let rel_y = pos.y - rect.loc.y as f64;
    let w = rect.size.w.max(1) as f64;
    let h = rect.size.h.max(1) as f64;

    let mut edges = ResizeEdge::empty();
    if rel_x < w / 3.0 {
        edges |= ResizeEdge::LEFT;
    } else if rel_x > w * 2.0 / 3.0 {
        edges |= ResizeEdge::RIGHT;
    }
    if rel_y < h / 3.0 {
        edges |= ResizeEdge::TOP;
    } else if rel_y > h * 2.0 / 3.0 {
        edges |= ResizeEdge::BOTTOM;
    }
    if edges.is_empty() {
        // Pointer in dead center — default to bottom-right (common UX).
        edges = ResizeEdge::BOTTOM_RIGHT;
    }
    edges
}
