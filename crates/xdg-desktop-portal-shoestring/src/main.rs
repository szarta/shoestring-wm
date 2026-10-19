//! `xdg-desktop-portal-shoestring` — the screencast portal backend for the
//! shoestring-wm session.
//!
//! Background: screen sharing on Wayland goes app → xdg-desktop-portal
//! (frontend) → an *implementation* backend → PipeWire. The usual wlroots
//! backend (xdg-desktop-portal-wlr) offers the PipeWire consumer a single,
//! dmabuf-only buffer type and never falls back to shm. That breaks Zoom,
//! which renders our dmabuf black (see task 150 / 157). This backend streams
//! shm by default and offers dmabuf only behind `[portal] screencast_dmabuf`.
//!
//! It is a *separate process* from the compositor (so a PipeWire/D-Bus fault
//! can't take down the desktop) and pulls frames from the WM over the
//! existing `zwlr_screencopy_v1` protocol, which already serves both buffer
//! types per frame. The compositor itself needs no changes.
//!
//! This file owns the reactor-less dispatch loop (a single `poll(2)` over the
//! D-Bus socket, a shutdown self-pipe, the PipeWire loop, and each active
//! cast's WM connection while a capture is in flight, timing out at the next
//! frame-pacing deadline), mirroring shoestring-notify. The portal protocol
//! lives in [`portal`]; PipeWire + frame pacing in [`cast`]; the screencopy
//! client in [`capture`].

mod capture;
mod cast;
mod chooser;
mod portal;
mod screenshot;
mod state;

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use rustbus::connection::ll_conn::force_finish_on_error;
use rustbus::message_builder::MarshalledMessage;
use rustbus::{peer, MessageType, RpcConn};

use crate::state::State;

/// Read end of the self-pipe written by SIGINT/SIGTERM handlers. Stored in an
/// atomic so the async-signal-safe handler only does a bare `libc::write`.
static SHUTDOWN_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

fn main() -> Result<()> {
    init_logging();

    if let Some(action) = parse_cli()? {
        match action {
            CliAction::Help => print!("{}", help_text()),
            CliAction::Version => {
                println!(
                    "xdg-desktop-portal-shoestring {}",
                    env!("CARGO_PKG_VERSION")
                )
            }
            CliAction::SelftestDmabuf(path) => {
                capture::selftest_dmabuf(None, std::path::Path::new(&path))?;
                println!("{path}");
            }
        }
        return Ok(());
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "xdg-desktop-portal-shoestring starting"
    );

    // SAFETY: pipewire::init() is the library's global init; must run once
    // before any other PipeWire call.
    pipewire::init();
    let pw = cast::Pw::new()?;
    tracing::info!("connected to PipeWire daemon");

    let rpc = portal::acquire_bus()?;
    let mut state = State::new(rpc, pw);

    // Live exactly as long as our compositor: hold one idle connection to it
    // and exit when it hangs up. Otherwise the portal survives a logout (the
    // systemd user manager and session bus persist) still owning our bus name,
    // and the next session's portal can't acquire it. We deliberately don't
    // take the name over instead: a nested or headless shoestring-wm started
    // inside a session autostarts its own portal, which must keep failing to
    // acquire rather than steal the session's. No compositor reachable ⇒ run
    // unwatched, as before.
    let compositor = match wayland_client::Connection::connect_to_env() {
        Ok(conn) => Some(conn),
        Err(e) => {
            tracing::warn!(error = %e, "no compositor to watch; portal lifetime not tied to it");
            None
        }
    };

    // Only look at calls aimed at our object tree (plus D-Bus peer pings).
    state.rpc.set_filter(Box::new(|msg| match msg.typ {
        MessageType::Call => peer::filter_peer(&msg.dynheader) || portal::aimed_at_us(msg),
        MessageType::Signal | MessageType::Reply | MessageType::Error => true,
        MessageType::Invalid => false,
    }));

    let (shutdown_r, shutdown_w) = make_pipe().context("create shutdown pipe")?;
    SHUTDOWN_PIPE_WRITE.store(shutdown_w, Ordering::SeqCst);
    install_signal_handlers()?;

    let dbus_fd = state.rpc.conn().as_raw_fd();
    let pw_fd = state.pw.loop_fd();
    run_loop(&mut state, dbus_fd, shutdown_r, pw_fd, compositor.as_ref())?;

    tracing::info!("xdg-desktop-portal-shoestring exiting cleanly");
    Ok(())
}

fn run_loop(
    state: &mut State,
    dbus_fd: RawFd,
    shutdown_r: RawFd,
    pw_fd: RawFd,
    compositor: Option<&wayland_client::Connection>,
) -> Result<()> {
    // poll(2) ignores a negative fd, so an unwatched portal just never wakes
    // on slot 3.
    let compositor_fd = compositor.map_or(-1, |c| c.backend().poll_fd().as_raw_fd());
    while state.running {
        // Order: dbus, shutdown, pipewire, the compositor we live with, then
        // one per cast with a capture in flight (its WM connection).
        let mut fds = vec![
            libc::pollfd {
                fd: dbus_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: shutdown_r,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pw_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: compositor_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut cast_fds: Vec<String> = Vec::new();
        for (session, cast) in &state.casts {
            if let Some(fd) = cast.poll_fd() {
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
                cast_fds.push(session.clone());
            }
        }
        // Wake for the earliest frame-pacing deadline (rounded up, so we
        // don't spin on a sub-millisecond remainder); otherwise block.
        let now = Instant::now();
        let timeout = state
            .casts
            .values()
            .filter_map(|c| c.timeout(now))
            .min()
            .map(|d| d.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32)
            .unwrap_or(-1);

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow!("poll failed: {err}"));
        }

        // Shutdown short-circuits.
        if fds[1].revents & libc::POLLIN != 0 {
            tracing::debug!("shutdown signal received");
            return Ok(());
        }
        // Our compositor went away (logout, crash, nested WM closed): exit,
        // releasing the bus name for the next session's portal.
        if let Some(conn) = compositor {
            if compositor_gone(conn, fds[3].revents) {
                tracing::info!("compositor connection closed; exiting");
                return Ok(());
            }
        }
        // D-Bus.
        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            drain_and_dispatch(state)?;
        }
        // PipeWire — pump its loop so stream negotiation + the process
        // callback (which fills/queues buffers) run.
        if fds[2].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            state.pw.iterate();
        }
        // WM connections with a capture in flight. Looked up by session, as
        // D-Bus dispatch above may have closed one.
        for (pfd, session) in fds[4..].iter().zip(&cast_fds) {
            if pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                if let Some(cast) = state.casts.get(session) {
                    cast.on_readable();
                }
            }
        }
        // Start whatever frames are now due.
        let now = Instant::now();
        for cast in state.casts.values() {
            cast.tick(now);
        }
    }
    Ok(())
}

/// Whether the watched compositor connection has hung up. We never send on
/// it, so readability means the compositor sent something (rare — a display
/// error) or closed: read to tell which, treating only a failed read as gone.
fn compositor_gone(conn: &wayland_client::Connection, revents: libc::c_short) -> bool {
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if revents & libc::POLLIN == 0 {
        return false;
    }
    match conn.prepare_read() {
        Some(guard) => match guard.read() {
            Ok(_) => false,
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                false
            }
            Err(_) => true,
        },
        // Events already queued from an earlier read; nothing to learn.
        None => false,
    }
}

fn drain_and_dispatch(state: &mut State) -> Result<()> {
    let auto_errors = state
        .rpc
        .refill_all()
        .map_err(|e| anyhow!("refill_all: {e:?}"))?;
    for mut reply in auto_errors {
        send_message(&mut state.rpc, &mut reply)?;
    }

    while let Some(call) = state.rpc.try_get_call() {
        if peer::handle_peer_message(&call, state.rpc.conn_mut())
            .map_err(|e| anyhow!("handle peer: {e:?}"))?
        {
            continue;
        }
        if let Some(mut reply) = portal::dispatch(&call, state) {
            send_message(&mut state.rpc, &mut reply)?;
        }
    }
    Ok(())
}

fn send_message(rpc: &mut RpcConn, msg: &mut MarshalledMessage) -> Result<()> {
    rpc.send_message(msg)
        .map_err(|e| anyhow!("queue message: {e:?}"))?
        .write_all()
        .map_err(force_finish_on_error)
        .map_err(|e| anyhow!("write message: {e:?}"))?;
    Ok(())
}

fn make_pipe() -> std::io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn install_signal_handlers() -> Result<()> {
    extern "C" fn handler(_sig: libc::c_int) {
        let fd = SHUTDOWN_PIPE_WRITE.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte: u8 = 1;
            unsafe {
                libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = handler as *const () as usize;
    sa.sa_flags = libc::SA_RESTART;
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
    }
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let rc = unsafe { libc::sigaction(sig, &sa, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(anyhow!(
                "sigaction({sig}): {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

// ---- logging + CLI -------------------------------------------------------

fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("valid default log filter");
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

enum CliAction {
    Help,
    Version,
    /// Hidden developer aid: capture the screen into a dmabuf and write a PNG.
    SelftestDmabuf(String),
}

/// Hand-parse argv; staying lean is a project goal (notify/bar do the same).
/// Returns `None` to run the daemon.
fn parse_cli() -> Result<Option<CliAction>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => Ok(None),
        Some("-h" | "--help") => Ok(Some(CliAction::Help)),
        Some("-V" | "--version") => Ok(Some(CliAction::Version)),
        Some("--selftest-dmabuf") => {
            let path = args
                .get(1)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("--selftest-dmabuf needs a PNG output path"))?;
            Ok(Some(CliAction::SelftestDmabuf(path)))
        }
        Some(other) => anyhow::bail!("unknown argument: {other:?} (try --help)"),
    }
}

fn help_text() -> String {
    format!(
        "xdg-desktop-portal-shoestring {ver} — ScreenCast portal backend for shoestring-wm.

Usage: xdg-desktop-portal-shoestring [OPTIONS]

Run with no options to start the backend; it claims
org.freedesktop.impl.portal.desktop.shoestring on the session bus and serves
the xdg-desktop-portal frontend's ScreenCast requests.

Options:
  -h, --help       Print this help and exit.
  -V, --version    Print version and exit.

Environment: DBUS_SESSION_BUS_ADDRESS (session bus), WAYLAND_DISPLAY (the WM
to capture from), RUST_LOG (tracing filter).
",
        ver = env!("CARGO_PKG_VERSION"),
    )
}
