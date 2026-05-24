//! xcursor theme loader. Honors `$XCURSOR_THEME` and `$XCURSOR_SIZE`,
//! falling back to `default` at size 24 — same conventions every other
//! X/Wayland app uses.
//!
//! [`Cursor::current_frame`] returns the right [`Image`] for a given
//! elapsed time + output scale. Callers are expected to wrap the
//! returned pixel data in a `MemoryRenderBuffer` (see [`crate::drawing`]).

use std::{io::Read, time::Duration};

use xcursor::{parser::parse_xcursor, parser::Image, CursorTheme};

const DEFAULT_THEME: &str = "default";
const DEFAULT_SIZE: u32 = 24;

pub struct Cursor {
    /// All animation frames at the nearest available size. Empty if the
    /// theme couldn't be loaded; the renderer skips drawing in that case.
    frames: Vec<Image>,
    /// Configured nominal size. Frame selection picks the nearest
    /// available among `frames`.
    size: u32,
}

impl Cursor {
    /// Load the user's configured cursor theme. Never panics — on any
    /// failure (missing theme, parse error) returns an empty cursor and
    /// logs a warning.
    pub fn load() -> Self {
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| DEFAULT_THEME.into());
        let size = std::env::var("XCURSOR_SIZE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_SIZE);

        let theme = CursorTheme::load(&theme_name);
        match load_default_icon(&theme) {
            Ok(frames) => {
                tracing::info!(
                    theme = %theme_name,
                    size,
                    frame_count = frames.len(),
                    "loaded xcursor theme",
                );
                Self { frames, size }
            }
            Err(e) => {
                tracing::warn!(
                    theme = %theme_name,
                    error = %e,
                    "could not load xcursor theme; pointer sprite disabled",
                );
                Self {
                    frames: Vec::new(),
                    size,
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Pick the appropriate frame for the given output `scale` and the
    /// elapsed time since startup. Animated cursors loop on the sum of
    /// their per-frame delays.
    pub fn current_frame(&self, scale: u32, time: Duration) -> Option<&Image> {
        if self.frames.is_empty() {
            return None;
        }
        let target_size = self.size * scale.max(1);
        Some(nearest_frame(
            target_size,
            time.as_millis() as u32,
            &self.frames,
        ))
    }
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

#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error("theme has no `default` cursor")]
    NoDefault,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("xcursor file failed to parse")]
    Parse,
}

/// Names a theme might use for the standard arrow cursor. `default` is the
/// freedesktop convention, but many themes (Adwaita, DMZ-*) only ship the
/// X11-era `left_ptr` name. We try each in order.
const DEFAULT_ICON_NAMES: &[&str] = &["default", "left_ptr", "arrow"];

fn load_default_icon(theme: &CursorTheme) -> Result<Vec<Image>, LoadError> {
    let path = DEFAULT_ICON_NAMES
        .iter()
        .find_map(|name| theme.load_icon(name))
        .ok_or(LoadError::NoDefault)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    parse_xcursor(&bytes).ok_or(LoadError::Parse)
}
