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
//!    **on**, it listens on a private Unix socket (reached from another machine
//!    over `ssh -L`); while **off**, the socket is gone — the consent model.
//! 3. Per client: streams the served output via the WM's capture subscription
//!    (damage tiles) and replays the client's raw input back into the WM
//!    (`inject_input`), reporting the viewer count so the "being viewed" chip
//!    lights.
//!
//! Transport is a single ssh-forwarded stream. The socket is mode 0600 and a
//! connection is accepted only from our own uid, so ssh login is the
//! authentication (see [`listen`]). An explicit `--tcp` listener has no such
//! check and warns at startup; one on a non-loopback address also needs
//! `--tcp-any-host`.

mod holds;
mod listen;
mod session;
mod viewers;
mod wm;

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use shoestring_ipc::Event;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;

use holds::Holds;
use listen::{Endpoint, Listener};
use session::Sessions;
use viewers::Viewers;

/// Most sessions served at once. Each holds a thread, two WM connections and
/// a capture stream; a real setup has one or two viewers, so this only stops
/// a runaway client from exhausting the box.
const MAX_SESSIONS: usize = 8;

/// How long sessions get to end on their own when the gate closes before the
/// server ends them. A healthy one ends within milliseconds, passing on the
/// WM's `Bye`; only one whose viewer stopped reading waits this out.
const SESSION_END_GRACE: Duration = Duration::from_millis(500);

/// Counts a live session; the count drops when its thread ends.
struct SessionSlot(Arc<AtomicUsize>);

impl SessionSlot {
    /// Take a slot, or `None` if [`MAX_SESSIONS`] are already in use.
    fn take(active: &Arc<AtomicUsize>) -> Option<SessionSlot> {
        let mut n = active.load(Ordering::SeqCst);
        while n < MAX_SESSIONS {
            match active.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(SessionSlot(Arc::clone(active))),
                Err(current) => n = current,
            }
        }
        None
    }
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Parsed configuration from CLI flags / env.
struct Cfg {
    endpoint: Endpoint,
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
        endpoint = %cfg.endpoint,
        "shoestring-remote-server starting"
    );
    if let Endpoint::Tcp(addr) = cfg.endpoint {
        tracing::warn!(%addr, "{}", listen::tcp_warning(addr));
    }

    // Register with the WM. The connection is kept alive for the whole process
    // (its drop forces the gate off); we read RemoteChanged events off it.
    let (event_reader, initial_enabled) =
        wm::register(&cfg.endpoint).context("registering with WM")?;
    tracing::info!(initial_enabled, "registered with WM");

    let gate = Arc::new(Gate::new(initial_enabled)?);
    let viewers = Arc::new(Viewers::new());
    let holds = Arc::new(Holds::new());

    // Reader thread: turn RemoteChanged events into gate transitions, and treat
    // the WM closing the connection as a shutdown signal.
    {
        let gate = Arc::clone(&gate);
        std::thread::Builder::new()
            .name("wm-events".into())
            .spawn(move || read_wm_events(event_reader, &gate))
            .context("spawn wm-events thread")?;
    }

    run_supervisor(&cfg, &gate, &viewers, &holds)
}

/// Gate state shared between the event reader and the listener supervisor.
///
/// Every change also writes a byte to a self-pipe, so the supervisor can sleep
/// in `poll` on the listener and the pipe together: it wakes for a connection
/// or a gate change and otherwise not at all.
struct Gate {
    inner: Mutex<GateInner>,
    cv: Condvar,
    wake_read: OwnedFd,
    wake_write: OwnedFd,
}

struct GateInner {
    enabled: bool,
    shutdown: bool,
}

impl Gate {
    fn new(enabled: bool) -> Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: pipe2 writes two fds into the array we pass.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error()).context("create gate wake pipe");
        }
        // SAFETY: both fds were just created and are owned by no one else.
        let (wake_read, wake_write) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(Gate {
            inner: Mutex::new(GateInner {
                enabled,
                shutdown: false,
            }),
            cv: Condvar::new(),
            wake_read,
            wake_write,
        })
    }

    fn set_enabled(&self, enabled: bool) {
        let mut g = self.inner.lock().unwrap();
        g.enabled = enabled;
        self.cv.notify_all();
        self.wake();
    }

    fn set_shutdown(&self) {
        let mut g = self.inner.lock().unwrap();
        g.shutdown = true;
        self.cv.notify_all();
        self.wake();
    }

    /// Nudge a supervisor sleeping in [`wait_for_activity`]. A full pipe
    /// already holds a pending wakeup, so a failed write is fine.
    fn wake(&self) {
        // SAFETY: one byte from a local, to our own non-blocking pipe.
        unsafe { libc::write(self.wake_write.as_raw_fd(), [0u8].as_ptr().cast(), 1) };
    }

    /// Empty the wake pipe after a wakeup.
    fn drain_wake(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: reading into a local buffer from our own non-blocking pipe.
        while unsafe {
            libc::read(
                self.wake_read.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        } > 0
        {}
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
fn run_supervisor(
    cfg: &Cfg,
    gate: &Arc<Gate>,
    viewers: &Arc<Viewers>,
    holds: &Arc<Holds>,
) -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let sessions = Arc::new(Sessions::default());
    loop {
        if gate.wait_enabled() {
            tracing::info!("shutting down");
            return Ok(());
        }

        let listener = match Listener::bind(&cfg.endpoint) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), endpoint = %cfg.endpoint, "failed to bind listener; gate stays on but idle");
                // Avoid a hot spin if bind keeps failing while enabled.
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        tracing::info!(endpoint = %listener.describe(), "listening (remote gate on)");

        // Accept while the gate stays on, sleeping until a connection arrives
        // or the gate changes; an idle server wakes for nothing.
        loop {
            let (enabled, shutdown) = gate.snapshot();
            if !enabled || shutdown {
                break;
            }
            if let Err(e) = wait_for_activity(listener.as_raw_fd(), gate.wake_read.as_raw_fd()) {
                tracing::warn!(error = %e, "poll failed; reopening listener");
                break;
            }
            gate.drain_wake();
            match listener.accept() {
                Ok(Some(accepted)) => {
                    let Some(slot) = SessionSlot::take(&active) else {
                        tracing::warn!(peer = %accepted.peer, max = MAX_SESSIONS, "refused a connection: too many sessions");
                        continue;
                    };
                    tracing::info!(peer = %accepted.peer, "client connected");
                    let viewers = Arc::clone(viewers);
                    let holds = Arc::clone(holds);
                    let sessions = Arc::clone(&sessions);
                    let output = cfg.output.clone();
                    std::thread::Builder::new()
                        .name("session".into())
                        .spawn(move || {
                            let _slot = slot;
                            if let Err(e) = session::serve(
                                accepted.stream,
                                accepted.peer,
                                output,
                                viewers,
                                holds,
                                sessions,
                            ) {
                                tracing::warn!(error = %e, "session error");
                            }
                        })
                        .ok();
                }
                // Nothing pending (a gate wakeup), or a peer the access check
                // refused: back to waiting.
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed; reopening listener");
                    break;
                }
            }
        }

        // Gate off (or accept died): drop the listener so the socket goes. With
        // the gate off, the WM has released the keys the sessions held, so the
        // viewer chip clears now and the count and holds start a new
        // generation. The sessions end as the WM closes their capture streams,
        // except one whose viewer stopped reading: it would never see that and
        // would keep its slot and its connection past the consent, so after a
        // grace the server ends what is left. After a failed accept the
        // sessions are still live and keep all of it.
        drop(listener);
        if !gate.snapshot().0 {
            viewers.reset();
            holds.reset();
            let ended = sessions.end_all(SESSION_END_GRACE);
            if ended > 0 {
                tracing::info!(
                    sessions = ended,
                    "ended sessions whose viewers stopped reading (remote gate off)"
                );
            }
        }
        tracing::info!("listener closed (remote gate off)");

        if gate.snapshot().1 {
            tracing::info!("shutting down");
            return Ok(());
        }
    }
}

/// Block until `listener` has a connection waiting or `wake` (the gate's pipe)
/// is readable.
fn wait_for_activity(listener: RawFd, wake: RawFd) -> std::io::Result<()> {
    let mut fds = [
        libc::pollfd {
            fd: listener,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: two valid pollfds; no timeout.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } >= 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Parse CLI flags, falling back to env then defaults. Returns `None` if a
/// help/version flag was handled and the process should exit.
fn parse_args() -> Result<Option<Cfg>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut socket = env("SHOESTRING_REMOTE_SOCKET").map(std::path::PathBuf::from);
    let mut tcp = env("SHOESTRING_REMOTE_TCP");
    let mut tcp_any_host = match env("SHOESTRING_REMOTE_TCP_ANY_HOST").as_deref() {
        None | Some("0") => false,
        Some("1") => true,
        Some(other) => anyhow::bail!("SHOESTRING_REMOTE_TCP_ANY_HOST must be 0 or 1, not {other}"),
    };
    let mut output = env("SHOESTRING_REMOTE_OUTPUT");
    // The old TCP-by-default settings. Honouring them would quietly reopen an
    // unchecked TCP port, so stop with a pointer to the explicit setting.
    for old in ["SHOESTRING_REMOTE_BIND", "SHOESTRING_REMOTE_PORT"] {
        if env(old).is_some() {
            anyhow::bail!(
                "{old} is no longer supported: the server listens on a Unix socket; \
                 set SHOESTRING_REMOTE_TCP=ADDR:PORT for TCP"
            );
        }
    }

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
            "--socket" => {
                socket = Some(args.next().context("--socket needs a path")?.into());
            }
            "--tcp" => {
                tcp = Some(args.next().context("--tcp needs ADDR:PORT")?);
            }
            "--tcp-any-host" => tcp_any_host = true,
            "--bind" | "--port" => {
                anyhow::bail!("{arg} is no longer supported: use --tcp ADDR:PORT (try --help)")
            }
            "--output" => {
                output = Some(args.next().context("--output needs an output name")?);
            }
            other => anyhow::bail!("unknown argument: {other} (try --help)"),
        }
    }
    let endpoint = match (socket, tcp) {
        (Some(_), Some(_)) => anyhow::bail!("--socket and --tcp are mutually exclusive"),
        (Some(path), None) => Endpoint::Unix(path),
        (None, Some(addr)) => listen::tcp_endpoint(&addr, tcp_any_host)?,
        (None, None) => Endpoint::default_unix()?,
    };
    Ok(Some(Cfg { endpoint, output }))
}

fn help_text() -> String {
    format!(
        "shoestring-remote-server {ver} — serve-mode remote-desktop bridge.

Usage: shoestring-remote-server [OPTIONS]

Registers with the local shoestring-wm and, while the remote gate is enabled,
streams the served output and replays client input over a Unix socket that
only this user can connect to. Reach it from another machine with OpenSSH's
socket forwarding:

  ssh -N -L \"$XDG_RUNTIME_DIR/box.sock:<remote socket path>\" box
  shoestring-remote-client --connect \"$XDG_RUNTIME_DIR/box.sock\"

Options:
      --socket PATH    Listen on this Unix socket (default
                       $XDG_RUNTIME_DIR/shoestring-remote-$WAYLAND_DISPLAY.sock).
      --tcp ADDR:PORT  Listen on TCP instead. Nothing checks who connects:
                       anyone who can reach the port can see and control the
                       session. A host name is resolved once, at startup.
      --tcp-any-host   Allow --tcp on an address other than loopback, such
                       as 0.0.0.0, which every host that can reach the port
                       can connect to. Refused without this.
      --output NAME    Served WM output name (default: the first output).
  -h, --help           Print this help and exit.
  -V, --version        Print version and exit.

Environment: SHOESTRING_WM_SOCKET (WM IPC socket, required to find the WM),
SHOESTRING_REMOTE_SOCKET / SHOESTRING_REMOTE_TCP / SHOESTRING_REMOTE_OUTPUT
(same as the flags), SHOESTRING_REMOTE_TCP_ANY_HOST=1 (same as --tcp-any-host), SHOESTRING_REMOTE_SERVER_LOG (append tracing to a file),
RUST_LOG (tracing filter).
",
        ver = env!("CARGO_PKG_VERSION"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Instant;

    /// A listener nobody connects to, at a private path.
    fn quiet_listener(name: &str) -> (UnixListener, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "shoestring-remote-test-{}-{name}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        (UnixListener::bind(&path).unwrap(), path)
    }

    /// The supervisor sleeps until the gate changes: no wakeup before, a
    /// prompt one after, and the pipe is empty again once drained.
    #[test]
    fn gate_change_wakes_the_supervisor() {
        let gate = Arc::new(Gate::new(true).unwrap());
        let (listener, path) = quiet_listener("gate");
        let flipper = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                gate.set_enabled(false);
            })
        };
        let start = Instant::now();
        wait_for_activity(listener.as_raw_fd(), gate.wake_read.as_raw_fd()).unwrap();
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(150),
            "woke early, after {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "woke late, after {waited:?}"
        );
        assert_eq!(gate.snapshot(), (false, false));
        gate.drain_wake();
        let mut fd = libc::pollfd {
            fd: gate.wake_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, zero timeout.
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 0) },
            0,
            "wake pipe not drained"
        );
        flipper.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    /// A connection wakes the supervisor too.
    #[test]
    fn connection_wakes_the_supervisor() {
        let gate = Gate::new(true).unwrap();
        let (listener, path) = quiet_listener("conn");
        let _client = UnixStream::connect(&path).unwrap();
        wait_for_activity(listener.as_raw_fd(), gate.wake_read.as_raw_fd()).unwrap();
        assert!(listener.accept().is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_slots_cap_and_free() {
        let active = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_SESSIONS)
            .map(|_| SessionSlot::take(&active).expect("under the cap"))
            .collect();
        assert!(SessionSlot::take(&active).is_none(), "the cap must hold");
        drop(slots);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(
            SessionSlot::take(&active).is_some(),
            "freed slots are reusable"
        );
    }
}
