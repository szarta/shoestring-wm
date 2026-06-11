//! wlr-foreign-toplevel-management state and broadcast helpers.
//!
//! Smithay ships no delegate for `zwlr_foreign_toplevel_management_v1`, so we
//! hand-roll the server side, following the same pattern as
//! [`crate::output_management`] / [`crate::screencopy`]. The dispatch impls live
//! in [`crate::handlers::foreign_toplevel_mgmt`]; this module owns the state and
//! the event-broadcast logic.
//!
//! This is the *writable* sibling of the read-only `ext-foreign-toplevel-list`
//! ([`crate::state::ShoestringWm::foreign_toplevels`]): waybar-style taskbars use
//! it to activate / close / minimize / maximize windows and to read each
//! window's state for display. The two are co-indexed by [`Window`] but have
//! distinct lifecycles and types — do not unify them.
//!
//! ## Lifecycle
//! - A client binds the manager global → gets a `toplevel` handle for *every*
//!   current window ([`announce_existing_to_manager`]).
//! - A window maps → a handle is created on *every* bound manager
//!   ([`announce_to_all_managers`]).
//! - A window's title/state/outputs change → [`sync_wlr_toplevel`] re-pushes the
//!   relevant events to all of that window's handles.
//! - A window closes → [`close_toplevel`] sends `closed` and drops the handles.
//!
//! Unlike the smithay-owned ext handle, dropping our own handle resource sends
//! nothing, so `closed` is emitted explicitly.

use std::collections::HashMap;

use smithay::{
    desktop::Window,
    output::Output,
    reexports::{
        wayland_protocols_wlr::foreign_toplevel::v1::server::{
            zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
            zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        },
        wayland_server::{backend::GlobalId, DisplayHandle, Resource},
    },
};

use crate::{layout::LayoutState, state::ShoestringWm};

// ── State ──────────────────────────────────────────────────────────────────────

pub struct ForeignToplevelMgmtState {
    /// Keeps the manager global advertised for the WM's lifetime.
    #[allow(dead_code)]
    pub manager_global: GlobalId,
    /// Every bound, not-yet-stopped manager. New windows are announced to each.
    pub managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// Per-window handle fan-out: one handle per (window × bound manager).
    pub handles: HashMap<Window, Vec<ZwlrForeignToplevelHandleV1>>,
    /// The outputs each window last reported, so [`sync_wlr_toplevel`] can emit
    /// `output_enter`/`output_leave` diffs rather than re-announcing every time.
    pub last_outputs: HashMap<Window, Vec<Output>>,
}

// ── User data attached to protocol objects ─────────────────────────────────────

/// Attached to each `zwlr_foreign_toplevel_manager_v1`. No per-manager state is
/// needed beyond membership in [`ForeignToplevelMgmtState::managers`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ForeignToplevelManagerData;

/// Attached to each `zwlr_foreign_toplevel_handle_v1`: the window it represents.
pub struct ForeignToplevelHandleData {
    pub window: Window,
}

// ── State-array encoding (pure; unit-tested) ───────────────────────────────────

/// Encode the active toplevel states as the `state` event's array payload:
/// native-endian `u32` codes, one per set flag, per wlroots convention. We never
/// emit `fullscreen` (the WM has no fullscreen concept).
pub fn state_array_bytes(maximized: bool, minimized: bool, activated: bool) -> Vec<u8> {
    use zwlr_foreign_toplevel_handle_v1::State;
    let mut v = Vec::new();
    if maximized {
        v.extend_from_slice(&(State::Maximized as u32).to_ne_bytes());
    }
    if minimized {
        v.extend_from_slice(&(State::Minimized as u32).to_ne_bytes());
    }
    if activated {
        v.extend_from_slice(&(State::Activated as u32).to_ne_bytes());
    }
    v
}

// ── Per-window info snapshot ────────────────────────────────────────────────────

struct WlrInfo {
    title: String,
    app_id: String,
    outputs: Vec<Output>,
    state_bytes: Vec<u8>,
}

fn window_info(state: &ShoestringWm, window: &Window) -> WlrInfo {
    let (title, app_id) = crate::window_ext::window_title_app_id(window);
    let outputs = state.space.outputs_for_element(window);
    let maximized = state.layout.layout_state(window) == LayoutState::Maximized;
    let minimized = state.layout.is_minimized(window);
    let activated = state.focused_window().as_ref() == Some(window);
    WlrInfo {
        title,
        app_id,
        outputs,
        state_bytes: state_array_bytes(maximized, minimized, activated),
    }
}

// ── Handle creation ─────────────────────────────────────────────────────────────

/// Create a handle for `window` on `manager` and send the `toplevel` event that
/// hands it to the client. Mirrors [`crate::output_management::announce_head`].
fn create_handle(
    dh: &DisplayHandle,
    manager: &ZwlrForeignToplevelManagerV1,
    window: &Window,
) -> Option<ZwlrForeignToplevelHandleV1> {
    let client = dh.get_client(manager.id()).ok()?;
    let handle = client
        .create_resource::<ZwlrForeignToplevelHandleV1, _, ShoestringWm>(
            dh,
            manager.version(),
            ForeignToplevelHandleData {
                window: window.clone(),
            },
        )
        .ok()?;
    manager.toplevel(&handle);
    Some(handle)
}

/// Send the full initial event burst (`title`, `app_id`, all `output_enter`,
/// `state`, `done`) for a freshly-created handle.
fn send_initial(state: &ShoestringWm, window: &Window, handle: &ZwlrForeignToplevelHandleV1) {
    let info = window_info(state, window);
    handle.title(info.title);
    handle.app_id(info.app_id);
    if let Ok(client) = state.display_handle.get_client(handle.id()) {
        for output in &info.outputs {
            for wl in output.client_outputs(&client) {
                handle.output_enter(&wl);
            }
        }
    }
    handle.state(info.state_bytes);
    handle.done();
}

// ── Lifecycle entry points ──────────────────────────────────────────────────────

/// Announce every current window to a newly-bound manager.
pub fn announce_existing_to_manager(
    state: &mut ShoestringWm,
    manager: &ZwlrForeignToplevelManagerV1,
) {
    let dh = state.display_handle.clone();
    let windows: Vec<Window> = state.foreign_toplevels.keys().cloned().collect();
    for window in windows {
        let Some(handle) = create_handle(&dh, manager, &window) else {
            continue;
        };
        send_initial(state, &window, &handle);
        let outs = state.space.outputs_for_element(&window);
        state
            .foreign_toplevel_mgmt
            .handles
            .entry(window.clone())
            .or_default()
            .push(handle);
        state
            .foreign_toplevel_mgmt
            .last_outputs
            .insert(window, outs);
    }
}

/// Announce a newly-mapped window to every bound manager.
pub fn announce_to_all_managers(state: &mut ShoestringWm, window: &Window) {
    let dh = state.display_handle.clone();
    let managers: Vec<_> = state.foreign_toplevel_mgmt.managers.clone();
    let mut created = Vec::new();
    for manager in &managers {
        if !manager.is_alive() {
            continue;
        }
        if let Some(handle) = create_handle(&dh, manager, window) {
            send_initial(state, window, &handle);
            created.push(handle);
        }
    }
    if created.is_empty() {
        return;
    }
    let outs = state.space.outputs_for_element(window);
    state
        .foreign_toplevel_mgmt
        .handles
        .entry(window.clone())
        .or_default()
        .extend(created);
    state
        .foreign_toplevel_mgmt
        .last_outputs
        .insert(window.clone(), outs);
}

/// Re-push title/app_id, an `output_enter`/`output_leave` diff, the `state`
/// array, and `done` to every handle for `window`. Cheap no-op if the window has
/// no handles.
pub fn sync_wlr_toplevel(state: &mut ShoestringWm, window: &Window) {
    let handles = match state.foreign_toplevel_mgmt.handles.get(window) {
        Some(h) if !h.is_empty() => h.clone(),
        _ => return,
    };
    let info = window_info(state, window);
    let prev = state
        .foreign_toplevel_mgmt
        .last_outputs
        .get(window)
        .cloned()
        .unwrap_or_default();
    let dh = state.display_handle.clone();
    for handle in &handles {
        if !handle.is_alive() {
            continue;
        }
        handle.title(info.title.clone());
        handle.app_id(info.app_id.clone());
        if let Ok(client) = dh.get_client(handle.id()) {
            for out in &prev {
                if !info.outputs.iter().any(|o| o == out) {
                    for wl in out.client_outputs(&client) {
                        handle.output_leave(&wl);
                    }
                }
            }
            for out in &info.outputs {
                if !prev.iter().any(|o| o == out) {
                    for wl in out.client_outputs(&client) {
                        handle.output_enter(&wl);
                    }
                }
            }
        }
        handle.state(info.state_bytes.clone());
        handle.done();
    }
    state
        .foreign_toplevel_mgmt
        .last_outputs
        .insert(window.clone(), info.outputs);
}

/// Re-sync every tracked window. Used at coarse change points (focus moved,
/// workspace switched, output hotplug) where several windows shift at once.
pub fn broadcast_all(state: &mut ShoestringWm) {
    let windows: Vec<Window> = state
        .foreign_toplevel_mgmt
        .handles
        .keys()
        .cloned()
        .collect();
    for window in windows {
        sync_wlr_toplevel(state, &window);
    }
}

/// Send `closed` to every handle for `window` and drop its bookkeeping. The
/// handle resources stay alive (inert) until the client destroys them; the
/// handle `destroyed` callback tolerates the now-missing map entry.
pub fn close_toplevel(state: &mut ShoestringWm, window: &Window) {
    if let Some(handles) = state.foreign_toplevel_mgmt.handles.remove(window) {
        for handle in handles {
            if handle.is_alive() {
                handle.closed();
            }
        }
    }
    state.foreign_toplevel_mgmt.last_outputs.remove(window);
}

#[cfg(test)]
mod tests {
    use super::*;

    // State enum codes, per the protocol XML: maximized=0, minimized=1,
    // activated=2, fullscreen=3.
    fn le(code: u32) -> [u8; 4] {
        code.to_ne_bytes()
    }

    #[test]
    fn no_flags_is_empty() {
        assert!(state_array_bytes(false, false, false).is_empty());
    }

    #[test]
    fn each_flag_emits_its_code() {
        assert_eq!(state_array_bytes(true, false, false), le(0));
        assert_eq!(state_array_bytes(false, true, false), le(1));
        assert_eq!(state_array_bytes(false, false, true), le(2));
    }

    #[test]
    fn flags_concatenate_in_order() {
        let mut expected = Vec::new();
        expected.extend_from_slice(&le(0)); // maximized
        expected.extend_from_slice(&le(1)); // minimized
        expected.extend_from_slice(&le(2)); // activated
        assert_eq!(state_array_bytes(true, true, true), expected);
    }

    #[test]
    fn length_is_four_bytes_per_set_flag() {
        assert_eq!(state_array_bytes(true, false, true).len(), 8);
        assert_eq!(state_array_bytes(true, true, true).len(), 12);
    }

    #[test]
    fn fullscreen_code_never_appears() {
        // No combination of our three flags should ever encode 3 (fullscreen).
        for &m in &[false, true] {
            for &mi in &[false, true] {
                for &a in &[false, true] {
                    let bytes = state_array_bytes(m, mi, a);
                    for chunk in bytes.chunks_exact(4) {
                        let code = u32::from_ne_bytes(chunk.try_into().unwrap());
                        assert_ne!(code, 3, "fullscreen state must never be emitted");
                    }
                }
            }
        }
    }
}
