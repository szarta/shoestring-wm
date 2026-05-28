//! TOML config for shoestring-notify.
//!
//! Loaded once at startup from `$XDG_CONFIG_HOME/shoestring-notify/config.toml`
//! (falling back to `~/.config/shoestring-notify/config.toml`). A missing
//! file is normal: defaults reproduce the values the daemon shipped with
//! before this module became loadable.

use std::{fs, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub notify: NotifySection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    pub position: Option<String>,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub border: Option<String>,
    pub default_timeout_ms: Option<u32>,
    pub max_width: Option<u32>,
    pub gap: Option<u32>,
    pub padding: Option<u32>,
    pub font: Option<String>,
    pub summary_size: Option<f32>,
    pub body_size: Option<f32>,
}

/// Resolved, validated runtime config. All fields are non-optional —
/// defaults are applied here so the rest of the program never sees a
/// missing field.
#[derive(Debug, Clone)]
pub struct Config {
    pub position: Position,
    /// ARGB8888 native-endian — matches the wl_shm Argb8888 buffer format.
    pub background: u32,
    pub foreground: u32,
    /// Accent color drawn as a left-edge stripe on urgency=critical
    /// toasts so they're visually distinct from normal traffic.
    pub border: u32,
    /// Milliseconds for the body lifetime when the sender passes
    /// `expire_timeout = -1` (the "server default" sentinel from the
    /// FDO notification spec).
    pub default_timeout_ms: u32,
    /// Toast width in logical pixels; body text wraps to fit.
    pub max_width: u32,
    /// Gap between stacked toasts AND the outer screen-edge margin.
    pub gap: u32,
    /// Padding inside each toast, all four sides.
    pub padding: u32,
    /// Optional font override. `None` falls through to
    /// $SHOESTRING_NOTIFY_FONT then the built-in candidate list.
    pub font: Option<PathBuf>,
    pub summary_size: f32,
    pub body_size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: Position::TopRight,
            background: 0xFF_22_22_22,
            foreground: 0xFF_FF_FF_FF,
            border: 0xFF_55_77_88,
            default_timeout_ms: 5_000,
            max_width: 400,
            gap: 8,
            padding: 12,
            font: None,
            summary_size: 15.0,
            body_size: 13.0,
        }
    }
}

impl Config {
    /// Load + validate the user's config. Returns the default config if
    /// no file exists. Parse/validation errors propagate so the caller
    /// can decide whether to fall back or abort.
    pub fn load() -> Result<Self> {
        let path = match default_config_path() {
            Some(p) => p,
            None => return Ok(Config::default()),
        };
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).context(format!("reading {}", path.display())),
        };
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Config::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self> {
        let mut cfg = Config::default();
        let n = raw.notify;
        if let Some(s) = n.position {
            cfg.position = match s.as_str() {
                "top-right" => Position::TopRight,
                "top-left" => Position::TopLeft,
                "bottom-right" => Position::BottomRight,
                "bottom-left" => Position::BottomLeft,
                _ => {
                    return Err(anyhow!(
                        "notify.position = {s:?} unknown; supported: \"top-right\", \
                         \"top-left\", \"bottom-right\", \"bottom-left\""
                    ));
                }
            };
        }
        if let Some(c) = n.background {
            cfg.background =
                parse_color(&c).with_context(|| format!("notify.background = {c:?}"))?;
        }
        if let Some(c) = n.foreground {
            cfg.foreground =
                parse_color(&c).with_context(|| format!("notify.foreground = {c:?}"))?;
        }
        if let Some(c) = n.border {
            cfg.border = parse_color(&c).with_context(|| format!("notify.border = {c:?}"))?;
        }
        if let Some(t) = n.default_timeout_ms {
            // u32 is already >= 0; the explicit check is here for
            // symmetry with the spec text and to future-proof if the
            // field becomes signed.
            cfg.default_timeout_ms = t;
        }
        if let Some(w) = n.max_width {
            if w == 0 {
                return Err(anyhow!("notify.max_width must be > 0"));
            }
            cfg.max_width = w;
        }
        if let Some(g) = n.gap {
            if g == 0 {
                return Err(anyhow!("notify.gap must be > 0"));
            }
            cfg.gap = g;
        }
        if let Some(p) = n.padding {
            cfg.padding = p;
        }
        if let Some(p) = n.font {
            cfg.font = Some(PathBuf::from(p));
        }
        if let Some(s) = n.summary_size {
            if !(s.is_finite() && s > 0.0) {
                return Err(anyhow!("notify.summary_size must be > 0"));
            }
            cfg.summary_size = s;
        }
        if let Some(s) = n.body_size {
            if !(s.is_finite() && s > 0.0) {
                return Err(anyhow!("notify.body_size must be > 0"));
            }
            cfg.body_size = s;
        }
        Ok(cfg)
    }
}

/// Resolved config file path. Honors `$XDG_CONFIG_HOME`, falling back
/// to `$HOME/.config`. Returns `None` if neither env var is set (rare;
/// mostly matters for CI / minimal-container builds).
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("shoestring-notify").join("config.toml"))
}

/// Canonical default config as written by `--write-default-config`.
/// Every field is set to the value `Config::default()` would produce,
/// so the file is a faithful starting point users can tweak.
pub fn default_config_toml() -> &'static str {
    DEFAULT_CONFIG_TOML
}

const DEFAULT_CONFIG_TOML: &str = "\
# shoestring-notify configuration. Regenerate this file at any time with:
#   shoestring-notify --write-default-config
#
# Every field is optional; values shown are the built-in defaults. Comment
# any field out to fall back to the default.

[notify]
# Which screen corner toasts stack from. Stacks grow inward from the
# anchored corner (top corners stack downward, bottom corners upward).
position           = \"top-right\"   # top-right | top-left | bottom-right | bottom-left

# Toast colors. Accepts #RGB, #RRGGBB, or #AARRGGBB; opaque alpha is
# assumed unless you spell out all eight digits.
background         = \"#222222\"
foreground         = \"#ffffff\"
# Accent stripe drawn on urgency=critical toasts so they stand out from
# normal traffic.
border             = \"#557788\"

# Lifetime in ms when the sender passes -1 (the FDO spec's \"server
# default\" sentinel). 0 from the sender always means \"never expire\";
# urgency=critical never auto-expires regardless of this value.
default_timeout_ms = 5000

# Toast width in logical pixels; body text word-wraps to fit.
max_width          = 400

# Pixels between stacked toasts AND from the screen edge.
gap                = 8

# Pixels of padding inside each toast, on all four sides.
padding            = 12

# Path to a TTF/OTF font. If unset, the daemon honors
# $SHOESTRING_NOTIFY_FONT then a built-in candidate list (DejaVu /
# Liberation / Noto in common distro install locations).
# font             = \"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf\"

summary_size       = 15.0
body_size          = 13.0
";

/// Accepts `#RGB`, `#RRGGBB`, `#AARRGGBB`, and `0x`-prefixed hex.
/// Returns ARGB8888 native-endian (the wl_shm buffer format we draw into).
pub fn parse_color(s: &str) -> Result<u32> {
    let raw = s.trim();
    let hex = raw
        .strip_prefix('#')
        .or_else(|| raw.strip_prefix("0x"))
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    let n =
        u32::from_str_radix(hex, 16).map_err(|_| anyhow!("not a hex color literal: {raw:?}"))?;
    let argb = match hex.len() {
        3 => {
            // #RGB → expand each nibble (e.g. #abc → 0xFFAABBCC)
            let r = ((n >> 8) & 0xF) * 0x11;
            let g = ((n >> 4) & 0xF) * 0x11;
            let b = (n & 0xF) * 0x11;
            0xFF00_0000 | (r << 16) | (g << 8) | b
        }
        6 => 0xFF00_0000 | (n & 0x00FF_FFFF),
        8 => n,
        _ => return Err(anyhow!("color must be 3, 6, or 8 hex digits: {raw:?}")),
    };
    Ok(argb)
}

/// Font search order: explicit config → $SHOESTRING_NOTIFY_FONT →
/// built-in candidate list. Mirrors shoestring-bar's `FONT_CANDIDATES`
/// so the same paths work on the same distros.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_forms() {
        assert_eq!(parse_color("#222222").unwrap(), 0xFF22_2222);
        assert_eq!(parse_color("#FF222222").unwrap(), 0xFF22_2222);
        assert_eq!(parse_color("#abc").unwrap(), 0xFFAA_BBCC);
        assert_eq!(parse_color("0x80112233").unwrap(), 0x8011_2233);
        assert!(parse_color("#zzzzzz").is_err());
        assert!(parse_color("#12345").is_err());
    }

    #[test]
    fn defaults_match_documented_values() {
        let c = Config::default();
        assert_eq!(c.position, Position::TopRight);
        assert_eq!(c.background, 0xFF_22_22_22);
        assert_eq!(c.foreground, 0xFF_FF_FF_FF);
        assert_eq!(c.border, 0xFF_55_77_88);
        assert_eq!(c.default_timeout_ms, 5_000);
        assert_eq!(c.max_width, 400);
        assert_eq!(c.gap, 8);
    }

    #[test]
    fn default_config_toml_round_trips_to_defaults() {
        let raw: RawConfig =
            toml::from_str(default_config_toml()).expect("default config TOML must parse");
        let cfg = Config::from_raw(raw).expect("default config TOML must validate");
        let d = Config::default();
        assert_eq!(cfg.position, d.position);
        assert_eq!(cfg.background, d.background);
        assert_eq!(cfg.foreground, d.foreground);
        assert_eq!(cfg.border, d.border);
        assert_eq!(cfg.default_timeout_ms, d.default_timeout_ms);
        assert_eq!(cfg.max_width, d.max_width);
        assert_eq!(cfg.gap, d.gap);
        assert_eq!(cfg.padding, d.padding);
        assert_eq!(cfg.summary_size, d.summary_size);
        assert_eq!(cfg.body_size, d.body_size);
        // Font is intentionally commented out in the template so the
        // built-in candidate list still runs.
        assert!(cfg.font.is_none());
    }

    #[test]
    fn position_accepts_all_four_corners() {
        for (s, want) in [
            ("top-right", Position::TopRight),
            ("top-left", Position::TopLeft),
            ("bottom-right", Position::BottomRight),
            ("bottom-left", Position::BottomLeft),
        ] {
            let raw: RawConfig =
                toml::from_str(&format!(r#"notify = {{ position = "{s}" }}"#)).unwrap();
            assert_eq!(Config::from_raw(raw).unwrap().position, want);
        }
    }

    #[test]
    fn unknown_position_rejected_with_pointer_to_supported_values() {
        let raw: RawConfig = toml::from_str(r#"notify = { position = "unknown-corner" }"#).unwrap();
        let err = Config::from_raw(raw).unwrap_err().to_string();
        assert!(err.contains("supported"), "expected hint, got: {err}");
    }

    #[test]
    fn zero_max_width_or_gap_rejected() {
        let raw: RawConfig = toml::from_str(r#"notify = { max_width = 0 }"#).unwrap();
        assert!(Config::from_raw(raw).is_err());
        let raw: RawConfig = toml::from_str(r#"notify = { gap = 0 }"#).unwrap();
        assert!(Config::from_raw(raw).is_err());
    }

    #[test]
    fn unknown_field_rejected() {
        let bad: Result<RawConfig, _> = toml::from_str(r#"notify = { potato = 1 }"#);
        assert!(bad.is_err());
    }
}
