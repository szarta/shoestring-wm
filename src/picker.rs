//! Interactive window picker — the WM-side machinery behind
//! [`Request::PickWindow`].
//!
//! A picker session is at-most-one: the WM stashes the IPC client whose
//! request started it in [`ShoestringWm::pending_picker`] and holds the
//! connection open. Pointer / keyboard input is intercepted in
//! [`crate::input`] while the picker is up:
//!
//! - left-click resolves to whichever toplevel `space.element_under` reports
//!   (or `None` if the click landed on a layer-shell surface or empty space),
//! - right-click and Escape cancel,
//! - other keys and the button release are swallowed so they don't reach the
//!   client beneath.
//!
//! Connection-drop is treated as cancel — see [`ShoestringWm::cancel_picker_if_owned_by`].

use std::{cell::RefCell, rc::Rc};

use shoestring_ipc::{Response, WindowSummary};
use smithay::{
    backend::input::{ButtonState, KeyState},
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

use crate::{
    ipc::{Client, ClientId},
    state::ShoestringWm,
};

/// Linux evdev `BTN_LEFT` code (also defined in [`crate::input`] /
/// [`crate::inject`]; kept local here so the shared resolver doesn't depend
/// on either module).
const BTN_LEFT: u32 = 0x110;
/// xkb `Escape` keysym — the key that cancels an armed picker.
const KEY_ESCAPE: u32 = 0xff1b;

pub struct PendingPicker {
    pub client_id: ClientId,
    pub(crate) client: Rc<RefCell<Client>>,
    /// Keyboard focus at the moment the picker armed. Restored when the
    /// picker resolves so the previously focused surface keeps receiving
    /// input — without this, any key the user was holding (notably the
    /// Enter that launched `shoestring-kill`) would auto-repeat in the
    /// surface forever because it never sees a release.
    pub saved_focus: Option<WlSurface>,
}

impl ShoestringWm {
    /// Begin a picker session. Returns `Err` if one is already in flight;
    /// the caller writes the error to its own client. The supplied client
    /// is held until [`Self::finish_picker`] (or a connection drop) resolves
    /// it.
    pub(crate) fn start_picker(
        &mut self,
        client_id: ClientId,
        client: Rc<RefCell<Client>>,
    ) -> Result<(), &'static str> {
        if self.pending_picker.is_some() {
            return Err("pick_window: another picker is already active");
        }
        if self.is_locked() {
            return Err("pick_window: refused while session is locked");
        }
        // Yank keyboard focus from whatever surface had it. Smithay sends
        // a `wl_keyboard.leave` to that surface, which clears its
        // held-key bookkeeping — without this step, a client-side
        // key-repeat timer (alacritty's, gtk's, etc.) keeps firing
        // because it never sees the release of the keystroke that
        // launched the picker.
        let saved_focus = self.seat.get_keyboard().and_then(|kb| {
            let focus = kb.current_focus();
            let serial = SERIAL_COUNTER.next_serial();
            kb.set_focus(self, Option::<WlSurface>::None, serial);
            focus
        });
        tracing::debug!(?client_id, "picker armed");
        self.pending_picker = Some(PendingPicker {
            client_id,
            client,
            saved_focus,
        });
        // Show the kill cursor right away rather than waiting for the first
        // pointer motion — `refresh_cursor_buffer` switches to it now that
        // `pending_picker` is set.
        self.kick_render_all();
        Ok(())
    }

    /// Resolve the picker with `result` (the window the user clicked, or
    /// `None` on cancel). Writes the deferred [`Response::PickedWindow`]
    /// and drops the client.
    pub fn finish_picker(&mut self, result: Option<WindowSummary>) {
        let Some(pending) = self.pending_picker.take() else {
            return;
        };
        tracing::debug!(
            ?pending.client_id,
            picked = result.is_some(),
            "picker resolved",
        );
        // Restore keyboard focus to whatever held it when the picker
        // armed (see `start_picker` for why we yank it). Skipped if a
        // session lock came up in the meantime — the lock surface owns
        // focus then and we shouldn't trample it.
        if !self.is_locked() {
            if let Some(kb) = self.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                kb.set_focus(self, pending.saved_focus, serial);
            }
        }
        self.restore_cursor_after_picker();
        let _ =
            crate::ipc::write_response(&pending.client, &Response::PickedWindow { window: result });
        self.drop_ipc_client(pending.client_id);
    }

    /// Picker-mode pointer-button handler shared by the real-input
    /// ([`crate::input`]) and injected ([`crate::inject`]) paths so the two
    /// can't drift. When a picker is armed, a press resolves it (left-click
    /// picks the toplevel under the pointer; any other button cancels) and a
    /// release is swallowed — either way the event must not reach the client
    /// beneath. Returns `true` when the picker consumed the event (caller
    /// returns without delivering it), `false` when no picker is armed.
    pub(crate) fn picker_handle_button(&mut self, button: u32, state: ButtonState) -> bool {
        if self.pending_picker.is_none() {
            return false;
        }
        if state == ButtonState::Pressed {
            let pos = self
                .seat
                .get_pointer()
                .expect("seat must have pointer")
                .current_location();
            let target = if button == BTN_LEFT {
                self.space.element_under(pos).map(|(w, _)| w.clone())
            } else {
                None
            };
            let summary = target.as_ref().and_then(|w| self.picker_summary(w));
            self.finish_picker(summary);
        }
        true
    }

    /// Picker-mode key decision shared by the real-input and injected key
    /// paths. Escape on press cancels the armed picker; every other key is a
    /// no-op (the caller swallows the key regardless so it never reaches the
    /// focused surface). `sym` is the resolved keysym value. No-op when no
    /// picker is armed.
    pub(crate) fn picker_resolve_key(&mut self, sym: u32, state: KeyState) {
        if self.pending_picker.is_none() {
            return;
        }
        if state == KeyState::Pressed && sym == KEY_ESCAPE {
            self.finish_picker(None);
        }
    }

    /// If the picker is owned by `client_id`, drop it without writing a
    /// reply (the connection is already gone). Called from the IPC layer
    /// when an in-flight client disconnects.
    pub fn cancel_picker_if_owned_by(&mut self, client_id: ClientId) {
        let owns = self
            .pending_picker
            .as_ref()
            .map(|p| p.client_id == client_id)
            .unwrap_or(false);
        if !owns {
            return;
        }
        tracing::debug!(?client_id, "picker cancelled by client disconnect");
        let pending = self.pending_picker.take().unwrap();
        if !self.is_locked() {
            if let Some(kb) = self.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                kb.set_focus(self, pending.saved_focus, serial);
            }
        }
        self.restore_cursor_after_picker();
    }

    /// Resync the pointer element's render status from the live
    /// [`cursor_status`](ShoestringWm::cursor_status) after a picker resolves,
    /// undoing the kill-cursor override [`refresh_cursor_buffer`] applied while
    /// the picker was armed. The next render then shows the client's real
    /// cursor again; a render is kicked so the revert is immediate even when no
    /// pointer motion follows.
    fn restore_cursor_after_picker(&mut self) {
        self.pointer_element.set_status(self.cursor_status.clone());
        self.kick_render_all();
    }

    /// Build a [`WindowSummary`] for `window` using the same data the
    /// `Windows` IPC reports. Returns `None` if the window has no FT
    /// handle (shouldn't happen for live toplevels, but the picker
    /// shouldn't panic on a race).
    pub fn picker_summary(&self, window: &smithay::desktop::Window) -> Option<WindowSummary> {
        let handle = self.foreign_toplevels.get(window)?;
        let surface = window.toplevel()?.wl_surface().clone();
        let (title, app_id) = read_title_app_id(&surface);
        let workspace = self
            .workspaces
            .windows_on_any()
            .find(|(w, _)| w == window)
            .map(|(_, ws)| ws.one_based())
            .unwrap_or(0);
        Some(WindowSummary {
            id: handle.identifier(),
            title,
            app_id,
            workspace,
            focused: self.focused_window().as_ref() == Some(window),
            geometry: crate::ipc::window_geometry(self, window),
            z: crate::ipc::window_z(self, window),
            sticky: self.is_sticky(window),
            always_on_top: self.is_always_on_top(window),
            pid: crate::ipc::window_pid(self, window),
        })
    }

    /// Close the toplevel whose FT identifier matches `id`. Sends
    /// `xdg_toplevel.close` (a polite request — the client can show a
    /// save-prompt instead of dying).
    pub fn close_window_by_id(&self, id: &str) -> Result<(), String> {
        for (window, handle) in &self.foreign_toplevels {
            if handle.identifier() == id {
                crate::window_ext::send_close(window);
                return Ok(());
            }
        }
        Err(format!("no window with id {id:?}"))
    }

    /// Focus the toplevel whose FT identifier matches `id`. Restores it
    /// from the minimized stack if needed, switches to its workspace
    /// if it lives elsewhere, then runs the same focus/raise/activate
    /// path as a click.
    pub fn focus_window_by_id(&mut self, id: &str) -> Result<(), String> {
        let window = self
            .foreign_toplevels
            .iter()
            .find(|(_, h)| h.identifier() == id)
            .map(|(w, _)| w.clone())
            .ok_or_else(|| format!("no window with id {id:?}"))?;
        self.activate_window(&window);
        Ok(())
    }

    /// Raise the toplevel whose FT identifier matches `id` to the top of the
    /// stacking order, without changing keyboard focus or switching workspace
    /// (a pure restack). No visible effect when the window is minimized or on
    /// a non-active workspace — those aren't in the mapped stack — but the
    /// lookup still succeeds, so it's not an error.
    pub fn raise_window_by_id(&mut self, id: &str) -> Result<(), String> {
        let window = self.window_by_ft_id(id)?;
        self.raise_window(&window);
        Ok(())
    }

    /// Lower the toplevel whose FT identifier matches `id` to the bottom of
    /// the stacking order. The complement of [`Self::raise_window_by_id`].
    pub fn lower_window_by_id(&mut self, id: &str) -> Result<(), String> {
        let window = self.window_by_ft_id(id)?;
        self.lower_window(&window);
        Ok(())
    }

    /// Set or clear the sticky flag (show on all workspaces) on the toplevel
    /// whose FT identifier matches `id`. Broadcasts `window_sticky_changed`.
    pub fn set_window_sticky_by_id(&mut self, id: &str, sticky: bool) -> Result<(), String> {
        let window = self.window_by_ft_id(id)?;
        self.set_sticky(&window, sticky);
        Ok(())
    }

    /// Set or clear the always-on-top flag on the toplevel whose FT
    /// identifier matches `id`. Broadcasts `window_always_on_top_changed`.
    pub fn set_window_always_on_top_by_id(&mut self, id: &str, on: bool) -> Result<(), String> {
        let window = self.window_by_ft_id(id)?;
        self.set_always_on_top(&window, on);
        Ok(())
    }

    /// Look up a tracked window by its `ext-foreign-toplevel-list-v1`
    /// identifier. Shared by the by-id IPC entry points.
    fn window_by_ft_id(&self, id: &str) -> Result<Window, String> {
        self.foreign_toplevels
            .iter()
            .find(|(_, h)| h.identifier() == id)
            .map(|(w, _)| w.clone())
            .ok_or_else(|| format!("no window with id {id:?}"))
    }

    /// Set (or, with an empty `name`, clear) the display-name override for the
    /// toplevel whose FT identifier matches `id`. The override supersedes the
    /// client's `xdg_toplevel` title everywhere the WM reports a title (see
    /// [`crate::ipc::effective_title_app_id`]). Emits a `window_title_changed`
    /// event carrying the new effective title so bars/menus update live.
    pub fn set_window_name_by_id(&mut self, id: &str, name: &str) -> Result<(), String> {
        let (window, identifier) = self
            .foreign_toplevels
            .iter()
            .find(|(_, h)| h.identifier() == id)
            .map(|(w, h)| (w.clone(), h.identifier()))
            .ok_or_else(|| format!("no window with id {id:?}"))?;
        if name.is_empty() {
            self.window_name_overrides.remove(&window);
        } else {
            self.window_name_overrides
                .insert(window.clone(), name.to_string());
        }
        let (title, app_id) = crate::ipc::effective_title_app_id(self, &window);
        self.emit_ipc(shoestring_ipc::Event::WindowTitleChanged {
            id: identifier,
            title,
            app_id,
        });
        Ok(())
    }

    /// Bring `window` to the foreground: restore it from the minimized stack if
    /// needed, switch to its workspace if it lives elsewhere, then run the same
    /// focus/raise/activate path as a click. Shared by the IPC `focus_window`
    /// (by FT id) and the foreign-toplevel-management `activate` request.
    pub fn activate_window(&mut self, window: &Window) {
        // Minimized → re-map at its saved rect on the active workspace
        // before doing anything else, so the workspace-switch and
        // focus paths see it as a live element.
        if let Some(rect) = self.layout.take_minimized(window) {
            let active = self.workspaces.active();
            self.space.map_element(window.clone(), rect.loc, true);
            self.workspaces.assign(window.clone(), active, rect.loc);
        }

        // If the window lives on another workspace, switch first.
        let target_ws = self
            .workspaces
            .windows_on_any()
            .find(|(w, _)| w == window)
            .map(|(_, ws)| ws);
        if let Some(ws) = target_ws {
            if ws != self.workspaces.active() {
                self.focus_workspace_id(ws);
            }
        }

        self.focus_window(window);
    }
}

fn read_title_app_id(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> (String, String) {
    use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};
    with_states(surface, |states| {
        let data = states.data_map.get::<XdgToplevelSurfaceData>();
        let Some(data) = data else {
            return (String::new(), String::new());
        };
        let g = data.lock().unwrap();
        (
            g.title.clone().unwrap_or_default(),
            g.app_id.clone().unwrap_or_default(),
        )
    })
}
