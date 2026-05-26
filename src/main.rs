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

use crate::dbus_iface::{notification_closed_signal, DispatchOutcome};
use crate::state::{CloseReason, Queue};

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

    let mut rpc = dbus_iface::acquire_bus()?;
    let mut queue = Queue::new();

    // Filter out anything that isn't aimed at us. Calls that don't pass the
    // filter get an automatic UnknownMethod reply from RpcConn — fine for
    // stray broadcasts and accidental misroutes.
    rpc.set_filter(Box::new(|msg| match msg.typ {
        MessageType::Call => peer::filter_peer(&msg.dynheader) || aimed_at_us(msg),
        MessageType::Signal | MessageType::Reply | MessageType::Error => true,
        MessageType::Invalid => false,
    }));

    let (shutdown_r, shutdown_w) = make_pipe().context("create shutdown pipe")?;
    SHUTDOWN_PIPE_WRITE.store(shutdown_w, Ordering::SeqCst);
    install_signal_handlers()?;

    let timer_fd = create_timerfd().context("create expiry timerfd")?;
    let dbus_fd = rpc.conn().as_raw_fd();

    run_loop(&mut rpc, &mut queue, dbus_fd, shutdown_r, timer_fd)?;

    tracing::info!("shoestring-notify exiting cleanly");
    Ok(())
}

fn aimed_at_us(msg: &MarshalledMessage) -> bool {
    msg.dynheader.object.as_deref() == Some(dbus_iface::OBJECT_PATH)
}

fn run_loop(
    rpc: &mut RpcConn,
    queue: &mut Queue,
    dbus_fd: RawFd,
    shutdown_r: RawFd,
    timer_fd: RawFd,
) -> Result<()> {
    loop {
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
                fd: timer_fd,
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

        if fds[1].revents & libc::POLLIN != 0 {
            tracing::debug!("shutdown signal received");
            return Ok(());
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
            drain_and_dispatch(rpc, queue)?;
        }

        if fds[2].revents & libc::POLLIN != 0 {
            drain_timerfd(timer_fd);
            handle_expirations(rpc, queue)?;
        }

        rearm_timerfd(timer_fd, queue.next_expiry(), Instant::now())?;
    }
}

fn drain_and_dispatch(rpc: &mut RpcConn, queue: &mut Queue) -> Result<()> {
    // refill_all is nonblocking — it pulls every message currently buffered
    // in the kernel's socket, queues calls/signals/responses internally, and
    // returns auto-generated UnknownMethod replies for filtered-out calls
    // (we still have to actually send those).
    let auto_errors = rpc.refill_all().map_err(|e| anyhow!("refill_all: {e:?}"))?;
    for mut reply in auto_errors {
        send_message(rpc, &mut reply)?;
    }

    while let Some(call) = rpc.try_get_call() {
        // Peer interface (Ping, GetMachineId) is handled by rustbus itself.
        if peer::handle_peer_message(&call, rpc.conn_mut())
            .map_err(|e| anyhow!("handle peer: {e:?}"))?
        {
            continue;
        }

        let DispatchOutcome { reply, signals } = dbus_iface::dispatch(&call, queue, Instant::now());
        if let Some(mut reply) = reply {
            send_message(rpc, &mut reply)?;
        }
        for mut sig in signals {
            send_message(rpc, &mut sig)?;
        }
    }
    Ok(())
}

fn handle_expirations(rpc: &mut RpcConn, queue: &mut Queue) -> Result<()> {
    let expired = queue.drain_expired(Instant::now());
    for id in expired {
        tracing::info!(id, "notification expired");
        let mut sig = notification_closed_signal(id, CloseReason::Expired);
        send_message(rpc, &mut sig)?;
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
/// We use relative arming (it_value = duration_from_now) rather than absolute
/// TFD_TIMER_ABSTIME, because `std::time::Instant` is opaque — we can't pull
/// a CLOCK_MONOTONIC `timespec` out of it without going via Duration math
/// anyway. A 1ns floor handles "already in the past" (kernel rejects a zero
/// it_value as a disarm).
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

/// Read the 8-byte expiration counter so the fd becomes unreadable again.
/// We don't actually care about the count — `drain_expired` will fish out
/// every notification whose deadline has passed.
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
