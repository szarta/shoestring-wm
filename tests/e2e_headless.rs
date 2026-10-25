//! End-to-end tests for the native headless backend and remote sessions.
//!
//! Each test starts its own `shoestring-wm --backend headless` with software
//! rendering in a private runtime dir, maps a test client that changes colour
//! when it gets input, and checks pixels: on the output itself (the `get_pixel`
//! probe), and as a remote viewer would see them, through a real
//! `shoestring-remote-server` speaking the `shoestring-remote` wire protocol.
//!
//! `#[ignore]`: they need Mesa's software EGL and the remote server binary, so
//! run them with the harness, which builds both binaries first:
//!
//!   scripts/headless-e2e-tests.sh
//!
//! or by hand:
//!
//!   cargo build -p shoestring-wm -p shoestring-remote-server
//!   cargo test -p shoestring-wm --test e2e_headless -- --ignored
//!
//! Every test name starts with `e2e_`, so the winit integration harness can
//! skip them.

mod common;

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::*;
use serde_json::json;
use shoestring_remote::ClientMessage;

// ── Tests ──────────────────────────────────────────────────────────────────────

/// The headless backend renders a client and delivers injected input to it.
#[test]
#[ignore]
fn e2e_headless_renders_and_delivers_input() {
    let wm = Wm::start();
    let output = wm.ipc_ok(json!({"type": "outputs"}));
    let head = &output["outputs"][0];
    assert_eq!(head["name"], "HEADLESS-1", "outputs: {output}");

    let _client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.wait_pixel(centre, rgb(IDLE), "mapped window");
    wm.wait_pixel((5, 5), BACKGROUND, "background");

    wm.ipc_ok(json!({"type": "inject_click", "button": "left", "x": centre.0, "y": centre.1}));
    wm.park_pointer();
    wm.wait_pixel(centre, rgb(CLICKED), "after a click");

    // The click focused the window, so a key goes to it.
    wm.ipc_ok(json!({"type": "inject_key", "keysym": "a"}));
    wm.wait_pixel(centre, rgb(KEYED), "after a key");
}

/// A `wait_window` sent before the client starts resolves only once the
/// window has its first buffer and its place, not on the earlier commit that
/// set its title, so a click at the reported geometry lands on the window.
#[test]
#[ignore]
fn e2e_wait_window_returns_a_placed_window() {
    let wm = Wm::start();
    let mut wait = wm.connect_ipc();
    let request =
        json!({"type": "wait_window", "title": format!("^{TITLE}$"), "timeout_ms": 10_000});
    writeln!(wait, "{request}").expect("write wait_window");

    let _client = TestClient::map(&wm);
    let reply = read_line(&mut wait);
    assert_eq!(reply["timed_out"], false, "{reply}");
    let g = &reply["window"]["geometry"];
    assert_eq!(
        (g["w"].as_u64(), g["h"].as_u64()),
        (Some(WINDOW_W as u64), Some(WINDOW_H as u64)),
        "wait_window resolved before the first buffer: {reply}"
    );
    let placed = wm.ipc_ok(json!({"type": "windows"}))["windows"][0]["geometry"].clone();
    assert_eq!(
        *g, placed,
        "wait_window resolved before the window was placed"
    );
}

/// A serve-mode viewer sees the output and drives the window with its input.
#[test]
#[ignore]
fn e2e_remote_serve_streams_and_drives() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let mut viewer = Viewer::serve(&wm);
    assert_eq!((viewer.width, viewer.height), (OUTPUT_W, OUTPUT_H));
    viewer.wait_pixel(centre, rgb(IDLE), "streamed window");
    viewer.wait_pixel((5, 5), BACKGROUND, "streamed background");
    wm.wait_viewers(1);

    viewer.click(centre);
    viewer.send(ClientMessage::PointerMotion {
        x: (OUTPUT_W - 1) as f64,
        y: (OUTPUT_H - 1) as f64,
    });
    viewer.wait_pixel(centre, rgb(CLICKED), "streamed window after a viewer click");
    viewer.key(KEY_A);
    viewer.wait_pixel(centre, rgb(KEYED), "streamed window after a viewer key");
    wm.wait_pixel(centre, rgb(KEYED), "output after viewer input");

    // An idle desktop sends nothing.
    viewer.frames_within(Duration::from_millis(300));
    assert_eq!(
        viewer.frames_within(Duration::from_millis(500)),
        0,
        "idle desktop still streaming"
    );

    viewer.bye();
    wm.wait_viewers(0);
}

/// A client from before the version exchange sends a `Hello` without version
/// fields; the server treats it as protocol 1 and serves it, and its `Ready`
/// carries the server's own version.
#[test]
#[ignore]
fn e2e_remote_serves_a_client_without_version_fields() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.start_remote_server();

    // Tag 0 (Hello), width, height, scale: the whole message, as 0.9.1 sends it.
    let mut hello = vec![0u8];
    hello.extend_from_slice(&OUTPUT_W.to_le_bytes());
    hello.extend_from_slice(&OUTPUT_H.to_le_bytes());
    hello.extend_from_slice(&1.0f64.to_le_bytes());
    let mut viewer = Viewer::connect_raw(&wm, &hello);
    viewer.wait_first_frame();
    viewer.wait_pixel(centre, rgb(IDLE), "streamed window");
    let server = viewer.server.clone().expect("Ready");
    assert_eq!(server, shoestring_remote::PeerInfo::ours());
}

/// A client on another protocol version is refused with a reason naming both
/// sides, before any session starts.
#[test]
#[ignore]
fn e2e_remote_refuses_another_protocol_version() {
    let mut wm = Wm::start();
    wm.start_remote_server();
    let mut viewer = Viewer::connect(
        &wm,
        ClientMessage::Hello {
            width: OUTPUT_W,
            height: OUTPUT_H,
            scale: 1.0,
            peer: shoestring_remote::PeerInfo {
                protocol: 99,
                capabilities: 0,
                software: "9.9.9".into(),
            },
        },
    );
    assert_eq!(viewer.wait_end(), End::Closed);
    let reason = viewer.refused.clone().expect("a Refused before the close");
    assert!(reason.contains("(9.9.9) speaks protocol 99"), "{reason}");
    assert!(reason.contains("upgrade the server"), "{reason}");
    assert_eq!(viewer.width, 0, "a session started anyway");
    wm.wait_viewers(0);
}

/// A message the server doesn't know (from a newer client) is skipped, and
/// the session carries on.
#[test]
#[ignore]
fn e2e_remote_skips_an_unknown_client_message() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    viewer.wait_pixel(centre, rgb(IDLE), "streamed window");

    viewer.send_raw(&[200, 1, 2, 3, 4]);
    viewer.click(centre);
    // Off the window, so the cursor sprite doesn't cover the probe.
    viewer.send(ClientMessage::PointerMotion {
        x: (OUTPUT_W - 1) as f64,
        y: (OUTPUT_H - 1) as f64,
    });
    viewer.wait_pixel(centre, rgb(CLICKED), "window after input that followed");
    wm.wait_viewers(1);
}

/// Viewers can leave and come back, and turning the gate off ends the session
/// and closes the listener until it is turned on again.
#[test]
#[ignore]
fn e2e_remote_reconnect_and_gate_revocation() {
    let mut wm = Wm::start();
    let client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    // Disconnect and reconnect: the new session starts with a full frame.
    let first = Viewer::serve(&wm);
    wm.wait_viewers(1);
    first.bye();
    wm.wait_viewers(0);
    client.paint(CLICKED);
    let mut second = Viewer::serve(&wm);
    second.wait_pixel(centre, rgb(CLICKED), "reconnected viewer");

    // Two viewers at once both see changes.
    let mut third = Viewer::serve(&wm);
    wm.wait_viewers(2);
    client.paint(KEYED);
    second.wait_pixel(centre, rgb(KEYED), "first of two viewers");
    third.wait_pixel(centre, rgb(KEYED), "second of two viewers");

    // Revoke: both sessions end, the listener goes away, and the coupled
    // automation gate turns off with it.
    wm.set_remote(false);
    second.wait_end();
    third.wait_end();
    assert!(
        UnixStream::connect(wm.remote_socket()).is_err(),
        "listener survived the gate"
    );
    // The server reports the count over its own connection, so it can land
    // just after the sessions end.
    wm.wait_viewers(0);
    let status = wm.remote_status();
    assert_eq!(status["enabled"], json!(false));
    // The server said where it listens when it registered.
    assert_eq!(
        status["endpoint"],
        json!(wm.remote_socket().display().to_string())
    );
    assert_eq!(
        wm.ipc(json!({"type": "automation_status"}))["enabled"],
        false
    );

    // Restore: a new viewer gets in again.
    wm.set_remote(true);
    let mut back = Viewer::serve(&wm);
    back.wait_pixel(centre, rgb(KEYED), "viewer after the gate came back");
    wm.wait_viewers(1);
}

/// Wait for the WM to turn the remote gate off by itself, then check it went
/// the whole way: listener gone, coupled automation gate off, no lease left.
fn wait_remote_off(wm: &Wm) {
    wait_for("the lease to turn the remote gate off", || {
        (wm.remote_status()["enabled"] == false).then_some(())
    });
    let socket = wm.remote_socket();
    wait_for("the listener to close", || (!socket.exists()).then_some(()));
    assert_eq!(
        wm.ipc(json!({"type": "automation_status"}))["enabled"],
        false
    );
    assert!(wm.remote_status().get("lease").is_none());
}

/// A time limit ends the gate with a viewer connected: the session ends as
/// if the gate had been turned off by hand.
#[test]
#[ignore]
fn e2e_remote_lease_time_limit_ends_a_session() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);

    let reply = wm.ipc_ok(json!({"type": "set_remote", "enabled": true, "for_secs": 1}));
    assert_eq!(reply["lease"], json!({"expires_in_secs": 1}), "{reply}");
    viewer.wait_end();
    wait_remote_off(&wm);
}

/// An idle lease waits while someone is connected, ends the gate once the
/// last viewer has been gone for the period, and ends it when nobody
/// connects at all.
#[test]
#[ignore]
fn e2e_remote_idle_lease_follows_the_viewers() {
    let mut wm = Wm::start();
    let client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);

    // Connected: the idle clock doesn't run, so the gate outlives the period.
    let reply = wm.ipc_ok(json!({"type": "set_remote", "enabled": true, "idle_secs": 1}));
    assert_eq!(reply["lease"], json!({"idle_secs": 1}), "{reply}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(wm.remote_status()["enabled"], true);
    client.paint(KEYED);
    viewer.wait_pixel(centre, rgb(KEYED), "the viewer, past the idle period");

    // The viewer leaves: the clock starts, and the gate goes.
    viewer.bye();
    wm.wait_viewers(0);
    wait_remote_off(&wm);

    // Nobody connects at all.
    wm.set_remote(true);
    wm.ipc_ok(json!({"type": "set_remote", "enabled": true, "idle_secs": 1}));
    wait_remote_off(&wm);

    // Turning the gate on again without a lease keeps it on.
    wm.set_remote(true);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(wm.remote_status()["enabled"], true);
}

/// A lease that can't mean anything is refused, and changes nothing.
#[test]
#[ignore]
fn e2e_remote_lease_refuses_nonsense() {
    let mut wm = Wm::start();
    wm.start_remote_server();
    for request in [
        json!({"type": "set_remote", "enabled": true, "for_secs": 0}),
        json!({"type": "set_remote", "enabled": true, "idle_secs": 0}),
        json!({"type": "set_remote", "enabled": false, "for_secs": 60}),
        json!({"type": "set_remote", "enabled": true, "for_secs": 30 * 86_400 + 1}),
        json!({"type": "set_remote", "enabled": true, "for_secs": u64::MAX}),
        json!({"type": "set_remote", "enabled": true, "idle_secs": u64::MAX}),
    ] {
        let reply = wm.ipc(request.clone());
        assert_eq!(reply["type"], "error", "{request} -> {reply}");
    }
    let status = wm.remote_status();
    assert_eq!(status["enabled"], true);
    assert!(status.get("lease").is_none(), "{status}");
}

/// [`shoestring_wm::capture_stream::LOCKED_FILL`] as RGB.
fn locked_fill() -> [u8; 3] {
    let [b, g, r, _] = shoestring_wm::capture_stream::LOCKED_FILL;
    [r, g, b]
}

/// While the session is locked, a serve viewer sees one static locked frame
/// (not the lock screen, and not the windows), even as the lock screen
/// animates, and a new graft session is refused.
#[test]
#[ignore]
fn e2e_remote_respects_the_session_lock() {
    let mut wm = Wm::start();
    let client = TestClient::map(&wm);
    let (id, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let mut viewer = Viewer::serve(&wm);
    viewer.wait_pixel(centre, rgb(IDLE), "window before the lock");
    let mut graft = Viewer::graft(&wm, &id);

    client.lock();
    viewer.wait_pixel(centre, locked_fill(), "static locked frame");
    viewer.wait_pixel((5, 5), locked_fill(), "static locked frame, corner");
    // The output itself shows the lock screen.
    wm.wait_pixel(centre, rgb(LOCK), "lock surface on the output");

    // The lock screen animates; the viewer gets nothing more.
    viewer.frames_within(Duration::from_millis(300));
    client.paint_lock(CLICKED);
    wm.wait_pixel(centre, rgb(CLICKED), "repainted lock surface on the output");
    assert_eq!(
        viewer.frames_within(Duration::from_secs(1)),
        0,
        "lock screen streamed"
    );
    client.paint_lock(LOCK);

    // A viewer that connects while locked gets the static frame too.
    let late = Viewer::serve(&wm);
    assert_eq!(
        late.pixel(centre),
        locked_fill(),
        "late viewer while locked"
    );
    drop(late);

    // Content changes behind the lock reach neither stream.
    graft.frames_within(Duration::from_millis(300));
    client.paint(CLICKED);
    assert_eq!(
        graft.frames_within(Duration::from_secs(1)),
        0,
        "graft streamed while locked"
    );
    viewer.frames_within(Duration::from_millis(500));
    assert_eq!(
        viewer.pixel(centre),
        locked_fill(),
        "window leaked through the lock"
    );

    // A new graft is refused: the server closes without a frame.
    let mut refused = Viewer::connect(
        &wm,
        ClientMessage::Graft {
            selector: id.clone(),
            peer: shoestring_remote::PeerInfo::ours(),
        },
    );
    refused.wait_end();
    assert_eq!(refused.width, 0, "a graft session started while locked");

    client.unlock();
    viewer.wait_pixel(centre, rgb(CLICKED), "window after unlock");
    graft.wait_pixel(
        ((WINDOW_W / 2) as i32, (WINDOW_H / 2) as i32),
        rgb(CLICKED),
        "graft after unlock",
    );
}

/// A graft streams one window even when it is minimized or on another
/// workspace, and input from the graft viewer reaches it.
#[test]
#[ignore]
fn e2e_remote_graft_hidden_windows() {
    let mut wm = Wm::start();
    let client = TestClient::map(&wm);
    let (id, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let inside = ((WINDOW_W / 2) as i32, (WINDOW_H / 2) as i32);

    // Minimized.
    wm.ipc_ok(json!({"type": "set_window_minimized", "id": id, "minimized": true}));
    wait_for("the window to unmap", || {
        matches!(wm.window(), Some((_, None))).then_some(())
    });
    wm.wait_pixel(centre, BACKGROUND, "output after minimize");
    let mut graft = Viewer::graft(&wm, &id);
    assert_eq!((graft.width, graft.height), (WINDOW_W, WINDOW_H));
    assert_eq!(graft.meta.as_ref().map(|(_, t)| t.as_str()), Some(TITLE));
    graft.wait_pixel(inside, rgb(IDLE), "graft of a minimized window");
    client.paint(CLICKED);
    graft.wait_pixel(inside, rgb(CLICKED), "minimized window repainted");
    graft.key(KEY_A);
    graft.wait_pixel(inside, rgb(KEYED), "graft key to a minimized window");
    graft.bye();

    // On another workspace.
    wm.ipc_ok(json!({"type": "set_window_minimized", "id": id, "minimized": false}));
    wm.mapped_window();
    wm.ipc_ok(json!({"type": "move_window_to_workspace", "id": id, "index": 2}));
    wait_for("the window to leave the workspace", || {
        matches!(wm.window(), Some((_, None))).then_some(())
    });
    wm.wait_pixel(centre, BACKGROUND, "output after the move");
    let mut graft = Viewer::graft(&wm, &id);
    graft.wait_pixel(inside, rgb(KEYED), "graft of an off-workspace window");
    graft.click(inside);
    graft.wait_pixel(
        inside,
        rgb(CLICKED),
        "graft click to an off-workspace window",
    );
    // The active workspace still shows only the background.
    wm.park_pointer();
    wm.wait_pixel(centre, BACKGROUND, "output while grafted");

    // Closing the window ends the graft.
    drop(client);
    assert_eq!(graft.wait_end(), End::Bye);
}

/// Context switches per second across all of `pid`'s threads, over `window`.
fn wakeups_per_sec(pid: u32, window: Duration) -> f64 {
    let switches = || -> u64 {
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .expect("read tasks")
            .flatten()
            .filter_map(|t| std::fs::read_to_string(t.path().join("status")).ok())
            .flat_map(|s| {
                s.lines()
                    .filter(|l| l.contains("ctxt_switches:"))
                    .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .collect::<Vec<_>>()
            })
            .sum()
    };
    let before = switches();
    std::thread::sleep(window);
    (switches() - before) as f64 / window.as_secs_f64()
}

/// With `[headless] idle_suspend`, an idle WM stops ticking, and a client
/// commit still reaches a viewer well inside the parked heartbeat.
#[test]
#[ignore]
fn e2e_headless_idle_suspend_parks_and_wakes() {
    let mut wm = Wm::start_with(WmOptions {
        idle_suspend: true,
        ..WmOptions::default()
    });
    let client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    viewer.wait_pixel(centre, rgb(IDLE), "window");

    // Idle: the 16 ms tick would be ~62 wakeups a second.
    std::thread::sleep(Duration::from_secs(1));
    let rate = wakeups_per_sec(wm.pid(), Duration::from_secs(3));
    assert!(rate < 15.0, "idle WM still wakes {rate:.0} times a second");

    // Woken by the commit itself (the viewer sends no IPC to the WM).
    for colour in [CLICKED, KEYED] {
        std::thread::sleep(Duration::from_millis(500));
        let start = std::time::Instant::now();
        client.paint(colour);
        viewer.wait_pixel(centre, rgb(colour), "repaint while parked");
        let latency = start.elapsed();
        assert!(
            latency < Duration::from_millis(300),
            "repaint took {latency:?} to reach the viewer"
        );
    }
}

/// `[general] idle_lock_secs` runs `lock_command` once nobody has given input
/// for that long. A remote viewer's input counts; a viewer that only watches
/// does not.
#[test]
#[ignore]
fn e2e_idle_lock_after_inactivity() {
    let marker = std::env::temp_dir().join(format!(
        "sw-e2e-locked-{}-{}",
        std::process::id(),
        next_id()
    ));
    let mut wm = Wm::start_with(WmOptions {
        idle_lock_secs: 2,
        lock_command: Some(format!("touch {}", marker.display())),
        ..WmOptions::default()
    });
    let _client = TestClient::map(&wm);
    wm.mapped_window();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);

    // Viewer input every half second for twice the idle period: no lock.
    for i in 0..8 {
        viewer.send(ClientMessage::PointerMotion {
            x: 10.0 + i as f64,
            y: 10.0,
        });
        viewer.frames_within(Duration::from_millis(500));
    }
    assert!(!marker.exists(), "locked while a viewer was giving input");

    // The viewer stays attached but goes quiet: the lock follows.
    let quiet = std::time::Instant::now();
    wait_for("the idle lock", || marker.exists().then_some(()));
    let after = quiet.elapsed();
    let _ = std::fs::remove_file(&marker);
    assert!(
        after >= Duration::from_millis(1500),
        "locked {after:?} after the last input, before the 2 s idle period"
    );
}

/// Send `lock` on a WM whose `lock_command` never locks, and return the
/// `lock_failed` event it broadcasts.
fn lock_and_expect_failure(lock_command: &str) -> serde_json::Value {
    let wm = Wm::start_with(WmOptions {
        lock_command: Some(lock_command.into()),
        ..WmOptions::default()
    });
    let mut events = wm.connect_ipc();
    writeln!(events, "{}", json!({"type": "event_stream"})).unwrap();
    assert_eq!(read_line(&mut events)["type"], "ok");
    wm.ipc_ok(json!({"type": "lock"}));
    let event = loop {
        let event = read_line(&mut events);
        if event["type"] == "lock_failed" {
            break event;
        }
    };
    let info = wm.ipc_ok(json!({"type": "server_info"}));
    assert_eq!(info["locked"], false, "{info}");
    event
}

/// A lock command that exits before locking leaves the session open; the WM
/// says so with `lock_failed`, carrying the exit status.
#[test]
#[ignore]
fn e2e_lock_command_exiting_unlocked_reports_failure() {
    let event = lock_and_expect_failure("false");
    assert_eq!(event["command"], "false", "{event}");
    assert_eq!(event["exit_code"], 1, "{event}");
    assert!(event["signal"].is_null(), "{event}");
}

/// A lock command that can't be spawned is reported the same way, with no
/// exit status.
#[test]
#[ignore]
fn e2e_lock_command_missing_reports_failure() {
    let event = lock_and_expect_failure("shoestring-no-such-locker");
    assert!(event["exit_code"].is_null(), "{event}");
    assert!(event["signal"].is_null(), "{event}");
}

/// After input while locked, a viewer sees the live lock screen (so it can
/// watch the password prompt) for `[general] lock_preview_secs`, then the
/// static frame again.
#[test]
#[ignore]
fn e2e_remote_lock_preview_after_input() {
    let mut wm = Wm::start_with(WmOptions {
        lock_preview_secs: Some(1),
        ..WmOptions::default()
    });
    let client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);

    // Locking alone (the input before it doesn't count) shows the static frame.
    client.lock();
    viewer.wait_pixel(centre, locked_fill(), "static frame after locking");

    // A viewer keypress opens the preview: the live lock screen, repaints included.
    let input = std::time::Instant::now();
    viewer.key(KEY_A);
    viewer.wait_pixel(centre, rgb(LOCK), "live lock screen after input");
    client.paint_lock(CLICKED);
    viewer.wait_pixel(
        centre,
        rgb(CLICKED),
        "lock screen repaint during the preview",
    );

    // A second without input closes it again.
    viewer.wait_pixel(centre, locked_fill(), "static frame after the preview");
    assert!(
        input.elapsed() >= Duration::from_secs(1),
        "preview closed {:?} after the input, before its 1 s",
        input.elapsed()
    );
}

/// Render one scene, a translucent gradient window plus the cursor, on
/// `renderer` at output `scale`, and return the settled output as a viewer
/// receives it.
fn capture_scene(renderer: &str, scale: f64) -> Vec<u8> {
    let mut wm = Wm::start_with(WmOptions {
        renderer: Some(renderer.into()),
        software: renderer != "gles",
        output_scale: Some(scale),
        ..WmOptions::default()
    });
    let client = TestClient::map(&wm);
    client.paint(PATTERN);
    wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let mut viewer = Viewer::serve(&wm);
    // Settled: half a second without a frame.
    while viewer.frames_within(Duration::from_millis(500)) > 0 {}
    viewer.framebuffer().to_vec()
}

/// The headless renderers draw the same scene alike. At scale 1 every pixel
/// maps to one texel and they agree to within rounding (byte for byte on the
/// Mesa this was written against). At a fractional scale filtering differs at
/// edges, so only a small share of channels may differ. Prints how far each
/// renderer is from pixman, so a difference shows even within the tolerance.
#[test]
#[ignore]
fn e2e_renderers_agree_on_pixels() {
    compare_renderers(1.0, |max, _| max <= 2);
    compare_renderers(1.5, |_, share_off| share_off < 0.01);
}

/// Compare every available renderer's capture against pixman's. `agree` gets
/// the largest channel difference and the share of channels off by more than
/// 2, and says whether that is close enough.
fn compare_renderers(scale: f64, agree: impl Fn(u8, f64) -> bool) {
    let mut renderers = vec!["pixman", "llvmpipe"];
    if (128..=130).any(|n| std::path::Path::new(&format!("/dev/dri/renderD{n}")).exists()) {
        renderers.push("gles");
    }
    let frames: Vec<(&str, Vec<u8>)> = renderers
        .iter()
        .map(|&r| (r, capture_scene(r, scale)))
        .collect();
    // The gradient must be on screen, or there is nothing to compare.
    for (name, frame) in &frames {
        let colours: std::collections::HashSet<&[u8]> = frame.chunks(4).collect();
        assert!(
            colours.len() > 1000,
            "{name} shows only {} colours: the gradient window is missing",
            colours.len()
        );
    }
    let (base_name, base) = &frames[0];
    for (name, frame) in &frames[1..] {
        assert_eq!(
            frame.len(),
            base.len(),
            "{name} streamed a different output size"
        );
        let (mut max, mut over) = (0u8, 0usize);
        for (a, b) in base.iter().zip(frame) {
            let d = a.abs_diff(*b);
            max = max.max(d);
            over += usize::from(d > 2);
        }
        eprintln!(
            "scale {scale}: {base_name} vs {name}: largest channel difference {max}, {over} of {} channels off by more than 2",
            base.len()
        );
        let share_off = over as f64 / base.len() as f64;
        assert!(
            agree(max, share_off),
            "scale {scale}: {base_name} and {name} differ by up to {max}, in {:.2}% of channels",
            share_off * 100.0
        );
    }
}

/// Input aimed at one window must not reach past the session lock: a
/// window-targeted drag is refused, a window-relative click meets the lock
/// screen, and a pointer pinned to a window by graft input before the lock
/// doesn't carry a press through it. All still work once unlocked.
#[test]
#[ignore]
fn e2e_window_targeted_input_respects_the_lock() {
    let wm = Wm::start();
    let client = TestClient::map(&wm);
    let (id, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.wait_pixel(centre, rgb(IDLE), "window before the lock");

    // Pin the pointer to the window the way graft input does, then lock.
    wm.ipc_ok(json!({"type": "inject_input_to_window", "id": id,
        "event": {"kind": "motion", "x": 20.0, "y": 20.0}}));
    client.lock();

    let drag = wm.ipc(json!({"type": "drag", "id": id, "button": "left",
        "from_x": 20.0, "from_y": 20.0, "to_x": 40.0, "to_y": 40.0}));
    assert_eq!(drag["type"], "error", "targeted drag while locked: {drag}");
    assert!(
        drag["message"]
            .as_str()
            .unwrap_or_default()
            .starts_with("session locked:"),
        "targeted drag while locked: {drag}"
    );
    wm.ipc_ok(
        json!({"type": "inject_click_to_window", "id": id, "button": "left",
        "wx": 20.0, "wy": 20.0}),
    );
    // A global press goes wherever the pointer's focus is.
    for pressed in [true, false] {
        wm.ipc_ok(json!({"type": "inject_input",
            "event": {"kind": "button", "button": 0x110, "pressed": pressed}}));
    }
    // A global drag is fine: it meets the lock screen.
    wm.ipc_ok(json!({"type": "drag", "button": "left",
        "from_x": centre.0, "from_y": centre.1, "to_x": centre.0 + 10, "to_y": centre.1}));
    std::thread::sleep(Duration::from_millis(300));

    client.unlock();
    wm.park_pointer();
    std::thread::sleep(Duration::from_millis(300));
    wm.wait_pixel(
        centre,
        rgb(IDLE),
        "window after the lock (no input got through)",
    );

    // Unlocked, the targeted drag works again.
    wm.ipc_ok(json!({"type": "drag", "id": id, "button": "left",
        "from_x": 20.0, "from_y": 20.0, "to_x": 40.0, "to_y": 40.0}));
    wm.park_pointer();
    wm.wait_pixel(centre, rgb(CLICKED), "targeted drag after unlock");
}

/// Send a `run_command` on its own connection and return it unread, with the
/// time it was sent, so the test can probe the WM while the command runs.
fn start_command(wm: &Wm, argv: &[&str], timeout_ms: u32) -> (UnixStream, std::time::Instant) {
    let mut stream = wm.connect_ipc();
    let request = json!({"type": "run_command", "argv": argv, "timeout_ms": timeout_ms});
    writeln!(stream, "{request}").expect("write run_command");
    (stream, std::time::Instant::now())
}

/// How long an unrelated IPC query takes to answer.
fn query_latency(wm: &Wm) -> Duration {
    let start = std::time::Instant::now();
    wm.ipc_ok(json!({"type": "outputs"}));
    start.elapsed()
}

/// A command that prints and then goes quiet must not park the event loop on
/// its output pipe: other IPC keeps being answered, and its timeout still
/// kills it on time. (`exec` makes the killed shell the process holding the
/// pipes; a descendant that keeps them open is a separate case.)
#[test]
#[ignore]
fn e2e_run_command_quiet_child_does_not_block_the_loop() {
    let wm = Wm::start();
    let (mut command, sent) =
        start_command(&wm, &["/bin/sh", "-c", "printf ready; exec sleep 5"], 300);

    std::thread::sleep(Duration::from_millis(100));
    let latency = query_latency(&wm);
    assert!(
        latency < Duration::from_millis(500),
        "outputs took {latency:?} while a command sat quiet"
    );

    let reply = read_line(&mut command);
    let took = sent.elapsed();
    assert_eq!(reply["type"], "command_result", "{reply}");
    assert_eq!(
        reply["exit_code"], -1,
        "the timeout should kill it: {reply}"
    );
    assert_eq!(reply["stdout"], "ready", "{reply}");
    assert!(
        took < Duration::from_secs(2),
        "a 300 ms timeout took {took:?} to answer"
    );
}

/// A command that prints without pause shares the loop: other IPC is still
/// answered promptly, and its output comes back capped.
#[test]
#[ignore]
fn e2e_run_command_chatty_child_shares_the_loop() {
    let wm = Wm::start();
    let (mut command, _) = start_command(&wm, &["yes"], 1500);

    std::thread::sleep(Duration::from_millis(200));
    for _ in 0..5 {
        let latency = query_latency(&wm);
        assert!(
            latency < Duration::from_millis(500),
            "outputs took {latency:?} while a command printed"
        );
    }

    let reply = read_line(&mut command);
    assert_eq!(reply["type"], "command_result", "{reply}");
    assert_eq!(reply["exit_code"], -1, "{reply}");
    assert_eq!(reply["truncated"], true, "{reply}");
}

/// evdev `KEY_LEFTMETA` (Super) and `KEY_L`.
const KEY_LEFTMETA: u32 = 125;
const KEY_L: u32 = 38;

/// Super+L to the next workspace, as in the default bindings. The test config
/// has to say so: a config file without bindings has none.
const SUPER_L_BINDING: &str = r#"
[[bindings]]
mods = ["Super"]
key = "l"
action = { type = "focus-workspace-relative", delta = 1 }
"#;

fn start_with_super_l() -> Wm {
    Wm::start_with(WmOptions {
        bindings: SUPER_L_BINDING,
        ..WmOptions::default()
    })
}

/// Super+L as raw transitions: Super down, L down and up, Super up.
fn super_l() -> [(u32, bool); 4] {
    [
        (KEY_LEFTMETA, true),
        (KEY_L, true),
        (KEY_L, false),
        (KEY_LEFTMETA, false),
    ]
}

fn active_workspace(wm: &Wm) -> u64 {
    let reply = wm.ipc_ok(json!({"type": "workspaces"}));
    reply["active"].as_u64().expect("active workspace")
}

/// Keys from a serve-mode viewer meet the served WM's own bindings: the
/// viewer is driving this whole desktop, so Super+L switches its workspace.
/// Automation's raw keys, without `bindings`, still go past the binding table.
#[test]
#[ignore]
fn e2e_remote_serve_keys_reach_bindings() {
    let mut wm = start_with_super_l();
    let _client = TestClient::map(&wm);
    wm.mapped_window();
    wm.start_remote_server();
    assert_eq!(active_workspace(&wm), 1);

    // Automation: the reply comes after the key is handled, so the workspace
    // would already have changed.
    for (keycode, pressed) in super_l() {
        wm.ipc_ok(json!({"type": "inject_input",
                         "event": {"kind": "key", "keycode": keycode, "pressed": pressed}}));
    }
    assert_eq!(active_workspace(&wm), 1, "automation keys ran a binding");

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    for (keycode, pressed) in super_l() {
        viewer.send(ClientMessage::Key { keycode, pressed });
    }
    wait_for("the viewer's Super+L to switch workspace", || {
        (active_workspace(&wm) == 2).then_some(())
    });
    viewer.bye();
}

/// A serve-mode viewer's mouse wheel over the bare desktop switches
/// workspaces, as a local wheel does. A scroll without wheel steps (a
/// touchpad's), a wheel over a window, and automation's wheel without
/// `bindings` don't.
#[test]
#[ignore]
fn e2e_remote_serve_wheel_switches_workspaces() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.start_remote_server();
    assert_eq!(active_workspace(&wm), 1);

    let wheel_up = ClientMessage::PointerAxis {
        horizontal: 0.0,
        vertical: -15.0,
        wheel: Some((0.0, -120.0)),
    };
    let wheel_down = ClientMessage::PointerAxis {
        horizontal: 0.0,
        vertical: 15.0,
        wheel: Some((0.0, 120.0)),
    };
    let desktop = ClientMessage::PointerMotion { x: 5.0, y: 5.0 };

    // Automation's raw wheel stays a plain scroll. The reply comes after it
    // is handled.
    wm.ipc_ok(json!({"type": "move_mouse", "x": 5, "y": 5}));
    wm.ipc_ok(json!({"type": "inject_input", "event": {"kind": "axis",
        "horizontal": 0.0, "vertical": -15.0,
        "wheel": {"horizontal": 0.0, "vertical": -120.0}}}));
    assert_eq!(active_workspace(&wm), 1, "automation's wheel switched");

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    viewer.send(desktop.clone());
    viewer.send(wheel_up.clone());
    wait_for("the viewer's wheel to switch workspace", || {
        (active_workspace(&wm) == 2).then_some(())
    });

    // The server forwards in order. Had the touchpad scroll (up) switched,
    // the wheel down after it would land on 2, not 1.
    viewer.send(ClientMessage::PointerAxis {
        horizontal: 0.0,
        vertical: -15.0,
        wheel: None,
    });
    viewer.send(wheel_down);
    wait_for("the wheel down to come back", || {
        (active_workspace(&wm) == 1).then_some(())
    });

    // Over a window the wheel goes to the window. Had it switched, the
    // desktop wheel after it would land on 3, not 2.
    viewer.send(ClientMessage::PointerMotion {
        x: centre.0 as f64,
        y: centre.1 as f64,
    });
    viewer.send(wheel_up.clone());
    viewer.send(desktop);
    viewer.send(wheel_up);
    wait_for("the desktop wheel after the window's", || {
        (active_workspace(&wm) == 2).then_some(())
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(active_workspace(&wm), 2);
    viewer.bye();
}

/// A graft viewer's keys go to the grafted window and never run the served
/// WM's bindings: that window is one among the viewer's local windows, and
/// the viewer's own WM owns the keys.
#[test]
#[ignore]
fn e2e_remote_graft_keys_skip_bindings() {
    let mut wm = start_with_super_l();
    let _client = TestClient::map(&wm);
    let (id, _) = wm.mapped_window();
    wm.start_remote_server();
    let inside = ((WINDOW_W / 2) as i32, (WINDOW_H / 2) as i32);

    let mut graft = Viewer::graft(&wm, &id);
    graft.wait_pixel(inside, rgb(IDLE), "grafted window");
    for (keycode, pressed) in super_l() {
        graft.send(ClientMessage::Key { keycode, pressed });
    }
    // The server forwards in order, so once this key has landed the chord
    // before it has been handled too.
    graft.key(KEY_A);
    graft.wait_pixel(inside, rgb(KEYED), "graft key after the chord");
    assert_eq!(active_workspace(&wm), 1, "a graft key ran a binding");
    graft.bye();
}

/// Send one raw key transition from a viewer.
fn viewer_key(viewer: &mut Viewer, keycode: u32, pressed: bool) {
    viewer.send(ClientMessage::Key { keycode, pressed });
}

/// Tap L from a viewer, then wait for the workspace it should reach.
fn viewer_tap_l(wm: &Wm, viewer: &mut Viewer, want: u64, what: &str) {
    viewer_key(viewer, KEY_L, true);
    viewer_key(viewer, KEY_L, false);
    wait_for(what, || (active_workspace(wm) == want).then_some(()));
}

/// Tap L through automation's raw keys with bindings on, as a keystroke on
/// the served machine would arrive: it runs Super+L only if Super is down.
fn automation_tap_l(wm: &Wm) {
    for pressed in [true, false] {
        wm.ipc_ok(json!({"type": "inject_input", "bindings": true,
                         "event": {"kind": "key", "keycode": KEY_L, "pressed": pressed}}));
    }
}

/// A viewer that drops mid-chord leaves nothing held: its Super is released,
/// so a later L is just L.
#[test]
#[ignore]
fn e2e_remote_hang_up_releases_held_keys() {
    let mut wm = start_with_super_l();
    let _client = TestClient::map(&wm);
    wm.mapped_window();
    wm.start_remote_server();

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    viewer_key(&mut viewer, KEY_LEFTMETA, true);
    viewer_tap_l(&wm, &mut viewer, 2, "Super+L while the viewer holds Super");

    viewer.hang_up();
    // The session releases what it held before it stops counting as a viewer.
    wm.wait_viewers(0);
    automation_tap_l(&wm);
    assert_eq!(
        active_workspace(&wm),
        2,
        "Super stayed held after a hang-up"
    );
}

/// One viewer leaving releases only what nobody else holds: a second viewer
/// still holding Super keeps it.
#[test]
#[ignore]
fn e2e_remote_hang_up_keeps_another_viewers_keys() {
    let mut wm = start_with_super_l();
    let _client = TestClient::map(&wm);
    wm.mapped_window();
    wm.start_remote_server();

    // One at a time: concurrent connects can report the viewer count out of
    // order, which is its own bug.
    let mut first = Viewer::serve(&wm);
    wm.wait_viewers(1);
    let mut second = Viewer::serve(&wm);
    wm.wait_viewers(2);
    viewer_key(&mut first, KEY_LEFTMETA, true);
    viewer_key(&mut second, KEY_LEFTMETA, true);
    viewer_tap_l(&wm, &mut first, 2, "Super+L with both viewers on Super");

    first.hang_up();
    wm.wait_viewers(1);
    viewer_tap_l(&wm, &mut second, 3, "Super+L after the other viewer left");

    second.hang_up();
    wm.wait_viewers(0);
    automation_tap_l(&wm);
    assert_eq!(
        active_workspace(&wm),
        3,
        "Super stayed held after both left"
    );
}

/// Closing the gate releases what viewers held even though injection is now
/// refused, and sessions after it reopens start clean.
#[test]
#[ignore]
fn e2e_remote_gate_off_releases_held_keys() {
    let mut wm = start_with_super_l();
    let _client = TestClient::map(&wm);
    wm.mapped_window();
    wm.start_remote_server();

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    viewer_key(&mut viewer, KEY_LEFTMETA, true);
    viewer_tap_l(&wm, &mut viewer, 2, "Super+L while the viewer holds Super");

    wm.set_remote(false);
    assert_eq!(viewer.wait_end(), End::Bye);
    wm.ipc_ok(json!({"type": "set_automation", "enabled": true}));
    automation_tap_l(&wm);
    assert_eq!(
        active_workspace(&wm),
        2,
        "Super stayed held after the gate closed"
    );

    // A new session holds and releases normally.
    wm.set_remote(true);
    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    viewer_key(&mut viewer, KEY_LEFTMETA, true);
    viewer_tap_l(
        &wm,
        &mut viewer,
        3,
        "Super+L from a session after the gate reopened",
    );
    viewer_key(&mut viewer, KEY_LEFTMETA, false);
    viewer.bye();
}

/// A test window as the `windows` list reports it.
struct ListedWindow {
    id: String,
    /// On-screen `(x, y, w, h)`.
    rect: (i32, i32, i32, i32),
    focused: bool,
}

/// The mapped test windows.
fn test_windows(wm: &Wm) -> Vec<ListedWindow> {
    let reply = wm.ipc_ok(json!({"type": "windows"}));
    reply["windows"]
        .as_array()
        .expect("windows")
        .iter()
        .filter(|w| w["title"] == TITLE)
        .filter_map(|w| {
            let g = &w["geometry"];
            let at = |k: &str| g[k].as_i64().map(|v| v as i32);
            Some(ListedWindow {
                id: w["id"].as_str()?.to_owned(),
                rect: (at("x")?, at("y")?, at("w")?, at("h")?),
                focused: w["focused"].as_bool()?,
            })
        })
        .collect()
}

fn rect_of(wm: &Wm, id: &str) -> (i32, i32, i32, i32) {
    test_windows(wm)
        .into_iter()
        .find(|w| w.id == id)
        .map(|w| w.rect)
        .expect("test window")
}

fn wait_focused(wm: &Wm, id: &str, what: &str) {
    wait_for(what, || {
        test_windows(wm)
            .iter()
            .any(|w| w.id == id && w.focused)
            .then_some(())
    });
}

/// Two windows: a large one mapped first, then a small one on top of its
/// centre, focused.
struct Stacked {
    _clients: [TestClient; 2],
    large: String,
    /// A point on the large window that the small one doesn't cover.
    on_large: (i32, i32),
    small: String,
    on_small: (i32, i32),
}

fn large_under_small(wm: &Wm) -> Stacked {
    let large = TestClient::map_sized(wm, (2 * WINDOW_W, 2 * WINDOW_H));
    wait_for("the large window to map", || {
        (test_windows(wm).len() == 1).then_some(())
    });
    let large_id = test_windows(wm)[0].id.clone();
    let small = TestClient::map(wm);
    let small_id = wait_for("the small window to map", || {
        test_windows(wm)
            .into_iter()
            .find(|w| w.id != large_id)
            .map(|w| w.id)
    });
    wait_focused(wm, &small_id, "the small window to take focus");
    let (lx, ly, _, _) = rect_of(wm, &large_id);
    let (sx, sy, sw, sh) = rect_of(wm, &small_id);
    let on_large = (lx + 10, ly + 10);
    assert!(
        on_large.0 < sx || on_large.1 < sy,
        "the small window covers the probe point"
    );
    Stacked {
        _clients: [large, small],
        large: large_id,
        on_large,
        small: small_id,
        on_small: (sx + sw / 2, sy + sh / 2),
    }
}

/// A serve-mode viewer's click focuses the window clicked, as a local click
/// would, so the viewer's next keys reach that window and not the one that
/// had focus. Automation's raw buttons, without `bindings`, leave focus alone.
#[test]
#[ignore]
fn e2e_remote_serve_click_moves_keyboard_focus() {
    let mut wm = Wm::start();
    let stacked = large_under_small(&wm);
    let (on_large, on_small) = (stacked.on_large, stacked.on_small);
    wm.park_pointer();
    wm.start_remote_server();

    // Automation: a raw press and release on the large window delivers the
    // button but keeps the small window focused.
    wm.ipc_ok(json!({"type": "inject_input",
                     "event": {"kind": "motion", "x": on_large.0, "y": on_large.1}}));
    for pressed in [true, false] {
        wm.ipc_ok(json!({"type": "inject_input",
                         "event": {"kind": "button", "button": BTN_LEFT, "pressed": pressed}}));
    }
    wm.park_pointer();
    wm.wait_pixel(on_large, rgb(CLICKED), "large window after a raw click");
    assert!(
        test_windows(&wm)
            .iter()
            .any(|w| w.id == stacked.small && w.focused),
        "a raw automation click moved focus"
    );
    wm.ipc_ok(json!({"type": "inject_click", "button": "left", "x": on_small.0, "y": on_small.1}));
    wm.park_pointer();

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    viewer.click(on_large);
    wait_focused(
        &wm,
        &stacked.large,
        "the viewer's click to focus the large window",
    );
    viewer.key(KEY_A);
    viewer.send(ClientMessage::PointerMotion {
        x: (OUTPUT_W - 1) as f64,
        y: (OUTPUT_H - 1) as f64,
    });
    // Only the focused window gets keys, so this is the key landing where
    // the viewer clicked.
    viewer.wait_pixel(on_large, rgb(KEYED), "large window after the viewer's key");
    viewer.bye();
}

/// A viewer drags a window with Super held: left moves it and right resizes
/// it, through the served WM's own grabs, and neither press reaches the app.
#[test]
#[ignore]
fn e2e_remote_serve_super_drag_moves_and_resizes() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (id, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();
    let (x, y, w, h) = rect_of(&wm, &id);

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    let drag = |viewer: &mut Viewer, button: u32, from: (i32, i32), by: (i32, i32)| {
        viewer_key(viewer, KEY_LEFTMETA, true);
        viewer.send(ClientMessage::PointerMotion {
            x: from.0 as f64,
            y: from.1 as f64,
        });
        viewer.send(ClientMessage::PointerButton {
            button,
            pressed: true,
        });
        for step in 1..=4 {
            viewer.send(ClientMessage::PointerMotion {
                x: (from.0 + by.0 * step / 4) as f64,
                y: (from.1 + by.1 * step / 4) as f64,
            });
        }
        viewer.send(ClientMessage::PointerButton {
            button,
            pressed: false,
        });
        viewer_key(viewer, KEY_LEFTMETA, false);
    };

    drag(&mut viewer, BTN_LEFT, centre, (40, 30));
    wait_for("Super+left drag to move the window", || {
        (rect_of(&wm, &id) == (x + 40, y + 30, w, h)).then_some(())
    });

    // Right-drag from near the bottom-right corner grows the window there.
    let (x, y, w, h) = rect_of(&wm, &id);
    drag(&mut viewer, BTN_RIGHT, (x + w - 10, y + h - 10), (30, 20));
    wait_for("Super+right drag to resize the window", || {
        let (_, _, nw, nh) = rect_of(&wm, &id);
        (nw > w && nh > h).then_some(())
    });
    // The app sees Super itself, as it would from a local keyboard, but no
    // button.
    wm.park_pointer();
    assert_ne!(
        wm.pixel((x + 20, y + 20)),
        rgb(CLICKED),
        "a Super+drag press reached the app"
    );
    viewer.bye();
}

/// While a picker is armed, a viewer's click picks the window under it and
/// never reaches the app.
#[test]
#[ignore]
fn e2e_remote_serve_click_answers_the_picker() {
    let mut wm = Wm::start();
    let _client = TestClient::map(&wm);
    let (id, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    let mut pick = wm.connect_ipc();
    wait_focused(&wm, &id, "the window to have focus before the pick");
    writeln!(pick, "{}", json!({"type": "pick_window"})).expect("write pick_window");
    // Arming the picker takes keyboard focus off the window.
    wait_for("the picker to arm", || {
        test_windows(&wm).iter().all(|w| !w.focused).then_some(())
    });
    viewer.click(centre);
    let reply = read_line(&mut pick);
    assert_eq!(reply["window"]["id"], id.as_str(), "{reply}");
    wm.park_pointer();
    assert_eq!(
        wm.pixel(centre),
        rgb(IDLE),
        "the picking click reached the app"
    );
    viewer.bye();
}

/// The "being viewed" count follows viewers that connect and leave all at
/// once: every change reaches the WM in order, so it ends on the true number.
#[test]
#[ignore]
fn e2e_remote_viewer_count_tracks_concurrent_sessions() {
    const N: u64 = 6;
    let mut wm = Wm::start();
    wm.start_remote_server();

    for _ in 0..3 {
        let viewers: Vec<Viewer> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..N).map(|_| s.spawn(|| Viewer::serve(&wm))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        wm.wait_viewers(N);
        std::thread::scope(|s| {
            for viewer in viewers {
                s.spawn(move || viewer.hang_up());
            }
        });
        wm.wait_viewers(0);
    }
}

/// Turning the gate off and straight back on: the old session's teardown
/// can't take the new viewer off the count.
#[test]
#[ignore]
fn e2e_remote_viewer_count_survives_a_gate_cycle() {
    let mut wm = Wm::start();
    wm.start_remote_server();

    let mut old = Viewer::serve(&wm);
    wm.wait_viewers(1);
    wm.set_remote(false);
    wm.set_remote(true);
    let new = Viewer::serve(&wm);
    assert_eq!(old.wait_end(), End::Bye);
    wm.wait_viewers(1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        wm.viewers(),
        1,
        "the old session's teardown changed the count"
    );
    new.bye();
    wm.wait_viewers(0);
}

/// A window whose frames of [`NOISE`] (170 KiB each) soon outgrow every
/// socket buffer between the WM and a viewer, yet stay cheap enough to encode
/// for every viewer of a debug build that the IPC latency measures backpressure,
/// not compression.
const NOISY_WINDOW: (u32, u32) = (240, 180);

/// Rounds of noise the tests stream: several MiB, well past what the buffers
/// hold.
const BACKLOG_ROUNDS: usize = 16;

/// The most sessions the remote server serves at once (its `MAX_SESSIONS`).
const SERVER_SESSIONS: usize = 8;

/// The longest an unrelated IPC query may take while viewers misbehave.
const RESPONSIVE: Duration = Duration::from_millis(500);

fn assert_responsive(wm: &Wm, when: &str) {
    let latency = query_latency(wm);
    assert!(latency < RESPONSIVE, "outputs took {latency:?} {when}");
}

/// Render [`BACKLOG_ROUNDS`] frames of noise, each confirmed on the output
/// before the next, so every viewer that isn't reading falls far behind.
/// Checks the WM stays responsive throughout, and ends on `last`.
fn back_up(wm: &Wm, client: &TestClient, centre: (i32, i32), last: u32) {
    for _ in 0..BACKLOG_ROUNDS {
        client.paint(IDLE);
        wm.wait_pixel(centre, rgb(IDLE), "a solid frame between the noise");
        client.paint(NOISE);
        wait_for("a frame of noise on the output", || {
            (wm.pixel(centre) != rgb(IDLE)).then_some(())
        });
        assert_responsive(wm, "while viewers fell behind");
    }
    client.paint(last);
    wm.wait_pixel(centre, rgb(last), "the final colour on the output");
}

/// A viewer that stops reading, or reads slowly, holds back only its own
/// session: the WM keeps answering IPC and keeps streaming to a viewer that
/// reads, the stalled one catches up with a full frame once it reads again
/// (without being sent every frame it missed), and the slow one gets there.
#[test]
#[ignore]
fn e2e_remote_stalled_and_slow_viewers_hold_back_only_themselves() {
    let mut wm = Wm::start();
    let client = TestClient::map_sized(&wm, NOISY_WINDOW);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let mut stalled = Viewer::serve(&wm);
    let mut slow = Viewer::serve(&wm);
    let mut reading = Viewer::serve(&wm);
    wm.wait_viewers(3);
    stalled.stop_reading();
    slow.read_slowly(Duration::from_millis(40));

    let colours = [CLICKED, KEYED];
    for (round, colour) in colours.iter().cycle().take(BACKLOG_ROUNDS).enumerate() {
        client.paint(NOISE);
        client.paint(*colour);
        reading.wait_pixel(
            centre,
            rgb(*colour),
            &format!("the reading viewer, round {round}"),
        );
        assert_responsive(&wm, "while a viewer was stalled and another slow");
    }
    let last = colours[(BACKLOG_ROUNDS - 1) % 2];

    slow.wait_pixel(centre, rgb(last), "the slow viewer");
    let before = stalled.frames;
    stalled.read_normally();
    stalled.wait_pixel(centre, rgb(last), "the stalled viewer once it reads");
    let caught_up_in = stalled.frames - before;
    assert!(
        caught_up_in < BACKLOG_ROUNDS,
        "the stalled viewer was sent {caught_up_in} frames to catch up; \
         the WM should skip what it can't take and resync"
    );
    wm.wait_viewers(3);
}

/// Turning the gate off ends every session, even those whose viewers stopped
/// reading with frames queued for them. None outlives the consent: a viewer
/// that reads again gets only what was captured before, then the end. And
/// none keeps one of the server's session slots, so once the gate reopens a
/// new viewer gets in although every slot was taken by a stalled one.
#[test]
#[ignore]
fn e2e_remote_gate_revocation_ends_stalled_sessions() {
    let mut wm = Wm::start();
    let client = TestClient::map_sized(&wm, NOISY_WINDOW);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let mut stalled: Vec<Viewer> = (0..SERVER_SESSIONS).map(|_| Viewer::serve(&wm)).collect();
    wm.wait_viewers(SERVER_SESSIONS as u64);
    for viewer in &stalled {
        viewer.stop_reading();
    }
    back_up(&wm, &client, centre, CLICKED);

    wm.set_remote(false);
    assert_responsive(&wm, "right after revoking a backed-up gate");
    wm.wait_viewers(0);
    // Painted after the revocation: no viewer may ever see it. (Closing the
    // remote gate closed the automation gate the pixel probe needs.)
    wm.ipc_ok(json!({"type": "set_automation", "enabled": true}));
    client.paint(KEYED);
    wm.wait_pixel(centre, rgb(KEYED), "the colour painted after revocation");

    wm.set_remote(true);
    let mut fresh = wait_for(
        "a new viewer to get a session after the gate reopened",
        || Viewer::try_serve(&wm),
    );
    fresh.wait_pixel(centre, rgb(KEYED), "the new viewer");
    wm.wait_viewers(1);

    for (i, viewer) in stalled.iter_mut().enumerate() {
        viewer.read_normally();
        viewer.wait_end();
        assert_ne!(
            viewer.pixel(centre),
            rgb(KEYED),
            "stalled viewer {i} saw a frame captured after the gate closed"
        );
    }
    assert_responsive(&wm, "after the stalled viewers drained");
    wm.wait_viewers(1);
}

/// Viewers connecting and leaving from several threads while the gate flips
/// on and off: the WM keeps answering throughout, and once it settles the
/// count is back to zero and a new viewer gets a working session.
#[test]
#[ignore]
fn e2e_remote_viewer_churn_across_gate_flips() {
    const CHURNERS: usize = 4;
    const FLIPS: usize = 10;
    let mut wm = Wm::start();
    let client = TestClient::map(&wm);
    let (_, centre) = wm.mapped_window();
    wm.park_pointer();
    wm.start_remote_server();

    let stop = std::sync::atomic::AtomicBool::new(false);
    let sessions = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for i in 0..CHURNERS {
            let (wm, stop, sessions) = (&wm, &stop, &sessions);
            s.spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    match Viewer::try_serve(wm) {
                        Some(viewer) => {
                            sessions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if i % 2 == 0 {
                                viewer.try_bye();
                            } else {
                                viewer.hang_up();
                            }
                        }
                        None => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
        }
        for _ in 0..FLIPS {
            std::thread::sleep(Duration::from_millis(100));
            wm.set_remote(false);
            assert_responsive(&wm, "just after the gate closed under churn");
            std::thread::sleep(Duration::from_millis(30));
            wm.set_remote(true);
            assert_responsive(&wm, "just after the gate opened under churn");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    assert!(
        sessions.into_inner() >= FLIPS,
        "too few sessions got going for the churn to mean anything"
    );

    wm.wait_viewers(0);
    let mut viewer = Viewer::serve(&wm);
    wm.wait_viewers(1);
    client.paint(CLICKED);
    viewer.wait_pixel(centre, rgb(CLICKED), "a viewer after the churn");
}

/// Whether `pid` is still a live process (not gone, not a zombie).
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The state is the field after the parenthesised command name.
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z')),
        Err(_) => false,
    }
}

/// Run a command to completion, returning its reply and how long it took.
fn run_command(wm: &Wm, argv: &[&str], timeout_ms: Option<u32>) -> (serde_json::Value, Duration) {
    let start = std::time::Instant::now();
    let mut request = json!({"type": "run_command", "argv": argv});
    if let Some(ms) = timeout_ms {
        request["timeout_ms"] = json!(ms);
    }
    let reply = wm.ipc(request);
    assert_eq!(reply["type"], "command_result", "{reply}");
    (reply, start.elapsed())
}

/// The first line of a command's stdout, as a pid.
fn printed_pid(reply: &serde_json::Value) -> u32 {
    let stdout = reply["stdout"].as_str().expect("stdout");
    stdout
        .lines()
        .next()
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!("no pid in {reply}");
        })
}

/// A timeout kills the command's whole process tree, not just the shell, so a
/// background process holding the output can't keep the request open.
#[test]
#[ignore]
fn e2e_run_command_timeout_kills_the_tree() {
    let wm = Wm::start();
    let (reply, took) = run_command(
        &wm,
        &["/bin/sh", "-c", "sleep 30 & echo $!; wait"],
        Some(300),
    );
    assert!(
        took < Duration::from_secs(3),
        "a 300 ms timeout took {took:?}"
    );
    assert_eq!(reply["exit_code"], -1, "{reply}");
    let pid = printed_pid(&reply);
    wait_for("the background sleep to die", || {
        (!process_alive(pid)).then_some(())
    });
}

/// A command that exits while something it started still holds its output
/// is answered shortly after it exits, and that leftover is killed.
#[test]
#[ignore]
fn e2e_run_command_leftover_holding_output_is_bounded() {
    let wm = Wm::start();
    let (reply, took) = run_command(&wm, &["/bin/sh", "-c", "sleep 30 & echo $!"], None);
    assert!(took < Duration::from_secs(3), "reply took {took:?}");
    assert_eq!(reply["exit_code"], 0, "{reply}");
    let pid = printed_pid(&reply);
    wait_for("the leftover sleep to die", || {
        (!process_alive(pid)).then_some(())
    });
}

/// A background process that doesn't hold the output is left running, as
/// before: the command was answered without waiting on it.
#[test]
#[ignore]
fn e2e_run_command_leaves_unrelated_background_alone() {
    let wm = Wm::start();
    let (reply, took) = run_command(
        &wm,
        &["/bin/sh", "-c", "sleep 30 >/dev/null 2>&1 & echo $!"],
        None,
    );
    let pid = printed_pid(&reply);
    let alive = process_alive(pid);
    // Clean up before asserting.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    assert!(took < Duration::from_secs(2), "reply took {took:?}");
    assert!(
        alive,
        "a background process that didn't hold the output was killed"
    );
}

/// A reply far bigger than the socket buffer arrives whole to a client that
/// reads it slowly, and the WM keeps answering others meanwhile. A clipboard
/// of a few hundred KiB used to come back cut off partway.
#[test]
#[ignore]
fn e2e_large_reply_reaches_a_slow_reader_whole() {
    use std::io::{BufRead, BufReader, Read};

    let wm = Wm::start();
    let data: Vec<u8> = (0..1024 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    wm.ipc_ok(json!({"type": "set_clipboard", "mime": "text/plain", "data": data}));

    let mut get = wm.connect_ipc();
    writeln!(get, "{}", json!({"type": "get_clipboard"})).expect("write get_clipboard");
    // Read a little at a time, with pauses, so the reply backs up in the WM.
    let mut reply = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(100));
        let n = get.read(&mut chunk).expect("read part of the reply");
        reply.extend_from_slice(&chunk[..n]);
        assert_responsive(&wm, "while a large reply backs up");
    }
    BufReader::new(get)
        .read_until(b'\n', &mut reply)
        .expect("read the rest of the reply");
    let reply: serde_json::Value = serde_json::from_slice(&reply).expect("reply is whole JSON");
    assert_eq!(reply["type"], "clipboard", "{reply}");
    let got: Vec<u8> = serde_json::from_value(reply["data"].clone()).expect("clipboard bytes");
    assert!(got == data, "the clipboard came back changed");
}
