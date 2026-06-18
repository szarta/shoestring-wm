//! Backend state: the bus connection, the PipeWire connection, and the table
//! of live screencast sessions. One [`Session`] per
//! `org.freedesktop.impl.portal.Session` object the frontend asks us to
//! manage, keyed by its object path; the matching [`Cast`] (PipeWire stream)
//! lives in `casts` under the same key.

use std::collections::HashMap;

use rustbus::RpcConn;

use crate::cast::{Cast, Pw};

/// Whole-process state threaded through the dispatch loop.
pub struct State {
    /// Session-bus RPC connection (we own
    /// `org.freedesktop.impl.portal.desktop.shoestring`).
    pub rpc: RpcConn,
    /// Cleared to break the poll loop on SIGINT/SIGTERM.
    pub running: bool,
    /// Live sessions keyed by their D-Bus object path (the `session_handle`
    /// the frontend passes to every call).
    pub sessions: HashMap<String, Session>,
    /// Active PipeWire streams, keyed by the same session object path.
    pub casts: HashMap<String, Cast>,
    /// Shared PipeWire daemon connection.
    pub pw: Pw,
}

impl State {
    pub fn new(rpc: RpcConn, pw: Pw) -> Self {
        Self {
            rpc,
            running: true,
            sessions: HashMap::new(),
            casts: HashMap::new(),
            pw,
        }
    }
}

/// Per-session capture parameters, accumulated across
/// CreateSession → SelectSources → Start.
#[derive(Debug, Default)]
pub struct Session {
    /// `app_id` the portal frontend reported (informational / logging).
    pub app_id: String,
    /// Source-type bitmask the app requested in SelectSources
    /// (1=MONITOR, 2=WINDOW, 4=VIRTUAL). We only serve MONITOR.
    pub source_types: u32,
    /// Cursor mode the app requested (1=HIDDEN, 2=EMBEDDED, 4=METADATA).
    /// Our screencopy always composites the cursor, i.e. EMBEDDED.
    pub cursor_mode: u32,
    /// Whether the app asked to pick multiple sources. We share one output.
    pub multiple: bool,
}
