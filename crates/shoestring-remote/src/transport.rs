//! The byte stream the wire protocol rides on: a Unix socket (the default,
//! reached from another machine with OpenSSH's socket forwarding) or, when a
//! setup explicitly asks for it, TCP.
//!
//! Both ends only need a duplex stream they can clone, shut down, switch to
//! non-blocking, and poll, so [`Stream`] wraps either kind behind those
//! operations. Who may connect is the server's business (it checks a Unix
//! peer's uid); this module only moves bytes.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// A connected protocol stream.
#[derive(Debug)]
pub enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

/// Where to connect: a Unix socket path, or a TCP `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Unix(std::path::PathBuf),
    Tcp(String),
}

impl Target {
    /// Read a `--connect` argument: anything containing a `/` is a socket
    /// path (write `./name.sock` for one in the current directory), anything
    /// else is `host:port`.
    pub fn parse(s: &str) -> Target {
        if s.contains('/') {
            Target::Unix(s.into())
        } else {
            Target::Tcp(s.to_owned())
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Unix(p) => write!(f, "{}", p.display()),
            Target::Tcp(a) => f.write_str(a),
        }
    }
}

impl Stream {
    /// Connect to `target`. TCP streams get `TCP_NODELAY`: the protocol sends
    /// small input events that shouldn't wait on Nagle.
    pub fn connect(target: &Target) -> io::Result<Stream> {
        match target {
            Target::Unix(path) => Ok(Stream::Unix(UnixStream::connect(path)?)),
            Target::Tcp(addr) => {
                let s = TcpStream::connect(addr)?;
                s.set_nodelay(true).ok();
                Ok(Stream::Tcp(s))
            }
        }
    }

    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
        })
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.shutdown(how),
            Stream::Tcp(s) => s.shutdown(how),
        }
    }

    /// Bound how long a blocking read waits (`None`: forever). Applies to the
    /// socket, so every clone of this stream shares it.
    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.set_read_timeout(timeout),
            Stream::Tcp(s) => s.set_read_timeout(timeout),
        }
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.set_nonblocking(nonblocking),
            Stream::Tcp(s) => s.set_nonblocking(nonblocking),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

impl AsRawFd for Stream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            Stream::Unix(s) => s.as_raw_fd(),
            Stream::Tcp(s) => s.as_raw_fd(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_addresses_parse_apart() {
        assert_eq!(
            Target::parse("/run/user/1000/sw.sock"),
            Target::Unix("/run/user/1000/sw.sock".into())
        );
        assert_eq!(Target::parse("./sw.sock"), Target::Unix("./sw.sock".into()));
        assert_eq!(
            Target::parse("127.0.0.1:7355"),
            Target::Tcp("127.0.0.1:7355".into())
        );
        assert_eq!(Target::parse("box:7355"), Target::Tcp("box:7355".into()));
    }

    #[test]
    fn unix_stream_round_trips_and_clones() {
        let (a, b) = UnixStream::pair().unwrap();
        let a = Stream::Unix(a);
        let mut b = Stream::Unix(b);
        let mut a2 = a.try_clone().unwrap();
        a2.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        a.shutdown(Shutdown::Both).unwrap();
        assert_eq!(b.read(&mut buf).unwrap(), 0, "peer sees EOF after shutdown");
    }
}
