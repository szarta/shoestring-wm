//! Integration tests for zwp_pointer_constraints_v1 + zwp_relative_pointer_v1.
//!
//! These connect to a running shoestring-wm compositor as a Wayland client and
//! verify the two protocols are wired: both manager globals are advertised, and
//! a client can lock the pointer / confine it / get a relative-pointer object
//! without tripping a protocol error.
//!
//! They do NOT exercise the motion-enforcement path (locked → relative-only,
//! confined → clamped): that needs synthesized pointer motion over a focused,
//! mapped surface, which this headless client harness can't drive. The
//! enforcement logic lives in `src/input.rs` and mirrors Smithay's reference
//! compositor.
//!
//! All tests are `#[ignore]` — they require a live compositor. Run with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test pointer_constraints -- --ignored

use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_pointer::WlPointer,
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_confined_pointer_v1::{self, ZwpConfinedPointerV1},
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};

// ── Client state ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    compositor: Option<WlCompositor>,
    seat: Option<WlSeat>,
    constraints: Option<ZwpPointerConstraintsV1>,
    relative: Option<ZwpRelativePointerManagerV1>,
}

// ── Dispatch impls ────────────────────────────────────────────────────────────

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
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ()));
                }
                "zwp_pointer_constraints_v1" => {
                    state.constraints =
                        Some(registry.bind::<ZwpPointerConstraintsV1, _, _>(name, 1, qh, ()));
                }
                "zwp_relative_pointer_manager_v1" => {
                    state.relative =
                        Some(registry.bind::<ZwpRelativePointerManagerV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

// These objects emit no events we need to react to in these tests; the empty
// impls just satisfy the type system so we can bind/create them.
macro_rules! ignore_events {
    ($($ty:ty),+ $(,)?) => {$(
        impl Dispatch<$ty, ()> for State {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}

ignore_events!(
    WlCompositor,
    WlSeat,
    WlPointer,
    WlSurface,
    ZwpPointerConstraintsV1,
    ZwpRelativePointerManagerV1,
);

// Constraint/relative objects: log the interesting events so a future
// enforcement test can assert on them; for now they're inert.
impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpLockedPointerV1,
        _: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpConfinedPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpConfinedPointerV1,
        _: zwp_confined_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpRelativePointerV1,
        _: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn wayland_display_or_skip() -> Option<String> {
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("WAYLAND_DISPLAY not set — skipping");
            None
        }
    }
}

/// Connect, enumerate globals, and bind the four we need. Panics if the
/// connection or registry roundtrip fails.
fn connect_and_bind() -> (Connection, wayland_client::EventQueue<State>, State) {
    let conn = Connection::connect_to_env()
        .expect("failed to connect to Wayland compositor via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip failed");
    (conn, eq, state)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Both manager globals must be advertised.
#[test]
#[ignore]
fn manager_globals_advertised() {
    let Some(_display) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, _eq, state) = connect_and_bind();
    assert!(
        state.constraints.is_some(),
        "zwp_pointer_constraints_v1 global not advertised — is this shoestring-wm?"
    );
    assert!(
        state.relative.is_some(),
        "zwp_relative_pointer_manager_v1 global not advertised"
    );
}

/// Locking the pointer on a fresh surface must round-trip without a protocol
/// error. This drives the full server dispatch path: bind → lock_pointer →
/// Smithay's `Dispatch2` impl → our `PointerConstraintsHandler::new_constraint`.
/// (The lock won't *activate* — the surface isn't focused — but creating it
/// must succeed.)
#[test]
#[ignore]
fn lock_pointer_roundtrips() {
    let Some(_display) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_bind();
    let qh = eq.handle();

    let compositor = state.compositor.clone().expect("no wl_compositor");
    let seat = state.seat.clone().expect("no wl_seat");
    let constraints = state.constraints.clone().expect("no pointer-constraints");

    let surface = compositor.create_surface(&qh, ());
    let pointer = seat.get_pointer(&qh, ());

    let _locked: ZwpLockedPointerV1 =
        constraints.lock_pointer(&surface, &pointer, None, Lifetime::Oneshot, &qh, ());

    // If the server raised a protocol error, the next roundtrip fails.
    eq.roundtrip(&mut state)
        .expect("lock_pointer round-trip raised a protocol error");
}

/// Confining the pointer must likewise round-trip cleanly.
#[test]
#[ignore]
fn confine_pointer_roundtrips() {
    let Some(_display) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_bind();
    let qh = eq.handle();

    let compositor = state.compositor.clone().expect("no wl_compositor");
    let seat = state.seat.clone().expect("no wl_seat");
    let constraints = state.constraints.clone().expect("no pointer-constraints");

    let surface = compositor.create_surface(&qh, ());
    let pointer = seat.get_pointer(&qh, ());

    let _confined: ZwpConfinedPointerV1 =
        constraints.confine_pointer(&surface, &pointer, None, Lifetime::Oneshot, &qh, ());

    eq.roundtrip(&mut state)
        .expect("confine_pointer round-trip raised a protocol error");
}

/// Two constraints on the same (surface, pointer) pair must be rejected with
/// the protocol's `already_constrained` error — confirming the request reaches
/// Smithay's real constraint bookkeeping, not a stub.
#[test]
#[ignore]
fn double_lock_is_already_constrained_error() {
    let Some(_display) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_bind();
    let qh = eq.handle();

    let compositor = state.compositor.clone().expect("no wl_compositor");
    let seat = state.seat.clone().expect("no wl_seat");
    let constraints = state.constraints.clone().expect("no pointer-constraints");

    let surface = compositor.create_surface(&qh, ());
    let pointer = seat.get_pointer(&qh, ());

    let _a = constraints.lock_pointer(&surface, &pointer, None, Lifetime::Oneshot, &qh, ());
    let _b = constraints.lock_pointer(&surface, &pointer, None, Lifetime::Oneshot, &qh, ());

    let result = eq.roundtrip(&mut state);
    assert!(
        result.is_err(),
        "second constraint on the same surface+pointer should raise already_constrained"
    );
}

/// Getting a relative-pointer object from the manager must round-trip cleanly.
#[test]
#[ignore]
fn get_relative_pointer_roundtrips() {
    let Some(_display) = wayland_display_or_skip() else {
        return;
    };
    let (_conn, mut eq, mut state) = connect_and_bind();
    let qh = eq.handle();

    let seat = state.seat.clone().expect("no wl_seat");
    let relative = state.relative.clone().expect("no relative-pointer manager");

    let pointer = seat.get_pointer(&qh, ());
    let _rel: ZwpRelativePointerV1 = relative.get_relative_pointer(&pointer, &qh, ());

    eq.roundtrip(&mut state)
        .expect("get_relative_pointer round-trip raised a protocol error");
}
