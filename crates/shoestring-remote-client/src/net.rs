//! The network connection to the remote `shoestring-remote-server`.
//!
//! One non-blocking TCP stream carrying the duplex `shoestring-remote` wire
//! protocol: we read [`ServerMessage`]s (Ready / Frame tiles / Cursor / Resize /
//! Bye) and write [`ClientMessage`]s (Hello + relayed input + Bye). The crate's
//! [`read_framed`] is blocking-exact, which doesn't fit a `poll(2)` loop, so we
//! buffer instead: drain whatever bytes are available into [`Net::in_buf`] and
//! peel complete `[u32 len][payload]` frames from it ([`Net::next_message`]).
//! Outgoing messages are tiny and infrequent — we stage them in [`Net::out_buf`]
//! and flush what the socket will take, asking the poll loop for `POLLOUT` only
//! while bytes remain.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};

use anyhow::{Context, Result};
use shoestring_remote::{ClientMessage, Message, ServerMessage, MAX_MESSAGE_LEN};

pub struct Net {
    stream: TcpStream,
    /// Bytes read from the socket but not yet parsed into whole frames.
    in_buf: Vec<u8>,
    /// Encoded outgoing frames not yet written (partial writes / `WouldBlock`).
    out_buf: Vec<u8>,
}

impl Net {
    /// Wrap a stream that the caller has already connected, performed the
    /// blocking `Hello`/`Ready` handshake on, and switched to non-blocking. From
    /// here the stream is driven only from the poll loop.
    pub fn from_stream(stream: TcpStream) -> Self {
        Net {
            stream,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// `true` while there are staged outgoing bytes — the poll loop should then
    /// watch for `POLLOUT` and call [`Net::flush_out`].
    pub fn wants_write(&self) -> bool {
        !self.out_buf.is_empty()
    }

    /// Stage one message to send, then try to flush immediately (the common case
    /// on an idle socket writes it straight through).
    pub fn send(&mut self, msg: &ClientMessage) -> Result<()> {
        // write_framed targets a Write; a Vec<u8> never errors, so this only
        // fails on an oversize message (never, for our tiny client messages).
        shoestring_remote::write_framed(&mut self.out_buf, msg)
            .map_err(|e| anyhow::anyhow!("encode client message: {e}"))?;
        self.flush_out()
    }

    /// Write as much of `out_buf` as the socket will take without blocking.
    /// Leaves the remainder staged for the next `POLLOUT`.
    pub fn flush_out(&mut self) -> Result<()> {
        let mut written = 0;
        while written < self.out_buf.len() {
            match self.stream.write(&self.out_buf[written..]) {
                Ok(0) => anyhow::bail!("remote server closed the connection"),
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("write to remote server"),
            }
        }
        self.out_buf.drain(..written);
        Ok(())
    }

    /// Drain all currently-available bytes from the socket into `in_buf`.
    /// Returns `Ok(false)` on a clean EOF (server hung up). Call once when the
    /// socket polls readable, then pull frames with [`Net::next_message`].
    pub fn fill(&mut self) -> Result<bool> {
        let mut tmp = [0u8; 64 * 1024];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(false),
                Ok(n) => self.in_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("read from remote server"),
            }
        }
        Ok(true)
    }

    /// Peel the next complete framed [`ServerMessage`] out of `in_buf`, or
    /// `None` if a whole frame isn't buffered yet. Decode errors (corrupt frame,
    /// oversize length) surface as `Err`.
    pub fn next_message(&mut self) -> Result<Option<ServerMessage>> {
        if self.in_buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.in_buf[..4].try_into().unwrap()) as usize;
        if len > MAX_MESSAGE_LEN {
            anyhow::bail!("remote frame length {len} exceeds cap");
        }
        if self.in_buf.len() < 4 + len {
            return Ok(None); // wait for the rest of the payload
        }
        let payload = self.in_buf[4..4 + len].to_vec();
        self.in_buf.drain(..4 + len);
        let msg =
            ServerMessage::decode(&payload).map_err(|e| anyhow::anyhow!("decode frame: {e}"))?;
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoestring_remote::{CursorUpdate, ServerMessage};

    // Feed a byte stream through a Net's parser without a real socket by poking
    // in_buf directly — exercises the frame-peeling boundary logic.
    fn parse_all(mut chunks: Vec<Vec<u8>>) -> Vec<ServerMessage> {
        // A Net needs a stream; use a connected loopback pair we never read/write.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let mut net = Net {
            stream: client,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
        };
        let mut out = Vec::new();
        for chunk in chunks.drain(..) {
            net.in_buf.extend_from_slice(&chunk);
            while let Some(m) = net.next_message().unwrap() {
                out.push(m);
            }
        }
        out
    }

    #[test]
    fn parses_frames_split_across_chunks() {
        let a = ServerMessage::Ready {
            width: 1920,
            height: 1080,
            scale: 1.0,
            format: shoestring_remote::PixelFormat::Argb8888,
        };
        let b = ServerMessage::Cursor(CursorUpdate::Shape {
            x: 10,
            y: 20,
            icon: "default".into(),
        });
        let mut bytes = Vec::new();
        shoestring_remote::write_framed(&mut bytes, &a).unwrap();
        shoestring_remote::write_framed(&mut bytes, &b).unwrap();

        // Split the byte stream at an awkward boundary (mid-frame) to prove the
        // parser waits for the rest instead of mis-decoding.
        let mid = bytes.len() / 2;
        let got = parse_all(vec![bytes[..mid].to_vec(), bytes[mid..].to_vec()]);
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn yields_nothing_until_a_full_frame_is_buffered() {
        let msg = ServerMessage::Bye;
        let mut bytes = Vec::new();
        shoestring_remote::write_framed(&mut bytes, &msg).unwrap();
        // Everything but the last byte → no message yet; last byte completes it.
        let got = parse_all(vec![
            bytes[..bytes.len() - 1].to_vec(),
            bytes[bytes.len() - 1..].to_vec(),
        ]);
        assert_eq!(got, vec![msg]);
    }
}
