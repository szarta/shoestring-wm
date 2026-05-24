//! Config types and TOML parser for shoestring-wm.
//!
//! Keysyms and modifier names are stored as plain strings here so this crate
//! does not depend on xkbcommon — the WM resolves them into a [`BindingTable`]
//! at startup.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// Top-level config. Sections are all optional; missing sections take defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Clone, Deserialize)]
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
}

fn default_repeat_delay() -> i32 {
    600
}
fn default_repeat_rate() -> i32 {
    25
}

impl Default for General {
    fn default() -> Self {
        Self {
            focus_mode: FocusMode::default(),
            repeat_delay: default_repeat_delay(),
            repeat_rate: default_repeat_rate(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FocusMode {
    #[default]
    ClickToFocus,
    FollowsMouse,
    Sloppy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Modifier names; case-insensitive. Recognized: Super, Ctrl, Alt, Shift.
    #[serde(default)]
    pub mods: Vec<String>,
    /// xkb keysym name, e.g. "Return", "q", "1".
    pub key: String,
    pub action: Action,
}

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
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
        Self {
            general: General::default(),
            bindings,
        }
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
}
