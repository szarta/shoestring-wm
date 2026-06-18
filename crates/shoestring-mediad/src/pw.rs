//! PipeWire glue: the one place in the tree (besides the screencast portal)
//! that links libpipewire. Tracks the *default* audio sink/source and their
//! mute, exposes a computed [`Snapshot`], and can set a node's mute — all via
//! the PipeWire registry/metadata/node API (no WirePlumber `wpctl` CLI), so it
//! works under any session manager and degrades when none sets a default.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::Rc;

use anyhow::{anyhow, Result};
use libspa as spa;
use pipewire as pw;
use pw::loop_::Timeout;
use pw::types::ObjectType;
use spa::param::ParamType;
use spa::pod::deserialize::PodDeserializer;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Property, Value};

/// What we observe from PipeWire and report to the WM. Camera lives here too so
/// the monitor reports one atomic snapshot (filled in phase 3).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Snapshot {
    pub audio_muted: bool,
    pub mic_muted: bool,
    pub camera_active: bool,
}

/// Which default endpoint a control request targets.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Sink,
    Source,
}

/// One tracked audio node (a sink or source). We keep the proxy + listener
/// alive so PipeWire keeps delivering `Props` param updates (the mute state).
struct AudioNode {
    name: String,
    is_sink: bool,
    mute: Option<bool>,
    _proxy: pw::node::Node,
    _listener: pw::node::NodeListener,
}

/// One tracked camera source (`media.class = Video/Source`). `running` is true
/// while the node is actively streaming to a consumer — the "camera in use"
/// signal. Status only; we never mute/stop it.
struct CameraNode {
    running: bool,
    _proxy: pw::node::Node,
    _listener: pw::node::NodeListener,
}

/// Shared registry/metadata/node state. Mutated from PipeWire callbacks (all on
/// the single loop thread, so a `RefCell` is enough) and read to compute the
/// snapshot.
#[derive(Default)]
struct Inner {
    nodes: HashMap<u32, AudioNode>,
    cameras: HashMap<u32, CameraNode>,
    default_sink: Option<String>,
    default_source: Option<String>,
    last_snapshot: Option<Snapshot>,
}

impl Inner {
    fn snapshot(&self) -> Snapshot {
        let mute_of = |name: &Option<String>, want_sink: bool| -> bool {
            let Some(name) = name else { return false };
            self.nodes
                .values()
                .find(|n| n.is_sink == want_sink && &n.name == name)
                .and_then(|n| n.mute)
                .unwrap_or(false)
        };
        Snapshot {
            audio_muted: mute_of(&self.default_sink, true),
            mic_muted: mute_of(&self.default_source, false),
            camera_active: self.cameras.values().any(|c| c.running),
        }
    }

    /// Recompute the snapshot and, if it differs from the last reported one,
    /// record + return it (so the caller emits one `on_change`). Centralises the
    /// dedup shared by every callback.
    fn changed_snapshot(&mut self) -> Option<Snapshot> {
        let snap = self.snapshot();
        if self.last_snapshot != Some(snap) {
            self.last_snapshot = Some(snap);
            Some(snap)
        } else {
            None
        }
    }

    /// Node id backing the current default sink/source, if discovered.
    fn default_node_id(&self, kind: Kind) -> Option<u32> {
        let (name, want_sink) = match kind {
            Kind::Sink => (&self.default_sink, true),
            Kind::Source => (&self.default_source, false),
        };
        let name = name.as_ref()?;
        self.nodes
            .iter()
            .find(|(_, n)| n.is_sink == want_sink && &n.name == name)
            .map(|(id, _)| *id)
    }
}

/// A live PipeWire connection plus the registry tracker. Held for the lifetime
/// of a monitor run or a control oneshot.
pub struct Pw {
    main_loop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    _core: pw::core::CoreRc,
    _registry: pw::registry::RegistryRc,
    _registry_listener: pw::registry::Listener,
    inner: Rc<RefCell<Inner>>,
}

impl Pw {
    /// Connect to the PipeWire daemon and start tracking audio nodes + the
    /// default-device metadata. `on_change` fires (with a freshly computed,
    /// de-duplicated snapshot) whenever the observed state changes — the
    /// monitor wires it to a WM `ReportMedia`; the control oneshot passes a
    /// no-op. `pipewire::init()` must already have run.
    pub fn connect(on_change: Rc<dyn Fn(Snapshot)>) -> Result<Self> {
        let main_loop =
            pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow!("pw main loop: {e}"))?;
        let context = pw::context::ContextRc::new(&main_loop, None)
            .map_err(|e| anyhow!("pw context: {e}"))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| anyhow!("pw connect: {e}"))?;
        let registry = core
            .get_registry_rc()
            .map_err(|e| anyhow!("pw get_registry: {e}"))?;

        let inner: Rc<RefCell<Inner>> = Rc::new(RefCell::new(Inner::default()));

        // Hold the metadata *proxy* + listener alive for the connection's
        // lifetime — dropping the proxy tears down the binding and the property
        // replay never arrives. Keyed by global id so removal drops both.
        let metas: Rc<
            RefCell<HashMap<u32, (pw::metadata::Metadata, pw::metadata::MetadataListener)>>,
        > = Rc::new(RefCell::new(HashMap::new()));

        let registry_for_cb = registry.clone();
        let inner_for_cb = inner.clone();
        let metas_for_cb = metas.clone();
        let on_change_global = on_change.clone();
        let inner_for_remove = inner.clone();
        let metas_for_remove = metas.clone();
        let on_change_remove = on_change.clone();

        let registry_listener = registry
            .add_listener_local()
            .global(move |global| match global.type_ {
                ObjectType::Node => {
                    bind_node(&registry_for_cb, global, &inner_for_cb, &on_change_global)
                }
                ObjectType::Metadata => bind_metadata(
                    &registry_for_cb,
                    global,
                    &metas_for_cb,
                    &inner_for_cb,
                    &on_change_global,
                ),
                _ => {}
            })
            .global_remove(move |id| {
                metas_for_remove.borrow_mut().remove(&id);
                let report = {
                    let mut g = inner_for_remove.borrow_mut();
                    let removed = g.nodes.remove(&id).is_some() | g.cameras.remove(&id).is_some();
                    if removed {
                        g.changed_snapshot()
                    } else {
                        None
                    }
                };
                if let Some(snap) = report {
                    on_change_remove(snap);
                }
            })
            .register();

        Ok(Self {
            main_loop,
            _context: context,
            _core: core,
            _registry: registry,
            _registry_listener: registry_listener,
            inner,
        })
    }

    /// The PipeWire loop fd, for the monitor's `poll(2)` set.
    pub fn loop_fd(&self) -> RawFd {
        self.main_loop.loop_().fd().as_raw_fd()
    }

    /// Pump the loop once, non-blocking — call when [`Self::loop_fd`] is ready.
    pub fn iterate(&self) {
        self.main_loop.loop_().iterate(Timeout::None);
    }

    /// Pump the loop, blocking up to `timeout`, until `pred` holds or the
    /// deadline passes. Used by the control oneshot to wait for discovery.
    pub fn pump_until(&self, timeout: std::time::Duration, mut pred: impl FnMut(&Self) -> bool) {
        // Coarse wall-clock bound via repeated short blocking iterations. The
        // loop's own fd readiness drives progress; `Timeout` blocks until an
        // event or the slice elapses.
        let slices = (timeout.as_millis() / 50).max(1) as u32;
        for _ in 0..slices {
            if pred(self) {
                return;
            }
            self.main_loop
                .loop_()
                .iterate(Timeout::Finite(std::time::Duration::from_millis(50)));
        }
    }

    /// Current de-duplicated snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.inner.borrow().snapshot()
    }

    /// True once we have discovered the node backing the given default and read
    /// its mute at least once (so a `toggle` has a real value to flip).
    pub fn default_mute_known(&self, kind: Kind) -> bool {
        let g = self.inner.borrow();
        g.default_node_id(kind)
            .and_then(|id| g.nodes.get(&id))
            .map(|n| n.mute.is_some())
            .unwrap_or(false)
    }

    /// Read the current mute of the default sink/source, if known.
    pub fn default_mute(&self, kind: Kind) -> Option<bool> {
        let g = self.inner.borrow();
        let id = g.default_node_id(kind)?;
        g.nodes.get(&id)?.mute
    }

    /// Set the mute of the node backing the default sink/source. Errors if no
    /// default has been discovered (e.g. no session manager set one).
    pub fn set_default_mute(&self, kind: Kind, mute: bool) -> Result<()> {
        let g = self.inner.borrow();
        let id = g
            .default_node_id(kind)
            .ok_or_else(|| anyhow!("no default {kind:?} discovered"))?;
        let node = g
            .nodes
            .get(&id)
            .ok_or_else(|| anyhow!("default node vanished"))?;
        let bytes = props_mute_pod(mute);
        let pod = Pod::from_bytes(&bytes).ok_or_else(|| anyhow!("build mute pod"))?;
        node._proxy.set_param(ParamType::Props, 0, pod);
        Ok(())
    }
}

/// Bind a `Node` global if it's an audio sink/source, subscribe to its `Props`
/// param (the mute), and record it. Non-audio nodes are ignored here.
fn bind_node(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    inner: &Rc<RefCell<Inner>>,
    on_change: &Rc<dyn Fn(Snapshot)>,
) {
    let Some(props) = global.props else { return };
    let class = props.get("media.class").unwrap_or("");
    let is_sink = class == "Audio/Sink";
    let is_source = class == "Audio/Source";
    let is_camera = class == "Video/Source";
    if is_camera {
        bind_camera(registry, global, inner, on_change);
        return;
    }
    if !is_sink && !is_source {
        return;
    }
    let name = props.get("node.name").unwrap_or_default().to_string();
    tracing::debug!(id = global.id, %class, %name, "tracking audio node");

    let node: pw::node::Node = match registry.bind(global) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(id = global.id, error = %e, "bind node failed");
            return;
        }
    };
    node.subscribe_params(&[ParamType::Props]);

    let id = global.id;
    let inner_cb = inner.clone();
    let on_change_cb = on_change.clone();
    let listener = node
        .add_listener_local()
        .param(move |_seq, _ty, _index, _next, param| {
            let Some(pod) = param else { return };
            let Some(mute) = parse_mute(pod) else { return };
            let report = {
                let mut g = inner_cb.borrow_mut();
                let Some(n) = g.nodes.get_mut(&id) else {
                    return;
                };
                if n.mute == Some(mute) {
                    return;
                }
                n.mute = Some(mute);
                g.changed_snapshot()
            };
            if let Some(snap) = report {
                on_change_cb(snap);
            }
        })
        .register();

    inner.borrow_mut().nodes.insert(
        id,
        AudioNode {
            name,
            is_sink,
            mute: None,
            _proxy: node,
            _listener: listener,
        },
    );
}

/// Bind a `Video/Source` camera node and watch its `state` via the node `info`
/// event — `Running` means a consumer is actively pulling frames, i.e. the
/// camera is in use. Status only: we never stop it. Note this sees cameras that
/// go through PipeWire (the portal, browsers/Electron); a raw `/dev/video*`
/// open that bypasses PipeWire is not visible here.
fn bind_camera(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    inner: &Rc<RefCell<Inner>>,
    on_change: &Rc<dyn Fn(Snapshot)>,
) {
    let name = global
        .props
        .and_then(|p| p.get("node.name"))
        .unwrap_or_default()
        .to_string();
    tracing::debug!(id = global.id, %name, "tracking camera node");

    let node: pw::node::Node = match registry.bind(global) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(id = global.id, error = %e, "bind camera failed");
            return;
        }
    };

    let id = global.id;
    let inner_cb = inner.clone();
    let on_change_cb = on_change.clone();
    let listener = node
        .add_listener_local()
        .info(move |info| {
            let running = matches!(info.state(), pw::node::NodeState::Running);
            let report = {
                let mut g = inner_cb.borrow_mut();
                let Some(c) = g.cameras.get_mut(&id) else {
                    return;
                };
                if c.running == running {
                    return;
                }
                c.running = running;
                g.changed_snapshot()
            };
            if let Some(snap) = report {
                on_change_cb(snap);
            }
        })
        .register();

    inner.borrow_mut().cameras.insert(
        id,
        CameraNode {
            running: false,
            _proxy: node,
            _listener: listener,
        },
    );
}

/// Bind the `default` metadata object and watch the `default.audio.sink` /
/// `default.audio.source` keys so we know which node's mute to report.
fn bind_metadata(
    registry: &pw::registry::RegistryRc,
    global: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    metas: &Rc<RefCell<HashMap<u32, (pw::metadata::Metadata, pw::metadata::MetadataListener)>>>,
    inner: &Rc<RefCell<Inner>>,
    on_change: &Rc<dyn Fn(Snapshot)>,
) {
    // Only the "default" metadata carries default.audio.sink/source.
    if global.props.and_then(|p| p.get("metadata.name")) != Some("default") {
        return;
    }
    tracing::debug!(id = global.id, "tracking default-device metadata");
    let metadata: pw::metadata::Metadata = match registry.bind(global) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(id = global.id, error = %e, "bind metadata failed");
            return;
        }
    };
    let inner_cb = inner.clone();
    let on_change_cb = on_change.clone();
    let listener = metadata
        .add_listener_local()
        .property(move |_subject, key, _type, value| {
            let which = match key {
                Some("default.audio.sink") => Some(true),
                Some("default.audio.source") => Some(false),
                _ => None,
            };
            if let Some(is_sink) = which {
                let name = value.and_then(parse_default_name);
                tracing::debug!(is_sink, ?name, "default device updated");
                let report = {
                    let mut g = inner_cb.borrow_mut();
                    if is_sink {
                        g.default_sink = name;
                    } else {
                        g.default_source = name;
                    }
                    g.changed_snapshot()
                };
                if let Some(snap) = report {
                    on_change_cb(snap);
                }
            }
            0
        })
        .register();
    metas.borrow_mut().insert(global.id, (metadata, listener));
}

/// The default-device metadata value is JSON like `{"name":"alsa_output.…"}`.
/// Pull the `name`, which equals the target node's `node.name`.
fn parse_default_name(value: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    v.get("name")?.as_str().map(|s| s.to_string())
}

/// Parse a node `Props` pod and return its `mute` boolean if present.
fn parse_mute(pod: &Pod) -> Option<bool> {
    let (_, value) = PodDeserializer::deserialize_any_from(pod.as_bytes()).ok()?;
    let Value::Object(obj) = value else {
        return None;
    };
    obj.properties
        .iter()
        .find(|p| p.key == spa::sys::SPA_PROP_mute)
        .and_then(|p| match p.value {
            Value::Bool(b) => Some(b),
            _ => None,
        })
}

/// Build a `Props` pod carrying just `mute = <b>`, for `Node.set_param`.
fn props_mute_pod(mute: bool) -> Vec<u8> {
    let obj = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_Props,
        id: ParamType::Props.as_raw(),
        properties: vec![Property::new(spa::sys::SPA_PROP_mute, Value::Bool(mute))],
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serialize Props mute pod")
        .0
        .into_inner()
}
