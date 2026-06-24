//! Local xcursor theme loader for the *remote* cursor.
//!
//! The remote cursor is structural: the server sends only its position and a
//! freedesktop cursor-shape name ([`shoestring_remote::CursorUpdate::Shape`]),
//! and we render it here with the **local** theme so it's crisp and matches the
//! rest of the desktop. Mirrors the WM's `src/cursor.rs`, but keyed by the name
//! string the wire already carries (no `CursorIcon` mapping needed) and
//! compositing straight into our ARGB8888 framebuffer.

use std::collections::HashMap;
use std::io::Read;

use xcursor::parser::{parse_xcursor, Image};
use xcursor::CursorTheme;

const DEFAULT_THEME: &str = "default";
const DEFAULT_SIZE: u32 = 24;

pub struct Cursor {
    theme: CursorTheme,
    base_size: u32,
    /// Parsed frames per cursor name (`None` = tried, not found). Avoids
    /// re-reading the theme file on every cursor change.
    cache: HashMap<String, Option<Vec<Image>>>,
}

impl Cursor {
    /// Load the user's configured theme, honoring `$XCURSOR_THEME` /
    /// `$XCURSOR_SIZE`. Never fails — a missing theme just means no sprite is
    /// drawn (the remote desktop still shows, cursor-less).
    pub fn load() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_THEME.into());
        let base_size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_SIZE);
        let theme = CursorTheme::load(&theme_name);
        let mut cursor = Cursor {
            theme,
            base_size,
            cache: HashMap::new(),
        };
        match cursor.frames(DEFAULT_THEME_ICON) {
            Some(_) => tracing::info!(theme = %theme_name, base_size, "loaded xcursor theme"),
            None => {
                tracing::warn!(theme = %theme_name, "no usable xcursor theme; remote cursor disabled")
            }
        }
        cursor
    }

    /// Composite the named cursor onto `fb` (ARGB8888, `fb_w`×`fb_h`) so its
    /// hotspot lands at remote pixel `(x, y)`. Picks the frame nearest
    /// `base_size * scale`; unknown names fall back to the default arrow. A
    /// no-op when no theme loaded or the hotspot is off-buffer.
    pub fn draw(
        &mut self,
        fb: &mut [u8],
        fb_w: u32,
        fb_h: u32,
        pos: (i32, i32),
        name: &str,
        scale: f64,
    ) {
        let desired = ((self.base_size as f64) * scale.max(1.0)).round() as u32;
        let Some(img) = self.pick_frame(name, desired) else {
            return;
        };
        let left = pos.0 - img.xhot as i32;
        let top = pos.1 - img.yhot as i32;
        blit_over(fb, fb_w, fb_h, img, left, top);
    }

    /// Choose the frame (by nearest nominal size) for `name`, falling back to
    /// the default arrow when the theme lacks `name`. Cached per name.
    fn pick_frame(&mut self, name: &str, desired: u32) -> Option<&Image> {
        let key = if self.frames(name).is_some() {
            name
        } else {
            DEFAULT_THEME_ICON
        };
        let frames = self.cache.get(key)?.as_ref()?;
        nearest(frames, desired)
    }

    /// Parsed frames for `name` (loading + caching on first ask). Tries the name
    /// itself, then a couple of common legacy aliases for the arrow.
    fn frames(&mut self, name: &str) -> Option<&Vec<Image>> {
        if !self.cache.contains_key(name) {
            let loaded = load_named(&self.theme, name);
            self.cache.insert(name.to_string(), loaded);
        }
        self.cache.get(name).and_then(|o| o.as_ref())
    }
}

/// The canonical default-arrow name we fall back to.
const DEFAULT_THEME_ICON: &str = "default";

/// Pick the frame whose nominal `size` is closest to `desired`.
fn nearest(frames: &[Image], desired: u32) -> Option<&Image> {
    frames
        .iter()
        .min_by_key(|im| (im.size as i64 - desired as i64).abs())
}

/// Load + parse the xcursor file for `name`, trying a few legacy aliases for the
/// default arrow so themes that only ship `left_ptr` still resolve.
fn load_named(theme: &CursorTheme, name: &str) -> Option<Vec<Image>> {
    let mut candidates = vec![name];
    if name == DEFAULT_THEME_ICON {
        candidates.extend(["left_ptr", "arrow"]);
    }
    candidates.into_iter().find_map(|n| {
        let path = theme.load_icon(n)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .ok()?
            .read_to_end(&mut bytes)
            .ok()?;
        parse_xcursor(&bytes)
    })
}

/// Premultiplied "over" composite of an xcursor frame onto an ARGB8888 buffer.
/// `img.pixels_rgba` is premultiplied `[R,G,B,A]`; the fb is `[B,G,R,A]`, so we
/// swap R↔B per channel. Clips to the buffer; off-buffer rows/cols are skipped.
fn blit_over(fb: &mut [u8], fb_w: u32, fb_h: u32, img: &Image, left: i32, top: i32) {
    let (fb_w, fb_h) = (fb_w as i32, fb_h as i32);
    let stride = fb_w as usize * 4;
    for row in 0..img.height as i32 {
        let dy = top + row;
        if dy < 0 || dy >= fb_h {
            continue;
        }
        for col in 0..img.width as i32 {
            let dx = left + col;
            if dx < 0 || dx >= fb_w {
                continue;
            }
            let src = ((row as usize * img.width as usize) + col as usize) * 4;
            let s = &img.pixels_rgba[src..src + 4];
            let (sr, sg, sb, sa) = (s[0] as u32, s[1] as u32, s[2] as u32, s[3] as u32);
            if sa == 0 {
                continue;
            }
            let dst = dy as usize * stride + dx as usize * 4;
            let inv = 255 - sa;
            // fb byte order: [B, G, R, A]
            fb[dst] = (sb + fb[dst] as u32 * inv / 255) as u8;
            fb[dst + 1] = (sg + fb[dst + 1] as u32 * inv / 255) as u8;
            fb[dst + 2] = (sr + fb[dst + 2] as u32 * inv / 255) as u8;
            fb[dst + 3] = (sa + fb[dst + 3] as u32 * inv / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(size: u32) -> Image {
        Image {
            size,
            width: size,
            height: size,
            xhot: 0,
            yhot: 0,
            delay: 0,
            pixels_rgba: vec![0; (size * size * 4) as usize],
            pixels_argb: vec![0; (size * size * 4) as usize],
        }
    }

    #[test]
    fn nearest_picks_closest_nominal_size() {
        let frames = vec![frame(16), frame(24), frame(48)];
        assert_eq!(nearest(&frames, 22).unwrap().size, 24);
        assert_eq!(nearest(&frames, 40).unwrap().size, 48);
        assert_eq!(nearest(&frames, 1).unwrap().size, 16);
    }

    #[test]
    fn blit_is_opaque_over_and_clips() {
        // 2x2 fb (BGRA), opaque-red 1x1 cursor placed at (0,0) then off-buffer.
        let mut fb = vec![0u8; 2 * 2 * 4];
        let mut red = frame(1);
        red.pixels_rgba = vec![255, 0, 0, 255]; // R,G,B,A premultiplied opaque red
        blit_over(&mut fb, 2, 2, &red, 0, 0);
        // fb pixel 0 is [B,G,R,A] → red opaque = [0,0,255,255]
        assert_eq!(&fb[0..4], &[0, 0, 255, 255]);
        // Off-buffer placement is a no-op (no panic, fb[4..] untouched).
        blit_over(&mut fb, 2, 2, &red, 5, 5);
        assert_eq!(&fb[4..8], &[0, 0, 0, 0]);
    }
}
