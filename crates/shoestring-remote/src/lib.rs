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
//! The transport is a single duplex byte stream: a Unix socket by default,
//! forwarded over ssh (see [`transport`]). Every message is length-prefixed:
//!
//! ```text
//! [ u32 little-endian payload length ][ payload bytes ]
//! ```
//!
//! The payload's first byte is the variant tag; the rest is the variant's
//! fields. Decoders ignore bytes past the fields they know, which is how the
//! handshake's version information reaches older peers harmlessly (see
//! [`PeerInfo`]). A reader pulls exactly one message's bytes off the stream, then
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
//! no cursor-in-stream lag). The client tells the server its output's pixel
//! size and scale in `Hello`; the server treats them as advisory today and
//! serves the virtual output it already has (sizing it to the client is
//! planned).
//!
//! [`shoestring-ipc`]: ../shoestring_ipc/index.html

mod codec;
mod message;
pub mod transport;
mod version;

pub use codec::{apply_tile, encode_tile, extract_tiles};
pub use message::{
    ClientMessage, CursorUpdate, PixelFormat, Rect, ServerMessage, Tile, TileEncoding,
    BYTES_PER_PIXEL,
};
pub use version::{incompatibility, PeerInfo, CAPABILITIES, LEGACY_PROTOCOL, PROTOCOL_VERSION};

use std::io::{Read, Write};

/// Hard cap on a single framed message, refused on read so a corrupt or
/// hostile length prefix can't make us allocate gigabytes. 256 MiB comfortably
/// covers a full-screen 8K raw frame (≈132 MiB) with headroom.
pub const MAX_MESSAGE_LEN: usize = 256 * 1024 * 1024;

/// Largest pixel payload, uncompressed, a sender puts in one tile. A larger
/// damaged region goes out as horizontal bands (see [`band_rows`]), so no
/// tile comes near [`MAX_MESSAGE_LEN`], even one zlib can't shrink.
pub const MAX_TILE_BYTES: usize = 32 * 1024 * 1024;

/// The most rows of a `width`-pixel-wide region one tile carries under
/// [`MAX_TILE_BYTES`]; at least one.
pub fn band_rows(width: u32) -> u32 {
    let row = width.max(1) as usize * BYTES_PER_PIXEL;
    (MAX_TILE_BYTES / row).clamp(1, u32::MAX as usize) as u32
}

/// What a [`ServerMessage::Frame`] takes besides its tiles: the message tag,
/// `seq` and the tile count.
const FRAME_HEADER_LEN: usize = 1 + 4 + 4;

/// Pack one render's `tiles` into as few [`ServerMessage::Frame`]s as keep
/// each encoded message within [`MAX_MESSAGE_LEN`], numbered on from
/// `last_seq`. Returns the frames and the last `seq` used. Damage that fits,
/// which is nearly all of it, is one frame; at least one is always returned,
/// so a render with no readable tiles still sends its (empty) frame. A viewer
/// applies each frame's tiles as they come, so a split frame needs nothing new
/// from it.
pub fn frame_messages(last_seq: u32, tiles: Vec<Tile>) -> (Vec<ServerMessage>, u32) {
    let mut frames = Vec::new();
    let mut seq = last_seq;
    let mut batch: Vec<Tile> = Vec::new();
    let mut len = FRAME_HEADER_LEN;
    for tile in tiles {
        let tile_len = tile.encoded_len();
        if !batch.is_empty() && len + tile_len > MAX_MESSAGE_LEN {
            seq = seq.wrapping_add(1);
            frames.push(ServerMessage::Frame {
                seq,
                tiles: std::mem::take(&mut batch),
            });
            len = FRAME_HEADER_LEN;
        }
        len += tile_len;
        batch.push(tile);
    }
    seq = seq.wrapping_add(1);
    frames.push(ServerMessage::Frame { seq, tiles: batch });
    (frames, seq)
}

/// Largest width or height a peer may announce for a frame (`Ready` /
/// `Resize`): twice 8K, beyond any real output.
pub const MAX_FRAME_DIMENSION: u32 = 16384;

/// Largest pixel count a viewer will allocate a framebuffer for: 8192×8192,
/// whose 4-byte pixels fill exactly [`MAX_MESSAGE_LEN`]. A whole frame that
/// size can't fit one message with its headers, so senders band and split it
/// (see [`frame_messages`]).
pub const MAX_FRAME_PIXELS: u64 = 8192 * 8192;

/// Largest output scale a peer may announce.
pub const MAX_SCALE: f64 = 16.0;

/// The byte length of a tightly packed `width`×`height` framebuffer, checked
/// against [`MAX_FRAME_DIMENSION`] and [`MAX_FRAME_PIXELS`] before anyone
/// allocates it. Both ends take these numbers from the other side.
pub fn framebuffer_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 || width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(Error::BadFrame(format!("frame size {width}x{height}")));
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_FRAME_PIXELS {
        return Err(Error::BadFrame(format!(
            "frame size {width}x{height} is over {MAX_FRAME_PIXELS} pixels"
        )));
    }
    Ok(pixels as usize * BYTES_PER_PIXEL)
}

/// Check an announced output scale: finite, positive, at most [`MAX_SCALE`].
pub fn check_scale(scale: f64) -> Result<f64> {
    if scale.is_finite() && scale > 0.0 && scale <= MAX_SCALE {
        Ok(scale)
    } else {
        Err(Error::BadFrame(format!("scale {scale}")))
    }
}

/// Errors from encoding, decoding, or framing a message.
#[derive(Debug)]
pub enum Error {
    /// The buffer ended mid-field while decoding.
    UnexpectedEof,
    /// An unknown variant tag inside a message (a pixel format, a tile
    /// encoding, ...): the message itself is malformed.
    BadTag { kind: &'static str, tag: u8 },
    /// A whole message with a tag this build doesn't know, likely one a
    /// newer peer added. The framing is intact, so a session can skip it.
    UnknownMessage { kind: &'static str, tag: u8 },
    /// A `bool` byte that was neither 0 nor 1.
    BadBool(u8),
    /// A string field held invalid UTF-8.
    Utf8,
    /// A framed message advertised a length over [`MAX_MESSAGE_LEN`].
    TooLarge(usize),
    /// zlib (de)compression failed, or a decompressed tile didn't match the
    /// size its rectangle implies.
    Codec(String),
    /// A peer announced a frame size or scale outside the protocol's limits
    /// ([`framebuffer_len`], [`check_scale`]).
    BadFrame(String),
    /// An I/O error from [`read_framed`]/[`write_framed`].
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of buffer while decoding"),
            Error::BadTag { kind, tag } => write!(f, "invalid {kind} tag: {tag}"),
            Error::UnknownMessage { kind, tag } => write!(f, "unknown {kind} tag: {tag}"),
            Error::BadBool(b) => write!(f, "invalid bool byte: {b}"),
            Error::Utf8 => write!(f, "invalid UTF-8 in string field"),
            Error::TooLarge(n) => write!(f, "framed message too large: {n} bytes"),
            Error::Codec(m) => write!(f, "tile codec error: {m}"),
            Error::BadFrame(m) => write!(f, "invalid frame: {m}"),
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

    /// Nothing left to read: an older peer's message ends here.
    pub(crate) fn is_at_end(&self) -> bool {
        self.pos >= self.buf.len()
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

    fn raw_tile(y: u16, h: u16, w: u16) -> Tile {
        Tile {
            rect: Rect::new(0, y, w, h),
            encoding: TileEncoding::Raw,
            data: vec![0; w as usize * h as usize * BYTES_PER_PIXEL],
        }
    }

    fn encoded_len(msg: &ServerMessage) -> usize {
        let mut out = Vec::new();
        msg.encode(&mut out);
        out.len()
    }

    #[test]
    fn tile_and_frame_lengths_match_the_encoder() {
        let tiles = vec![raw_tile(0, 3, 5), raw_tile(3, 2, 5)];
        let want = FRAME_HEADER_LEN + tiles.iter().map(Tile::encoded_len).sum::<usize>();
        let (frames, _) = frame_messages(0, tiles);
        assert_eq!(encoded_len(&frames[0]), want);
    }

    #[test]
    fn ordinary_damage_is_one_frame() {
        let (frames, seq) = frame_messages(41, vec![raw_tile(0, 10, 10), raw_tile(10, 10, 10)]);
        assert_eq!(seq, 42);
        assert!(
            matches!(&frames[..], [ServerMessage::Frame { seq: 42, tiles }] if tiles.len() == 2)
        );
        let (frames, seq) = frame_messages(u32::MAX, Vec::new());
        assert_eq!(seq, 0, "seq wraps");
        assert!(matches!(&frames[..], [ServerMessage::Frame { tiles, .. }] if tiles.is_empty()));
    }

    /// Full damage of incompressible pixels on the largest framebuffer a
    /// viewer accepts: banded and packed, every message fits the cap, and the
    /// tiles cover the frame exactly once, in order.
    #[test]
    fn a_full_frame_at_the_pixel_limit_fits_the_message_cap() {
        let (w, h) = (8192u32, 8192u32);
        assert_eq!(framebuffer_len(w, h).unwrap(), MAX_MESSAGE_LEN);
        let rows = band_rows(w);
        assert!(rows as usize * w as usize * BYTES_PER_PIXEL <= MAX_TILE_BYTES);
        let tiles: Vec<Tile> = (0..h)
            .step_by(rows as usize)
            .map(|y| raw_tile(y as u16, rows.min(h - y) as u16, w as u16))
            .collect();
        let (frames, seq) = frame_messages(0, tiles);
        assert_eq!(seq as usize, frames.len());
        assert!(
            frames.len() > 1,
            "a full max-size frame can't be one message"
        );
        let mut next_row = 0u32;
        for (i, frame) in frames.iter().enumerate() {
            assert!(
                encoded_len(frame) <= MAX_MESSAGE_LEN,
                "frame {i} is over the cap"
            );
            let ServerMessage::Frame { seq, tiles } = frame else {
                panic!("not a frame");
            };
            assert_eq!(*seq as usize, i + 1);
            for tile in tiles {
                assert_eq!(u32::from(tile.rect.y), next_row);
                next_row += u32::from(tile.rect.h);
            }
        }
        assert_eq!(next_row, h);
    }

    #[test]
    fn band_rows_is_at_least_one() {
        assert_eq!(band_rows(0), (MAX_TILE_BYTES / BYTES_PER_PIXEL) as u32);
        assert_eq!(band_rows(1920), (MAX_TILE_BYTES / (1920 * 4)) as u32);
        assert!(band_rows(u32::MAX) >= 1);
    }

    #[test]
    fn framebuffer_len_bounds_what_a_peer_can_make_us_allocate() {
        assert_eq!(framebuffer_len(1920, 1080).unwrap(), 1920 * 1080 * 4);
        assert_eq!(framebuffer_len(8192, 8192).unwrap(), 8192 * 8192 * 4);
        for (w, h) in [
            (0, 1080),
            (1920, 0),
            (16385, 1),
            (1, 16385),
            (16384, 16384),
            (u32::MAX, u32::MAX),
        ] {
            assert!(
                matches!(framebuffer_len(w, h), Err(Error::BadFrame(_))),
                "{w}x{h} should be refused"
            );
        }
    }

    #[test]
    fn check_scale_refuses_nonsense() {
        assert_eq!(check_scale(1.5).unwrap(), 1.5);
        for s in [0.0, -1.0, f64::NAN, f64::INFINITY, 16.5] {
            assert!(check_scale(s).is_err(), "{s} should be refused");
        }
    }

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
            peer: PeerInfo::ours(),
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
