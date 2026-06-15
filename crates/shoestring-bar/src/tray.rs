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

struct Item {
    /// Bus name that owns the item (e.g. `:1.42`).
    service: String,
    /// Object path of the item, usually `/StatusNotifierItem`.
    path: String,
    /// Latest `IconName`; resolved against the icon themes for rendering.
    icon_name: String,
    /// Item-private icon dirs from `IconThemePath`, searched first.
    theme_dirs: Vec<std::path::PathBuf>,
    /// Decoded icon, lazily filled by [`Tray::ensure_icons`] at the bar's
    /// current pixel size; `icon_px` records that size so a bar resize
    /// re-decodes.
    icon: Option<crate::icons::Icon>,
    icon_px: u16,
}

pub struct Tray {
    rpc: RpcConn,
    items: Vec<Item>,
    theme: crate::icons::IconTheme,
    /// Set when the item set / an icon changed; [`Tray::dispatch`] returns and
    /// clears it so the bar knows to repaint.
    dirty: bool,
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

        // Subscribe to item-side signals (NewIcon/NewStatus/NewToolTip) and to
        // NameOwnerChanged so we can drop an item when its app exits.
        for rule in [
            format!("type='signal',interface='{ITEM_IFACE}'"),
            "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'".to_string(),
        ] {
            if send(&mut rpc, &mut standard_messages::add_match(&rule)).is_err() {
                tracing::warn!("tray: AddMatch failed");
            }
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
            theme: crate::icons::IconTheme::detect(),
            dirty: false,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.rpc.conn().as_raw_fd()
    }

    /// Drain the D-Bus socket: answer watcher calls, track item registrations,
    /// removals, and icon changes. Returns `true` if the rendered set changed
    /// (so the bar repaints). Call when the tray fd polls readable.
    #[must_use]
    pub fn dispatch(&mut self) -> bool {
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
                return false;
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
            let iface = sig.dynheader.interface.as_deref().unwrap_or_default();
            let member = sig.dynheader.member.as_deref().unwrap_or_default();
            // An item's owner left the bus → drop it.
            if iface == "org.freedesktop.DBus" && member == "NameOwnerChanged" {
                let mut p = sig.body.parser();
                let name: String = p.get().unwrap_or_default();
                let _old: String = p.get().unwrap_or_default();
                let new_owner: String = p.get().unwrap_or_default();
                if new_owner.is_empty() {
                    let before = self.items.len();
                    self.items.retain(|it| it.service != name);
                    if self.items.len() != before {
                        tracing::info!(%name, "tray: item gone");
                        self.dirty = true;
                    }
                }
                continue;
            }
            // The item changed its icon → re-resolve.
            if iface == ITEM_IFACE {
                let sender = sig.dynheader.sender.clone().unwrap_or_default();
                match member {
                    "NewIcon" | "NewToolTip" => self.refresh_icon(&sender),
                    "NewStatus" => self.dirty = true,
                    _ => {}
                }
            }
        }

        std::mem::take(&mut self.dirty)
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

        // Reply, then broadcast the registration.
        let _ = self.send(call.dynheader.make_response());
        let mut sig = MessageBuilder::new()
            .signal(WATCHER_IFACE, "StatusNotifierItemRegistered", WATCHER_PATH)
            .build();
        let _ = sig.body.push_param(format!("{service}{path}"));
        let _ = self.send(sig);

        // Pull the item's identity + icon hints over an outgoing call. The icon
        // is decoded lazily in `ensure_icons` at the bar's current pixel size.
        let id = self
            .item_get_string(&service, &path, "Id")
            .unwrap_or_default();
        let icon_name = self
            .item_get_string(&service, &path, "IconName")
            .unwrap_or_default();
        let theme_dirs: Vec<std::path::PathBuf> = self
            .item_get_string(&service, &path, "IconThemePath")
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        tracing::info!(%service, %id, %icon_name, "tray: item registered");

        // Replace any prior registration from the same service/path.
        self.items
            .retain(|i| !(i.service == service && i.path == path));
        self.items.push(Item {
            service,
            path,
            icon_name,
            theme_dirs,
            icon: None,
            icon_px: 0,
        });
        self.dirty = true;
    }

    /// Decode any item icons that are missing or sized for a different bar
    /// height, at `px`. Cheap after the first pass (cached per item+size).
    pub fn ensure_icons(&mut self, px: u16) {
        let theme = &self.theme;
        for item in &mut self.items {
            if item.icon_px == px && (item.icon.is_some() || item.icon_name.is_empty()) {
                continue;
            }
            item.icon = if item.icon_name.is_empty() {
                None
            } else {
                theme
                    .lookup(&item.icon_name, px, &item.theme_dirs)
                    .and_then(|p| crate::icons::decode(&p, px))
            };
            item.icon_px = px;
        }
    }

    /// Decoded item icons in registration order, for the bar to blit.
    pub fn icons(&self) -> impl Iterator<Item = &crate::icons::Icon> {
        self.items.iter().filter_map(|i| i.icon.as_ref())
    }

    /// Re-fetch the icon name for the item owned by `service` and drop its
    /// cached pixels so the next `ensure_icons` re-resolves (e.g. on `NewIcon`).
    fn refresh_icon(&mut self, service: &str) {
        let Some(i) = self.items.iter().position(|it| it.service == service) else {
            return;
        };
        let (svc, path) = (self.items[i].service.clone(), self.items[i].path.clone());
        let name = self
            .item_get_string(&svc, &path, "IconName")
            .unwrap_or_default();
        self.items[i].icon_name = name;
        self.items[i].icon = None;
        self.items[i].icon_px = 0;
        self.dirty = true;
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
