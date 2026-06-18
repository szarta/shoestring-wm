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

use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use rustbus::connection::ll_conn::force_finish_on_error;
use rustbus::connection::Timeout;
use rustbus::message_builder::{MarshalledMessage, MessageBuilder, MessageType};
use rustbus::wire::unmarshal::traits::Variant;
use rustbus::{peer, standard_messages, RpcConn};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH_DEFAULT: &str = "/StatusNotifierItem";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const INTROSPECT_IFACE: &str = "org.freedesktop.DBus.Introspectable";
/// Context-menu protocol exposed by appindicator/ayatana items (nm-applet,
/// blueman): the item's `Menu` property is an object path implementing this.
const DBUSMENU_IFACE: &str = "com.canonical.dbusmenu";

/// One node of a `com.canonical.dbusmenu` layout, decoded into an owned tree
/// for rendering. The root node's `children` are the top-level entries.
#[derive(Debug, Clone)]
pub struct MenuNode {
    /// dbusmenu item id, echoed back in `Event`/`AboutToShow`.
    pub id: i32,
    /// Display label, mnemonic underscores stripped.
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    /// `type == "separator"` — drawn as a divider, never selectable.
    pub separator: bool,
    /// `children-display == "submenu"` — selecting it descends a level.
    pub has_submenu: bool,
    /// `Some(checked)` for checkmark/radio items; `None` for plain items.
    pub toggle: Option<bool>,
    pub children: Vec<MenuNode>,
}

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
    /// `com.canonical.dbusmenu` object path from the item's `Menu` property,
    /// if it exposes one. Drives the click-to-open context menu.
    menu_path: Option<String>,
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
        // The `Menu` property is a D-Bus object path (sig 'o', not 's'); a
        // missing/empty/"/" value means the item has no context menu.
        let menu_path = self
            .item_get_objpath(&service, &path, "Menu")
            .filter(|p| !p.is_empty() && p != "/");
        tracing::info!(%service, %id, %icon_name, has_menu = menu_path.is_some(), "tray: item registered");

        // Replace any prior registration from the same service/path.
        self.items
            .retain(|i| !(i.service == service && i.path == path));
        self.items.push(Item {
            service,
            path,
            icon_name,
            theme_dirs,
            menu_path,
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

    /// Visible tray items (those with a decoded icon) as `(item_index, &Icon)`
    /// in registration order. Same set/order as [`Tray::icons`], but carrying
    /// the index so the bar can map a click back to the item for menu actions.
    pub fn visible_items(&self) -> impl Iterator<Item = (usize, &crate::icons::Icon)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| it.icon.as_ref().map(|ic| (i, ic)))
    }

    /// Does the item at `item_idx` expose a context menu?
    pub fn has_menu(&self, item_idx: usize) -> bool {
        self.items
            .get(item_idx)
            .is_some_and(|i| i.menu_path.is_some())
    }

    /// Fetch + decode the item's top-level menu (root node; its `children` are
    /// the top-level entries). Blocking; `None` if the item has no menu or the
    /// call fails/times out.
    /// Fetch the entries to display at `path` (a list of submenu *labels* from
    /// the root). Returns the children at that level.
    ///
    /// Navigation is by **label, not id**: appindicator items (nm-applet) rebuild
    /// their menu and reassign ids on every `AboutToShow` / Wi-Fi rescan, so a
    /// cached id goes stale within seconds (`GetLayout(stale_id)` → libdbusmenu
    /// "id not found"). We instead re-fetch the whole tree (`GetLayout(0,-1)`),
    /// re-derive the current id for each level by matching its label, and
    /// `AboutToShow` that fresh id to populate the next level. Heavier (a few
    /// round-trips per descent) but immune to id churn.
    pub fn fetch_level(&mut self, item_idx: usize, path: &[String]) -> Option<Vec<MenuNode>> {
        let mut root = self.get_layout(item_idx, 0)?;
        if path.is_empty() {
            return Some(root.children);
        }
        for depth in 1..=path.len() {
            // Re-derive this level's id by label from the freshly-fetched tree
            // (ids churn), then descend.
            let sid = find_by_path(&root, &path[..depth])
                .filter(|n| n.has_submenu)
                .map(|n| n.id)?;
            if depth == path.len() {
                // Deepest level: fetch the submenu directly by its current id —
                // GetLayout on a *fresh* id returns its (now-populated) children.
                return self.get_layout(item_idx, sid).map(|n| n.children);
            }
            self.about_to_show_id(item_idx, sid);
            root = self.get_layout(item_idx, 0)?;
        }
        None
    }

    /// `AboutToShow(id)` against the item's menu, resolving service/path from
    /// `item_idx`. Best-effort (errors ignored — it only prompts population).
    fn about_to_show_id(&mut self, item_idx: usize, id: i32) {
        let Some(item) = self.items.get(item_idx) else {
            return;
        };
        let Some(menu_path) = item.menu_path.clone() else {
            return;
        };
        let service = item.service.clone();
        self.about_to_show(&service, &menu_path, id);
    }

    /// `AboutToShow(parent_id)` then `GetLayout(parent_id, -1, [])`, decoding the
    /// reply `(u revision, (i a{sv} av) layout)` into an owned [`MenuNode`].
    fn get_layout(&mut self, item_idx: usize, parent_id: i32) -> Option<MenuNode> {
        let item = self.items.get(item_idx)?;
        let service = item.service.clone();
        let menu_path = item.menu_path.clone()?;

        // Prompt (lazy) population of this (sub)menu before reading it.
        self.about_to_show(&service, &menu_path, parent_id);

        let mut call = MessageBuilder::new()
            .call("GetLayout")
            .at(service)
            .on(menu_path)
            .with_interface(DBUSMENU_IFACE)
            .build();
        call.body.push_param(parent_id).ok()?;
        call.body.push_param(-1i32).ok()?;
        call.body.push_param(Vec::<String>::new()).ok()?;
        let serial = match self
            .rpc
            .send_message(&mut call)
            .and_then(|c| c.write_all().map_err(|(_, e)| e))
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(parent_id, error = ?e, "GetLayout send failed");
                return None;
            }
        };
        let resp = match self
            .rpc
            .wait_response(serial, Timeout::Duration(Duration::from_millis(2000)))
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(parent_id, error = ?e, "GetLayout wait_response failed");
                return None;
            }
        };
        if resp.typ == MessageType::Error {
            tracing::warn!(
                parent_id,
                name = ?resp.dynheader.error_name,
                "GetLayout returned D-Bus error"
            );
            return None;
        }
        let mut p = resp.body.parser();
        let _revision: u32 = p.get().ok()?;
        let layout: (i32, HashMap<String, Variant>, Vec<Variant>) = match p.get() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(parent_id, error = ?e, "GetLayout parse failed");
                return None;
            }
        };
        Some(parse_menu_node(layout))
    }

    /// Tell the item we're about to show a (sub)menu so it can refresh it.
    /// Best-effort: we wait briefly for the reply but ignore its `needUpdate`.
    fn about_to_show(&mut self, service: &str, path: &str, id: i32) {
        let mut call = MessageBuilder::new()
            .call("AboutToShow")
            .at(service.to_string())
            .on(path.to_string())
            .with_interface(DBUSMENU_IFACE)
            .build();
        if call.body.push_param(id).is_err() {
            return;
        }
        let serial = {
            let Ok(ctx) = self.rpc.send_message(&mut call) else {
                return;
            };
            let Ok(s) = ctx.write_all() else { return };
            s
        };
        let _ = self
            .rpc
            .wait_response(serial, Timeout::Duration(Duration::from_millis(400)));
    }

    /// Send a `clicked` Event for dbusmenu item `id` of tray item `item_idx`.
    /// This is what makes a menu entry actually do its thing.
    pub fn menu_clicked(&mut self, item_idx: usize, id: i32) {
        let Some(item) = self.items.get(item_idx) else {
            return;
        };
        let Some(menu_path) = item.menu_path.clone() else {
            return;
        };
        let service = item.service.clone();
        // Event(id i, eventId s, data v, timestamp u)
        let mut call = MessageBuilder::new()
            .call("Event")
            .at(service)
            .on(menu_path)
            .with_interface(DBUSMENU_IFACE)
            .build();
        let _ = call.body.push_param(id);
        let _ = call.body.push_param("clicked");
        let _ = call.body.push_variant(0i32);
        let _ = call.body.push_param(0u32);
        // Drain the (empty) reply so it doesn't accumulate in the response map.
        let serial = {
            let Ok(ctx) = self.rpc.send_message(&mut call) else {
                return;
            };
            let Ok(s) = ctx.write_all() else { return };
            s
        };
        let _ = self
            .rpc
            .wait_response(serial, Timeout::Duration(Duration::from_millis(300)));
    }

    /// `org.kde.StatusNotifierItem.Activate(x, y)` — the left-click default for
    /// items with no menu (some toggle visibility / raise a window). Best-effort.
    pub fn activate(&mut self, item_idx: usize, x: i32, y: i32) {
        let Some(item) = self.items.get(item_idx) else {
            return;
        };
        let (service, path) = (item.service.clone(), item.path.clone());
        let mut call = MessageBuilder::new()
            .call("Activate")
            .at(service)
            .on(path)
            .with_interface(ITEM_IFACE)
            .build();
        let _ = call.body.push_param(x);
        let _ = call.body.push_param(y);
        let _ = self.send(call);
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

    /// Like [`Tray::item_get_string`] but for a D-Bus *object path* property
    /// (signature `o`, e.g. the item's `Menu`). A `String`-typed get would
    /// reject the variant on signature mismatch, so we read `ObjectPath`.
    fn item_get_objpath(&mut self, service: &str, path: &str, prop: &str) -> Option<String> {
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
            .wait_response(serial, Timeout::Duration(Duration::from_millis(500)))
            .ok()?;
        if resp.typ == MessageType::Error {
            return None;
        }
        resp.body
            .parser()
            .get::<Variant>()
            .ok()
            .and_then(|v| v.get::<rustbus::wire::ObjectPath<String>>().ok())
            .map(|op| op.as_ref().to_string())
    }

    fn send(&mut self, mut msg: MarshalledMessage) -> Result<(), ()> {
        send(&mut self.rpc, &mut msg)
    }
}

/// Walk `root` down a path of submenu *labels*, returning the node at that
/// level (`root` itself for an empty path). Matches submenu children by label
/// so it survives dbusmenu id reassignment.
fn find_by_path<'a>(root: &'a MenuNode, path: &[String]) -> Option<&'a MenuNode> {
    let mut node = root;
    for label in path {
        node = node
            .children
            .iter()
            .find(|c| c.has_submenu && &c.label == label)?;
    }
    Some(node)
}

/// Decode one `(i a{sv} av)` dbusmenu layout node (and its descendants) into an
/// owned [`MenuNode`]. Children are variant-wrapped structs of the same shape.
fn parse_menu_node(node: (i32, HashMap<String, Variant>, Vec<Variant>)) -> MenuNode {
    let (id, props, children) = node;
    let get_str = |k: &str| props.get(k).and_then(|v| v.get::<String>().ok());
    let get_bool = |k: &str, default: bool| {
        props
            .get(k)
            .and_then(|v| v.get::<bool>().ok())
            .unwrap_or(default)
    };

    let separator = get_str("type").as_deref() == Some("separator");
    let has_submenu = get_str("children-display").as_deref() == Some("submenu");
    let toggle = match get_str("toggle-type").as_deref() {
        Some("") | None => None,
        Some(_) => {
            let st = props
                .get("toggle-state")
                .and_then(|v| v.get::<i32>().ok())
                .unwrap_or(0);
            Some(st > 0)
        }
    };
    let kids = children
        .into_iter()
        .filter_map(|cv| {
            cv.get::<(i32, HashMap<String, Variant>, Vec<Variant>)>()
                .ok()
                .map(parse_menu_node)
        })
        .collect();

    MenuNode {
        id,
        label: strip_mnemonics(&get_str("label").unwrap_or_default()),
        enabled: get_bool("enabled", true),
        visible: get_bool("visible", true),
        separator,
        has_submenu,
        toggle,
        children: kids,
    }
}

/// Strip GTK-style mnemonic markers: a lone `_` flags the next char as an
/// accelerator and is dropped; `__` is a literal underscore.
fn strip_mnemonics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
            // otherwise: drop the marker
        } else {
            out.push(c);
        }
    }
    out
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
