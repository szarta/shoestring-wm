mod config;
mod dbus_iface;
mod render;
mod state;
mod wayland;

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rustbus::connection::ll_conn::force_finish_on_error;
use rustbus::message_builder::MarshalledMessage;
use rustbus::{peer, MessageType, RpcConn};
use wayland_client::{Connection, EventQueue, QueueHandle};

use crate::config::Config;
use crate::dbus_iface::{notification_closed_signal, DispatchOutcome};
use crate::state::CloseReason;
use crate::wayland::State;

/// Read end of the self-pipe written by SIGINT/SIGTERM handlers. We store
/// it in an atomic so the async-signal-safe handler can just call
/// `libc::write(fd, ..)` without touching any non-trivial state.
static SHUTDOWN_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "shoestring-notify starting"
    );

    let cfg = Config::default();
    let (conn, mut queue, qh, mut state) = wayland::init(cfg)?;

    // Filter rustbus traffic down to messages aimed at us (plus peer).
    state.rpc.set_filter(Box::new(|msg| match msg.typ {
        MessageType::Call => peer::filter_peer(&msg.dynheader) || aimed_at_us(msg),
        MessageType::Signal | MessageType::Reply | MessageType::Error => true,
        MessageType::Invalid => false,
    }));

    let (shutdown_r, shutdown_w) = make_pipe().context("create shutdown pipe")?;
    SHUTDOWN_PIPE_WRITE.store(shutdown_w, Ordering::SeqCst);
    install_signal_handlers()?;

    let timer_fd = create_timerfd().context("create expiry timerfd")?;
    let dbus_fd = state.rpc.conn().as_raw_fd();

    run_loop(
        &conn, &mut queue, &qh, &mut state, dbus_fd, shutdown_r, timer_fd,
    )?;

    tracing::info!("shoestring-notify exiting cleanly");
    Ok(())
}

fn aimed_at_us(msg: &MarshalledMessage) -> bool {
    msg.dynheader.object.as_deref() == Some(dbus_iface::OBJECT_PATH)
}

fn run_loop(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    qh: &QueueHandle<State>,
    state: &mut State,
    dbus_fd: RawFd,
    shutdown_r: RawFd,
    timer_fd: RawFd,
) -> Result<()> {
    while state.running {
        // Bring surfaces into sync with the queue, restack, paint.
        wayland::sync_surfaces(state, qh);

        // Flush wayland requests, prep for read.
        conn.flush()?;
        let guard = conn.prepare_read();
        let wayland_fd = guard.as_ref().map(|g| g.connection_fd().as_raw_fd());

        // Build the pollfd array. Order: dbus, shutdown, timer, [wayland].
        let mut fds: [libc::pollfd; 4] = unsafe { std::mem::zeroed() };
        let mut nfds: libc::nfds_t = 0;
        let mut wayland_idx: Option<usize> = None;
        for fd in [Some(dbus_fd), Some(shutdown_r), Some(timer_fd), wayland_fd]
            .into_iter()
            .flatten()
        {
            fds[nfds as usize] = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            nfds += 1;
        }
        if wayland_fd.is_some() {
            wayland_idx = Some((nfds - 1) as usize);
        }

        let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                drop(guard);
                continue;
            }
            return Err(anyhow!("poll failed: {err}"));
        }

        // 1. Shutdown short-circuits everything else.
        if fds[1].revents & libc::POLLIN != 0 {
            drop(guard);
            tracing::debug!("shutdown signal received");
            return Ok(());
        }

        // 2. dbus.
        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            drain_and_dispatch(state)?;
        }

        // 3. timerfd.
        if fds[2].revents & libc::POLLIN != 0 {
            drain_timerfd(timer_fd);
            handle_expirations(state)?;
        }

        // 4. wayland — must consume the prepared read regardless of
        //    whether it had POLLIN, otherwise the guard's prepare_read
        //    invariant gets held across the next iteration.
        if let Some(guard) = guard {
            let idx = wayland_idx.expect("wayland_idx set when guard exists");
            if fds[idx].revents & libc::POLLIN != 0 {
                if let Err(e) = guard.read() {
                    // WouldBlock here is benign — POLLIN can fire after
                    // wayland-rs already drained the fd inside prepare_read.
                    if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        tracing::warn!(error = ?e, "wayland read failed");
                    }
                }
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(state)?;

        // 5. Always rearm the timerfd from the current queue state. The
        //    dispatch above may have changed `next_expiry`.
        rearm_timerfd(timer_fd, state.queue.next_expiry(), Instant::now())?;
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
        let DispatchOutcome { reply, signals } =
            dbus_iface::dispatch(&call, &mut state.queue, Instant::now());
        if let Some(mut reply) = reply {
            send_message(&mut state.rpc, &mut reply)?;
        }
        for mut sig in signals {
            send_message(&mut state.rpc, &mut sig)?;
        }
    }
    Ok(())
}

fn handle_expirations(state: &mut State) -> Result<()> {
    let expired = state.queue.drain_expired(Instant::now());
    for id in expired {
        tracing::info!(id, "notification expired");
        let mut sig = notification_closed_signal(id, CloseReason::Expired);
        send_message(&mut state.rpc, &mut sig)?;
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

fn create_timerfd() -> std::io::Result<RawFd> {
    let fd = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// Rearm the timerfd to fire at `next_expiry`. Passing `None` disarms it.
///
/// We use relative arming (it_value = duration_from_now) rather than
/// absolute TFD_TIMER_ABSTIME because `std::time::Instant` is opaque —
/// we can't pull a CLOCK_MONOTONIC `timespec` out of it without going
/// via Duration math anyway. A 1ns floor handles "already in the past"
/// since the kernel treats a zero it_value as disarm.
fn rearm_timerfd(fd: RawFd, next_expiry: Option<Instant>, now: Instant) -> Result<()> {
    let it_value = match next_expiry {
        None => libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        Some(t) => {
            let d = t
                .checked_duration_since(now)
                .unwrap_or(Duration::from_nanos(1));
            let d = if d.is_zero() {
                Duration::from_nanos(1)
            } else {
                d
            };
            libc::timespec {
                tv_sec: d.as_secs() as libc::time_t,
                tv_nsec: d.subsec_nanos() as i64,
            }
        }
    };
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value,
    };
    let rc = unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(anyhow!(
            "timerfd_settime: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn drain_timerfd(fd: RawFd) {
    let mut buf = [0u8; 8];
    unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
    }
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
