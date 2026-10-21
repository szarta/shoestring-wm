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
