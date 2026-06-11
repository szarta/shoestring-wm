//! Integration tests for the wlr-foreign-toplevel-management-unstable-v1
//! protocol (the writable taskbar protocol — activate / close / minimize /
//! maximize, plus per-toplevel state).
//!
//! These connect to a running shoestring-wm compositor as a Wayland client and
//! verify the protocol is correctly wired: the manager global is advertised,
//! every current toplevel is announced with a terminating `done`, the `state`
//! array only ever carries codes the WM emits (maximized/minimized/activated,
//! never fullscreen), at most one toplevel is `activated`, and a `set_minimized`
//! round-trip actually flips the minimized bit in the `state` event.
//!
//! All tests are `#[ignore]` — they require a live compositor. Run them with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test foreign_toplevel_management -- --ignored
//!
//! `minimize_roundtrip_flips_state` momentarily minimizes and then restores one
//! real window, so run it deliberately. Task #99 (headless/winit CI) would pair
//! these with a test-spawned toplevel so they run unattended without touching a
//! user's windows.

use std::collections::HashMap;
use std::sync::Arc;

use wayland_client::{
    backend::{ObjectData, ObjectId},
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

// State codes carried in the `state` array event, per the protocol XML.
const STATE_MAXIMIZED: u32 = 0;
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;

// ── Per-toplevel state collected during enumeration ───────────────────────────

#[derive(Default, Clone)]
struct TopInfo {
    title: String,
    #[allow(dead_code)]
    app_id: String,
    /// Decoded `state` array (native-endian u32 codes).
    states: Vec<u32>,
    /// `output_enter` count minus `output_leave` count.
    outputs: i32,
    /// Number of `done` events seen (≥1 once fully described).
    done_count: u32,
    /// The handle proxy, so tests can issue requests against it.
    handle: Option<ZwlrForeignToplevelHandleV1>,
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    #[allow(dead_code)]
    seat: Option<WlSeat>,
    tops: HashMap<ObjectId, TopInfo>,
}

fn decode_states(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
        .collect()
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
            match interface.as_str() {
                "zwlr_foreign_toplevel_manager_v1" => {
                    let mgr = registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    );
                    state.manager = Some(mgr);
                }
                // Bind a seat so `activate` has one to name, and outputs so the
                // server actually sends us `output_enter`/`output_leave`
                // (it only does for outputs this client has bound).
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "wl_output" => {
                    registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        // opcode 0 = toplevel (new_id: zwlr_foreign_toplevel_handle_v1)
        if opcode == 0 {
            qh.make_data::<ZwlrForeignToplevelHandleV1, _>(())
        } else {
            panic!("unexpected event_created_child opcode {opcode} on manager");
        }
    }

    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                let info = TopInfo {
                    handle: Some(toplevel.clone()),
                    ..Default::default()
                };
                state.tops.insert(toplevel.id(), info);
            }
            zwlr_foreign_toplevel_manager_v1::Event::Finished => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        // `closed` removes the toplevel; subsequent events are guaranteed absent.
        if let Event::Closed = event {
            state.tops.remove(&handle.id());
            return;
        }
        let Some(info) = state.tops.get_mut(&handle.id()) else {
            return;
        };
        match event {
            Event::Title { title } => info.title = title,
            Event::AppId { app_id } => info.app_id = app_id,
            Event::OutputEnter { .. } => info.outputs += 1,
            Event::OutputLeave { .. } => info.outputs -= 1,
            Event::State { state } => info.states = decode_states(&state),
            Event::Done => info.done_count += 1,
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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

// ── Helpers ────────────────────────────────────────────────────────────────────

fn wayland_display_or_skip() -> Option<String> {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            None
        }
    }
}

/// Connect, bind the manager (+ seat + outputs), and settle by roundtripping a
/// few times so every announced toplevel's initial event burst (title/app_id/
/// state/output_enter/done) has arrived.
fn connect_and_enumerate() -> (Connection, wayland_client::EventQueue<State>, State) {
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();

    conn.display().get_registry(&qh, ());
    let mut state = State::default();

    // First roundtrip binds the manager; the bind handler then queues a
    // `toplevel` + full event burst for every existing window.
    eq.roundtrip(&mut state).expect("registry roundtrip failed");
    assert!(
        state.manager.is_some(),
        "zwlr_foreign_toplevel_manager_v1 global not advertised — is this shoestring-wm?"
    );

    // Settle: a handful of roundtrips is plenty for the per-toplevel bursts.
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("enumeration roundtrip failed");
    }

    (conn, eq, state)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// The manager global must be advertised and bind cleanly. Safe to run against
/// any session (no windows required, no mutation).
#[test]
#[ignore]
fn manager_global_advertised() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, _eq, state) = connect_and_enumerate();
    assert!(state.manager.is_some(), "manager global missing");
}

/// Every announced toplevel must be fully described (a terminating `done`), its
/// `state` array must contain only codes the WM emits (never `fullscreen`), and
/// at most one toplevel may be `activated`. Non-mutating; safe against a live
/// session.
#[test]
#[ignore]
fn announced_toplevels_are_well_formed() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, _eq, state) = connect_and_enumerate();

    if state.tops.is_empty() {
        eprintln!("no toplevels open — open a window to exercise this test");
    }

    let mut activated = 0;
    for (id, info) in &state.tops {
        assert!(
            info.done_count >= 1,
            "toplevel {id:?} never received a terminating `done`"
        );
        for &code in &info.states {
            assert_ne!(
                code, STATE_FULLSCREEN,
                "toplevel {id:?} reported fullscreen — the WM must never emit it"
            );
            assert!(
                matches!(code, STATE_MAXIMIZED | STATE_MINIMIZED | STATE_ACTIVATED),
                "toplevel {id:?} reported unknown state code {code}"
            );
        }
        if info.states.contains(&STATE_ACTIVATED) {
            activated += 1;
        }
    }
    assert!(
        activated <= 1,
        "{activated} toplevels report `activated`; at most one may be focused"
    );
}

/// `set_minimized` then `unset_minimized` on a live window must be reflected in
/// its `state` event both ways. Mutating but self-restoring: it briefly
/// minimizes and then restores one real window. `#[ignore]` — run deliberately.
#[test]
#[ignore]
fn minimize_roundtrip_flips_state() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_enumerate();

    // Pick a currently-non-minimized toplevel to act on.
    let target = state
        .tops
        .iter()
        .find(|(_, i)| !i.states.contains(&STATE_MINIMIZED) && i.handle.is_some())
        .map(|(id, i)| (id.clone(), i.handle.clone().unwrap()));
    let Some((id, handle)) = target else {
        eprintln!("no non-minimized toplevel to act on — skipping");
        return;
    };

    // Minimize, settle, assert the bit is set.
    handle.set_minimized();
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("roundtrip after set_minimized failed");
    }
    assert!(
        state
            .tops
            .get(&id)
            .map(|i| i.states.contains(&STATE_MINIMIZED))
            .unwrap_or(false),
        "state did not report `minimized` after set_minimized"
    );

    // Restore, settle, assert the bit cleared.
    handle.unset_minimized();
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("roundtrip after unset_minimized failed");
    }
    assert!(
        state
            .tops
            .get(&id)
            .map(|i| !i.states.contains(&STATE_MINIMIZED))
            .unwrap_or(false),
        "state still reports `minimized` after unset_minimized"
    );
}

/// `set_maximized` then `unset_maximized` must flip the maximized bit in the
/// `state` event both ways. Mutating but self-restoring. `#[ignore]`.
#[test]
#[ignore]
fn maximize_roundtrip_flips_state() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_enumerate();

    let target = state
        .tops
        .iter()
        .find(|(_, i)| !i.states.contains(&STATE_MAXIMIZED) && i.handle.is_some())
        .map(|(id, i)| (id.clone(), i.handle.clone().unwrap()));
    let Some((id, handle)) = target else {
        eprintln!("no non-maximized toplevel to act on — skipping");
        return;
    };

    handle.set_maximized();
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("roundtrip after set_maximized failed");
    }
    assert!(
        state
            .tops
            .get(&id)
            .map(|i| i.states.contains(&STATE_MAXIMIZED))
            .unwrap_or(false),
        "state did not report `maximized` after set_maximized"
    );

    handle.unset_maximized();
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("roundtrip after unset_maximized failed");
    }
    assert!(
        state
            .tops
            .get(&id)
            .map(|i| !i.states.contains(&STATE_MAXIMIZED))
            .unwrap_or(false),
        "state still reports `maximized` after unset_maximized"
    );
}

/// `activate` must move the `activated` bit to the targeted toplevel: after
/// activating a non-activated window, it gains the bit and no more than one
/// toplevel reports it. Mutating (changes focus). `#[ignore]`.
#[test]
#[ignore]
fn activate_moves_activated_state() {
    let Some(_d) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_enumerate();

    let seat = match &state.seat {
        Some(s) => s.clone(),
        None => {
            eprintln!("no wl_seat advertised — skipping");
            return;
        }
    };

    // A toplevel that isn't already activated (so we can observe it gaining it).
    let target = state
        .tops
        .iter()
        .find(|(_, i)| !i.states.contains(&STATE_ACTIVATED) && i.handle.is_some())
        .map(|(id, i)| (id.clone(), i.handle.clone().unwrap()));
    let Some((id, handle)) = target else {
        eprintln!("no non-activated toplevel to act on — skipping");
        return;
    };

    handle.activate(&seat);
    for _ in 0..5 {
        eq.roundtrip(&mut state)
            .expect("roundtrip after activate failed");
    }
    assert!(
        state
            .tops
            .get(&id)
            .map(|i| i.states.contains(&STATE_ACTIVATED))
            .unwrap_or(false),
        "activated toplevel did not gain the `activated` state"
    );
    let activated = state
        .tops
        .values()
        .filter(|i| i.states.contains(&STATE_ACTIVATED))
        .count();
    assert_eq!(
        activated, 1,
        "exactly one toplevel must be activated after activate"
    );
}
