//! Config types and TOML parser for shoestring-wm.
//!
//! Keysyms and modifier names are stored as plain strings here so this crate
//! does not depend on xkbcommon — the WM resolves them into a [`BindingTable`]
//! at startup.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Per-output overrides keyed by connector name (e.g. `"DP-1"`, `"HDMI-A-1"`).
/// All fields are optional; unset fields fall back to the matching `[general]`
/// default.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputConfig {
    /// Per-output scale factor. Overrides `general.output_scale` for this
    /// connector only. Same semantics: whole values use integer scaling,
    /// fractional values use `wp_fractional_scale_v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// Fixed compositor-space position `[x, y]` for this output. When set,
    /// overrides the automatic left-to-right layout. Use this to declare a
    /// stable multi-monitor arrangement independent of plug-in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[i32; 2]>,
    /// Enable variable refresh rate (VRR / adaptive sync) on this output.
    /// `true` opts the connector in; the WM only actually enables VRR if the
    /// monitor and driver advertise support (otherwise it logs a warning and
    /// leaves it off). Defaults to off — VRR can cause visible flicker on some
    /// panels, so it is strictly opt-in per output. Only honored on the
    /// DRM/KMS (TTY) backend; ignored when running nested under winit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_sync: Option<bool>,
    /// Rotate / flip this output. Applied at output creation. Unset leaves the
    /// panel in its native orientation. Only honored on the DRM/KMS (TTY)
    /// backend; ignored under nested winit (which owns its own transform).
    /// A later `wlr-output-management` apply (e.g. `wlr-randr --transform`)
    /// overrides whatever is configured here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<OutputTransform>,
}

/// Output orientation, matching the `wl_output` / `wlr-randr` transform names.
/// `_90`/`_180`/`_270` are clockwise rotations; the `Flipped*` variants mirror
/// horizontally first, then rotate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum OutputTransform {
    #[serde(rename = "normal")]
    Normal,
    #[serde(rename = "90")]
    _90,
    #[serde(rename = "180")]
    _180,
    #[serde(rename = "270")]
    _270,
    #[serde(rename = "flipped")]
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

/// Top-level config. Sections are all optional; missing sections take defaults.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub workspaces: Workspaces,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// Per-app rules evaluated once when a window's `app_id` / `title`
    /// first becomes available after map. The first rule whose matcher
    /// fields all match wins; rules are evaluated in declaration order.
    #[serde(default, rename = "window_rules")]
    pub window_rules: Vec<WindowRule>,
    /// Per-output overrides. Keys are connector names as reported by the WM
    /// (e.g. `"DP-1"`, `"HDMI-A-1"`). Omit entirely to use global defaults.
    #[serde(
        default,
        rename = "outputs",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub outputs: BTreeMap<String, OutputConfig>,
    #[serde(default)]
    pub diagnostics: Diagnostics,
    #[serde(default)]
    pub input: Input,
    #[serde(default)]
    pub touch: Touch,
    #[serde(default)]
    pub portal: Portal,
    #[serde(default)]
    pub decorations: Decorations,
    #[serde(default)]
    pub background: Background,
    #[serde(default)]
    pub debug: Debug,
}

/// `[portal]` — settings for the `xdg-desktop-portal-shoestring` screen-sharing
/// backend. That backend is a *separate process* from the WM; it reads this
/// same `config.toml` so the screencast output choice lives in one place.
///
/// Both fields are optional. With no `[portal]` section the backend shares the
/// output you pick from the [`screencast_chooser`](Self::screencast_chooser)
/// when more than one is connected.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Portal {
    /// Pin screencast to one output by connector name (e.g. `"DP-2"`). When set,
    /// the chooser is skipped and this output is always shared. Unset (default)
    /// ⇒ the chooser runs whenever more than one output is connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screencast_output: Option<String>,
    /// How to choose the output when none is pinned and more than one is
    /// connected:
    /// - `"region"` (default) — pop the `shoestring-region` overlay so you
    ///   click/drag the monitor to share;
    /// - `"none"` — silently share the first output (a warning names it and how
    ///   to pin one);
    /// - anything else — run as a dmenu-style command: the connector names are
    ///   written to its stdin, one per line, and the line it prints on stdout is
    ///   taken as the chosen output.
    #[serde(default = "default_screencast_chooser")]
    pub screencast_chooser: String,
}

fn default_screencast_chooser() -> String {
    "region".to_string()
}

impl Default for Portal {
    fn default() -> Self {
        Self {
            screencast_output: None,
            screencast_chooser: default_screencast_chooser(),
        }
    }
}

/// `[diagnostics]` — the metrics/observability subsystem. When `enabled`
/// (the default), the WM samples its own process resources on a timer,
/// feeds the `metrics` IPC snapshot/stream, and runs the fd-leak detector
/// that warns before an unbounded file-descriptor leak hits
/// `RLIMIT_NOFILE` and crashes the session. Turn it off to drop all
/// background sampling (snapshot queries still answer on demand; the
/// stream subscription requires it on).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostics {
    /// Master switch for background sampling + the leak detector. Default
    /// `true` so the crash protection is on without configuration.
    #[serde(default = "default_diagnostics_enabled")]
    pub enabled: bool,
    /// Background sampling cadence in milliseconds. Also the floor for a
    /// `metrics` stream subscriber's push interval (a subscriber can ask
    /// for slower, not faster, in v1). Default 1000.
    #[serde(default = "default_sample_interval_ms")]
    pub sample_interval_ms: u64,
    /// Warn when `process.open_fds` exceeds this fraction of the
    /// `RLIMIT_NOFILE` soft limit. Default 0.75. Clamped to `(0.0, 1.0]`
    /// at load.
    #[serde(default = "default_fd_warn_fraction")]
    pub fd_warn_fraction: f64,
    /// Font size (logical px) of the F3-style on-screen diagnostics overlay
    /// ([`Action::ToggleDiagnostics`]). Default 15.
    #[serde(default = "default_overlay_font_size")]
    pub overlay_font_size: f32,
    /// Overlay text color, `#RRGGBB` or `#RRGGBBAA`. Default light grey.
    #[serde(default = "default_overlay_fg")]
    pub overlay_fg_color: String,
    /// Overlay panel background color, `#RRGGBB` or `#RRGGBBAA`. Default a
    /// translucent dark slate so the scene shows through faintly.
    #[serde(default = "default_overlay_bg")]
    pub overlay_bg_color: String,
}

fn default_diagnostics_enabled() -> bool {
    true
}
fn default_sample_interval_ms() -> u64 {
    1000
}
fn default_fd_warn_fraction() -> f64 {
    0.75
}
fn default_overlay_font_size() -> f32 {
    15.0
}
fn default_overlay_fg() -> String {
    "#e0e0e0".to_string()
}
fn default_overlay_bg() -> String {
    "#1c1f26e0".to_string()
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            enabled: default_diagnostics_enabled(),
            sample_interval_ms: default_sample_interval_ms(),
            fd_warn_fraction: default_fd_warn_fraction(),
            overlay_font_size: default_overlay_font_size(),
            overlay_fg_color: default_overlay_fg(),
            overlay_bg_color: default_overlay_bg(),
        }
    }
}

impl Diagnostics {
    /// Overlay text color as straight (non-premultiplied) RGBA in `0.0..=1.0`,
    /// falling back to the default if the configured string won't parse.
    pub fn overlay_fg_rgba(&self) -> [f32; 4] {
        parse_hex_rgba(&self.overlay_fg_color)
            .unwrap_or_else(|| parse_hex_rgba(&default_overlay_fg()).unwrap())
    }

    /// Overlay background color, same semantics as [`overlay_fg_rgba`](Self::overlay_fg_rgba).
    pub fn overlay_bg_rgba(&self) -> [f32; 4] {
        parse_hex_rgba(&self.overlay_bg_color)
            .unwrap_or_else(|| parse_hex_rgba(&default_overlay_bg()).unwrap())
    }
}

/// `[decorations]` — server-side window decorations. The WM advertises
/// `xdg-decoration` ServerSide and, when `border_width > 0`, draws a
/// focus-aware solid-color border ring just inside each window's edges.
///
/// Off by default (`border_width = 0`): the no-decorations workflow stays the
/// default, so well-behaved clients that honor `xdg-decoration` keep their
/// borderless look until you opt in here. Colors are `#RRGGBB` or `#RRGGBBAA`
/// hex; an unparseable value falls back to the default for that field.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decorations {
    /// Border thickness in logical pixels, drawn inside the window's own rect
    /// (so it never bleeds onto neighboring tiles). `0` disables borders.
    #[serde(default)]
    pub border_width: u32,
    /// Border color of the focused window.
    #[serde(default = "default_focused_border")]
    pub focused_color: String,
    /// Border color of unfocused windows.
    #[serde(default = "default_unfocused_border")]
    pub unfocused_color: String,
}

fn default_focused_border() -> String {
    "#5e81ac".to_string()
}
fn default_unfocused_border() -> String {
    "#4c566a".to_string()
}

impl Default for Decorations {
    fn default() -> Self {
        Self {
            border_width: 0,
            focused_color: default_focused_border(),
            unfocused_color: default_unfocused_border(),
        }
    }
}

impl Decorations {
    /// Focused-border color as straight (non-premultiplied) RGBA in `0.0..=1.0`,
    /// falling back to the default color if the configured string won't parse.
    pub fn focused_rgba(&self) -> [f32; 4] {
        parse_hex_rgba(&self.focused_color)
            .unwrap_or_else(|| parse_hex_rgba(&default_focused_border()).unwrap())
    }

    /// Unfocused-border color, same semantics as [`focused_rgba`](Self::focused_rgba).
    pub fn unfocused_rgba(&self) -> [f32; 4] {
        parse_hex_rgba(&self.unfocused_color)
            .unwrap_or_else(|| parse_hex_rgba(&default_unfocused_border()).unwrap())
    }

    /// Names of color fields that don't parse, for a load-time warning. Empty
    /// when both colors are valid (the common case).
    pub fn color_errors(&self) -> Vec<&'static str> {
        let mut errs = Vec::new();
        if parse_hex_rgba(&self.focused_color).is_none() {
            errs.push("decorations.focused_color");
        }
        if parse_hex_rgba(&self.unfocused_color).is_none() {
            errs.push("decorations.unfocused_color");
        }
        errs
    }
}

/// Parse a `#RRGGBB` or `#RRGGBBAA` hex color into straight RGBA floats in
/// `0.0..=1.0`. A leading `#` is optional; missing alpha defaults to opaque.
/// Returns `None` for any other length or a non-hex digit.
fn parse_hex_rgba(s: &str) -> Option<[f32; 4]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 && h.len() != 8 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok();
    let r = byte(0)?;
    let g = byte(1)?;
    let b = byte(2)?;
    let a = if h.len() == 8 { byte(3)? } else { 0xff };
    Some([
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ])
}

/// `[background]` — the desktop background drawn beneath every window and
/// layer-shell surface.
///
/// With no `[background]` section the screen is cleared to [`color`](Self::color)
/// (a dark grey by default, matching the historic hardcoded clear). Set
/// [`image`](Self::image) to a PNG or SVG file to paint a wallpaper on top of
/// that color, positioned per [`mode`](Self::mode). The color still shows in any
/// region the image doesn't cover (e.g. the letterbox bars of `fit`, or the gaps
/// of a non-tiling `center`), so pick a `color` that complements the image.
///
/// The wallpaper is rendered identically into screenshots/screencasts — it is
/// part of the scene, not an on-screen-only overlay.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Background {
    /// Solid clear color, `#RRGGBB` or `#RRGGBBAA` hex. Painted across the whole
    /// output; also the backdrop the [`image`](Self::image) (if any) sits on.
    /// An unparseable value falls back to the default. Default `#1a1a1a`.
    #[serde(default = "default_background_color")]
    pub color: String,
    /// Path to a wallpaper image (PNG or SVG, by extension). `~` and
    /// `$VAR`/`${VAR}` are expanded at load. Unset (default) ⇒ solid
    /// [`color`](Self::color) only. A path that doesn't exist or won't decode
    /// logs a warning and leaves the solid color showing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// How the image is fitted to each output. Default `fill`.
    #[serde(default)]
    pub mode: BackgroundMode,
}

/// Wallpaper fitting strategy. All preserve the image's own pixels; they differ
/// in how the image is scaled/placed within the output rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundMode {
    /// Scale (preserving aspect) to cover the whole output, cropping overflow.
    #[default]
    Fill,
    /// Scale (preserving aspect) to fit entirely within the output, letterboxing
    /// the remainder with [`Background::color`].
    Fit,
    /// No scaling; place the image at its native size, centered. Larger images
    /// are cropped, smaller ones leave a color border.
    Center,
    /// Stretch to exactly the output size, ignoring aspect ratio.
    Stretch,
    /// Tile the image at native size from the top-left, repeating to fill.
    Tile,
}

fn default_background_color() -> String {
    "#1a1a1a".to_string()
}

impl Default for Background {
    fn default() -> Self {
        Self {
            color: default_background_color(),
            image: None,
            mode: BackgroundMode::default(),
        }
    }
}

impl Background {
    /// Clear color as straight (non-premultiplied) RGBA in `0.0..=1.0`, falling
    /// back to the default color if the configured string won't parse.
    pub fn color_rgba(&self) -> [f32; 4] {
        parse_hex_rgba(&self.color)
            .unwrap_or_else(|| parse_hex_rgba(&default_background_color()).unwrap())
    }

    /// The wallpaper image path with `~`/`$VAR` expanded, or `None` when unset.
    pub fn image_path(&self) -> Option<PathBuf> {
        self.image.as_deref().map(expand_path)
    }

    /// Names of fields that don't parse, for a load-time warning. Empty when the
    /// color is valid (the common case).
    pub fn color_errors(&self) -> Vec<&'static str> {
        if parse_hex_rgba(&self.color).is_none() {
            vec!["background.color"]
        } else {
            Vec::new()
        }
    }
}

/// `[debug]` — runtime debug toggles for diagnosing the render path without a
/// recompile. Niri exposes a similar block; the knobs here turn off the
/// DRM/KMS plane optimizations that, when they misbehave, produce the hardest
/// bugs to reason about (glitched scanout, a stuck or torn hardware cursor).
///
/// Every flag defaults `false` (the optimization stays on — normal operation).
/// The render-path flags (`disable_*`) are honored **only on the DRM/KMS (TTY)
/// backend**: the nested winit backend has no hardware planes, so it composites
/// everything regardless and ignores them. Because those flags are read fresh on
/// every frame, a config hot-reload (`reload-config`) applies them on the next
/// frame — no restart needed, which is the point of having them in config
/// rather than a build flag. The one exception is
/// [`protocol_trace`](Debug::protocol_trace), an observability toggle latched
/// once at startup and active on both backends (see its own note).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Debug {
    /// Force every window's content through GL composition instead of letting a
    /// fullscreen/opaque surface be scanned out directly from a primary or
    /// overlay plane. Turn on to rule out direct-scanout as the cause of a
    /// visual glitch (at the cost of the power/latency win scanout buys).
    /// Implies [`disable_overlay_planes`](Self::disable_overlay_planes).
    #[serde(default)]
    pub disable_direct_scanout: bool,
    /// Disable only *overlay*-plane scanout, leaving primary-plane (fullscreen)
    /// scanout in place. A narrower cut than
    /// [`disable_direct_scanout`](Self::disable_direct_scanout) for isolating
    /// overlay-plane-specific issues.
    #[serde(default)]
    pub disable_overlay_planes: bool,
    /// Composite the cursor into the frame instead of using a hardware cursor
    /// plane. Turn on when chasing cursor-plane artifacts (wrong scale, ghosting,
    /// a cursor that lags or sticks); the software cursor is slower but takes the
    /// KMS cursor plane out of the picture.
    #[serde(default)]
    pub disable_cursor_plane: bool,
    /// Log the Wayland wire protocol: every request dispatched from a client and
    /// every event sent to one, per client, to **stderr** (`<- interface@id.msg`
    /// for requests, `-> …` for events). The built-in equivalent of running with
    /// `WAYLAND_DEBUG=server`, which is exactly what this turns on under the hood.
    ///
    /// Unlike the scanout flags above this is **read once at startup** (the
    /// protocol backend latches it when the display is created), so it is *not*
    /// hot-reloadable and applies on both backends. An explicit `WAYLAND_DEBUG`
    /// in the environment always wins. Leave off for normal use — a busy session
    /// logs thousands of lines a second.
    #[serde(default)]
    pub protocol_trace: bool,
}

/// Expand a leading `~` (home) and `$VAR` / `${VAR}` environment references in a
/// path string. Unknown variables expand to empty, matching shell behavior.
fn expand_path(s: &str) -> PathBuf {
    let mut out = String::with_capacity(s.len());
    let after_home = if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            out.push_str(&home);
            out.push('/');
        }
        rest
    } else {
        s
    };
    let bytes = after_home.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            let (name, next) = if bytes[i + 1] == b'{' {
                // ${VAR}
                match after_home[i + 2..].find('}') {
                    Some(end) => (&after_home[i + 2..i + 2 + end], i + 2 + end + 1),
                    None => {
                        out.push('$');
                        i += 1;
                        continue;
                    }
                }
            } else {
                // $VAR — letters, digits, underscore.
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == start {
                    out.push('$');
                    i += 1;
                    continue;
                }
                (&after_home[start..end], end)
            };
            if let Ok(val) = std::env::var(name) {
                out.push_str(&val);
            }
            i = next;
        } else {
            out.push(after_home[i..].chars().next().unwrap());
            i += after_home[i..].chars().next().unwrap().len_utf8();
        }
    }
    PathBuf::from(out)
}

/// `[input]` — libinput device tuning applied to every applicable device on
/// connect (and re-applied on config hot-reload), so touchpad/pointer
/// behaviour can be set here instead of via udev/kernel rules.
///
/// The section is **declarative**: every field is optional, and an unset
/// field applies the device's own libinput default. Editing or *removing* a
/// field and reloading therefore takes effect immediately — a removed field
/// resets that knob to the default rather than leaving the last value. A
/// setting a given device doesn't support (e.g. tap-to-click on a wired
/// mouse) is silently ignored for that device, so a single global section is
/// safe across mixed hardware. The settings apply only on the native TTY/udev
/// backend; the nested winit backend has no real input devices and ignores
/// this section.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Tap-to-click on touchpads (`libinput_device_config_tap_set_enabled`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_to_click: Option<bool>,
    /// Which button 1/2/3-finger taps map to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_button_map: Option<TapButtonMap>,
    /// Tap-and-drag (a tap immediately followed by a finger-down drags).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap_and_drag: Option<bool>,
    /// Drag-lock: keep dragging after the finger lifts, until the next tap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drag_lock: Option<bool>,
    /// Natural (reversed) scrolling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub natural_scroll: Option<bool>,
    /// How scroll events are produced (two-finger, edge, on-button, none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_method: Option<ScrollMethod>,
    /// Button used for on-button-down scrolling (evdev button code, e.g.
    /// `274` for middle). Only meaningful with `scroll_method = "on-button-down"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_button: Option<u32>,
    /// How software button clicks are emulated on clickpads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click_method: Option<ClickMethod>,
    /// Pointer acceleration speed in `[-1.0, 1.0]` (0 = libinput default).
    /// Clamped to that range when applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_speed: Option<f64>,
    /// Pointer acceleration profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_profile: Option<AccelProfile>,
    /// Disable the touchpad while typing on the internal keyboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_while_typing: Option<bool>,
    /// Swap left/right buttons for left-handed use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_handed: Option<bool>,
    /// Middle-button emulation (left+right chord → middle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middle_emulation: Option<bool>,
}

/// `[touch]` — touchscreen routing. Separate from `[input]` because this is a
/// compositor-level routing decision, not a libinput per-device knob: a
/// touchscreen reports contacts in its own normalized `[0,1]²` space, and on a
/// multi-output desktop that space has to be projected onto the *one* output the
/// panel physically overlays. We pick that output as: this explicit mapping if
/// set, else the output libinput reports for the device (when a udev rule tagged
/// it), else the first output (correct for the common single-output case).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Touch {
    /// Connector name of the output every touchscreen maps onto (e.g.
    /// `"eDP-1"`, `"HDMI-A-1"`, as listed by ``shoestring-ctl outputs``). Unset
    /// (the default) leaves touch on the libinput-reported or first output. Read
    /// fresh per contact, so a hot-reload retargets touch immediately. A name
    /// that matches no current output falls back as if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_to_output: Option<String>,
}

/// Which physical button each tap maps to (libinput `TapButtonMap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapButtonMap {
    /// 1/2/3-finger tap → left/right/middle.
    LeftRightMiddle,
    /// 1/2/3-finger tap → left/middle/right.
    LeftMiddleRight,
}

/// Scroll method (libinput `ScrollMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScrollMethod {
    /// Never emit scroll events from motion (wheels still scroll).
    None,
    /// Two fingers on the touchpad.
    TwoFinger,
    /// Dragging along the bottom/right edge.
    Edge,
    /// Holding `scroll_button` and moving.
    OnButtonDown,
}

/// Software click-emulation method on clickpads (libinput `ClickMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClickMethod {
    /// Bottom-of-pad button areas.
    ButtonAreas,
    /// Number of fingers down picks the button.
    Clickfinger,
}

/// Pointer acceleration profile (libinput `AccelProfile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccelProfile {
    /// Speed-dependent acceleration (libinput default for most devices).
    Adaptive,
    /// Constant factor, no acceleration.
    Flat,
}

/// Workspace layout. `count` controls how many workspaces exist (and
/// the number of boxes a status bar renders). `names` is a sparse
/// 1-based map of optional human labels; slots without an entry fall
/// back to their number.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspaces {
    /// Number of workspaces, 1..=32. Defaults to 16. Out-of-range
    /// values get clamped at load time with a warning.
    #[serde(default = "default_workspace_count")]
    pub count: u8,
    /// `1`-based map of `index → display name`. TOML keys are strings
    /// (TOML has no integer keys), parsed back to `u8` at WM startup;
    /// entries with non-numeric or out-of-range keys are dropped with
    /// a warning. Use `BTreeMap` so the serialized order is stable
    /// across `--write-default-config`.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
}

fn default_workspace_count() -> u8 {
    16
}

/// Hard ceiling on `[workspaces].count`. Beyond this, MRU stacks and
/// the bar's workspace cluster start to crowd a 1080p display; if a
/// real user needs more we'll raise it then.
pub const MAX_WORKSPACE_COUNT: u8 = 32;

impl Default for Workspaces {
    fn default() -> Self {
        Self {
            count: default_workspace_count(),
            names: BTreeMap::new(),
        }
    }
}

/// A single window-rule entry. Both the matcher and the action set can
/// be sparse — only fields the user filled in are evaluated.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    /// All set fields must match for the rule to apply. An empty matcher
    /// (no fields set) matches *every* window — almost never what a user
    /// wants, but legal so a top-of-list default-applies entry is
    /// possible.
    #[serde(rename = "match")]
    pub matcher: WindowMatch,
    pub actions: WindowActions,
}

/// Matcher predicates. All set fields are AND-ed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowMatch {
    /// Exact-match on the toplevel's `app_id` (xdg-shell). Use this for
    /// the common case; under Wayland `app_id` is the closest analogue
    /// to the X11 `WM_CLASS` that older WMs match on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Case-sensitive substring match against the toplevel title.
    /// Prefer this for the common case; reach for [`Self::title_regex`]
    /// only when a substring can't express the match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    /// Regex match against the `app_id` (Rust `regex` syntax, unanchored —
    /// `firefox` matches anywhere; use `^firefox$` for an exact match). AND-ed
    /// with the other fields, so it composes with an exact [`Self::app_id`] or
    /// a [`Self::title_contains`]. An invalid pattern never matches (and is
    /// reported at config load — see [`Config::window_rule_regex_errors`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id_regex: Option<String>,
    /// Regex match against the toplevel title (same syntax/semantics as
    /// [`Self::app_id_regex`]). The regex counterpart to
    /// [`Self::title_contains`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_regex: Option<String>,
}

/// Actions applied to a matched window. Each is independently optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindowActions {
    /// 1-based target workspace (1..=16). Window is moved off the
    /// currently-active workspace if needed; the user's view does not
    /// follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<u8>,
    /// Compositor-space position `[x, y]` to map the window at, in
    /// logical pixels. Overrides the auto-centered spawn location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[i32; 2]>,
    /// Preferred size `[w, h]` (logical pixels). Sent to the client as
    /// part of the next configure; the client may negotiate a different
    /// size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[i32; 2]>,
    /// Make the window sticky (shown on every workspace). `Some(true)` pins
    /// it across workspace switches; `Some(false)` and the default (`None`)
    /// leave it as an ordinary per-workspace window. See [`Action::ToggleSticky`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky: Option<bool>,
    /// Keep the window above all ordinary windows. `Some(true)` pins it to
    /// the always-on-top layer; `Some(false)` and the default (`None`) leave
    /// it in normal stacking. See [`Action::ToggleAlwaysOnTop`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_on_top: Option<bool>,
    /// Place the window on the named output (connector name, e.g. `"DP-1"`),
    /// centering it within that output's usable area. Applied before
    /// `position`, so an explicit `position` still wins. A name that matches
    /// no connected output is ignored with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Apply an initial tiling layout instead of leaving the window floating.
    /// Computed against whichever output the window ends up on (so combine
    /// with `output` to tile on a specific monitor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<RuleLayout>,
}

/// The layout a `[[window_rules]]` `layout` action puts a window into. Mirrors
/// the WM's internal layout states (and the `layout` field the IPC `windows` /
/// `get_tree` snapshots report), minus the app-driven `fullscreen` state which
/// a rule can't sensibly force.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuleLayout {
    /// Leave the window floating (the default; explicit for clarity).
    Floating,
    /// Tile to the left half of the output's usable area.
    TiledLeft,
    /// Tile to the right half.
    TiledRight,
    /// Maximize to the output's usable area (minus bars/docks).
    Maximized,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default)]
    pub focus_mode: FocusMode,
    /// Milliseconds before a held key starts repeating. Matches the
    /// X server's default ("Repeat delay"). Lower = more aggressive.
    #[serde(default = "default_repeat_delay")]
    pub repeat_delay: i32,
    /// Repeats-per-second once repeat kicks in.
    #[serde(default = "default_repeat_rate")]
    pub repeat_rate: i32,
    /// Mouse-wheel detents required to switch one workspace when scrolling
    /// the bare desktop (no window/layer surface under the pointer). `1`
    /// switches one workspace per physical notch; higher values slow it
    /// down (e.g. `2` = two notches per switch). High-resolution wheels
    /// that emit several sub-detent events per notch are accumulated, so a
    /// single notch never overshoots regardless of this value. Touchpad
    /// scrolling is unaffected. Treated as at least `1`.
    #[serde(default = "default_desktop_scroll_notches")]
    pub desktop_scroll_notches: u32,
    /// Global scale factor fallback, used for any output that does not have a
    /// per-output `scale` entry in `[outputs.<name>]`. Whole values (1.0,
    /// 2.0, …) are sent as integer scales; non-integer values use fractional
    /// scaling (`wp_fractional_scale_v1`). Match this to your `Xft.dpi`
    /// equivalent so text size carries over from an X session.
    #[serde(default = "default_output_scale")]
    pub output_scale: f64,
    /// Command spawned for `Action::Lock` and the `Lock` IPC request. The
    /// string is split on whitespace; the first token is the executable
    /// and the rest are passed as arguments. Defaults to
    /// `"shoestring-lock"`, which is expected to be on `$PATH`. Replace
    /// to substitute e.g. `swaylock` or a custom locker.
    #[serde(default = "default_lock_command")]
    pub lock_command: String,
    /// Commands spawned once at WM startup, after the wayland socket is
    /// listening but before user interaction. Each entry is split on
    /// whitespace (first token = executable, rest = args), same as
    /// `lock_command`. Failures log a warning and don't block startup.
    /// Default: `["shoestring-bar", "shoestring-mediad"]` so a fresh user gets
    /// the status bar plus the media-privacy monitor (which feeds the bar's
    /// MUTE/MIC/CAM indicators). `shoestring-mediad` links PipeWire; where it's
    /// absent (no-PipeWire build) the spawn just logs a warning and the bar
    /// shows no media chips — a graceful no-op.
    #[serde(default = "default_autostart")]
    pub autostart: Vec<String>,
    /// Foundational gate for remote-automation IPC methods (key/text/click
    /// injection, future remote screenshot + command exec). Off by default
    /// so an attacker who only has socket access cannot drive the session.
    /// The CLI flag `--enable-automation` and the runtime IPC
    /// `set_automation` request both override this without writing back to
    /// disk — the config file stays the source of truth at next start.
    #[serde(default)]
    pub automation_enabled: bool,
    /// Gate for screen capture via the `zwlr_screencopy_v1` protocol — the
    /// path external tools (OBS, grim, the xdg-desktop-portal-wlr screencast
    /// backend) use to read your screen. Off by default: unlike X11, Wayland
    /// isolates clients, and screencopy is the sanctioned exception, so
    /// leaving it off means a stray or malicious client simply cannot capture
    /// the screen. When `false` the `zwlr_screencopy_manager_v1` global is not
    /// advertised at all *and* any capture request is refused. Flip it on (in
    /// config for always-on, or at runtime via the `set_screen_capture` IPC /
    /// `shoestring-ctl screen-capture on`) when you actually want to screen
    /// share or record. The runtime toggle does not write back to disk — this
    /// key stays the source of truth at next start. Does not affect the IPC
    /// `screenshot` request (separately behind the automation gate).
    #[serde(default)]
    pub screen_capture_enabled: bool,
    /// Advertise the `ext_idle_notify_v1` global so clients (idle daemons,
    /// screen-dimmers, auto-lockers) can ask to be told after N ms of no
    /// input. Off by default: on a desktop that never travels, idle
    /// behaviour is mostly an annoyance, and not advertising the global at
    /// all means a stray `swayidle` simply finds nothing to talk to. Flip
    /// to `true` on a laptop where you do want idle dimming/locking.
    #[serde(default)]
    pub idle_notifications_enabled: bool,
    /// XKB keyboard layout(s): a comma-separated list of layout codes — e.g.
    /// `"us"`, `"us,de"`, `"fr,ru"`. With more than one, `Action::CycleLayout`
    /// (bound to Super+Space by default) switches between them at runtime.
    /// Empty uses xkbcommon's default (the `XKB_DEFAULT_LAYOUT` environment
    /// variable, usually `us`).
    #[serde(default)]
    pub xkb_layout: String,
    /// XKB variant(s), comma-separated, one per layout — e.g. `"dvorak"`, or
    /// `",nodeadkeys"` (default variant for the first layout, nodeadkeys for
    /// the second). Empty uses the default variant of each layout.
    #[serde(default)]
    pub xkb_variant: String,
    /// XKB options, comma-separated — non-layout tweaks like `"ctrl:nocaps"`
    /// (Caps Lock acts as Ctrl) or `"grp:alt_shift_toggle"` (also switch
    /// layouts with Alt+Shift). Unset leaves xkbcommon's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xkb_options: Option<String>,
    /// XKB rules file (rarely changed; usually `"evdev"`). Empty uses the
    /// xkbcommon default.
    #[serde(default)]
    pub xkb_rules: String,
    /// XKB keyboard model (rarely changed; e.g. `"pc105"`). Empty uses the
    /// xkbcommon default.
    #[serde(default)]
    pub xkb_model: String,
}

fn default_repeat_delay() -> i32 {
    600
}
fn default_repeat_rate() -> i32 {
    25
}
fn default_desktop_scroll_notches() -> u32 {
    1
}
fn default_output_scale() -> f64 {
    1.0
}
fn default_lock_command() -> String {
    "shoestring-lock".into()
}
fn default_autostart() -> Vec<String> {
    vec!["shoestring-bar".into(), "shoestring-mediad".into()]
}

impl Default for General {
    fn default() -> Self {
        Self {
            focus_mode: FocusMode::default(),
            repeat_delay: default_repeat_delay(),
            repeat_rate: default_repeat_rate(),
            desktop_scroll_notches: default_desktop_scroll_notches(),
            output_scale: default_output_scale(),
            lock_command: default_lock_command(),
            autostart: default_autostart(),
            automation_enabled: false,
            screen_capture_enabled: false,
            idle_notifications_enabled: false,
            xkb_layout: String::new(),
            xkb_variant: String::new(),
            xkb_options: None,
            xkb_rules: String::new(),
            xkb_model: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusMode {
    #[default]
    ClickToFocus,
    FollowsMouse,
    Sloppy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Modifier names; case-insensitive. Recognized: Super, Ctrl, Alt, Shift.
    #[serde(default)]
    pub mods: Vec<String>,
    /// xkb keysym name, e.g. "Return", "q", "1".
    pub key: String,
    pub action: Action,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    /// Run a command. `args` is split by the parser; pass tokens individually.
    Spawn {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Exit the WM cleanly.
    Quit,
    /// Power off the machine, behind the same `shoestring-confirm` dialog as
    /// [`Action::Quit`]. The WM owns no power policy — it shells out to the
    /// first available of `systemctl` / `loginctl` / `shutdown(8)`. Bind to
    /// `XF86PowerOff` to make the hardware power key prompt instead of
    /// shutting down immediately — note `logind` handles that key itself by
    /// default, so also set `HandlePowerKey=ignore` (see docs/install.rst).
    PowerOff,
    /// Reboot the machine; confirm-gated like [`Action::PowerOff`].
    Reboot,
    /// Suspend (sleep / S3) the machine; confirm-gated like
    /// [`Action::PowerOff`]. Bind to `XF86Sleep` (with `HandleSuspendKey=ignore`).
    Suspend,
    /// Re-read the config file from disk.
    ReloadConfig,
    /// Snap the focused window to the left half of its monitor's usable rect.
    /// Toggles back to the saved floating rect when re-pressed.
    TileLeft,
    /// Snap the focused window to the right half of its monitor.
    TileRight,
    /// Maximize the focused window to its monitor's usable rect (toggle).
    Maximize,
    /// One-shot: tile every window on the active workspace into an even-ish
    /// grid (~√n rows). Computed per output from the current window count and
    /// order; the windows stay floating afterwards (no persistent layout, no
    /// reflow on later map/close). Wire form `{"type":"arrange-grid"}`.
    ArrangeGrid,
    /// One-shot: tile the active workspace into a fibonacci spiral. Like
    /// [`Action::ArrangeGrid`] but each window halves the remaining space with
    /// the kept corner rotating inward. Wire form `{"type":"arrange-spiral"}`.
    ArrangeSpiral,
    /// One-shot: tile the active workspace with a binary "dwindle" split — like
    /// [`Action::ArrangeSpiral`] but the kept corner is fixed, so windows
    /// shrink toward one corner. Wire form `{"type":"arrange-bsp"}`.
    ArrangeBsp,
    /// Hide the focused window without destroying it. Restore via `unminimize`.
    Minimize,
    /// Restore the most-recently-minimized window.
    Unminimize,
    /// Ask the focused window's client to close gracefully.
    Close,
    /// Cycle keyboard focus to the next window on the active workspace,
    /// raising it (Alt+Tab). Round-robins through every window and wraps;
    /// a no-op when the workspace has fewer than two windows.
    CycleWindows,
    /// Raise the focused window to the top of the stacking order without
    /// changing which window holds keyboard focus. A no-op when no window is
    /// focused or the focused window is unmapped (minimized).
    Raise,
    /// Lower the focused window to the bottom of the stacking order, again
    /// without touching keyboard focus. The complement of [`Action::Raise`].
    Lower,
    /// Toggle the "sticky" flag on the focused window. A sticky window is
    /// shown on every workspace — it stays mapped (and keeps its position)
    /// across workspace switches instead of being hidden with the rest of
    /// its workspace. Useful for a reference doc or picture-in-picture video.
    /// A no-op when no window is focused.
    ToggleSticky,
    /// Toggle the "always on top" flag on the focused window. An
    /// always-on-top window stays above all ordinary windows in the
    /// stacking order regardless of focus — clicking another window raises
    /// it only as far as just below the always-on-top layer. Combine with
    /// [`Action::ToggleSticky`] for a picture-in-picture window. A no-op
    /// when no window is focused.
    ToggleAlwaysOnTop,
    /// Switch every output to show the windows on workspace `index`
    /// (1-based; valid range 1..=16).
    FocusWorkspace { index: u8 },
    /// Switch workspace by a relative offset (-1 = previous, +1 = next).
    /// Saturating at 1 and 16 (no wrap).
    FocusWorkspaceRelative { delta: i8 },
    /// Move the focused window to workspace `index` (1-based) and stay on the
    /// current workspace.
    MoveWindowToWorkspace { index: u8 },
    /// Move the focused window to a workspace offset (-1 / +1).
    MoveWindowToWorkspaceRelative { delta: i8 },
    /// Switch to Linux virtual terminal `vt` (1..=12). Only effective when
    /// running on the TTY backend; no-op with a warning under winit.
    ChangeVt { vt: u8 },
    /// Synthesize a single keypress (press + release) targeting whichever
    /// surface holds keyboard focus. `keysym` is an X keysym name (e.g.
    /// `"Return"`, `"F5"`, `"q"`).
    InjectKey { keysym: String },
    /// Synthesize a sequence of keypresses that types `text`. ASCII letters,
    /// digits, and space only (v1).
    InjectText { text: String },
    /// Synthesize a single mouse click at the current pointer location.
    /// `button` is `"left"` / `"right"` / `"middle"` or a numeric BTN_*
    /// code (e.g. `"272"`).
    InjectClick { button: String },
    /// Spawn the configured lock binary
    /// ([`General::lock_command`]). The binary is expected to bind
    /// `ext-session-lock-v1` and request a lock; this action does not
    /// itself drive the protocol so a misconfigured / missing binary
    /// just logs and leaves the session unlocked.
    Lock,
    /// Cycle the active keyboard layout to the next entry in
    /// [`General::xkb_layout`], wrapping at the end. A no-op when only one
    /// layout is configured. Bound to Super+Space by default.
    CycleLayout,
    /// Toggle the default audio sink (output) mute. The WM doesn't mute
    /// anything itself — it spawns `shoestring-mediad audio-mute toggle`, which
    /// flips PipeWire's real default-sink mute; the new state returns via the
    /// monitor's `Request::ReportMedia`. Using `toggle` (not a cached bool)
    /// keeps us honest when media keys / pavucontrol changed it underneath us.
    ToggleAudioMute,
    /// Toggle the default audio source (microphone) mute, the mic analogue of
    /// [`Action::ToggleAudioMute`]. Spawns `shoestring-mediad mic-mute toggle`.
    ToggleMicMute,
    /// Toggle the on-screen diagnostics overlay — a Minecraft-F3-style panel
    /// the WM draws (top-left of the output under the pointer) from the live
    /// metrics registry. Purely a visualization of the same data
    /// [`Request::Metrics`] serves; no effect on the metrics themselves. Bound
    /// to Super+F3 by default. Values refresh on the `[diagnostics]` sampler,
    /// so set `enabled = true` (the default) for them to update live.
    ToggleDiagnostics,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
}

/// Serialize a default config to TOML with a header comment explaining how
/// the file is meant to be edited. Used by `shoestring-wm --write-default-config`
/// (ranger-style) and intended for packagers to drop as `/etc/skel`.
pub fn default_config_toml() -> String {
    let cfg = Config::with_default_bindings();
    let body = toml::to_string_pretty(&cfg).expect("default config must serialize");
    let header = "\
# shoestring-wm configuration. Regenerate this file at any time with:
#   shoestring-wm --write-default-config
#
# Action types: spawn, quit, reload-config, tile-left, tile-right, maximize,
# minimize, unminimize, close, cycle-windows, raise, lower, toggle-sticky,
# toggle-always-on-top, focus-workspace, focus-workspace-relative,
# move-window-to-workspace, move-window-to-workspace-relative, change-vt,
# inject-key, inject-text, inject-click, lock, cycle-layout,
# toggle-audio-mute, toggle-mic-mute, toggle-diagnostics.
#
# Modifier names (case-insensitive): Super, Ctrl, Alt, Shift.
# Key names use xkb keysym strings (e.g. \"Return\", \"q\", \"F1\").
";
    format!("{header}\n{body}")
}

/// Resolve the user's config file path. Honors `$XDG_CONFIG_HOME`, falling
/// back to `$HOME/.config`. Returns `None` if neither env var is set.
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("shoestring-wm").join("config.toml"))
}

/// Load config from the given path. Returns the parsed config and the path
/// it was read from (useful for hot-reload).
pub fn load_from(path: &Path) -> Result<Config, LoadError> {
    let text = fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}

/// Load from `path` if provided; else from the default path. Returns
/// `Config::default()` (with the bundled default bindings) when no file
/// exists at either location.
pub fn load_or_default(path: Option<&Path>) -> Result<(Config, Option<PathBuf>), LoadError> {
    let resolved = path.map(PathBuf::from).or_else(default_config_path);
    if let Some(p) = resolved {
        if p.exists() {
            return Ok((load_from(&p)?, Some(p)));
        }
    }
    Ok((Config::with_default_bindings(), None))
}

impl Config {
    /// A starter config so a fresh user has working binds out of the box.
    /// Mirrors the Openbox layout the user is migrating from: E=left half,
    /// W=right half, M=maximize toggle, D=minimize, X=close.
    pub fn with_default_bindings() -> Self {
        let super_only = || vec!["Super".into()];
        let super_shift = || vec!["Super".into(), "Shift".into()];
        let super_ctrl = || vec!["Super".into(), "Ctrl".into()];

        let mut bindings = vec![
            Binding {
                mods: super_only(),
                key: "Return".into(),
                action: Action::Spawn {
                    command: "alacritty".into(),
                    args: vec![],
                },
            },
            Binding {
                mods: super_shift(),
                key: "q".into(),
                action: Action::Quit,
            },
            // Launcher: Super+P → command picker, Super+B → bookmarks.
            // Spawn the shoestring-menu binary; if it's not on $PATH the
            // spawn fails and the WM logs a warning — bind still defined
            // so installing it later "just works".
            Binding {
                mods: super_only(),
                key: "p".into(),
                action: Action::Spawn {
                    command: "shoestring-menu".into(),
                    args: vec![],
                },
            },
            Binding {
                mods: super_only(),
                key: "b".into(),
                action: Action::Spawn {
                    command: "shoestring-menu".into(),
                    args: vec!["--mode".into(), "bookmarks".into()],
                },
            },
            // Window-jump menu: fuzzy-focus any mapped window across all
            // workspaces. `j` for "jump".
            Binding {
                mods: super_only(),
                key: "j".into(),
                action: Action::Spawn {
                    command: "shoestring-menu".into(),
                    args: vec!["--mode".into(), "windows".into()],
                },
            },
            Binding {
                mods: super_only(),
                key: "e".into(),
                action: Action::TileLeft,
            },
            Binding {
                mods: super_only(),
                key: "w".into(),
                action: Action::TileRight,
            },
            Binding {
                mods: super_only(),
                key: "m".into(),
                action: Action::Maximize,
            },
            // One-shot auto-arrange of the active workspace, on the "g" cluster
            // (g for grid/arrange): Super+G grid, +Shift spiral, +Ctrl dwindle.
            Binding {
                mods: super_only(),
                key: "g".into(),
                action: Action::ArrangeGrid,
            },
            Binding {
                mods: super_shift(),
                key: "g".into(),
                action: Action::ArrangeSpiral,
            },
            Binding {
                mods: super_ctrl(),
                key: "g".into(),
                action: Action::ArrangeBsp,
            },
            Binding {
                mods: super_only(),
                key: "d".into(),
                action: Action::Minimize,
            },
            Binding {
                mods: super_shift(),
                key: "d".into(),
                action: Action::Unminimize,
            },
            Binding {
                mods: super_only(),
                key: "x".into(),
                action: Action::Close,
            },
            // Alt+Tab cycles focus through the active workspace's windows,
            // raising each in turn — the conventional window switcher.
            Binding {
                mods: vec!["Alt".into()],
                key: "Tab".into(),
                action: Action::CycleWindows,
            },
            // Super+Down does the same thing — an alternate, one-hand
            // friendly window switcher.
            Binding {
                mods: super_only(),
                key: "Down".into(),
                action: Action::CycleWindows,
            },
            // Explicit stacking control for the focused window: Super+Up
            // brings it to the front, Super+Shift+Up pushes it to the back.
            // Neither moves keyboard focus — they only restack.
            Binding {
                mods: super_only(),
                key: "Up".into(),
                action: Action::Raise,
            },
            Binding {
                mods: super_shift(),
                key: "Up".into(),
                action: Action::Lower,
            },
            // Toggle "show on all workspaces" for the focused window.
            // `s` for sticky.
            Binding {
                mods: super_only(),
                key: "s".into(),
                action: Action::ToggleSticky,
            },
            // Toggle "always on top" for the focused window. `a` for above.
            Binding {
                mods: super_only(),
                key: "a".into(),
                action: Action::ToggleAlwaysOnTop,
            },
            // Lock screen. Spawns `general.lock_command` (default
            // `shoestring-lock`) which binds ext-session-lock-v1.
            // Super+L is already workspace-next; pair with Shift.
            Binding {
                mods: super_shift(),
                key: "l".into(),
                action: Action::Lock,
            },
            // Cycle keyboard layout (Super+Space), matching the GNOME/macOS
            // muscle memory. A no-op until [general].xkb_layout lists more
            // than one layout.
            Binding {
                mods: super_only(),
                key: "space".into(),
                action: Action::CycleLayout,
            },
            // Workspace navigation — mirrors the user's Openbox W-h / W-l.
            Binding {
                mods: super_only(),
                key: "h".into(),
                action: Action::FocusWorkspaceRelative { delta: -1 },
            },
            Binding {
                mods: super_only(),
                key: "l".into(),
                action: Action::FocusWorkspaceRelative { delta: 1 },
            },
            Binding {
                mods: super_ctrl(),
                key: "h".into(),
                action: Action::MoveWindowToWorkspaceRelative { delta: -1 },
            },
            Binding {
                mods: super_ctrl(),
                key: "l".into(),
                action: Action::MoveWindowToWorkspaceRelative { delta: 1 },
            },
        ];
        // Super+F3 → toggle the on-screen diagnostics overlay (F3-style).
        bindings.push(Binding {
            mods: super_only(),
            key: "F3".into(),
            action: Action::ToggleDiagnostics,
        });
        // Super+1..9 → focus workspace 1..9; Super+Shift+1..9 → move window there.
        for n in 1u8..=9 {
            let key = char::from(b'0' + n).to_string();
            bindings.push(Binding {
                mods: super_only(),
                key: key.clone(),
                action: Action::FocusWorkspace { index: n },
            });
            bindings.push(Binding {
                mods: super_shift(),
                key,
                action: Action::MoveWindowToWorkspace { index: n },
            });
        }
        // Ctrl+Alt+F1..F12 → VT switch (TTY backend only). Matches getty /
        // X / Openbox behavior so muscle memory carries over.
        let ctrl_alt = || vec!["Ctrl".into(), "Alt".into()];
        for n in 1u8..=12 {
            bindings.push(Binding {
                mods: ctrl_alt(),
                key: format!("F{n}"),
                action: Action::ChangeVt { vt: n },
            });
        }
        // XF86 media keys → action scripts under scripts/actions/.
        // Spawning fails silently with a log warning if the user hasn't
        // installed the scripts on $PATH — the bind still resolves, so
        // putting them on $PATH later "just works" without a config edit.
        for (key, command) in [
            ("XF86AudioRaiseVolume", "shoestring-volume-up"),
            ("XF86AudioLowerVolume", "shoestring-volume-down"),
            ("XF86AudioMute", "shoestring-volume-mute"),
            ("XF86AudioMicMute", "shoestring-mic-mute"),
            ("XF86MonBrightnessUp", "shoestring-brightness-up"),
            ("XF86MonBrightnessDown", "shoestring-brightness-down"),
        ] {
            bindings.push(Binding {
                mods: Vec::new(),
                key: key.into(),
                action: Action::Spawn {
                    command: command.into(),
                    args: Vec::new(),
                },
            });
        }
        Self {
            general: General::default(),
            workspaces: Workspaces::default(),
            bindings,
            window_rules: Vec::new(),
            outputs: BTreeMap::new(),
            diagnostics: Diagnostics::default(),
            input: Input::default(),
            touch: Touch::default(),
            portal: Portal::default(),
            decorations: Decorations::default(),
            background: Background::default(),
            debug: Debug::default(),
        }
    }
}

impl WindowMatch {
    /// `true` when every set field matches the given window facts. An
    /// empty matcher matches everything; see [`WindowRule::matcher`].
    pub fn matches(&self, app_id: &str, title: &str) -> bool {
        if let Some(want) = &self.app_id {
            if want != app_id {
                return false;
            }
        }
        if let Some(needle) = &self.title_contains {
            if !title.contains(needle.as_str()) {
                return false;
            }
        }
        // Regex fields are AND-ed in too. An invalid pattern matches nothing
        // (fail closed); the bad pattern is surfaced separately at config
        // load via [`Config::window_rule_regex_errors`]. Patterns are tiny
        // and rules fire once per window, so compiling here is cheap enough.
        if let Some(pat) = &self.app_id_regex {
            if !regex::Regex::new(pat).is_ok_and(|re| re.is_match(app_id)) {
                return false;
            }
        }
        if let Some(pat) = &self.title_regex {
            if !regex::Regex::new(pat).is_ok_and(|re| re.is_match(title)) {
                return false;
            }
        }
        true
    }
}

impl Config {
    /// Human-readable errors for every `[[window_rules]]` matcher whose
    /// `app_id_regex` / `title_regex` fails to compile. Empty when all
    /// patterns are valid (or none are set). The WM calls this at config
    /// load + reload and logs each entry as a warning — a bad pattern is
    /// non-fatal (the matcher simply never fires) but worth surfacing so a
    /// typo isn't silently ignored.
    pub fn window_rule_regex_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        for (i, rule) in self.window_rules.iter().enumerate() {
            for (field, pat) in [
                ("app_id_regex", &rule.matcher.app_id_regex),
                ("title_regex", &rule.matcher.title_regex),
            ] {
                if let Some(pat) = pat {
                    if let Err(e) = regex::Regex::new(pat) {
                        errors.push(format!(
                            "window_rules[{i}].match.{field}: invalid regex {pat:?}: {e}"
                        ));
                    }
                }
            }
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let toml = r#"
            [[bindings]]
            mods = ["Super"]
            key = "Return"
            action = { type = "spawn", command = "alacritty" }

            [[bindings]]
            mods = ["Super", "Shift"]
            key = "q"
            action = { type = "quit" }
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.bindings.len(), 2);
        assert!(matches!(cfg.bindings[1].action, Action::Quit));
    }

    #[test]
    fn focus_mode_defaults_to_click() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.general.focus_mode, FocusMode::ClickToFocus);
    }

    #[test]
    fn output_scale_defaults_to_one() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.general.output_scale, 1.0);
    }

    #[test]
    fn desktop_scroll_notches_defaults_to_one() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.general.desktop_scroll_notches, 1);
    }

    #[test]
    fn desktop_scroll_notches_user_override() {
        let cfg: Config = toml::from_str("[general]\ndesktop_scroll_notches = 3\n").unwrap();
        assert_eq!(cfg.general.desktop_scroll_notches, 3);
    }

    #[test]
    fn window_rules_default_empty_and_parse() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.window_rules.is_empty());

        let toml = r#"
            [[window_rules]]
            match = { app_id = "firefox" }
            actions = { workspace = 3 }

            [[window_rules]]
            match = { title_contains = "Slack" }
            actions = { position = [100, 50], size = [1200, 800] }
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.window_rules.len(), 2);
        assert_eq!(
            cfg.window_rules[0].matcher.app_id.as_deref(),
            Some("firefox")
        );
        assert_eq!(cfg.window_rules[0].actions.workspace, Some(3));
        assert_eq!(
            cfg.window_rules[1].matcher.title_contains.as_deref(),
            Some("Slack")
        );
        assert_eq!(cfg.window_rules[1].actions.position, Some([100, 50]));
        assert_eq!(cfg.window_rules[1].actions.size, Some([1200, 800]));
    }

    #[test]
    fn window_match_semantics() {
        let only_app = WindowMatch {
            app_id: Some("firefox".into()),
            ..Default::default()
        };
        assert!(only_app.matches("firefox", "anything"));
        assert!(!only_app.matches("chromium", "Firefox-like"));

        let app_and_title = WindowMatch {
            app_id: Some("term".into()),
            title_contains: Some("vim".into()),
            ..Default::default()
        };
        assert!(app_and_title.matches("term", "nvim buffer"));
        assert!(!app_and_title.matches("term", "fish shell"));
        assert!(!app_and_title.matches("other", "vim"));

        // Empty matcher = wildcard.
        let empty = WindowMatch::default();
        assert!(empty.matches("anything", "anything"));
    }

    #[test]
    fn window_match_regex_fields() {
        // Unanchored app_id regex, AND-ed with a title regex.
        let m = WindowMatch {
            app_id_regex: Some("^(firefox|chromium)$".into()),
            title_regex: Some(r"\bGitHub\b".into()),
            ..Default::default()
        };
        assert!(m.matches("firefox", "PR #12 · GitHub"));
        assert!(!m.matches("firefox", "no match here")); // title fails
        assert!(!m.matches("firefoxdev", "x GitHub y")); // anchored app_id fails

        // Unanchored: a bare pattern matches as a substring.
        let sub = WindowMatch {
            app_id_regex: Some("fire".into()),
            ..Default::default()
        };
        assert!(sub.matches("firefoxdeveloperedition", "t"));

        // An invalid pattern matches nothing (fails closed).
        let bad = WindowMatch {
            app_id_regex: Some("(unclosed".into()),
            ..Default::default()
        };
        assert!(!bad.matches("anything", "t"));
    }

    #[test]
    fn window_rule_new_actions_parse() {
        let toml_src = "\
[[window_rules]]
match = { app_id_regex = \"^mpv$\" }
actions = { output = \"DP-1\", layout = \"tiled-right\" }
";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        let r = &cfg.window_rules[0];
        assert_eq!(r.matcher.app_id_regex.as_deref(), Some("^mpv$"));
        assert_eq!(r.actions.output.as_deref(), Some("DP-1"));
        assert_eq!(r.actions.layout, Some(RuleLayout::TiledRight));
    }

    #[test]
    fn window_rule_regex_errors_reports_bad_patterns() {
        let toml_src = "\
[[window_rules]]
match = { app_id_regex = \"(unclosed\" }
actions = { sticky = true }

[[window_rules]]
match = { title_regex = \"valid\" }
actions = {}
";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        let errors = cfg.window_rule_regex_errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("window_rules[0].match.app_id_regex"));
    }

    #[test]
    fn default_config_toml_round_trips() {
        let dumped = default_config_toml();
        let parsed: Config = toml::from_str(&dumped).expect("dumped default must parse");
        let original = Config::with_default_bindings();
        assert_eq!(parsed.bindings.len(), original.bindings.len());
        assert_eq!(parsed.general.output_scale, original.general.output_scale);
        assert_eq!(parsed.general.focus_mode, original.general.focus_mode);
    }

    #[test]
    fn autostart_defaults_include_bar() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(
            cfg.general.autostart,
            vec![
                "shoestring-bar".to_string(),
                "shoestring-mediad".to_string()
            ]
        );
    }

    #[test]
    fn autostart_user_override_replaces_default() {
        let cfg: Config =
            toml::from_str("[general]\nautostart = [\"foo\", \"bar --baz\"]\n").unwrap();
        assert_eq!(cfg.general.autostart, vec!["foo", "bar --baz"]);
    }

    #[test]
    fn automation_defaults_off() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.general.automation_enabled);
    }

    #[test]
    fn automation_enabled_user_override() {
        let cfg: Config = toml::from_str("[general]\nautomation_enabled = true\n").unwrap();
        assert!(cfg.general.automation_enabled);
    }

    #[test]
    fn screen_capture_defaults_off() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.general.screen_capture_enabled);
    }

    #[test]
    fn screen_capture_enabled_user_override() {
        let cfg: Config = toml::from_str("[general]\nscreen_capture_enabled = true\n").unwrap();
        assert!(cfg.general.screen_capture_enabled);
    }

    #[test]
    fn portal_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.portal.screencast_output.is_none());
        assert_eq!(cfg.portal.screencast_chooser, "region");
    }

    #[test]
    fn portal_user_override() {
        let cfg: Config = toml::from_str(
            "[portal]\nscreencast_output = \"DP-2\"\nscreencast_chooser = \"none\"\n",
        )
        .unwrap();
        assert_eq!(cfg.portal.screencast_output.as_deref(), Some("DP-2"));
        assert_eq!(cfg.portal.screencast_chooser, "none");
    }

    #[test]
    fn diagnostics_defaults_on() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.diagnostics.enabled);
        assert_eq!(cfg.diagnostics.sample_interval_ms, 1000);
        assert_eq!(cfg.diagnostics.fd_warn_fraction, 0.75);
    }

    #[test]
    fn diagnostics_user_override() {
        let cfg: Config = toml::from_str(
            "[diagnostics]\nenabled = false\nsample_interval_ms = 5000\nfd_warn_fraction = 0.9\n",
        )
        .unwrap();
        assert!(!cfg.diagnostics.enabled);
        assert_eq!(cfg.diagnostics.sample_interval_ms, 5000);
        assert_eq!(cfg.diagnostics.fd_warn_fraction, 0.9);
    }

    #[test]
    fn debug_defaults_all_off() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.debug, Debug::default());
        assert!(!cfg.debug.disable_direct_scanout);
        assert!(!cfg.debug.disable_overlay_planes);
        assert!(!cfg.debug.disable_cursor_plane);
        assert!(!cfg.debug.protocol_trace);
    }

    #[test]
    fn debug_user_override() {
        let cfg: Config =
            toml::from_str("[debug]\ndisable_direct_scanout = true\ndisable_cursor_plane = true\n")
                .unwrap();
        assert!(cfg.debug.disable_direct_scanout);
        assert!(!cfg.debug.disable_overlay_planes);
        assert!(cfg.debug.disable_cursor_plane);
    }

    #[test]
    fn debug_protocol_trace_override() {
        let cfg: Config = toml::from_str("[debug]\nprotocol_trace = true\n").unwrap();
        assert!(cfg.debug.protocol_trace);
        // Independent of the render-path flags.
        assert!(!cfg.debug.disable_direct_scanout);
    }

    #[test]
    fn debug_rejects_unknown_key() {
        // deny_unknown_fields guards against typo'd debug knobs silently no-op'ing.
        let err = toml::from_str::<Config>("[debug]\ndisable_scanout = true\n");
        assert!(err.is_err());
    }

    #[test]
    fn decorations_default_off() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.decorations.border_width, 0);
        // Defaults still parse to valid colors even though borders are off.
        assert!(cfg.decorations.color_errors().is_empty());
    }

    #[test]
    fn decorations_user_override() {
        let cfg: Config = toml::from_str(
            "[decorations]\nborder_width = 2\nfocused_color = \"#ff0000\"\nunfocused_color = \"#00ff0080\"\n",
        )
        .unwrap();
        assert_eq!(cfg.decorations.border_width, 2);
        assert_eq!(cfg.decorations.focused_rgba(), [1.0, 0.0, 0.0, 1.0]);
        let [r, g, b, a] = cfg.decorations.unfocused_rgba();
        assert_eq!([r, g, b], [0.0, 1.0, 0.0]);
        assert!((a - 0x80 as f32 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn decorations_bad_color_falls_back_and_reports() {
        let cfg: Config =
            toml::from_str("[decorations]\nfocused_color = \"not-a-color\"\n").unwrap();
        // Falls back to the default focused color rather than panicking.
        assert_eq!(
            cfg.decorations.focused_rgba(),
            parse_hex_rgba("#5e81ac").unwrap()
        );
        assert_eq!(
            cfg.decorations.color_errors(),
            vec!["decorations.focused_color"]
        );
    }

    #[test]
    fn background_default_solid_color() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.background.color, "#1a1a1a");
        assert_eq!(cfg.background.mode, BackgroundMode::Fill);
        assert!(cfg.background.image.is_none());
        assert!(cfg.background.image_path().is_none());
        assert!(cfg.background.color_errors().is_empty());
        // ~0.1 like the historic hardcoded clear.
        let [r, g, b, a] = cfg.background.color_rgba();
        assert!((r - 26.0 / 255.0).abs() < 1e-6);
        assert_eq!([g, b, a], [r, r, 1.0]);
    }

    #[test]
    fn background_user_override() {
        let cfg: Config = toml::from_str(
            "[background]\ncolor = \"#102030\"\nimage = \"/usr/share/wp.png\"\nmode = \"center\"\n",
        )
        .unwrap();
        assert_eq!(cfg.background.mode, BackgroundMode::Center);
        assert_eq!(
            cfg.background.image_path().unwrap(),
            PathBuf::from("/usr/share/wp.png")
        );
        let [r, g, b, a] = cfg.background.color_rgba();
        assert_eq!(
            [r, g, b, a],
            [
                0x10 as f32 / 255.0,
                0x20 as f32 / 255.0,
                0x30 as f32 / 255.0,
                1.0
            ]
        );
    }

    #[test]
    fn background_all_modes_parse() {
        for (s, want) in [
            ("fill", BackgroundMode::Fill),
            ("fit", BackgroundMode::Fit),
            ("center", BackgroundMode::Center),
            ("stretch", BackgroundMode::Stretch),
            ("tile", BackgroundMode::Tile),
        ] {
            let cfg: Config = toml::from_str(&format!("[background]\nmode = \"{s}\"\n")).unwrap();
            assert_eq!(cfg.background.mode, want, "mode {s}");
        }
    }

    #[test]
    fn background_bad_color_falls_back_and_reports() {
        let cfg: Config = toml::from_str("[background]\ncolor = \"nope\"\n").unwrap();
        assert_eq!(
            cfg.background.color_rgba(),
            parse_hex_rgba("#1a1a1a").unwrap()
        );
        assert_eq!(cfg.background.color_errors(), vec!["background.color"]);
    }

    #[test]
    fn background_path_expands_home_and_env() {
        std::env::set_var("HOME", "/home/tester");
        std::env::set_var("WP_DIR", "/data/walls");
        let cfg: Config = toml::from_str("[background]\nimage = \"~/pics/bg.png\"\n").unwrap();
        assert_eq!(
            cfg.background.image_path().unwrap(),
            PathBuf::from("/home/tester/pics/bg.png")
        );
        let cfg: Config = toml::from_str("[background]\nimage = \"${WP_DIR}/a.svg\"\n").unwrap();
        assert_eq!(
            cfg.background.image_path().unwrap(),
            PathBuf::from("/data/walls/a.svg")
        );
        let cfg: Config = toml::from_str("[background]\nimage = \"$WP_DIR/b.png\"\n").unwrap();
        assert_eq!(
            cfg.background.image_path().unwrap(),
            PathBuf::from("/data/walls/b.png")
        );
    }

    #[test]
    fn parse_hex_rgba_forms() {
        assert_eq!(parse_hex_rgba("#000000"), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_hex_rgba("ffffff"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse_hex_rgba("#ffffff00"), Some([1.0, 1.0, 1.0, 0.0]));
        assert_eq!(parse_hex_rgba("#fff"), None); // 3-digit shorthand unsupported
        assert_eq!(parse_hex_rgba("#gggggg"), None); // non-hex
        assert_eq!(parse_hex_rgba(""), None);
    }

    #[test]
    fn input_defaults_all_unset() {
        let cfg: Config = toml::from_str("").unwrap();
        let i = cfg.input;
        assert!(i.tap_to_click.is_none());
        assert!(i.natural_scroll.is_none());
        assert!(i.accel_speed.is_none());
        assert!(i.accel_profile.is_none());
        assert!(i.scroll_method.is_none());
        assert!(i.click_method.is_none());
        assert!(i.tap_button_map.is_none());
    }

    #[test]
    fn input_parses_all_fields() {
        let toml_src = "\
[input]
tap_to_click = true
tap_button_map = \"left-right-middle\"
tap_and_drag = true
drag_lock = false
natural_scroll = true
scroll_method = \"two-finger\"
scroll_button = 274
click_method = \"clickfinger\"
accel_speed = 0.3
accel_profile = \"flat\"
disable_while_typing = true
left_handed = false
middle_emulation = true
";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        let i = cfg.input;
        assert_eq!(i.tap_to_click, Some(true));
        assert_eq!(i.tap_button_map, Some(TapButtonMap::LeftRightMiddle));
        assert_eq!(i.tap_and_drag, Some(true));
        assert_eq!(i.drag_lock, Some(false));
        assert_eq!(i.natural_scroll, Some(true));
        assert_eq!(i.scroll_method, Some(ScrollMethod::TwoFinger));
        assert_eq!(i.scroll_button, Some(274));
        assert_eq!(i.click_method, Some(ClickMethod::Clickfinger));
        assert_eq!(i.accel_speed, Some(0.3));
        assert_eq!(i.accel_profile, Some(AccelProfile::Flat));
        assert_eq!(i.disable_while_typing, Some(true));
        assert_eq!(i.left_handed, Some(false));
        assert_eq!(i.middle_emulation, Some(true));
    }

    #[test]
    fn input_enums_use_kebab_case() {
        let cfg: Config =
            toml::from_str("[input]\nscroll_method = \"on-button-down\"\naccel_profile = \"adaptive\"\ntap_button_map = \"left-middle-right\"\n").unwrap();
        assert_eq!(cfg.input.scroll_method, Some(ScrollMethod::OnButtonDown));
        assert_eq!(cfg.input.accel_profile, Some(AccelProfile::Adaptive));
        assert_eq!(
            cfg.input.tap_button_map,
            Some(TapButtonMap::LeftMiddleRight)
        );
    }

    #[test]
    fn input_unknown_field_rejected() {
        let err = toml::from_str::<Config>("[input]\nfast_pointer = true\n").unwrap_err();
        assert!(
            err.to_string().contains("fast_pointer") || err.to_string().contains("unknown field"),
            "error should point at the bad key: {err}"
        );
    }

    #[test]
    fn input_unknown_enum_value_rejected() {
        assert!(toml::from_str::<Config>("[input]\naccel_profile = \"turbo\"\n").is_err());
    }

    #[test]
    fn touch_defaults_to_no_mapping() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.touch, Touch::default());
        assert!(cfg.touch.map_to_output.is_none());
    }

    #[test]
    fn touch_map_to_output_parses() {
        let cfg: Config = toml::from_str("[touch]\nmap_to_output = \"eDP-1\"\n").unwrap();
        assert_eq!(cfg.touch.map_to_output.as_deref(), Some("eDP-1"));
    }

    #[test]
    fn touch_rejects_unknown_key() {
        assert!(toml::from_str::<Config>("[touch]\nmap_to_region = \"0,0 1x1\"\n").is_err());
    }

    #[test]
    fn xkb_defaults_are_empty() {
        let g = Config::default().general;
        assert!(g.xkb_layout.is_empty());
        assert!(g.xkb_variant.is_empty());
        assert!(g.xkb_options.is_none());
        assert!(g.xkb_rules.is_empty());
        assert!(g.xkb_model.is_empty());
    }

    #[test]
    fn xkb_config_parses() {
        let cfg: Config = toml::from_str(
            "[general]\nxkb_layout = \"us,de\"\nxkb_variant = \",nodeadkeys\"\nxkb_options = \"grp:alt_shift_toggle,ctrl:nocaps\"\nxkb_model = \"pc105\"\n",
        )
        .unwrap();
        assert_eq!(cfg.general.xkb_layout, "us,de");
        assert_eq!(cfg.general.xkb_variant, ",nodeadkeys");
        assert_eq!(
            cfg.general.xkb_options.as_deref(),
            Some("grp:alt_shift_toggle,ctrl:nocaps")
        );
        assert_eq!(cfg.general.xkb_model, "pc105");
    }

    #[test]
    fn cycle_layout_action_parses() {
        let b: Binding = toml::from_str(
            "mods = [\"Super\"]\nkey = \"space\"\naction = { type = \"cycle-layout\" }\n",
        )
        .unwrap();
        assert!(matches!(b.action, Action::CycleLayout));
    }

    #[test]
    fn default_bindings_include_cycle_layout() {
        let cfg = Config::with_default_bindings();
        assert!(cfg.bindings.iter().any(|b| {
            matches!(b.action, Action::CycleLayout)
                && b.key == "space"
                && b.mods.iter().any(|m| m == "Super")
        }));
    }

    #[test]
    fn default_bindings_include_raise_and_lower() {
        let cfg = Config::with_default_bindings();
        let has = |action_matches: fn(&Action) -> bool, mods: &[&str]| {
            cfg.bindings.iter().any(|b| {
                action_matches(&b.action)
                    && b.key == "Up"
                    && mods.iter().all(|m| b.mods.iter().any(|bm| bm == m))
                    && b.mods.len() == mods.len()
            })
        };
        assert!(has(|a| matches!(a, Action::Raise), &["Super"]));
        assert!(has(|a| matches!(a, Action::Lower), &["Super", "Shift"]));
    }

    #[test]
    fn default_bindings_include_toggle_sticky() {
        let cfg = Config::with_default_bindings();
        assert!(cfg.bindings.iter().any(|b| {
            matches!(b.action, Action::ToggleSticky) && b.key == "s" && b.mods == ["Super"]
        }));
    }

    #[test]
    fn window_rule_sticky_action_parses() {
        let toml_src = "\
[[window_rules]]
match = { app_id = \"mpv\" }
actions = { sticky = true }
";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.window_rules.len(), 1);
        assert_eq!(cfg.window_rules[0].actions.sticky, Some(true));
    }

    #[test]
    fn default_bindings_include_toggle_always_on_top() {
        let cfg = Config::with_default_bindings();
        assert!(cfg.bindings.iter().any(|b| {
            matches!(b.action, Action::ToggleAlwaysOnTop) && b.key == "a" && b.mods == ["Super"]
        }));
    }

    #[test]
    fn default_bindings_include_arrange_cluster() {
        let cfg = Config::with_default_bindings();
        let has = |pred: &dyn Fn(&Binding) -> bool| cfg.bindings.iter().any(pred);
        assert!(has(&|b| {
            matches!(b.action, Action::ArrangeGrid) && b.key == "g" && b.mods == ["Super"]
        }));
        assert!(has(&|b| {
            matches!(b.action, Action::ArrangeSpiral)
                && b.key == "g"
                && b.mods == ["Super", "Shift"]
        }));
        assert!(has(&|b| {
            matches!(b.action, Action::ArrangeBsp) && b.key == "g" && b.mods == ["Super", "Ctrl"]
        }));
    }

    #[test]
    fn arrange_actions_use_kebab_case_wire_form() {
        // The IPC dispatch_action path (serde_json) and config (toml) both rely
        // on these kebab-case tags; parse each from a binding to lock them in.
        let parse = |ty: &str| -> Action {
            toml::from_str::<Binding>(&format!(
                "mods = [\"Super\"]\nkey = \"g\"\naction = {{ type = \"{ty}\" }}\n"
            ))
            .unwrap()
            .action
        };
        assert!(matches!(parse("arrange-grid"), Action::ArrangeGrid));
        assert!(matches!(parse("arrange-spiral"), Action::ArrangeSpiral));
        assert!(matches!(parse("arrange-bsp"), Action::ArrangeBsp));
    }

    #[test]
    fn window_rule_always_on_top_action_parses() {
        let toml_src = "\
[[window_rules]]
match = { app_id = \"mpv\" }
actions = { sticky = true, always_on_top = true }
";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.window_rules.len(), 1);
        assert_eq!(cfg.window_rules[0].actions.always_on_top, Some(true));
    }

    #[test]
    fn output_scale_parses_fractional() {
        let cfg: Config = toml::from_str("[general]\noutput_scale = 1.5\n").unwrap();
        assert_eq!(cfg.general.output_scale, 1.5);
    }

    #[test]
    fn per_output_scale_overrides_general() {
        let toml_src = "[general]\noutput_scale = 1.0\n\n[outputs.DP-1]\nscale = 2.0\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.outputs.get("DP-1").and_then(|o| o.scale), Some(2.0));
        assert_eq!(cfg.outputs.get("HDMI-A-1").and_then(|o| o.scale), None);
        assert_eq!(cfg.general.output_scale, 1.0);
    }

    #[test]
    fn per_output_scale_absent_gives_empty_map() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.outputs.is_empty());
    }

    #[test]
    fn per_output_position_parses() {
        let toml_src = "[outputs.DP-1]\nposition = [1920, 0]\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(
            cfg.outputs.get("DP-1").and_then(|o| o.position),
            Some([1920, 0])
        );
    }

    #[test]
    fn per_output_scale_and_position_together() {
        let toml_src = "[outputs.eDP-1]\nscale = 2.0\nposition = [0, 0]\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        let oc = cfg.outputs.get("eDP-1").unwrap();
        assert_eq!(oc.scale, Some(2.0));
        assert_eq!(oc.position, Some([0, 0]));
    }

    #[test]
    fn per_output_transform_parses_rotations() {
        let toml_src =
            "[outputs.DP-1]\ntransform = \"90\"\n\n[outputs.DP-2]\ntransform = \"flipped-270\"\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(
            cfg.outputs.get("DP-1").and_then(|o| o.transform),
            Some(OutputTransform::_90)
        );
        assert_eq!(
            cfg.outputs.get("DP-2").and_then(|o| o.transform),
            Some(OutputTransform::Flipped270)
        );
    }

    #[test]
    fn per_output_transform_absent_is_none() {
        let toml_src = "[outputs.DP-1]\nscale = 1.0\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.outputs.get("DP-1").and_then(|o| o.transform), None);
    }

    #[test]
    fn per_output_transform_rejects_unknown() {
        let toml_src = "[outputs.DP-1]\ntransform = \"sideways\"\n";
        assert!(toml::from_str::<Config>(toml_src).is_err());
    }

    #[test]
    fn per_output_transform_round_trips() {
        let toml_src = "[outputs.DP-1]\ntransform = \"180\"\n";
        let cfg: Config = toml::from_str(toml_src).unwrap();
        let dumped = toml::to_string(&cfg).unwrap();
        assert!(
            dumped.contains("transform = \"180\""),
            "transform must serialize back to its wlr-randr name, got:\n{dumped}"
        );
    }

    #[test]
    fn default_config_toml_no_outputs_section() {
        let dumped = default_config_toml();
        assert!(
            !dumped.contains("[outputs]"),
            "empty outputs must not appear in default config"
        );
    }

    #[test]
    fn workspaces_defaults_to_sixteen_unnamed() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.workspaces.count, 16);
        assert!(cfg.workspaces.names.is_empty());
    }

    #[test]
    fn workspaces_parses_count_and_sparse_names() {
        let toml_src = r#"
[workspaces]
count = 8

[workspaces.names]
1 = "web"
3 = "chat"
"#;
        let cfg: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.workspaces.count, 8);
        assert_eq!(
            cfg.workspaces.names.get("1").map(String::as_str),
            Some("web")
        );
        assert_eq!(
            cfg.workspaces.names.get("3").map(String::as_str),
            Some("chat")
        );
        assert!(!cfg.workspaces.names.contains_key("2"));
    }
}
