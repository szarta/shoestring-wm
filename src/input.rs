use std::time::Duration;

use shoestring_config::Action;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::Window,
    input::{
        keyboard::FilterResult,
        pointer::{
            AxisFrame, ButtonEvent, Focus, GrabStartData as PointerGrabStartData, MotionEvent,
            RelativeMotionEvent,
        },
    },
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        RegistrationToken,
    },
    utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER},
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
        }
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

    fn minimize_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            tracing::debug!("minimize: no focused window");
            return;
        };
        let loc = self.space.element_location(&window).unwrap_or_default();
        let size = window.geometry().size;
        let rect = Rectangle::new(loc, size);
        tracing::debug!(?loc, ?size, "minimize");

        self.space.unmap_elem(&window);
        // Drop the window from its workspace's MRU history so a workspace
        // switch doesn't try to focus a hidden surface. Workspace assignment
        // is dropped too — Unminimize will re-assign to whatever workspace is
        // active at restore time.
        self.workspaces.forget(&window);
        self.clear_focus();
        self.layout.push_minimized(window, rect);
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
    }

    fn close_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            return;
        };
        crate::window_ext::send_close(&window);
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

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
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
                let mut new = current + delta;
                if let Some(bounds) = workspace_bounds(&self.space) {
                    new.x = new
                        .x
                        .clamp(bounds.loc.x as f64, (bounds.loc.x + bounds.size.w) as f64);
                    new.y = new
                        .y
                        .clamp(bounds.loc.y as f64, (bounds.loc.y + bounds.size.h) as f64);
                }
                let under = self.surface_under(new);
                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location: new,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.relative_motion(
                    self,
                    under,
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: event.delta_unaccel(),
                        utime: event.time(),
                    },
                );
                pointer.frame(self);
                self.maybe_pointer_focus(new);
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
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
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
                        Some((window, _loc)) => self.focus_window(&window),
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
                        // Wayland axis convention: positive vertical is
                        // downward, so wheel up (negative) advances forward.
                        let delta = if vertical_amount < 0.0 { 1 } else { -1 };
                        let target = self.workspaces.shifted(self.workspaces.active(), delta);
                        if target != self.workspaces.active() {
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
            _ => {}
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
