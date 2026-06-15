//! Integration tests for the `zwp_idle_inhibit_manager_v1` protocol — the
//! screensaver-inhibition path video players use so the screen doesn't blank
//! or lock during playback.
//!
//! These connect to a running shoestring-wm compositor as a Wayland client and
//! verify the wiring end to end: the manager global is advertised, and an
//! inhibitor on a *visible* toplevel actually flips the compositor's idle state
//! — observed through the `wm.idle_inhibited` diagnostics gauge over the IPC
//! socket (the only externally observable signal for an internal flag).
//!
//! Both tests are `#[ignore]` — they require a live compositor *with idle
//! enabled* (`[general].idle_notifications_enabled = true`), since the inhibit
//! manager is advertised only alongside the idle notifier. Run them with:
//!
//!   WAYLAND_DISPLAY=wayland-1 cargo test --test idle_inhibit -- --ignored
//!
//! When idle is disabled (the default) the manager global is absent; the tests
//! detect that and skip rather than fail. `visible_inhibitor_flips_idle_state`
//! briefly maps and destroys a real toplevel, so run it deliberately. Task #99
//! (headless/winit CI) would run these unattended against a test compositor.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use shoestring_ipc::{client_socket_path, MetricValue, Request, Response};
use wayland_client::{
    protocol::{
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_surface::WlSurface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
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
    inhibit_manager: Option<ZwpIdleInhibitManagerV1>,
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
                "zwp_idle_inhibit_manager_v1" => {
                    state.inhibit_manager =
                        Some(registry.bind::<ZwpIdleInhibitManagerV1, _, _>(name, 1, qh, ()));
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

// These objects are inert from the client side; their events (if any) are
// irrelevant to what we assert.
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

impl Dispatch<ZwpIdleInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpIdleInhibitManagerV1,
        _: wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpIdleInhibitorV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpIdleInhibitorV1,
        _: wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::Event,
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

/// Read one gauge from the WM's diagnostics registry over the IPC socket.
/// `None` if the socket can't be reached or the gauge isn't present (the
/// latter means idle is disabled, so the inhibit subsystem is off).
fn read_gauge(name: &str) -> Option<i64> {
    let path = client_socket_path()?;
    let mut stream = UnixStream::connect(path).ok()?;
    let req = serde_json::to_string(&Request::Metrics).ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let resp: Response = serde_json::from_str(line.trim()).ok()?;
    match resp {
        Response::Metrics { metrics, .. } => match metrics.get(name)? {
            MetricValue::Gauge { value } => Some(*value),
            _ => None,
        },
        _ => None,
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

/// The manager global must be advertised (when idle is enabled) and bind
/// cleanly. Non-mutating; safe against any session.
#[test]
#[ignore]
fn manager_global_advertised() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, _eq, state) = connect();
    if state.inhibit_manager.is_none() {
        eprintln!(
            "zwp_idle_inhibit_manager_v1 not advertised — idle is disabled \
             ([general].idle_notifications_enabled = false). Skipping."
        );
    }
}

/// An inhibitor on a *visible* toplevel must flip `wm.idle_inhibited` to 1,
/// and destroying it must flip it back to 0. Maps a real (buffer-less)
/// toplevel — the WM maps on `get_toplevel`, so it counts as visible — then
/// observes the compositor's idle state through the diagnostics gauge.
#[test]
#[ignore]
fn visible_inhibitor_flips_idle_state() {
    if !wayland_display_or_skip() {
        return;
    }
    let (_conn, mut eq, mut state) = connect();
    let qh = eq.handle();

    let (Some(compositor), Some(wm_base), Some(manager)) = (
        state.compositor.clone(),
        state.wm_base.clone(),
        state.inhibit_manager.clone(),
    ) else {
        eprintln!("idle inhibit manager absent — idle disabled. Skipping.");
        return;
    };
    if read_gauge("wm.idle_inhibited").is_none() {
        eprintln!("wm.idle_inhibited gauge absent — diagnostics or idle off. Skipping.");
        return;
    }

    // Map a toplevel. The WM inserts it into the space at `get_toplevel`, so
    // it's "visible" without ever attaching a buffer.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("shoestring idle-inhibit test".into());
    surface.commit();
    eq.roundtrip(&mut state).expect("map roundtrip failed");

    // No inhibitor yet → not inhibited.
    assert_eq!(
        read_gauge("wm.idle_inhibited"),
        Some(0),
        "idle should not be inhibited before any inhibitor is created"
    );

    // Inhibit on the visible surface → inhibited.
    let inhibitor = manager.create_inhibitor(&surface, &qh, ());
    eq.roundtrip(&mut state).expect("inhibit roundtrip failed");
    assert_eq!(
        read_gauge("wm.idle_inhibited"),
        Some(1),
        "idle should be inhibited while a visible surface holds an inhibitor"
    );
    assert_eq!(
        read_gauge("wm.idle_inhibitors"),
        Some(1),
        "exactly one inhibitor surface should be tracked"
    );

    // Drop the inhibitor → back to not inhibited.
    inhibitor.destroy();
    eq.roundtrip(&mut state)
        .expect("uninhibit roundtrip failed");
    assert_eq!(
        read_gauge("wm.idle_inhibited"),
        Some(0),
        "idle should resume once the inhibitor is destroyed"
    );

    // Tidy up the test window.
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    eq.roundtrip(&mut state).expect("teardown roundtrip failed");
}
