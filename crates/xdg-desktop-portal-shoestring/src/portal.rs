//! `org.freedesktop.impl.portal.ScreenCast` — the backend half of the
//! screencast portal, spoken to the xdg-desktop-portal frontend.
//!
//! Flow (all calls carry a `handle` Request path + a `session_handle`
//! Session path the frontend owns):
//!
//! 1. `CreateSession` — register a [`Session`].
//! 2. `SelectSources` — record requested source types + cursor mode.
//! 3. `Start` — start the PipeWire stream, return its node id in
//!    `results["streams"]` as `a(ua{sv})`.
//! 4. `OpenPipeWireRemote` — hand the app an fd onto our PipeWire remote.
//!
//! The frontend also reads our properties (`version`,
//! `AvailableSourceTypes`, `AvailableCursorModes`) via
//! `org.freedesktop.DBus.Properties`, and may `Close` a Session or Request.
//!
//! This module only translates wire calls into state mutations + reply
//! messages; it sends nothing itself (see [`crate::state::State`] and the
//! dispatch loop in `main`). The PipeWire stream it drives on `Start` lives in
//! [`crate::cast`]. (Phase 1b streams a test pattern; Phase 2 swaps in real
//! frames captured from the WM over `zwlr_screencopy_v1`.)

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{anyhow, Context, Result};
use rustbus::connection::Timeout;
use rustbus::message_builder::{MarshalledMessage, MessageType};
use rustbus::wire::unmarshal::traits::Variant;
use rustbus::wire::ObjectPath;
use rustbus::wire::UnixFd;
use rustbus::{standard_messages, RpcConn};

use crate::capture::Capture;
use crate::cast;
use crate::state::{Session, State};

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.shoestring";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

const SCREENCAST_IFACE: &str = "org.freedesktop.impl.portal.ScreenCast";
const SESSION_IFACE: &str = "org.freedesktop.impl.portal.Session";
const REQUEST_IFACE: &str = "org.freedesktop.impl.portal.Request";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const INTROSPECTABLE_IFACE: &str = "org.freedesktop.DBus.Introspectable";

/// Portal method response codes (the leading `u` in `(ua{sv})`).
const RESPONSE_SUCCESS: u32 = 0;
#[allow(dead_code)]
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_OTHER: u32 = 2;

/// `AvailableSourceTypes` bit for whole-monitor capture (the only kind we do).
const SOURCE_TYPE_MONITOR: u32 = 1;
/// `AvailableCursorModes` bit: cursor composited into the frame. Our
/// screencopy always includes the cursor, so this is all we offer.
const CURSOR_MODE_EMBEDDED: u32 = 2;
/// Interface version we implement. v2 is enough for source-type + cursor-mode
/// selection; restore tokens (v3+) are not implemented.
const SCREENCAST_VERSION: u32 = 2;

// Variant enum for marshalling property / results values. The `_sig` macro
// generates an owned (lifetime-free) enum, which is what we want for building
// outgoing messages. `StreamProps`/`Streams` cover the nested `a(ua{sv})`
// shape Start returns.
type StreamList = Vec<(u32, HashMap<String, StreamProp>)>;
rustbus::dbus_variant_sig!(StreamProp, U32 => u32);
rustbus::dbus_variant_sig!(PortalValue, U32 => u32; Streams => StreamList);

/// Connect to the session bus and claim [`BUS_NAME`].
///
/// `DO_NOT_QUEUE` without `REPLACE_EXISTING`: if an instance already owns the
/// name (e.g. the session autostarted us and the portal frontend then tries to
/// D-Bus-activate a second copy), we don't steal it — we fail and exit, leaving
/// the running instance in charge.
pub fn acquire_bus() -> Result<RpcConn> {
    let mut rpc = RpcConn::session_conn(Timeout::Infinite).context("connect to session bus")?;

    let flags = standard_messages::DBUS_NAME_FLAG_DO_NOT_QUEUE;
    let serial = rpc
        .send_message(&mut standard_messages::request_name(BUS_NAME, flags))
        .context("send RequestName")?
        .write_all()
        .map_err(|(_, e)| anyhow!("write RequestName: {e:?}"))?;
    let resp = rpc
        .wait_response(serial, Timeout::Infinite)
        .context("await RequestName reply")?;
    let code: u32 = resp
        .body
        .parser()
        .get()
        .map_err(|e| anyhow!("parse RequestName reply: {e:?}"))?;

    match code {
        standard_messages::DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER
        | standard_messages::DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER => {
            tracing::info!("acquired {BUS_NAME} on session bus");
            Ok(rpc)
        }
        code => Err(anyhow!(
            "could not acquire {BUS_NAME}: RequestName returned {code} \
             (1=primary, 2=in-queue, 3=exists, 4=already-owner)"
        )),
    }
}

/// True for calls we should handle (anything under our object tree —
/// the main object plus the Session/Request objects the frontend creates).
pub fn aimed_at_us(call: &MarshalledMessage) -> bool {
    call.dynheader
        .object
        .as_deref()
        .is_some_and(|o| o.starts_with(OBJECT_PATH))
}

/// Dispatch one inbound call to its handler, returning the reply to send.
/// Returns `None` only for non-calls (which the filter already excludes).
pub fn dispatch(call: &MarshalledMessage, state: &mut State) -> Option<MarshalledMessage> {
    if call.typ != MessageType::Call {
        return None;
    }
    let object = call.dynheader.object.as_deref()?;
    let member = call.dynheader.member.as_deref()?;
    let interface = call.dynheader.interface.as_deref();

    let reply = match (interface, member) {
        (Some(PROPS_IFACE), "Get") => properties_get(call),
        (Some(PROPS_IFACE), "GetAll") => properties_get_all(call),
        (Some(SCREENCAST_IFACE), "CreateSession") => create_session(call, state),
        (Some(SCREENCAST_IFACE), "SelectSources") => select_sources(call, state),
        (Some(SCREENCAST_IFACE), "Start") => start(call, state),
        (Some(SCREENCAST_IFACE), "OpenPipeWireRemote") => open_pipewire_remote(call, state),
        (Some(SESSION_IFACE), "Close") => close_session(call, state),
        (Some(REQUEST_IFACE), "Close") => close_request(call),
        (Some(INTROSPECTABLE_IFACE) | None, "Introspect") => introspect(call),
        _ => {
            tracing::debug!(object, ?interface, member, "unhandled call");
            standard_messages::unknown_method(&call.dynheader)
        }
    };
    Some(reply)
}

// ---- properties ----------------------------------------------------------

fn property_value(name: &str) -> Option<PortalValue> {
    match name {
        "version" => Some(PortalValue::U32(SCREENCAST_VERSION)),
        "AvailableSourceTypes" => Some(PortalValue::U32(SOURCE_TYPE_MONITOR)),
        "AvailableCursorModes" => Some(PortalValue::U32(CURSOR_MODE_EMBEDDED)),
        _ => None,
    }
}

fn properties_get(call: &MarshalledMessage) -> MarshalledMessage {
    let mut p = call.body.parser();
    let (iface, prop) = match p.get2::<&str, &str>() {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("Get args: {e:?}")),
    };
    if iface != SCREENCAST_IFACE {
        return invalid_args(call, &format!("no properties on interface {iface}"));
    }
    match property_value(prop) {
        Some(v) => {
            let mut reply = call.dynheader.make_response();
            reply.body.push_param(v).expect("push property variant");
            reply
        }
        None => invalid_args(call, &format!("unknown property {prop}")),
    }
}

fn properties_get_all(call: &MarshalledMessage) -> MarshalledMessage {
    let iface: &str = match call.body.parser().get() {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("GetAll arg: {e:?}")),
    };
    let mut props: HashMap<String, PortalValue> = HashMap::new();
    if iface == SCREENCAST_IFACE {
        for name in ["version", "AvailableSourceTypes", "AvailableCursorModes"] {
            if let Some(v) = property_value(name) {
                props.insert(name.to_owned(), v);
            }
        }
    }
    let mut reply = call.dynheader.make_response();
    reply.body.push_param(props).expect("push properties dict");
    reply
}

// ---- ScreenCast methods --------------------------------------------------

/// Read the two leading object-path args (`handle`, `session_handle`) that
/// every ScreenCast method except OpenPipeWireRemote carries.
fn read_handles(call: &MarshalledMessage) -> Result<(String, String)> {
    let mut p = call.body.parser();
    let handle: ObjectPath<String> = p.get().map_err(|e| anyhow!("handle: {e:?}"))?;
    let session: ObjectPath<String> = p.get().map_err(|e| anyhow!("session_handle: {e:?}"))?;
    Ok((handle.as_ref().to_owned(), session.as_ref().to_owned()))
}

fn create_session(call: &MarshalledMessage, state: &mut State) -> MarshalledMessage {
    let mut p = call.body.parser();
    let _handle: ObjectPath<String> = match p.get() {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("handle: {e:?}")),
    };
    let session: ObjectPath<String> = match p.get() {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("session_handle: {e:?}")),
    };
    let app_id: String = p.get().unwrap_or_default();
    let session = session.as_ref().to_owned();

    tracing::info!(%session, %app_id, "CreateSession");
    state.sessions.insert(
        session,
        Session {
            app_id,
            ..Session::default()
        },
    );
    response(call, RESPONSE_SUCCESS)
}

fn select_sources(call: &MarshalledMessage, state: &mut State) -> MarshalledMessage {
    let (_handle, session) = match read_handles(call) {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("{e:#}")),
    };
    // Skip app_id (s), then read the options dict.
    let mut p = call.body.parser();
    let _ = p.get::<ObjectPath<String>>();
    let _ = p.get::<ObjectPath<String>>();
    let _ = p.get::<&str>();
    let options: HashMap<String, Variant> = p.get().unwrap_or_default();

    let types = variant_u32(&options, "types").unwrap_or(SOURCE_TYPE_MONITOR);
    let cursor_mode = variant_u32(&options, "cursor_mode").unwrap_or(CURSOR_MODE_EMBEDDED);
    let multiple = variant_bool(&options, "multiple").unwrap_or(false);

    tracing::info!(%session, types, cursor_mode, multiple, "SelectSources");
    if let Some(s) = state.sessions.get_mut(&session) {
        s.source_types = types;
        s.cursor_mode = cursor_mode;
        s.multiple = multiple;
        response(call, RESPONSE_SUCCESS)
    } else {
        tracing::warn!(%session, "SelectSources for unknown session");
        response(call, RESPONSE_OTHER)
    }
}

fn start(call: &MarshalledMessage, state: &mut State) -> MarshalledMessage {
    let (_handle, session) = match read_handles(call) {
        Ok(v) => v,
        Err(e) => return invalid_args(call, &format!("{e:#}")),
    };
    let Some(s) = state.sessions.get(&session) else {
        tracing::warn!(%session, "Start for unknown session");
        return response(call, RESPONSE_OTHER);
    };
    tracing::info!(%session, app = %s.app_id, types = s.source_types,
        cursor_mode = s.cursor_mode, multiple = s.multiple, "Start");

    // Connect to the WM and learn the output geometry. `Capture::new` fails if
    // the screen-capture gate is off (the screencopy manager global is absent).
    let capture = match Capture::new(None) {
        Ok(c) => Rc::new(RefCell::new(c)),
        Err(e) => {
            tracing::warn!(error = %e, "Start: cannot capture (screen-capture gate off?)");
            return response(call, RESPONSE_OTHER);
        }
    };
    let info = match capture.borrow_mut().dimensions() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "Start: could not capture initial frame");
            return response(call, RESPONSE_OTHER);
        }
    };

    match cast::start(&state.pw, capture, info.width, info.height) {
        Ok((c, node_id)) => {
            state.casts.insert(session, c);
            start_response(call, node_id)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Start: could not start PipeWire stream");
            response(call, RESPONSE_OTHER)
        }
    }
}

/// `(u response, a{sv} results)` with `results["streams"] = [(node_id, props)]`.
fn start_response(call: &MarshalledMessage, node_id: u32) -> MarshalledMessage {
    let mut props: HashMap<String, StreamProp> = HashMap::new();
    props.insert(
        "source_type".to_owned(),
        StreamProp::U32(SOURCE_TYPE_MONITOR),
    );
    let streams: StreamList = vec![(node_id, props)];

    let mut results: HashMap<String, PortalValue> = HashMap::new();
    results.insert("streams".to_owned(), PortalValue::Streams(streams));

    let mut reply = call.dynheader.make_response();
    reply
        .body
        .push_param(RESPONSE_SUCCESS)
        .expect("push response code");
    reply.body.push_param(results).expect("push results");
    reply
}

fn open_pipewire_remote(call: &MarshalledMessage, _state: &mut State) -> MarshalledMessage {
    match cast::open_remote_fd() {
        Ok(raw) => {
            tracing::info!("OpenPipeWireRemote");
            let mut reply = call.dynheader.make_response();
            // UnixFd takes ownership of the fd and closes it on drop, after the
            // kernel has dup'd it to the receiver via SCM_RIGHTS on send.
            reply
                .body
                .push_param(UnixFd::new(raw))
                .expect("push pipewire fd");
            reply
        }
        Err(e) => {
            tracing::warn!(error = %e, "OpenPipeWireRemote failed");
            call.dynheader.make_error_response(
                "org.freedesktop.DBus.Error.Failed".to_owned(),
                Some(format!("{e:#}")),
            )
        }
    }
}

// ---- Session / Request lifecycle -----------------------------------------

fn close_session(call: &MarshalledMessage, state: &mut State) -> MarshalledMessage {
    if let Some(path) = call.dynheader.object.as_deref() {
        let had_session = state.sessions.remove(path).is_some();
        // Dropping the Cast tears down the PipeWire stream.
        let had_cast = state.casts.remove(path).is_some();
        if had_session || had_cast {
            tracing::info!(session = path, "Session.Close");
        }
    }
    call.dynheader.make_response()
}

fn close_request(call: &MarshalledMessage) -> MarshalledMessage {
    // We complete every method synchronously, so there's nothing to cancel;
    // just acknowledge.
    call.dynheader.make_response()
}

// ---- helpers -------------------------------------------------------------

/// Build a `(u response, a{sv} results)` reply with an empty results dict.
fn response(call: &MarshalledMessage, code: u32) -> MarshalledMessage {
    let mut reply = call.dynheader.make_response();
    reply.body.push_param(code).expect("push response code");
    let results: HashMap<String, PortalValue> = HashMap::new();
    reply.body.push_param(results).expect("push empty results");
    reply
}

fn variant_u32(opts: &HashMap<String, Variant>, key: &str) -> Option<u32> {
    opts.get(key).and_then(|v| v.get::<u32>().ok())
}

fn variant_bool(opts: &HashMap<String, Variant>, key: &str) -> Option<bool> {
    opts.get(key).and_then(|v| v.get::<bool>().ok())
}

fn invalid_args(call: &MarshalledMessage, msg: &str) -> MarshalledMessage {
    tracing::warn!(msg, "rejecting call: invalid args");
    call.dynheader.make_error_response(
        "org.freedesktop.DBus.Error.InvalidArgs".to_owned(),
        Some(msg.to_owned()),
    )
}

fn introspect(call: &MarshalledMessage) -> MarshalledMessage {
    let mut reply = call.dynheader.make_response();
    reply.body.push_param(INTROSPECTION_XML).expect("push xml");
    reply
}

const INTROSPECTION_XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect">
      <arg name="data" type="s" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get">
      <arg name="interface_name" type="s" direction="in"/>
      <arg name="property_name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="GetAll">
      <arg name="interface_name" type="s" direction="in"/>
      <arg name="properties" type="a{sv}" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.impl.portal.ScreenCast">
    <property name="version" type="u" access="read"/>
    <property name="AvailableSourceTypes" type="u" access="read"/>
    <property name="AvailableCursorModes" type="u" access="read"/>
    <method name="CreateSession">
      <arg name="handle" type="o" direction="in"/>
      <arg name="session_handle" type="o" direction="in"/>
      <arg name="app_id" type="s" direction="in"/>
      <arg name="options" type="a{sv}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
      <arg name="results" type="a{sv}" direction="out"/>
    </method>
    <method name="SelectSources">
      <arg name="handle" type="o" direction="in"/>
      <arg name="session_handle" type="o" direction="in"/>
      <arg name="app_id" type="s" direction="in"/>
      <arg name="options" type="a{sv}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
      <arg name="results" type="a{sv}" direction="out"/>
    </method>
    <method name="Start">
      <arg name="handle" type="o" direction="in"/>
      <arg name="session_handle" type="o" direction="in"/>
      <arg name="app_id" type="s" direction="in"/>
      <arg name="parent_window" type="s" direction="in"/>
      <arg name="options" type="a{sv}" direction="in"/>
      <arg name="response" type="u" direction="out"/>
      <arg name="results" type="a{sv}" direction="out"/>
    </method>
    <method name="OpenPipeWireRemote">
      <arg name="session_handle" type="o" direction="in"/>
      <arg name="options" type="a{sv}" direction="in"/>
      <arg name="fd" type="h" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.impl.portal.Session">
    <method name="Close"/>
    <signal name="Closed"/>
  </interface>
  <interface name="org.freedesktop.impl.portal.Request">
    <method name="Close"/>
  </interface>
</node>
"#;
