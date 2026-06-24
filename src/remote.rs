//! The **machine-axis** — the viewer-box half of remote desktop.
//!
//! Where serve mode (A1–A5) makes *this* box viewable by others, the machine
//! axis is what the user navigates *from*: you are always at `(machine,
//! workspace)`, with the existing workspace binds as the horizontal axis and
//! `Super+J/K` selecting which machine you view as the vertical one. Index 0 is
//! always the local machine; each `shoestring-remote-client` that has tunnelled
//! out to a remote box registers ([`shoestring_ipc::Request::RegisterRemoteClient`])
//! and takes the next index. Switching to a remote enters *capture mode*: every
//! local input event is swallowed and forwarded to that client as
//! [`shoestring_ipc::Event::CapturedInput`] (raw KVM passthrough), so the
//! remote's own keymap and keybinds apply natively — see [`crate::input`].
//!
//! This module owns the axis bookkeeping on [`ShoestringWm`]; the IPC handlers
//! live in [`crate::ipc`] and the input routing in [`crate::input`].

use shoestring_ipc::{Event, MachineEntry, RawInput};

use crate::ipc::ClientId;
use crate::state::ShoestringWm;

/// One registered remote machine on the axis (index 1.. ; index 0 is the
/// implicit local machine). Bound to the IPC connection that registered it — the
/// connection dropping removes the machine (see [`ShoestringWm::drop_remote_client`]).
#[derive(Debug, Clone)]
pub struct RemoteClient {
    /// The registering IPC connection. Captured input is pushed here, and its
    /// disconnect removes the machine from the axis.
    pub id: ClientId,
    /// Human name the client registered with — typically the remote host.
    pub label: String,
    /// The remote's negotiated pixel width; clamps the forwarded virtual pointer.
    pub width: u32,
    /// The remote's negotiated pixel height; clamps the forwarded virtual pointer.
    pub height: u32,
}

/// Where the active view lands when the machine at axis `removed_index` (1-based)
/// disconnects, given the current `view_index`. Factored out of any compositor
/// state so the index arithmetic is unit-testable:
/// - you were driving the machine that left → return to local (0);
/// - a machine *earlier* on the axis left → everything after it shifts down one,
///   so decrement to keep following the machine you're on;
/// - a machine *after* the active view left → no change.
fn view_after_removal(view_index: u8, removed_index: u8) -> u8 {
    if view_index == removed_index {
        0
    } else if view_index > removed_index {
        view_index - 1
    } else {
        view_index
    }
}

impl ShoestringWm {
    /// The active machine-axis view's client connection, or `None` when the
    /// local machine is in view (index 0) or while a session lock is up. `Some`
    /// is the signal that the WM is in **capture mode**: every local input event
    /// should be forwarded to this connection rather than handled locally.
    pub fn captured_client(&self) -> Option<ClientId> {
        if self.view_index == 0 || self.is_locked() {
            return None;
        }
        self.remote_clients
            .get(self.view_index as usize - 1)
            .map(|c| c.id)
    }

    /// The active remote machine, or `None` at the local view (index 0).
    pub fn active_machine(&self) -> Option<&RemoteClient> {
        if self.view_index == 0 {
            return None;
        }
        self.remote_clients.get(self.view_index as usize - 1)
    }

    /// The active machine's label, or `None` for local — the payload the
    /// [`Event::ViewChanged`] / bar chip want.
    pub fn active_machine_label(&self) -> Option<String> {
        self.active_machine().map(|c| c.label.clone())
    }

    /// The registered remote machines as wire entries (index 1.. ; local is the
    /// implicit index 0 and is not listed) for [`shoestring_ipc::Response::RemoteClients`].
    pub fn machine_entries(&self) -> Vec<MachineEntry> {
        self.remote_clients
            .iter()
            .enumerate()
            .map(|(i, c)| MachineEntry {
                index: i as u8 + 1,
                label: c.label.clone(),
            })
            .collect()
    }

    /// Switch the active view to `index` (0 = local, 1.. = a remote machine),
    /// clamped to the current machine count. Recentres the forwarded virtual
    /// pointer over the newly-viewed remote and broadcasts [`Event::ViewChanged`]
    /// when the index actually moves. The single internal path for the J/K /
    /// break-out keybinds and [`shoestring_ipc::Request::SetView`].
    pub fn set_view(&mut self, index: u8) {
        let max = self.remote_clients.len() as u8;
        let target = index.min(max);
        if target == self.view_index {
            return;
        }
        self.view_index = target;
        // Start the forwarded pointer at the centre of the remote we just
        // entered — there's no shared cursor between machines, so a defined
        // origin beats inheriting a stale position.
        if let Some(c) = self.active_machine() {
            self.remote_pointer = (c.width as f64 / 2.0, c.height as f64 / 2.0);
        }
        let label = self.active_machine_label();
        tracing::info!(view = target, ?label, "machine-axis view changed");
        self.emit_ipc(Event::ViewChanged {
            index: target,
            label,
        });
    }

    /// Move the active view along the axis by `delta` (e.g. `+1` next machine,
    /// `-1` toward local), clamped to `[0, machine_count]`. Drives `Super+J/K`.
    pub fn focus_machine_relative(&mut self, delta: i8) {
        let max = self.remote_clients.len() as i32;
        let target = (self.view_index as i32 + delta as i32).clamp(0, max) as u8;
        self.set_view(target);
    }

    /// Forward one captured local input transition to the active remote view as
    /// [`Event::CapturedInput`] — the per-event half of capture mode (the
    /// caller decides what to capture; see [`crate::input`]). A targeted push to
    /// just that client's connection, not a broadcast.
    pub fn forward_captured(&mut self, id: ClientId, event: RawInput) {
        self.push_event_to(id, Event::CapturedInput { event });
    }

    /// Remove a registered remote machine when its IPC connection drops. If it
    /// was the active view (or its removal shifts the active index out of
    /// range), the view clamps back toward local and a [`Event::ViewChanged`] is
    /// emitted so capture mode can never strand on a vanished machine. Returns
    /// `true` if a machine was actually removed.
    pub fn drop_remote_client(&mut self, id: ClientId) -> bool {
        let Some(pos) = self.remote_clients.iter().position(|c| c.id == id) else {
            return false;
        };
        self.remote_clients.remove(pos);

        // Axis index `pos+1` just vanished; recompute where the view should land.
        // set_view clamps and only emits if the index actually moves.
        let new_view = view_after_removal(self.view_index, pos as u8 + 1);
        if new_view != self.view_index {
            self.set_view(new_view);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::view_after_removal;

    #[test]
    fn removal_returns_to_local_when_active_machine_leaves() {
        // Driving machine at index 2; it disconnects → back to local.
        assert_eq!(view_after_removal(2, 2), 0);
        // Driving index 1; it leaves → local.
        assert_eq!(view_after_removal(1, 1), 0);
    }

    #[test]
    fn removal_decrements_when_earlier_machine_leaves() {
        // Driving index 3; index 1 leaves → still the same machine, now index 2.
        assert_eq!(view_after_removal(3, 1), 2);
        assert_eq!(view_after_removal(2, 1), 1);
    }

    #[test]
    fn removal_leaves_view_when_later_machine_leaves() {
        // Driving index 1; index 3 leaves → unaffected.
        assert_eq!(view_after_removal(1, 3), 1);
        // At local; any removal keeps you local.
        assert_eq!(view_after_removal(0, 1), 0);
        assert_eq!(view_after_removal(0, 5), 0);
    }
}
