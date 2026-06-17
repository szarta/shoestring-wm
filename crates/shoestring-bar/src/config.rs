//! TOML config for shoestring-bar.
//!
//! Loaded once at startup from `$XDG_CONFIG_HOME/shoestring-bar/config.toml`
//! (falling back to `~/.config/shoestring-bar/config.toml`). A missing file
//! is normal: defaults match the hardcoded behavior the bar shipped with
//! before this module existed.

use std::{fs, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub bar: BarSection,
    #[serde(default)]
    pub clock: ClockSection,
    #[serde(default)]
    pub battery: BatterySection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BarSection {
    pub position: Option<String>,
    pub height: Option<u32>,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub font: Option<String>,
    pub font_size: Option<f32>,
    pub show_workspaces: Option<bool>,
    pub show_control: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockSection {
    pub format: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatterySection {
    pub show: Option<bool>,
    pub format: Option<String>,
    pub low_threshold: Option<u8>,
    pub critical_threshold: Option<u8>,
}

/// Resolved, validated runtime config. All fields are non-optional — defaults
/// are applied here so the rest of the program never has to think about a
/// missing field.
#[derive(Debug, Clone)]
pub struct Config {
    pub position: Position,
    pub height: u32,
    /// ARGB8888, native-endian. Matches the wl_shm buffer format.
    pub background: u32,
    pub foreground: u32,
    /// Explicit font path from config. If `None`, fall back to
    /// $SHOESTRING_BAR_FONT and then the built-in candidate list.
    pub font: Option<PathBuf>,
    pub font_size: f32,
    pub clock_format: String,
    /// When `false`, the workspace box cluster + active-workspace name are
    /// suppressed and the window list slides to the left edge. Pairs with
    /// shoestring-wsindicator for users who prefer a popup-on-switch.
    pub show_workspaces: bool,
    /// When `true` (default), a clickable control applet sits left of the
    /// clock; clicking it opens a menu to toggle the screen-capture and
    /// automation gates, lock, or log out. Set `false` for a pure-keyboard
    /// bar with no mouse-driven control surface.
    pub show_control: bool,
    /// When `false`, the battery indicator is suppressed even if a
    /// battery is detected. Default `true`: auto-detect + show if
    /// present, hide silently otherwise.
    pub battery_show: bool,
    /// Format template with `{pct}` and `{sign}` placeholders. Default
    /// `"{pct}%{sign}"` renders `"85%-"` while discharging.
    pub battery_format: String,
    /// Capacity at or below which the indicator switches to the
    /// "low" color. Only applied when discharging / unknown.
    pub battery_low_threshold: u8,
    /// Capacity at or below which the indicator switches to the
    /// "critical" color (also only when discharging / unknown). Must
    /// be `<=` `battery_low_threshold`.
    pub battery_critical_threshold: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Bottom,
    Top,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            position: Position::Bottom,
            height: 24,
            background: 0xFF_22_22_22,
            foreground: 0xFF_FF_FF_FF,
            font: None,
            font_size: 14.0,
            clock_format: "%a %b %d  %H:%M".to_string(),
            show_workspaces: true,
            show_control: true,
            battery_show: true,
            battery_format: "{pct}%{sign}".to_string(),
            battery_low_threshold: 20,
            battery_critical_threshold: 10,
        }
    }
}

impl Config {
    /// Load + validate the user's config. Returns the default config if no
    /// file exists. Parse/validation errors propagate so the caller can
    /// decide whether to fall back or abort.
    pub fn load() -> Result<Self> {
        let path = match config_path() {
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
        if let Some(s) = raw.bar.position {
            cfg.position = match s.as_str() {
                "bottom" => Position::Bottom,
                "top" => Position::Top,
                _ => {
                    return Err(anyhow!(
                        "bar.position = {s:?} unknown; supported: \"bottom\", \"top\""
                    ));
                }
            };
        }
        if let Some(h) = raw.bar.height {
            if h == 0 {
                return Err(anyhow!("bar.height must be > 0"));
            }
            cfg.height = h;
        }
        if let Some(c) = raw.bar.background {
            cfg.background = parse_color(&c).with_context(|| format!("bar.background = {c:?}"))?;
        }
        if let Some(c) = raw.bar.foreground {
            cfg.foreground = parse_color(&c).with_context(|| format!("bar.foreground = {c:?}"))?;
        }
        if let Some(p) = raw.bar.font {
            cfg.font = Some(PathBuf::from(p));
        }
        if let Some(s) = raw.bar.font_size {
            if !(s.is_finite() && s > 0.0) {
                return Err(anyhow!("bar.font_size must be > 0"));
            }
            cfg.font_size = s;
        }
        if let Some(fmt) = raw.clock.format {
            cfg.clock_format = expand_clock_alias(&fmt);
        }
        if let Some(b) = raw.bar.show_workspaces {
            cfg.show_workspaces = b;
        }
        if let Some(b) = raw.bar.show_control {
            cfg.show_control = b;
        }
        if let Some(b) = raw.battery.show {
            cfg.battery_show = b;
        }
        if let Some(f) = raw.battery.format {
            cfg.battery_format = f;
        }
        if let Some(t) = raw.battery.low_threshold {
            if t > 100 {
                return Err(anyhow!("battery.low_threshold must be 0..=100"));
            }
            cfg.battery_low_threshold = t;
        }
        if let Some(t) = raw.battery.critical_threshold {
            if t > 100 {
                return Err(anyhow!("battery.critical_threshold must be 0..=100"));
            }
            cfg.battery_critical_threshold = t;
        }
        if cfg.battery_critical_threshold > cfg.battery_low_threshold {
            return Err(anyhow!(
                "battery.critical_threshold ({}) must be <= battery.low_threshold ({})",
                cfg.battery_critical_threshold,
                cfg.battery_low_threshold
            ));
        }
        Ok(cfg)
    }
}

/// Resolve shortcut aliases for the clock format. Anything else is passed
/// through verbatim — `libc::strftime` does the actual formatting.
fn expand_clock_alias(s: &str) -> String {
    match s {
        "24h-short" => "%H:%M".to_string(),
        "iso" => "%Y-%m-%d %H:%M:%S".to_string(),
        other => other.to_string(),
    }
}

/// Resolved config file path. Honors `$XDG_CONFIG_HOME`, falling back to
/// `$HOME/.config`. Returns `None` if neither env var is set (rare; mostly
/// matters for CI / minimal-container builds).
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("shoestring-bar").join("config.toml"))
}

fn config_path() -> Option<PathBuf> {
    default_config_path()
}

/// Canonical default config as written by `--write-default-config`. Every
/// field is set to the value `Config::default()` would produce, so the
/// file is a faithful starting point users can tweak rather than a list
/// of un-set knobs.
pub fn default_config_toml() -> &'static str {
    DEFAULT_CONFIG_TOML
}

const DEFAULT_CONFIG_TOML: &str = "\
# shoestring-bar configuration. Regenerate this file at any time with:
#   shoestring-bar --write-default-config
#
# Every field is optional; values shown are the built-in defaults. Comment
# any field out to fall back to the default.

[bar]
# Edge of the output to anchor the bar to. Only horizontal layouts are
# supported.
position    = \"bottom\"     # \"bottom\" | \"top\"

# Bar thickness in logical pixels.
height      = 24

# Background fill and foreground (text) color. Accepts #RGB, #RRGGBB, or
# #AARRGGBB; opaque alpha is assumed unless you spell out all eight digits.
background  = \"#222222\"
foreground  = \"#ffffff\"

# Path to a TTF/OTF font. If unset, the bar honors $SHOESTRING_BAR_FONT,
# then falls back to a built-in candidate list (DejaVu / Liberation /
# Noto, in common distro install locations).
# font      = \"/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf\"

# Font size in pixels. Picked to leave a couple of px of padding inside
# a 24px bar.
font_size   = 14.0

# Whether to render the workspace indicator (box cluster + active name)
# on the far left. Set to false if you prefer a popup-on-switch UX
# (e.g. shoestring-wsindicator) and want the window list to start flush
# against the left edge.
show_workspaces = true

# Clickable control applet (left of the clock): opens a menu to toggle the
# screen-capture and automation gates, lock the screen, or log out. Set to
# false for a pure-keyboard bar with no mouse-driven control surface.
show_control = true

[clock]
# strftime(3) pattern. The two named aliases below are convenience
# shortcuts; anything else is passed straight to libc::strftime.
format      = \"%a %b %d  %H:%M\"
# format    = \"24h-short\"   # alias for \"%H:%M\"
# format    = \"iso\"         # alias for \"%Y-%m-%d %H:%M:%S\"

[battery]
# Show a battery indicator when a battery is detected. Auto-hidden on
# systems with no battery (desktops, headless boxes, BSDs without
# ACPI). Set to false to suppress entirely.
show              = true

# {pct} is the percent, {sign} is '+' (charging), '-' (discharging),
# or empty (full / unknown).
format            = \"{pct}%{sign}\"

# Capacity (%) at or below which the indicator switches to an orange
# warning color. Only applied while discharging.
low_threshold     = 20

# Capacity (%) at or below which the indicator switches to red.
# Must be <= low_threshold.
critical_threshold = 10
";

/// Accepts `#RGB`, `#RRGGBB`, `#AARRGGBB`, and `0x`-prefixed hex. Returns
/// ARGB8888 native-endian (the wl_shm buffer format we draw into).
fn parse_color(s: &str) -> Result<u32> {
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
    fn defaults_match_legacy_constants() {
        let c = Config::default();
        assert_eq!(c.height, 24);
        assert_eq!(c.background, 0xFF_22_22_22);
        assert_eq!(c.foreground, 0xFF_FF_FF_FF);
        assert_eq!(c.font_size, 14.0);
        assert_eq!(c.position, Position::Bottom);
        assert_eq!(c.clock_format, "%a %b %d  %H:%M");
        assert!(c.show_workspaces);
        assert!(c.show_control);
        assert!(c.battery_show);
        assert_eq!(c.battery_format, "{pct}%{sign}");
        assert_eq!(c.battery_low_threshold, 20);
        assert_eq!(c.battery_critical_threshold, 10);
    }

    #[test]
    fn battery_section_overrides() {
        let raw: RawConfig = toml::from_str(
            r#"
            [battery]
            show = false
            format = "BAT {pct}%"
            low_threshold = 30
            critical_threshold = 15
        "#,
        )
        .unwrap();
        let c = Config::from_raw(raw).unwrap();
        assert!(!c.battery_show);
        assert_eq!(c.battery_format, "BAT {pct}%");
        assert_eq!(c.battery_low_threshold, 30);
        assert_eq!(c.battery_critical_threshold, 15);
    }

    #[test]
    fn battery_critical_must_not_exceed_low() {
        let raw: RawConfig = toml::from_str(
            r#"[battery]
            low_threshold = 10
            critical_threshold = 20
        "#,
        )
        .unwrap();
        let err = Config::from_raw(raw).unwrap_err().to_string();
        assert!(err.contains("critical_threshold"));
    }

    #[test]
    fn battery_threshold_out_of_range_rejected() {
        let raw: RawConfig = toml::from_str(
            r#"[battery]
            low_threshold = 200
        "#,
        )
        .unwrap();
        assert!(Config::from_raw(raw).is_err());
    }

    #[test]
    fn show_workspaces_toggle() {
        let raw: RawConfig = toml::from_str(r#"bar = { show_workspaces = false }"#).unwrap();
        assert!(!Config::from_raw(raw).unwrap().show_workspaces);
    }

    #[test]
    fn accepts_top_and_strftime() {
        let raw: RawConfig = toml::from_str(
            r#"
            [bar]
            position = "top"
            [clock]
            format = "%H:%M"
        "#,
        )
        .unwrap();
        let c = Config::from_raw(raw).unwrap();
        assert_eq!(c.position, Position::Top);
        assert_eq!(c.clock_format, "%H:%M");
    }

    #[test]
    fn clock_aliases() {
        let raw: RawConfig = toml::from_str(r#"clock = { format = "iso" }"#).unwrap();
        assert_eq!(
            Config::from_raw(raw).unwrap().clock_format,
            "%Y-%m-%d %H:%M:%S"
        );
        let raw: RawConfig = toml::from_str(r#"clock = { format = "24h-short" }"#).unwrap();
        assert_eq!(Config::from_raw(raw).unwrap().clock_format, "%H:%M");
    }

    #[test]
    fn default_config_toml_parses_and_matches_defaults() {
        let raw: RawConfig =
            toml::from_str(default_config_toml()).expect("default config TOML must parse");
        let cfg = Config::from_raw(raw).expect("default config TOML must validate");
        let d = Config::default();
        assert_eq!(cfg.position, d.position);
        assert_eq!(cfg.height, d.height);
        assert_eq!(cfg.background, d.background);
        assert_eq!(cfg.foreground, d.foreground);
        assert_eq!(cfg.font_size, d.font_size);
        assert_eq!(cfg.clock_format, d.clock_format);
        // Font is intentionally commented out in the template so the
        // built-in candidate list still runs.
        assert!(cfg.font.is_none());
    }

    #[test]
    fn unknown_position_rejected_with_pointer_to_supported_values() {
        for bad in ["left", "right", "diagonal"] {
            let raw: RawConfig =
                toml::from_str(&format!(r#"bar = {{ position = "{bad}" }}"#)).unwrap();
            let err = Config::from_raw(raw).unwrap_err().to_string();
            assert!(err.contains("supported"), "{bad}: {err}");
        }
    }

    #[test]
    fn unknown_field_rejected() {
        let bad: Result<RawConfig, _> = toml::from_str(r#"bar = { potato = 1 }"#);
        assert!(bad.is_err());
    }
}
