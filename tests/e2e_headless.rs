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

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shoestring_remote::{
    apply_tile, framebuffer_len, read_framed, write_framed, ClientMessage, ServerMessage,
};
use wayland_client::{
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
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

// ── Fixture constants ──────────────────────────────────────────────────────────

const OUTPUT_W: u32 = 640;
const OUTPUT_H: u32 = 480;
/// `[background] color`, set explicitly so the test doesn't track the default.
const BACKGROUND: [u8; 3] = [0x10, 0x20, 0x30];
const WINDOW_W: u32 = 200;
const WINDOW_H: u32 = 150;
const TITLE: &str = "shoestring e2e client";

/// The client's colours, as ARGB8888: at map, after a pointer button press,
/// after a key press. The lock surface gets its own.
const IDLE: u32 = 0xff20_60c0;
const CLICKED: u32 = 0xffc0_4020;
const KEYED: u32 = 0xff20_c060;
const LOCK: u32 = 0xff60_2080;

const BTN_LEFT: u32 = 0x110;
/// evdev `KEY_A`.
const KEY_A: u32 = 30;

/// How long any single expectation may take before the test fails.
const TIMEOUT: Duration = Duration::from_secs(10);

fn rgb(argb: u32) -> [u8; 3] {
    [(argb >> 16) as u8, (argb >> 8) as u8, argb as u8]
}

/// Poll `probe` until it returns `Some`, or panic naming `what`.
fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ── The WM under test ──────────────────────────────────────────────────────────

/// A headless WM in its own runtime dir, plus an optional remote server bound
/// to it. Dropping it stops both and removes the dir; if the test is failing,
/// it first prints both logs.
struct Wm {
    child: Child,
    server: Option<Child>,
    runtime_dir: PathBuf,
    wayland_display: String,
    ipc_path: PathBuf,
}

impl Wm {
    fn start() -> Wm {
        // Unix socket paths are capped near 108 bytes, so keep the dir short.
        let runtime_dir =
            std::env::temp_dir().join(format!("sw-e2e-{}-{}", std::process::id(), next_id()));
        assert!(
            runtime_dir.as_os_str().len() < 60,
            "runtime dir {} is too long for unix socket paths; set TMPDIR=/tmp",
            runtime_dir.display()
        );
        std::fs::create_dir(&runtime_dir).expect("create runtime dir");
        std::fs::set_permissions(
            &runtime_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("chmod runtime dir");
        let config = runtime_dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[general]\nautostart = []\n\n[background]\ncolor = \"#{:02x}{:02x}{:02x}\"\n",
                BACKGROUND[0], BACKGROUND[1], BACKGROUND[2]
            ),
        )
        .expect("write config");

        let stderr = std::fs::File::create(runtime_dir.join("wm.stderr")).expect("wm.stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_shoestring-wm"))
            .args(["--backend", "headless", "--enable-automation"])
            .arg("--output-size")
            .arg(format!("{OUTPUT_W}x{OUTPUT_H}"))
            .arg("--config")
            .arg(&config)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("SHOESTRING_WM_SOCKET")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("SHOESTRING_WM_LOG", runtime_dir.join("wm.log"))
            .env("SHOESTRING_WM_HEADLESS_SOFTWARE", "1")
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn shoestring-wm");

        let mut wm = Wm {
            child,
            server: None,
            runtime_dir,
            wayland_display: String::new(),
            ipc_path: PathBuf::new(),
        };
        // The IPC socket is named after the Wayland socket and comes up after it.
        let (display, ipc_path) = wait_for("the WM's IPC socket", || {
            if let Ok(Some(status)) = wm.child.try_wait() {
                panic!("the WM exited during startup ({status})");
            }
            std::fs::read_dir(&wm.runtime_dir)
                .ok()?
                .flatten()
                .find_map(|e| {
                    let name = e.file_name().into_string().ok()?;
                    let display = name.strip_prefix("shoestring-wm-")?.strip_suffix(".sock")?;
                    Some((display.to_owned(), e.path()))
                })
        });
        wm.wayland_display = display;
        wm.ipc_path = ipc_path;
        let ready = wm.ipc(json!({"type": "wait_ready"}));
        assert_eq!(ready["ready"], true, "wait_ready: {ready}");
        wm
    }

    fn wayland_socket(&self) -> PathBuf {
        self.runtime_dir.join(&self.wayland_display)
    }

    fn connect_ipc(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.ipc_path).expect("connect to WM IPC");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream
    }

    /// One request, one reply, on a fresh connection.
    fn ipc(&self, request: Value) -> Value {
        let mut stream = self.connect_ipc();
        writeln!(stream, "{request}").expect("write IPC request");
        read_line(&mut stream)
    }

    /// Like [`Wm::ipc`], but the reply must not be an error.
    fn ipc_ok(&self, request: Value) -> Value {
        let reply = self.ipc(request.clone());
        assert_ne!(reply["type"], "error", "{request} failed: {reply}");
        reply
    }

    fn pixel(&self, (x, y): (i32, i32)) -> [u8; 3] {
        let p = self.ipc_ok(json!({"type": "get_pixel", "x": x, "y": y}));
        let channel = |c: &str| p[c].as_u64().expect("pixel channel") as u8;
        [channel("r"), channel("g"), channel("b")]
    }

    fn wait_pixel(&self, at: (i32, i32), want: [u8; 3], what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let last = self.pixel(at);
            if last == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: output pixel at {at:?} is {last:02x?}, expected {want:02x?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The test client's window, once mapped: `(id, centre)`. `centre` is
    /// `None` while the window has no on-screen geometry (minimized, or on
    /// another workspace).
    fn window(&self) -> Option<(String, Option<(i32, i32)>)> {
        let reply = self.ipc_ok(json!({"type": "windows"}));
        let w = reply["windows"]
            .as_array()?
            .iter()
            .find(|w| w["title"] == TITLE)?;
        let g = &w["geometry"];
        let centre = g["x"].as_i64().map(|x| {
            let at = |k: &str| g[k].as_i64().unwrap();
            ((x + at("w") / 2) as i32, (at("y") + at("h") / 2) as i32)
        });
        Some((w["id"].as_str()?.to_owned(), centre))
    }

    fn mapped_window(&self) -> (String, (i32, i32)) {
        wait_for("the test window to map", || match self.window() {
            Some((id, Some(centre))) => Some((id, centre)),
            _ => None,
        })
    }

    /// Park the pointer in a corner, so the cursor sprite never covers a probe.
    fn park_pointer(&self) {
        self.ipc_ok(json!({"type": "move_mouse", "x": OUTPUT_W - 1, "y": OUTPUT_H - 1}));
    }

    fn remote_status(&self) -> Value {
        self.ipc_ok(json!({"type": "remote_status"}))
    }

    fn remote_socket(&self) -> PathBuf {
        self.runtime_dir.join("remote.sock")
    }

    /// Start `shoestring-remote-server` against this WM, wait for it to
    /// register, and turn the remote gate on.
    fn start_remote_server(&mut self) {
        let bin = Path::new(env!("CARGO_BIN_EXE_shoestring-wm"))
            .with_file_name("shoestring-remote-server");
        assert!(
            bin.exists(),
            "{} is missing; build it with `cargo build -p shoestring-remote-server`",
            bin.display()
        );
        let stderr = std::fs::File::create(self.runtime_dir.join("server.stderr")).unwrap();
        let server = Command::new(bin)
            .arg("--socket")
            .arg(self.remote_socket())
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .env("SHOESTRING_WM_SOCKET", &self.ipc_path)
            .env(
                "SHOESTRING_REMOTE_SERVER_LOG",
                self.runtime_dir.join("server.log"),
            )
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn shoestring-remote-server");
        self.server = Some(server);
        wait_for("the remote server to register", || {
            (self.remote_status()["server_available"] == true).then_some(())
        });
        self.set_remote(true);
    }

    /// Flip the remote gate and wait for the server's listener to follow.
    fn set_remote(&self, enabled: bool) {
        let reply = self.ipc_ok(json!({"type": "set_remote", "enabled": enabled}));
        assert_eq!(reply["enabled"], enabled, "set_remote: {reply}");
        let socket = self.remote_socket();
        wait_for("the remote listener to follow the gate", || {
            (socket.exists() == enabled).then_some(())
        });
    }

    fn viewers(&self) -> u64 {
        self.remote_status()["viewers"].as_u64().unwrap_or(0)
    }

    fn wait_viewers(&self, n: u64) {
        wait_for(&format!("the viewer count to reach {n}"), || {
            (self.viewers() == n).then_some(())
        });
    }

    fn dump_logs(&self) {
        for name in ["wm.log", "wm.stderr", "server.log", "server.stderr"] {
            let Ok(text) = std::fs::read_to_string(self.runtime_dir.join(name)) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            eprintln!("==> {name} (last 40 lines)");
            for line in &lines[lines.len().saturating_sub(40)..] {
                eprintln!("{line}");
            }
        }
    }
}

impl Drop for Wm {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.dump_logs();
        }
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

fn next_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Read one newline-terminated line byte by byte, so nothing past it is
/// consumed.
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

// ── The remote viewer ──────────────────────────────────────────────────────────

/// How a viewer's stream ended.
#[derive(Debug, PartialEq)]
enum End {
    /// The server said `Bye`.
    Bye,
    /// The socket closed (or broke) without one.
    Closed,
}

/// A remote viewer: the test's side of a serve or graft session. A reader
/// thread decodes server messages into a channel, so waits never desync the
/// framing; frames are applied to a local framebuffer as they are received.
struct Viewer {
    stream: UnixStream,
    messages: Receiver<Result<ServerMessage, End>>,
    width: u32,
    height: u32,
    framebuffer: Vec<u8>,
    meta: Option<(String, String)>,
    ended: Option<End>,
}

impl Viewer {
    fn connect(wm: &Wm, first: ClientMessage) -> Viewer {
        let mut stream = UnixStream::connect(wm.remote_socket()).expect("connect to remote server");
        write_framed(&mut stream, &first).expect("send first message");
        stream.flush().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || loop {
            let msg = match read_framed::<_, ServerMessage>(&mut reader) {
                Ok(ServerMessage::Bye) => Err(End::Bye),
                Ok(m) => Ok(m),
                Err(_) => Err(End::Closed),
            };
            let last = msg.is_err();
            if tx.send(msg).is_err() || last {
                return;
            }
        });
        Viewer {
            stream,
            messages,
            width: 0,
            height: 0,
            framebuffer: Vec::new(),
            meta: None,
            ended: None,
        }
    }

    /// A serve-mode viewer, after its first full frame.
    fn serve(wm: &Wm) -> Viewer {
        let hello = ClientMessage::Hello {
            width: OUTPUT_W,
            height: OUTPUT_H,
            scale: 1.0,
        };
        let mut v = Viewer::connect(wm, hello);
        v.wait_first_frame();
        v
    }

    /// A graft-mode viewer of the window matching `selector`, after its first
    /// full frame.
    fn graft(wm: &Wm, selector: &str) -> Viewer {
        let mut v = Viewer::connect(
            wm,
            ClientMessage::Graft {
                selector: selector.into(),
            },
        );
        v.wait_first_frame();
        v
    }

    fn wait_first_frame(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        // `Ready` (which sizes the framebuffer) always precedes the first frame.
        while !self.pump_one(deadline) {
            assert!(
                self.ended.is_none(),
                "session ended before its first frame: {:?}",
                self.ended
            );
            assert!(
                Instant::now() < deadline,
                "no first frame within {TIMEOUT:?}"
            );
        }
    }

    /// Handle one message, waiting until `deadline`. Returns whether it was a
    /// frame.
    fn pump_one(&mut self, deadline: Instant) -> bool {
        if self.ended.is_some() {
            return false;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = match self.messages.recv_timeout(left) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => Err(End::Closed),
        };
        match msg {
            Ok(
                ServerMessage::Ready { width, height, .. }
                | ServerMessage::Resize { width, height, .. },
            ) => {
                self.width = width;
                self.height = height;
                self.framebuffer = vec![0; framebuffer_len(width, height).expect("frame size")];
                false
            }
            Ok(ServerMessage::Frame { tiles, .. }) => {
                assert!(self.width > 0, "Frame before Ready");
                for tile in &tiles {
                    apply_tile(&mut self.framebuffer, self.width, self.height, tile)
                        .expect("apply tile");
                }
                true
            }
            Ok(ServerMessage::Meta { app_id, title }) => {
                self.meta = Some((app_id, title));
                false
            }
            Ok(_) => false,
            Err(end) => {
                self.ended = Some(end);
                false
            }
        }
    }

    fn pixel(&self, (x, y): (i32, i32)) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        // Argb8888 is B, G, R, A in memory.
        [
            self.framebuffer[i + 2],
            self.framebuffer[i + 1],
            self.framebuffer[i],
        ]
    }

    /// Apply frames until the pixel at `at` (in the stream's coordinates) is
    /// `want`.
    fn wait_pixel(&mut self, at: (i32, i32), want: [u8; 3], what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while self.pixel(at) != want {
            assert!(
                self.ended.is_none(),
                "{what}: session ended: {:?}",
                self.ended
            );
            assert!(
                Instant::now() < deadline,
                "{what}: streamed pixel at {at:?} is {:02x?}, expected {want:02x?}",
                self.pixel(at)
            );
            self.pump_one(deadline);
        }
    }

    /// Count frames that arrive within `window`.
    fn frames_within(&mut self, window: Duration) -> usize {
        let deadline = Instant::now() + window;
        let mut frames = 0;
        while Instant::now() < deadline && self.ended.is_none() {
            frames += usize::from(self.pump_one(deadline));
        }
        frames
    }

    /// Wait for the session to end, however it ends.
    fn wait_end(&mut self) -> End {
        let deadline = Instant::now() + TIMEOUT;
        while self.ended.is_none() {
            assert!(Instant::now() < deadline, "the session did not end");
            self.pump_one(deadline);
        }
        self.ended.take().unwrap()
    }

    fn send(&mut self, msg: ClientMessage) {
        write_framed(&mut self.stream, &msg).expect("send to server");
        self.stream.flush().unwrap();
    }

    fn click(&mut self, (x, y): (i32, i32)) {
        self.send(ClientMessage::PointerMotion {
            x: x as f64,
            y: y as f64,
        });
        for pressed in [true, false] {
            self.send(ClientMessage::PointerButton {
                button: BTN_LEFT,
                pressed,
            });
        }
    }

    fn key(&mut self, keycode: u32) {
        for pressed in [true, false] {
            self.send(ClientMessage::Key { keycode, pressed });
        }
    }

    fn bye(mut self) {
        self.send(ClientMessage::Bye);
    }
}

// ── The test client ────────────────────────────────────────────────────────────

/// Requests to the client thread. Each is acknowledged once done.
enum Cmd {
    /// Repaint the toplevel in this colour.
    Paint(u32),
    /// Take an `ext-session-lock-v1` lock, with a lock surface per output.
    Lock,
    Unlock,
}

/// A Wayland client on its own thread: one solid-colour toplevel that turns
/// [`CLICKED`] on a pointer button press and [`KEYED`] on a key press. It can
/// also be told to repaint, and to lock and unlock the session.
struct TestClient {
    cmds: Sender<Cmd>,
    acks: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl TestClient {
    fn map(wm: &Wm) -> TestClient {
        let socket =
            UnixStream::connect(wm.wayland_socket()).expect("connect to the WM's Wayland socket");
        let (cmds, cmd_rx) = mpsc::channel();
        let (ack_tx, acks) = mpsc::channel();
        let thread = std::thread::spawn(move || client_main(socket, cmd_rx, ack_tx));
        let client = TestClient {
            cmds,
            acks,
            thread: Some(thread),
        };
        client.wait_ack("the client to map its window");
        client
    }

    fn wait_ack(&self, what: &str) {
        match self.acks.recv_timeout(TIMEOUT) {
            Ok(()) => {}
            Err(e) => panic!("test client: no ack for {what} ({e:?})"),
        }
    }

    fn request(&self, cmd: Cmd, what: &str) {
        self.cmds.send(cmd).expect("client thread is gone");
        self.wait_ack(what);
    }

    fn paint(&self, argb: u32) {
        self.request(Cmd::Paint(argb), "a repaint");
    }

    fn lock(&self) {
        self.request(Cmd::Lock, "the session lock");
    }

    fn unlock(&self) {
        self.request(Cmd::Unlock, "the unlock");
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        // Dropping `cmds` stops the thread.
        let (dead, _) = mpsc::channel();
        self.cmds = dead;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct ClientState {
    compositor: Option<WlCompositor>,
    wm_base: Option<XdgWmBase>,
    shm: Option<WlShm>,
    seat: Option<WlSeat>,
    lock_manager: Option<ExtSessionLockManagerV1>,
    outputs: Vec<WlOutput>,
    configured: bool,
    /// The colour the window should show; repainted when it changes.
    colour: u32,
    painted: u32,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    /// Pending lock-surface configures: (surface index, serial, width, height).
    lock_configures: Vec<(usize, u32, u32, u32)>,
    locked: bool,
    lock_finished: bool,
}

fn client_main(socket: UnixStream, cmds: Receiver<Cmd>, acks: Sender<()>) {
    let conn = Connection::from_socket(socket).expect("wayland connection");
    let mut eq = conn.new_event_queue::<ClientState>();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = ClientState::default();
    eq.roundtrip(&mut state).expect("registry roundtrip");
    let compositor = state.compositor.clone().expect("wl_compositor");
    let wm_base = state.wm_base.clone().expect("xdg_wm_base");
    assert!(
        state.shm.is_some() && state.seat.is_some(),
        "wl_shm / wl_seat missing"
    );
    eq.roundtrip(&mut state).expect("seat roundtrip");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(TITLE.into());
    toplevel.set_app_id("shoestring-e2e".into());
    surface.commit();
    pump_until(&conn, &mut eq, &mut state, "toplevel configure", |s| {
        s.configured
    });
    state.colour = IDLE;
    acks.send(()).unwrap();

    let mut lock: Option<(ExtSessionLockV1, Vec<(WlSurface, ExtSessionLockSurfaceV1)>)> = None;
    loop {
        if state.painted != state.colour {
            let buffer = solid_buffer(&state, &qh, WINDOW_W, WINDOW_H, state.colour);
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, WINDOW_W as i32, WINDOW_H as i32);
            surface.commit();
            state.painted = state.colour;
        }
        match cmds.try_recv() {
            Ok(Cmd::Paint(argb)) => {
                state.colour = argb;
                acks.send(()).unwrap();
                continue;
            }
            Ok(Cmd::Lock) => {
                lock = Some(take_lock(&conn, &mut eq, &mut state, &compositor));
                acks.send(()).unwrap();
            }
            Ok(Cmd::Unlock) => {
                let (session_lock, surfaces) = lock.take().expect("unlock without a lock");
                session_lock.unlock_and_destroy();
                for (s, ls) in surfaces {
                    ls.destroy();
                    s.destroy();
                }
                conn.flush().unwrap();
                eq.roundtrip(&mut state).expect("unlock roundtrip");
                acks.send(()).unwrap();
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }
        pump(&conn, &mut eq, &mut state, 20);
    }
}

/// Lock the session: one black-free, [`LOCK`]-coloured surface per output.
fn take_lock(
    conn: &Connection,
    eq: &mut EventQueue<ClientState>,
    state: &mut ClientState,
    compositor: &WlCompositor,
) -> (ExtSessionLockV1, Vec<(WlSurface, ExtSessionLockSurfaceV1)>) {
    let qh = eq.handle();
    let manager = state
        .lock_manager
        .clone()
        .expect("ext_session_lock_manager_v1");
    let session_lock = manager.lock(&qh, ());
    let surfaces: Vec<_> = state
        .outputs
        .clone()
        .iter()
        .enumerate()
        .map(|(i, output)| {
            let s = compositor.create_surface(&qh, ());
            let ls = session_lock.get_lock_surface(&s, output, &qh, i);
            (s, ls)
        })
        .collect();
    let outputs = surfaces.len();
    state.locked = false;
    pump_until(conn, eq, state, "lock surface configures", |s| {
        s.lock_configures.len() >= outputs || s.lock_finished
    });
    assert!(!state.lock_finished, "the WM refused the session lock");
    for (index, serial, width, height) in std::mem::take(&mut state.lock_configures) {
        let (s, ls) = &surfaces[index];
        ls.ack_configure(serial);
        let (w, h) = (width.max(1), height.max(1));
        s.attach(Some(&solid_buffer(state, &qh, w, h, LOCK)), 0, 0);
        s.damage_buffer(0, 0, w as i32, h as i32);
        s.commit();
    }
    pump_until(conn, eq, state, "`locked`", |s| s.locked);
    (session_lock, surfaces)
}

/// Flush, wait up to `timeout_ms` for events, and dispatch them.
fn pump(
    conn: &Connection,
    eq: &mut EventQueue<ClientState>,
    state: &mut ClientState,
    timeout_ms: i32,
) {
    conn.flush().expect("flush");
    eq.dispatch_pending(state).expect("dispatch");
    if let Some(guard) = eq.prepare_read() {
        let mut fd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fd, 1, timeout_ms) } > 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("wayland read: {e}"),
            }
        }
    }
    eq.dispatch_pending(state).expect("dispatch");
}

fn pump_until(
    conn: &Connection,
    eq: &mut EventQueue<ClientState>,
    state: &mut ClientState,
    what: &str,
    done: impl Fn(&ClientState) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    while !done(state) {
        assert!(
            Instant::now() < deadline,
            "test client: timed out waiting for {what}"
        );
        pump(conn, eq, state, 20);
    }
}

/// A solid-colour ARGB8888 shm buffer, backed by an unlinked file in
/// `$XDG_RUNTIME_DIR` (the pool keeps its own reference to the fd).
fn solid_buffer(
    state: &ClientState,
    qh: &QueueHandle<ClientState>,
    width: u32,
    height: u32,
    argb: u32,
) -> WlBuffer {
    let shm = state.shm.as_ref().expect("wl_shm");
    let stride = width * 4;
    let len = (stride * height) as usize;
    let dir = std::env::temp_dir();
    let path = dir.join(format!("sw-e2e-shm-{}-{}", std::process::id(), next_id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create shm file");
    std::fs::remove_file(&path).expect("unlink shm file");
    file.write_all(&argb.to_le_bytes().repeat(len / 4))
        .expect("fill shm file");
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

impl Dispatch<WlRegistry, ()> for ClientState {
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
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(2), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
                "wl_output" => state
                    .outputs
                    .push(registry.bind(name, version.min(3), qh, ())),
                "ext_session_lock_manager_v1" => {
                    state.lock_manager = Some(registry.bind(name, version.min(1), qh, ()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for ClientState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            state.colour = KEYED;
        }
    }
}

impl Dispatch<WlPointer, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            state.colour = CLICKED;
        }
    }
}

impl Dispatch<XdgWmBase, ()> for ClientState {
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

impl Dispatch<XdgSurface, ()> for ClientState {
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
            state.configured = true;
        }
    }
}

impl Dispatch<ExtSessionLockV1, ()> for ClientState {
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
impl Dispatch<ExtSessionLockSurfaceV1, usize> for ClientState {
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
        impl Dispatch<$ty, ()> for ClientState {
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

/// While the session is locked, a serve viewer sees the lock surface rather
/// than the windows, and a new graft session is refused.
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
    viewer.wait_pixel(centre, rgb(LOCK), "lock surface");
    viewer.wait_pixel((5, 5), rgb(LOCK), "lock surface over the background");

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
        rgb(LOCK),
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
