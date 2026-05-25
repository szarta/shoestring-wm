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
    /// Synthesize a single keypress (press + release) targeting whichever
    /// surface currently holds keyboard focus. `keysym` is an X keysym
    /// name as understood by `xkb_keysym_from_name` (e.g. `"Return"`,
    /// `"F5"`, `"q"`, `"BackSpace"`).
    InjectKey { keysym: String },
    /// Synthesize a sequence of keypresses that types `text`. v1 supports
    /// ASCII letters, digits, and space; other codepoints fall back to a
    /// server-side error so the caller knows to break the input up.
    InjectText { text: String },
    /// Synthesize a single mouse click. `button` is one of `"left"`,
    /// `"right"`, `"middle"`, or a numeric Linux `BTN_*` code as a string
    /// (`"272"` etc). Optional `x` / `y` move the pointer to the given
    /// compositor-space coordinates first (both must be present together).
    InjectClick {
        button: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
    },
    /// Lock the session. Spawns the WM's configured lock binary
    /// (`general.lock_command`); the binary itself drives the
    /// `ext-session-lock-v1` handshake. Returns immediately with
    /// [`Response::Ok`] — no wait for the lock to confirm.
    Lock,
    /// Toggle the runtime automation gate (see
    /// `general.automation_enabled`). Reply is [`Response::Automation`]
    /// with the new state; an [`Event::AutomationChanged`] is broadcast
    /// to subscribers when the value actually flips. Not persisted to
    /// disk — the config file is the source of truth at next start.
    SetAutomation { enabled: bool },
    /// Read the current automation gate state without changing it. Reply
    /// is [`Response::Automation`].
    AutomationStatus,
    /// Capture a PNG screenshot via the WM's wlr-screencopy server. The
    /// WM spawns `shoestring-screenshot` on the user's behalf and replies
    /// with the resulting [`Response::Screenshot`] once the file is
    /// written.
    ///
    /// - `output: None` → first advertised output (full-screen).
    /// - `output: Some(name)` → capture that output. Required when
    ///   `region` is set, since region coordinates are output-relative.
    /// - `region: Some(...)` → capture only that rectangle in the named
    ///   output's logical coords.
    ///
    /// Gated by `set_automation`: returns [`Response::Error`] when the
    /// runtime automation gate is off.
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<ScreenshotRegion>,
    },
}

/// Rectangle for [`Request::Screenshot`], in the target output's logical
/// pixel coords. All fields are positive; `w` and `h` must be > 0.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRegion {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
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
    /// Current state of the automation gate. Returned for both
    /// [`Request::SetAutomation`] and [`Request::AutomationStatus`].
    Automation {
        enabled: bool,
    },
    /// Path of the PNG written by [`Request::Screenshot`]. Absolute,
    /// usually under `$XDG_PICTURES_DIR`.
    Screenshot {
        path: String,
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
    /// Fired when the runtime automation gate flips. Subscribers can use
    /// this to surface a status indicator (e.g. bar widget) without
    /// polling.
    AutomationChanged {
        enabled: bool,
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
    fn inject_request_shapes() {
        let key = Request::InjectKey {
            keysym: "Return".into(),
        };
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            r#"{"type":"inject_key","keysym":"Return"}"#
        );
        let typ = Request::InjectText { text: "hi".into() };
        assert_eq!(
            serde_json::to_string(&typ).unwrap(),
            r#"{"type":"inject_text","text":"hi"}"#
        );
        let click_no_xy = Request::InjectClick {
            button: "left".into(),
            x: None,
            y: None,
        };
        // x/y are skipped when None so simple cases stay terse.
        assert_eq!(
            serde_json::to_string(&click_no_xy).unwrap(),
            r#"{"type":"inject_click","button":"left"}"#
        );
        let click_xy = Request::InjectClick {
            button: "right".into(),
            x: Some(100.5),
            y: Some(200.0),
        };
        let s = serde_json::to_string(&click_xy).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::InjectClick {
                ref button,
                x: Some(_),
                y: Some(_)
            } if button == "right"
        ));
    }

    #[test]
    fn automation_request_response_event_shapes() {
        let set = Request::SetAutomation { enabled: true };
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"type":"set_automation","enabled":true}"#
        );
        let status = Request::AutomationStatus;
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"type":"automation_status"}"#
        );
        let resp = Response::Automation { enabled: false };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"automation","enabled":false}"#
        );
        let ev = Event::AutomationChanged { enabled: true };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"automation_changed","enabled":true}"#
        );
    }

    #[test]
    fn screenshot_request_shapes() {
        let bare = Request::Screenshot {
            output: None,
            region: None,
        };
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"type":"screenshot"}"#
        );
        let with_region = Request::Screenshot {
            output: Some("eDP-1".into()),
            region: Some(ScreenshotRegion {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            }),
        };
        let s = serde_json::to_string(&with_region).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            Request::Screenshot {
                output: Some(ref n),
                region: Some(ScreenshotRegion {
                    x: 10, y: 20, w: 800, h: 600,
                }),
            } if n == "eDP-1"
        ));
        let resp = Response::Screenshot {
            path: "/tmp/foo.png".into(),
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"type":"screenshot","path":"/tmp/foo.png"}"#
        );
    }

    #[test]
    fn event_window_focused_none() {
        let e = Event::WindowFocused { id: None };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(s, r#"{"type":"window_focused","id":null}"#);
    }
}
