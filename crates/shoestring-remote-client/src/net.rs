//! The network connection to the remote `shoestring-remote-server`.
//!
//! One non-blocking stream (a forwarded Unix socket, or TCP) carrying the duplex `shoestring-remote` wire
//! protocol: we read [`ServerMessage`]s (Ready / Frame tiles / Cursor / Resize /
//! Bye) and write [`ClientMessage`]s (Hello + relayed input + Bye). The crate's
//! [`read_framed`] is blocking-exact, which doesn't fit a `poll(2)` loop, so we
//! buffer instead: drain whatever bytes are available into [`Net::in_buf`] and
//! peel complete `[u32 len][payload]` frames from it ([`Net::next_message`]).
//! Outgoing messages are tiny and infrequent — we stage them in [`Net::out_buf`]
//! and flush what the socket will take, asking the poll loop for `POLLOUT` only
//! while bytes remain.
//!
//! The server is not trusted to be well behaved. Each frame's length is checked
//! against [`MAX_MESSAGE_LEN`] as soon as its header arrives, before its payload
//! is buffered, and one [`Net::fill`] reads at most [`READ_BUDGET`] bytes, so a
//! peer that keeps the socket readable can neither grow the buffer without
//! bound nor keep the viewer's loop reading. Since the caller parses every
//! complete frame after each fill, `in_buf` holds at most one partly received
//! frame plus one budget.

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};

use anyhow::{Context, Result};
use shoestring_remote::transport::Stream;
use shoestring_remote::{ClientMessage, Message, ServerMessage, MAX_MESSAGE_LEN};

/// Most bytes one [`Net::fill`] reads. The rest waits for the next pass of the
/// poll loop (which stays readable), after the caller has parsed these.
const READ_BUDGET: usize = 4 * 1024 * 1024;

pub struct Net {
    stream: Stream,
    /// Bytes read from the socket but not yet parsed into whole frames.
    in_buf: Vec<u8>,
    /// Offset in `in_buf` of the next frame header not yet length-checked.
    validated_to: usize,
    /// Encoded outgoing frames not yet written (partial writes / `WouldBlock`).
    out_buf: Vec<u8>,
}

impl Net {
    /// Wrap a stream that the caller has already connected, performed the
    /// blocking `Hello`/`Ready` handshake on, and switched to non-blocking. From
    /// here the stream is driven only from the poll loop.
    pub fn from_stream(stream: Stream) -> Self {
        Net {
            stream,
            in_buf: Vec::new(),
            validated_to: 0,
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

    /// Read the bytes available from the socket into `in_buf`, up to
    /// [`READ_BUDGET`], checking each frame header's length as it arrives.
    /// Returns `Ok(false)` on a clean EOF (server hung up) with nothing new read,
    /// and `Err` as soon as a header advertises more than [`MAX_MESSAGE_LEN`].
    /// When EOF follows data in the same call it returns `Ok(true)`, so the
    /// caller parses that data (a final frame, or `Bye`) first; the next call
    /// then sees the EOF at once. Call once when the
    /// socket polls readable, then pull frames with [`Net::next_message`].
    pub fn fill(&mut self) -> Result<bool> {
        let mut tmp = [0u8; 64 * 1024];
        let mut read = 0;
        while read < READ_BUDGET {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(read > 0),
                Ok(n) => {
                    self.in_buf.extend_from_slice(&tmp[..n]);
                    read += n;
                    self.validate_headers()?;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("read from remote server"),
            }
        }
        Ok(true)
    }

    /// Length-check every frame header now fully buffered, so an oversize
    /// frame is refused before its payload is.
    fn validate_headers(&mut self) -> Result<()> {
        while self.validated_to + 4 <= self.in_buf.len() {
            let at = self.validated_to;
            let len = u32::from_le_bytes(self.in_buf[at..at + 4].try_into().unwrap()) as usize;
            if len > MAX_MESSAGE_LEN {
                anyhow::bail!("remote frame length {len} exceeds cap");
            }
            self.validated_to += 4 + len;
        }
        Ok(())
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
        self.validated_to = self.validated_to.saturating_sub(4 + len);
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
        // A Net needs a stream; use a connected pair we never read/write.
        let (client, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut net = Net::from_stream(Stream::Unix(client));
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

#[cfg(test)]
mod socket_tests {
    use super::*;
    use shoestring_remote::Tile;
    use std::os::unix::net::UnixStream;

    /// A `Net` over one end of a socket pair; the test writes the other end.
    fn pair() -> (Net, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        (Net::from_stream(Stream::Unix(ours)), theirs)
    }

    /// A framed `Frame` message carrying about `payload` bytes of tile data.
    fn frame_bytes(seq: u32, payload: usize) -> Vec<u8> {
        let msg = ServerMessage::Frame {
            seq,
            tiles: vec![Tile {
                rect: shoestring_remote::Rect {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                },
                encoding: shoestring_remote::TileEncoding::Raw,
                data: vec![0x5a; payload],
            }],
        };
        let mut bytes = Vec::new();
        shoestring_remote::write_framed(&mut bytes, &msg).unwrap();
        bytes
    }

    /// An oversize length is refused as soon as its header arrives, without
    /// buffering what follows.
    #[test]
    fn oversize_header_is_refused_before_its_payload() {
        let (mut net, mut peer) = pair();
        peer.write_all(&((MAX_MESSAGE_LEN as u32) + 1).to_le_bytes())
            .unwrap();
        // More input after the bad header, as a hostile peer would send.
        // Non-blocking, because nobody reads the far end: the write stops when
        // the socket buffer fills (8 KiB by default on FreeBSD, more on Linux)
        // instead of hanging the test.
        peer.set_nonblocking(true).unwrap();
        let _ = peer.write_all(&vec![0u8; 100 * 1024]);
        let err = net.fill().expect_err("oversize header accepted");
        assert!(err.to_string().contains("exceeds cap"), "{err:#}");
        assert!(
            net.in_buf.len() <= 64 * 1024,
            "buffered {} bytes past a refused header",
            net.in_buf.len()
        );
    }

    /// Data that arrives just before the peer closes is still delivered.
    #[test]
    fn final_frames_before_a_hang_up_are_kept() {
        let (mut net, mut peer) = pair();
        peer.write_all(&frame_bytes(7, 100)).unwrap();
        let mut bye = Vec::new();
        shoestring_remote::write_framed(&mut bye, &ServerMessage::Bye).unwrap();
        peer.write_all(&bye).unwrap();
        drop(peer);
        assert!(net.fill().unwrap(), "data before EOF reported as a hang-up");
        assert!(matches!(
            net.next_message().unwrap(),
            Some(ServerMessage::Frame { seq: 7, .. })
        ));
        assert_eq!(net.next_message().unwrap(), Some(ServerMessage::Bye));
        assert!(!net.fill().unwrap(), "EOF not reported on the next fill");
    }

    /// Frames split across reads, and several arriving at once, all decode.
    #[test]
    fn fragmented_and_coalesced_frames_decode() {
        let (mut net, mut peer) = pair();
        let first = frame_bytes(1, 1000);
        let (head, tail) = first.split_at(first.len() / 2);
        peer.write_all(head).unwrap();
        assert!(net.fill().unwrap());
        assert!(
            net.next_message().unwrap().is_none(),
            "half a frame decoded"
        );
        peer.write_all(tail).unwrap();
        for seq in 2..=5 {
            peer.write_all(&frame_bytes(seq, 1000)).unwrap();
        }
        assert!(net.fill().unwrap());
        let mut seqs = Vec::new();
        while let Some(msg) = net.next_message().unwrap() {
            let ServerMessage::Frame { seq, .. } = msg else {
                panic!("unexpected {msg:?}");
            };
            seqs.push(seq);
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert!(net.in_buf.is_empty());
    }

    /// A peer that never stops sending can't keep one fill reading or grow the
    /// buffer past a budget; every frame still arrives, over several fills.
    #[test]
    fn sustained_traffic_is_read_in_budgeted_steps() {
        let (mut net, peer) = pair();
        const FRAMES: u32 = 256;
        const PAYLOAD: usize = 64 * 1024;
        let writer = std::thread::spawn(move || {
            let mut peer = peer;
            for seq in 0..FRAMES {
                peer.write_all(&frame_bytes(seq, PAYLOAD)).unwrap();
            }
        });
        let mut next = 0;
        let mut fills = 0;
        loop {
            let before = net.in_buf.len();
            let open = net.fill().unwrap();
            fills += 1;
            assert!(
                net.in_buf.len() - before <= READ_BUDGET + 64 * 1024,
                "one fill read {} bytes",
                net.in_buf.len() - before
            );
            while let Some(msg) = net.next_message().unwrap() {
                let ServerMessage::Frame { seq, .. } = msg else {
                    panic!("unexpected {msg:?}");
                };
                assert_eq!(seq, next);
                next += 1;
            }
            if !open {
                break;
            }
            if net.in_buf.len() < 4 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        writer.join().unwrap();
        assert_eq!(next, FRAMES, "frames lost before the hang-up");
        assert!(fills > 1, "16 MiB arrived in one fill");
    }
}
