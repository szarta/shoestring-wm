//! Native DRM/KMS + libinput + libseat backend, for running shoestring-wm
//! from a TTY without an outer Wayland/X11 session.
//!
//! Aggressively stripped vs. anvil's reference:
//! - single primary GPU only (no multi-GPU fallback paths)
//! - one scale across all outputs (from `general.output_scale` in config)
//! - no XWayland, dmabuf-feedback, syncobj, DRM lease, screencopy
//! - no cursor plane / pointer image (cursor not drawn; M8/M9 territory)
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

use anyhow::Result;
use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
            CreateDrmNodeError, DrmDevice, DrmDeviceFd, DrmError, DrmEvent, DrmNode, NodeType,
        },
        egl::{self, context::ContextPriority, EGLContext, EGLDevice, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            element::surface::WaylandSurfaceRenderElement,
            gles::{Capability, GlesRenderer},
            multigpu::{gbm::GbmGlesBackend, GpuManager, MultiRenderer},
        },
        session::{
            libseat::{self, LibSeatSession},
            Event as SessionEvent, Session,
        },
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
    },
    desktop::space::SpaceRenderElements,
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

type ShoestringDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

type ShoestringOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

/// User-data tag we hang on each [`Output`] so VBlank handlers can find the
/// [`SurfaceData`] that owns the crtc that just flipped.
#[derive(Debug, PartialEq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

pub struct UdevData {
    pub session: LibSeatSession,
    primary_gpu: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, BackendData>,
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

    state.udev = Some(UdevData {
        session,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
    });

    // udev: notifies us when DRM devices come and go.
    let udev_backend = UdevBackend::new(&seat_name)
        .map_err(|e| anyhow::anyhow!("udev backend init failed: {e}"))?;

    // libinput: keyboard, pointer, touch. libseat hands fds out via the
    // SessionInterface so we keep working across VT switches.
    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        state.udev.as_ref().unwrap().session.clone().into(),
    );
    libinput_context
        .udev_assign_seat(&seat_name)
        .map_err(|_| anyhow::anyhow!("libinput could not assign seat {seat_name}"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, state| {
            state.process_input_event(event);
        })
        .map_err(|e| anyhow::anyhow!("insert libinput source: {e}"))?;

    // Session pause/resume on VT switch. We drop libinput when paused so we
    // don't fight whoever owns the foreground VT, then reactivate every drm
    // device on resume and re-render once.
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, &mut (), state| match event {
            SessionEvent::PauseSession => {
                tracing::info!("session paused (vt switch away)");
                libinput_context.suspend();
                if let Some(udev) = state.udev.as_mut() {
                    for backend in udev.backends.values_mut() {
                        backend.drm_output_manager.pause();
                    }
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("session activated (vt switch in)");
                if let Err(e) = libinput_context.resume() {
                    tracing::warn!(error = ?e, "libinput resume failed");
                }
                let Some(udev) = state.udev.as_mut() else {
                    return;
                };
                let nodes: Vec<DrmNode> = udev.backends.keys().copied().collect();
                for node in &nodes {
                    if let Some(backend) = udev.backends.get_mut(node) {
                        if let Err(e) = backend.drm_output_manager.lock().activate(false) {
                            tracing::error!(?node, error = ?e, "drm activate failed");
                        }
                    }
                }
                // Kick a render on every surface so the screen comes back.
                for node in nodes {
                    let crtcs: Vec<crtc::Handle> = state
                        .udev
                        .as_ref()
                        .and_then(|u| u.backends.get(&node))
                        .map(|b| b.surfaces.keys().copied().collect())
                        .unwrap_or_default();
                    for crtc in crtcs {
                        state.render_surface(node, crtc);
                    }
                }
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
        .insert_source(drm_notifier, move |event, _meta, state| match event {
            DrmEvent::VBlank(crtc) => state.frame_finish(node, crtc),
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

    // Lay outputs left-to-right in the order they arrive. M9 will add a
    // config-driven arrangement.
    let x = state.space.outputs().fold(0, |acc, o| {
        acc + state
            .space
            .output_geometry(o)
            .map(|g| g.size.w)
            .unwrap_or(0)
    });
    let position = (x, 0).into();
    let scale = crate::backend::scale_from_config(state.config.general.output_scale);
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), None, Some(scale), Some(position));
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

    let surface = SurfaceData {
        output: output.clone(),
        global: Some(global),
        drm_output,
    };
    device.surfaces.insert(crtc, surface);

    state.emit_ipc(shoestring_ipc::Event::OutputAdded(
        shoestring_ipc::OutputSummary {
            name: output.name(),
            width: wl_mode.size.w,
            height: wl_mode.size.h,
            scale: state.config.general.output_scale,
        },
    ));

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
        state.space.unmap_output(&surface.output);
        state.space.refresh();
        surface.output.leave_all();
        if let Some(global) = surface.global {
            state.display_handle.remove_global::<ShoestringWm>(global);
        }
        state.emit_ipc(shoestring_ipc::Event::OutputRemoved { name });
        tracing::info!(?node, ?crtc, conn = ?connector.handle(), "connector disconnected");
    }
}

impl ShoestringWm {
    /// VBlank handler: the GPU just finished presenting a frame on `crtc`.
    /// Acknowledge it and schedule the next render one frame later.
    pub(crate) fn frame_finish(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(udev) = self.udev.as_mut() else {
            return;
        };
        let Some(device) = udev.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        if let Err(e) = surface.drm_output.frame_submitted() {
            tracing::warn!(?crtc, error = ?e, "frame_submitted failed");
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
                return;
            }
        };

        let elements: Vec<
            SpaceRenderElements<UdevRenderer<'_>, WaylandSurfaceRenderElement<UdevRenderer<'_>>>,
        > = smithay::desktop::space::space_render_elements(
            &mut renderer,
            [&self.space],
            &output,
            1.0,
        )
        .unwrap_or_default();

        let result = surface.drm_output.render_frame(
            &mut renderer,
            &elements,
            [0.1, 0.1, 0.1, 1.0],
            FrameFlags::DEFAULT,
        );

        let rendered = match result {
            Ok(frame) => !frame.is_empty,
            Err(e) => {
                tracing::warn!(?crtc, error = ?e, "render_frame failed");
                // Best-effort: reschedule one frame later so we can try again
                // (e.g. PermissionDenied during a brief VT race).
                let timer = Timer::from_duration(Duration::from_millis(16));
                let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
                    state.render_surface(node, crtc);
                    TimeoutAction::Drop
                });
                return;
            }
        };

        if rendered {
            if let Err(e) = surface.drm_output.queue_frame(()) {
                tracing::warn!(?crtc, error = ?e, "queue_frame failed");
            }
        }

        // Send wl_surface.frame callbacks so clients know to draw the next
        // buffer; otherwise frame-callback-driven clients sit idle.
        let elapsed = self.start_time.elapsed();
        self.space.elements().for_each(|w| {
            w.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        });

        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();

        // If we didn't actually render (no damage), no VBlank will come — so
        // schedule another check after a frame.
        if !rendered {
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
