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
    /// (Substring rather than regex to stay zero-dep; regex can be
    /// added later behind a `regex` feature if a real user hits a case
    /// substring can't express.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
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
    /// Default: `["shoestring-bar"]` so a fresh user gets the status bar.
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
    vec!["shoestring-bar".into()]
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
    /// Re-read the config file from disk.
    ReloadConfig,
    /// Snap the focused window to the left half of its monitor's usable rect.
    /// Toggles back to the saved floating rect when re-pressed.
    TileLeft,
    /// Snap the focused window to the right half of its monitor.
    TileRight,
    /// Maximize the focused window to its monitor's usable rect (toggle).
    Maximize,
    /// Hide the focused window without destroying it. Restore via `unminimize`.
    Minimize,
    /// Restore the most-recently-minimized window.
    Unminimize,
    /// Ask the focused window's client to close gracefully.
    Close,
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
# minimize, unminimize, close, focus-workspace, focus-workspace-relative,
# move-window-to-workspace, move-window-to-workspace-relative, change-vt,
# inject-key, inject-text, inject-click, lock.
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
            // Lock screen. Spawns `general.lock_command` (default
            // `shoestring-lock`) which binds ext-session-lock-v1.
            // Super+L is already workspace-next; pair with Shift.
            Binding {
                mods: super_shift(),
                key: "l".into(),
                action: Action::Lock,
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
        true
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
            title_contains: None,
        };
        assert!(only_app.matches("firefox", "anything"));
        assert!(!only_app.matches("chromium", "Firefox-like"));

        let app_and_title = WindowMatch {
            app_id: Some("term".into()),
            title_contains: Some("vim".into()),
        };
        assert!(app_and_title.matches("term", "nvim buffer"));
        assert!(!app_and_title.matches("term", "fish shell"));
        assert!(!app_and_title.matches("other", "vim"));

        // Empty matcher = wildcard.
        let empty = WindowMatch::default();
        assert!(empty.matches("anything", "anything"));
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
        assert_eq!(cfg.general.autostart, vec!["shoestring-bar".to_string()]);
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
