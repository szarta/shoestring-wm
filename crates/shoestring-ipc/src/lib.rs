//! Wire types for the shoestring-wm IPC protocol.
//!
//! Types-only crate: depends on serde + serde_json so a future companion bar
//! can link them without pulling in Smithay or a runtime.
//!
//! ## Protocol
//!
//! Newline-delimited JSON over a `SOCK_STREAM` unix socket. The socket path
//! defaults to `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`; the WM
//! exports it as `$SHOESTRING_WM_SOCKET` so children can find it without
//! knowing the formula.
//!
//! A client opens the socket, sends exactly one [`Request`] (one JSON
//! object terminated by `\n`), then reads the [`Response`]. For
//! [`Request::EventStream`], the server keeps the connection open and
//! pushes [`Event`]s forever (one JSON line per event) until the client
//! disconnects.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Environment variable the WM exports so children (notably `shoestring-ctl`
/// and the bar) can find the socket without re-deriving the path formula.
pub const SOCKET_ENV: &str = "SHOESTRING_WM_SOCKET";

/// Resolve the conventional socket path:
/// `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`. Returns `None`
/// if either env var is unset — callers should prefer `$SHOESTRING_WM_SOCKET`
/// when present.
pub fn default_socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")?;
    let display = display.to_string_lossy();
    Some(PathBuf::from(runtime).join(format!("shoestring-wm-{display}.sock")))
}

/// Resolve the socket path for a *client*: prefer `$SHOESTRING_WM_SOCKET`,
/// fall back to [`default_socket_path`].
pub fn client_socket_path() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV)
        .map(PathBuf::from)
        .or_else(default_socket_path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// List workspaces and which one is currently active.
    Workspaces,
    /// List every mapped window (across all workspaces).
    Windows,
    /// List every connected output.
    Outputs,
    /// Switch the connection into streaming mode: server sends one
    /// [`Response::Ok`] then a stream of [`Event`]s, one per line, forever.
    EventStream,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Generic success acknowledgement (no payload). Sent in response to
    /// [`Request::EventStream`] before the event stream starts.
    Ok,
    Workspaces {
        /// 1-based index of the active workspace.
        active: u8,
        /// Total workspace count (always 16 in current builds).
        count: u8,
    },
    Windows {
        windows: Vec<WindowSummary>,
    },
    Outputs {
        outputs: Vec<OutputSummary>,
    },
    /// Server-side error; the client should print and exit non-zero.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSummary {
    /// Stable identifier matching `ext-foreign-toplevel-list-v1`'s
    /// `identifier` event so a bar can cross-reference.
    pub id: String,
    pub title: String,
    pub app_id: String,
    /// 1-based workspace.
    pub workspace: u8,
    /// `true` for the currently keyboard-focused window.
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Logical scale; matches what's advertised on `wl_output.scale` /
    /// `wp_fractional_scale_v1`.
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    WorkspaceChanged {
        /// 1-based.
        active: u8,
    },
    WindowOpened {
        id: String,
        title: String,
        app_id: String,
        workspace: u8,
    },
    WindowClosed {
        id: String,
    },
    /// `id` is `None` when no window holds keyboard focus.
    WindowFocused {
        id: Option<String>,
    },
    /// Window's title or app_id changed.
    WindowTitleChanged {
        id: String,
        title: String,
        app_id: String,
    },
    OutputAdded(OutputSummary),
    OutputRemoved {
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let r = Request::EventStream;
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"type":"event_stream"}"#);
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::EventStream));
    }

    #[test]
    fn response_workspaces_shape() {
        let r = Response::Workspaces {
            active: 3,
            count: 16,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"type":"workspaces","active":3,"count":16}"#);
    }

    #[test]
    fn event_window_focused_none() {
        let e = Event::WindowFocused { id: None };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"type":"window_focused","id":null}"#);
    }
}
