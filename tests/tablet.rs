//! Integration tests for the `zwp_tablet_manager_v2` protocol — graphics
//! tablet / stylus support (Wacom et al.).
//!
//! These connect to a running shoestring-wm compositor as a Wayland client.
//! Without a physical tablet there are no tool events to observe, so the tests
//! verify the protocol *plumbing*: the manager global is advertised and a
//! tablet seat can be obtained for the wl_seat and bound cleanly. The actual
//! proximity/pressure/tilt routing only fires from the libinput backend with
//! real hardware attached and isn't exercised here.
//!
//! Both tests are `#[ignore]` — they need a live compositor. Run with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test tablet -- --ignored

use wayland_client::{
    protocol::{
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2::ZwpTabletManagerV2, zwp_tablet_seat_v2::ZwpTabletSeatV2,
    zwp_tablet_tool_v2::ZwpTabletToolV2, zwp_tablet_v2::ZwpTabletV2,
};

#[derive(Default)]
struct State {
    seat: Option<WlSeat>,
    tablet_manager: Option<ZwpTabletManagerV2>,
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
                "zwp_tablet_manager_v2" => {
                    state.tablet_manager =
                        Some(registry.bind::<ZwpTabletManagerV2, _, _>(name, 1, qh, ()));
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

impl Dispatch<ZwpTabletManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletManagerV2,
        _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// A tablet seat announces tablets/tools/pads as they appear. With no hardware
// attached it stays silent; we only need the object to bind without protocol
// error.
impl Dispatch<ZwpTabletSeatV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletSeatV2,
        _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_seat_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletV2,
        _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTabletToolV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpTabletToolV2,
        _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_tool_v2::Event,
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
        state.tablet_manager.is_some(),
        "zwp_tablet_manager_v2 should always be advertised"
    );
}

/// Obtaining the tablet seat for the wl_seat must round-trip without a
/// protocol error. With no hardware attached the seat emits no
/// tablet_added/tool_added events — binding cleanly is the whole assertion.
#[test]
#[ignore]
fn tablet_seat_binds() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, mut eq, mut state) = connect();
    let qh = eq.handle();

    let (Some(seat), Some(manager)) = (state.seat.clone(), state.tablet_manager.clone()) else {
        panic!("wl_seat and zwp_tablet_manager_v2 must both be present");
    };

    let _tablet_seat = manager.get_tablet_seat(&seat, &qh, ());
    // Two round-trips: one to flush the request, one to surface any deferred
    // protocol error the server might raise in response.
    eq.roundtrip(&mut state).expect("get_tablet_seat roundtrip");
    eq.roundtrip(&mut state)
        .expect("post-bind roundtrip should be clean");
}
