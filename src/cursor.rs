//! xcursor theme loader. Honors `$XCURSOR_THEME` and `$XCURSOR_SIZE`,
//! falling back to `default` at size 24 — same conventions every other
//! X/Wayland app uses.
//!
//! The loader resolves *named* cursors on demand: clients pick a cursor by
//! name through `wp_cursor_shape_v1` (or the legacy `wl_pointer.set_cursor`
//! surface path), smithay hands us a [`CursorIcon`], and we rasterize the
//! matching xcursor sprite — no client-side pixels involved. Parsed frames
//! are cached per icon so repeated hovers don't re-stat the theme directory.
//!
//! [`Cursor::current_frame`] returns the right [`Image`] for a given icon,
//! output scale, and elapsed time. Callers are expected to wrap the returned
//! pixel data in a `MemoryRenderBuffer` (see [`crate::drawing`]).

use std::{collections::HashMap, io::Read, time::Duration};

use smithay::input::pointer::CursorIcon;
use xcursor::{parser::parse_xcursor, parser::Image, CursorTheme};

const DEFAULT_THEME: &str = "default";
const DEFAULT_SIZE: u32 = 24;

pub struct Cursor {
    /// The loaded theme. Held so named icons can be resolved lazily — a
    /// client may request any of ~35 shapes, and most themes ship only a
    /// handful, so loading them all up front would mostly waste effort.
    theme: CursorTheme,
    /// Configured nominal size. Frame selection picks the nearest
    /// available size among an icon's frames.
    size: u32,
    /// Parsed-frame cache keyed by the requested icon. A `None` value
    /// records a confirmed miss (the theme ships no file for that icon)
    /// so we don't re-stat the theme directory on every render.
    cache: HashMap<CursorIcon, Option<Vec<Image>>>,
}

impl Cursor {
    /// Load the user's configured cursor theme. Never panics — on any
    /// failure (missing theme, parse error) the default icon resolves to
    /// `None` and the renderer skips drawing the sprite.
    pub fn load() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_THEME.into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_SIZE);

        let theme = CursorTheme::load(&theme_name);
        let mut cursor = Self {
            theme,
            size,
            cache: HashMap::new(),
        };

        // Warm the default arrow so a load failure is visible at startup
        // rather than on first hover.
        match cursor.frames(CursorIcon::Default) {
            Some(frames) => tracing::info!(
                theme = %theme_name,
                size,
                frame_count = frames.len(),
                "loaded xcursor theme",
            ),
            None => tracing::warn!(
                theme = %theme_name,
                "could not load xcursor theme; pointer sprite disabled",
            ),
        }
        cursor
    }

    /// Pick the appropriate frame for the given `icon`, output `scale`, and
    /// the elapsed time since startup. Animated cursors loop on the sum of
    /// their per-frame delays. When the theme doesn't ship `icon`, falls
    /// back to the default arrow; returns `None` only when even the default
    /// is unavailable (no usable theme).
    pub fn current_frame(
        &mut self,
        icon: CursorIcon,
        scale: u32,
        time: Duration,
    ) -> Option<&Image> {
        // Resolve to a concrete icon we actually have frames for. Most of
        // the resize/zoom/dnd shapes are absent from typical themes, so a
        // graceful fall back to the arrow is the norm for compositors.
        let resolved = if self.frames(icon).is_some() {
            icon
        } else if icon != CursorIcon::Default && self.frames(CursorIcon::Default).is_some() {
            CursorIcon::Default
        } else {
            return None;
        };

        let target_size = self.size * scale.max(1);
        let frames = self.cache[&resolved].as_ref().expect("resolved is present");
        Some(nearest_frame(target_size, time.as_millis() as u32, frames))
    }

    /// The configured nominal cursor size in logical pixels. The picker's
    /// kill cursor matches it so the `×` is the same visual size as the
    /// theme's arrow.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Rasterize the xkill-style kill cursor — a bold red `×` with a white
    /// outline — at `self.size * scale` physical pixels. Returned as
    /// premultiplied `Argb8888` bytes (little-endian `[B, G, R, A]`, what a
    /// `MemoryRenderBuffer` expects), `size*size*4` long, alongside the square
    /// edge length. Drawn from scratch rather than pulled from the theme so the
    /// "click to kill" cursor is unmistakable on any system. The hotspot is the
    /// center (`self.size/2` logical px).
    pub fn kill_frame(&self, scale: u32) -> (u32, Vec<u8>) {
        let size = (self.size * scale.max(1)).max(8);
        (size, render_kill_x(size))
    }

    /// Resolve `icon` to its parsed frames, loading + caching on first use.
    /// Returns `None` (and caches the miss) when the theme ships no file
    /// for it.
    fn frames(&mut self, icon: CursorIcon) -> Option<&Vec<Image>> {
        self.cache
            .entry(icon)
            .or_insert_with(|| load_named(&self.theme, icon))
            .as_ref()
    }
}

/// Draw a bold `×` (two diagonal strokes) into a `size`×`size` premultiplied
/// `Argb8888` buffer: opaque red core with a white outline for contrast against
/// any window, fully transparent elsewhere. The strokes are inset from the
/// edges so the mark reads as an `×` rather than a full corner-to-corner cross.
fn render_kill_x(size: u32) -> Vec<u8> {
    // Opaque colors are their own premultiplied form. Byte order is
    // `[B, G, R, A]` (little-endian `Argb8888`).
    const RED: [u8; 4] = [40, 40, 220, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    let n = size as f32;
    let inset = n * 0.16; // keep the arms off the very corners
    let core = (n * 0.10).max(1.0); // half-width of the red stroke
    let outline = core + (n * 0.05).max(1.5); // half-width incl. white edge
    let lo = inset;
    let hi = n - 1.0 - inset;

    let mut buf = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32, y as f32);
            // Only within the inset square do the diagonals draw.
            if fx < lo || fx > hi || fy < lo || fy > hi {
                continue;
            }
            // Distance to each diagonal of the inset square (`\` and `/`).
            let d1 = ((fx - lo) - (fy - lo)).abs() / std::f32::consts::SQRT_2;
            let d2 = ((fx - lo) + (fy - lo) - (hi - lo)).abs() / std::f32::consts::SQRT_2;
            let d = d1.min(d2);
            let color = if d <= core {
                RED
            } else if d <= outline {
                WHITE
            } else {
                continue;
            };
            let i = ((y * size + x) * 4) as usize;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
    buf
}

fn nearest_frame(target_size: u32, mut millis: u32, frames: &[Image]) -> &Image {
    // Match the nominal cursor size first…
    let nearest_size = frames
        .iter()
        .min_by_key(|f| (target_size as i32 - f.size as i32).abs())
        .expect("frames is non-empty here")
        .size;
    let same_size: Vec<&Image> = frames.iter().filter(|f| f.size == nearest_size).collect();

    // …then pick the right animation frame at that size based on elapsed time.
    let total: u32 = same_size.iter().map(|f| f.delay).sum();
    if total == 0 {
        return same_size[0];
    }
    millis %= total;
    for f in &same_size {
        if millis < f.delay {
            return f;
        }
        millis -= f.delay;
    }
    same_size[0]
}

/// Load and parse the xcursor file for `icon`, trying the freedesktop
/// (w3c) name first, then the legacy Xcursor `alt_names` many older themes
/// ship instead (e.g. `left_ptr`/`arrow` for [`CursorIcon::Default`],
/// `xterm` for `Text`). Returns `None` if no candidate file exists or none
/// parse.
fn load_named(theme: &CursorTheme, icon: CursorIcon) -> Option<Vec<Image>> {
    std::iter::once(icon.name())
        .chain(icon.alt_names().iter().copied())
        .find_map(|name| {
            let path = theme.load_icon(name)?;
            let mut bytes = Vec::new();
            std::fs::File::open(&path)
                .ok()?
                .read_to_end(&mut bytes)
                .ok()?;
            parse_xcursor(&bytes)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn kill_x_has_red_center_and_clear_corners() {
        let size = 32;
        let buf = render_kill_x(size);
        assert_eq!(buf.len() as u32, size * size * 4);
        // The two diagonals cross at the center → opaque red ([B,G,R,A]).
        let c = px(&buf, size, size / 2, size / 2);
        assert_eq!(c[3], 255, "center is opaque");
        assert!(c[2] > c[0] && c[2] > c[1], "center is red-dominant: {c:?}");
        // The arms are inset, so the very corner stays fully transparent.
        assert_eq!(px(&buf, size, 0, 0), [0, 0, 0, 0], "corner is clear");
        // Off-diagonal midpoint (top-center) is between the strokes → clear.
        assert_eq!(px(&buf, size, size / 2, 2), [0, 0, 0, 0], "gap is clear");
    }

    #[test]
    fn kill_x_white_outline_flanks_the_red() {
        let size = 64;
        let buf = render_kill_x(size);
        // Walk a horizontal line through the center; there must be at least one
        // white outline pixel and one red core pixel along it.
        let mut saw_red = false;
        let mut saw_white = false;
        for x in 0..size {
            let p = px(&buf, size, x, size / 2);
            if p == [40, 40, 220, 255] {
                saw_red = true;
            }
            if p == [255, 255, 255, 255] {
                saw_white = true;
            }
        }
        assert!(saw_red, "found red core on the center row");
        assert!(saw_white, "found white outline on the center row");
    }
}
