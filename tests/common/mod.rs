//! Shared harness for the headless end-to-end tests and measurements: a
//! headless WM in a private runtime dir ([`Wm`]), a remote viewer speaking the
//! `shoestring-remote` wire protocol ([`Viewer`]), and a Wayland test client on
//! its own thread that changes colour on input ([`TestClient`]).
//!
//! Each test binary uses a different subset, hence the `dead_code` allowance.

#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
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

pub const OUTPUT_W: u32 = 640;
pub const OUTPUT_H: u32 = 480;
/// `[background] color`, set explicitly so the test doesn't track the default.
pub const BACKGROUND: [u8; 3] = [0x10, 0x20, 0x30];
pub const WINDOW_W: u32 = 200;
pub const WINDOW_H: u32 = 150;
pub const TITLE: &str = "shoestring e2e client";

/// The client's colours, as ARGB8888: at map, after a pointer button press,
/// after a key press. The lock surface gets its own.
pub const IDLE: u32 = 0xff20_60c0;
pub const CLICKED: u32 = 0xffc0_4020;
pub const KEYED: u32 = 0xff20_c060;
pub const LOCK: u32 = 0xff60_2080;
/// Not a colour: [`TestClient::paint`] with this draws [`pattern_pixels`]
/// instead, a translucent gradient that exercises blending.
pub const PATTERN: u32 = 0;

pub const BTN_LEFT: u32 = 0x110;
/// evdev `KEY_A`.
pub const KEY_A: u32 = 30;

/// How long any single expectation may take before the test fails.
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub fn rgb(argb: u32) -> [u8; 3] {
    [(argb >> 16) as u8, (argb >> 8) as u8, argb as u8]
}

/// Poll `probe` until it returns `Some`, or panic naming `what`.
pub fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
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
pub struct Wm {
    child: Child,
    server: Option<Child>,
    runtime_dir: PathBuf,
    wayland_display: String,
    ipc_path: PathBuf,
    output: (u32, u32),
}

/// How to start a [`Wm`].
#[derive(Clone, Debug)]
pub struct WmOptions {
    /// The virtual output's size in pixels.
    pub output: (u32, u32),
    /// Keep off the GPU even when a render node exists
    /// (`$SHOESTRING_WM_HEADLESS_SOFTWARE`), so `auto` picks pixman.
    pub software: bool,
    /// `[headless] renderer`; defaults to `$SHOESTRING_E2E_RENDERER`, if set.
    pub renderer: Option<String>,
    /// `[headless] idle_suspend`. Defaults to on when
    /// `SHOESTRING_E2E_IDLE_SUSPEND=1`, so the whole suite can run both ways.
    pub idle_suspend: bool,
    /// `[general] idle_lock_secs`; 0 disables.
    pub idle_lock_secs: u64,
    /// `[general] lock_command`, if not the default.
    pub lock_command: Option<String>,
    /// `[general] lock_preview_secs`, if not the default.
    pub lock_preview_secs: Option<u64>,
    /// `[general] output_scale`, if not 1.
    pub output_scale: Option<f64>,
}

impl Default for WmOptions {
    fn default() -> Self {
        WmOptions {
            output: (OUTPUT_W, OUTPUT_H),
            software: true,
            renderer: std::env::var("SHOESTRING_E2E_RENDERER")
                .ok()
                .filter(|r| !r.is_empty()),
            idle_suspend: std::env::var("SHOESTRING_E2E_IDLE_SUSPEND").as_deref() == Ok("1"),
            idle_lock_secs: 0,
            lock_command: None,
            lock_preview_secs: None,
            output_scale: None,
        }
    }
}

impl Wm {
    pub fn start() -> Wm {
        Wm::start_with(WmOptions::default())
    }

    pub fn start_with(options: WmOptions) -> Wm {
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
        // Only keys that differ from the defaults, so the config also loads in
        // older builds (see `bin`), whose parser rejects unknown keys.
        let mut text = String::from("[general]\nautostart = []\n");
        if options.idle_lock_secs > 0 {
            text += &format!("idle_lock_secs = {}\n", options.idle_lock_secs);
        }
        if let Some(cmd) = &options.lock_command {
            text += &format!("lock_command = {cmd:?}\n");
        }
        if let Some(secs) = options.lock_preview_secs {
            text += &format!("lock_preview_secs = {secs}\n");
        }
        if let Some(scale) = options.output_scale {
            text += &format!("output_scale = {scale:?}\n");
        }
        let renderer = options.renderer.as_ref();
        if options.idle_suspend || renderer.is_some() {
            text += "\n[headless]\n";
        }
        if options.idle_suspend {
            text += "idle_suspend = true\n";
        }
        if let Some(renderer) = renderer {
            text += &format!("renderer = {renderer:?}\n");
        }
        let [r, g, b] = BACKGROUND;
        text += &format!("\n[background]\ncolor = \"#{r:02x}{g:02x}{b:02x}\"\n");
        std::fs::write(&config, text).expect("write config");

        let stderr = std::fs::File::create(runtime_dir.join("wm.stderr")).expect("wm.stderr");
        let (output_w, output_h) = options.output;
        let mut command = Command::new(bin("shoestring-wm"));
        command
            .args(["--backend", "headless", "--enable-automation"])
            .arg("--output-size")
            .arg(format!("{output_w}x{output_h}"))
            .arg("--config")
            .arg(&config)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("SHOESTRING_WM_SOCKET")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("SHOESTRING_WM_LOG", runtime_dir.join("wm.log"));
        // An explicit `gles` means the GPU; Mesa's software switches would
        // quietly turn it into llvmpipe.
        if options.software && options.renderer.as_deref() != Some("gles") {
            command
                .env("SHOESTRING_WM_HEADLESS_SOFTWARE", "1")
                .env("LIBGL_ALWAYS_SOFTWARE", "1");
        }
        let child = command
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
            output: options.output,
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

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The remote server's pid, once [`Wm::start_remote_server`] has run.
    pub fn server_pid(&self) -> Option<u32> {
        self.server.as_ref().map(Child::id)
    }

    pub fn wayland_socket(&self) -> PathBuf {
        self.runtime_dir.join(&self.wayland_display)
    }

    pub fn connect_ipc(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.ipc_path).expect("connect to WM IPC");
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream
    }

    /// One request, one reply, on a fresh connection.
    pub fn ipc(&self, request: Value) -> Value {
        let mut stream = self.connect_ipc();
        writeln!(stream, "{request}").expect("write IPC request");
        read_line(&mut stream)
    }

    /// Like [`Wm::ipc`], but the reply must not be an error.
    pub fn ipc_ok(&self, request: Value) -> Value {
        let reply = self.ipc(request.clone());
        assert_ne!(reply["type"], "error", "{request} failed: {reply}");
        reply
    }

    pub fn pixel(&self, (x, y): (i32, i32)) -> [u8; 3] {
        let p = self.ipc_ok(json!({"type": "get_pixel", "x": x, "y": y}));
        let channel = |c: &str| p[c].as_u64().expect("pixel channel") as u8;
        [channel("r"), channel("g"), channel("b")]
    }

    pub fn wait_pixel(&self, at: (i32, i32), want: [u8; 3], what: &str) {
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
    pub fn window(&self) -> Option<(String, Option<(i32, i32)>)> {
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

    pub fn mapped_window(&self) -> (String, (i32, i32)) {
        wait_for("the test window to map", || match self.window() {
            Some((id, Some(centre))) => Some((id, centre)),
            _ => None,
        })
    }

    /// Park the pointer in a corner, so the cursor sprite never covers a probe.
    pub fn park_pointer(&self) {
        let (w, h) = self.output;
        self.ipc_ok(json!({"type": "move_mouse", "x": w - 1, "y": h - 1}));
    }

    pub fn remote_status(&self) -> Value {
        self.ipc_ok(json!({"type": "remote_status"}))
    }

    pub fn remote_socket(&self) -> PathBuf {
        self.runtime_dir.join("remote.sock")
    }

    /// Start `shoestring-remote-server` against this WM, wait for it to
    /// register, and turn the remote gate on.
    pub fn start_remote_server(&mut self) {
        let bin = bin("shoestring-remote-server");
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
    pub fn set_remote(&self, enabled: bool) {
        let reply = self.ipc_ok(json!({"type": "set_remote", "enabled": enabled}));
        assert_eq!(reply["enabled"], enabled, "set_remote: {reply}");
        let socket = self.remote_socket();
        wait_for("the remote listener to follow the gate", || {
            (socket.exists() == enabled).then_some(())
        });
    }

    pub fn viewers(&self) -> u64 {
        self.remote_status()["viewers"].as_u64().unwrap_or(0)
    }

    pub fn wait_viewers(&self, n: u64) {
        wait_for(&format!("the viewer count to reach {n}"), || {
            (self.viewers() == n).then_some(())
        });
    }

    pub fn dump_logs(&self) {
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

/// The path of a workspace binary: from `$SHOESTRING_E2E_BIN_DIR` when set
/// (to measure or test another build, such as a release tag's), else next to
/// this build's `shoestring-wm`.
pub fn bin(name: &str) -> PathBuf {
    match std::env::var_os("SHOESTRING_E2E_BIN_DIR") {
        Some(dir) => PathBuf::from(dir).join(name),
        None => Path::new(env!("CARGO_BIN_EXE_shoestring-wm")).with_file_name(name),
    }
}

pub fn next_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Read one newline-terminated line byte by byte, so nothing past it is
/// consumed.
pub fn read_line(stream: &mut UnixStream) -> Value {
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
pub enum End {
    /// The server said `Bye`.
    Bye,
    /// The socket closed (or broke) without one.
    Closed,
}

/// A remote viewer: the test's side of a serve or graft session. A reader
/// thread decodes server messages into a channel, so waits never desync the
/// framing; frames are applied to a local framebuffer as they are received.
pub struct Viewer {
    stream: UnixStream,
    messages: Receiver<Result<ServerMessage, End>>,
    /// Bytes read off the socket so far, framing included.
    received: Arc<AtomicU64>,
    pub width: u32,
    pub height: u32,
    framebuffer: Vec<u8>,
    pub meta: Option<(String, String)>,
    ended: Option<End>,
}

impl Viewer {
    pub fn connect(wm: &Wm, first: ClientMessage) -> Viewer {
        let mut stream = UnixStream::connect(wm.remote_socket()).expect("connect to remote server");
        write_framed(&mut stream, &first).expect("send first message");
        stream.flush().unwrap();
        let received = Arc::new(AtomicU64::new(0));
        let mut reader = CountingReader {
            inner: stream.try_clone().unwrap(),
            count: Arc::clone(&received),
        };
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
            received,
            width: 0,
            height: 0,
            framebuffer: Vec::new(),
            meta: None,
            ended: None,
        }
    }

    /// Bytes received from the server so far, framing included.
    pub fn bytes_received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// A serve-mode viewer, after its first full frame.
    pub fn serve(wm: &Wm) -> Viewer {
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
    pub fn graft(wm: &Wm, selector: &str) -> Viewer {
        let mut v = Viewer::connect(
            wm,
            ClientMessage::Graft {
                selector: selector.into(),
            },
        );
        v.wait_first_frame();
        v
    }

    pub fn wait_first_frame(&mut self) {
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
    pub fn pump_one(&mut self, deadline: Instant) -> bool {
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

    pub fn pixel(&self, (x, y): (i32, i32)) -> [u8; 3] {
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
    pub fn wait_pixel(&mut self, at: (i32, i32), want: [u8; 3], what: &str) {
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

    /// The streamed output as received: Argb8888, B, G, R, A in memory.
    pub fn framebuffer(&self) -> &[u8] {
        &self.framebuffer
    }

    /// Count frames that arrive within `window`.
    pub fn frames_within(&mut self, window: Duration) -> usize {
        let deadline = Instant::now() + window;
        let mut frames = 0;
        while Instant::now() < deadline && self.ended.is_none() {
            frames += usize::from(self.pump_one(deadline));
        }
        frames
    }

    /// Wait for the session to end, however it ends.
    pub fn wait_end(&mut self) -> End {
        let deadline = Instant::now() + TIMEOUT;
        while self.ended.is_none() {
            assert!(Instant::now() < deadline, "the session did not end");
            self.pump_one(deadline);
        }
        self.ended.take().unwrap()
    }

    pub fn send(&mut self, msg: ClientMessage) {
        write_framed(&mut self.stream, &msg).expect("send to server");
        self.stream.flush().unwrap();
    }

    pub fn click(&mut self, (x, y): (i32, i32)) {
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

    pub fn key(&mut self, keycode: u32) {
        for pressed in [true, false] {
            self.send(ClientMessage::Key { keycode, pressed });
        }
    }

    pub fn bye(mut self) {
        self.send(ClientMessage::Bye);
    }
}

/// Counts the bytes that pass through a reader.
struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

// ── The test client ────────────────────────────────────────────────────────────

/// Requests to the client thread. Each is acknowledged once done.
pub enum Cmd {
    /// Repaint the toplevel in this colour.
    Paint(u32),
    /// Take an `ext-session-lock-v1` lock, with a lock surface per output.
    Lock,
    Unlock,
    /// Repaint every lock surface in this colour, as an animating lock
    /// screen would.
    PaintLock(u32),
}

/// A Wayland client on its own thread: one solid-colour toplevel that turns
/// [`CLICKED`] on a pointer button press and [`KEYED`] on a key press. It can
/// also be told to repaint, and to lock and unlock the session.
pub struct TestClient {
    cmds: Sender<Cmd>,
    acks: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl TestClient {
    pub fn map(wm: &Wm) -> TestClient {
        TestClient::map_sized(wm, (WINDOW_W, WINDOW_H))
    }

    /// Map a `size` window instead of the default [`WINDOW_W`]×[`WINDOW_H`].
    pub fn map_sized(wm: &Wm, size: (u32, u32)) -> TestClient {
        let socket =
            UnixStream::connect(wm.wayland_socket()).expect("connect to the WM's Wayland socket");
        let (cmds, cmd_rx) = mpsc::channel();
        let (ack_tx, acks) = mpsc::channel();
        let thread = std::thread::spawn(move || client_main(socket, size, cmd_rx, ack_tx));
        let client = TestClient {
            cmds,
            acks,
            thread: Some(thread),
        };
        client.wait_ack("the client to map its window");
        client
    }

    pub fn wait_ack(&self, what: &str) {
        match self.acks.recv_timeout(TIMEOUT) {
            Ok(()) => {}
            Err(e) => panic!("test client: no ack for {what} ({e:?})"),
        }
    }

    pub fn request(&self, cmd: Cmd, what: &str) {
        self.cmds.send(cmd).expect("client thread is gone");
        self.wait_ack(what);
    }

    /// Repaint the window in `argb`; returns once the commit is flushed.
    pub fn paint(&self, argb: u32) {
        self.request(Cmd::Paint(argb), "a repaint");
    }

    /// Repaint the lock surfaces in `argb`; returns once the commit is flushed.
    pub fn paint_lock(&self, argb: u32) {
        self.request(Cmd::PaintLock(argb), "a lock-surface repaint");
    }

    pub fn lock(&self) {
        self.request(Cmd::Lock, "the session lock");
    }

    pub fn unlock(&self) {
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
pub struct ClientState {
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
    /// Each lock surface's configured size, by index.
    lock_sizes: Vec<(u32, u32)>,
}

pub fn client_main(
    socket: UnixStream,
    (width, height): (u32, u32),
    cmds: Receiver<Cmd>,
    acks: Sender<()>,
) {
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
    let mut pending_paint_ack = false;
    loop {
        if state.painted != state.colour {
            let buffer = solid_buffer(&state, &qh, width, height, state.colour);
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            surface.commit();
            conn.flush().expect("flush");
            state.painted = state.colour;
        }
        if std::mem::take(&mut pending_paint_ack) {
            acks.send(()).unwrap();
        }
        match cmds.try_recv() {
            Ok(Cmd::Paint(argb)) => {
                // Paint on the next pass, then acknowledge once it is flushed.
                state.colour = argb;
                pending_paint_ack = true;
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
            Ok(Cmd::PaintLock(argb)) => {
                let (_, surfaces) = lock.as_ref().expect("paint_lock without a lock");
                for ((s, _), &(w, h)) in surfaces.iter().zip(&state.lock_sizes) {
                    s.attach(Some(&solid_buffer(&state, &qh, w, h, argb)), 0, 0);
                    s.damage_buffer(0, 0, w as i32, h as i32);
                    s.commit();
                }
                conn.flush().unwrap();
                acks.send(()).unwrap();
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return,
        }
        pump(&conn, &mut eq, &mut state, 20);
    }
}

/// Lock the session: one black-free, [`LOCK`]-coloured surface per output.
pub fn take_lock(
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
    state.lock_sizes = vec![(1, 1); surfaces.len()];
    for (index, serial, width, height) in std::mem::take(&mut state.lock_configures) {
        let (s, ls) = &surfaces[index];
        ls.ack_configure(serial);
        let (w, h) = (width.max(1), height.max(1));
        state.lock_sizes[index] = (w, h);
        s.attach(Some(&solid_buffer(state, &qh, w, h, LOCK)), 0, 0);
        s.damage_buffer(0, 0, w as i32, h as i32);
        s.commit();
    }
    pump_until(conn, eq, state, "`locked`", |s| s.locked);
    (session_lock, surfaces)
}

/// Flush, wait up to `timeout_ms` for events, and dispatch them.
pub fn pump(
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

pub fn pump_until(
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
/// A `width`×`height` gradient, premultiplied ARGB8888 in memory order: red
/// across, green down, blue fixed, alpha falling from opaque at the top-left to
/// about a quarter at the bottom-right.
pub fn pattern_pixels(width: u32, height: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let a = 255 - (x * 95 / width.max(1) + y * 95 / height.max(1)) as u8;
            let premul = |c: u32| ((c * a as u32 + 127) / 255) as u8;
            let r = premul(x * 255 / width.max(1));
            let g = premul(y * 255 / height.max(1));
            let b = premul(160);
            px.extend_from_slice(&[b, g, r, a]);
        }
    }
    px
}

pub fn solid_buffer(
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
    let pixels = if argb == PATTERN {
        pattern_pixels(width, height)
    } else {
        argb.to_le_bytes().repeat(len / 4)
    };
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

// Buffers are single-use: destroy each once the WM is done reading it.
impl Dispatch<WlBuffer, ()> for ClientState {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
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
    WlOutput => wl_output::Event,
    XdgToplevel => xdg_toplevel::Event,
    ExtSessionLockManagerV1 => wayland_protocols::ext::session_lock::v1::client::ext_session_lock_manager_v1::Event,
);
