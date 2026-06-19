//! The `shoestring-wm` binary: CLI parsing, tracing/SIGCHLD/systemd glue, and
//! the backend-selection startup sequence. Everything else lives in the
//! [`shoestring_wm`] library crate (see `src/lib.rs`).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::Result;
use clap::Parser;
use smithay::reexports::{
    calloop::{generic::Generic, EventLoop, Interest, LoopHandle, Mode, PostAction},
    wayland_server::Display,
};
use tracing_subscriber::EnvFilter;

use shoestring_wm::state::ShoestringWm;
use shoestring_wm::{backend, binds, import_systemd_env, metrics};

/// Write end of the SIGCHLD self-pipe. The async-signal-safe reaper
/// handler `write(2)`s one `(pid, raw_status)` record per reaped child
/// here; the calloop drain source reads them back on the main thread.
/// `-1` until [`install_sigchld_autoreap`] sets it.
static REAP_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum BackendKind {
    /// Nested winit window for dev work inside an existing X11/Wayland session.
    Winit,
    /// Native DRM/KMS + libinput + libseat backend for running from a TTY.
    Tty,
}

#[derive(Debug, Parser)]
#[command(name = "shoestring-wm", version, about)]
struct Cli {
    /// Path to a TOML config file. Defaults to $XDG_CONFIG_HOME/shoestring-wm/config.toml.
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Which backend to launch. Auto-detected from the environment if omitted:
    /// `tty` when WAYLAND_DISPLAY and DISPLAY are both unset, `winit` otherwise.
    #[arg(short, long, value_enum)]
    backend: Option<BackendKind>,

    /// Optional client command to spawn once the compositor is up.
    /// Defaults to `weston-terminal` if available.
    #[arg(short = 'C', long = "command")]
    command: Option<String>,

    /// Write the bundled default config to the user's config path (or to
    /// `--config PATH` if given) and exit. Refuses to overwrite an existing
    /// file unless `--force` is also passed.
    #[arg(long)]
    write_default_config: bool,

    /// Allow `--write-default-config` to overwrite an existing file.
    #[arg(long)]
    force: bool,

    /// Validate a config file and exit. Parses the file at `--config PATH`
    /// (or the default path), then compiles the keybindings and reports any
    /// problems. Exits non-zero on a parse error or a missing file. Touches
    /// nothing and starts no compositor — safe to run against a live session.
    #[arg(long)]
    check_config: bool,

    /// Force the runtime automation gate ON at startup, overriding
    /// `general.automation_enabled` from the config. Off by default so
    /// remote-automation IPC methods (key/text/click injection, future
    /// remote screenshot + command exec) refuse to fire. The flag only
    /// forces ON; `set_automation` IPC can still flip the gate at
    /// runtime.
    #[arg(long)]
    enable_automation: bool,
}

/// Initialise tracing. Writes to stderr by default; if `SHOESTRING_WM_LOG`
/// is set, appends to that file instead (with ANSI escapes disabled so the
/// file stays grep-friendly). The file case is the practical way to debug
/// the TTY backend, where stderr scrolls past on the console.
fn init_tracing() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match std::env::var_os("SHOESTRING_WM_LOG") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open SHOESTRING_WM_LOG path");
            tracing_subscriber::fmt()
                .with_env_filter(env)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(env).init();
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing();
    // Defense-in-depth before anything opens fds: a higher ceiling buys the
    // leak detector more time to warn. Secondary to detection — see metrics.rs.
    metrics::raise_fd_limit();
    let reap_read_fd = install_sigchld_autoreap();

    if cli.write_default_config {
        return write_default_config(cli.config.as_deref(), cli.force);
    }

    if cli.check_config {
        return check_config(cli.config.as_deref());
    }

    let (config, config_path) = shoestring_config::load_or_default(cli.config.as_deref())?;
    match &config_path {
        Some(p) => tracing::info!(path = %p.display(), "loaded config"),
        None => tracing::info!("no config file found; using built-in defaults"),
    }

    let mut event_loop: EventLoop<'static, ShoestringWm> = EventLoop::try_new()?;
    let display: Display<ShoestringWm> = Display::new()?;

    let mut state = ShoestringWm::new(&mut event_loop, display, config, config_path);

    // Drain reaped-child statuses (pushed by the SIGCHLD handler) on the
    // main thread so remote_command / remote_screenshot can collect a real
    // exit status without racing the reaper to ECHILD.
    register_reaper_drain(&state.loop_handle, reap_read_fd)?;

    if cli.enable_automation && !state.automation_enabled {
        // Couples the screen-capture gate on too (automation is a superset),
        // so a session started with --enable-automation can screenshot without
        // a separate `screen-capture on`. No subscribers exist yet, so the
        // coupled ScreenCaptureChanged event is intentionally not emitted here.
        state.set_automation(true);
        tracing::info!("automation gate forced on by --enable-automation");
    }

    let backend = cli.backend.unwrap_or_else(|| {
        let in_session =
            std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
        if in_session {
            BackendKind::Winit
        } else {
            BackendKind::Tty
        }
    });
    tracing::info!(?backend, "starting backend");

    // Only the real session compositor (TTY/udev) integrates with the
    // surrounding session. The nested winit backend runs inside another
    // compositor and must not push its WAYLAND_DISPLAY/DISPLAY into the
    // shared systemd user manager — doing so retargets that session's
    // socket-activated services (portals, ssh-tpm-agent, ...) at the
    // nested instance's now-throwaway displays.
    state.session_integration = matches!(backend, BackendKind::Tty);

    match backend {
        BackendKind::Winit => {
            #[cfg(feature = "winit")]
            {
                backend::winit::init_winit(&mut event_loop, &mut state)?;
            }
            #[cfg(not(feature = "winit"))]
            anyhow::bail!("winit backend not compiled in");
        }
        BackendKind::Tty => {
            #[cfg(feature = "tty")]
            {
                backend::udev::init_udev(&mut event_loop, &mut state)?;
            }
            #[cfg(not(feature = "tty"))]
            anyhow::bail!("tty backend not compiled in");
        }
    }

    // Point child processes at our socket. Process-env only, so this is
    // safe even when nested — it affects this WM and its children, not the
    // parent session.
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);

    // Push WAYLAND_DISPLAY into the systemd user manager so D-Bus-activated
    // services (xdg-desktop-portal, notification daemons, etc.) see it. The
    // session wrapper already imported the static variables (XDG_CURRENT_DESKTOP,
    // XDG_SESSION_TYPE, PATH); this covers the dynamic one set just above.
    // DISPLAY is imported separately in xwayland.rs once XWayland is ready.
    // Skipped when nested so we don't clobber the host session's environment.
    if state.session_integration {
        import_systemd_env(&["WAYLAND_DISPLAY"]);
    } else {
        tracing::info!("nested backend: skipping systemd WAYLAND_DISPLAY import");
    }

    // IPC socket goes up after WAYLAND_DISPLAY is exported so
    // default_socket_path() can resolve it.
    state.start_ipc();

    // Diagnostics sampler (fd/RSS metrics + leak detector). After IPC so a
    // stream subscriber has a socket to attach to; harmless if disabled.
    state.start_metrics();

    // XWayland goes up before autostart so X11 apps in the autostart
    // list (and any first client launched from a terminal that inherits
    // our env) find $DISPLAY when they reach for it.
    state.start_xwayland();

    // Config hot-reload watcher. No-op when launched without a config file.
    state.start_config_watcher();

    tracing::info!(
        socket = ?state.socket_name,
        version = env!("CARGO_PKG_VERSION"),
        "shoestring-wm ready",
    );

    // Tell systemd we're up. No-op unless launched as a `Type=notify`
    // service (i.e. `$NOTIFY_SOCKET` is set), so it's safe in every other
    // launch path — display-manager session, nested dev, non-systemd. Done
    // here, after the wayland socket + IPC are listening and the env is
    // pushed into the user manager, so units ordered `After=` us don't race
    // `$WAYLAND_DISPLAY`. Must come before autostart: it also strips
    // `$NOTIFY_SOCKET` so spawned children don't inherit it.
    sd_notify_ready();

    for entry in &state.config.general.autostart {
        spawn_client(entry);
    }

    if let Some(cmd) = cli.command.as_deref() {
        spawn_client(cmd);
    }

    // After each batch of dispatched events (client commits, input, IPC
    // actions, timers) re-evaluate what sits under a *stationary* pointer.
    // Surfaces that mapped/unmapped/moved without a real pointer motion still
    // owe the client enter/leave/motion; the input handlers only refocus on
    // actual motion, so this is the catch-all. Cheap when nothing changed.
    event_loop.run(None, &mut state, |state| {
        state.refresh_pointer_focus();
    })?;
    Ok(())
}

fn write_default_config(path: Option<&std::path::Path>, force: bool) -> Result<()> {
    let target = path
        .map(std::path::PathBuf::from)
        .or_else(shoestring_config::default_config_path)
        .ok_or_else(|| {
            anyhow::anyhow!("no config path: pass --config or set $HOME/$XDG_CONFIG_HOME")
        })?;

    if target.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite",
            target.display()
        );
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, shoestring_config::default_config_toml())?;
    println!("wrote default config to {}", target.display());
    Ok(())
}

/// Validate a config file without starting the compositor. Resolves the
/// path the same way startup does, but — unlike `load_or_default` — treats a
/// missing file as an error rather than silently falling back to defaults, so
/// a typo'd `--config` path can't masquerade as "valid". On success prints a
/// one-line summary and any non-fatal binding warnings (unknown keysyms /
/// modifiers), exactly the warnings the WM would log at startup.
fn check_config(path: Option<&std::path::Path>) -> Result<()> {
    let target = path
        .map(std::path::PathBuf::from)
        .or_else(shoestring_config::default_config_path)
        .ok_or_else(|| {
            anyhow::anyhow!("no config path: pass --config or set $HOME/$XDG_CONFIG_HOME")
        })?;

    if !target.exists() {
        anyhow::bail!("{} does not exist", target.display());
    }

    let config = shoestring_config::load_from(&target)
        .map_err(|e| anyhow::anyhow!("{}: {e}", target.display()))?;

    // Parsing succeeded. Compiling the bindings is the only other place the
    // config can be "wrong" in a way worth flagging — bad keysyms, unknown
    // modifiers — so surface those warnings here too. They are non-fatal: the
    // WM drops the offending bind and runs, so `--check-config` still exits 0.
    let (_table, warnings) = binds::BindingTable::compile(&config);
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    let regex_errors = config.window_rule_regex_errors();
    for e in &regex_errors {
        eprintln!("warning: {e}");
    }
    println!(
        "{}: OK — {} binding(s), {} warning(s)",
        target.display(),
        config.bindings.len(),
        warnings.len() + regex_errors.len()
    );
    Ok(())
}

/// Reap exited helper children (autostart, bar, menus, ...) so they never
/// become zombies. We spawn these with `Command::spawn` and drop the
/// `Child`; without reaping they sit as `<defunct>` in `ps` forever.
///
/// Implementation: a SIGCHLD handler that calls `waitpid(-1, WNOHANG)` in a
/// loop. This reaps any child that has already exited without blocking, and
/// crucially does NOT break `waitpid(specific_pid, 0)` calls in smithay's
/// XWayland supervisor — those calls still see the child while it is alive.
///
/// The previous SA_NOCLDWAIT approach caused POSIX-specified breakage:
/// with SA_NOCLDWAIT set, *any* waitpid() call returns ECHILD immediately,
/// which panicked inside smithay when it waited on the XWayland process.
///
/// Because this global reaper also collects the children that
/// `remote_command` / `remote_screenshot` spawn, those helpers cannot call
/// `child.wait()` themselves — they would race the handler to ECHILD. So the
/// handler captures each child's raw wait-status and pushes a `(pid, status)`
/// record through a self-pipe; [`register_reaper_drain`] reads them back on
/// the main thread and routes the status to the matching pending request.
/// Returns the read end of that self-pipe for the caller to register.
fn install_sigchld_autoreap() -> RawFd {
    // Self-pipe: the handler writes reaped statuses to `write`, the calloop
    // drain reads them from `read`. O_NONBLOCK so a full pipe never blocks
    // the handler (it drops the record instead — see below); O_CLOEXEC so
    // children don't inherit it.
    let mut fds = [0 as RawFd; 2];
    let read_fd = unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) != 0 {
            let err = std::io::Error::last_os_error();
            // Without the pipe we fall back to reap-only (no status routing);
            // remote_command / remote_screenshot will then hang waiting for a
            // status that never arrives. Loud warning, but keep running.
            tracing::error!(error = %err, "SIGCHLD self-pipe creation failed; remote command/screenshot status routing disabled");
            -1
        } else {
            REAP_WRITE_FD.store(fds[1], Ordering::SeqCst);
            fds[0]
        }
    };

    // SAFETY: the handler only calls async-signal-safe functions
    // (waitpid, write, atomic load). We install it once before children
    // are spawned.
    unsafe extern "C" fn sigchld_handler(_: libc::c_int) {
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            let wfd = REAP_WRITE_FD.load(Ordering::SeqCst);
            if wfd >= 0 {
                // One record: [pid: i32, raw_status: i32], native-endian.
                // 8 bytes ≤ PIPE_BUF, so the write is atomic; a failed or
                // short write (e.g. EAGAIN on a full pipe) just drops the
                // record — acceptable, the child is already reaped.
                let record: [libc::c_int; 2] = [pid, status];
                unsafe {
                    libc::write(wfd, record.as_ptr() as *const libc::c_void, 8);
                }
            }
        }
    }

    use std::mem::MaybeUninit;
    unsafe {
        let mut sa: libc::sigaction = MaybeUninit::zeroed().assume_init();
        sa.sa_sigaction = sigchld_handler as *const () as libc::sighandler_t;
        sa.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut sa.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut()) != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(error = %err, "install_sigchld_autoreap failed; expect <defunct> in ps");
        }
    }

    read_fd
}

/// Register the read end of the SIGCHLD self-pipe with calloop. On each
/// wakeup it drains all buffered `(pid, raw_status)` records and hands each
/// to [`ShoestringWm::note_child_reaped`], which routes it to the matching
/// pending remote command/screenshot (or ignores fire-and-forget children).
fn register_reaper_drain(handle: &LoopHandle<'static, ShoestringWm>, read_fd: RawFd) -> Result<()> {
    if read_fd < 0 {
        // Self-pipe creation failed earlier; nothing to drain.
        return Ok(());
    }
    // SAFETY: read_fd is the unique owner of the pipe read end (the handler
    // only ever holds the write end). calloop takes ownership via OwnedFd.
    let owned = unsafe { OwnedFd::from_raw_fd(read_fd) };
    let source = Generic::new(owned, Interest::READ, Mode::Level);
    // Records can straddle reads, so carry an inter-callback remainder.
    let mut leftover: Vec<u8> = Vec::new();
    handle
        .insert_source(source, move |_, fd, state| {
            let raw = fd.as_raw_fd();
            let mut buf = [0u8; 256];
            loop {
                let n =
                    unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n > 0 {
                    leftover.extend_from_slice(&buf[..n as usize]);
                } else if n == 0 {
                    break; // EOF: write end gone (process shutting down).
                } else {
                    let err = std::io::Error::last_os_error();
                    match err.kind() {
                        std::io::ErrorKind::WouldBlock => break,
                        std::io::ErrorKind::Interrupted => continue,
                        _ => {
                            tracing::warn!(error = %err, "reaper drain read failed");
                            break;
                        }
                    }
                }
            }
            while leftover.len() >= 8 {
                let pid = i32::from_ne_bytes([leftover[0], leftover[1], leftover[2], leftover[3]]);
                let status =
                    i32::from_ne_bytes([leftover[4], leftover[5], leftover[6], leftover[7]]);
                leftover.drain(..8);
                state.note_child_reaped(pid, ExitStatus::from_raw(status));
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| anyhow::anyhow!("insert reaper drain source: {e}"))?;
    Ok(())
}

/// Signal readiness to systemd via the `sd_notify` protocol (`READY=1`).
///
/// systemd sets `$NOTIFY_SOCKET` for a `Type=notify` service and holds units
/// ordered `After=` us in the "activating" state until we send this datagram;
/// without it they start the instant we `exec`, racing `$WAYLAND_DISPLAY`.
/// niri/cosmic-comp do the same. A no-op when `$NOTIFY_SOCKET` is unset — the
/// display-manager session path, nested dev instances, and non-systemd systems
/// all leave it unset — so it is always safe to call unconditionally.
///
/// Hand-rolled over libc rather than pulling in the `sd_notify` crate: the
/// protocol is a single datagram to an `AF_UNIX`/`SOCK_DGRAM` socket and we
/// already lean on libc throughout. After signalling we drop `$NOTIFY_SOCKET`
/// from our environment (systemd convention) so spawned children — autostart
/// entries, helpers — don't inherit it and accidentally notify the manager on
/// our behalf.
fn sd_notify_ready() {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    // Children must not inherit NOTIFY_SOCKET; strip it whether or not the
    // send below succeeds.
    std::env::remove_var("NOTIFY_SOCKET");

    let path = socket.as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let cap = std::mem::size_of_val(&addr.sun_path);
    // Path-based names need room for a trailing NUL; abstract names (leading
    // '@', mapped to a NUL byte) use the leading slot instead. Either way the
    // name must fit within sun_path.
    if path.is_empty() || path.len() >= cap {
        tracing::warn!(
            "sd_notify: NOTIFY_SOCKET length {} out of range for sun_path",
            path.len()
        );
        return;
    }

    let base = std::mem::size_of::<libc::sa_family_t>();
    let dst = addr.sun_path.as_mut_ptr() as *mut u8;
    let addrlen = unsafe {
        if path[0] == b'@' {
            // Abstract namespace: leading NUL (already zeroed), name follows.
            let name = &path[1..];
            std::ptr::copy_nonoverlapping(name.as_ptr(), dst.add(1), name.len());
            (base + 1 + name.len()) as libc::socklen_t
        } else {
            std::ptr::copy_nonoverlapping(path.as_ptr(), dst, path.len());
            // Include the trailing NUL in the address length for pathname sockets.
            (base + path.len() + 1) as libc::socklen_t
        }
    };

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "sd_notify: socket() failed");
        return;
    }
    let msg = b"READY=1\n";
    let sent = unsafe {
        libc::sendto(
            fd,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
            libc::MSG_NOSIGNAL,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addrlen,
        )
    };
    unsafe {
        libc::close(fd);
    }
    if sent < 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "sd_notify: sendto() failed");
    } else {
        tracing::info!("sd_notify READY=1 sent");
    }
}

fn spawn_client(command: &str) {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else { return };
    let args: Vec<&str> = parts.collect();
    match std::process::Command::new(program).args(&args).spawn() {
        Ok(child) => tracing::info!(pid = child.id(), %command, "spawned client"),
        Err(e) => tracing::warn!(%command, error = %e, "failed to spawn client"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    /// `sd_notify_ready()` sends exactly `READY=1\n` to the path-based socket
    /// named by `$NOTIFY_SOCKET`, then strips the variable so children don't
    /// inherit it; with the variable unset it is a no-op. Both cases live in
    /// one test because `$NOTIFY_SOCKET` is process-global and cargo runs
    /// tests in parallel.
    #[test]
    fn sd_notify_ready_behavior() {
        // Unset → no-op, no panic.
        std::env::remove_var("NOTIFY_SOCKET");
        sd_notify_ready();

        // Set → datagram delivered, then env stripped.
        let path = std::env::temp_dir().join(format!("shoestring-sdnotify-{}.sock", unsafe {
            libc::getpid()
        }));
        let _ = std::fs::remove_file(&path);
        let listener = UnixDatagram::bind(&path).expect("bind notify socket");

        std::env::set_var("NOTIFY_SOCKET", &path);
        sd_notify_ready();

        assert!(std::env::var_os("NOTIFY_SOCKET").is_none());

        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).expect("recv READY datagram");
        assert_eq!(&buf[..n], b"READY=1\n");

        let _ = std::fs::remove_file(&path);
    }
}
