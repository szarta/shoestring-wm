//! Wire protocol + tile codec for shoestring-wm **remote desktop** (serve mode).
//!
//! A standalone, low-dependency crate — like [`shoestring-ipc`] but binary
//! rather than JSON, because the bulk of the traffic is pixel data. It is
//! consumed by both ends of an ssh-tunneled connection:
//!
//! - `shoestring-remote-server` (on the served, headless box) streams the
//!   virtual output to the client and replays the client's input.
//! - `shoestring-remote-client` (on the machine you sit at) presents the
//!   frames and forwards local input.
//!
//! ## Framing
//!
//! The transport is a single duplex byte stream (the ssh tunnel). Every
//! message is length-prefixed:
//!
//! ```text
//! [ u32 little-endian payload length ][ payload bytes ]
//! ```
//!
//! The payload's first byte is the variant tag; the rest is the variant's
//! fields. A reader pulls exactly one message's bytes off the stream, then
//! decodes. The two directions carry different message types — the client
//! writes [`ClientMessage`] and reads [`ServerMessage`]; the server does the
//! reverse — but the framing is identical, so [`read_framed`]/[`write_framed`]
//! work for both.
//!
//! ## Design intent
//!
//! Optimized for *mostly-static* desktop content (terminals, code, browser
//! UIs on a LAN), not full-screen video — so the server pushes only the
//! **damaged tiles** each frame, zlib-compressed, and an idle desktop produces
//! zero tiles. The cursor is **structural**: the server sends its position and
//! `CursorIcon` name and the client renders it with the *local* theme (crisp,
//! no cursor-in-stream lag). The client tells the server its exact output pixel
//! size + scale, and the server serves a virtual output at exactly that size —
//! no rescale blur.
//!
//! [`shoestring-ipc`]: ../shoestring_ipc/index.html

mod codec;
mod message;

pub use codec::{apply_tile, encode_tile, extract_tiles};
pub use message::{
    ClientMessage, CursorUpdate, PixelFormat, Rect, ServerMessage, Tile, TileEncoding,
    BYTES_PER_PIXEL,
};

use std::io::{Read, Write};

/// Hard cap on a single framed message, refused on read so a corrupt or
/// hostile length prefix can't make us allocate gigabytes. 256 MiB comfortably
/// covers a full-screen 8K raw frame (≈132 MiB) with headroom.
pub const MAX_MESSAGE_LEN: usize = 256 * 1024 * 1024;

/// Errors from encoding, decoding, or framing a message.
#[derive(Debug)]
pub enum Error {
    /// The buffer ended mid-field while decoding.
    UnexpectedEof,
    /// An unknown variant tag for the named message type.
    BadTag { kind: &'static str, tag: u8 },
    /// A `bool` byte that was neither 0 nor 1.
    BadBool(u8),
    /// A string field held invalid UTF-8.
    Utf8,
    /// A framed message advertised a length over [`MAX_MESSAGE_LEN`].
    TooLarge(usize),
    /// zlib (de)compression failed, or a decompressed tile didn't match the
    /// size its rectangle implies.
    Codec(String),
    /// An I/O error from [`read_framed`]/[`write_framed`].
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of buffer while decoding"),
            Error::BadTag { kind, tag } => write!(f, "invalid {kind} tag: {tag}"),
            Error::BadBool(b) => write!(f, "invalid bool byte: {b}"),
            Error::Utf8 => write!(f, "invalid UTF-8 in string field"),
            Error::TooLarge(n) => write!(f, "framed message too large: {n} bytes"),
            Error::Codec(m) => write!(f, "tile codec error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A trait for the two message enums so framing helpers are generic over both.
pub trait Message: Sized {
    /// Append the binary encoding (variant tag + fields) to `out`.
    fn encode(&self, out: &mut Vec<u8>);
    /// Decode one message from `buf`, which must hold exactly one message's
    /// bytes (the framed payload).
    fn decode(buf: &[u8]) -> Result<Self>;
}

/// Write one message to `w`, length-prefixed. Encodes into a scratch buffer,
/// then writes the `u32` length and the payload. Flushing is the caller's
/// choice (batch several, then flush).
pub fn write_framed<W: Write, M: Message>(w: &mut W, msg: &M) -> Result<()> {
    let mut payload = Vec::new();
    msg.encode(&mut payload);
    let len = payload.len();
    if len > MAX_MESSAGE_LEN {
        return Err(Error::TooLarge(len));
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    Ok(())
}

/// Read one length-prefixed message from `r`. Blocks until a full message is
/// available (or the stream ends / errors). Refuses a length over
/// [`MAX_MESSAGE_LEN`] before allocating.
pub fn read_framed<R: Read, M: Message>(r: &mut R) -> Result<M> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_MESSAGE_LEN {
        return Err(Error::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    M::decode(&payload)
}

// ---------------------------------------------------------------------------
// Primitive encode/decode helpers — a tiny hand-rolled binary format (no
// bincode/serde dependency). Little-endian throughout; lengths are u32.
// ---------------------------------------------------------------------------

pub(crate) fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}
pub(crate) fn put_bool(out: &mut Vec<u8>, v: bool) {
    out.push(v as u8);
}
pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}
pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

/// A cursor over a byte slice with checked little-endian reads. Every read
/// advances the position and returns [`Error::UnexpectedEof`] past the end, so
/// a truncated or malformed message can never panic or over-read.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let slice = self.buf.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(Error::BadBool(b)),
        }
    }
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub(crate) fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
    pub(crate) fn string(&mut self) -> Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b).map_err(|_| Error::Utf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_roundtrips() {
        let mut out = Vec::new();
        put_u8(&mut out, 7);
        put_bool(&mut out, true);
        put_u16(&mut out, 0xBEEF);
        put_u32(&mut out, 0xDEAD_BEEF);
        put_f64(&mut out, 1.5);
        put_str(&mut out, "héllo");
        put_bytes(&mut out, &[1, 2, 3]);

        let mut r = Reader::new(&out);
        assert_eq!(r.u8().unwrap(), 7);
        assert!(r.bool().unwrap());
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.f64().unwrap(), 1.5);
        assert_eq!(r.string().unwrap(), "héllo");
        assert_eq!(r.bytes().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn reader_rejects_truncation() {
        let mut r = Reader::new(&[0u8; 2]);
        assert!(matches!(r.u32(), Err(Error::UnexpectedEof)));
    }

    #[test]
    fn reader_rejects_bad_bool() {
        let mut r = Reader::new(&[2u8]);
        assert!(matches!(r.bool(), Err(Error::BadBool(2))));
    }

    #[test]
    fn framed_roundtrip_over_stream() {
        // A round-trip through write_framed/read_framed using an in-memory
        // pipe, with two messages back to back to prove framing boundaries.
        let a = ClientMessage::Hello {
            width: 2560,
            height: 1440,
            scale: 1.5,
        };
        let b = ClientMessage::Key {
            keycode: 30,
            pressed: true,
        };
        let mut stream = Vec::new();
        write_framed(&mut stream, &a).unwrap();
        write_framed(&mut stream, &b).unwrap();

        let mut cursor = std::io::Cursor::new(stream);
        let da: ClientMessage = read_framed(&mut cursor).unwrap();
        let db: ClientMessage = read_framed(&mut cursor).unwrap();
        assert_eq!(da, a);
        assert_eq!(db, b);
    }

    #[test]
    fn read_framed_refuses_oversize_length() {
        // A length prefix over the cap is refused before allocating.
        let mut bytes = ((MAX_MESSAGE_LEN + 1) as u32).to_le_bytes().to_vec();
        bytes.push(0); // tag byte, never reached
        let mut cursor = std::io::Cursor::new(bytes);
        let r: Result<ClientMessage> = read_framed(&mut cursor);
        assert!(matches!(r, Err(Error::TooLarge(_))));
    }
}
