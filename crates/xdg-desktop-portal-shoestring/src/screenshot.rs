//! Screenshot portal capture: grab a frame and write it to a PNG, returning a
//! `file://` URI for the xdg-desktop-portal frontend to hand back to the app.
//!
//! Two paths:
//! - **non-interactive** — capture the whole default output in-process via the
//!   same [`Capture`] (wlr-screencopy) client the screencast side uses, then
//!   encode the shm pixels to PNG here. No subprocess, so it works even when
//!   D-Bus-activated with a sparse environment.
//! - **interactive** — shell out to `shoestring-screenshot --region`, which
//!   drives the `shoestring-region` rubber-band picker and writes the PNG
//!   itself. We only supply the destination path and read it back.
//!
//! Both honor the screen-capture gate for free: when it is off the WM hides
//! `zwlr_screencopy_manager_v1`, so [`Capture::new`] (and the subprocess) fail,
//! and the caller turns that into a portal failure response.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use wayland_client::protocol::wl_shm;

use crate::capture::{Capture, FrameInfo};
use crate::chooser;

/// Pick a single screen pixel's color for the Screenshot portal's `PickColor`.
///
/// The user clicks the point via the `shoestring-region` overlay (a click is a
/// zero-size drag); region reports the output and the point in that output's
/// logical coordinates. We capture that output and read the pixel, mapping the
/// logical point to a physical one by the output's integer scale. Returns RGB
/// components in `0.0..=1.0`.
///
/// Caveat: the logical→physical mapping uses the `wl_output` integer scale, so
/// under fractional scaling the sampled pixel can be off by the
/// fractional/integer ratio — acceptable for an eyedropper, which samples
/// roughly-uniform regions.
pub fn pick_color() -> Result<(f64, f64, f64)> {
    let sel = chooser::run_region()
        .context("run region picker")?
        .ok_or_else(|| anyhow!("color pick cancelled"))?;
    let mut cap = Capture::new(Some(&sel.output))
        .context("start capture (is the screen-capture gate on?)")?;
    cap.capture().context("capture frame")?;
    let scale = cap.current_scale().max(1);
    let (info, pixels) = cap
        .frame()
        .ok_or_else(|| anyhow!("capture produced no frame"))?;

    let (w, h, stride) = (info.width as i32, info.height as i32, info.stride as usize);
    let px = (sel.x.max(0) * scale).clamp(0, w - 1) as usize;
    let py_top = (sel.y.max(0) * scale).clamp(0, h - 1) as usize;
    // Honor YInvert: shm rows may be stored bottom-to-top.
    let py = if info.yinvert {
        info.height as usize - 1 - py_top
    } else {
        py_top
    };
    let off = py * stride + px * 4;
    if off + 2 >= pixels.len() {
        anyhow::bail!("pick coordinate out of buffer bounds");
    }
    // XRGB8888 little-endian == bytes [B, G, R, X].
    let r = pixels[off + 2] as f64 / 255.0;
    let g = pixels[off + 1] as f64 / 255.0;
    let b = pixels[off] as f64 / 255.0;
    tracing::info!(output = %sel.output, px, py, r, g, b, "PickColor sampled");
    Ok((r, g, b))
}

/// Capture a screenshot and return its `file://` URI.
pub fn capture_to_uri(interactive: bool) -> Result<String> {
    let path = if interactive {
        interactive_region()
    } else {
        full_screen()
    }?;
    path_to_uri(&path)
}

/// Capture the whole default output in-process and write the PNG.
fn full_screen() -> Result<PathBuf> {
    let mut cap = Capture::new(None).context("start capture (is the screen-capture gate on?)")?;
    cap.capture().context("capture frame")?;
    let (info, pixels) = cap
        .frame()
        .ok_or_else(|| anyhow!("capture produced no frame"))?;
    let png = encode_png(pixels, &info)?;
    let path = dest_path();
    write_png(&path, &png)?;
    Ok(path)
}

/// Let the user drag-select a rectangle and have `shoestring-screenshot`
/// capture + encode it to our chosen path.
fn interactive_region() -> Result<PathBuf> {
    let bin = std::env::var_os("SHOESTRING_SCREENSHOT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("shoestring-screenshot"));
    let path = dest_path();
    let status = std::process::Command::new(&bin)
        .arg("--region")
        .arg("--file")
        .arg(&path)
        .status()
        .with_context(|| format!("spawn interactive screenshot helper {bin:?}"))?;
    if !status.success() {
        // Non-zero == user hit Escape / degenerate drag, or capture failed.
        anyhow::bail!("interactive screenshot cancelled or failed (status {status})");
    }
    Ok(path)
}

/// `$XDG_CACHE_HOME/xdg-desktop-portal-shoestring/Screenshot-<unix>-<nanos>.png`.
///
/// A cache dir, not Pictures: portal screenshots are usually consumed by the
/// requesting app (the frontend copies the file into its sandbox), so we don't
/// want to clutter the user's gallery with every app's capture.
fn dest_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    base.join("xdg-desktop-portal-shoestring").join(format!(
        "Screenshot-{}-{:09}.png",
        now.as_secs(),
        now.subsec_nanos()
    ))
}

fn write_png(path: &Path, png_bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create screenshot cache dir")?;
    }
    std::fs::write(path, png_bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Percent-encode a filesystem path into a `file://` URI (RFC 3986 unreserved
/// set plus `/` left literal). The frontend re-parses this back to a path.
fn path_to_uri(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow!("screenshot path is not valid UTF-8: {}", path.display()))?;
    let mut uri = String::from("file://");
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    Ok(uri)
}

/// Encode a captured shm frame (ARGB/XRGB8888, i.e. little-endian BGRA) to RGBA
/// PNG bytes, honoring the screencopy YInvert flag. Adapted from
/// `shoestring-screenshot::encode_png`.
fn encode_png(src: &[u8], info: &FrameInfo) -> Result<Vec<u8>> {
    if !matches!(
        info.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) {
        anyhow::bail!(
            "unexpected buffer format {:?}; only ARGB/XRGB8888",
            info.format
        );
    }
    let (width, height, stride) = (
        info.width as usize,
        info.height as usize,
        info.stride as usize,
    );
    let row_bytes = width * 4;
    if stride < row_bytes {
        anyhow::bail!("stride {stride} too small for width {width}");
    }

    let mut rgba = vec![0u8; row_bytes * height];
    for y in 0..height {
        let src_y = if info.yinvert { height - 1 - y } else { y };
        let src_off = src_y * stride;
        let dst_off = y * row_bytes;
        for x in 0..width {
            let so = src_off + x * 4;
            let doff = dst_off + x * 4;
            // wl_shm ARGB8888 == native-endian 0xAARRGGBB == bytes [B,G,R,A].
            rgba[doff] = src[so + 2]; // R
            rgba[doff + 1] = src[so + 1]; // G
            rgba[doff + 2] = src[so]; // B
            rgba[doff + 3] = if info.format == wl_shm::Format::Xrgb8888 {
                0xFF
            } else {
                src[so + 3]
            };
        }
    }

    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, info.width, info.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("write PNG header")?;
        writer
            .write_image_data(&rgba)
            .context("write PNG image data")?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_percent_encodes_spaces_and_keeps_slashes() {
        let p = PathBuf::from("/home/a b/Screenshot-1.png");
        assert_eq!(
            path_to_uri(&p).unwrap(),
            "file:///home/a%20b/Screenshot-1.png"
        );
    }

    #[test]
    fn dest_path_is_a_png_under_the_cache_subdir() {
        let p = dest_path();
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("png"));
        assert!(
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                == Some("xdg-desktop-portal-shoestring")
        );
    }
}
