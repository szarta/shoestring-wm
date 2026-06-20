//! Native DRM/KMS + libinput + libseat backend, for running shoestring-wm
//! from a TTY without an outer Wayland/X11 session.
//!
//! Aggressively stripped vs. anvil's reference:
//! - single primary GPU only (no multi-GPU fallback paths)
//! - per-output scale via `[outputs.<name>].scale`; falls back to `general.output_scale`
//! - no dmabuf-feedback, DRM lease
//! - explicit sync (`wp_linux_drm_syncobj_v1`) and wlr-screencopy *are* wired
//! - no cursor plane (cursor sprite is composited into the framebuffer
//!   from an xcursor theme; see [`crate::cursor`] and [`crate::drawing`])
//! - no FPS overlay, no presentation-throttle heuristics
//! - 8-bit color only
//!
//! When the user adds a connector at runtime we get a udev `Changed` event,
//! re-scan, and bring up an [`Output`] + a [`DrmOutput`] for it. Each [`DrmOutput`]
//! drives one CRTC via [`DrmOutputManager`], submitting frames on a calloop timer
//! and getting woken back up by the DRM event source on VBlank.
//!
//! [`Output`]: smithay::output::Output
//! [`DrmOutput`]: smithay::backend::drm::output::DrmOutput

use std::{collections::HashMap, path::Path, time::Duration};

// Bring acquire_master_lock() into scope so we can reclaim DRM master
// directly on the fd after a VT switch, even in unprivileged (logind-managed)
// mode.  After the initial ResumeDevice from logind, was_master is set on
// the file description, so SET_MASTER succeeds again without CAP_SYS_ADMIN.
use smithay::reexports::drm::{control::Device as DrmControlDevice, Device as DrmBasicDevice};

use anyhow::Result;
use smithay::{
    backend::{
        allocator::{
            dmabuf::Dmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::{FrameError, FrameFlags, RenderFrameError},
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            CreateDrmNodeError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmEventMetadata,
            DrmEventTime, DrmNode, NodeType, VrrSupport,
        },
        egl::{self, context::ContextPriority, EGLContext, EGLDevice, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::surface::WaylandSurfaceRenderElement,
            gles::{Capability, GlesRenderer},
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiRenderer},
            ImportDma,
        },
        session::{
            libseat::{self, LibSeatSession},
            Event as SessionEvent, Session,
        },
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
    },
    desktop::{space::SpaceRenderElements, utils::OutputPresentationFeedback},
    output::{Mode as WlMode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop, RegistrationToken,
        },
        drm::control::{connector, crtc, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::DeviceFd,
    wayland::drm_syncobj::{supports_syncobj_eventfd, DrmSyncobjHandler, DrmSyncobjState},
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};

use crate::state::ShoestringWm;

/// Color formats we'll accept for scanout. Keep this 8-bit only — 10-bit
/// works on most modern hardware but we'd rather not chase format-negotiation
/// regressions on first boot.
const SUPPORTED_FORMATS: &[Fourcc] = &[Fourcc::Abgr8888, Fourcc::Argb8888];

type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// Per-frame user-data we hang on each queued frame: the presentation
/// feedback collected at render time, returned by `frame_submitted` on the
/// matching VBlank so we can mark it `presented` with the hardware timestamp.
type FrameUserData = Option<OutputPresentationFeedback>;

type ShoestringDrmOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    FrameUserData,
    DrmDeviceFd,
>;

type ShoestringOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    FrameUserData,
    DrmDeviceFd,
>;

/// User-data tag we hang on each [`Output`] so VBlank handlers can find the
/// [`SurfaceData`] that owns the crtc that just flipped. Also used by
/// screencopy to wake the right CRTC when a capture is pending.
#[derive(Debug, PartialEq)]
pub(crate) struct UdevOutputId {
    pub(crate) device_id: DrmNode,
    pub(crate) crtc: crtc::Handle,
}

pub struct UdevData {
    pub session: LibSeatSession,
    /// libinput context — stored here so the VT watchdog can call resume()
    /// without needing a closure-captured copy.
    pub libinput: smithay::reexports::input::Libinput,
    /// Every currently-connected libinput device, tracked from
    /// `DeviceAdded`/`DeviceRemoved`. libinput has no device enumeration, so
    /// we keep this list to re-apply `[input]` settings on config hot-reload.
    /// Devices are ref-counted handles; a clone points at the same device.
    pub devices: Vec<smithay::reexports::input::Device>,
    /// Our own session-active flag.  Set true at startup and by every
    /// successful activation (ActivateSession handler or VT watchdog), set
    /// false in PauseSession.  Using this instead of session.is_active() lets
    /// the watchdog path mark the session active before libseat's AtomicBool
    /// is updated.
    pub vt_active: bool,
    primary_gpu: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, BackendData>,
    /// `wp_linux_drm_syncobj_v1` (explicit sync) delegate. Stood up once, on
    /// the first GPU that supports `drmSyncobjEventfd` (see
    /// [`supports_syncobj_eventfd`]); `None` until then or if unsupported.
    /// Lets XWayland 23.2+ / Wine / Proton hand us GPU fences instead of
    /// relying on implicit sync — fixes tearing/stutter in GL/Vulkan apps.
    syncobj_state: Option<DrmSyncobjState>,
}

impl UdevData {
    /// Try to import `dmabuf` into the primary GPU's renderer. Backs
    /// [`crate::state::ShoestringWm::import_dmabuf_test`] — the validation the
    /// `zwp_linux_dmabuf_v1` protocol runs before creating a wl_buffer.
    pub fn import_dmabuf_test(&mut self, dmabuf: &Dmabuf) -> bool {
        match self.gpus.single_renderer(&self.primary_gpu) {
            Ok(mut renderer) => renderer.import_dmabuf(dmabuf, None).is_ok(),
            Err(err) => {
                tracing::warn!(?err, "dmabuf import test: no renderer for primary gpu");
                false
            }
        }
    }
}

/// Explicit-sync (`wp_linux_drm_syncobj_v1`) routes through the udev backend:
/// the delegate lives in [`UdevData`] because it owns the importing DRM device.
/// The acquire-fence commit blocker is wired in [`crate::handlers::compositor`].
impl DrmSyncobjHandler for ShoestringWm {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.udev.as_mut().and_then(|u| u.syncobj_state.as_mut())
    }
}

struct BackendData {
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    drm_output_manager: ShoestringOutputManager,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

struct SurfaceData {
    output: Output,
    /// Held until drop so we can `remove_global` symmetrically.
    global: Option<GlobalId>,
    drm_output: ShoestringDrmOutput,
    /// Whether VRR/adaptive-sync is enabled for this surface. Resolved once at
    /// connect time (config opt-in ∧ connector support). When `true` we
    /// re-assert it before each `render_frame` — `use_vrr` is idempotent, so
    /// this is cheap and guards against any pending-state reset across mode
    /// changes or VT switches.
    vrr: bool,
}

#[derive(Debug, thiserror::Error)]
enum DeviceAddError {
    #[error("libseat could not open the device: {0}")]
    DeviceOpen(libseat::Error),
    #[error("DrmDevice init failed: {0}")]
    DrmDevice(DrmError),
    #[error("GbmDevice init failed: {0}")]
    GbmDevice(std::io::Error),
    #[error("DrmNode resolution failed: {0}")]
    DrmNode(CreateDrmNodeError),
    #[error("Could not register render node with GpuManager: {0}")]
    AddNode(egl::Error),
    #[error("Device has no usable render node")]
    NoRenderNode,
}

/// Entry point. Wires every event source onto `event_loop` and stashes
/// [`UdevData`] on `state.udev`. Returns once everything is registered;
/// the actual rendering happens inside the loop.
pub fn init_udev(event_loop: &mut EventLoop<ShoestringWm>, state: &mut ShoestringWm) -> Result<()> {
    let (session, session_notifier) =
        LibSeatSession::new().map_err(|e| anyhow::anyhow!("libseat session init failed: {e}"))?;
    let seat_name = session.seat();
    tracing::info!(%seat_name, "libseat session acquired");

    // Pick the primary GPU. Prefer its render node so dmabuf/EGL paths work
    // without needing the primary node's elevated privileges.
    let primary_gpu = primary_gpu(&seat_name)
        .ok()
        .flatten()
        .and_then(|path| DrmNode::from_path(path).ok())
        .and_then(|node| node.node_with_type(NodeType::Render).and_then(|r| r.ok()))
        .or_else(|| {
            all_gpus(&seat_name)
                .ok()?
                .into_iter()
                .find_map(|p| DrmNode::from_path(p).ok())
        })
        .ok_or_else(|| anyhow::anyhow!("no GPU found for seat {seat_name}"))?;
    tracing::info!(?primary_gpu, "selected primary GPU");

    let gpus = GpuManager::new(GbmGlesBackend::with_factory(|display| {
        let context = EGLContext::new_with_priority(display, ContextPriority::High)?;
        let capabilities = unsafe { GlesRenderer::supported_capabilities(&context)? };
        let _: &[Capability] = &capabilities; // type hint
        Ok(unsafe { GlesRenderer::with_capabilities(context, capabilities)? })
    }))
    .map_err(|e| anyhow::anyhow!("GpuManager init failed: {e}"))?;

    // libinput: keyboard, pointer, touch.  Build before UdevData so we can
    // store the context in the struct — the VT watchdog needs it for resume().
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow::anyhow!("libinput could not assign seat {seat_name}"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    // Record the active VT at startup — we are the foreground, so the current
    let vt_active = session.is_active();
    tracing::info!(vt_active, "session active at startup");

    state.udev = Some(UdevData {
        devices: Vec::new(),
        session,
        libinput: libinput_context,
        vt_active,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
        syncobj_state: None,
    });

    // udev: notifies us when DRM devices come and go.
    let udev_backend = UdevBackend::new(&seat_name)
        .map_err(|e| anyhow::anyhow!("udev backend init failed: {e}"))?;

    event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, state| {
            // Apply `[input]` settings as devices appear, and keep a roster so
            // a config hot-reload can re-apply to everything already connected.
            // Done here (not in the generic `process_input_event`) because only
            // the libinput backend yields real `input::Device`s to configure.
            match &mut event {
                InputEvent::DeviceAdded { device } => {
                    crate::input_config::apply(device, &state.config.input);
                    if let Some(udev) = state.udev.as_mut() {
                        udev.devices.push(device.clone());
                    }
                }
                InputEvent::DeviceRemoved { device } => {
                    if let Some(udev) = state.udev.as_mut() {
                        udev.devices.retain(|d| d != device);
                    }
                }
                _ => {}
            }
            state.process_input_event(event);
        })
        .map_err(|e| anyhow::anyhow!("insert libinput source: {e}"))?;

    // Session pause/resume on VT switch.  We suspend libinput when paused so
    // we don't fight whoever owns the foreground VT, then reactivate every drm
    // device on resume and re-render once.
    //
    // Problem: when running under LightDM whose own logind session lives on a
    // *different* VT from the one this compositor drives, logind's ResumeDevice
    // signal is never delivered for our VT.  ActivateSession therefore never
    // fires.  As a fallback we install a 200 ms timer in PauseSession that
    // polls /sys/class/tty/tty0/active; when it matches our home VT we call
    // do_activate_session() directly.
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, &mut (), state| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused (vt switch away)");
                if let Some(udev) = state.udev.as_mut() {
                    udev.vt_active = false;
                    udev.libinput.suspend();
                    for (node, backend) in udev.backends.iter_mut() {
                        tracing::debug!(
                            ?node,
                            surfaces = backend.surfaces.len(),
                            "pausing drm device"
                        );
                        backend.drm_output_manager.pause();
                    }
                }
                // Start the VT watchdog.  It drops itself once vt_active goes true.
                //
                // Problem: when the compositor session lives on a different VT
                // (e.g. tty8 under LightDM) from the one the user expects to
                // return to (e.g. tty7), logind never sends ResumeDevice and
                // ActivateSession never fires.
                //
                // Fix: record the VT the user just switched *to* (away_vt).
                // Poll every 200 ms.  The moment the active VT is anything other
                // than away_vt the user has moved toward the compositor; fire
                // do_activate_session() immediately so the WM reclaims DRM master
                // before the display blanks.
                let away_vt = std::fs::read_to_string("/sys/class/tty/tty0/active")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                tracing::info!(away_vt, "VT watchdog armed");
                if away_vt.is_empty() {
                    tracing::warn!("away_vt unknown; VT watchdog disabled");
                } else {
                    let _ = state.loop_handle.insert_source(
                        Timer::from_duration(Duration::from_millis(200)),
                        move |_, _, state| {
                            if state.udev.as_ref().is_some_and(|u| u.vt_active) {
                                return TimeoutAction::Drop;
                            }
                            let current = std::fs::read_to_string("/sys/class/tty/tty0/active")
                                .map(|s| s.trim().to_string())
                                .unwrap_or_default();
                            // Still on the away VT — keep waiting.
                            if current == away_vt || current.is_empty() {
                                return TimeoutAction::ToDuration(Duration::from_millis(200));
                            }
                            // User has moved to a different VT (presumably back
                            // toward the compositor).  Self-activate immediately.
                            tracing::warn!(
                                current,
                                away_vt,
                                "VT watchdog: user left away-VT; self-activating"
                            );
                            do_activate_session(state);
                            TimeoutAction::Drop
                        },
                    );
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session activated (vt switch in)");
                if state.udev.as_ref().is_some_and(|u| u.vt_active) {
                    tracing::debug!(
                        "ActivateSession: already active (watchdog fired first), skipping"
                    );
                    return;
                }
                do_activate_session(state);
            }
        })
        .map_err(|e| anyhow::anyhow!("insert session notifier: {e}"))?;

    // Walk every device currently visible to udev, opening the ones we can
    // and ignoring the rest. Primary first so its render node is available
    // before any other device tries to fall back to it.
    let primary_dev_id = primary_gpu.dev_id();
    let mut devices: Vec<(u64, std::path::PathBuf)> = udev_backend
        .device_list()
        .map(|(id, p)| (id, p.to_path_buf()))
        .collect();
    devices.sort_by_key(|(id, _)| (*id != primary_dev_id, *id));
    for (device_id, path) in devices {
        match DrmNode::from_dev_id(device_id).map_err(DeviceAddError::DrmNode) {
            Ok(node) => {
                if let Err(e) = device_added(state, node, &path) {
                    tracing::warn!(device_id, error = %e, "skipping device");
                }
            }
            Err(e) => tracing::warn!(device_id, error = %e, "drm node resolution failed"),
        }
    }

    // Hot-plug listener. Add/remove devices as the user plugs them.
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, state| match event {
            UdevEvent::Added { device_id, path } => {
                match DrmNode::from_dev_id(device_id).map_err(DeviceAddError::DrmNode) {
                    Ok(node) => {
                        if let Err(e) = device_added(state, node, &path) {
                            tracing::warn!(device_id, error = %e, "device add failed");
                        }
                    }
                    Err(e) => tracing::warn!(device_id, error = %e, "drm node resolution failed"),
                }
            }
            UdevEvent::Changed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_changed(state, node);
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    device_removed(state, node);
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("insert udev source: {e}"))?;

    Ok(())
}

fn device_added(
    state: &mut ShoestringWm,
    node: DrmNode,
    path: &Path,
) -> std::result::Result<(), DeviceAddError> {
    let udev = state.udev.as_mut().expect("udev not initialized");

    let fd = udev
        .session
        .open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )
        .map_err(DeviceAddError::DeviceOpen)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm, drm_notifier) =
        DrmDevice::new(fd.clone(), true).map_err(DeviceAddError::DrmDevice)?;
    let gbm = GbmDevice::new(fd).map_err(DeviceAddError::GbmDevice)?;

    let registration_token = state
        .loop_handle
        .insert_source(drm_notifier, move |event, meta, state| match event {
            DrmEvent::VBlank(crtc) => state.frame_finish(node, crtc, meta.take()),
            DrmEvent::Error(error) => tracing::error!(?node, ?error, "drm error"),
        })
        .expect("insert drm notifier");

    let render_node = {
        let display = unsafe { EGLDisplay::new(gbm.clone()).map_err(DeviceAddError::AddNode)? };
        let egl_device =
            EGLDevice::device_for_display(&display).map_err(DeviceAddError::AddNode)?;
        if egl_device.is_software() {
            return Err(DeviceAddError::NoRenderNode);
        }
        let render_node = egl_device
            .try_get_render_node()
            .ok()
            .flatten()
            .unwrap_or(node);
        udev.gpus
            .as_mut()
            .add_node(render_node, gbm.clone())
            .map_err(DeviceAddError::AddNode)?;
        render_node
    };

    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());

    let mut renderer = udev
        .gpus
        .single_renderer(&render_node)
        .expect("single_renderer for freshly-added node");
    let render_formats: smithay::backend::allocator::format::FormatSet = renderer
        .as_mut()
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();
    // Keep a copy for the dmabuf global / screencopy advertisement; the
    // original is consumed by the DrmOutputManager below.
    let dmabuf_formats = render_formats.clone();

    let drm_output_manager = DrmOutputManager::new(
        drm,
        allocator,
        framebuffer_exporter,
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        render_formats,
    );

    udev.backends.insert(
        node,
        BackendData {
            surfaces: HashMap::new(),
            drm_output_manager,
            drm_scanner: DrmScanner::new(),
            render_node: Some(render_node),
            registration_token,
        },
    );

    // First GPU to come up stands up the zwp_linux_dmabuf_v1 global, advertising
    // this renderer's import formats. This is a general GPU facility (clients use
    // it for their own surface buffers) and is intentionally NOT behind the
    // screen-capture gate — only the screencopy `linux_dmabuf` event respects
    // that gate. The udev borrow ends above, so we touch `state` directly here.
    if state.dmabuf_global.is_none() {
        let dh = state.display_handle.clone();
        let global = state
            .dmabuf_state
            .create_global::<ShoestringWm>(&dh, dmabuf_formats.iter().copied());
        state.dmabuf_global = Some(global);
        tracing::info!(
            formats = dmabuf_formats.iter().count(),
            "zwp_linux_dmabuf_v1 global created"
        );
        state.dmabuf_formats = Some(dmabuf_formats);
    }

    // The first GPU that supports `drmSyncobjEventfd` stands up the
    // `wp_linux_drm_syncobj_v1` (explicit sync) global. Mirrors the dmabuf
    // global above: a general GPU facility, created at most once, NOT behind
    // the screen-capture gate. XWayland 23.2+ prefers explicit sync, so this
    // removes the implicit-sync fallback that causes tearing/stutter in
    // Wine/Proton GL/Vulkan apps. The udev borrow ended above; touch `state`.
    if state
        .udev
        .as_ref()
        .is_some_and(|u| u.syncobj_state.is_none())
    {
        let import_device = state
            .udev
            .as_ref()
            .and_then(|u| u.backends.get(&node))
            .map(|b| b.drm_output_manager.device().device_fd().clone());
        if let Some(import_device) = import_device {
            if supports_syncobj_eventfd(&import_device) {
                let dh = state.display_handle.clone();
                let syncobj_state = DrmSyncobjState::new::<ShoestringWm>(&dh, import_device);
                state.udev.as_mut().unwrap().syncobj_state = Some(syncobj_state);
                tracing::info!("wp_linux_drm_syncobj_v1 (explicit sync) global created");
            } else {
                tracing::info!("primary GPU lacks drmSyncobjEventfd; explicit sync not advertised");
            }
        }
    }

    device_changed(state, node);
    Ok(())
}

fn device_changed(state: &mut ShoestringWm, node: DrmNode) {
    let udev = state.udev.as_mut().expect("udev");
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let scan = match device
        .drm_scanner
        .scan_connectors(device.drm_output_manager.device())
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(?node, error = ?e, "connector scan failed");
            return;
        }
    };

    // Collect first because connector_connected/disconnected mutate `state`.
    let events: Vec<_> = scan.into_iter().collect();
    for ev in events {
        match ev {
            DrmScanEvent::Connected {
                connector,
                crtc: Some(crtc),
            } => {
                connector_connected(state, node, connector, crtc);
            }
            DrmScanEvent::Disconnected {
                connector,
                crtc: Some(crtc),
            } => {
                connector_disconnected(state, node, connector, crtc);
            }
            _ => {}
        }
    }
}

fn device_removed(state: &mut ShoestringWm, node: DrmNode) {
    let udev = state.udev.as_mut().expect("udev");
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let crtcs: Vec<_> = device
        .drm_scanner
        .crtcs()
        .map(|(info, crtc)| (info.clone(), crtc))
        .collect();
    for (conn, crtc) in crtcs {
        connector_disconnected(state, node, conn, crtc);
    }

    let udev = state.udev.as_mut().unwrap();
    if let Some(backend) = udev.backends.remove(&node) {
        if let Some(rn) = backend.render_node {
            udev.gpus.as_mut().remove_node(&rn);
        }
        state.loop_handle.remove(backend.registration_token);
        tracing::info!(?node, "drm device removed");
    }
}

fn connector_connected(
    state: &mut ShoestringWm,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let udev = state.udev.as_mut().expect("udev");
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    let render_node = device.render_node.unwrap_or(udev.primary_gpu);
    let mut renderer = match udev.gpus.single_renderer(&render_node) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?node, error = ?e, "no renderer for connector");
            return;
        }
    };

    let output_name = format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id()
    );
    tracing::info!(%output_name, ?crtc, "connector connected");

    let drm_device = device.drm_output_manager.device();
    let info = display_info::for_connector(drm_device, connector.handle());
    let make = info
        .as_ref()
        .and_then(|i| i.make())
        .unwrap_or_else(|| "Unknown".into());
    let model = info
        .as_ref()
        .and_then(|i| i.model())
        .unwrap_or_else(|| "Unknown".into());
    let serial = info
        .as_ref()
        .and_then(|i| i.serial())
        .unwrap_or_else(|| "Unknown".into());

    let mode_id = connector
        .modes()
        .iter()
        .position(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(0);
    let drm_mode = connector.modes()[mode_id];
    let wl_mode = WlMode::from(drm_mode);

    let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
    let output = Output::new(
        output_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: connector.subpixel().into(),
            make,
            model,
            serial_number: serial,
        },
    );
    let global = output.create_global::<ShoestringWm>(&state.display_handle);

    // Use the configured position if set; otherwise lay outputs left-to-right.
    let position = if let Some([px, py]) = state
        .config
        .outputs
        .get(&output_name)
        .and_then(|oc| oc.position)
    {
        (px, py).into()
    } else {
        let x = state.space.outputs().fold(0, |acc, o| {
            acc + state
                .space
                .output_geometry(o)
                .map(|g| g.size.w)
                .unwrap_or(0)
        });
        (x, 0).into()
    };
    let scale_val = state
        .config
        .outputs
        .get(&output_name)
        .and_then(|oc| oc.scale)
        .unwrap_or(state.config.general.output_scale);
    let scale = crate::backend::scale_from_config(scale_val);
    let transform = state
        .config
        .outputs
        .get(&output_name)
        .and_then(|oc| oc.transform)
        .map(crate::backend::transform_from_config);
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), transform, Some(scale), Some(position));
    state.space.map_output(&output, position);

    output.user_data().insert_if_missing(|| UdevOutputId {
        crtc,
        device_id: node,
    });

    let planes = match drm_device.planes(&crtc) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?crtc, error = ?e, "planes query failed");
            return;
        }
    };

    let drm_output = match device.drm_output_manager.lock().initialize_output::<
        _,
        SpaceRenderElements<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
    >(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        Some(planes),
        &mut renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(?crtc, error = ?e, "drm output init failed");
            return;
        }
    };

    // Resolve VRR/adaptive-sync: opt-in per output via config, gated on the
    // connector actually advertising support. `use_vrr` only takes effect on
    // the next `render_frame`; for `RequiresModeset` connectors that first
    // frame does a modeset, which is fine here on initial bring-up.
    let want_vrr = state
        .config
        .outputs
        .get(&output_name)
        .and_then(|oc| oc.adaptive_sync)
        .unwrap_or(false);
    let vrr = want_vrr
        && match drm_output.with_compositor(|c| c.vrr_supported(connector.handle())) {
            Ok(VrrSupport::Supported) | Ok(VrrSupport::RequiresModeset) => {
                match drm_output.with_compositor(|c| c.use_vrr(true)) {
                    Ok(()) => {
                        tracing::info!(%output_name, "adaptive sync (VRR) enabled");
                        true
                    }
                    Err(e) => {
                        tracing::warn!(%output_name, error = ?e, "enabling VRR failed");
                        false
                    }
                }
            }
            Ok(VrrSupport::NotSupported) => {
                tracing::warn!(
                    %output_name,
                    "adaptive_sync requested but the connector does not advertise VRR support"
                );
                false
            }
            Err(e) => {
                tracing::warn!(%output_name, error = ?e, "querying VRR support failed");
                false
            }
        };
    output
        .user_data()
        .insert_if_missing(|| crate::backend::VrrState(vrr));

    let surface = SurfaceData {
        output: output.clone(),
        global: Some(global),
        drm_output,
        vrr,
    };
    device.surfaces.insert(crtc, surface);

    state.emit_ipc(shoestring_ipc::Event::OutputAdded(
        shoestring_ipc::OutputSummary {
            name: output.name(),
            width: wl_mode.size.w,
            height: wl_mode.size.h,
            scale: scale_val,
            adaptive_sync: vrr,
            transform: crate::backend::transform_name(output.current_transform()).to_string(),
        },
    ));

    crate::output_management::broadcast_head_added(state, &output);
    crate::ext_workspace::broadcast_output_enter(state, &output);
    // A new output may now contain existing windows — refresh taskbar
    // output_enter/leave.
    crate::foreign_toplevel_mgmt::broadcast_all(state);

    // First render is scheduled as an idle task so we return out of this
    // event handler before touching the surface again.
    state.loop_handle.insert_idle(move |state| {
        state.render_surface(node, crtc);
    });
}

fn connector_disconnected(
    state: &mut ShoestringWm,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle,
) {
    let udev = state.udev.as_mut().expect("udev");
    let Some(device) = udev.backends.get_mut(&node) else {
        return;
    };

    if let Some(surface) = device.surfaces.remove(&crtc) {
        let name = surface.output.name();
        crate::output_management::broadcast_head_removed(state, &surface.output);
        crate::ext_workspace::broadcast_output_leave(state, &surface.output);
        state.space.unmap_output(&surface.output);
        state.space.refresh();
        // Windows that were on this output now report no output — refresh
        // taskbar output_leave while the wl_output still resolves.
        crate::foreign_toplevel_mgmt::broadcast_all(state);
        surface.output.leave_all();
        if let Some(global) = surface.global {
            state.display_handle.remove_global::<ShoestringWm>(global);
        }
        state.emit_ipc(shoestring_ipc::Event::OutputRemoved { name });
        tracing::info!(?node, ?crtc, conn = ?connector.handle(), "connector disconnected");
    }
}

/// Disable `output` in response to a wlr-output-management `disable_head`
/// request.  Tears down the `DrmOutput` (releasing the CRTC), removes the
/// output from the compositor space, and notifies IPC / output-management
/// clients.  The connector stays physically plugged in; a future
/// `connector_connected` (or re-enable) will bring it back.
///
/// Returns `true` on success, `false` if the output has no `UdevOutputId`
/// (e.g. winit backend) or was already torn down.
pub fn disable_output(state: &mut ShoestringWm, output: &Output) -> bool {
    let Some(udev_id) = output.user_data().get::<UdevOutputId>() else {
        tracing::warn!(output = output.name(), "disable_output: no UdevOutputId");
        return false;
    };
    let node = udev_id.device_id;
    let crtc = udev_id.crtc;

    let surface = {
        let Some(udev) = state.udev.as_mut() else {
            return false;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return false;
        };
        device.surfaces.remove(&crtc)
    };

    let Some(surface) = surface else {
        tracing::warn!(
            output = output.name(),
            "disable_output: surface already gone"
        );
        return false;
    };

    // Notify wlr-output-management clients *before* unmapping so they still
    // see a valid output object in the finished event.
    let name = surface.output.name();
    crate::output_management::broadcast_head_removed(state, &surface.output);
    crate::ext_workspace::broadcast_output_leave(state, &surface.output);
    state.space.unmap_output(&surface.output);
    state.space.refresh();
    // Windows that were on this output now report no output — refresh taskbar
    // output_leave while the wl_output still resolves.
    crate::foreign_toplevel_mgmt::broadcast_all(state);
    surface.output.leave_all();
    if let Some(global) = surface.global {
        state.display_handle.remove_global::<ShoestringWm>(global);
    }
    // DrmOutput is dropped here, releasing the CRTC.
    drop(surface.drm_output);

    state.emit_ipc(shoestring_ipc::Event::OutputRemoved { name: name.clone() });
    tracing::info!(%name, ?node, ?crtc, "output disabled (CRTC released)");
    true
}

/// Apply a DRM mode change to `output`, called from the wlr-output-management
/// apply path.  Returns `true` on success, `false` if no matching DRM mode was
/// found or the kernel rejected the change.
///
/// Only the DRM kernel mode is changed here; the smithay `Output` state
/// (`change_current_state`) is updated by the caller so that transform /
/// scale / position changes can be batched into a single call.
pub fn change_output_mode(
    state: &mut ShoestringWm,
    output: &Output,
    req_size: smithay::utils::Size<i32, smithay::utils::Physical>,
    req_refresh: i32,
) -> bool {
    let Some(udev_id) = output.user_data().get::<UdevOutputId>() else {
        tracing::warn!(
            output = output.name(),
            "change_output_mode: no UdevOutputId"
        );
        return false;
    };
    let node = udev_id.device_id;
    let crtc = udev_id.crtc;

    // Collect connector handles from the surface compositor.
    let connector_handles: Vec<connector::Handle> = {
        let Some(udev) = state.udev.as_ref() else {
            return false;
        };
        let Some(device) = udev.backends.get(&node) else {
            return false;
        };
        let Some(surface) = device.surfaces.get(&crtc) else {
            return false;
        };
        surface
            .drm_output
            .with_compositor(|c| c.surface().current_connectors().into_iter().collect())
    };

    // Find the DRM mode that matches the requested (size, refresh).
    let drm_mode: Option<smithay::reexports::drm::control::Mode> = (|| {
        let udev = state.udev.as_ref()?;
        let device = udev.backends.get(&node)?;
        let dev = device.drm_output_manager.device().device_fd();
        for &conn_handle in &connector_handles {
            if let Ok(info) = dev.get_connector(conn_handle, false) {
                for &m in info.modes() {
                    let wl = WlMode::from(m);
                    if wl.size == req_size && wl.refresh == req_refresh {
                        return Some(m);
                    }
                }
            }
        }
        None
    })();

    let Some(drm_mode) = drm_mode else {
        tracing::warn!(
            output = output.name(),
            ?req_size,
            req_refresh,
            "change_output_mode: no matching DRM mode"
        );
        return false;
    };

    let render_node = {
        let Some(udev) = state.udev.as_ref() else {
            return false;
        };
        let Some(device) = udev.backends.get(&node) else {
            return false;
        };
        device.render_node.unwrap_or(udev.primary_gpu)
    };

    let Some(udev) = state.udev.as_mut() else {
        return false;
    };
    let mut renderer: UdevRenderer<'_> = match udev.gpus.single_renderer(&render_node) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(output = output.name(), error = ?e, "change_output_mode: renderer unavailable");
            return false;
        }
    };
    let Some(device) = udev.backends.get_mut(&node) else {
        return false;
    };
    let Some(surface) = device.surfaces.get_mut(&crtc) else {
        return false;
    };

    let elements: DrmOutputRenderElements<
        UdevRenderer<'_>,
        crate::drawing::PointerRenderElement<UdevRenderer<'_>>,
    > = DrmOutputRenderElements::new();

    match surface
        .drm_output
        .use_mode(drm_mode, &mut renderer, &elements)
    {
        Ok(()) => {
            let wl_mode = WlMode::from(drm_mode);
            output.change_current_state(Some(wl_mode), None, None, None);
            tracing::info!(
                output = output.name(),
                ?req_size,
                req_refresh,
                "DRM mode changed"
            );
            true
        }
        Err(e) => {
            tracing::warn!(output = output.name(), error = ?e, "change_output_mode: use_mode failed");
            false
        }
    }
}

/// Shared activation logic for both the `ActivateSession` event path and the
/// VT watchdog path.  Resumes libinput, reactivates every DRM backend, resets
/// framebuffer age tracking, and kicks an idle render on each surface.
fn do_activate_session(state: &mut ShoestringWm) {
    {
        let Some(udev) = state.udev.as_mut() else {
            tracing::error!("do_activate_session: udev is None");
            return;
        };
        if let Err(e) = udev.libinput.resume() {
            tracing::warn!(error = ?e, "libinput resume failed");
        }
        let nodes: Vec<DrmNode> = udev.backends.keys().copied().collect();
        tracing::debug!(node_count = nodes.len(), "activating drm backends");
        for node in &nodes {
            if let Some(backend) = udev.backends.get_mut(node) {
                // Explicitly reclaim DRM master before asking smithay to
                // activate the device.  Smithay only calls SET_MASTER when
                // the fd is "privileged" (it acquired master at open-time),
                // but we run unprivileged — logind manages master via
                // ResumeDevice.  After the initial ResumeDevice, was_master
                // is set on the file description, so SET_MASTER succeeds
                // again after a DROP_MASTER even without CAP_SYS_ADMIN.
                let device_fd = backend.drm_output_manager.device().device_fd().clone();
                match device_fd.acquire_master_lock() {
                    Ok(()) => tracing::info!(?node, "DRM master re-acquired"),
                    Err(e) => {
                        tracing::warn!(?node, error = ?e, "DRM master re-acquire failed; rendering may fail")
                    }
                }
                tracing::debug!(
                    ?node,
                    surfaces = backend.surfaces.len(),
                    "calling drm activate"
                );
                match backend.drm_output_manager.lock().activate(false) {
                    Ok(()) => tracing::debug!(?node, "drm activate ok"),
                    Err(e) => tracing::error!(?node, error = ?e, "drm activate failed"),
                }
                // Reset swapchain age so damage tracking considers the whole
                // screen dirty, guaranteeing a non-empty frame and that
                // queue_frame is called — without this the VBlank-driven loop
                // never restarts after a VT switch.
                for (crtc, surface) in backend.surfaces.iter() {
                    tracing::debug!(?node, ?crtc, "reset_buffers");
                    surface.drm_output.reset_buffers();
                }
            }
        }
        udev.vt_active = true;
    }
    // A VT switch resets every CRTC's gamma to identity, so re-push the ramps
    // night-light clients had set. Done now that vt_active is true (so
    // apply_gamma actually touches the hardware) and before the render kicks.
    state.reapply_all_gamma();
    // Kick a render on every surface.  Deferred via insert_idle so calloop
    // finishes the current event before we submit DRM commits.
    let nodes: Vec<DrmNode> = state
        .udev
        .as_ref()
        .map(|u| u.backends.keys().copied().collect())
        .unwrap_or_default();
    for node in nodes {
        let crtcs: Vec<crtc::Handle> = state
            .udev
            .as_ref()
            .and_then(|u| u.backends.get(&node))
            .map(|b| b.surfaces.keys().copied().collect())
            .unwrap_or_default();
        for crtc in crtcs {
            tracing::debug!(
                ?node,
                ?crtc,
                "scheduling post-activate render_surface via insert_idle"
            );
            state.loop_handle.insert_idle(move |state| {
                state.render_surface(node, crtc);
            });
        }
    }
}

/// DRM glue for `zwlr_gamma_control_v1`. These resolve a Wayland output to its
/// CRTC and read/apply/restore the hardware gamma ramp. They live here (rather
/// than in [`crate::gamma_control`]) because they reach into private udev
/// state — `UdevData::backends` and the per-CRTC `DrmDevice`. Dispatch in
/// [`crate::handlers::gamma_control`] calls them.
impl ShoestringWm {
    /// Resolve `output` to `(node, crtc, gamma_size)`. `None` if the output is
    /// not a KMS/udev output or its CRTC advertises no gamma table.
    pub(crate) fn gamma_target_for_output(
        &self,
        output: &Output,
    ) -> Option<(DrmNode, crtc::Handle, u32)> {
        let id = output.user_data().get::<UdevOutputId>()?;
        let backend = self.udev.as_ref()?.backends.get(&id.device_id)?;
        let size = backend
            .drm_output_manager
            .device()
            .get_crtc(id.crtc)
            .ok()?
            .gamma_length();
        (size != 0).then_some((id.device_id, id.crtc, size))
    }

    /// Read the CRTC's current gamma ramp, used to snapshot the table before a
    /// client takes over so it can be restored on release.
    pub(crate) fn read_gamma(
        &self,
        node: DrmNode,
        crtc: crtc::Handle,
        size: u32,
    ) -> Option<crate::gamma_control::GammaRamp> {
        let backend = self.udev.as_ref()?.backends.get(&node)?;
        let dev = backend.drm_output_manager.device();
        let mut red = vec![0u16; size as usize];
        let mut green = vec![0u16; size as usize];
        let mut blue = vec![0u16; size as usize];
        dev.get_gamma(crtc, &mut red, &mut green, &mut blue).ok()?;
        Some(crate::gamma_control::GammaRamp { red, green, blue })
    }

    /// Push a gamma ramp to the CRTC. Returns `false` only on a real DRM
    /// failure; while we don't hold DRM master (VT switched away) it stores
    /// nothing here and reports success — [`Self::reapply_all_gamma`] pushes it
    /// once we're active again.
    pub(crate) fn apply_gamma(
        &self,
        node: DrmNode,
        crtc: crtc::Handle,
        ramp: &crate::gamma_control::GammaRamp,
    ) -> bool {
        let Some(udev) = self.udev.as_ref() else {
            return false;
        };
        if !udev.vt_active {
            return true;
        }
        let Some(backend) = udev.backends.get(&node) else {
            return false;
        };
        match backend.drm_output_manager.device().set_gamma(
            crtc,
            &ramp.red,
            &ramp.green,
            &ramp.blue,
        ) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(?node, ?crtc, error = ?e, "set_gamma failed");
                false
            }
        }
    }

    /// Re-apply every client's gamma ramp. Called after a VT switch back in,
    /// which resets the CRTC gamma to identity.
    pub(crate) fn reapply_all_gamma(&self) {
        for ctrl in self.gamma_control.controls.values() {
            if let Some(ramp) = &ctrl.current {
                self.apply_gamma(ctrl.node, ctrl.crtc, ramp);
            }
        }
    }

    /// Release the control for `name` *if* `resource` is its current holder,
    /// restoring the output's original gamma. A stale resource (one already
    /// transferred away to a newer client) is ignored so it can't clobber the
    /// new holder.
    pub(crate) fn release_gamma_control(
        &mut self,
        name: &str,
        resource: &smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1,
    ) {
        let is_active = self
            .gamma_control
            .controls
            .get(name)
            .is_some_and(|c| c.owner.is_client(resource));
        if !is_active {
            return;
        }
        let ctrl = self.gamma_control.controls.remove(name).unwrap();
        self.apply_gamma(ctrl.node, ctrl.crtc, &ctrl.original);
    }

    /// IPC `set-gamma`: compute a ramp from a color `temperature` (Kelvin),
    /// `brightness` and `gamma`, and apply it to `filter` (one output by name,
    /// or all gamma-capable outputs when `None`). Takes over from any
    /// wlr-gamma-control client on those outputs (the client is `failed`).
    /// Returns the number of outputs set, or an error string.
    pub(crate) fn set_gamma_ipc(
        &mut self,
        filter: Option<&str>,
        temperature: u32,
        brightness: f64,
        gamma: f64,
    ) -> Result<usize, String> {
        if !(1000..=25000).contains(&temperature) {
            return Err(format!(
                "temperature {temperature}K out of range (1000–25000)"
            ));
        }
        if !(0.1..=1.0).contains(&brightness) {
            return Err(format!("brightness {brightness} out of range (0.1–1.0)"));
        }
        if !(0.1..=10.0).contains(&gamma) {
            return Err(format!("gamma {gamma} out of range (0.1–10.0)"));
        }

        // Collect targets first (shared borrows of self.space + self.udev),
        // then mutate self.gamma_control below.
        let targets: Vec<(String, DrmNode, crtc::Handle, u32)> = self
            .space
            .outputs()
            .filter(|o| filter.is_none_or(|f| o.name() == f))
            .filter_map(|o| {
                self.gamma_target_for_output(o)
                    .map(|(n, c, s)| (o.name(), n, c, s))
            })
            .collect();

        if targets.is_empty() {
            return Err(match filter {
                Some(f) => format!("no gamma-capable output named '{f}'"),
                None => "no gamma-capable outputs".into(),
            });
        }

        let mut applied = 0;
        for (name, node, crtc, size) in targets {
            let ramp =
                crate::gamma_control::ramp_from_temperature(size, temperature, brightness, gamma);
            // Transfer: fail any client owner, carry its original ramp forward.
            let prior_original = self.gamma_control.controls.remove(&name).map(|old| {
                if let crate::gamma_control::GammaOwner::Client(r) = &old.owner {
                    r.failed();
                }
                old.original
            });
            let original = prior_original
                .or_else(|| self.read_gamma(node, crtc, size))
                .unwrap_or_else(|| crate::gamma_control::GammaRamp::identity(size));
            let ok = self.apply_gamma(node, crtc, &ramp);
            self.gamma_control.controls.insert(
                name,
                crate::gamma_control::GammaControl {
                    owner: crate::gamma_control::GammaOwner::Ipc,
                    node,
                    crtc,
                    size,
                    original,
                    current: Some(ramp),
                },
            );
            if ok {
                applied += 1;
            }
        }

        if applied == 0 {
            return Err("the DRM driver rejected the gamma ramp".into());
        }
        Ok(applied)
    }

    /// IPC `reset-gamma`: drop the WM's own (IPC-set) gamma on `filter` (one
    /// output, or all when `None`), restoring each output's original ramp.
    /// Leaves wlr-gamma-control clients alone. Returns the number cleared.
    pub(crate) fn reset_gamma_ipc(&mut self, filter: Option<&str>) -> usize {
        let names: Vec<String> = self
            .gamma_control
            .controls
            .iter()
            .filter(|(name, c)| {
                matches!(c.owner, crate::gamma_control::GammaOwner::Ipc)
                    && filter.is_none_or(|f| name.as_str() == f)
            })
            .map(|(n, _)| n.clone())
            .collect();

        let mut count = 0;
        for name in names {
            if let Some(ctrl) = self.gamma_control.controls.remove(&name) {
                self.apply_gamma(ctrl.node, ctrl.crtc, &ctrl.original);
                count += 1;
            }
        }
        count
    }
}

impl ShoestringWm {
    /// VBlank handler: the GPU just finished presenting a frame on `crtc`.
    /// Acknowledge it, mark the frame's `wp_presentation` feedback presented
    /// (with the hardware timestamp when the driver supplied one), and
    /// schedule the next render one frame later.
    pub(crate) fn frame_finish(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        metadata: Option<DrmEventMetadata>,
    ) {
        // Captured before borrowing `self.udev` so it's available as the
        // fallback presentation time (when the driver gives no vblank stamp).
        let fallback_now = self.clock.now();

        let Some(udev) = self.udev.as_mut() else {
            return;
        };

        // VBlank events keep arriving on the DRM fd even while we are paused
        // (the CRTC keeps running for the TTY console). If we schedule a render
        // timer here while inactive, that timer fires, fails with DeviceInactive,
        // reschedules another timer — and VBlank creates yet another. After even
        // a few seconds away, hundreds of timers pile up and race each other on
        // session resume, preventing a clean takeover of the display.
        let session_active = udev.vt_active;

        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        // A hardware monotonic vblank timestamp is the most accurate present
        // time; fall back to "now" (still monotonic, same clock id) otherwise.
        use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
        let hw_tp = metadata.as_ref().and_then(|m| match m.time {
            DrmEventTime::Monotonic(tp) if !tp.is_zero() => Some(tp),
            _ => None,
        });
        let seq = metadata.as_ref().map(|m| m.sequence).unwrap_or(0) as u64;
        let (present_time, flags) = match hw_tp {
            Some(tp) => (tp.into(), Kind::Vsync | Kind::HwClock | Kind::HwCompletion),
            None => (fallback_now, Kind::Vsync),
        };
        let refresh = surface
            .output
            .current_mode()
            .map(|m| {
                smithay::wayland::presentation::Refresh::fixed(Duration::from_secs_f64(
                    1_000.0 / m.refresh as f64,
                ))
            })
            .unwrap_or(smithay::wayland::presentation::Refresh::Unknown);

        match surface.drm_output.frame_submitted() {
            Ok(user_data) => {
                if let Some(mut feedback) = user_data.flatten() {
                    feedback.presented(present_time, refresh, seq, flags);
                }
            }
            Err(e) => tracing::warn!(?crtc, error = ?e, "frame_submitted failed"),
        }

        if !session_active {
            tracing::debug!(
                ?node,
                ?crtc,
                "frame_finish: session inactive, suppressing render timer"
            );
            return;
        }

        // Frame interval from the output's current mode.
        let refresh_mhz = surface
            .output
            .current_mode()
            .map(|m| m.refresh)
            .unwrap_or(60_000);
        let frame_us = 1_000_000_000u64 / refresh_mhz as u64;
        let interval = Duration::from_micros(frame_us);

        let timer = Timer::from_duration(interval);
        let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
            state.render_surface(node, crtc);
            TimeoutAction::Drop
        });
    }

    /// Compose `space` onto `crtc` and queue the resulting frame.
    pub(crate) fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle) {
        // Read session state before taking any borrows. If the session is
        // paused (we switched to a TTY), all reschedule paths must be skipped
        // — rescheduling while paused builds up a storm of pending timers that
        // race each other on resume. The activate-triggered render runs with
        // is_active() already true, so that path is unaffected.
        let session_active = self.udev.as_ref().map(|u| u.vt_active).unwrap_or(false);

        // Snapshot the output for send_frame, since the borrow on `udev`
        // below precludes touching `self.space` while holding it.
        let output = {
            let Some(udev) = self.udev.as_ref() else {
                return;
            };
            let Some(device) = udev.backends.get(&node) else {
                return;
            };
            let Some(surface) = device.surfaces.get(&crtc) else {
                return;
            };
            surface.output.clone()
        };

        // Refresh first so newly-mapped surfaces show up this frame.
        self.space.refresh();

        // Capture cursor state and refresh its buffer before we mutably
        // re-borrow `self.udev` below — the renderer borrow conflicts with
        // any `&mut self` method call from inside the borrow scope. Cursor
        // is a raster sprite so we round any fractional scale up.
        let scale_int = self.config.general.output_scale.ceil().max(1.0) as u32;
        self.refresh_cursor_buffer(scale_int);
        let cursor_snapshot = self.cursor_render_snapshot();
        // This output's place in the global logical layout, captured now so
        // the `self.udev` re-borrow below doesn't preclude touching
        // `self.space`. Used to scope the cursor to the output it sits on and
        // to translate it into that output's local coordinate space.
        let output_geometry = self.space.output_geometry(&output);
        // Capture lock state before re-borrowing self.udev — the renderer
        // borrow precludes calling &self methods further down.
        let locked = self.is_locked();
        let lock_surface = self.lock_surface_for(&output);
        // Likewise capture the fullscreen window (if any) on this output now:
        // the render path renders only it, dropping the bar/layer surfaces it
        // covers. Skipped while locked (the lock surface owns the screen).
        let fullscreen = (!locked)
            .then(|| crate::layout::fullscreen_window_on(&self.space, &self.layout, &output))
            .flatten();

        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let render_node = device.render_node.unwrap_or(udev.primary_gpu);
        let mut renderer = match udev.gpus.single_renderer(&render_node) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(?crtc, error = ?e, "no renderer for render_surface");
                if session_active {
                    let timer = Timer::from_duration(Duration::from_millis(16));
                    let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
                        state.render_surface(node, crtc);
                        TimeoutAction::Drop
                    });
                }
                return;
            }
        };

        let space_elements: Vec<
            SpaceRenderElements<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
        > = if locked {
            // Drop all client windows from the composition while locked.
            Vec::new()
        } else {
            crate::drawing::output_space_elements(
                &mut renderer,
                &self.space,
                &output,
                fullscreen.as_ref(),
            )
        };

        // Cursor goes on top: build pointer elements from a captured snapshot
        // (taken before we re-borrowed `self.udev` above), then prepend them
        // — render_frame draws first-element-first. While locked we don't
        // render the cursor; the lock surface (if present) fills its place.
        let cursor_elements: Vec<crate::drawing::PointerRenderElement<UdevRenderer<'_>>> = if locked
        {
            let scale: smithay::utils::Scale<f64> =
                output.current_scale().fractional_scale().into();
            crate::handlers::session_lock::lock_render_elements(
                lock_surface.as_ref(),
                &mut renderer,
                scale,
            )
        } else if let Some((pe, location, hotspot)) = cursor_snapshot
            // Only the output the pointer currently sits on draws the cursor.
            // Without this every output renders it (each framebuffer's origin
            // is its own top-left, so an un-scoped global location lands on
            // them all), producing a ghost cursor on the other monitor.
            .filter(|(_, location, _)| {
                output_geometry.is_some_and(|geo| geo.to_f64().contains(*location))
            })
        {
            let scale: smithay::utils::Scale<f64> =
                output.current_scale().fractional_scale().into();
            // Translate from global logical space into this output's local
            // space (subtract its origin) before scaling to physical pixels.
            let origin = output_geometry.map(|g| g.loc).unwrap_or_default();
            let physical_location: smithay::utils::Point<i32, smithay::utils::Physical> =
                smithay::utils::Point::<f64, smithay::utils::Physical>::from((
                    (location.x - origin.x as f64 - hotspot.0 as f64) * scale.x,
                    (location.y - origin.y as f64 - hotspot.1 as f64) * scale.y,
                ))
                .to_i32_round();
            use smithay::backend::renderer::element::AsRenderElements;
            pe.render_elements(&mut renderer, physical_location, scale, 1.0)
        } else {
            Vec::new()
        };

        // Run any pending screencopy captures for this output BEFORE the
        // scanout render. The capture path renders the same scene into an
        // offscreen GLES texture and reads back the requested region; doing
        // it first means we still hold a live `&mut renderer` and aren't
        // racing the swapchain. Reads `&self.space` and `&mut self.screencopy`
        // — disjoint from the `self.udev`-rooted renderer borrow.
        crate::screencopy::process_pending(
            &mut self.screencopy,
            &self.space,
            &output,
            &mut renderer,
            &cursor_elements,
        );

        let mut elements: Vec<
            crate::drawing::OutputRenderElements<
                UdevRenderer<'_>,
                WaylandSurfaceRenderElement<UdevRenderer<'_>>,
            >,
        > = Vec::with_capacity(space_elements.len() + cursor_elements.len());
        elements.extend(
            cursor_elements
                .into_iter()
                .map(crate::drawing::OutputRenderElements::Pointer),
        );
        elements.extend(
            space_elements
                .into_iter()
                .map(crate::drawing::OutputRenderElements::Space),
        );

        let clear = if locked {
            [0.0, 0.0, 0.0, 1.0]
        } else {
            [0.1, 0.1, 0.1, 1.0]
        };
        // Re-assert VRR for outputs that opted in. `use_vrr` short-circuits
        // when the pending state already matches, so this is a couple of cheap
        // lock acquisitions per frame; it only does real work after something
        // (mode change, VT switch) reset the surface's pending state.
        if surface.vrr {
            if let Err(e) = surface.drm_output.with_compositor(|c| c.use_vrr(true)) {
                tracing::warn!(?crtc, error = ?e, "re-asserting VRR failed");
            }
        }

        let render_start = std::time::Instant::now();
        let result =
            surface
                .drm_output
                .render_frame(&mut renderer, &elements, clear, FrameFlags::DEFAULT);
        let frame_us = render_start.elapsed().as_micros() as u64;

        let (rendered, render_states) = match result {
            Ok(frame) => (!frame.is_empty, frame.states),
            Err(e) => {
                let is_test_failed = matches!(
                    e,
                    RenderFrameError::PrepareFrame(FrameError::DrmError(DrmError::TestFailed(_)))
                );
                tracing::warn!(
                    ?crtc,
                    ?e,
                    is_test_failed,
                    session_active,
                    "render_frame failed"
                );
                // TestFailed means the kernel's DRM state diverged from what
                // we have cached — most likely because the TTY took DRM master
                // and rearranged CRTC/connector/plane bindings while we were
                // away. Reset the device to a blank state so the next render
                // can do a full modeset from scratch; without this, every retry
                // also fails with TestFailed and we never recover.
                if is_test_failed {
                    drop(renderer);
                    match device.drm_output_manager.device_mut().reset_state() {
                        Ok(()) => {
                            tracing::info!(?node, "DRM device reset_state ok after TestFailed")
                        }
                        Err(re) => {
                            tracing::warn!(?node, error = ?re, "DRM device reset_state failed after TestFailed")
                        }
                    }
                }
                if session_active {
                    let timer = Timer::from_duration(Duration::from_millis(16));
                    let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
                        state.render_surface(node, crtc);
                        TimeoutAction::Drop
                    });
                }
                return;
            }
        };

        if rendered {
            // Attribute each surface to this output, then collect its
            // wp_presentation feedback and hand it to the frame so the
            // matching VBlank can mark it `presented` (see `frame_finish`).
            crate::presentation::update_primary_scanout_output(
                &self.space,
                &output,
                &render_states,
            );
            let feedback = crate::presentation::take_presentation_feedback(
                &self.space,
                &output,
                &render_states,
            );
            if let Err(e) = surface.drm_output.queue_frame(Some(feedback)) {
                tracing::warn!(?crtc, ?e, session_active, "queue_frame failed");
                if session_active {
                    let timer = Timer::from_duration(Duration::from_millis(16));
                    let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
                        state.render_surface(node, crtc);
                        TimeoutAction::Drop
                    });
                }
            }
        }

        // Count it only when a frame was actually drawn (damage present), so
        // `render.fps` tracks real presented frames rather than empty polls.
        // The udev/renderer borrows above have dropped by here, freeing
        // `&mut self.metrics`.
        if rendered {
            self.metrics.record_frame(frame_us);
        }

        // Keep wp_fractional_scale clients on this output in sync with its
        // current scale (no-op when unchanged).
        crate::scale::send_preferred_scale(&self.space, &output);

        // Send wl_surface.frame callbacks so clients know to draw the next
        // buffer; otherwise frame-callback-driven clients sit idle. Covers
        // layer-shell surfaces too, not just toplevels.
        let elapsed = self.start_time.elapsed();
        crate::presentation::send_frames(&self.space, &output, elapsed);

        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();

        // If we didn't actually render (no damage), no VBlank will come — so
        // schedule another check after a frame.
        if !rendered && session_active {
            let refresh_mhz = output.current_mode().map(|m| m.refresh).unwrap_or(60_000);
            let frame_us = 1_000_000_000u64 / refresh_mhz as u64;
            let timer = Timer::from_duration(Duration::from_micros(frame_us));
            let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
                state.render_surface(node, crtc);
                TimeoutAction::Drop
            });
        }
    }
}
