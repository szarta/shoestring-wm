//! Integration tests for the wlr-output-management-unstable-v1 protocol.
//!
//! These tests connect to a running shoestring-wm compositor as a Wayland
//! client and verify the protocol is correctly wired: heads are announced on
//! bind, `done` carries a non-zero serial, and a no-op configuration round-trip
//! returns `succeeded`.
//!
//! All tests are `#[ignore]` — they require a live compositor.  Run them with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test output_management -- --ignored
//!
//! Connector names printed at WM startup (INFO log) or via
//! `shoestring-ctl outputs` can be used to verify the head names returned here.

use std::collections::HashMap;

use std::sync::Arc;

use wayland_client::{
    backend::{ObjectData, ObjectId},
    protocol::wl_registry::{self, WlRegistry},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

// ── Per-head state collected during enumeration ───────────────────────────────

#[derive(Debug, Default)]
struct HeadInfo {
    name: String,
    enabled: bool,
    mode_count: u32,
    /// Serial of the `done` event that concluded this head's description.
    seen_in_serial: u32,
}

// ── Top-level client state ────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    manager: Option<ZwlrOutputManagerV1>,
    /// Heads indexed by their wayland object ID.
    heads: HashMap<ObjectId, HeadInfo>,
    /// Serial from the most recent `done` event; 0 = not yet received.
    done_serial: u32,
    /// Result of the most recent `apply` or `test` call.
    config_result: Option<ConfigResult>,
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigResult {
    Succeeded,
    Failed,
    Cancelled,
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
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "zwlr_output_manager_v1" {
                let mgr = registry
                    .bind::<ZwlrOutputManagerV1, _, _>(name, version.min(4), qh, ());
                state.manager = Some(mgr);
            }
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // opcode 0 = head (new_id: zwlr_output_head_v1)
        if opcode == 0 {
            qh.make_data::<ZwlrOutputHeadV1, _>(())
        } else {
            panic!("unexpected event_created_child opcode {opcode} on ZwlrOutputManagerV1");
        }
    }

    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => {
                state.heads.insert(head.id(), HeadInfo::default());
            }
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.done_serial = serial;
                for info in state.heads.values_mut() {
                    if info.seen_in_serial == 0 {
                        info.seen_in_serial = serial;
                    }
                }
            }
            zwlr_output_manager_v1::Event::Finished => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // opcode 3 = mode (new_id: zwlr_output_mode_v1)
        if opcode == 3 {
            qh.make_data::<ZwlrOutputModeV1, _>(())
        } else {
            panic!("unexpected event_created_child opcode {opcode} on ZwlrOutputHeadV1");
        }
    }

    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.heads.get_mut(&head.id()) else { return };
        match event {
            zwlr_output_head_v1::Event::Name { name } => info.name = name,
            zwlr_output_head_v1::Event::Enabled { enabled } => info.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Mode { .. } => info.mode_count += 1,
            zwlr_output_head_v1::Event::Finished => {
                // Head removed — drop from map on next access, no action needed.
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputModeV1,
        _: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.config_result = Some(match event {
            zwlr_output_configuration_v1::Event::Succeeded => ConfigResult::Succeeded,
            zwlr_output_configuration_v1::Event::Failed => ConfigResult::Failed,
            zwlr_output_configuration_v1::Event::Cancelled => ConfigResult::Cancelled,
            _ => return,
        });
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: zwlr_output_configuration_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the value of `WAYLAND_DISPLAY`, or skip the test if not set.
///
/// "Skip" is implemented by returning early from the test via a sentinel value;
/// callers use `let Some(display) = wayland_display_or_skip() else { return }`.
fn wayland_display_or_skip() -> Option<String> {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            None
        }
    }
}

/// Connect, get the registry, bind any globals we recognise, then roundtrip
/// until `done_serial > 0`.  Returns the populated `State`.
fn connect_and_enumerate() -> State {
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();

    conn.display().get_registry(&qh, ());

    let mut state = State::default();

    // First roundtrip: receive globals and bind the manager.
    eq.roundtrip(&mut state).expect("registry roundtrip failed");

    assert!(
        state.manager.is_some(),
        "zwlr_output_manager_v1 global not advertised — is this shoestring-wm?"
    );

    // The manager is bound during the first roundtrip, so the server starts
    // queuing head events at that moment.  Keep dispatching until `done` lands.
    // Two extra roundtrips are typically sufficient; allow up to 10 for slow
    // compositors or high head counts.
    for _ in 0..10 {
        if state.done_serial > 0 {
            break;
        }
        eq.roundtrip(&mut state).expect("head-enumeration roundtrip failed");
    }

    state
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The manager must announce at least one head and a `done` event with a
/// non-zero serial when a client first binds.
#[test]
#[ignore]
fn manager_announces_heads_and_done() {
    let Some(_display) = wayland_display_or_skip() else { return };

    let state = connect_and_enumerate();

    assert!(
        state.done_serial > 0,
        "done event not received after binding zwlr_output_manager_v1"
    );
    assert!(
        !state.heads.is_empty(),
        "no heads announced (compositor has no outputs?)"
    );

    for (id, info) in &state.heads {
        assert!(
            !info.name.is_empty(),
            "head {id:?} has an empty name"
        );
        assert!(
            info.mode_count > 0,
            "head {:?} ({}) has no modes",
            id,
            info.name
        );
    }
}

/// A `create_configuration` that mirrors the current state (no actual changes)
/// must return `succeeded`, confirming the serial handshake and apply path work.
#[test]
#[ignore]
fn noop_configuration_succeeds() {
    let Some(_display) = wayland_display_or_skip() else { return };

    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();

    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip failed");
    for _ in 0..10 {
        if state.done_serial > 0 {
            break;
        }
        eq.roundtrip(&mut state).expect("head-enumeration roundtrip failed");
    }

    let serial = state.done_serial;
    assert!(serial > 0, "no done serial received");

    let manager = state.manager.as_ref().expect("no manager");

    // create_configuration with the serial we just received.
    let config = manager.create_configuration(serial, &qh, ());

    // Apply without changing anything. The compositor validates the serial and
    // the set of enabled heads; since we haven't set any head overrides this is
    // effectively a no-op (enable_head / disable_head not called → no heads in
    // the configuration → the compositor applies "no changes" and succeeds).
    config.apply();

    // Roundtrip to receive succeeded/failed/cancelled.
    eq.roundtrip(&mut state).expect("apply roundtrip failed");

    assert_eq!(
        state.config_result,
        Some(ConfigResult::Succeeded),
        "no-op configuration did not return succeeded"
    );
}
