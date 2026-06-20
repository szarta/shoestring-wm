//! Integration tests for the ext-workspace-v1 protocol.
//!
//! These connect to a running shoestring-wm compositor as a Wayland client and
//! verify the protocol is correctly wired: on bind the client receives one
//! workspace group spanning the outputs and one handle per workspace, each with
//! a name, 1-D coordinates, the `activate` capability, and exactly one handle
//! flagged `active`. A second test drives an `activate` + `commit` and confirms
//! the active flag moves — then restores the original workspace.
//!
//! All tests are `#[ignore]` — they require a live compositor.  Run with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test ext_workspace -- --ignored
//!
//! Note: the activation test switches the live workspace and switches back.

use std::collections::HashMap;

use wayland_client::{
    backend::{ObjectData, ObjectId},
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{
        self, ExtWorkspaceHandleV1, State as WsState, WorkspaceCapabilities,
    },
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

// ── Per-workspace state collected during enumeration ──────────────────────────

#[derive(Clone)]
struct WsInfo {
    proxy: ExtWorkspaceHandleV1,
    id: String,
    name: String,
    coordinates: Vec<u8>,
    /// Raw bits of the most recent `state` event.
    state_bits: u32,
    /// Raw bits of the most recent `capabilities` event.
    caps_bits: u32,
}

impl WsInfo {
    fn new(proxy: ExtWorkspaceHandleV1) -> Self {
        Self {
            proxy,
            id: String::new(),
            name: String::new(),
            coordinates: Vec::new(),
            state_bits: 0,
            caps_bits: 0,
        }
    }

    fn is_active(&self) -> bool {
        self.state_bits & WsState::Active.bits() != 0
    }
}

// ── Top-level client state ────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    manager: Option<ExtWorkspaceManagerV1>,
    group: Option<ExtWorkspaceGroupHandleV1>,
    /// Workspaces indexed by their wayland object ID.
    workspaces: HashMap<ObjectId, WsInfo>,
    /// Object IDs in the order their `workspace` events arrived.
    order: Vec<ObjectId>,
    /// Number of `output_enter` events seen on the group.
    output_enters: u32,
    /// Bumped on every manager `done`, so tests can wait for a fresh burst.
    done_count: u32,
}

impl State {
    fn ordered(&self) -> Vec<WsInfo> {
        self.order
            .iter()
            .filter_map(|id| self.workspaces.get(id).cloned())
            .collect()
    }
}

// ── Dispatch implementations ──────────────────────────────────────────────────

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            // Bind wl_output first (a real bar does) so the compositor has a
            // client-side output to reference in the group's output_enter. The
            // wl_output global is registered before ext_workspace, so it arrives
            // first and is bound before the manager triggers the announce.
            if interface == "wl_output" {
                registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
            }
            if interface == "ext_workspace_manager_v1" {
                let mgr =
                    registry.bind::<ExtWorkspaceManagerV1, _, _>(name, version.min(1), qh, ());
                state.manager = Some(mgr);
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> std::sync::Arc<dyn ObjectData> {
        match opcode {
            // 0 = workspace_group (new_id: ext_workspace_group_handle_v1)
            0 => qh.make_data::<ExtWorkspaceGroupHandleV1, _>(()),
            // 1 = workspace (new_id: ext_workspace_handle_v1)
            1 => qh.make_data::<ExtWorkspaceHandleV1, _>(()),
            o => panic!("unexpected event_created_child opcode {o} on ExtWorkspaceManagerV1"),
        }
    }

    fn event(
        state: &mut Self,
        _: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.group = Some(workspace_group);
            }
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                let id = workspace.id();
                state.workspaces.insert(id.clone(), WsInfo::new(workspace));
                state.order.push(id);
            }
            ext_workspace_manager_v1::Event::Done => state.done_count += 1,
            ext_workspace_manager_v1::Event::Finished => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_workspace_group_handle_v1::Event::OutputEnter { .. } = event {
            state.output_enters += 1;
        }
    }
}

impl Dispatch<ExtWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.workspaces.get_mut(&handle.id()) else {
            return;
        };
        match event {
            ext_workspace_handle_v1::Event::Id { id } => info.id = id,
            ext_workspace_handle_v1::Event::Name { name } => info.name = name,
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                info.coordinates = coordinates
            }
            ext_workspace_handle_v1::Event::State { state: s } => info.state_bits = enum_bits(s),
            ext_workspace_handle_v1::Event::Capabilities { capabilities } => {
                info.caps_bits = enum_bits(capabilities)
            }
            _ => {}
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract the raw `u32` bits from a `WEnum` carrying a bitfield value.
fn enum_bits<T: Into<u32>>(w: WEnum<T>) -> u32 {
    match w {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

fn wayland_display_or_skip() -> Option<String> {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            None
        }
    }
}

/// Connect, bind the manager, and roundtrip until the initial `done` lands.
fn connect_and_enumerate() -> (Connection, wayland_client::EventQueue<State>, State) {
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();

    conn.display().get_registry(&qh, ());

    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip failed");

    assert!(
        state.manager.is_some(),
        "ext_workspace_manager_v1 global not advertised — is this shoestring-wm?"
    );

    for _ in 0..10 {
        if state.done_count > 0 && !state.workspaces.is_empty() {
            break;
        }
        eq.roundtrip(&mut state)
            .expect("enumeration roundtrip failed");
    }

    (conn, eq, state)
}

/// Send `activate` on `handle`, `commit` the manager, and roundtrip until the
/// active flag settles on that workspace's id.
fn activate_and_commit(
    eq: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    handle: &ExtWorkspaceHandleV1,
) {
    handle.activate();
    state.manager.as_ref().unwrap().commit();
    let target = handle.id();
    for _ in 0..10 {
        eq.roundtrip(state).expect("activate roundtrip failed");
        if state
            .workspaces
            .get(&target)
            .map(|w| w.is_active())
            .unwrap_or(false)
        {
            break;
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// On bind the manager must announce a group, one handle per workspace (each
/// named, with 1-D coordinates and the `activate` capability), exactly one
/// active workspace, and a concluding `done`.
#[test]
#[ignore]
fn manager_announces_workspaces() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, _eq, state) = connect_and_enumerate();

    assert!(state.done_count > 0, "no done event after binding manager");
    assert!(state.group.is_some(), "no workspace group announced");
    assert!(!state.workspaces.is_empty(), "no workspaces announced");
    assert!(
        state.output_enters > 0,
        "group reported no outputs (output_enter never fired)"
    );

    let workspaces = state.ordered();
    let active_count = workspaces.iter().filter(|w| w.is_active()).count();
    assert_eq!(
        active_count, 1,
        "expected exactly one active workspace, got {active_count}"
    );

    for (i, ws) in workspaces.iter().enumerate() {
        assert!(!ws.name.is_empty(), "workspace {i} has an empty name");
        assert!(!ws.id.is_empty(), "workspace {i} has an empty id");
        assert_eq!(
            ws.coordinates.len(),
            4,
            "workspace {i} coordinates should be one u32"
        );
        let coord = u32::from_ne_bytes(ws.coordinates.clone().try_into().unwrap());
        assert_eq!(coord, i as u32, "workspace {i} coordinate mismatch");
        assert!(
            ws.caps_bits & WorkspaceCapabilities::Activate.bits() != 0,
            "workspace {i} missing activate capability"
        );
    }
}

/// Activating a different workspace and committing must move the active flag,
/// confirming the activate/commit request path drives a real switch.
#[test]
#[ignore]
fn activate_switches_workspace() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_enumerate();

    let workspaces = state.ordered();
    assert!(
        workspaces.len() >= 2,
        "need at least 2 workspaces to test switching"
    );

    let original_pos = workspaces
        .iter()
        .position(|w| w.is_active())
        .expect("no active workspace");
    let target_pos = (original_pos + 1) % workspaces.len();
    let target = workspaces[target_pos].proxy.clone();
    let original = workspaces[original_pos].proxy.clone();

    activate_and_commit(&mut eq, &mut state, &target);
    assert!(
        state.workspaces.get(&target.id()).unwrap().is_active(),
        "target workspace not active after activate+commit"
    );
    assert!(
        !state.workspaces.get(&original.id()).unwrap().is_active(),
        "original workspace still active after switching away"
    );

    // Restore the original workspace.
    activate_and_commit(&mut eq, &mut state, &original);
    assert!(
        state.workspaces.get(&original.id()).unwrap().is_active(),
        "failed to restore original workspace"
    );
}
