//! org.freedesktop.Notifications service bound via rustbus.
//!
//! Owns the session-bus name request and message dispatch. State lives
//! in [`crate::state::Queue`]; this module only translates wire calls
//! into mutations + reply / signal messages.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use rustbus::connection::Timeout;
use rustbus::message_builder::{MarshalledMessage, MessageBuilder, MessageType};
use rustbus::wire::unmarshal::traits::Variant;
use rustbus::{standard_messages, RpcConn};

use crate::state::{CloseReason, NotifyArgs, Queue, Urgency};

pub const BUS_NAME: &str = "org.freedesktop.Notifications";
pub const OBJECT_PATH: &str = "/org/freedesktop/Notifications";
pub const INTERFACE: &str = "org.freedesktop.Notifications";

const INTROSPECTABLE_INTERFACE: &str = "org.freedesktop.DBus.Introspectable";

const SPEC_VERSION: &str = "1.2";
const VENDOR: &str = "shoestring";

/// Connect to the session bus and claim BUS_NAME.
///
/// Uses DO_NOT_QUEUE | REPLACE_EXISTING per task spec — we want to take over
/// from any existing notification daemon (dunst, mako, ...) when launched,
/// and refuse to start silently if the bus rejects us.
pub fn acquire_bus() -> Result<RpcConn> {
    let mut rpc = RpcConn::session_conn(Timeout::Infinite).context("connect to session bus")?;

    let flags = standard_messages::DBUS_NAME_FLAG_DO_NOT_QUEUE
        | standard_messages::DBUS_NAME_FLAG_REPLACE_EXISTING;

    let serial = rpc
        .send_message(&mut standard_messages::request_name(BUS_NAME, flags))
        .context("send RequestName")?
        .write_all()
        .map_err(|(_, e)| anyhow!("write RequestName: {e:?}"))?;

    let resp = rpc
        .wait_response(serial, Timeout::Infinite)
        .context("await RequestName reply")?;

    let reply_code: u32 = resp
        .body
        .parser()
        .get()
        .map_err(|e| anyhow!("parse RequestName reply: {e:?}"))?;

    match reply_code {
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

/// Outcome of dispatching one inbound call.
///
/// `reply` is the method-return / error to send back to the caller.
/// `signals` are broadcast signals triggered by the state change (e.g.
/// `NotificationClosed` for a `CloseNotification` call). All messages
/// in `signals` are addressed and ready to write.
pub struct DispatchOutcome {
    pub reply: Option<MarshalledMessage>,
    pub signals: Vec<MarshalledMessage>,
}

impl DispatchOutcome {
    fn reply_only(reply: MarshalledMessage) -> Self {
        Self {
            reply: Some(reply),
            signals: Vec::new(),
        }
    }

    fn empty() -> Self {
        Self {
            reply: None,
            signals: Vec::new(),
        }
    }
}

pub fn dispatch(
    call: &MarshalledMessage,
    queue: &mut Queue,
    default_timeout_ms: u32,
    now: Instant,
) -> DispatchOutcome {
    if call.typ != MessageType::Call {
        return DispatchOutcome::empty();
    }
    let Some(object) = call.dynheader.object.as_deref() else {
        return DispatchOutcome::empty();
    };
    if object != OBJECT_PATH {
        return DispatchOutcome::reply_only(unknown_object(call));
    }
    let Some(member) = call.dynheader.member.as_deref() else {
        return DispatchOutcome::empty();
    };
    let interface = call.dynheader.interface.as_deref();

    match (interface, member) {
        (Some(INTERFACE) | None, "GetCapabilities") => {
            DispatchOutcome::reply_only(get_capabilities(call))
        }
        (Some(INTERFACE) | None, "GetServerInformation") => {
            DispatchOutcome::reply_only(get_server_information(call))
        }
        (Some(INTERFACE) | None, "Notify") => notify(call, queue, default_timeout_ms, now),
        (Some(INTERFACE) | None, "CloseNotification") => close_notification(call, queue),
        (Some(INTROSPECTABLE_INTERFACE) | None, "Introspect") => {
            DispatchOutcome::reply_only(introspect(call))
        }
        _ => DispatchOutcome::reply_only(standard_messages::unknown_method(&call.dynheader)),
    }
}

fn get_capabilities(call: &MarshalledMessage) -> MarshalledMessage {
    let mut reply = call.dynheader.make_response();
    let caps: &[&str] = &["body", "persistence"];
    reply.body.push_param(caps).expect("push capabilities");
    reply
}

fn get_server_information(call: &MarshalledMessage) -> MarshalledMessage {
    let mut reply = call.dynheader.make_response();
    reply
        .body
        .push_param(env!("CARGO_PKG_NAME"))
        .and_then(|_| reply.body.push_param(VENDOR))
        .and_then(|_| reply.body.push_param(env!("CARGO_PKG_VERSION")))
        .and_then(|_| reply.body.push_param(SPEC_VERSION))
        .expect("push server info");
    reply
}

fn introspect(call: &MarshalledMessage) -> MarshalledMessage {
    let mut reply = call.dynheader.make_response();
    reply.body.push_param(INTROSPECTION_XML).expect("push xml");
    reply
}

fn notify(
    call: &MarshalledMessage,
    queue: &mut Queue,
    default_timeout_ms: u32,
    now: Instant,
) -> DispatchOutcome {
    let args = match parse_notify(call) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "rejecting malformed Notify");
            return DispatchOutcome::reply_only(invalid_args(call, &format!("{e:#}")));
        }
    };
    tracing::info!(
        app = %args.app_name,
        summary = %args.summary,
        body = %args.body,
        urgency = ?args.urgency,
        expire_ms = args.expire_timeout,
        replaces = args.replaces_id,
        "Notify"
    );
    let id = queue.notify(args, default_timeout_ms, now);
    let mut reply = call.dynheader.make_response();
    reply.body.push_param(id).expect("push id");
    DispatchOutcome::reply_only(reply)
}

fn close_notification(call: &MarshalledMessage, queue: &mut Queue) -> DispatchOutcome {
    let id: u32 = match call.body.parser().get() {
        Ok(v) => v,
        Err(e) => return DispatchOutcome::reply_only(invalid_args(call, &format!("{e:?}"))),
    };
    let removed = queue.close(id);
    let reply = call.dynheader.make_response();
    let mut signals = Vec::new();
    if removed {
        tracing::info!(id, "CloseNotification");
        signals.push(notification_closed_signal(id, CloseReason::ClosedByCall));
    } else {
        // Spec: silently succeed when the id is unknown.
        tracing::debug!(id, "CloseNotification (no such id)");
    }
    DispatchOutcome {
        reply: Some(reply),
        signals,
    }
}

pub fn notification_closed_signal(id: u32, reason: CloseReason) -> MarshalledMessage {
    let mut msg = MessageBuilder::new()
        .signal(INTERFACE, "NotificationClosed", OBJECT_PATH)
        .build();
    msg.body.push_param(id).expect("push id");
    msg.body.push_param(reason as u32).expect("push reason");
    msg
}

fn parse_notify(call: &MarshalledMessage) -> Result<NotifyArgs> {
    let mut p = call.body.parser();
    let (app_name, replaces_id, app_icon, summary, body) = p
        .get5::<&str, u32, &str, &str, &str>()
        .map_err(|e| anyhow!("first 5 args: {e:?}"))?;
    let _actions: Vec<&str> = p.get().map_err(|e| anyhow!("actions: {e:?}"))?;
    let hints: HashMap<String, Variant> = p.get().map_err(|e| anyhow!("hints: {e:?}"))?;
    let expire_timeout: i32 = p.get().map_err(|e| anyhow!("expire_timeout: {e:?}"))?;

    let urgency = hints
        .get("urgency")
        .and_then(|v| v.get::<u8>().ok())
        .map(Urgency::from_byte)
        .unwrap_or(Urgency::Normal);

    let image_path = hints.get("image-path").and_then(|v| v.get::<String>().ok());

    Ok(NotifyArgs {
        app_name: app_name.to_owned(),
        replaces_id,
        app_icon: app_icon.to_owned(),
        summary: summary.to_owned(),
        body: body.to_owned(),
        urgency,
        image_path,
        expire_timeout,
    })
}

fn invalid_args(call: &MarshalledMessage, msg: &str) -> MarshalledMessage {
    call.dynheader.make_error_response(
        "org.freedesktop.DBus.Error.InvalidArgs".to_owned(),
        Some(msg.to_owned()),
    )
}

fn unknown_object(call: &MarshalledMessage) -> MarshalledMessage {
    call.dynheader.make_error_response(
        "org.freedesktop.DBus.Error.UnknownObject".to_owned(),
        Some(format!(
            "no object at {}",
            call.dynheader.object.as_deref().unwrap_or("")
        )),
    )
}

const INTROSPECTION_XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.DBus.Introspectable">
    <method name="Introspect">
      <arg name="data" type="s" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.DBus.Peer">
    <method name="Ping"/>
    <method name="GetMachineId">
      <arg name="machine_uuid" type="s" direction="out"/>
    </method>
  </interface>
  <interface name="org.freedesktop.Notifications">
    <method name="GetCapabilities">
      <arg name="capabilities" type="as" direction="out"/>
    </method>
    <method name="GetServerInformation">
      <arg name="name" type="s" direction="out"/>
      <arg name="vendor" type="s" direction="out"/>
      <arg name="version" type="s" direction="out"/>
      <arg name="spec_version" type="s" direction="out"/>
    </method>
    <method name="Notify">
      <arg name="app_name" type="s" direction="in"/>
      <arg name="replaces_id" type="u" direction="in"/>
      <arg name="app_icon" type="s" direction="in"/>
      <arg name="summary" type="s" direction="in"/>
      <arg name="body" type="s" direction="in"/>
      <arg name="actions" type="as" direction="in"/>
      <arg name="hints" type="a{sv}" direction="in"/>
      <arg name="expire_timeout" type="i" direction="in"/>
      <arg name="id" type="u" direction="out"/>
    </method>
    <method name="CloseNotification">
      <arg name="id" type="u" direction="in"/>
    </method>
    <signal name="NotificationClosed">
      <arg name="id" type="u"/>
      <arg name="reason" type="u"/>
    </signal>
  </interface>
</node>
"#;
