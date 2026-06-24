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
//! Scope (keystone): shm clients and the capture path. GPU clients that need
//! `zwp_linux_dmabuf_v1` aren't served yet — advertising the dmabuf global off
//! the headless renderer's formats is a follow-up.

use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{gbm::GbmDevice, Fourcc},
        egl::{EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
            Bind, Offscreen,
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
    // Open a render node and build a surfaceless GLES renderer on it. A render
    // node (renderD*) is unprivileged — no DRM master, no seat — so this needs
    // no logind/libseat session, matching "user logs into the box and launches
    // the WM".
    let path = render_node_path();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open render node {}", path.display()))?;
    let fd: OwnedFd = file.into();
    let gbm = GbmDevice::new(fd).context("create gbm device on render node")?;
    // EGLDisplay takes ownership of the gbm device (kept alive inside the
    // renderer's context for the session).
    let egl_display =
        unsafe { EGLDisplay::new(gbm) }.context("create EGL display on render node")?;
    let egl_context = EGLContext::new(&egl_display).context("create EGL context")?;
    let renderer = unsafe {
        let caps = GlesRenderer::supported_capabilities(&egl_context)
            .context("query GLES capabilities")?;
        GlesRenderer::with_capabilities(egl_context, caps).context("create GLES renderer")?
    };
    tracing::info!(node = %path.display(), "headless renderer created");

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
    let mut renderer = renderer;
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
                &mut renderer,
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
