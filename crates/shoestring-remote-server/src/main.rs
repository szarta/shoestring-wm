//! `shoestring-remote-server` — the serve-mode remote-desktop bridge.
//!
//! Runs on the box being remoted *into* (typically headless), added to the WM
//! autostart. It is a privileged IPC bridge between the local shoestring-wm and
//! an ssh-tunneled network protocol:
//!
//! 1. **Registers** with the local WM ([`Request::RegisterRemoteServer`]), which
//!    makes the remote-gate toggle selectable (greyed until a server registers)
//!    and means the process's exit forces the gate back off.
//! 2. Watches [`Event::RemoteChanged`] on that connection. While the gate is
//!    **on**, it opens a localhost TCP listener (reached over `ssh -L`); while
//!    **off**, the listener is closed and no port is open — the consent model.
//! 3. Per client: streams the served output via the WM's capture subscription
//!    (damage tiles) and replays the client's raw input back into the WM
//!    (`inject_input`), reporting the viewer count so the "being viewed" chip
//!    lights.
//!
//! Transport is a single ssh-tunneled stream; there is no auth surface of our
//! own (reuse existing ssh keys/trust + the named-node workflow).

mod session;
mod viewers;
mod wm;

use std::net::TcpListener;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use shoestring_ipc::Event;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;

use viewers::Viewers;

/// Default listener port. Bound to loopback only; a remote client reaches it via
/// `ssh -L <port>:127.0.0.1:<port> served-host` and connects locally.
const DEFAULT_PORT: u16 = 7355;
const DEFAULT_BIND: &str = "127.0.0.1";

/// Parsed configuration from CLI flags / env.
struct Cfg {
    bind: String,
    port: u16,
    output: Option<String>,
}

fn main() -> Result<()> {
    let cfg = match parse_args()? {
        Some(cfg) => cfg,
        None => return Ok(()), // --help / --version already printed.
    };
    init_tracing();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %cfg.bind,
        port = cfg.port,
        "shoestring-remote-server starting"
    );

    // Register with the WM. The connection is kept alive for the whole process
    // (its drop forces the gate off); we read RemoteChanged events off it.
    let (event_reader, initial_enabled) = wm::register().context("registering with WM")?;
    tracing::info!(initial_enabled, "registered with WM");

    let gate = Arc::new(Gate::new(initial_enabled));
    let viewers = Arc::new(Viewers::new());

    // Reader thread: turn RemoteChanged events into gate transitions, and treat
    // the WM closing the connection as a shutdown signal.
    {
        let gate = Arc::clone(&gate);
        std::thread::Builder::new()
            .name("wm-events".into())
            .spawn(move || read_wm_events(event_reader, &gate))
            .context("spawn wm-events thread")?;
    }

    run_supervisor(&cfg, &gate, &viewers)
}

/// Gate state shared between the event reader and the listener supervisor.
struct Gate {
    inner: Mutex<GateInner>,
    cv: Condvar,
}

struct GateInner {
    enabled: bool,
    shutdown: bool,
}

impl Gate {
    fn new(enabled: bool) -> Self {
        Gate {
            inner: Mutex::new(GateInner {
                enabled,
                shutdown: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn set_enabled(&self, enabled: bool) {
        let mut g = self.inner.lock().unwrap();
        g.enabled = enabled;
        self.cv.notify_all();
    }

    fn set_shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        g.shutdown = true;
        self.cv.notify_all();
    }

    /// Block until the gate is enabled or shutdown is requested. Returns the
    /// shutdown flag.
    fn wait_enabled(&self) -> bool {
        let g = self
            .cv
            .wait_while(self.inner.lock().unwrap(), |s| !s.enabled && !s.shutdown)
            .unwrap();
        g.shutdown
    }

    fn snapshot(&self) -> (bool, bool) {
        let g = self.inner.lock().unwrap();
        (g.enabled, g.shutdown)
    }
}

/// Read newline-JSON event lines from the WM registration connection. Updates
/// the gate on `RemoteChanged`; a clean EOF (WM exit / kicked) requests
/// shutdown.
fn read_wm_events(mut reader: std::io::BufReader<std::os::unix::net::UnixStream>, gate: &Gate) {
    loop {
        match wm::read_line(&mut reader) {
            Ok(Some(line)) => {
                // The connection carries events plus the odd response ack; only
                // RemoteChanged matters. Parse leniently — ignore anything else.
                match serde_json::from_str::<Event>(&line) {
                    Ok(Event::RemoteChanged { enabled, .. }) => {
                        tracing::info!(enabled, "remote gate changed");
                        gate.set_enabled(enabled);
                    }
                    Ok(_) => {}
                    Err(_) => {} // a Response ack or unknown event — ignore.
                }
            }
            Ok(None) => {
                tracing::warn!("WM closed the registration connection; shutting down");
                gate.set_shutdown();
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "error reading WM events; shutting down");
                gate.set_shutdown();
                return;
            }
        }
    }
}

/// Open the listener while the gate is on; close it when the gate goes off.
/// Each accepted connection is handed to a session thread.
fn run_supervisor(cfg: &Cfg, gate: &Arc<Gate>, viewers: &Arc<Viewers>) -> Result<()> {
    loop {
        if gate.wait_enabled() {
            tracing::info!("shutting down");
            return Ok(());
        }

        let addr = format!("{}:{}", cfg.bind, cfg.port);
        let listener = match TcpListener::bind(&addr) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, %addr, "failed to bind listener; gate stays on but idle");
                // Avoid a hot spin if bind keeps failing while enabled.
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        listener.set_nonblocking(true).ok();
        tracing::info!(%addr, "listening (remote gate on)");

        // Accept while the gate stays on. Non-blocking accept + a short sleep so
        // a gate-off is noticed promptly without a dedicated wakeup socket.
        loop {
            let (enabled, shutdown) = gate.snapshot();
            if !enabled || shutdown {
                break;
            }
            match listener.accept() {
                Ok((net, peer)) => {
                    tracing::info!(%peer, "client connected");
                    let viewers = Arc::clone(viewers);
                    let output = cfg.output.clone();
                    std::thread::Builder::new()
                        .name("session".into())
                        .spawn(move || {
                            if let Err(e) = session::serve(net, output, viewers) {
                                tracing::warn!(error = %e, "session error");
                            }
                        })
                        .ok();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed; reopening listener");
                    break;
                }
            }
        }

        // Gate off (or accept died): drop the listener so the port closes, and
        // clear the viewer chip. Live sessions end on their own when the WM
        // tears down their capture subscriptions.
        drop(listener);
        viewers.reset();
        tracing::info!("listener closed (remote gate off)");

        if gate.snapshot().1 {
            tracing::info!("shutting down");
            return Ok(());
        }
    }
}

/// Parse CLI flags, falling back to env then defaults. Returns `None` if a
/// help/version flag was handled and the process should exit.
fn parse_args() -> Result<Option<Cfg>> {
    let mut bind = std::env::var("SHOESTRING_REMOTE_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());
    let mut port = std::env::var("SHOESTRING_REMOTE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let mut output = std::env::var("SHOESTRING_REMOTE_OUTPUT").ok();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", help_text());
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("shoestring-remote-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--bind" => {
                bind = args.next().context("--bind needs an address")?;
            }
            "--port" => {
                port = args
                    .next()
                    .context("--port needs a number")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--output" => {
                output = Some(args.next().context("--output needs an output name")?);
            }
            other => anyhow::bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(Some(Cfg { bind, port, output }))
}

fn help_text() -> String {
    format!(
        "shoestring-remote-server {ver} — serve-mode remote-desktop bridge.

Usage: shoestring-remote-server [OPTIONS]

Registers with the local shoestring-wm and, while the remote gate is enabled,
streams the served output and replays client input over a localhost TCP
listener (reach it from another machine with `ssh -L`).

Options:
      --bind ADDR    Listener bind address (default {bind}, loopback only).
      --port PORT    Listener port (default {port}).
      --output NAME  Served WM output name (default: the first output).
  -h, --help         Print this help and exit.
  -V, --version      Print version and exit.

Environment: SHOESTRING_WM_SOCKET (WM IPC socket, required to find the WM),
SHOESTRING_REMOTE_BIND / SHOESTRING_REMOTE_PORT / SHOESTRING_REMOTE_OUTPUT
(same as the flags), SHOESTRING_REMOTE_SERVER_LOG (append tracing to a file),
RUST_LOG (tracing filter).
",
        ver = env!("CARGO_PKG_VERSION"),
        bind = DEFAULT_BIND,
        port = DEFAULT_PORT,
    )
}

/// Initialise tracing. Writes to stderr by default; if
/// `SHOESTRING_REMOTE_SERVER_LOG` is set, appends to that file (ANSI off),
/// mirroring `SHOESTRING_WM_LOG` / `SHOESTRING_BAR_LOG`.
fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = match std::env::var_os("SHOESTRING_REMOTE_SERVER_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_REMOTE_SERVER_LOG path");
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_filter(env)
                .boxed()
        }
        None => tracing_subscriber::fmt::layer().with_filter(env).boxed(),
    };
    tracing_subscriber::registry().with(fmt_layer).init();
}
