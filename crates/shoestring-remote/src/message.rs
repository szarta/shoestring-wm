//! The two message enums and their binary encoding.
//!
//! Each variant begins with a `u8` tag; fields follow in declaration order
//! using the little-endian primitives in the crate root. Adding a variant is
//! append-only (new tag at the end) so old and new peers stay compatible for
//! the variants they share.

use crate::{
    put_bool, put_bytes, put_f64, put_str, put_u16, put_u32, put_u8, Error, Message, Reader, Result,
};

/// Bytes per pixel for every [`PixelFormat`] the protocol carries today.
pub const BYTES_PER_PIXEL: usize = 4;

/// Pixel layout of [`Tile`] data. The headless serve output renders
/// `Argb8888` (the byte order GLES/`wl_shm` hand back), so that is the only
/// format today; the field exists so the client can branch and so a future
/// opaque/10-bit format is an append-only addition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit, 8 bits per channel, byte order B, G, R, A (little-endian
    /// `0xAARRGGBB`). Matches `Fourcc::Argb8888`.
    Argb8888,
}

impl PixelFormat {
    fn tag(self) -> u8 {
        match self {
            PixelFormat::Argb8888 => 0,
        }
    }
    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(PixelFormat::Argb8888),
            t => Err(Error::BadTag {
                kind: "PixelFormat",
                tag: t,
            }),
        }
    }
}

/// How a tile's pixel bytes are encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileEncoding {
    /// Uncompressed pixels, row-major, `width * height * BYTES_PER_PIXEL`
    /// bytes. Used when compression wouldn't pay (tiny tiles, or already-noisy
    /// content) so the server can pick per tile.
    Raw,
    /// zlib (RFC 1950) stream of the raw pixels.
    Zlib,
}

impl TileEncoding {
    fn tag(self) -> u8 {
        match self {
            TileEncoding::Raw => 0,
            TileEncoding::Zlib => 1,
        }
    }
    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(TileEncoding::Raw),
            1 => Ok(TileEncoding::Zlib),
            t => Err(Error::BadTag {
                kind: "TileEncoding",
                tag: t,
            }),
        }
    }
}

/// An axis-aligned rectangle in output **pixel** coordinates (top-left origin).
/// `u16` bounds every field to 65535 px, which covers any single served output
/// (8K is 7680×4320) while keeping tile headers tiny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }
    /// Number of pixels the rectangle covers.
    pub fn pixel_count(&self) -> usize {
        self.w as usize * self.h as usize
    }
    /// Raw (uncompressed) byte length of a tile for this rectangle.
    pub fn raw_len(&self) -> usize {
        self.pixel_count() * BYTES_PER_PIXEL
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_u16(out, self.x);
        put_u16(out, self.y);
        put_u16(out, self.w);
        put_u16(out, self.h);
    }
    fn decode(r: &mut Reader) -> Result<Self> {
        Ok(Rect {
            x: r.u16()?,
            y: r.u16()?,
            w: r.u16()?,
            h: r.u16()?,
        })
    }
}

/// One damaged tile: its rectangle, how `data` is encoded, and the (possibly
/// compressed) pixels. Build one with [`crate::extract_tiles`] and read its
/// pixels back with [`crate::apply_tile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub rect: Rect,
    pub encoding: TileEncoding,
    /// Raw or zlib-compressed pixels per `encoding`.
    pub data: Vec<u8>,
}

impl Tile {
    fn encode(&self, out: &mut Vec<u8>) {
        self.rect.encode(out);
        put_u8(out, self.encoding.tag());
        put_bytes(out, &self.data);
    }
    fn decode(r: &mut Reader) -> Result<Self> {
        let rect = Rect::decode(r)?;
        let encoding = TileEncoding::from_tag(r.u8()?)?;
        let data = r.bytes()?;
        Ok(Tile {
            rect,
            encoding,
            data,
        })
    }
}

/// The structural cursor: where it is and which themed shape to draw. The
/// client renders the named shape from its *own* cursor theme, so there is no
/// cursor sprite in the pixel stream.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorUpdate {
    /// The cursor is hidden (e.g. a fullscreen video player hid it).
    Hidden,
    /// Draw `icon` at output pixel `(x, y)`. `icon` is a `CursorIcon` name as
    /// used by the freedesktop cursor-shape vocabulary (`"default"`, `"text"`,
    /// `"pointer"`, `"grabbing"`, …); an unknown name should fall back to
    /// `"default"` on the client.
    Shape { x: i32, y: i32, icon: String },
}

impl CursorUpdate {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            CursorUpdate::Hidden => put_u8(out, 0),
            CursorUpdate::Shape { x, y, icon } => {
                put_u8(out, 1);
                put_u32(out, *x as u32);
                put_u32(out, *y as u32);
                put_str(out, icon);
            }
        }
    }
    fn decode(r: &mut Reader) -> Result<Self> {
        match r.u8()? {
            0 => Ok(CursorUpdate::Hidden),
            1 => Ok(CursorUpdate::Shape {
                x: r.u32()? as i32,
                y: r.u32()? as i32,
                icon: r.string()?,
            }),
            t => Err(Error::BadTag {
                kind: "CursorUpdate",
                tag: t,
            }),
        }
    }
}

/// Client → server. Negotiation plus raw input (KVM passthrough): the client
/// forwards Linux **keycodes** and pointer events unchanged, so the *remote's*
/// own keymap and keybindings apply natively.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// First message after connect: the client's output pixel size and scale.
    /// The server sizes its virtual output to match exactly (no rescale blur).
    Hello { width: u32, height: u32, scale: f64 },
    /// A key transition. `keycode` is a Linux/evdev keycode (the value before
    /// XKB's `+8` offset); `pressed` is down vs up.
    Key { keycode: u32, pressed: bool },
    /// Absolute pointer position in the remote output's pixel coordinates.
    PointerMotion { x: f64, y: f64 },
    /// A pointer button transition. `button` is an evdev button code
    /// (`BTN_LEFT` = 0x110, `BTN_RIGHT` = 0x111, …).
    PointerButton { button: u32, pressed: bool },
    /// Scroll deltas (continuous, in surface-local units). Either axis may be
    /// zero.
    PointerAxis { horizontal: f64, vertical: f64 },
    /// Pull request: asks the server for the served machine's current selection
    /// (`primary` = primary vs clipboard). The server replies with one
    /// [`ServerMessage::Clipboard`]. The viewer fires this for `Super+Shift+V`.
    GetClipboard { primary: bool },
    /// Push: set the served machine's selection to `data` under `mime` (empty
    /// `mime` clears / is a no-op). The viewer fires this for `Super+Shift+C`
    /// after reading its own local clipboard.
    SetClipboard {
        primary: bool,
        mime: String,
        data: Vec<u8>,
    },
    /// Orderly disconnect; the server may stop streaming and close.
    Bye,
}

impl Message for ClientMessage {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ClientMessage::Hello {
                width,
                height,
                scale,
            } => {
                put_u8(out, 0);
                put_u32(out, *width);
                put_u32(out, *height);
                put_f64(out, *scale);
            }
            ClientMessage::Key { keycode, pressed } => {
                put_u8(out, 1);
                put_u32(out, *keycode);
                put_bool(out, *pressed);
            }
            ClientMessage::PointerMotion { x, y } => {
                put_u8(out, 2);
                put_f64(out, *x);
                put_f64(out, *y);
            }
            ClientMessage::PointerButton { button, pressed } => {
                put_u8(out, 3);
                put_u32(out, *button);
                put_bool(out, *pressed);
            }
            ClientMessage::PointerAxis {
                horizontal,
                vertical,
            } => {
                put_u8(out, 4);
                put_f64(out, *horizontal);
                put_f64(out, *vertical);
            }
            ClientMessage::GetClipboard { primary } => {
                put_u8(out, 6);
                put_bool(out, *primary);
            }
            ClientMessage::SetClipboard {
                primary,
                mime,
                data,
            } => {
                put_u8(out, 7);
                put_bool(out, *primary);
                put_str(out, mime);
                put_bytes(out, data);
            }
            ClientMessage::Bye => put_u8(out, 5),
        }
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            0 => ClientMessage::Hello {
                width: r.u32()?,
                height: r.u32()?,
                scale: r.f64()?,
            },
            1 => ClientMessage::Key {
                keycode: r.u32()?,
                pressed: r.bool()?,
            },
            2 => ClientMessage::PointerMotion {
                x: r.f64()?,
                y: r.f64()?,
            },
            3 => ClientMessage::PointerButton {
                button: r.u32()?,
                pressed: r.bool()?,
            },
            4 => ClientMessage::PointerAxis {
                horizontal: r.f64()?,
                vertical: r.f64()?,
            },
            5 => ClientMessage::Bye,
            6 => ClientMessage::GetClipboard { primary: r.bool()? },
            7 => ClientMessage::SetClipboard {
                primary: r.bool()?,
                mime: r.string()?,
                data: r.bytes()?,
            },
            t => {
                return Err(Error::BadTag {
                    kind: "ClientMessage",
                    tag: t,
                })
            }
        };
        Ok(msg)
    }
}

/// Server → client. The served output's parameters, damage-tile frames, the
/// structural cursor, and lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Sent once after [`ClientMessage::Hello`]: the size, scale, and pixel
    /// format the server actually serves (normally echoing the client's Hello).
    Ready {
        width: u32,
        height: u32,
        scale: f64,
        format: PixelFormat,
    },
    /// A frame: the tiles that changed since the previous frame. `seq`
    /// increments per frame (for logging / loss detection). An empty `tiles`
    /// is a valid heartbeat — but an idle desktop ideally sends no frame at
    /// all.
    Frame { seq: u32, tiles: Vec<Tile> },
    /// The cursor moved or changed shape.
    Cursor(CursorUpdate),
    /// The served output was resized (config change on the server); the client
    /// should reallocate its presentation surface. A fresh full-damage frame
    /// follows.
    Resize { width: u32, height: u32, scale: f64 },
    /// Reply to [`ClientMessage::GetClipboard`]: the served machine's current
    /// selection. Empty `mime` (and `data`) means the selection was empty / had
    /// no readable text. `primary` echoes which selection this is.
    Clipboard {
        primary: bool,
        mime: String,
        data: Vec<u8>,
    },
    /// The server is shutting the session down.
    Bye,
}

impl Message for ServerMessage {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ServerMessage::Ready {
                width,
                height,
                scale,
                format,
            } => {
                put_u8(out, 0);
                put_u32(out, *width);
                put_u32(out, *height);
                put_f64(out, *scale);
                put_u8(out, format.tag());
            }
            ServerMessage::Frame { seq, tiles } => {
                put_u8(out, 1);
                put_u32(out, *seq);
                put_u32(out, tiles.len() as u32);
                for tile in tiles {
                    tile.encode(out);
                }
            }
            ServerMessage::Cursor(update) => {
                put_u8(out, 2);
                update.encode(out);
            }
            ServerMessage::Resize {
                width,
                height,
                scale,
            } => {
                put_u8(out, 3);
                put_u32(out, *width);
                put_u32(out, *height);
                put_f64(out, *scale);
            }
            ServerMessage::Clipboard {
                primary,
                mime,
                data,
            } => {
                put_u8(out, 5);
                put_bool(out, *primary);
                put_str(out, mime);
                put_bytes(out, data);
            }
            ServerMessage::Bye => put_u8(out, 4),
        }
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            0 => ServerMessage::Ready {
                width: r.u32()?,
                height: r.u32()?,
                scale: r.f64()?,
                format: PixelFormat::from_tag(r.u8()?)?,
            },
            1 => {
                let seq = r.u32()?;
                let count = r.u32()? as usize;
                let mut tiles = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    tiles.push(Tile::decode(&mut r)?);
                }
                ServerMessage::Frame { seq, tiles }
            }
            2 => ServerMessage::Cursor(CursorUpdate::decode(&mut r)?),
            3 => ServerMessage::Resize {
                width: r.u32()?,
                height: r.u32()?,
                scale: r.f64()?,
            },
            4 => ServerMessage::Bye,
            5 => ServerMessage::Clipboard {
                primary: r.bool()?,
                mime: r.string()?,
                data: r.bytes()?,
            },
            t => {
                return Err(Error::BadTag {
                    kind: "ServerMessage",
                    tag: t,
                })
            }
        };
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode then decode, asserting the value survives the round trip.
    fn roundtrip<M: Message + PartialEq + std::fmt::Debug>(msg: M) {
        let mut out = Vec::new();
        msg.encode(&mut out);
        let back = M::decode(&out).expect("decode");
        assert_eq!(msg, back);
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip(ClientMessage::Hello {
            width: 1920,
            height: 1080,
            scale: 2.0,
        });
        roundtrip(ClientMessage::Key {
            keycode: 16,
            pressed: false,
        });
        roundtrip(ClientMessage::PointerMotion { x: 12.5, y: -3.0 });
        roundtrip(ClientMessage::PointerButton {
            button: 0x110,
            pressed: true,
        });
        roundtrip(ClientMessage::PointerAxis {
            horizontal: 0.0,
            vertical: 15.0,
        });
        roundtrip(ClientMessage::GetClipboard { primary: true });
        roundtrip(ClientMessage::SetClipboard {
            primary: false,
            mime: "text/plain;charset=utf-8".to_string(),
            data: b"hello over the wire".to_vec(),
        });
        roundtrip(ClientMessage::Bye);
    }

    #[test]
    fn server_messages_roundtrip() {
        roundtrip(ServerMessage::Ready {
            width: 1600,
            height: 900,
            scale: 1.0,
            format: PixelFormat::Argb8888,
        });
        roundtrip(ServerMessage::Frame {
            seq: 42,
            tiles: vec![
                Tile {
                    rect: Rect::new(0, 0, 16, 16),
                    encoding: TileEncoding::Raw,
                    data: vec![0xAB; 16 * 16 * BYTES_PER_PIXEL],
                },
                Tile {
                    rect: Rect::new(100, 50, 8, 4),
                    encoding: TileEncoding::Zlib,
                    data: vec![1, 2, 3, 4],
                },
            ],
        });
        roundtrip(ServerMessage::Frame {
            seq: 0,
            tiles: vec![],
        });
        roundtrip(ServerMessage::Cursor(CursorUpdate::Hidden));
        roundtrip(ServerMessage::Cursor(CursorUpdate::Shape {
            x: -5,
            y: 720,
            icon: "text".to_string(),
        }));
        roundtrip(ServerMessage::Resize {
            width: 3840,
            height: 2160,
            scale: 1.5,
        });
        roundtrip(ServerMessage::Clipboard {
            primary: false,
            mime: "text/plain".to_string(),
            data: b"pulled from the remote".to_vec(),
        });
        roundtrip(ServerMessage::Clipboard {
            primary: true,
            mime: String::new(),
            data: vec![],
        });
        roundtrip(ServerMessage::Bye);
    }

    #[test]
    fn unknown_tag_is_rejected() {
        let err = ClientMessage::decode(&[99]).unwrap_err();
        assert!(matches!(
            err,
            Error::BadTag {
                kind: "ClientMessage",
                tag: 99
            }
        ));
    }

    #[test]
    fn truncated_message_is_rejected() {
        // Hello tag with only a partial width field.
        let err = ClientMessage::decode(&[0, 1, 2]).unwrap_err();
        assert!(matches!(err, Error::UnexpectedEof));
    }

    #[test]
    fn negative_cursor_coords_survive() {
        // i32 round-trips through the u32 slot, including negatives.
        roundtrip(ServerMessage::Cursor(CursorUpdate::Shape {
            x: -1234,
            y: -1,
            icon: "grabbing".to_string(),
        }));
    }
}
