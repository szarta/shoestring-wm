//! The length-framed clipboard wire between `watch` (stdout) and `set` (stdin).
//!
//! `shoestring-clipboard watch | ssh host shoestring-clipboard set` ships one
//! frame per selection over an ordinary byte pipe, so the format is dead simple
//! and self-describing:
//!
//! ```text
//! frame := [u32 LE mime_len][mime utf8][u32 LE data_len][data bytes]
//! ```
//!
//! The mime string travels with the bytes so the far `set` advertises the same
//! type back to clients (and so extending past text is just dropping the
//! text-only filter in `watch`). `decode_frame` peels exactly one frame from the
//! front of a running buffer, leaving any trailing partial bytes for next time.

use anyhow::{bail, Result};

/// Cap on a single clipboard payload. Clipboard text is tiny; this is only a
/// guard against a corrupt/oversized length on the wire wedging memory.
pub const MAX_DATA_LEN: usize = 64 * 1024 * 1024; // 64 MiB
/// Mime strings are short; reject anything absurd before allocating.
pub const MAX_MIME_LEN: usize = 4096;

/// Append one framed `(mime, data)` entry to `out`.
pub fn encode_frame(out: &mut Vec<u8>, mime: &str, data: &[u8]) {
    out.extend_from_slice(&(mime.len() as u32).to_le_bytes());
    out.extend_from_slice(mime.as_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// Try to peel one frame from the front of `buf`. On success the frame's bytes
/// are drained from `buf` and `Some((mime, data))` is returned; if a full frame
/// isn't buffered yet, `buf` is left untouched and `None` is returned. A
/// length that exceeds the guards (or a non-utf8 mime) is a hard error.
pub fn decode_frame(buf: &mut Vec<u8>) -> Result<Option<(String, Vec<u8>)>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let mime_len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if mime_len > MAX_MIME_LEN {
        bail!("clipboard frame mime length {mime_len} exceeds {MAX_MIME_LEN}");
    }
    let after_mime = 4 + mime_len;
    if buf.len() < after_mime + 4 {
        return Ok(None);
    }
    let data_len = u32::from_le_bytes(buf[after_mime..after_mime + 4].try_into().unwrap()) as usize;
    if data_len > MAX_DATA_LEN {
        bail!("clipboard frame data length {data_len} exceeds {MAX_DATA_LEN}");
    }
    let end = after_mime + 4 + data_len;
    if buf.len() < end {
        return Ok(None);
    }
    let mime = String::from_utf8(buf[4..after_mime].to_vec())
        .map_err(|_| anyhow::anyhow!("clipboard frame mime is not utf8"))?;
    let data = buf[after_mime + 4..end].to_vec();
    buf.drain(..end);
    Ok(Some((mime, data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_one_frame() {
        let mut buf = Vec::new();
        encode_frame(&mut buf, "text/plain;charset=utf-8", b"hello");
        let (mime, data) = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(mime, "text/plain;charset=utf-8");
        assert_eq!(data, b"hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn yields_nothing_until_a_full_frame_is_buffered() {
        let mut full = Vec::new();
        encode_frame(&mut full, "text/plain", b"abcd");
        // Feed it one byte at a time; only the final byte completes the frame.
        let mut buf = Vec::new();
        for (i, b) in full.iter().enumerate() {
            buf.push(*b);
            let got = decode_frame(&mut buf).unwrap();
            if i + 1 < full.len() {
                assert!(got.is_none(), "completed early at byte {i}");
            } else {
                let (mime, data) = got.unwrap();
                assert_eq!(mime, "text/plain");
                assert_eq!(data, b"abcd");
            }
        }
    }

    #[test]
    fn decodes_back_to_back_frames_and_keeps_the_remainder() {
        let mut buf = Vec::new();
        encode_frame(&mut buf, "text/plain", b"one");
        encode_frame(&mut buf, "UTF8_STRING", b"two");
        // A trailing partial third frame must survive untouched.
        buf.extend_from_slice(&[7, 0, 0]); // 3 bytes of a 4-byte length header

        let (m1, d1) = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            (m1.as_str(), d1.as_slice()),
            ("text/plain", b"one".as_ref())
        );
        let (m2, d2) = decode_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            (m2.as_str(), d2.as_slice()),
            ("UTF8_STRING", b"two".as_ref())
        );
        assert!(decode_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf, vec![7, 0, 0]);
    }

    #[test]
    fn rejects_an_oversized_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes());
        buf.extend_from_slice(b"text/");
        buf.extend_from_slice(&(MAX_DATA_LEN as u32 + 1).to_le_bytes());
        assert!(decode_frame(&mut buf).is_err());
    }
}
