//! Integration test for the `zwp_pointer_gestures_v1` protocol — touchpad
//! multi-finger swipe / pinch / hold gestures.
//!
//! Connects to a running shoestring-wm as a Wayland client. Without a physical
//! touchpad there are no gesture events to observe, so this verifies the
//! protocol *plumbing*: the manager global is advertised and binds cleanly.
//! Real swipe/pinch/hold routing only fires from the libinput backend with a
//! gesture-capable touchpad attached and isn't exercised here.
//!
//! `#[ignore]` — needs a live compositor. Run with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test pointer_gestures -- --ignored

use wayland_client::{
    protocol::{
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1;

#[derive(Default)]
struct State {
    gestures: Option<ZwpPointerGesturesV1>,
}

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
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    registry.bind::<WlSeat, _, _>(name, 1, qh, ());
                }
                "zwp_pointer_gestures_v1" => {
                    state.gestures =
                        Some(registry.bind::<ZwpPointerGesturesV1, _, _>(name, 1, qh, ()));
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

impl Dispatch<ZwpPointerGesturesV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpPointerGesturesV1,
        _: <ZwpPointerGesturesV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn wayland_display_or_skip() -> bool {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => true,
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            false
        }
    }
}

/// The manager global is unconditional; it must be advertised and bind cleanly.
/// Non-mutating.
#[test]
#[ignore]
fn manager_global_advertised() {
    if !wayland_display_or_skip() {
        return;
    }
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip failed");
    assert!(
        state.gestures.is_some(),
        "zwp_pointer_gestures_v1 should always be advertised"
    );
}
