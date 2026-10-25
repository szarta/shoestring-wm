//! The listener remote clients connect to, and who may use it.
//!
//! By default the server listens on a Unix socket in `$XDG_RUNTIME_DIR`,
//! mode 0600, and accepts a connection only from a process running as the
//! same uid. A viewer on another machine reaches it with OpenSSH's socket
//! forwarding, which runs as the logged-in user, so ssh login is the
//! authentication:
//!
//! ```text
//! ssh -N -L "$XDG_RUNTIME_DIR/box.sock:/run/user/1000/shoestring-remote-wayland-1.sock" box
//! ```
//!
//! TCP is still available for setups that need it (`--tcp ADDR:PORT`), but
//! nothing checks who connects to it: anyone who can reach the port gets the
//! session, so the server warns at startup. A loopback address exposes the
//! session to local users only; any other address exposes it to every host
//! that can reach the port, so it takes a second opt-in (`--tcp-any-host`)
//! on top of `--tcp` ([`tcp_endpoint`]).

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use shoestring_remote::transport::Stream;

/// What to listen on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    /// Resolved once at startup, so the policy check, the bind and what the
    /// WM reports all name the same address.
    Tcp(SocketAddr),
}

impl Endpoint {
    /// `$XDG_RUNTIME_DIR/shoestring-remote-$WAYLAND_DISPLAY.sock`, named after
    /// the display like the WM's own IPC socket, so two WMs for one user don't
    /// collide. Without `WAYLAND_DISPLAY`, `shoestring-remote.sock`.
    pub fn default_unix() -> Result<Endpoint> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .context("XDG_RUNTIME_DIR is not set; pass --socket PATH")?;
        let name = match std::env::var("WAYLAND_DISPLAY") {
            Ok(d) if !d.is_empty() && !d.contains('/') => format!("shoestring-remote-{d}.sock"),
            _ => "shoestring-remote.sock".into(),
        };
        Ok(Endpoint::Unix(Path::new(&dir).join(name)))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Unix(p) => write!(f, "{}", p.display()),
            Endpoint::Tcp(a) => write!(f, "tcp {a}"),
        }
    }
}

/// An open listener. Dropping a Unix listener removes its socket file, so the
/// path exists only while the remote gate is on.
pub enum Listener {
    Unix {
        listener: UnixListener,
        path: PathBuf,
    },
    Tcp(TcpListener),
}

/// One accepted connection that passed the access check.
pub struct Accepted {
    pub stream: Stream,
    /// For logs: the socket path, or the TCP peer address.
    pub peer: String,
}

impl Listener {
    pub fn bind(endpoint: &Endpoint) -> Result<Listener> {
        let listener = match endpoint {
            Endpoint::Unix(path) => bind_unix(path)?,
            Endpoint::Tcp(addr) => {
                Listener::Tcp(TcpListener::bind(addr).with_context(|| format!("bind tcp {addr}"))?)
            }
        };
        match &listener {
            Listener::Unix { listener, .. } => listener.set_nonblocking(true),
            Listener::Tcp(listener) => listener.set_nonblocking(true),
        }
        .context("set listener non-blocking")?;
        Ok(listener)
    }

    /// Where this listener is, for logs: the socket path, or the TCP address
    /// it actually bound (the port a `:0` request was given).
    pub fn describe(&self) -> String {
        match self {
            Listener::Unix { path, .. } => path.display().to_string(),
            Listener::Tcp(listener) => match listener.local_addr() {
                Ok(addr) => format!("tcp {addr}"),
                Err(_) => "tcp (address unknown)".into(),
            },
        }
    }

    /// Accept one connection. `Ok(None)` when none is pending, and also when a
    /// Unix peer runs as another uid: that connection is closed before any
    /// byte is read or written.
    pub fn accept(&self) -> io::Result<Option<Accepted>> {
        let result = match self {
            Listener::Unix { listener, path } => {
                listener.accept().map(|(s, _)| accept_unix(s, path))
            }
            Listener::Tcp(listener) => listener.accept().map(|(s, peer)| {
                s.set_nodelay(true).ok();
                Some(Accepted {
                    stream: Stream::Tcp(s),
                    peer: peer.to_string(),
                })
            }),
        };
        match result {
            Ok(accepted) => Ok(accepted),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Listener::Unix { listener, .. } => listener.as_raw_fd(),
            Listener::Tcp(listener) => listener.as_raw_fd(),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Listener::Unix { path, .. } = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn accept_unix(stream: UnixStream, path: &Path) -> Option<Accepted> {
    // SAFETY: geteuid has no failure mode.
    let ours = unsafe { libc::geteuid() };
    match peer_uid(&stream) {
        Ok(uid) if uid == ours => Some(Accepted {
            stream: Stream::Unix(stream),
            peer: path.display().to_string(),
        }),
        Ok(uid) => {
            tracing::warn!(uid, ours, "refused a connection from another user");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "refused a connection whose uid can't be read");
            None
        }
    }
}

/// Bind `path` so that nobody but us can connect at any point: bind under a
/// temporary name in the same directory, restrict it to 0600, then rename it
/// into place. A socket already at `path` is replaced only if nothing answers
/// on it (a stale one from a crash); a live one means another server is
/// running, and anything that isn't a socket is left alone.
fn bind_unix(path: &Path) -> Result<Listener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                bail!("{} is in use by another remote server", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        Ok(_) => bail!("{} exists and is not a socket", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    }

    let tmp = path.with_file_name(format!(
        ".{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("socket"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
    let placed = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("place socket at {}", path.display()));
    }
    Ok(Listener::Unix {
        listener,
        path: path.to_owned(),
    })
}

/// The effective uid of the process on the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` describe a writable ucred, as SO_PEERCRED wants.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// The effective uid of the process on the other end of `stream`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: valid out-pointers for getpeereid.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

/// Resolve a `--tcp` address and apply the listening policy. `addr` is
/// `ADDR:PORT` or `HOST:PORT`; a host name is resolved now and the first
/// address is used from then on. A loopback address is accepted. Any other
/// address, `0.0.0.0` included, is refused unless `any_host` is set, because
/// every host that can reach it gets the session with no authentication.
pub fn tcp_endpoint(addr: &str, any_host: bool) -> Result<Endpoint> {
    let resolved = addr
        .to_socket_addrs()
        .with_context(|| format!("resolve tcp address {addr} (expected ADDR:PORT)"))?
        .next()
        .with_context(|| format!("tcp address {addr} resolves to nothing"))?;
    if !is_loopback(resolved.ip()) && !any_host {
        let named = if resolved.to_string() == addr {
            addr.to_owned()
        } else {
            format!("{addr} ({resolved})")
        };
        bail!(
            "refusing to listen on {named}: it is not a loopback address, so any host that \
             can reach the port could see and control this session, with nothing checking \
             who connects. Prefer the Unix socket reached over ssh -L. To listen there \
             anyway, also pass --tcp-any-host (or SHOESTRING_REMOTE_TCP_ANY_HOST=1)"
        );
    }
    Ok(Endpoint::Tcp(resolved))
}

/// Loopback, counting an IPv4-mapped IPv6 loopback (`::ffff:127.0.0.1`).
fn is_loopback(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// The startup warning for a TCP listener.
pub fn tcp_warning(addr: SocketAddr) -> &'static str {
    if is_loopback(addr.ip()) {
        "TCP listener: any local user can connect and see and control this session"
    } else {
        "TCP listener on a non-loopback address: any host that can reach it can see and \
         control this session"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sw-remote-listen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unix_socket_is_private_and_removed_on_drop() {
        let dir = temp_dir();
        let path = dir.join("s.sock");
        let listener = Listener::bind(&Endpoint::Unix(path.clone())).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        drop(listener);
        assert!(
            !path.exists(),
            "socket file must go when the listener closes"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_uid_peer_is_accepted() {
        let dir = temp_dir();
        let path = dir.join("s.sock");
        let listener = Listener::bind(&Endpoint::Unix(path.clone())).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let accepted = loop {
            if let Some(a) = listener.accept().unwrap() {
                break a;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(matches!(accepted.stream, Stream::Unix(_)));
        drop(listener);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peer_uid_reads_our_own_uid() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), unsafe { libc::geteuid() });
    }

    #[test]
    fn stale_socket_is_replaced_but_live_one_and_files_are_not() {
        let dir = temp_dir();
        let path = dir.join("s.sock");
        // Stale: a socket file nobody listens on.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let replaced = Listener::bind(&Endpoint::Unix(path.clone())).unwrap();
        // Live: a second server must refuse rather than steal it.
        let err = Listener::bind(&Endpoint::Unix(path.clone())).err().unwrap();
        assert!(err.to_string().contains("in use"), "{err}");
        drop(replaced);
        // Not a socket: leave it alone.
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(Listener::bind(&Endpoint::Unix(path.clone())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loopback_tcp_needs_no_second_opt_in() {
        for addr in ["127.0.0.1:7355", "[::1]:7355", "[::ffff:127.0.0.1]:7355"] {
            let endpoint = tcp_endpoint(addr, false).unwrap();
            assert_eq!(endpoint, Endpoint::Tcp(addr.parse().unwrap()));
        }
    }

    #[test]
    fn non_loopback_tcp_is_refused_without_the_second_opt_in() {
        for addr in ["0.0.0.0:7355", "[::]:7355", "192.0.2.7:7355"] {
            let err = tcp_endpoint(addr, false).unwrap_err().to_string();
            assert!(err.contains("not a loopback address"), "{err}");
            assert!(err.contains("--tcp-any-host"), "{err}");
            assert_eq!(
                tcp_endpoint(addr, true).unwrap(),
                Endpoint::Tcp(addr.parse().unwrap())
            );
        }
    }

    #[test]
    fn a_host_name_is_resolved_before_the_check() {
        let Endpoint::Tcp(addr) = tcp_endpoint("localhost:7355", false).unwrap() else {
            panic!("not tcp");
        };
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 7355);
        assert!(tcp_endpoint("not-an-address", true).is_err());
    }

    #[test]
    fn tcp_listener_reports_the_port_it_bound() {
        let listener = Listener::bind(&tcp_endpoint("127.0.0.1:0", false).unwrap()).unwrap();
        let described = listener.describe();
        assert!(described.starts_with("tcp 127.0.0.1:"), "{described}");
        assert!(!described.ends_with(":0"), "{described}");
    }

    #[test]
    fn tcp_warnings_name_the_exposure() {
        assert!(tcp_warning("127.0.0.1:7355".parse().unwrap()).contains("local user"));
        assert!(tcp_warning("0.0.0.0:7355".parse().unwrap()).contains("any host"));
    }
}
