//! `xdg-desktop-portal-shoestring` — the screencast portal backend for the
//! shoestring-wm session.
//!
//! Background: screen sharing on Wayland goes app → xdg-desktop-portal
//! (frontend) → an *implementation* backend → PipeWire. The usual wlroots
//! backend (xdg-desktop-portal-wlr) offers the PipeWire consumer a single,
//! dmabuf-only buffer type and never falls back to shm. That breaks Zoom,
//! whose bundled Mesa can't import our dmabuf (see task 150 / 157). This
//! backend fixes it by offering the consumer **both** dmabuf and shm and
//! letting it pick: Zoom negotiates shm, Chrome/OBS keep zero-copy dmabuf.
//!
//! It is a *separate process* from the compositor (so a PipeWire/D-Bus fault
//! can't take down the desktop) and pulls frames from the WM over the
//! existing `zwlr_screencopy_v1` protocol, which already serves both buffer
//! types per frame. The compositor itself needs no changes.
//!
//! This file owns the reactor-less dispatch loop (a single `poll(2)` over the
//! D-Bus socket + a shutdown self-pipe), mirroring shoestring-notify. The
//! portal protocol lives in [`portal`]; PipeWire + the screencopy client land
//! in later phases.

mod capture;
mod cast;
mod portal;
mod state;

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

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
    run_loop(&mut state, dbus_fd, shutdown_r, pw_fd)?;

    tracing::info!("xdg-desktop-portal-shoestring exiting cleanly");
    Ok(())
}

fn run_loop(state: &mut State, dbus_fd: RawFd, shutdown_r: RawFd, pw_fd: RawFd) -> Result<()> {
    while state.running {
        // Order: dbus, shutdown, pipewire.
        let mut fds = [
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
        ];

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
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
        // D-Bus.
        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            drain_and_dispatch(state)?;
        }
        // PipeWire — pump its loop so stream negotiation + the process
        // callback (which fills/queues buffers) run.
        if fds[2].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            state.pw.iterate();
        }
    }
    Ok(())
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
}

/// Hand-parse argv; staying lean is a project goal (notify/bar do the same).
/// Returns `None` to run the daemon.
fn parse_cli() -> Result<Option<CliAction>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => Ok(None),
        Some("-h" | "--help") => Ok(Some(CliAction::Help)),
        Some("-V" | "--version") => Ok(Some(CliAction::Version)),
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
