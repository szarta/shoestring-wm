//! Integration tests for the `zwp_keyboard_shortcuts_inhibit_v1` protocol —
//! the path a focused client (a nested compositor, a VM/SPICE window, a
//! remote-desktop viewer, a full-screen game) uses to ask the WM to stop
//! eating its keybinds so every key reaches it.
//!
//! These connect to a running shoestring-wm compositor as a Wayland client and
//! verify the wiring end to end: the manager global is advertised, and an
//! inhibitor created on a *focused* toplevel is actually activated by the WM —
//! observed directly through the inhibitor's own `active` event (unlike idle
//! inhibit, whose state is only visible via the diagnostics gauge, this
//! protocol reports activation to the client).
//!
//! Both tests are `#[ignore]` — they need a live compositor, since the headless
//! WLCS build injects clients directly and has no focus-following render loop.
//! Run them against a nested or real session with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test keyboard_shortcuts_inhibit -- --ignored
//!
//! `inhibitor_activates_on_focused_surface` briefly maps and destroys a real
//! toplevel, so run it deliberately. Task #99 (headless/winit CI) would run
//! these unattended against a test compositor.

use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

#[derive(Default)]
struct State {
    compositor: Option<WlCompositor>,
    wm_base: Option<XdgWmBase>,
    seat: Option<WlSeat>,
    inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    /// Set by the inhibitor's `active` / `inactive` events so a test can assert
    /// the WM actually armed it. `None` until the WM reports a decision.
    active: Option<bool>,
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
                "wl_compositor" => {
                    state.compositor =
                        Some(registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<XdgWmBase, _, _>(name, version.min(3), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ()));
                }
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    state.inhibit_manager =
                        Some(registry.bind::<ZwpKeyboardShortcutsInhibitManagerV1, _, _>(
                            name,
                            1,
                            qh,
                            (),
                        ));
                }
                _ => {}
            }
        }
    }
}

// xdg_wm_base must be ponged or the server eventually disconnects us.
impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

// Ack every xdg_surface configure so the surface is well-behaved.
impl Dispatch<XdgSurface, ()> for State {
    fn event(
        _: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        _: &mut Self,
        _: &XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// These objects are inert from the client side.
impl Dispatch<WlCompositor, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: wayland_client::protocol::wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: wayland_client::protocol::wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitManagerV1,
        _: wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// The inhibitor reports its activation state to us; record the WM's decision
// on the shared `State` so the test can assert on it.
impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => state.active = Some(true),
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => state.active = Some(false),
            _ => {}
        }
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

/// The manager global is always advertised (unlike idle inhibit it has no
/// companion protocol to gate on) and must bind cleanly. Non-mutating.
#[test]
#[ignore]
fn manager_global_advertised() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, _eq, state) = connect();
    assert!(
        state.inhibit_manager.is_some(),
        "zwp_keyboard_shortcuts_inhibit_manager_v1 should always be advertised"
    );
}

/// An inhibitor created on the *focused* toplevel must be activated by the WM,
/// which the client learns via the inhibitor's `active` event. The WM
/// auto-focuses newly mapped windows, so the test toplevel holds focus by the
/// time the inhibitor is created — no buffer or input synthesis needed.
#[test]
#[ignore]
fn inhibitor_activates_on_focused_surface() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, mut eq, mut state) = connect();
    let qh = eq.handle();

    let (Some(compositor), Some(wm_base), Some(seat), Some(manager)) = (
        state.compositor.clone(),
        state.wm_base.clone(),
        state.seat.clone(),
        state.inhibit_manager.clone(),
    ) else {
        panic!("required globals (compositor / xdg_wm_base / seat / inhibit manager) absent");
    };

    // Map a toplevel. The WM inserts it into the space and auto-focuses it at
    // `get_toplevel`, so it holds keyboard focus without ever attaching a
    // buffer.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("shoestring shortcuts-inhibit test".into());
    surface.commit();
    eq.roundtrip(&mut state).expect("map roundtrip failed");

    // Inhibit shortcuts on the focused surface. Because it holds focus, the WM
    // arms the inhibitor immediately and sends `active`.
    let _inhibitor = manager.inhibit_shortcuts(&surface, &seat, &qh, ());
    eq.roundtrip(&mut state).expect("inhibit roundtrip failed");

    assert_eq!(
        state.active,
        Some(true),
        "inhibitor on the focused surface should be activated by the WM"
    );

    // Tidy up the test window.
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    eq.roundtrip(&mut state).expect("teardown roundtrip failed");
}
