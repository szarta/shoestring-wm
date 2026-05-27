//! Hardcoded daemon config.
//!
//! TOML loading is deferred to a future task — for now everything is
//! `Default`. Layout-affecting knobs the task spec calls out explicitly
//! (max_width, gap) live here so the render and stack code can read them
//! through a single struct rather than scattered constants.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// Width of each notification surface in logical pixels. Body text
    /// wraps to fit (minus internal padding).
    pub max_width: u32,
    /// Gap between stacked notifications AND the outer screen-edge margin.
    /// Single knob to keep the visual rhythm consistent.
    pub gap: u32,
    /// Padding inside each surface, applied on all four sides.
    pub padding: u32,
    /// ARGB8888 native-endian — matches the wl_shm Argb8888 buffer format.
    pub background: u32,
    pub foreground: u32,
    /// Optional override of the font path. `None` falls through to
    /// $SHOESTRING_NOTIFY_FONT and then the built-in candidate list.
    pub font: Option<PathBuf>,
    pub summary_size: f32,
    pub body_size: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_width: 360,
            gap: 8,
            padding: 12,
            background: 0xFF_22_22_22,
            foreground: 0xFF_FF_FF_FF,
            font: None,
            summary_size: 15.0,
            body_size: 13.0,
        }
    }
}

/// Font search order: explicit config → $SHOESTRING_NOTIFY_FONT →
/// built-in candidate list. Mirrors shoestring-bar's `FONT_CANDIDATES` so
/// the same paths work on the same distros.
pub const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
    "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/local/share/fonts/liberation-fonts-ttf/LiberationSans-Regular.ttf",
    "/usr/local/share/fonts/noto/NotoSans-Regular.ttf",
];
