//! IPC server: newline-delimited JSON over a unix socket. Wire types live
//! in [`shoestring_ipc`]; this file is just the WM-side plumbing.
//!
//! ## Topology
//!
//! - One [`UnixListener`] bound at `$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock`
//!   (or `$SHOESTRING_WM_SOCKET` if set). Registered as a calloop `Generic`
//!   source with `READ` interest; the handler calls `accept()` until it
//!   would block, then registers each accepted stream as its own `Generic`
//!   source so we never block the WM main loop on a single client.
//! - One [`Client`] per accepted connection. We hold a `Rc<RefCell<Client>>`
//!   on `ShoestringWm::ipc.clients` keyed by an opaque [`ClientId`]; the
//!   calloop source for the stream owns the same `Rc`. Read events parse
//!   one JSON line per call, dispatch a [`Request`], and either write a
//!   single [`Response`] (closing after) or flip the client into event-
//!   streaming mode.
//! - Events: [`Server::emit`] walks `clients` and writes one JSON line to
//!   every subscriber. Writes are non-blocking; on `WouldBlock` (or any
//!   I/O error) we drop the subscriber rather than buffering — IPC traffic
//!   is small (workspace changes, focus changes), so backpressure means
//!   "client is broken," not "we need a real queue."
//!
//! Stream-mode clients are also re-armed for `READ` to detect hangup
//! (`read()` → 0); we don't expect more requests on a streaming connection.

use std::{
    cell::RefCell,
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    rc::Rc,
};

use anyhow::{Context, Result};
use shoestring_ipc::{
    default_socket_path, Event, OutputSummary, Request, Response, WindowSummary, SOCKET_ENV,
};
use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};

use crate::state::ShoestringWm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(u64);

pub struct Server {
    pub socket_path: PathBuf,
    /// Held for the lifetime of the server so the listener source is kept
    /// alive; calloop drops it with the EventLoop on shutdown. Removing it
    /// manually would require an extra LoopHandle parameter on `Drop`.
    #[allow(dead_code)]
    listener_token: RegistrationToken,
    clients: HashMap<ClientId, ClientEntry>,
    next_id: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

struct ClientEntry {
    client: Rc<RefCell<Client>>,
    token: RegistrationToken,
}

pub(crate) struct Client {
    stream: UnixStream,
    /// Buffered partial line(s) we haven't seen a `\n` for yet.
    read_buf: Vec<u8>,
    /// `true` after the client sent [`Request::EventStream`]; events get
    /// pushed at it on every state change.
    subscriber: bool,
    /// Once we've handled the (single) one-shot request on this connection,
    /// further input is unexpected — set this and close on next read.
    spent: bool,
}

impl ShoestringWm {
    /// Start the IPC server. Failure is non-fatal: we log and continue
    /// without IPC, so a misconfigured `$XDG_RUNTIME_DIR` doesn't take
    /// down the compositor.
    pub fn start_ipc(&mut self) {
        match Server::bind(self.loop_handle.clone()) {
            Ok(server) => {
                tracing::info!(path = %server.socket_path.display(), "ipc listening");
                // Children get the socket via env var.
                std::env::set_var(SOCKET_ENV, &server.socket_path);
                self.ipc = Some(server);
            }
            Err(e) => tracing::warn!(error = %e, "ipc disabled"),
        }
    }
}

impl Server {
    fn bind(loop_handle: LoopHandle<'static, ShoestringWm>) -> Result<Self> {
        let socket_path = std::env::var_os(SOCKET_ENV)
            .map(PathBuf::from)
            .or_else(default_socket_path)
            .context("neither $SHOESTRING_WM_SOCKET nor $XDG_RUNTIME_DIR+$WAYLAND_DISPLAY set")?;

        // Stale socket from a previous (crashed) run would make bind() fail
        // with EADDRINUSE. Remove it unconditionally; if a *live* WM owns it
        // the subsequent bind will succeed and the old WM's listener was
        // already detached when its process exited.
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        listener.set_nonblocking(true)?;

        let source = Generic::new(listener, Interest::READ, Mode::Level);
        let listener_token = loop_handle
            .insert_source(source, move |_, listener, state| {
                // Drain every pending accept; the level-triggered source
                // would otherwise fire again immediately.
                loop {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            if let Err(e) = accept_client(state, stream) {
                                tracing::warn!(error = %e, "ipc accept follow-up failed");
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!(error = %e, "ipc accept failed");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            })
            .map_err(|e| anyhow::anyhow!("insert ipc listener source: {e}"))?;

        Ok(Self {
            socket_path,
            listener_token,
            clients: HashMap::new(),
            next_id: 0,
        })
    }

    fn next_id(&mut self) -> ClientId {
        let id = self.next_id;
        self.next_id += 1;
        ClientId(id)
    }
}

fn accept_client(state: &mut ShoestringWm, stream: UnixStream) -> Result<()> {
    stream.set_nonblocking(true)?;
    let server = state
        .ipc
        .as_mut()
        .expect("accept_client without an ipc server");
    let id = server.next_id();
    let client = Rc::new(RefCell::new(Client {
        stream,
        read_buf: Vec::new(),
        subscriber: false,
        spent: false,
    }));

    let source_client = Rc::clone(&client);
    let try_clone = client.borrow().stream.try_clone()?;
    let source = Generic::new(try_clone, Interest::READ, Mode::Level);
    let token = state
        .loop_handle
        .insert_source(source, move |_, _fd, state| {
            // The Rc lets us pass owned access into ShoestringWm helpers
            // without re-borrowing the source.
            let drop_me = handle_readable(state, id, &source_client);
            if drop_me {
                state.drop_ipc_client(id);
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| anyhow::anyhow!("insert ipc client source: {e}"))?;

    server.clients.insert(id, ClientEntry { client, token });
    tracing::debug!(?id, "ipc client connected");
    Ok(())
}

/// Returns `true` if the caller should drop this client (EOF, error, or
/// one-shot reply already written).
fn handle_readable(state: &mut ShoestringWm, id: ClientId, client: &Rc<RefCell<Client>>) -> bool {
    let mut buf = [0u8; 1024];
    let n = {
        let mut c = client.borrow_mut();
        match c.stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                c.read_buf.extend_from_slice(&buf[..n]);
                n
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) => {
                tracing::debug!(?id, error = %e, "ipc client read failed");
                return true;
            }
        }
    };
    let _ = n;

    // Pull off one line at a time. We only expect one request per connection,
    // but loop in case the client sent garbage after it.
    loop {
        let line = {
            let mut c = client.borrow_mut();
            let Some(nl) = c.read_buf.iter().position(|&b| b == b'\n') else {
                return false;
            };
            let line: Vec<u8> = c.read_buf.drain(..=nl).collect();
            line
        };
        let line = match std::str::from_utf8(&line) {
            Ok(s) => s.trim_end_matches(['\n', '\r']).to_string(),
            Err(_) => {
                let _ = write_response(
                    client,
                    &Response::Error {
                        message: "request is not valid utf-8".into(),
                    },
                );
                return true;
            }
        };
        if line.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_response(
                    client,
                    &Response::Error {
                        message: format!("invalid request json: {e}"),
                    },
                );
                return true;
            }
        };

        if client.borrow().spent {
            // Extra request on a connection that already replied. Hang up.
            return true;
        }

        match request {
            Request::Workspaces => {
                let resp = Response::Workspaces {
                    active: state.workspaces.active().one_based(),
                    count: state.workspaces.count(),
                    names: state.workspaces.name_list(),
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::Windows => {
                let windows = collect_windows(state);
                let _ = write_response(client, &Response::Windows { windows });
                return true;
            }
            Request::Outputs => {
                let outputs = collect_outputs(state);
                let _ = write_response(client, &Response::Outputs { outputs });
                return true;
            }
            Request::EventStream => {
                if write_response(client, &Response::Ok).is_err() {
                    return true;
                }
                client.borrow_mut().subscriber = true;
                client.borrow_mut().spent = true;
                tracing::debug!(?id, "ipc client subscribed to events");
                return false;
            }
            Request::InjectKey { keysym } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                let resp = match state.inject_key(&keysym) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::InjectText { text } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                let resp = match state.inject_text(&text) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::Lock => {
                state.spawn_lock();
                let _ = write_response(client, &Response::Ok);
                return true;
            }
            Request::SetAutomation { enabled } => {
                let changed = state.automation_enabled != enabled;
                state.automation_enabled = enabled;
                let _ = write_response(client, &Response::Automation { enabled });
                if changed {
                    tracing::info!(enabled, "automation gate changed via ipc");
                    state.emit_ipc(Event::AutomationChanged { enabled });
                }
                return true;
            }
            Request::AutomationStatus => {
                let _ = write_response(
                    client,
                    &Response::Automation {
                        enabled: state.automation_enabled,
                    },
                );
                return true;
            }
            Request::Screenshot { output, region } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                if region.is_some() && output.is_none() {
                    let _ = write_response(
                        client,
                        &Response::Error {
                            message: "screenshot: region requires output (coordinates are \
                                      output-relative)"
                                .into(),
                        },
                    );
                    return true;
                }
                // Mark spent so any further bytes from this client are
                // dropped — the response is deferred until the subprocess
                // exits, but we've already accepted the request.
                client.borrow_mut().spent = true;
                match state.spawn_remote_screenshot(
                    id,
                    Rc::clone(client),
                    output.as_deref(),
                    region,
                ) {
                    Ok(_path) => {
                        // Hold the connection open; finalize_remote_screenshot
                        // will write the response and drop_ipc_client.
                        return false;
                    }
                    Err(e) => {
                        let _ = write_response(
                            client,
                            &Response::Error {
                                message: format!("screenshot spawn failed: {e}"),
                            },
                        );
                        return true;
                    }
                }
            }
            Request::RunCommand { argv, timeout_ms } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                if argv.is_empty() {
                    let _ = write_response(
                        client,
                        &Response::Error {
                            message: "run_command: argv must be non-empty".into(),
                        },
                    );
                    return true;
                }
                // Hold the connection open across the deferred reply,
                // same as Screenshot. Mark spent so a misbehaving
                // client can't slip in a second request.
                client.borrow_mut().spent = true;
                match state.spawn_remote_command(id, Rc::clone(client), &argv, timeout_ms) {
                    Ok(()) => return false,
                    Err(e) => {
                        let _ = write_response(
                            client,
                            &Response::Error {
                                message: format!("run_command spawn failed: {e}"),
                            },
                        );
                        return true;
                    }
                }
            }
            Request::ReloadConfig => {
                let resp = match state.reload_config_from_disk() {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: format!("reload_config: {e}"),
                    },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::PickWindow => {
                // The reply is deferred until the user clicks (or
                // cancels). Mark spent so we don't try to read a
                // follow-up request on this same connection — the
                // picker resolution will write the response and drop
                // the client. A disconnect mid-pick cancels via
                // `cancel_picker_if_owned_by` in `drop_ipc_client`.
                client.borrow_mut().spent = true;
                if let Err(msg) = state.start_picker(id, Rc::clone(client)) {
                    let _ = write_response(
                        client,
                        &Response::Error {
                            message: msg.into(),
                        },
                    );
                    return true;
                }
                return false;
            }
            Request::CloseWindow { id: window_id } => {
                let resp = match state.close_window_by_id(&window_id) {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::FocusWindow { id: window_id } => {
                let resp = match state.focus_window_by_id(&window_id) {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::InjectClick { button, x, y } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                let xy = match (x, y) {
                    (Some(x), Some(y)) => Some((x, y)),
                    (None, None) => None,
                    _ => {
                        let _ = write_response(
                            client,
                            &Response::Error {
                                message: "inject_click: x and y must be passed together".into(),
                            },
                        );
                        return true;
                    }
                };
                let resp = match state.inject_click(&button, xy) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                };
                let _ = write_response(client, &resp);
                return true;
            }
        }
    }
}

/// Structured rejection for IPC methods gated behind
/// `automation_enabled`. The message is stable enough that callers (and
/// the bar's status indicator) can scrape on the prefix; we keep it
/// human-readable rather than introducing a typed error variant.
fn automation_off_error() -> Response {
    Response::Error {
        message: "automation disabled: enable with `shoestring-ctl automation on` \
                  or restart the WM with --enable-automation"
            .into(),
    }
}

pub(crate) fn write_response(client: &Rc<RefCell<Client>>, resp: &Response) -> std::io::Result<()> {
    let line = serde_json::to_string(resp).expect("Response must serialize");
    let mut c = client.borrow_mut();
    c.stream.write_all(line.as_bytes())?;
    c.stream.write_all(b"\n")?;
    c.spent = true;
    Ok(())
}

fn collect_windows(state: &ShoestringWm) -> Vec<WindowSummary> {
    let focused = state.focused_window();
    state
        .foreign_toplevels
        .iter()
        .filter_map(|(window, handle)| {
            let surface = window.toplevel()?.wl_surface().clone();
            let (title, app_id) = read_title_app_id(&surface);
            let workspace = state
                .workspaces
                .windows_on_any()
                .find(|(w, _)| w == window)
                .map(|(_, ws)| ws.one_based())
                .unwrap_or(0);
            Some(WindowSummary {
                id: handle.identifier(),
                title,
                app_id,
                workspace,
                focused: focused.as_ref() == Some(window),
            })
        })
        .collect()
}

fn read_title_app_id(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> (String, String) {
    use smithay::wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData};
    with_states(surface, |states| {
        let data = states.data_map.get::<XdgToplevelSurfaceData>();
        let Some(data) = data else {
            return (String::new(), String::new());
        };
        let g = data.lock().unwrap();
        (
            g.title.clone().unwrap_or_default(),
            g.app_id.clone().unwrap_or_default(),
        )
    })
}

fn collect_outputs(state: &ShoestringWm) -> Vec<OutputSummary> {
    state
        .space
        .outputs()
        .map(|o| {
            let mode = o.current_mode();
            let scale = match o.current_scale() {
                smithay::output::Scale::Integer(i) => i as f64,
                smithay::output::Scale::Fractional(f) => f,
                _ => 1.0,
            };
            OutputSummary {
                name: o.name(),
                width: mode.map(|m| m.size.w).unwrap_or(0),
                height: mode.map(|m| m.size.h).unwrap_or(0),
                scale,
            }
        })
        .collect()
}

impl ShoestringWm {
    /// Push an event to every subscribed client. Dropping subscribers on
    /// write failure is intentional — see module docs.
    pub fn emit_ipc(&mut self, event: Event) {
        let Some(server) = self.ipc.as_mut() else {
            return;
        };
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, ?event, "event serialize failed");
                return;
            }
        };
        let mut to_drop: Vec<ClientId> = Vec::new();
        for (&id, entry) in &server.clients {
            let mut c = entry.client.borrow_mut();
            if !c.subscriber {
                continue;
            }
            if let Err(e) = c
                .stream
                .write_all(line.as_bytes())
                .and_then(|_| c.stream.write_all(b"\n"))
            {
                tracing::debug!(?id, error = %e, "ipc event write failed; dropping subscriber");
                to_drop.push(id);
            }
        }
        for id in to_drop {
            self.drop_ipc_client(id);
        }
    }

    pub(crate) fn drop_ipc_client(&mut self, id: ClientId) {
        // Picker mode is owned by a specific client connection; if that
        // client disconnects before the user resolves the pick, the
        // session must end (no one to deliver the reply to).
        self.cancel_picker_if_owned_by(id);
        let Some(server) = self.ipc.as_mut() else {
            return;
        };
        if let Some(entry) = server.clients.remove(&id) {
            self.loop_handle.remove(entry.token);
            tracing::debug!(?id, "ipc client dropped");
        }
    }
}
