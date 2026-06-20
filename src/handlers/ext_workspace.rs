//! `ext-workspace-v1` dispatch. See [`crate::ext_workspace`] for state and the
//! broadcast helpers driven from the workspace-switch / output-hotplug paths.
//!
//! All three interfaces ride the blanket `delegate_dispatch2!(ShoestringWm)`,
//! so we only implement [`Dispatch2`] / [`GlobalDispatch2`] for the per-resource
//! data types.

use smithay::{
    reexports::wayland_server::{
        backend::ClientId, Client, DataInit, DisplayHandle, New, Resource,
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::{
    ext_workspace::{ExtWorkspaceGroupData, ExtWorkspaceHandleData, ExtWorkspaceManagerData},
    state::ShoestringWm,
};

// ── Manager ─────────────────────────────────────────────────────────────────

impl GlobalDispatch2<ExtWorkspaceManagerV1, ShoestringWm> for ExtWorkspaceManagerData {
    fn bind(
        &self,
        state: &mut ShoestringWm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        let manager = data_init.init(resource, ExtWorkspaceManagerData);
        crate::ext_workspace::announce_to_manager(state, manager);
    }
}

impl Dispatch2<ExtWorkspaceManagerV1, ShoestringWm> for ExtWorkspaceManagerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use ext_workspace_manager_v1::Request;
        match request {
            // Apply the activation batched since the previous commit atomically.
            Request::Commit => {
                let target = state
                    .ext_workspace
                    .instances
                    .iter_mut()
                    .find(|inst| inst.manager.id() == resource.id())
                    .and_then(|inst| inst.pending_activate.take());
                if let Some(target) = target {
                    state.focus_workspace_id(target);
                }
            }
            // The client is done; reply `finished` (a destructor event) and drop
            // our bookkeeping. The client must send no further requests.
            Request::Stop => {
                resource.finished();
                crate::ext_workspace::forget_manager(state, resource);
            }
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
    ) {
        crate::ext_workspace::forget_manager(state, resource);
    }
}

// ── Group handle ────────────────────────────────────────────────────────────

impl Dispatch2<ExtWorkspaceGroupHandleV1, ShoestringWm> for ExtWorkspaceGroupData {
    fn request(
        &self,
        _state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use ext_workspace_group_handle_v1::Request;
        match request {
            // We advertise no create_workspace capability — the set is fixed.
            Request::CreateWorkspace { .. } => {}
            Request::Destroy => {}
            _ => {}
        }
    }
}

// ── Workspace handle ────────────────────────────────────────────────────────

impl Dispatch2<ExtWorkspaceHandleV1, ShoestringWm> for ExtWorkspaceHandleData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use ext_workspace_handle_v1::Request;
        match request {
            // Stage the switch; it's applied on the manager's next `commit`.
            Request::Activate => {
                let Some(target) = state.workspaces.from_one_based(self.index + 1) else {
                    return;
                };
                if let Some(inst) = state.ext_workspace.instances.iter_mut().find(|inst| {
                    // `ext_workspace_handle_v1::id` is an *event* method, so
                    // it shadows `Resource::id`; disambiguate explicitly.
                    inst.workspaces
                        .iter()
                        .any(|h| Resource::id(h) == Resource::id(resource))
                }) {
                    inst.pending_activate = Some(target);
                }
            }
            // deactivate / assign / remove: capabilities not advertised, ignored.
            Request::Deactivate => {}
            Request::Assign { .. } => {}
            Request::Remove => {}
            Request::Destroy => {}
            _ => {}
        }
    }
}
