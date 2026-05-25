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
    utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER},
};

use crate::{
    binds::ModMask,
    grabs::{MoveSurfaceGrab, ResizeEdge, ResizeSurfaceGrab},
    layout::{self, LayoutState},
    state::ShoestringWm,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl ShoestringWm {
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
                tracing::info!("Quit action received; stopping event loop");
                self.loop_signal.stop();
            }
            Action::ReloadConfig => {
                let Some(path) = self.config_path.clone() else {
                    tracing::warn!("ReloadConfig requested but no config file is loaded");
                    return;
                };
                match shoestring_config::load_from(&path) {
                    Ok(cfg) => {
                        let (table, warnings) = crate::binds::BindingTable::compile(&cfg);
                        for w in warnings {
                            tracing::warn!(target: "shoestring_wm::config", "{w}");
                        }
                        self.config = cfg;
                        self.bindings = table;
                        tracing::info!(path = %path.display(), "config reloaded");
                    }
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "reload failed"),
                }
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
                if let Some(ws) = crate::workspace::WorkspaceId::from_one_based(index) {
                    self.focus_workspace_id(ws);
                } else {
                    tracing::warn!(index, "FocusWorkspace index out of range 1..=16");
                }
            }
            Action::FocusWorkspaceRelative { delta } => {
                let target = self.workspaces.active().shifted(delta as i32);
                tracing::debug!(delta, target = target.one_based(), "FocusWorkspaceRelative");
                self.focus_workspace_id(target);
            }
            Action::MoveWindowToWorkspace { index } => {
                tracing::debug!(index, "MoveWindowToWorkspace");
                if let Some(ws) = crate::workspace::WorkspaceId::from_one_based(index) {
                    self.move_focused_to_workspace(ws);
                    self.focus_workspace_id(ws); // follow the window
                } else {
                    tracing::warn!(index, "MoveWindowToWorkspace index out of range 1..=16");
                }
            }
            Action::MoveWindowToWorkspaceRelative { delta } => {
                let target = self.workspaces.active().shifted(delta as i32);
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
                if let Err(e) = self.inject_key(&keysym) {
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

    fn window_layout_action(&mut self, target: LayoutState) {
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
        window.toplevel().unwrap().send_close();
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
            kb.set_focus(
                self,
                Some(window.toplevel().unwrap().wl_surface().clone()),
                serial,
            );
        }
        self.space.elements().for_each(|w| {
            w.set_activated(w == window);
            w.toplevel().unwrap().send_pending_configure();
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
            let grab = MoveSurfaceGrab {
                start_data,
                window: window.clone(),
                initial_window_location: window_loc,
            };
            pointer.set_grab(self, grab, serial, Focus::Clear);
        } else {
            let geometry = window.geometry();
            let initial_rect = Rectangle::new(window_loc, geometry.size);
            let edges = edges_for_pointer(initial_rect, pointer_pos);
            let grab = ResizeSurfaceGrab::start(start_data, window.clone(), edges, initial_rect);
            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();
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
                        // Suppress all binds while a session lock is up so
                        // the user can't Quit / VT-switch / Spawn out of the
                        // lock. Everything is forwarded to the lock surface
                        // (which currently holds keyboard focus).
                        if state.is_locked() {
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
                    self.dispatch_action(a);
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
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();

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
