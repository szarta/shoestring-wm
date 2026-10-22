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
    let status = wm.remote_status();
    assert_eq!(
        (status["enabled"].clone(), status["viewers"].clone()),
        (json!(false), json!(0))
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
