//! System-tray (StatusNotifier) plumbing — **PLUMBING SPIKE** (task 149).
//!
//! Stands up `org.kde.StatusNotifierWatcher` + a `StatusNotifierHost` on the
//! session bus, accepts `RegisterStatusNotifierItem`, and logs each item — then
//! proves the two risky rustbus paths the real tray will lean on:
//!   1. an **outgoing method call** to the item (`Properties.Get`) + matching
//!      its reply, and
//!   2. a **signal subscription** (`AddMatch`) that delivers the item's
//!      `NewIcon`/`NewStatus` back to us.
//!
//! No rendering, no menus — those come in the follow-up. The point is to
//! de-risk the rustbus client+server mix before building UI on top.
//!
//! Wired into the bar's existing `libc::poll` loop exactly like
//! `shoestring-notify` does (see `feedback_rustbus_dispatch_loop`): poll the
//! D-Bus fd, then `refill_all` + drain `try_get_call` / `try_get_signal`.

use std::os::fd::{AsRawFd, RawFd};

use rustbus::connection::ll_conn::force_finish_on_error;
use rustbus::connection::Timeout;
use rustbus::message_builder::{MarshalledMessage, MessageBuilder, MessageType};
use rustbus::{peer, standard_messages, RpcConn};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH_DEFAULT: &str = "/StatusNotifierItem";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const INTROSPECT_IFACE: &str = "org.freedesktop.DBus.Introspectable";

/// Minimal introspection so KDE/Qt items (which introspect before registering)
/// see a real watcher interface.
const WATCHER_XML: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="org.kde.StatusNotifierWatcher">
  <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
  <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
  <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
  <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
  <property name="ProtocolVersion" type="i" access="read"/>
  <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
  <signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
  <signal name="StatusNotifierHostRegistered"/>
 </interface>
</node>"#;

#[derive(Debug, Clone)]
struct Item {
    /// Bus name that owns the item (e.g. `:1.42`).
    service: String,
    /// Object path of the item, usually `/StatusNotifierItem`.
    path: String,
}

pub struct Tray {
    rpc: RpcConn,
    items: Vec<Item>,
}

impl Tray {
    /// Connect to the session bus and become the StatusNotifierWatcher + host.
    /// Returns `None` (with a logged reason) if there's no session bus or some
    /// other process already owns the watcher — the bar then runs trayless.
    pub fn new() -> Option<Tray> {
        let mut rpc = match RpcConn::session_conn(Timeout::Infinite) {
            Ok(rpc) => rpc,
            Err(e) => {
                tracing::info!(error = ?e, "tray: no session bus; running without a tray");
                return None;
            }
        };

        // Own the watcher name. DO_NOT_QUEUE: if someone already hosts a tray
        // we don't fight them, we just bow out.
        match request_name(&mut rpc, WATCHER_NAME) {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!("tray: {WATCHER_NAME} already owned; running without a tray");
                return None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "tray: RequestName failed; running without a tray");
                return None;
            }
        }

        // Claim a host name too (advisory; items key off the watcher).
        let host = format!("org.kde.StatusNotifierHost-{}", std::process::id());
        let _ = request_name(&mut rpc, &host);

        // Subscribe to item-side signals so NewIcon/NewStatus/NewToolTip reach
        // us — the second risky path this spike exercises.
        let rule = format!("type='signal',interface='{ITEM_IFACE}'");
        if send(&mut rpc, &mut standard_messages::add_match(&rule)).is_err() {
            tracing::warn!("tray: AddMatch failed");
        }

        // Announce ourselves as a host so host-gated items (KStatusNotifierItem)
        // come out of hiding.
        let mut sig = MessageBuilder::new()
            .signal(WATCHER_IFACE, "StatusNotifierHostRegistered", WATCHER_PATH)
            .build();
        let _ = send(&mut rpc, &mut sig);

        tracing::info!("tray: StatusNotifierWatcher up ({host})");
        Some(Tray {
            rpc,
            items: Vec::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.rpc.conn().as_raw_fd()
    }

    /// Drain the D-Bus socket: answer watcher calls, log item registrations and
    /// signals. Call when the tray fd polls readable.
    pub fn dispatch(&mut self) {
        // refill_all drains the socket and hands back auto-synthesised
        // UnknownMethod replies for filtered calls — we must actually send them.
        match self.rpc.refill_all() {
            Ok(mut unhandled) => {
                for msg in &mut unhandled {
                    let _ = send(&mut self.rpc, msg);
                }
            }
            Err(e) => {
                tracing::warn!(error = ?e, "tray: refill_all failed");
                return;
            }
        }

        while let Some(call) = self.rpc.try_get_call() {
            // Ping / GetMachineId handled for us.
            if matches!(
                peer::handle_peer_message(&call, self.rpc.conn_mut()),
                Ok(true)
            ) {
                continue;
            }
            self.handle_call(&call);
        }

        while let Some(sig) = self.rpc.try_get_signal() {
            // We get all signals the bus delivers (NameAcquired, etc.); only the
            // item interface is interesting here (NewIcon/NewStatus/NewToolTip).
            if sig.dynheader.interface.as_deref() != Some(ITEM_IFACE) {
                continue;
            }
            let member = sig.dynheader.member.as_deref().unwrap_or("?");
            let sender = sig.dynheader.sender.as_deref().unwrap_or("?");
            tracing::info!(%member, %sender, "tray: item signal");
        }
    }

    fn handle_call(&mut self, call: &MarshalledMessage) {
        if call.typ != MessageType::Call {
            return;
        }
        let member = call.dynheader.member.as_deref().unwrap_or_default();
        let iface = call.dynheader.interface.as_deref();

        match (iface, member) {
            (Some(WATCHER_IFACE) | None, "RegisterStatusNotifierItem") => {
                self.register_item(call);
            }
            (Some(WATCHER_IFACE) | None, "RegisterStatusNotifierHost") => {
                let _ = self.send(call.dynheader.make_response());
                let sig = MessageBuilder::new()
                    .signal(WATCHER_IFACE, "StatusNotifierHostRegistered", WATCHER_PATH)
                    .build();
                let _ = self.send(sig);
            }
            (Some(PROPS_IFACE), "Get") => self.prop_get(call),
            (Some(INTROSPECT_IFACE), "Introspect") => {
                let mut reply = call.dynheader.make_response();
                let _ = reply.body.push_param(WATCHER_XML);
                let _ = self.send(reply);
            }
            _ => {
                let _ = self.send(standard_messages::unknown_method(&call.dynheader));
            }
        }
    }

    fn register_item(&mut self, call: &MarshalledMessage) {
        let arg: String = call.body.parser().get().unwrap_or_default();
        // The argument is either a bus name (item at the default path) or, from
        // some toolkits, an object path (item on the caller's bus name).
        let (service, path) = if arg.starts_with('/') {
            (
                call.dynheader.sender.clone().unwrap_or_default(),
                arg.clone(),
            )
        } else if arg.is_empty() {
            (
                call.dynheader.sender.clone().unwrap_or_default(),
                ITEM_PATH_DEFAULT.to_string(),
            )
        } else {
            (arg.clone(), ITEM_PATH_DEFAULT.to_string())
        };

        tracing::info!(%service, %path, "tray: item registered");
        self.items.push(Item {
            service: service.clone(),
            path: path.clone(),
        });

        // Reply, then broadcast the registration.
        let _ = self.send(call.dynheader.make_response());
        let mut sig = MessageBuilder::new()
            .signal(WATCHER_IFACE, "StatusNotifierItemRegistered", WATCHER_PATH)
            .build();
        let _ = sig.body.push_param(format!("{service}{path}"));
        let _ = self.send(sig);

        // De-risk the outgoing-call path: pull a couple of properties off the
        // freshly registered item and log them.
        for prop in ["Id", "Title", "Status", "IconName"] {
            match self.item_get_string(&service, &path, prop) {
                Some(val) => tracing::info!(%service, prop, value = %val, "tray: item property"),
                None => tracing::debug!(%service, prop, "tray: item property unavailable"),
            }
        }
    }

    /// `org.kde.StatusNotifierWatcher` read-only properties.
    fn prop_get(&mut self, call: &MarshalledMessage) {
        let mut p = call.body.parser();
        let _iface: String = p.get().unwrap_or_default();
        let prop: String = p.get().unwrap_or_default();
        let mut reply = call.dynheader.make_response();
        match prop.as_str() {
            "RegisteredStatusNotifierItems" => {
                let names: Vec<String> = self
                    .items
                    .iter()
                    .map(|i| format!("{}{}", i.service, i.path))
                    .collect();
                let _ = reply.body.push_variant(names);
            }
            "IsStatusNotifierHostRegistered" => {
                let _ = reply.body.push_variant(true);
            }
            "ProtocolVersion" => {
                let _ = reply.body.push_variant(0i32);
            }
            _ => {
                let _ = self.send(standard_messages::unknown_method(&call.dynheader));
                return;
            }
        }
        let _ = self.send(reply);
    }

    /// Blocking `Properties.Get` against an item, returning the value as a
    /// string when it is one (spike-grade: just enough to prove the round-trip
    /// and surface icon/status names in the log).
    fn item_get_string(&mut self, service: &str, path: &str, prop: &str) -> Option<String> {
        let mut call = MessageBuilder::new()
            .call("Get")
            .at(service.to_string())
            .on(path.to_string())
            .with_interface(PROPS_IFACE)
            .build();
        call.body.push_param(ITEM_IFACE).ok()?;
        call.body.push_param(prop).ok()?;
        let serial = self.rpc.send_message(&mut call).ok()?.write_all().ok()?;
        let resp = self
            .rpc
            .wait_response(
                serial,
                Timeout::Duration(std::time::Duration::from_millis(500)),
            )
            .ok()?;
        if resp.typ == MessageType::Error {
            return None;
        }
        resp.body
            .parser()
            .get::<rustbus::wire::unmarshal::traits::Variant>()
            .ok()
            .and_then(|v| v.get::<String>().ok())
    }

    fn send(&mut self, mut msg: MarshalledMessage) -> Result<(), ()> {
        send(&mut self.rpc, &mut msg)
    }
}

/// `RequestName` with DO_NOT_QUEUE; `Ok(true)` iff we became the primary owner.
fn request_name(rpc: &mut RpcConn, name: &str) -> Result<bool, String> {
    let flags = standard_messages::DBUS_NAME_FLAG_DO_NOT_QUEUE;
    let serial = rpc
        .send_message(&mut standard_messages::request_name(name, flags))
        .map_err(|e| format!("send RequestName: {e:?}"))?
        .write_all()
        .map_err(|(_, e)| format!("write RequestName: {e:?}"))?;
    let resp = rpc
        .wait_response(serial, Timeout::Infinite)
        .map_err(|e| format!("await RequestName: {e:?}"))?;
    let code: u32 = resp
        .body
        .parser()
        .get()
        .map_err(|e| format!("parse RequestName: {e:?}"))?;
    Ok(
        code == standard_messages::DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER
            || code == standard_messages::DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER,
    )
}

fn send(rpc: &mut RpcConn, msg: &mut MarshalledMessage) -> Result<(), ()> {
    match rpc.send_message(msg) {
        Ok(ctx) => ctx
            .write_all()
            .map(|_| ())
            .map_err(force_finish_on_error)
            .map_err(|_| ()),
        Err(_) => Err(()),
    }
}
