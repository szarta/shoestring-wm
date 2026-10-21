//! Integration test: per-window capture must not see through a session lock.
//!
//! Whole-output capture renders the locked scene, but `capture_window` (graft)
//! and `screenshot_window` render an app's own surface tree offscreen. While an
//! `ext-session-lock-v1` lock is held, the WM must therefore pause graft streams
//! (no frames, subscription kept) and refuse new per-window captures with a
//! `session locked:` error, then resume the stream on unlock. Input aimed at
//! one window is refused the same way, since it would move keyboard focus off
//! the lock surface.
//!
//! The test maps a toplevel, opens a graft stream on it, takes the lock itself
//! (lock surfaces on every output), checks the refusals and the pause, unlocks,
//! and checks that the stream resumes. It drives the IPC socket, so it flips
//! the automation gate on for its duration and restores it afterwards.
//!
//! `#[ignore]` — it needs a live compositor, and it briefly locks the session
//! and maps a window, so run it against a throwaway WM (the harness does):
//!
//!   xvfb-run -a -s "-screen 0 1280x800x24" scripts/headless-integration-tests.sh

use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shoestring_remote::{read_framed, ServerMessage};
use wayland_client::{
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

const TITLE: &str = "shoestring session-lock capture test";

#[derive(Default)]
struct State {
    compositor: Option<WlCompositor>,
    wm_base: Option<XdgWmBase>,
    shm: Option<WlShm>,
    lock_manager: Option<ExtSessionLockManagerV1>,
    outputs: Vec<WlOutput>,
    /// The toplevel's xdg_surface has been configured (and acked).
    toplevel_configured: bool,
    /// Pending lock-surface configures: (surface index, serial, width, height).
    lock_configures: Vec<(usize, u32, u32, u32)>,
    locked: bool,
    lock_finished: bool,
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
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    state
                        .outputs
                        .push(registry.bind(name, version.min(3), qh, ()));
                }
                "ext_session_lock_manager_v1" => {
                    state.lock_manager = Some(registry.bind(name, version.min(1), qh, ()));
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

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.toplevel_configured = true;
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_session_lock_v1::Event::Locked => state.locked = true,
            ext_session_lock_v1::Event::Finished => state.lock_finished = true,
            _ => {}
        }
    }
}

// The user data is the lock surface's index, so a configure can be matched to
// the wl_surface it belongs to.
impl Dispatch<ExtSessionLockSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.lock_configures.push((*index, serial, width, height));
        }
    }
}

// These objects are inert from the client side.
macro_rules! inert {
    ($($ty:ty => $ev:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for State {
            fn event(_: &mut Self, _: &$ty, _: $ev, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
inert!(
    WlCompositor => wayland_client::protocol::wl_compositor::Event,
    WlSurface => wayland_client::protocol::wl_surface::Event,
    WlShm => wl_shm::Event,
    WlShmPool => wayland_client::protocol::wl_shm_pool::Event,
    WlBuffer => wl_buffer::Event,
    WlOutput => wl_output::Event,
    XdgToplevel => xdg_toplevel::Event,
    ExtSessionLockManagerV1 => wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::Event,
);

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

/// A solid-colour `width`×`height` ARGB8888 shm buffer, backed by an unlinked
/// file in `$XDG_RUNTIME_DIR` (the pool keeps its own reference to the fd).
fn solid_buffer(
    state: &State,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
    argb: u32,
) -> WlBuffer {
    let shm = state.shm.as_ref().expect("wl_shm advertised");
    let stride = width * 4;
    let len = (stride * height) as usize;
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::Path::new(&dir).join(format!(
        "shoestring-lock-test-{}-{argb:08x}-{width}x{height}",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("create shm file");
    std::fs::remove_file(&path).expect("unlink shm file");
    let pixels: Vec<u8> = argb.to_le_bytes().repeat(len / 4);
    file.write_all(&pixels).expect("fill shm file");
    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    buffer
}

/// The IPC socket of the WM behind `$WAYLAND_DISPLAY`, the one this test's
/// Wayland connection talks to. Deliberately not `$SHOESTRING_WM_SOCKET`: run
/// from inside a session that names the live desktop, and the test would lock
/// the nested WM but query and flip gates on the real one.
fn ipc_socket() -> UnixStream {
    let path =
        shoestring_ipc::default_socket_path().expect("XDG_RUNTIME_DIR / WAYLAND_DISPLAY unset");
    UnixStream::connect(&path).unwrap_or_else(|e| panic!("connect {}: {e}", path.display()))
}

/// Read one newline-terminated line byte by byte, so nothing past it (a graft
/// stream's binary frames) is consumed.
fn read_line(stream: &mut UnixStream) -> Value {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read IPC reply");
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    serde_json::from_slice(&line).expect("IPC reply is JSON")
}

/// One request, one reply, on a fresh connection.
fn ipc(request: Value) -> Value {
    let mut stream = ipc_socket();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(stream, "{request}").expect("write IPC request");
    read_line(&mut stream)
}

fn window_id(title: &str) -> Option<String> {
    let reply = ipc(json!({"type": "windows"}));
    reply["windows"]
        .as_array()?
        .iter()
        .find(|w| w["title"] == title)
        .and_then(|w| w["id"].as_str().map(str::to_owned))
}

fn assert_session_locked(reply: &Value, request: &str) {
    assert_eq!(reply["type"], "error", "{request} while locked: {reply}");
    let message = reply["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("session locked:"),
        "{request} while locked should be refused with `session locked:`, got {message:?}"
    );
}

/// Count graft `Frame`s that arrive within `window`. Other messages (`Ready`,
/// `Meta`, `Resize`) are skipped; a `Bye` or a closed stream fails the test.
fn frames_within(stream: &mut UnixStream, window: Duration) -> usize {
    let deadline = Instant::now() + window;
    let mut frames = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return frames;
        }
        stream.set_read_timeout(Some(left)).unwrap();
        match read_framed::<_, ServerMessage>(stream) {
            Ok(ServerMessage::Frame { .. }) => frames += 1,
            Ok(ServerMessage::Bye) => panic!("graft stream ended with Bye"),
            Ok(_) => {}
            Err(shoestring_remote::Error::Io(io))
                if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return frames;
            }
            Err(e) => panic!("graft stream read failed: {e:?}"),
        }
    }
}

/// Dispatch Wayland events until `done` holds, or panic after `timeout`.
fn dispatch_until(
    conn: &Connection,
    eq: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
    what: &str,
    done: impl Fn(&State) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while !done(state) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        conn.flush().expect("flush");
        eq.roundtrip(state).expect("roundtrip");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ── Test ───────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn window_capture_pauses_and_refuses_while_locked() {
    if !wayland_display_or_skip() {
        return;
    }
    let conn = Connection::connect_to_env().expect("connect via WAYLAND_DISPLAY");
    let mut eq = conn.new_event_queue::<State>();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    eq.roundtrip(&mut state).expect("registry roundtrip");
    let (Some(compositor), Some(wm_base), Some(lock_manager)) = (
        state.compositor.clone(),
        state.wm_base.clone(),
        state.lock_manager.clone(),
    ) else {
        panic!("required globals (compositor / xdg_wm_base / ext_session_lock_manager_v1) absent");
    };
    assert!(!state.outputs.is_empty(), "no wl_output advertised");

    // Map a toplevel with real content, so it renders and can be captured.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(TITLE.into());
    surface.commit();
    dispatch_until(
        &conn,
        &mut eq,
        &mut state,
        Duration::from_secs(5),
        "toplevel configure",
        |s| s.toplevel_configured,
    );
    surface.attach(Some(&solid_buffer(&state, &qh, 64, 64, 0xff20_60c0)), 0, 0);
    surface.damage_buffer(0, 0, 64, 64);
    surface.commit();
    conn.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let id = loop {
        eq.roundtrip(&mut state).unwrap();
        if let Some(id) = window_id(TITLE) {
            break id;
        }
        assert!(
            Instant::now() < deadline,
            "test window never appeared in `windows`"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    let automation_was = ipc(json!({"type": "automation_status"}))["enabled"] == true;
    ipc(json!({"type": "set_automation", "enabled": true}));

    // Unlocked baseline: the per-window paths work.
    let shot_path =
        std::env::temp_dir().join(format!("shoestring-lock-test-{}.png", std::process::id()));
    let shot = ipc(json!({"type": "screenshot_window", "id": id, "path": shot_path}));
    assert_eq!(
        shot["type"], "screenshot",
        "unlocked screenshot_window: {shot}"
    );
    let _ = std::fs::remove_file(&shot_path);

    let mut graft = ipc_socket();
    graft
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    writeln!(graft, "{}", json!({"type": "capture_window", "id": id})).unwrap();
    assert_eq!(
        read_line(&mut graft)["type"],
        "ok",
        "capture_window subscribe"
    );
    assert!(
        frames_within(&mut graft, Duration::from_secs(5)) > 0,
        "graft stream sent no initial frame"
    );

    // Lock: one lock surface per output, each with a buffer, so the WM confirms.
    let lock = lock_manager.lock(&qh, ());
    let lock_surfaces: Vec<(WlSurface, ExtSessionLockSurfaceV1)> = state
        .outputs
        .clone()
        .iter()
        .enumerate()
        .map(|(i, output)| {
            let s = compositor.create_surface(&qh, ());
            let ls = lock.get_lock_surface(&s, output, &qh, i);
            (s, ls)
        })
        .collect();
    let outputs = lock_surfaces.len();
    dispatch_until(
        &conn,
        &mut eq,
        &mut state,
        Duration::from_secs(5),
        "lock surface configures",
        |s| s.lock_configures.len() >= outputs || s.lock_finished,
    );
    assert!(!state.lock_finished, "the WM refused the session lock");
    for (index, serial, width, height) in std::mem::take(&mut state.lock_configures) {
        let (s, ls) = &lock_surfaces[index];
        ls.ack_configure(serial);
        let (w, h) = (width.max(1), height.max(1));
        s.attach(Some(&solid_buffer(&state, &qh, w, h, 0xff00_0000)), 0, 0);
        s.damage_buffer(0, 0, w as i32, h as i32);
        s.commit();
    }
    dispatch_until(
        &conn,
        &mut eq,
        &mut state,
        Duration::from_secs(5),
        "`locked`",
        |s| s.locked,
    );

    // Drain whatever was in flight from before the lock took effect.
    frames_within(&mut graft, Duration::from_millis(500));

    // New content while locked must not reach the stream.
    surface.attach(Some(&solid_buffer(&state, &qh, 64, 64, 0xffc0_4020)), 0, 0);
    surface.damage_buffer(0, 0, 64, 64);
    surface.commit();
    eq.roundtrip(&mut state).unwrap();
    let leaked = frames_within(&mut graft, Duration::from_secs(1));

    let shot_locked = ipc(json!({"type": "screenshot_window", "id": id, "path": shot_path}));
    let graft_locked = ipc(json!({"type": "capture_window", "id": id}));
    let input_locked = ipc(json!({
        "type": "inject_input_to_window",
        "id": id,
        "event": {"kind": "motion", "x": 1.0, "y": 1.0},
    }));
    let _ = std::fs::remove_file(&shot_path);

    // Unlock before asserting, so a failure doesn't leave the session locked.
    lock.unlock_and_destroy();
    for (s, ls) in lock_surfaces {
        ls.destroy();
        s.destroy();
    }
    eq.roundtrip(&mut state).unwrap();

    assert_eq!(
        leaked, 0,
        "graft stream sent {leaked} frame(s) while locked"
    );
    assert_session_locked(&shot_locked, "screenshot_window");
    assert_session_locked(&graft_locked, "capture_window");
    assert_session_locked(&input_locked, "inject_input_to_window");

    // The paused stream resumes with the change committed during the lock.
    assert!(
        frames_within(&mut graft, Duration::from_secs(5)) > 0,
        "graft stream did not resume after unlock"
    );

    drop(graft);
    if !automation_was {
        ipc(json!({"type": "set_automation", "enabled": false}));
    }
    toplevel.destroy();
    xdg_surface.destroy();
    surface.destroy();
    eq.roundtrip(&mut state).unwrap();
}
