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
//! **Renderers.** `[headless] renderer` (or `--renderer`) picks what composites
//! the output: `gles` on a GPU render node, `llvmpipe` (GLES on Mesa's software
//! rasterizer), or `pixman`, a CPU compositor that needs no GPU or EGL at all.
//! `auto`, the default, takes the GPU when a render node opens and pixman
//! otherwise. For desktop content pixman costs a small fraction of llvmpipe's
//! CPU: llvmpipe runs every frame through a JIT-compiled GL pipeline and a
//! thread pool, pixman blits. The render loop and the capture paths are
//! written once for any renderer (see [`crate::render::OffscreenRenderer`]).
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
use std::fmt::Display;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use shoestring_config::HeadlessRenderer;
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, gbm::GbmDevice, Fourcc},
        drm::DrmNode,
        egl::{native::EGLSurfacelessDisplay, EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            element::{surface::WaylandSurfaceRenderElement, RenderElement},
            gles::GlesRenderer,
            pixman::PixmanRenderer,
            Bind, ExportMem, ImportAll, ImportDma, ImportMem, RendererSuper, Texture,
        },
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop, LoopHandle, RegistrationToken,
    },
    utils::{Buffer, Size, Transform},
};

use crate::{
    backend::scale_from_config, drawing::OutputRenderElements, render::OffscreenRenderer,
    state::ShoestringWm,
};

/// Native-headless backend handle stored in [`ShoestringWm::headless`]: the
/// render tick for whichever renderer was chosen at startup, the timer that
/// drives it, and, on the GLES renderers, the renderer itself and the DRM render
/// node it runs on. Lets the `zwp_linux_dmabuf_v1` import test and the
/// ext-image-copy dmabuf constraints reach the renderer without threading it
/// through state.
///
/// The renderer is shared with the render tick via `Rc<RefCell<…>>`: the WM is
/// single-threaded (one calloop), and the import test runs during client
/// dispatch while the render tick runs in its own timer callback, so the two
/// never borrow the renderer at the same time.
#[cfg(feature = "headless")]
pub struct HeadlessData {
    /// The GLES renderer (GPU or llvmpipe); `None` with pixman, which offers
    /// clients no dmabuf to import.
    gles: Option<Rc<RefCell<GlesRenderer>>>,
    /// The render node the offscreen renderer runs on, advertised to
    /// ext-image-copy-capture clients as where to allocate dmabuf buffers.
    /// `None` on the software renderers, or if the node couldn't be derived
    /// from the device path.
    pub render_node: Option<DrmNode>,
    /// One render tick; returns whether the output changed. Built for the
    /// chosen renderer by [`render_tick`], and shared by each (re-)armed timer.
    tick: RenderTick,
    /// The render timer currently in the event loop.
    timer: Option<RegistrationToken>,
    /// A tick found no damage with `[headless] idle_suspend` on, so the timer
    /// only ticks every [`PARKED_HEARTBEAT`] until [`wake`] re-arms it.
    parked: bool,
    /// Parked during the current loop iteration; see [`after_dispatch`].
    parked_this_iteration: bool,
}

/// One render tick, returning whether the output changed; see [`render_tick`].
type RenderTick = Rc<RefCell<dyn FnMut(&mut ShoestringWm) -> bool>>;

#[cfg(feature = "headless")]
impl HeadlessData {
    /// Try to import `dmabuf` into the offscreen renderer. Backs
    /// [`crate::state::ShoestringWm::import_dmabuf_test`] — the validation the
    /// `zwp_linux_dmabuf_v1` protocol runs before creating a wl_buffer. Mirrors
    /// the udev backend's test against the primary GPU.
    pub fn import_dmabuf_test(&self, dmabuf: &Dmabuf) -> bool {
        self.gles
            .as_ref()
            .is_some_and(|r| r.borrow_mut().import_dmabuf(dmabuf, None).is_ok())
    }
}

/// Default virtual-output size when neither config nor env overrides it. A
/// laptop-ish 1080p so a screenshot of the virtual head is immediately useful.
const DEFAULT_SIZE: (i32, i32) = (1920, 1080);

/// Frame cadence for the render loop. No vblank exists to pace us, so we tick a
/// timer at ~60 Hz. `render_output` early-outs when there is no damage, so an
/// idle desktop costs only the per-frame element build (about 0.3 ms of CPU)
/// and produces no capture traffic ("idle ⇒ 0 frames" on the stream). With
/// `[headless] idle_suspend`, the timer itself parks while idle.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// How often a parked render loop still ticks, so a missed wake can only leave
/// the output stale for this long.
const PARKED_HEARTBEAT: Duration = Duration::from_secs(1);

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

/// The renderer the headless backend composites with.
enum Chosen {
    /// GLES, on a GPU render node (`render_node` is `Some`) or llvmpipe.
    Gles {
        renderer: Box<GlesRenderer>,
        render_node: Option<DrmNode>,
    },
    Pixman(PixmanRenderer),
}

/// Build the renderer `choice` asks for.
///
/// - `gles` opens the GBM render node and fails if it can't.
/// - `llvmpipe` builds GLES on a surfaceless EGL platform, which Mesa backs
///   with llvmpipe when no GPU is present.
/// - `pixman` needs no EGL at all.
/// - `auto` tries the render node and falls back to pixman, logging the
///   failure at `warn` so a genuine GPU misconfiguration is still visible.
///   `$SHOESTRING_WM_HEADLESS_SOFTWARE=1` skips the GPU (CPU-only CI even on a
///   box that has one).
fn choose_renderer(choice: HeadlessRenderer) -> Result<Chosen> {
    let gles = |(display, render_node): (EGLDisplay, Option<DrmNode>)| -> Result<Chosen> {
        let context = EGLContext::new(&display).context("create EGL context")?;
        let renderer = unsafe {
            let caps = GlesRenderer::supported_capabilities(&context)
                .context("query GLES capabilities")?;
            GlesRenderer::with_capabilities(context, caps).context("create GLES renderer")?
        };
        Ok(Chosen::Gles {
            renderer: Box::new(renderer),
            render_node,
        })
    };
    let pixman = || -> Result<Chosen> {
        Ok(Chosen::Pixman(
            PixmanRenderer::new().context("create pixman renderer")?,
        ))
    };
    match choice {
        HeadlessRenderer::Gles => gles(gbm_display()?),
        HeadlessRenderer::Llvmpipe => gles((surfaceless_display()?, None)),
        HeadlessRenderer::Pixman => pixman(),
        HeadlessRenderer::Auto if env_flag("SHOESTRING_WM_HEADLESS_SOFTWARE") => {
            tracing::info!("headless: SHOESTRING_WM_HEADLESS_SOFTWARE set; using pixman");
            pixman()
        }
        HeadlessRenderer::Auto => match gbm_display().and_then(gles) {
            Ok(chosen) => Ok(chosen),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "headless: no usable GPU render node; falling back to pixman"
                );
                pixman()
            }
        },
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
    crate::parse_output_size(&spec).unwrap_or_else(|| {
        tracing::warn!(%spec, "invalid SHOESTRING_WM_HEADLESS_SIZE; using default");
        DEFAULT_SIZE
    })
}

pub fn init_headless(
    event_loop: &mut EventLoop<'static, ShoestringWm>,
    state: &mut ShoestringWm,
) -> Result<()> {
    // Every headless renderer can render a single window offscreen, so
    // per-window capture (graft, screenshot_window) is available.
    state.window_capture_supported = true;
    state.output_probe_supported = true;
    let choice = state.config.headless.renderer;
    let chosen = choose_renderer(choice)?;

    // On a GPU, advertise `zwp_linux_dmabuf_v1` off the renderer's import
    // formats so GPU-accelerated clients can hand us dmabuf surface buffers (the
    // udev backend does the same off the primary GPU). This is a general GPU
    // facility, NOT behind the screen-capture gate — only the screencopy
    // `linux_dmabuf` *event* respects that gate. There is no DRM scanout here,
    // so unlike udev we never feed these formats to a DrmOutputManager. The
    // software renderers import no dmabuf formats, and a zero-format global only
    // confuses GPU clients, so they get none: every client still works over shm.
    let (gles, render_node) = match &chosen {
        Chosen::Gles {
            renderer,
            render_node,
        } => {
            tracing::info!(
                software = render_node.is_none(),
                "headless renderer created (GLES)"
            );
            let dmabuf_formats: smithay::backend::allocator::format::FormatSet = renderer
                .egl_context()
                .dmabuf_render_formats()
                .iter()
                .copied()
                .collect();
            if state.dmabuf_global.is_none() && dmabuf_formats.iter().next().is_some() {
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
            (true, *render_node)
        }
        Chosen::Pixman(_) => {
            tracing::info!("headless renderer created (pixman)");
            (false, None)
        }
    };
    if !gles || state.dmabuf_global.is_none() {
        tracing::info!(
            "headless: no dmabuf formats (software renderer); zwp_linux_dmabuf_v1 not advertised"
        );
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

    // Share a GLES renderer between the render tick and the dmabuf import test
    // ([`HeadlessData::import_dmabuf_test`]). Single-threaded calloop, so the
    // `RefCell` is never borrowed re-entrantly: import runs during client
    // dispatch, the tick runs in its own timer callback.
    let (gles, tick) = match chosen {
        Chosen::Gles { renderer, .. } => {
            let renderer = Rc::new(RefCell::new(*renderer));
            (Some(renderer.clone()), render_tick(renderer, output))
        }
        Chosen::Pixman(renderer) => (None, render_tick(Rc::new(RefCell::new(renderer)), output)),
    };
    state.headless = Some(HeadlessData {
        gles,
        render_node,
        tick,
        timer: None,
        parked: false,
        parked_this_iteration: false,
    });
    arm(state, &event_loop.handle());
    if state.headless.as_ref().is_some_and(|h| h.timer.is_none()) {
        anyhow::bail!("failed to insert headless render timer");
    }

    Ok(())
}

/// (Re-)insert the render timer, due now, replacing any current one.
fn arm(state: &mut ShoestringWm, handle: &LoopHandle<'static, ShoestringWm>) {
    let Some(headless) = state.headless.as_mut() else {
        return;
    };
    if let Some(token) = headless.timer.take() {
        handle.remove(token);
    }
    headless.parked = false;
    let tick = headless.tick.clone();
    let inserted = handle.insert_source(Timer::immediate(), move |_, _, state| {
        let damaged = (tick.borrow_mut())(state);
        let park = state.config.headless.idle_suspend && !damaged;
        if let Some(headless) = state.headless.as_mut() {
            headless.parked = park;
            headless.parked_this_iteration = park;
        }
        TimeoutAction::ToDuration(if park {
            PARKED_HEARTBEAT
        } else {
            FRAME_INTERVAL
        })
    });
    match inserted {
        Ok(token) => headless.timer = Some(token),
        Err(e) => tracing::error!(error = %e, "headless: render timer insert failed"),
    }
}

/// Resume full-rate rendering if the loop is parked. Call where something may
/// have changed the scene: a client commit, input, an IPC request.
pub fn wake(state: &mut ShoestringWm) {
    if state.headless.as_ref().is_some_and(|h| h.parked) {
        let handle = state.loop_handle.clone();
        arm(state, &handle);
    }
}

/// After each event-loop iteration: whatever ran may have changed the scene,
/// so wake a parked loop, unless parking was all that happened.
pub fn after_dispatch(state: &mut ShoestringWm) {
    let Some(headless) = state.headless.as_mut() else {
        return;
    };
    if std::mem::take(&mut headless.parked_this_iteration) {
        return;
    }
    wake(state);
}

/// The render tick for `renderer` on `output`: a closure owning the damage
/// tracker and the offscreen target, so the timer needn't know the renderer
/// type.
fn render_tick<R>(renderer: Rc<RefCell<R>>, output: Output) -> RenderTick
where
    R: OffscreenRenderer + ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + 'static,
    R::TextureId: Texture + Clone + Send + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    // The offscreen target: the buffer, its size, and its *age*. Lazily
    // (re)created to the output's current pixel size; nothing scans it out — it
    // exists so the scene is composited with real damage tracking and so the
    // capture path has a consistent renderer. The age is what makes damage
    // tracking incremental: a freshly created buffer has unknown contents (age
    // 0 → full damage), but since we reuse the *same* buffer every frame it is
    // exactly one frame old thereafter (age 1 → only the regions that actually
    // changed). Without this the tracker would report full-output damage every
    // frame and the capture stream would never idle.
    let mut target: Option<(R::Target, Size<i32, Buffer>, usize)> = None;
    Rc::new(RefCell::new(move |state: &mut ShoestringWm| {
        render_once(
            state,
            &output,
            &mut renderer.borrow_mut(),
            &mut damage_tracker,
            &mut target,
        )
    }))
}

/// One render tick: compose the scene, fulfil pending captures, paint into the
/// offscreen target, then send frame callbacks and flush. Mirrors the winit
/// `Redraw` handler, minus the window submit/vsync. Returns whether the output
/// changed (any damage), or `true` when the tick couldn't tell.
fn render_once<R>(
    state: &mut ShoestringWm,
    output: &Output,
    renderer: &mut R,
    damage_tracker: &mut OutputDamageTracker,
    target: &mut Option<(R::Target, Size<i32, Buffer>, usize)>,
) -> bool
where
    R: OffscreenRenderer + ImportAll + ImportMem + ExportMem + Bind<Dmabuf>,
    R::TextureId: Texture + Clone + Send + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    let Some(mode) = output.current_mode() else {
        return true;
    };
    let size: smithay::utils::Size<i32, smithay::utils::Buffer> = (mode.size.w, mode.size.h).into();

    // (Re)create the offscreen texture if missing or the output resized. A
    // fresh buffer starts at age 0 (contents unknown → full repaint).
    if target.as_ref().map(|(_, s, _)| *s) != Some(size) {
        match renderer.create_buffer(Fourcc::Argb8888, size) {
            Ok(tex) => *target = Some((tex, size, 0)),
            Err(e) => {
                tracing::warn!(error = %e, "headless: offscreen create_buffer failed");
                return true;
            }
        }
    }
    let Some((texture, _, age)) = target.as_mut() else {
        return true;
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
    // One-shot per-window screenshots parked by the IPC handler, serviced here
    // where the renderer is in scope (same reason as graft above).
    crate::window_capture::process_pending_window_screenshots(state, renderer);
    // One-shot get_pixel / hash_region probes, serviced the same way.
    crate::screencopy::process_pending_probes(state, output, renderer, &elements, clear);

    // Diagnostics overlay on top of everything (index 0 = drawn first), added
    // after the capture pass so it stays out of captures.
    if show_overlay {
        let scale_int = (output.current_scale().fractional_scale().ceil() as i32).max(1);
        if let Some(el) = state.diag_overlay.element(renderer, scale_int) {
            elements.insert(0, crate::drawing::OutputRenderElements::Memory(el));
        }
    }

    // Paint the scene into the offscreen texture with real damage tracking.
    let (render_states, damaged) = {
        let mut framebuffer = match renderer.bind(texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "headless: bind offscreen failed");
                return true;
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
                return true;
            }
        };
        state
            .metrics
            .record_frame(render_start.elapsed().as_micros() as u64);
        // Stream the damaged tiles to any capture subscribers (serve mode),
        // reading them back from the framebuffer we just rendered. No-op when
        // the gate is off or nobody's subscribed.
        let damaged = result.damage.is_some_and(|d| !d.is_empty());
        crate::capture_stream::push_capture(
            state,
            output,
            renderer,
            &framebuffer,
            result.damage.map(|v| v.as_slice()),
        );
        (result.states, damaged)
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
    damaged
}
