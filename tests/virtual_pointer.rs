//! Integration tests for the `zwlr_virtual_pointer_v1` protocol — pointer
//! emulation for clients such as `wlrctl` and remote-desktop agents.
//!
//! These connect to a running shoestring-wm compositor as a Wayland client.
//! Driving the pointer (motion/button/axis + frame) moves the real cursor and
//! would be invasive against a live session, so — like the tablet tests — these
//! verify the protocol *plumbing*: the manager global is advertised and a
//! virtual pointer can be created and torn down cleanly without sending a
//! `frame` (so no synthetic events are produced). The full event-delivery
//! contract is covered by the WLCS `VirtualPointerV1Test` suite.
//!
//! Both tests are `#[ignore]` — they need a live compositor. Run with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test virtual_pointer -- --ignored

use wayland_client::{
    protocol::{
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct State {
    seat: Option<WlSeat>,
    manager: Option<ZwlrVirtualPointerManagerV1>,
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
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.manager = Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(
                        name,
                        version.min(2),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn wayland_display_or_skip() -> bool {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => true,
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            false
        }
    }
}

fn connect() -> (Connection, wayland_client::EventQueue<State>, State) {
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip failed");
    (conn, eq, state)
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// The manager global must be advertised (it is unconditional) and bind
/// cleanly. Non-mutating.
#[test]
#[ignore]
fn manager_global_advertised() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, _eq, state) = connect();
    assert!(
        state.manager.is_some(),
        "zwlr_virtual_pointer_manager_v1 should always be advertised"
    );
}

/// Creating a virtual pointer and destroying it must round-trip without a
/// protocol error. No `frame` is sent, so this produces no synthetic input —
/// the clean bind/teardown is the whole assertion.
#[test]
#[ignore]
fn create_virtual_pointer_binds() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, mut eq, mut state) = connect();
    let qh = eq.handle();

    let Some(manager) = state.manager.clone() else {
        panic!("zwlr_virtual_pointer_manager_v1 must be present");
    };

    // Pass the seat as a hint when we have one; null is also valid.
    let pointer = manager.create_virtual_pointer(state.seat.as_ref(), &qh, ());
    // Two round-trips: one to flush the request, one to surface any deferred
    // protocol error the server might raise in response.
    eq.roundtrip(&mut state)
        .expect("create_virtual_pointer roundtrip");
    pointer.destroy();
    eq.roundtrip(&mut state)
        .expect("post-destroy roundtrip should be clean");
}
