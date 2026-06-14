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
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use shoestring_ipc::{
    default_socket_path, Event, OutputNode, OutputSummary, Request, Response, WindowGeometry,
    WindowNode, WindowSummary, WorkspaceNode, SOCKET_ENV,
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
    /// `Some` after [`Request::MetricsStream`]: this connection tails the
    /// diagnostics sampler. The sampler timer pushes [`Event::Metrics`] at
    /// it every tick, gated by the per-subscriber interval below.
    metrics_sub: Option<MetricsSub>,
}

/// Per-connection state for a `metrics` stream subscriber.
struct MetricsSub {
    /// Minimum gap between pushes. Clamped *up* to the sampler cadence at
    /// subscribe time, so it never asks for samples faster than they're
    /// taken.
    interval: Duration,
    /// When we last pushed to this client; `None` until the first push.
    last_push: Option<Instant>,
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
        // Listen at the path derived from our own WAYLAND_DISPLAY (set to our
        // socket name just before start_ipc). We deliberately do NOT honor an
        // inherited $SHOESTRING_WM_SOCKET: that variable is how *clients*
        // locate a socket, so a nested WM that inherited it from the host
        // session would otherwise bind — and unlink (below) — the host
        // compositor's live socket. start_ipc re-exports SOCKET_ENV to our
        // own path for the children we spawn.
        let socket_path = default_socket_path()
            .context("cannot derive IPC socket path: $XDG_RUNTIME_DIR or $WAYLAND_DISPLAY unset")?;

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

    /// Total connected IPC clients (one-shot and streaming alike).
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Long-lived streaming subscribers — event-stream tails plus metrics
    /// tails. Surfaced as the `ipc.subscribers` gauge.
    pub fn subscriber_count(&self) -> usize {
        self.clients
            .values()
            .filter(|e| {
                let c = e.client.borrow();
                c.subscriber || c.metrics_sub.is_some()
            })
            .count()
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
        metrics_sub: None,
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
            Request::GetTree => {
                let (outputs, workspaces, minimized) = collect_tree(state);
                let _ = write_response(
                    client,
                    &Response::Tree {
                        outputs,
                        workspaces,
                        minimized,
                    },
                );
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
            Request::InjectKey { keysym, modifiers } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                let resp = match state.inject_key(&keysym, &modifiers) {
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
                // Automation is a superset of screen capture: this also opens
                // (or closes) the capture gate so `screenshot` works under
                // automation alone. Returns whether capture followed.
                let capture_changed = state.set_automation(enabled);
                let _ = write_response(client, &Response::Automation { enabled });
                if changed {
                    tracing::info!(enabled, "automation gate changed via ipc");
                    state.emit_ipc(Event::AutomationChanged { enabled });
                }
                if capture_changed {
                    tracing::info!(enabled, "screen-capture gate followed automation gate");
                    state.emit_ipc(Event::ScreenCaptureChanged { enabled });
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
            Request::SetScreenCapture { enabled } => {
                let changed = state.screen_capture_enabled != enabled;
                state.set_screen_capture(enabled);
                let _ = write_response(client, &Response::ScreenCapture { enabled });
                if changed {
                    tracing::info!(enabled, "screen-capture gate changed via ipc");
                    state.emit_ipc(Event::ScreenCaptureChanged { enabled });
                }
                return true;
            }
            Request::ScreenCaptureStatus => {
                let _ = write_response(
                    client,
                    &Response::ScreenCapture {
                        enabled: state.screen_capture_enabled,
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
            Request::RaiseWindow { id: window_id } => {
                let resp = match state.raise_window_by_id(&window_id) {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::LowerWindow { id: window_id } => {
                let resp = match state.lower_window_by_id(&window_id) {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::SetWindowSticky {
                id: window_id,
                sticky,
            } => {
                let resp = match state.set_window_sticky_by_id(&window_id, sticky) {
                    Ok(()) => Response::Ok,
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::SetWindowName {
                id: window_id,
                name,
            } => {
                let resp = match state.set_window_name_by_id(&window_id, &name) {
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
            Request::MoveMouse { x, y } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                state.inject_move_mouse(x, y);
                let _ = write_response(client, &Response::Ok);
                return true;
            }
            Request::PointerPosition => {
                let (x, y) = state.pointer_position();
                let _ = write_response(client, &Response::PointerPosition { x, y });
                return true;
            }
            Request::FindWindows { title, app_id } => {
                let resp = match find_windows(state, title.as_deref(), app_id.as_deref()) {
                    Ok(windows) => Response::Windows { windows },
                    Err(message) => Response::Error { message },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::Metrics => {
                // Sample fresh on demand so a snapshot is current even when
                // the background sampler is off. Ungated — pure observation.
                state.sample_metrics();
                let resp = Response::Metrics {
                    ts_ms: crate::metrics::now_ms(),
                    metrics: state.metrics.snapshot(),
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::MetricsStream { interval_ms } => {
                if !state.config.diagnostics.enabled {
                    let _ = write_response(
                        client,
                        &Response::Error {
                            message: "metrics stream unavailable: diagnostics disabled \
                                      (set [diagnostics].enabled = true)"
                                .into(),
                        },
                    );
                    return true;
                }
                if write_response(client, &Response::Ok).is_err() {
                    return true;
                }
                // Clamp the requested interval up to the sampler cadence —
                // v1 can't push faster than it samples.
                let floor = state.config.diagnostics.sample_interval_ms.max(1);
                let want = interval_ms.map(u64::from).unwrap_or(0).max(floor);
                {
                    let mut c = client.borrow_mut();
                    c.metrics_sub = Some(MetricsSub {
                        interval: Duration::from_millis(want),
                        last_push: None,
                    });
                    // Long-lived like an event subscriber: stay open, re-armed
                    // for READ only to notice hangup.
                    c.spent = true;
                }
                tracing::debug!(?id, interval_ms = want, "ipc client subscribed to metrics");
                return false;
            }
            Request::DispatchAction { action } => {
                if !state.automation_enabled {
                    let _ = write_response(client, &automation_off_error());
                    return true;
                }
                // Decode the JSON Value into a real Action via shoestring-config's
                // serde impl. Failures here are user-fixable (bad key, missing
                // field) so the error string is surfaced verbatim.
                let resp = match serde_json::from_value::<shoestring_config::Action>(action) {
                    Ok(action) => {
                        tracing::info!(?action, "dispatching action via ipc");
                        state.dispatch_action(action);
                        Response::Ok
                    }
                    Err(e) => Response::Error {
                        message: format!("dispatch_action: malformed action: {e}"),
                    },
                };
                let _ = write_response(client, &resp);
                return true;
            }
            Request::SetGamma {
                output,
                temperature,
                brightness,
                gamma,
            } => {
                let resp = set_gamma_response(state, output, temperature, brightness, gamma);
                let _ = write_response(client, &resp);
                return true;
            }
            Request::ResetGamma { output } => {
                let resp = reset_gamma_response(state, output);
                let _ = write_response(client, &resp);
                return true;
            }
        }
    }
}

/// Handle `Request::SetGamma`. Gamma is KMS-only, so this is a no-op error in
/// builds without the `tty` backend.
#[cfg(feature = "tty")]
fn set_gamma_response(
    state: &mut ShoestringWm,
    output: Option<String>,
    temperature: u32,
    brightness: Option<f64>,
    gamma: Option<f64>,
) -> Response {
    match state.set_gamma_ipc(
        output.as_deref(),
        temperature,
        brightness.unwrap_or(1.0),
        gamma.unwrap_or(1.0),
    ) {
        Ok(_) => Response::Ok,
        Err(message) => Response::Error { message },
    }
}

#[cfg(not(feature = "tty"))]
fn set_gamma_response(
    _state: &mut ShoestringWm,
    _output: Option<String>,
    _temperature: u32,
    _brightness: Option<f64>,
    _gamma: Option<f64>,
) -> Response {
    Response::Error {
        message: "gamma control unavailable: this build has no KMS/TTY backend".into(),
    }
}

/// Handle `Request::ResetGamma`. See [`set_gamma_response`] for the non-TTY case.
#[cfg(feature = "tty")]
fn reset_gamma_response(state: &mut ShoestringWm, output: Option<String>) -> Response {
    state.reset_gamma_ipc(output.as_deref());
    Response::Ok
}

#[cfg(not(feature = "tty"))]
fn reset_gamma_response(_state: &mut ShoestringWm, _output: Option<String>) -> Response {
    Response::Error {
        message: "gamma control unavailable: this build has no KMS/TTY backend".into(),
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

/// Compile each supplied pattern once, then return every window whose
/// `title` and `app_id` match. An unset filter matches everything; both
/// filters are AND-ed when both are set. Returns `Err(message)` if either
/// regex fails to compile so the caller (IPC) can surface a useful error
/// instead of an empty match list.
fn find_windows(
    state: &ShoestringWm,
    title: Option<&str>,
    app_id: Option<&str>,
) -> Result<Vec<WindowSummary>, String> {
    let title_re = title
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| format!("find_windows: title regex invalid: {e}"))?;
    let app_id_re = app_id
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| format!("find_windows: app_id regex invalid: {e}"))?;
    Ok(collect_windows(state)
        .into_iter()
        .filter(|w| {
            title_re.as_ref().is_none_or(|re| re.is_match(&w.title))
                && app_id_re.as_ref().is_none_or(|re| re.is_match(&w.app_id))
        })
        .collect())
}

fn collect_windows(state: &ShoestringWm) -> Vec<WindowSummary> {
    let focused = state.focused_window();
    state
        .foreign_toplevels
        .iter()
        .map(|(window, handle)| {
            let (title, app_id) = effective_title_app_id(state, window);
            let workspace = state
                .workspaces
                .windows_on_any()
                .find(|(w, _)| w == window)
                .map(|(_, ws)| ws.one_based())
                .unwrap_or(0);
            WindowSummary {
                id: handle.identifier(),
                title,
                app_id,
                workspace,
                focused: focused.as_ref() == Some(window),
                geometry: window_geometry(state, window),
                z: window_z(state, window),
                sticky: state.is_sticky(window),
            }
        })
        .collect()
}

/// A window's on-screen rectangle in compositor-global logical coords, using
/// the same convention as the layout code: location from the space, size from
/// the window's `geometry()`. `None` when the window isn't mapped (minimized,
/// or on a non-active workspace — those are unmapped from the space and so
/// have no location).
pub(crate) fn window_geometry(
    state: &ShoestringWm,
    window: &smithay::desktop::Window,
) -> Option<WindowGeometry> {
    let loc = state.space.element_location(window)?;
    let size = window.geometry().size;
    Some(WindowGeometry {
        x: loc.x,
        y: loc.y,
        w: size.w,
        h: size.h,
    })
}

/// Stacking position of `window` within the mapped stack: `space.elements()`
/// yields bottom-to-top, so the enumeration index is the Z position (`0` is
/// bottom-most). `None` for windows that aren't mapped — minimized windows are
/// unmapped, and only the active workspace's windows are in the space. Shared
/// by the `windows` and `get_tree` snapshots so both report the same order.
pub(crate) fn window_z(state: &ShoestringWm, window: &smithay::desktop::Window) -> Option<u32> {
    state
        .space
        .elements()
        .position(|w| w == window)
        .map(|i| i as u32)
}

/// Name of the output a window's center sits on, by point containment against
/// each output's logical geometry. `None` when the window is unmapped or its
/// center falls outside every output.
fn window_output(state: &ShoestringWm, window: &smithay::desktop::Window) -> Option<String> {
    let geo = window_geometry(state, window)?;
    let (cx, cy) = (geo.x + geo.w / 2, geo.y + geo.h / 2);
    state
        .space
        .outputs()
        .find(|o| {
            state
                .space
                .output_geometry(o)
                .map(|g| {
                    cx >= g.loc.x
                        && cx < g.loc.x + g.size.w
                        && cy >= g.loc.y
                        && cy < g.loc.y + g.size.h
                })
                .unwrap_or(false)
        })
        .map(|o| o.name())
}

/// Build the get_tree-style snapshot: outputs with their logical placement,
/// plus every workspace that has windows (always including the active one)
/// and the windows on it. Only the active workspace is mapped, so only its
/// windows carry a `z` stacking index and geometry.
fn collect_tree(state: &ShoestringWm) -> (Vec<OutputNode>, Vec<WorkspaceNode>, Vec<WindowNode>) {
    use crate::layout::LayoutState;

    let outputs = state
        .space
        .outputs()
        .map(|o| {
            let geo = state.space.output_geometry(o).unwrap_or_default();
            let scale = match o.current_scale() {
                smithay::output::Scale::Integer(i) => i as f64,
                smithay::output::Scale::Fractional(f) => f,
                _ => 1.0,
            };
            OutputNode {
                name: o.name(),
                x: geo.loc.x,
                y: geo.loc.y,
                w: geo.size.w,
                h: geo.size.h,
                scale,
            }
        })
        .collect();

    let focused = state.focused_window();
    let active = state.workspaces.active();

    // Group every tracked window under its workspace.
    let mut by_workspace: std::collections::BTreeMap<u8, Vec<WindowNode>> =
        std::collections::BTreeMap::new();
    for (window, handle) in &state.foreign_toplevels {
        let ws = state
            .workspaces
            .windows_on_any()
            .find(|(w, _)| w == window)
            .map(|(_, ws)| ws.one_based())
            .unwrap_or(0);
        let (title, app_id) = effective_title_app_id(state, window);
        let layout = match state.layout.layout_state(window) {
            LayoutState::Floating => "floating",
            LayoutState::TiledLeft => "tiled_left",
            LayoutState::TiledRight => "tiled_right",
            LayoutState::Maximized => "maximized",
            LayoutState::Fullscreen => "fullscreen",
        };
        by_workspace.entry(ws).or_default().push(WindowNode {
            id: handle.identifier(),
            title,
            app_id,
            geometry: window_geometry(state, window),
            output: window_output(state, window),
            z: window_z(state, window),
            focused: focused.as_ref() == Some(window),
            minimized: state.layout.is_minimized(window),
            sticky: state.is_sticky(window),
            layout: layout.to_string(),
        });
    }

    // Sort the active workspace's windows by Z (bottom to top) so the order
    // reflects the stack; unmapped windows (z=None) sink to the front.
    if let Some(windows) = by_workspace.get_mut(&active.one_based()) {
        windows.sort_by_key(|w| w.z);
    }

    // Windows with no workspace (key 0) are minimized — minimizing drops the
    // workspace assignment (see `minimize_window`). Pull them out into the
    // flat `minimized` list rather than inventing a workspace 0.
    let minimized = by_workspace.remove(&0).unwrap_or_default();

    // Always surface the active workspace even when empty, so a script can
    // see "the focused workspace has no windows" rather than an absent entry.
    by_workspace.entry(active.one_based()).or_default();

    let names = state.workspaces.name_list();
    let workspaces = by_workspace
        .into_iter()
        .map(|(index, windows)| WorkspaceNode {
            index,
            name: names.get(index as usize - 1).cloned().unwrap_or_default(),
            focused: index == active.one_based(),
            windows,
        })
        .collect();

    (outputs, workspaces, minimized)
}

/// `(effective_title, app_id)` for a window: the IPC-set name override
/// (see [`Request::SetWindowName`]) when one is present, otherwise the
/// client's own title. `app_id` is never overridden. This is the title the
/// WM reports everywhere on its own IPC surface (`windows`, `get_tree`,
/// `find_windows`, the `window_title_changed` event) so a bar or window-jump
/// menu shows and matches on the override.
pub(crate) fn effective_title_app_id(
    state: &ShoestringWm,
    window: &smithay::desktop::Window,
) -> (String, String) {
    let (title, app_id) = window_title_app_id(window);
    let title = state
        .window_name_overrides
        .get(window)
        .cloned()
        .unwrap_or(title);
    (title, app_id)
}

/// `(title, app_id)` for either an xdg toplevel or an X11 window.
/// X11 surfaces use their `WM_CLASS` second field as the app_id
/// equivalent — that's what window rules match against and what the
/// bar will surface as the app label.
fn window_title_app_id(window: &smithay::desktop::Window) -> (String, String) {
    if let Some(toplevel) = window.toplevel() {
        return read_title_app_id(toplevel.wl_surface());
    }
    if let Some(x11) = window.x11_surface() {
        return (x11.title(), x11.class());
    }
    (String::new(), String::new())
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

    /// Push the current metrics snapshot to every `metrics` subscriber
    /// whose per-client interval has elapsed. Called once per sampler
    /// tick. Same drop-on-write-failure discipline as [`Self::emit_ipc`]:
    /// a subscriber we can't write to is broken, so we drop it rather than
    /// buffer.
    pub fn push_metrics_to_subscribers(&mut self) {
        // Bail before serializing if nobody's listening.
        let any = self.ipc.as_ref().is_some_and(|s| {
            s.clients
                .values()
                .any(|e| e.client.borrow().metrics_sub.is_some())
        });
        if !any {
            return;
        }
        let event = Event::Metrics {
            ts_ms: crate::metrics::now_ms(),
            metrics: self.metrics.snapshot(),
        };
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "metrics event serialize failed");
                return;
            }
        };
        let now = Instant::now();
        let server = self.ipc.as_mut().expect("checked above");
        let mut to_drop: Vec<ClientId> = Vec::new();
        for (&id, entry) in &server.clients {
            let mut c = entry.client.borrow_mut();
            let Some(sub) = c.metrics_sub.as_ref() else {
                continue;
            };
            // Honor the per-subscriber interval (slower than the sampler is
            // fine; faster was clamped away at subscribe time).
            if let Some(last) = sub.last_push {
                if now.duration_since(last) < sub.interval {
                    continue;
                }
            }
            if let Err(e) = c
                .stream
                .write_all(line.as_bytes())
                .and_then(|_| c.stream.write_all(b"\n"))
            {
                tracing::debug!(?id, error = %e, "metrics push failed; dropping subscriber");
                to_drop.push(id);
            } else if let Some(sub) = c.metrics_sub.as_mut() {
                sub.last_push = Some(now);
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
