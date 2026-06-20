//! `ext-workspace-v1` state and broadcast helpers.
//!
//! Smithay ships no delegate for this staging protocol, so we hand-roll the
//! server side following the same pattern as [`crate::foreign_toplevel_mgmt`] /
//! [`crate::output_management`]: this module owns the state and the
//! event-broadcast logic, while the dispatch impls live in
//! [`crate::handlers::ext_workspace`].
//!
//! The protocol lets standard bars (waybar, Sway 1.11+/Niri-style) read and
//! switch workspaces without a shoestring-specific IPC client. It is the
//! Wayland-side mirror of the WM's own workspace state
//! ([`crate::workspace::WorkspaceManager`]) and the IPC `Workspaces` query.
//!
//! ## Model
//! Workspaces in this WM are *global* — all outputs swap together (see
//! [`crate::workspace`]). The protocol's grouping is "which set of outputs a
//! workspace belongs to", so we advertise a **single workspace group** that
//! spans every output, holding the fixed set of `[workspaces].count`
//! workspaces. The set never grows or shrinks at runtime, so we advertise no
//! `create_workspace`/`remove`/`assign` capabilities — only `activate`, which
//! maps onto a workspace switch.
//!
//! ## Lifecycle
//! - A client binds the manager global → gets the group + one handle per
//!   workspace, then `done` ([`announce_to_manager`]).
//! - The active workspace changes → an updated `state` burst goes to every
//!   bound manager ([`broadcast_active`]), driven from
//!   [`crate::state::ShoestringWm::focus_workspace_id`].
//! - An output is hot-plugged / removed → an `output_enter`/`output_leave` is
//!   pushed on every group ([`broadcast_output_enter`] / [`broadcast_output_leave`]).
//! - The client sends `stop` → we reply `finished` and drop the instance.

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{
                ExtWorkspaceHandleV1, State as WorkspaceHandleState, WorkspaceCapabilities,
            },
            ext_workspace_manager_v1::ExtWorkspaceManagerV1,
        },
        wayland_server::{backend::GlobalId, protocol::wl_output::WlOutput, Resource},
    },
};

use crate::{state::ShoestringWm, workspace::WorkspaceId};

// ── State ───────────────────────────────────────────────────────────────────

pub struct ExtWorkspaceState {
    /// Keeps the manager global advertised for the WM's lifetime.
    #[allow(dead_code)]
    pub manager_global: GlobalId,
    /// Every bound, not-yet-stopped manager and the resources we created for it.
    pub instances: Vec<ExtWorkspaceInstance>,
}

/// One client's view: the manager it bound, the single group we advertise, and
/// one workspace handle per workspace (indexed by zero-based id).
pub struct ExtWorkspaceInstance {
    pub manager: ExtWorkspaceManagerV1,
    pub group: ExtWorkspaceGroupHandleV1,
    /// Indexed by [`WorkspaceId::zero_based`]; length == workspace count.
    pub workspaces: Vec<ExtWorkspaceHandleV1>,
    /// Activation target accumulated since the last `commit`, applied
    /// atomically when `commit` arrives (the protocol batches requests).
    pub pending_activate: Option<WorkspaceId>,
}

// ── User data attached to protocol objects ──────────────────────────────────

/// Attached to each `ext_workspace_manager_v1`; the instance lives in
/// [`ExtWorkspaceState::instances`], keyed by the manager resource.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtWorkspaceManagerData;

/// Attached to each `ext_workspace_group_handle_v1`. No per-group state.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExtWorkspaceGroupData;

/// Attached to each `ext_workspace_handle_v1`: the zero-based workspace index
/// it represents, used to resolve `activate` back to a [`WorkspaceId`].
#[derive(Debug, Clone, Copy)]
pub struct ExtWorkspaceHandleData {
    pub index: u8,
}

// ── Pure encoding helpers (unit-tested) ─────────────────────────────────────

/// The `coordinates` array payload for a 1-D layout: a single native-endian
/// `u32` carrying the zero-based workspace index, so bars can order workspaces.
pub fn coordinates_bytes(index: u8) -> Vec<u8> {
    (index as u32).to_ne_bytes().to_vec()
}

/// The workspace `state` flags for `ws` given the currently `active` workspace.
/// We only model `active`; `urgent`/`hidden` are never set.
fn handle_state(ws_index: u8, active: WorkspaceId) -> WorkspaceHandleState {
    if ws_index == active.zero_based() {
        WorkspaceHandleState::Active
    } else {
        WorkspaceHandleState::empty()
    }
}

/// Human-readable name for the zero-based workspace `index`: the configured
/// name if non-empty, else the 1-based number as a string.
fn workspace_name(names: &[String], index: u8) -> String {
    names
        .get(index as usize)
        .filter(|n| !n.is_empty())
        .cloned()
        .unwrap_or_else(|| (index + 1).to_string())
}

// ── Lifecycle entry points ──────────────────────────────────────────────────

/// Build the group + per-workspace handles for a freshly-bound `manager`, send
/// the full initial burst, and record the instance. Mirrors
/// [`crate::foreign_toplevel_mgmt::announce_existing_to_manager`].
pub fn announce_to_manager(state: &mut ShoestringWm, manager: ExtWorkspaceManagerV1) {
    let dh = state.display_handle.clone();
    let Ok(client) = dh.get_client(manager.id()) else {
        return;
    };
    let version = manager.version();

    // A single group spanning all outputs (workspaces are global here).
    let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, ShoestringWm>(
        &dh,
        version,
        ExtWorkspaceGroupData,
    ) else {
        return;
    };
    manager.workspace_group(&group);
    // No create_workspace: the workspace set is fixed by `[workspaces].count`.
    group.capabilities(GroupCapabilities::empty());
    for output in state.space.outputs() {
        for wl in output.client_outputs(&client) {
            group.output_enter(&wl);
        }
    }

    let active = state.workspaces.active();
    let count = state.workspaces.count();
    let names = state.workspaces.name_list();
    let mut handles = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, ShoestringWm>(
            &dh,
            version,
            ExtWorkspaceHandleData { index },
        ) else {
            continue;
        };
        manager.workspace(&handle);
        // 1-based number as a stable id: workspaces are config-defined and
        // persist across sessions, so clients may key preferences on it.
        handle.id((index + 1).to_string());
        handle.name(workspace_name(&names, index));
        handle.coordinates(coordinates_bytes(index));
        handle.state(handle_state(index, active));
        // Only activation is meaningful; the set is fixed and single-active.
        handle.capabilities(WorkspaceCapabilities::Activate);
        group.workspace_enter(&handle);
        handles.push(handle);
    }
    manager.done();

    state.ext_workspace.instances.push(ExtWorkspaceInstance {
        manager,
        group,
        workspaces: handles,
        pending_activate: None,
    });
}

/// Re-push the `state` flags for every workspace on every bound manager after
/// the active workspace changed. Driven from
/// [`crate::state::ShoestringWm::focus_workspace_id`].
pub fn broadcast_active(state: &mut ShoestringWm) {
    let active = state.workspaces.active();
    for inst in &state.ext_workspace.instances {
        if !inst.manager.is_alive() {
            continue;
        }
        for (index, handle) in inst.workspaces.iter().enumerate() {
            if handle.is_alive() {
                handle.state(handle_state(index as u8, active));
            }
        }
        inst.manager.done();
    }
}

/// Push `output_enter` for a newly-added `output` to every group, then `done`.
/// Mirrors [`crate::output_management::broadcast_head_added`]; a no-op when no
/// manager is bound yet.
pub fn broadcast_output_enter(state: &mut ShoestringWm, output: &Output) {
    let dh = state.display_handle.clone();
    for inst in &state.ext_workspace.instances {
        if !inst.group.is_alive() {
            continue;
        }
        let Ok(client) = dh.get_client(inst.manager.id()) else {
            continue;
        };
        for wl in output.client_outputs(&client) {
            inst.group.output_enter(&wl);
        }
        inst.manager.done();
    }
}

/// Push `output_leave` for an `output` being removed to every group, then
/// `done`. Call *before* the output's client resources are torn down (i.e.
/// before `leave_all`), so `client_outputs` still resolves.
pub fn broadcast_output_leave(state: &mut ShoestringWm, output: &Output) {
    let dh = state.display_handle.clone();
    for inst in &state.ext_workspace.instances {
        if !inst.group.is_alive() {
            continue;
        }
        let Ok(client) = dh.get_client(inst.manager.id()) else {
            continue;
        };
        for wl in output.client_outputs(&client) {
            inst.group.output_leave(&wl);
        }
        inst.manager.done();
    }
}

/// A client just bound a `wl_output` global. The protocol requires the group to
/// emit `output_enter` for a newly-bound `wl_output` of an already-assigned
/// output — important because our manager global is created before the output
/// globals, so a client that binds the manager first sees no outputs at announce
/// time and only learns of them when it binds `wl_output`. Sends `output_enter`
/// (+ `done`) on this client's group for the specific `wl_output`.
pub fn notify_output_bound(state: &mut ShoestringWm, wl_output: &WlOutput) {
    let dh = state.display_handle.clone();
    let Ok(target_client) = dh.get_client(wl_output.id()) else {
        return;
    };
    let target_id = target_client.id();
    for inst in &state.ext_workspace.instances {
        if !inst.group.is_alive() {
            continue;
        }
        // Only the instance owned by the client that bound this wl_output.
        if dh.get_client(inst.manager.id()).map(|c| c.id()).ok() != Some(target_id.clone()) {
            continue;
        }
        inst.group.output_enter(wl_output);
        inst.manager.done();
    }
}

/// Drop the instance owned by `manager` (client disconnect, or after a `stop`
/// → `finished` exchange). The group/handle resources are left for the client
/// to destroy; their dispatch carries no per-resource bookkeeping.
pub fn forget_manager(state: &mut ShoestringWm, manager: &ExtWorkspaceManagerV1) {
    state
        .ext_workspace
        .instances
        .retain(|inst| inst.manager.id() != manager.id());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_carry_the_index_as_one_u32() {
        assert_eq!(coordinates_bytes(0), 0u32.to_ne_bytes());
        assert_eq!(coordinates_bytes(5), 5u32.to_ne_bytes());
        assert_eq!(coordinates_bytes(5).len(), 4);
    }

    #[test]
    fn name_falls_back_to_one_based_number() {
        let names = vec![String::new(), "web".to_string(), String::new()];
        assert_eq!(workspace_name(&names, 0), "1");
        assert_eq!(workspace_name(&names, 1), "web");
        assert_eq!(workspace_name(&names, 2), "3");
        // Out-of-range index still yields the number rather than panicking.
        assert_eq!(workspace_name(&names, 9), "10");
    }
}
