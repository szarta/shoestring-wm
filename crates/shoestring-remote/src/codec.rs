//! Tile codec: crop damaged rectangles out of a framebuffer into compressed
//! [`Tile`]s (server side) and blit them back into a framebuffer (client side).
//!
//! Framebuffers here are tightly packed: `width * height * BYTES_PER_PIXEL`
//! bytes, row-major, stride = `width * BYTES_PER_PIXEL`, top-left origin. That
//! is the layout `wlr-screencopy` shm readback and the headless offscreen
//! texture both produce, so the server feeds one straight in.

use std::io::{Read, Write};

use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};

use crate::message::{Rect, Tile, TileEncoding, BYTES_PER_PIXEL};
use crate::{Error, Result};

/// zlib level. Remote desktop wants low latency on mostly-static content, so
/// the fast level (1) is the right trade — most of the win is from sending
/// *only damaged tiles*, not from squeezing each one maximally.
const ZLIB_LEVEL: Compression = Compression::new(1);

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::with_capacity(data.len() / 2), ZLIB_LEVEL);
    // Writing into a Vec is infallible.
    enc.write_all(data).expect("zlib write to Vec");
    enc.finish().expect("zlib finish to Vec")
}

fn zlib_decompress(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(expected);
    dec.read_to_end(&mut out)
        .map_err(|e| Error::Codec(format!("inflate: {e}")))?;
    Ok(out)
}

/// Copy the `rect` region out of the tightly-packed `fb` into a contiguous,
/// row-major pixel buffer. Returns `None` if the rectangle falls outside the
/// framebuffer or `fb` is too small for its declared size.
fn crop(fb: &[u8], fb_width: u32, fb_height: u32, rect: &Rect) -> Option<Vec<u8>> {
    let stride = fb_width as usize * BYTES_PER_PIXEL;
    if fb.len() < stride * fb_height as usize {
        return None;
    }
    let (x, y, w, h) = (
        rect.x as usize,
        rect.y as usize,
        rect.w as usize,
        rect.h as usize,
    );
    if w == 0 || h == 0 || x + w > fb_width as usize || y + h > fb_height as usize {
        return None;
    }
    let row_bytes = w * BYTES_PER_PIXEL;
    let mut out = Vec::with_capacity(row_bytes * h);
    for row in 0..h {
        let start = (y + row) * stride + x * BYTES_PER_PIXEL;
        out.extend_from_slice(&fb[start..start + row_bytes]);
    }
    Some(out)
}

/// Crop and encode each damaged `rect` out of `fb` into a [`Tile`].
///
/// `fb` is a tightly-packed framebuffer of `fb_width × fb_height` pixels.
/// Rectangles outside the framebuffer are skipped (so a stale damage rect from
/// a just-resized output can't panic). When `prefer` is [`TileEncoding::Zlib`]
/// each tile is compressed, but the raw form is kept instead whenever it is
/// smaller — so a tile is never inflated by compression.
pub fn extract_tiles(
    fb: &[u8],
    fb_width: u32,
    fb_height: u32,
    rects: &[Rect],
    prefer: TileEncoding,
) -> Vec<Tile> {
    let mut tiles = Vec::with_capacity(rects.len());
    for rect in rects {
        let Some(raw) = crop(fb, fb_width, fb_height, rect) else {
            continue;
        };
        let tile = match prefer {
            TileEncoding::Raw => Tile {
                rect: *rect,
                encoding: TileEncoding::Raw,
                data: raw,
            },
            TileEncoding::Zlib => {
                let compressed = zlib_compress(&raw);
                if compressed.len() < raw.len() {
                    Tile {
                        rect: *rect,
                        encoding: TileEncoding::Zlib,
                        data: compressed,
                    }
                } else {
                    Tile {
                        rect: *rect,
                        encoding: TileEncoding::Raw,
                        data: raw,
                    }
                }
            }
        };
        tiles.push(tile);
    }
    tiles
}

/// Decode `tile` (decompressing if needed) and blit its pixels into the
/// tightly-packed `dst` framebuffer at the tile's rectangle.
///
/// Errors (without touching `dst`) if the tile's rectangle falls outside the
/// destination, `dst` is too small, or the decoded pixel count doesn't match
/// the rectangle — so a malformed tile can never corrupt memory.
pub fn apply_tile(dst: &mut [u8], dst_width: u32, dst_height: u32, tile: &Tile) -> Result<()> {
    let stride = dst_width as usize * BYTES_PER_PIXEL;
    if dst.len() < stride * dst_height as usize {
        return Err(Error::Codec(format!(
            "dst too small: {} bytes for {dst_width}x{dst_height}",
            dst.len()
        )));
    }
    let rect = tile.rect;
    let (x, y, w, h) = (
        rect.x as usize,
        rect.y as usize,
        rect.w as usize,
        rect.h as usize,
    );
    if w == 0 || h == 0 || x + w > dst_width as usize || y + h > dst_height as usize {
        return Err(Error::Codec(format!(
            "tile rect {rect:?} outside {dst_width}x{dst_height}"
        )));
    }

    let expected = rect.raw_len();
    let pixels = match tile.encoding {
        TileEncoding::Raw => {
            if tile.data.len() != expected {
                return Err(Error::Codec(format!(
                    "raw tile {} bytes, rect needs {expected}",
                    tile.data.len()
                )));
            }
            std::borrow::Cow::Borrowed(&tile.data)
        }
        TileEncoding::Zlib => {
            let decoded = zlib_decompress(&tile.data, expected)?;
            if decoded.len() != expected {
                return Err(Error::Codec(format!(
                    "zlib tile decoded to {} bytes, rect needs {expected}",
                    decoded.len()
                )));
            }
            std::borrow::Cow::Owned(decoded)
        }
    };

    let row_bytes = w * BYTES_PER_PIXEL;
    for row in 0..h {
        let dst_start = (y + row) * stride + x * BYTES_PER_PIXEL;
        let src_start = row * row_bytes;
        dst[dst_start..dst_start + row_bytes]
            .copy_from_slice(&pixels[src_start..src_start + row_bytes]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `w×h` framebuffer whose pixels encode their coordinates, so a
    /// blit landing at the wrong place is detectable.
    fn gradient_fb(w: u32, h: u32) -> Vec<u8> {
        let mut fb = vec![0u8; w as usize * h as usize * BYTES_PER_PIXEL];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * BYTES_PER_PIXEL;
                fb[i] = x as u8;
                fb[i + 1] = y as u8;
                fb[i + 2] = (x ^ y) as u8;
                fb[i + 3] = 0xFF;
            }
        }
        fb
    }

    #[test]
    fn extract_then_apply_reproduces_region_zlib() {
        let (w, h) = (64, 48);
        let src = gradient_fb(w, h);
        let rects = [Rect::new(10, 5, 20, 16), Rect::new(0, 0, 1, 1)];
        let tiles = extract_tiles(&src, w, h, &rects, TileEncoding::Zlib);
        assert_eq!(tiles.len(), 2);

        // Apply onto a blank destination and confirm only those rects changed,
        // and to the exact source pixels.
        let mut dst = vec![0u8; src.len()];
        for t in &tiles {
            apply_tile(&mut dst, w, h, t).unwrap();
        }
        let stride = w as usize * BYTES_PER_PIXEL;
        for rect in &rects {
            for row in 0..rect.h as usize {
                let off = (rect.y as usize + row) * stride + rect.x as usize * BYTES_PER_PIXEL;
                let len = rect.w as usize * BYTES_PER_PIXEL;
                assert_eq!(dst[off..off + len], src[off..off + len]);
            }
        }
    }

    #[test]
    fn raw_encoding_is_byte_identical() {
        let (w, h) = (16, 16);
        let src = gradient_fb(w, h);
        let rect = Rect::new(2, 3, 8, 8);
        let tiles = extract_tiles(&src, w, h, &[rect], TileEncoding::Raw);
        assert_eq!(tiles[0].encoding, TileEncoding::Raw);
        assert_eq!(tiles[0].data.len(), rect.raw_len());

        let mut dst = vec![0u8; src.len()];
        apply_tile(&mut dst, w, h, &tiles[0]).unwrap();
        let stride = w as usize * BYTES_PER_PIXEL;
        for row in 0..rect.h as usize {
            let off = (rect.y as usize + row) * stride + rect.x as usize * BYTES_PER_PIXEL;
            let len = rect.w as usize * BYTES_PER_PIXEL;
            assert_eq!(dst[off..off + len], src[off..off + len]);
        }
    }

    #[test]
    fn compressible_tile_picks_zlib_and_shrinks() {
        // A solid-color tile compresses far below raw, so Zlib is chosen.
        let (w, h) = (32, 32);
        let fb = vec![0x7Fu8; w as usize * h as usize * BYTES_PER_PIXEL];
        let rect = Rect::new(0, 0, 32, 32);
        let tiles = extract_tiles(&fb, w, h, &[rect], TileEncoding::Zlib);
        assert_eq!(tiles[0].encoding, TileEncoding::Zlib);
        assert!(tiles[0].data.len() < rect.raw_len());
    }

    #[test]
    fn incompressible_tile_falls_back_to_raw() {
        // High-entropy pixels: zlib can't beat raw, so extract keeps Raw and
        // never inflates the tile.
        let (w, h) = (24, 24);
        let mut fb = vec![0u8; w as usize * h as usize * BYTES_PER_PIXEL];
        // A simple LCG fills the buffer with pseudo-random bytes (no rng dep).
        let mut s: u32 = 0x1234_5678;
        for b in fb.iter_mut() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *b = (s >> 24) as u8;
        }
        let rect = Rect::new(0, 0, 24, 24);
        let tiles = extract_tiles(&fb, w, h, &[rect], TileEncoding::Zlib);
        assert_eq!(tiles[0].encoding, TileEncoding::Raw);
        assert_eq!(tiles[0].data.len(), rect.raw_len());
    }

    #[test]
    fn out_of_bounds_rect_is_skipped_on_extract() {
        let (w, h) = (16, 16);
        let fb = gradient_fb(w, h);
        // One valid, one off the right edge, one off the bottom.
        let rects = [
            Rect::new(0, 0, 4, 4),
            Rect::new(14, 0, 4, 4),
            Rect::new(0, 14, 4, 4),
        ];
        let tiles = extract_tiles(&fb, w, h, &rects, TileEncoding::Raw);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].rect, Rect::new(0, 0, 4, 4));
    }

    #[test]
    fn apply_rejects_out_of_bounds_rect() {
        let (w, h) = (16, 16);
        let mut dst = vec![0u8; w as usize * h as usize * BYTES_PER_PIXEL];
        let tile = Tile {
            rect: Rect::new(14, 14, 8, 8),
            encoding: TileEncoding::Raw,
            data: vec![0; Rect::new(14, 14, 8, 8).raw_len()],
        };
        assert!(matches!(
            apply_tile(&mut dst, w, h, &tile),
            Err(Error::Codec(_))
        ));
    }

    #[test]
    fn apply_rejects_wrong_sized_raw_tile() {
        let (w, h) = (16, 16);
        let mut dst = vec![0u8; w as usize * h as usize * BYTES_PER_PIXEL];
        let tile = Tile {
            rect: Rect::new(0, 0, 4, 4),
            encoding: TileEncoding::Raw,
            data: vec![0; 4], // far too few bytes for 4x4
        };
        assert!(matches!(
            apply_tile(&mut dst, w, h, &tile),
            Err(Error::Codec(_))
        ));
    }

    #[test]
    fn apply_rejects_corrupt_zlib() {
        let (w, h) = (16, 16);
        let mut dst = vec![0u8; w as usize * h as usize * BYTES_PER_PIXEL];
        let tile = Tile {
            rect: Rect::new(0, 0, 4, 4),
            encoding: TileEncoding::Zlib,
            data: vec![0xFF, 0xFF, 0xFF, 0xFF], // not a zlib stream
        };
        assert!(matches!(
            apply_tile(&mut dst, w, h, &tile),
            Err(Error::Codec(_))
        ));
    }
}
