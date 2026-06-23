//! `ext-image-copy-capture-v1` + `ext-image-capture-source-v1` — the modern,
//! standardized successor to wlr-screencopy (which we still implement in
//! [`crate::screencopy`] for grim and friends). Unlike wlr-screencopy this one
//! *is* shipped by smithay as a delegate (`image_copy_capture` +
//! `image_capture_source`), so the protocol wire handling lives upstream and we
//! only implement the handler traits (see [`crate::handlers::ext_screencopy`])
//! plus the render-path capture below.
//!
//! Scope: **output capture** (the common case — newer xdg-desktop-portal,
//! future grim/screenshot tools). Toplevel and cursor sources are deliberately
//! not advertised; clients that need them fall back to wlr-screencopy or are
//! told the source is unsupported.
//!
//! Privacy: the two manager globals are gated by the same screen-capture gate
//! as wlr-screencopy. Rather than create/remove the globals on every toggle (as
//! the single wlr global does), we register them once with a client filter that
//! consults the live gate flag — so a client connecting while the gate is off
//! cannot *discover* the capability, and [`ImageCopyCaptureHandler`] additionally
//! refuses to hand out constraints or capture frames while it is off. Flipping
//! the gate off also stops any live sessions (see
//! [`crate::state::ShoestringWm::set_screen_capture`]).
//!
//! [`ImageCopyCaptureHandler`]: smithay::wayland::image_copy_capture::ImageCopyCaptureHandler

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Fourcc},
        renderer::{
            damage::OutputDamageTracker, element::RenderElement, gles::GlesTexture, Bind,
            ExportMem, ImportAll, ImportMem, Offscreen, Renderer, RendererSuper,
        },
    },
    output::Output,
    utils::{Buffer as BufferCoord, Rectangle, Size, Transform},
    wayland::{
        dmabuf::get_dmabuf,
        image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState},
        image_copy_capture::{Frame, ImageCopyCaptureState},
    },
};

#[cfg(feature = "tty")]
pub use smithay::wayland::image_copy_capture::DmabufConstraints;
pub use smithay::wayland::image_copy_capture::{
    BufferConstraints, CaptureFailureReason, Session, SessionRef,
};

/// A frame whose `capture` request has arrived and which is now waiting for the
/// next render of its target output. Owns the [`Frame`] — dropping it without
/// calling `success`/`fail` makes smithay fail the frame, so we always resolve
/// it explicitly in [`process_pending`].
pub struct PendingExtFrame {
    pub frame: Frame,
    pub output: Output,
}

/// Container on `ShoestringWm` for the ext-image-copy-capture delegate state.
///
/// The three smithay states route protocol requests; `sessions` keeps the owned
/// [`Session`] handles alive (dropping one sends `stopped`), and `pending`
/// holds frames queued for the render path.
pub struct ExtScreencopyState {
    /// Routes `ext_image_capture_source_v1` (the opaque source objects). Creates
    /// no global of its own.
    pub image_capture_source: ImageCaptureSourceState,
    /// `ext_output_image_capture_source_manager_v1` — lets clients turn an
    /// output into a capture source. Gated by the capture filter.
    pub output_capture_source: OutputCaptureSourceState,
    /// `ext_image_copy_capture_manager_v1` — the capture factory. Gated by the
    /// capture filter.
    pub image_copy_capture: ImageCopyCaptureState,
    /// Owned session handles, one per live capture session. Kept alive for the
    /// session's lifetime; cleared when the gate closes or a session is
    /// destroyed.
    pub sessions: Vec<Session>,
    /// Frames awaiting their target output's render pass.
    pub pending: Vec<PendingExtFrame>,
}

impl ExtScreencopyState {
    /// Register the manager globals, gating both behind `gate` (the live
    /// screen-capture flag) via a client visibility filter.
    pub fn new<D>(
        dh: &smithay::reexports::wayland_server::DisplayHandle,
        gate: Arc<AtomicBool>,
    ) -> Self
    where
        D: smithay::wayland::image_copy_capture::ImageCopyCaptureHandler
            + smithay::wayland::image_capture_source::OutputCaptureSourceHandler
            + 'static,
    {
        let g1 = gate.clone();
        let output_capture_source =
            OutputCaptureSourceState::new_with_filter::<D, _>(dh, move |_| {
                g1.load(Ordering::Relaxed)
            });
        let g2 = gate;
        let image_copy_capture =
            ImageCopyCaptureState::new_with_filter::<D, _>(dh, move |_| g2.load(Ordering::Relaxed));
        Self {
            image_capture_source: ImageCaptureSourceState::new(),
            output_capture_source,
            image_copy_capture,
            sessions: Vec::new(),
            pending: Vec::new(),
        }
    }
}

/// Resolve the output backing an output-capture session (stored as a
/// `WeakOutput` in the source's user-data by `output_source_created`).
pub fn output_of_session(session: &SessionRef) -> Option<Output> {
    session
        .source()
        .user_data()
        .get::<smithay::output::WeakOutput>()?
        .upgrade()
}

/// Render every pending ext-capture frame whose target matches `output` into
/// its client buffer and signal completion. Mirrors
/// [`crate::screencopy::process_pending`] but for the ext protocol: capture is
/// always full-output (the source *is* the output), so there is no sub-region
/// to crop. Reuses the wlr path's offscreen-readback and dmabuf-bind helpers so
/// both protocols composite the exact same element list.
pub fn process_pending<R>(
    ext: &mut ExtScreencopyState,
    output: &Output,
    renderer: &mut R,
    elements: &[crate::drawing::OutputRenderElements<
        R,
        smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>,
    >],
    clear: [f32; 4],
) where
    R: Renderer
        + ImportAll
        + ImportMem
        + ExportMem
        + Bind<GlesTexture>
        + Bind<Dmabuf>
        + Offscreen<GlesTexture>,
    <R as RendererSuper>::Error: std::fmt::Display,
    <R as RendererSuper>::TextureId: Clone + 'static,
    crate::drawing::OutputRenderElements<
        R,
        smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>,
    >: RenderElement<R>,
{
    if ext.pending.is_empty() {
        return;
    }

    // Pull out frames for this output; the rest wait for their own render pass.
    let (for_us, others): (Vec<_>, Vec<_>) = std::mem::take(&mut ext.pending)
        .into_iter()
        .partition(|p| &p.output == output);
    ext.pending = others;
    if for_us.is_empty() {
        return;
    }

    let mode = match output.current_mode() {
        Some(m) => m,
        None => {
            for p in for_us {
                p.frame.fail(CaptureFailureReason::Unknown);
            }
            return;
        }
    };
    let output_size = mode.size;
    let scale = output.current_scale().fractional_scale();
    // Capture region is the whole rendered framebuffer.
    let region: Rectangle<i32, BufferCoord> =
        Rectangle::from_size(Size::from((output_size.w, output_size.h)));

    // Split by buffer kind. smithay has already validated each buffer against
    // the session constraints (size + format), so `frame.buffer()` is present.
    let mut shm_frames: Vec<PendingExtFrame> = Vec::new();
    let mut dmabuf_frames: Vec<(PendingExtFrame, Dmabuf)> = Vec::new();
    for p in for_us {
        let buffer = p.frame.buffer();
        match get_dmabuf(&buffer) {
            Ok(dmabuf) => dmabuf_frames.push((p, dmabuf.clone())),
            Err(_) => shm_frames.push(p),
        }
    }

    // dmabuf path: bind the client buffer as the render target and paint the
    // scene straight into it — no readback.
    for (p, mut dmabuf) in dmabuf_frames {
        let mut framebuffer = match renderer.bind(&mut dmabuf) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "ext-screencopy: bind client dmabuf failed");
                p.frame.fail(CaptureFailureReason::Unknown);
                continue;
            }
        };
        let mut damage_tracker =
            OutputDamageTracker::new(output_size, scale, output.current_transform());
        let result = crate::screencopy::render_into::<R>(
            &mut damage_tracker,
            renderer,
            &mut framebuffer,
            elements,
            clear,
        );
        drop(framebuffer);
        match result {
            Ok(()) => finish_frame(p.frame),
            Err(e) => {
                tracing::warn!(error = %e, "ext-screencopy: dmabuf render failed");
                p.frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    if shm_frames.is_empty() {
        return;
    }

    // shm path: render the scene into an offscreen texture once, then read back
    // into each client's shm buffer.
    let texture: GlesTexture =
        match renderer.create_buffer(Fourcc::Argb8888, (output_size.w, output_size.h).into()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "ext-screencopy: offscreen create_buffer failed");
                for p in shm_frames {
                    p.frame.fail(CaptureFailureReason::Unknown);
                }
                return;
            }
        };

    let mut texture = texture;
    let mut framebuffer = match renderer.bind(&mut texture) {
        Ok(fb) => fb,
        Err(e) => {
            tracing::warn!(error = %e, "ext-screencopy: bind offscreen failed");
            for p in shm_frames {
                p.frame.fail(CaptureFailureReason::Unknown);
            }
            return;
        }
    };
    let mut damage_tracker =
        OutputDamageTracker::new(output_size, scale, output.current_transform());
    let render_result = crate::screencopy::render_into::<R>(
        &mut damage_tracker,
        renderer,
        &mut framebuffer,
        elements,
        clear,
    );
    drop(framebuffer);
    if let Err(e) = render_result {
        tracing::warn!(error = %e, "ext-screencopy: render_output failed");
        for p in shm_frames {
            p.frame.fail(CaptureFailureReason::Unknown);
        }
        return;
    }

    // Re-bind for the copy (render dropped the framebuffer).
    let framebuffer = match renderer.bind(&mut texture) {
        Ok(fb) => fb,
        Err(e) => {
            tracing::warn!(error = %e, "ext-screencopy: rebind offscreen failed");
            for p in shm_frames {
                p.frame.fail(CaptureFailureReason::Unknown);
            }
            return;
        }
    };

    let stride = region.size.w * 4;
    for p in shm_frames {
        let buffer = p.frame.buffer();
        match crate::screencopy::copy_region(
            renderer,
            &framebuffer,
            region,
            Fourcc::Argb8888,
            stride,
            &buffer,
        ) {
            Ok(()) => finish_frame(p.frame),
            Err(e) => {
                tracing::warn!(error = %e, "ext-screencopy: copy_region failed");
                p.frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }
    drop(framebuffer);
}

/// Signal a successful capture. Both render paths land rows top-to-bottom and
/// are already in the output's logical orientation (we render through the
/// output transform), so the buffer is `Normal` from the client's view — same
/// no-YInvert reasoning as the wlr path. `None` damage means "fully damaged".
fn finish_frame(frame: Frame) {
    let presented = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    frame.success(Transform::Normal, None, presented);
}
