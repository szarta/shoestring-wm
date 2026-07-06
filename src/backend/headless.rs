//! Native headless backend: surfaceless EGL on a DRM render node, one virtual
//! output, and a timer-driven render loop — no physical display, no DRM
//! scanout. This is the foundation for remote-desktop *serve mode* (a beefy
//! headless box runs shoestring-wm + apps; a thin client views and drives it),
//! but it is independently valuable for CI, the WLCS harness, and automation,
//! all of which want a real renderer without a window or a TTY.
//!
//! How it differs from the other two backends:
//! - **winit** renders into a nested window's framebuffer; **udev** scans out to
//!   a CRTC. Headless renders into an offscreen GLES texture that nothing
//!   displays — the pixels are reached only via the capture path
//!   (`wlr-screencopy` / `ext-image-copy-capture`) and the streaming damage
//!   subscription ([`crate::capture_stream`], the remote-desktop serve path).
//! - There is no vblank to pace the loop, so a calloop [`Timer`] drives the
//!   render at a fixed cadence (the winit backend's `request_redraw` loop is the
//!   moral equivalent).
//!
//! The EGL setup mirrors the udev backend's, minus the DRM device: we open a
//! render node (`/dev/dri/renderD128` by default, or `$SHOESTRING_WM_RENDER_NODE`),
//! wrap it in a [`GbmDevice`], and build a plain [`GlesRenderer`] on a surfaceless
//! context. Render nodes need no DRM master / logind seat, so this works for a
//! user who simply logs into the box and launches the WM.
//!
//! **GPU-less hosts** (CI, cheap cloud, containers with no `/dev/dri`): when no
//! render node can be opened we fall back to a *surfaceless EGL platform*
//! (`EGL_MESA_platform_surfaceless`), which Mesa backs with the llvmpipe
//! software rasterizer — no GBM, no DRM node, just CPU rendering. The offscreen
//! render target is a GLES texture (not a GBM buffer), so compositing and the
//! shm capture/stream path work identically; only dmabuf import/export needs a
//! real GPU, so on the software path we simply don't advertise
//! `zwp_linux_dmabuf_v1` (it would carry zero formats). `$SHOESTRING_WM_HEADLESS_SOFTWARE=1`
//! forces this path even on a box that has a GPU (set `LIBGL_ALWAYS_SOFTWARE=1`
//! too if a real GPU is present and you want to *guarantee* llvmpipe).
//!
//! GPU clients that need `zwp_linux_dmabuf_v1` are served too: at init we
//! advertise a dmabuf global built from the offscreen renderer's
//! `dmabuf_render_formats()` (task 182), mirroring udev's global minus DRM
//! scanout. The same renderer backs both the per-frame composite and the
//! [`DmabufHandler`](crate::handlers::dmabuf) import test, so it is shared
//! (single-threaded calloop, never borrowed re-entrantly) via [`HeadlessData`].

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, gbm::GbmDevice, Fourcc},
        drm::DrmNode,
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
            Bind, ImportDma, Offscreen,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    utils::Transform,
};

use crate::{backend::scale_from_config, state::ShoestringWm};

/// Native-headless backend handle stored in [`ShoestringWm::headless`]. Owns a
/// shared reference to the offscreen GLES renderer (also held by the render
/// timer) plus the DRM render node it runs on. Lets the `zwp_linux_dmabuf_v1`
/// import test and the ext-image-copy dmabuf constraints reach the renderer
/// without threading it through state.
///
/// The renderer is shared with the render loop via `Rc<RefCell<…>>`: the WM is
/// single-threaded (one calloop), and the import test runs during client
/// dispatch while the render loop runs in its own timer callback, so the two
/// never borrow the renderer at the same time.
#[cfg(feature = "headless")]
pub struct HeadlessData {
    renderer: Rc<RefCell<GlesRenderer>>,
    /// The render node the offscreen renderer runs on, advertised to
    /// ext-image-copy-capture clients as where to allocate dmabuf buffers.
    /// `None` if the node couldn't be derived from the device path.
    pub render_node: Option<DrmNode>,
}

#[cfg(feature = "headless")]
impl HeadlessData {
    /// Try to import `dmabuf` into the offscreen renderer. Backs
    /// [`crate::state::ShoestringWm::import_dmabuf_test`] — the validation the
    /// `zwp_linux_dmabuf_v1` protocol runs before creating a wl_buffer. Mirrors
    /// the udev backend's test against the primary GPU.
    pub fn import_dmabuf_test(&self, dmabuf: &Dmabuf) -> bool {
        self.renderer
            .borrow_mut()
            .import_dmabuf(dmabuf, None)
            .is_ok()
    }
}

/// Default virtual-output size when neither config nor env overrides it. A
/// laptop-ish 1080p so a screenshot of the virtual head is immediately useful.
const DEFAULT_SIZE: (i32, i32) = (1920, 1080);

/// Frame cadence for the render loop. No vblank exists to pace us, so we tick a
/// timer at ~60 Hz. `render_output` early-outs when there is no damage, so an
/// idle desktop costs only the per-frame element build and produces no capture
/// traffic ("idle ⇒ 0 frames" on the stream). Idling the *timer* itself when
/// nothing's pending is a later optimization.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Resolve the render node path: `$SHOESTRING_WM_RENDER_NODE`, else the first of
/// `/dev/dri/renderD128..130` that exists, else the canonical default (so the
/// open error names a real path).
fn render_node_path() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("SHOESTRING_WM_RENDER_NODE") {
        return p.into();
    }
    for n in 128..=130 {
        let p = std::path::PathBuf::from(format!("/dev/dri/renderD{n}"));
        if p.exists() {
            return p;
        }
    }
    "/dev/dri/renderD128".into()
}

/// Whether `$name` is set to a truthy value (`1`/`true`/`yes`).
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Build the headless EGL display, preferring a real GPU render node (GBM, which
/// gives dmabuf import/export) and falling back to a surfaceless software EGL
/// platform (Mesa llvmpipe) when no node is usable. Returns the display and the
/// DRM render node it bound — `None` on the software path, where there is no node
/// to advertise to dmabuf clients.
///
/// Selection:
/// - `$SHOESTRING_WM_HEADLESS_SOFTWARE=1` forces the surfaceless path outright
///   (CPU-only CI even on a box that has a GPU).
/// - otherwise try the GBM render node; if it can't be opened or EGL rejects it
///   (no `/dev/dri`, permissions, ...) fall back to surfaceless rather than
///   failing, so a GPU-less host still produces frames. The failure is logged at
///   `warn` so a genuine GPU misconfiguration is still visible.
fn create_egl_display() -> Result<(EGLDisplay, Option<DrmNode>)> {
    if env_flag("SHOESTRING_WM_HEADLESS_SOFTWARE") {
        tracing::info!(
            "headless: SHOESTRING_WM_HEADLESS_SOFTWARE set; using surfaceless software EGL (llvmpipe)"
        );
        return Ok((surfaceless_display()?, None));
    }
    match gbm_display() {
        Ok(pair) => Ok(pair),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "headless: no usable GPU render node; falling back to surfaceless software EGL (llvmpipe)"
            );
            Ok((surfaceless_display()?, None))
        }
    }
}

/// Open the configured DRM render node, wrap it in GBM, and build an EGL display
/// on it. The display takes ownership of the GBM device (kept alive inside the
/// renderer's context for the session). Also derives the [`DrmNode`] advertised
/// to ext-image-copy-capture as where clients should allocate dmabuf buffers
/// (best-effort: `None` if the path isn't a recognizable DRM node).
fn gbm_display() -> Result<(EGLDisplay, Option<DrmNode>)> {
    let path = render_node_path();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open render node {}", path.display()))?;
    let fd: OwnedFd = file.into();
    let gbm = GbmDevice::new(fd).context("create gbm device on render node")?;
    let egl_display =
        unsafe { EGLDisplay::new(gbm) }.context("create EGL display on render node")?;
    let render_node = match DrmNode::from_path(&path) {
        Ok(node) => Some(node),
        Err(e) => {
            tracing::warn!(node = %path.display(), error = %e, "headless: DrmNode::from_path failed");
            None
        }
    };
    Ok((egl_display, render_node))
}

/// Build a surfaceless EGL display (`EGL_MESA_platform_surfaceless`). No GBM, no
/// DRM node — Mesa backs it with llvmpipe when no GPU is present.
fn surfaceless_display() -> Result<EGLDisplay> {
    unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }
        .context("create surfaceless EGL display (software/llvmpipe)")
}

/// Parse `$SHOESTRING_WM_HEADLESS_SIZE` (`WIDTHxHEIGHT`, e.g. `2560x1440`),
/// falling back to [`DEFAULT_SIZE`] when unset or malformed.
fn virtual_size() -> (i32, i32) {
    let Some(spec) = std::env::var_os("SHOESTRING_WM_HEADLESS_SIZE") else {
        return DEFAULT_SIZE;
    };
    let spec = spec.to_string_lossy();
    let parse = || -> Option<(i32, i32)> {
        let (w, h) = spec.split_once(['x', 'X'])?;
        let w: i32 = w.trim().parse().ok()?;
        let h: i32 = h.trim().parse().ok()?;
        (w > 0 && h > 0).then_some((w, h))
    };
    parse().unwrap_or_else(|| {
        tracing::warn!(%spec, "invalid SHOESTRING_WM_HEADLESS_SIZE; using default");
        DEFAULT_SIZE
    })
}

pub fn init_headless(
    event_loop: &mut EventLoop<'static, ShoestringWm>,
    state: &mut ShoestringWm,
) -> Result<()> {
    // Build an EGL display and a surfaceless GLES renderer. Preference order: a
    // real GPU render node via GBM (unprivileged renderD* — no DRM master, no
    // seat — and gives dmabuf import/export), else a surfaceless software EGL
    // platform (Mesa llvmpipe) so a host with no usable `/dev/dri` still renders
    // real frames. `render_node` is `Some` only on the GPU path; `None` marks
    // the software path (nothing to advertise to dmabuf clients).
    let (egl_display, render_node) = create_egl_display()?;
    let software = render_node.is_none();
    let egl_context = EGLContext::new(&egl_display).context("create EGL context")?;
    let renderer = unsafe {
        let caps = GlesRenderer::supported_capabilities(&egl_context)
            .context("query GLES capabilities")?;
        GlesRenderer::with_capabilities(egl_context, caps).context("create GLES renderer")?
    };
    tracing::info!(software, "headless renderer created");

    // Advertise `zwp_linux_dmabuf_v1` off this renderer's import formats so
    // GPU-accelerated clients can hand us dmabuf surface buffers (the udev
    // backend does the same off the primary GPU). This is a general GPU
    // facility, NOT behind the screen-capture gate — only the screencopy
    // `linux_dmabuf` *event* respects that gate. There is no DRM scanout here,
    // so unlike udev we never feed these formats to a DrmOutputManager.
    let dmabuf_formats: smithay::backend::allocator::format::FormatSet = renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect();
    if state.dmabuf_global.is_none() {
        if dmabuf_formats.iter().next().is_none() {
            // Software/llvmpipe path: the renderer imports no dmabuf formats.
            // Advertising a zero-format `zwp_linux_dmabuf_v1` is useless and only
            // confuses GPU clients, so we skip the global entirely — every client
            // still works over shm buffers.
            tracing::info!(
                "headless: renderer exposes no dmabuf formats (software); zwp_linux_dmabuf_v1 not advertised"
            );
        } else {
            let dh = state.display_handle.clone();
            let global = state
                .dmabuf_state
                .create_global::<ShoestringWm>(&dh, dmabuf_formats.iter().copied());
            state.dmabuf_global = Some(global);
            tracing::info!(
                formats = dmabuf_formats.iter().count(),
                "zwp_linux_dmabuf_v1 global created (headless)"
            );
            state.dmabuf_formats = Some(dmabuf_formats);
        }
    }

    // One virtual output, sized from the environment (a future remote client
    // will resize it to its own pixel size) at the configured scale.
    let (w, h) = virtual_size();
    let mode = Mode {
        size: (w, h).into(),
        refresh: 60_000,
    };
    let scale = scale_from_config(state.config.general.output_scale);
    let output = Output::new(
        "HEADLESS-1".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "shoestring-wm".into(),
            model: "Headless".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<ShoestringWm>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(scale),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.space.map_output(&output, (0, 0));
    tracing::info!(
        width = w,
        height = h,
        scale = state.config.general.output_scale,
        "virtual output mapped"
    );

    state.emit_ipc(shoestring_ipc::Event::OutputAdded(
        shoestring_ipc::OutputSummary {
            name: output.name(),
            width: mode.size.w,
            height: mode.size.h,
            scale: state.config.general.output_scale,
            adaptive_sync: false,
            transform: "normal".to_string(),
        },
    ));
    // Register with wlr-output-management + ext-workspace exactly as the other
    // backends do on output add (bumps the manager serial off zero).
    crate::output_management::broadcast_head_added(state, &output);
    crate::ext_workspace::broadcast_output_enter(state, &output);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    // Share the renderer between the render loop (below) and the dmabuf import
    // test ([`HeadlessData::import_dmabuf_test`]). Single-threaded calloop, so
    // the `RefCell` is never borrowed re-entrantly: import runs during client
    // dispatch, the loop runs in its own timer callback.
    let renderer = Rc::new(RefCell::new(renderer));
    state.headless = Some(HeadlessData {
        renderer: renderer.clone(),
        render_node,
    });
    // The offscreen target: the texture, its size, and its buffer *age*.
    // Lazily (re)created to the output's current pixel size; nothing scans it
    // out — it exists so the scene is composited with real damage tracking and
    // so the capture path has a consistent renderer. The age is what makes
    // damage tracking incremental: a freshly created buffer has unknown
    // contents (age 0 → full damage), but since we reuse the *same* texture
    // every frame it is exactly one frame old thereafter (age 1 → only the
    // regions that actually changed). Without this the tracker would report
    // full-output damage every frame and the capture stream would never idle.
    let mut target: Option<(
        GlesTexture,
        smithay::utils::Size<i32, smithay::utils::Buffer>,
        usize,
    )> = None;

    let timer = Timer::immediate();
    event_loop
        .handle()
        .insert_source(timer, move |_instant, _meta, state| {
            render_once(
                state,
                &output,
                &mut renderer.borrow_mut(),
                &mut damage_tracker,
                &mut target,
            );
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .map_err(|e| anyhow::anyhow!("failed to insert headless render timer: {e}"))?;

    Ok(())
}

/// One render tick: compose the scene, fulfil pending captures, paint into the
/// offscreen target, then send frame callbacks and flush. Mirrors the winit
/// `Redraw` handler, minus the window submit/vsync.
fn render_once(
    state: &mut ShoestringWm,
    output: &Output,
    renderer: &mut GlesRenderer,
    damage_tracker: &mut OutputDamageTracker,
    target: &mut Option<(
        GlesTexture,
        smithay::utils::Size<i32, smithay::utils::Buffer>,
        usize,
    )>,
) {
    let Some(mode) = output.current_mode() else {
        return;
    };
    let size: smithay::utils::Size<i32, smithay::utils::Buffer> = (mode.size.w, mode.size.h).into();

    // (Re)create the offscreen texture if missing or the output resized. A
    // fresh buffer starts at age 0 (contents unknown → full repaint).
    if target.as_ref().map(|(_, s, _)| *s) != Some(size) {
        match renderer.create_buffer(Fourcc::Argb8888, size) {
            Ok(tex) => *target = Some((tex, size, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "headless: offscreen create_buffer failed");
                return;
            }
        }
    }
    let Some((texture, _, age)) = target.as_mut() else {
        return;
    };
    let buffer_age = *age;

    // Build the composited scene (shared with the winit backend). The
    // screen-only diagnostics overlay is added below, after the capture pass,
    // so it stays out of screenshots.
    let (mut elements, clear) = state.compose_scene(output, renderer);

    let show_overlay = !state.is_locked() && state.is_diag_overlay_output(output);
    if show_overlay {
        state.refresh_diag_overlay(output);
    }

    // Fulfil pending wlr-screencopy / ext-image-copy captures first. Each binds
    // its own offscreen/dmabuf target with the same `elements` + `clear`, so
    // captures match the scene; doing it before our own bind keeps the texture
    // bind below last.
    crate::screencopy::process_pending(&mut state.screencopy, output, renderer, &elements, clear);
    crate::ext_screencopy::process_pending(
        &mut state.ext_screencopy,
        output,
        renderer,
        &elements,
        clear,
    );

    // Graft mode: render any subscribed single windows to their own offscreen
    // buffers and stream their damage tiles. Independent of the output's own
    // damage (a grafted window may live on another workspace), so it runs here —
    // next to the screencopy passes, before the main framebuffer is bound.
    crate::window_capture::push_window_capture(state, renderer);

    // Diagnostics overlay on top of everything (index 0 = drawn first), added
    // after the capture pass so it stays out of captures.
    if show_overlay {
        let scale_int = (output.current_scale().fractional_scale().ceil() as i32).max(1);
        if let Some(el) = state.diag_overlay.element(renderer, scale_int) {
            elements.insert(0, crate::drawing::OutputRenderElements::Memory(el));
        }
    }

    // Paint the scene into the offscreen texture with real damage tracking.
    let render_states = {
        let mut framebuffer = match renderer.bind(texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "headless: bind offscreen failed");
                return;
            }
        };
        let render_start = std::time::Instant::now();
        let result = match damage_tracker.render_output(
            renderer,
            &mut framebuffer,
            buffer_age,
            &elements,
            clear,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = ?e, "headless: render_output failed");
                return;
            }
        };
        state
            .metrics
            .record_frame(render_start.elapsed().as_micros() as u64);
        // Stream the damaged tiles to any capture subscribers (serve mode),
        // reading them back from the framebuffer we just rendered. No-op when
        // the gate is off or nobody's subscribed.
        crate::capture_stream::push_capture(
            state,
            output,
            renderer,
            &framebuffer,
            result.damage.map(|v| v.as_slice()),
        );
        result.states
    };
    // The texture now holds this frame; next time it is exactly one frame old.
    *age = 1;
    crate::profiling::frame_mark();

    // wp_presentation, best effort: no hardware vblank, so mark the frame
    // presented at render time against the advertised clock (Unknown refresh,
    // no HwClock/HwCompletion). Without this, clients awaiting feedback hang.
    {
        use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
        crate::presentation::update_primary_scanout_output(&state.space, output, &render_states);
        let mut feedback =
            crate::presentation::take_presentation_feedback(&state.space, output, &render_states);
        feedback.presented(
            state.clock.now(),
            smithay::wayland::presentation::Refresh::Unknown,
            0,
            Kind::Vsync,
        );
    }

    crate::scale::send_preferred_scale(&state.space, output);
    crate::presentation::send_frames(&state.space, output, state.start_time.elapsed());

    state.space.refresh();
    state.popups.cleanup();
    let _ = state.display_handle.flush_clients();
}
