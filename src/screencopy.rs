//! wlr-screencopy state and the capture helper invoked from the render path.
//!
//! Smithay ships only the modern `ext-image-copy-capture-v1` delegate, so we
//! wire `zwlr_screencopy_v1` manually. Clients (grim, grimshot, wf-recorder,
//! OBS, xdg-desktop-portal-wlr, our own shoestring-screenshot) bind the
//! manager and request a capture for an output. We offer two buffer kinds:
//!
//! - **shm** (always): render the scene into an offscreen GLES texture and
//!   read the requested region back into the client's wl_shm buffer.
//! - **dmabuf** (udev/KMS, full-output only): bind the client's dmabuf as the
//!   render target and paint the scene straight into it — no readback. This
//!   is the path xdpw's screencast probe requires; it refuses shm-only
//!   compositors. Advertised via the screencopy `linux_dmabuf` event, gated
//!   on the dmabuf global existing (see [`crate::backend::udev`]).
//!
//! Why offscreen for shm rather than capturing the scanout buffer: it works
//! uniformly across winit (nested dev) and udev (TTY) without poking at
//! backend framebuffer internals, and shm screencopy is rare enough that the
//! extra render pass is fine.

use std::sync::{Arc, Mutex};

use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Fourcc},
        renderer::{
            damage::OutputDamageTracker, element::RenderElement, gles::GlesTexture, Bind,
            ExportMem, ImportAll, ImportMem, Offscreen, Renderer, RendererSuper,
        },
    },
    desktop::Space,
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_frame_v1::{
            self, ZwlrScreencopyFrameV1,
        },
        wayland_server::{backend::GlobalId, protocol::wl_buffer::WlBuffer, Resource},
    },
    utils::{Buffer as BufferCoord, Rectangle},
    wayland::{
        dmabuf::get_dmabuf,
        shm::{with_buffer_contents_mut, BufferAccessError},
    },
};

/// Marker user-data for `ZwlrScreencopyManagerV1` resources. Empty — the
/// manager has no per-instance state, but we need a local type so we can
/// impl `Dispatch2`/`GlobalDispatch2` on it without tripping the orphan rule.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScreencopyManagerData;

/// Container on `ShoestringWm` for the manager global + outstanding requests.
pub struct ScreencopyState {
    /// `Some` while the screen-capture gate is on — the manager global is
    /// advertised iff this is set. Created/withdrawn at runtime by
    /// [`crate::state::ShoestringWm::set_screen_capture`], which is what keeps
    /// the protocol entirely absent (not merely inert) when capture is off.
    pub manager_global: Option<GlobalId>,
    /// Frames whose `copy`/`copy_with_damage` request has come in and which
    /// are now waiting for the next render of their target output. Drained
    /// from the render path.
    pub pending: Vec<ZwlrScreencopyFrameV1>,
}

/// Per-frame user data attached to each `ZwlrScreencopyFrameV1` resource.
/// Wraps a `Mutex<FrameInner>` so dispatch handlers (which only get `&self`
/// on the user data) can mutate the buffer/used fields.
pub struct ScreencopyFrameData {
    pub inner: Arc<Mutex<FrameInner>>,
}

pub struct FrameInner {
    pub output: Output,
    /// Capture region in **buffer pixel** coordinates relative to the output's
    /// rendered framebuffer (top-left origin, ready to feed `copy_framebuffer`).
    pub region: Rectangle<i32, BufferCoord>,
    /// Whether the client asked for cursor overlay. We always composite the
    /// cursor into our framebuffer (no hardware cursor plane), so this flag
    /// is currently ignored — captures always include the cursor.
    #[allow(dead_code)]
    pub overlay_cursor: bool,
    pub format: Fourcc,
    /// Bytes per row in the destination wl_shm buffer.
    pub stride: i32,
    pub buffer: Option<WlBuffer>,
    /// True once `copy`/`copy_with_damage` has been processed. A frame can
    /// only be used once (protocol error otherwise).
    pub used: bool,
    /// Set when the client used `copy_with_damage` rather than `copy`. The
    /// protocol requires the compositor to emit `damage` event(s) before
    /// `ready` in that case; consumers that use the damage variant (e.g.
    /// xdg-desktop-portal-wlr's screencast loop) rely on it and mismanage
    /// their per-frame state if it's missing. We always repaint the full
    /// output, so the damage box is the whole capture region.
    pub with_damage: bool,
}

/// Process every pending screencopy frame whose target matches `output`:
/// render the scene into an offscreen GLES texture sized to the output's
/// physical pixels, then read back the requested region into the client's
/// wl_shm buffer and send `ready`. Frames for other outputs are left in
/// place for their own render pass to handle.
///
/// `cursor_elements` are the same per-frame pointer elements the backend
/// composites onto its real framebuffer; passing them in means screenshots
/// match what's on screen.
pub fn process_pending<R>(
    screencopy: &mut ScreencopyState,
    space: &Space<smithay::desktop::Window>,
    output: &Output,
    renderer: &mut R,
    cursor_elements: &[crate::drawing::PointerRenderElement<R>],
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
    crate::drawing::PointerRenderElement<R>: RenderElement<R>,
    smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>: RenderElement<R>,
{
    // Fast path: nothing to do.
    if screencopy.pending.is_empty() {
        return;
    }

    // Pull out the frames targeting this output. Anything else stays in
    // screencopy.pending for the matching output's render pass.
    let (for_us, others): (Vec<_>, Vec<_>) = std::mem::take(&mut screencopy.pending)
        .into_iter()
        .partition(|f| {
            f.data::<ScreencopyFrameData>()
                .map(|d| &d.inner.lock().unwrap().output == output)
                .unwrap_or(false)
        });
    screencopy.pending = others;

    if for_us.is_empty() {
        return;
    }

    let mode = match output.current_mode() {
        Some(m) => m,
        None => {
            for f in for_us {
                f.failed();
            }
            return;
        }
    };
    let output_size = mode.size;
    let scale = output.current_scale().fractional_scale();

    // Split frames by buffer kind. dmabuf-backed frames render straight into
    // the client buffer; shm-backed frames go through the offscreen-texture
    // readback below. A frame whose buffer vanished (client raced us) is
    // failed here.
    let mut shm_frames: Vec<ZwlrScreencopyFrameV1> = Vec::new();
    let mut dmabuf_frames: Vec<(ZwlrScreencopyFrameV1, Dmabuf)> = Vec::new();
    for frame in for_us {
        let Some(user) = frame.data::<ScreencopyFrameData>() else {
            continue;
        };
        let buffer = user.inner.lock().unwrap().buffer.clone();
        match buffer {
            Some(buf) => match get_dmabuf(&buf) {
                Ok(dmabuf) => dmabuf_frames.push((frame, dmabuf.clone())),
                Err(_) => shm_frames.push(frame),
            },
            None => frame.failed(),
        }
    }

    // dmabuf path: bind each client buffer as the render target and paint the
    // scene directly into it. The bound framebuffer borrows the dmabuf (not the
    // renderer), so render_into can still take `&mut renderer` — mirrors the
    // offscreen path's bind/render split below.
    for (frame, mut dmabuf) in dmabuf_frames {
        let mut framebuffer = match renderer.bind(&mut dmabuf) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "screencopy: bind client dmabuf failed");
                frame.failed();
                continue;
            }
        };
        let mut damage_tracker =
            OutputDamageTracker::new(output_size, scale, output.current_transform());
        let result = render_into::<R>(
            &mut damage_tracker,
            renderer,
            &mut framebuffer,
            output,
            space,
            cursor_elements,
        );
        drop(framebuffer);
        match result {
            Ok(()) => {
                // dmabuf is Y-inverted: we render through a GL FBO over the
                // dmabuf's EGLImage, whose bottom-left origin leaves the buffer
                // rows bottom-to-top (matches wlroots). The shm path is NOT
                // inverted — `copy_framebuffer`'s readback lands rows top-to-
                // bottom — so only the dmabuf path flags YInvert.
                finish_frame(&frame, true);
            }
            Err(e) => {
                tracing::warn!(error = %e, "screencopy: dmabuf render failed");
                frame.failed();
            }
        }
    }

    if shm_frames.is_empty() {
        return;
    }

    // Render the scene into an offscreen texture sized to the output's
    // physical pixels. We only do this once even if multiple frames are
    // pending for the same output.
    let texture: GlesTexture =
        match renderer.create_buffer(Fourcc::Argb8888, (output_size.w, output_size.h).into()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "screencopy: offscreen create_buffer failed");
                for f in shm_frames {
                    f.failed();
                }
                return;
            }
        };

    // Bind the texture as the current framebuffer and render the scene
    // (space + cursor) into it. We use a fresh damage tracker per capture
    // so the whole output is repainted (avoids age/damage mismatches with
    // the real scanout's tracker).
    {
        let mut texture = texture;
        let mut framebuffer = match renderer.bind(&mut texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "screencopy: bind offscreen failed");
                for f in shm_frames {
                    f.failed();
                }
                return;
            }
        };
        // Use the output's actual transform so the offscreen render matches
        // the on-screen orientation byte-for-byte. Otherwise a winit dev
        // backend (which uses Flipped180) ends up with a 180°-rotated PNG.
        let mut damage_tracker =
            OutputDamageTracker::new(output_size, scale, output.current_transform());

        let render_result = render_into::<R>(
            &mut damage_tracker,
            renderer,
            &mut framebuffer,
            output,
            space,
            cursor_elements,
        );
        // Drop framebuffer before we re-borrow the renderer for copy.
        drop(framebuffer);

        if let Err(e) = render_result {
            tracing::warn!(error = %e, "screencopy: render_output failed");
            for f in shm_frames {
                f.failed();
            }
            return;
        }

        // Re-bind so copy_framebuffer has a current target. (render_output
        // already required the bind, but the framebuffer was dropped above.)
        let framebuffer = match renderer.bind(&mut texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "screencopy: rebind offscreen failed");
                for f in shm_frames {
                    f.failed();
                }
                return;
            }
        };

        for frame in shm_frames {
            let Some(user) = frame.data::<ScreencopyFrameData>() else {
                continue;
            };
            let (region, format, stride, buffer) = {
                let inner = user.inner.lock().unwrap();
                (
                    inner.region,
                    inner.format,
                    inner.stride,
                    inner.buffer.clone(),
                )
            };
            let Some(buffer) = buffer else {
                // Race: client destroyed the buffer between copy() and now.
                frame.failed();
                continue;
            };

            match copy_region(renderer, &framebuffer, region, format, stride, &buffer) {
                Ok(()) => {
                    // shm readback (copy_framebuffer) lands rows top-to-bottom;
                    // no YInvert (see the dmabuf path comment above).
                    finish_frame(&frame, false);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "screencopy: copy_region failed");
                    frame.failed();
                }
            }
        }
        drop(framebuffer);
    }
}

/// Complete a successfully captured frame: send `flags` (only when the buffer
/// is Y-inverted), then a `damage` event if the client used `copy_with_damage`,
/// then `ready` — in that order, matching wlroots. The damage box covers the
/// whole capture region because each capture repaints the full output (no
/// incremental damage tracking against prior captures). Reads
/// `with_damage`/`region` from the frame's own user data so both the dmabuf and
/// shm paths share one code path.
///
/// `y_invert` differs per path: the dmabuf path renders into an EGLImage FBO
/// (bottom-left origin → rows bottom-to-top → invert), while the shm path's
/// `copy_framebuffer` readback already lands rows top-to-bottom. Tagging shm as
/// inverted is what flipped `shoestring-screenshot` PNGs and portal frames.
fn finish_frame(frame: &ZwlrScreencopyFrameV1, y_invert: bool) {
    if y_invert {
        frame.flags(zwlr_screencopy_frame_v1::Flags::YInvert);
    }
    if let Some(user) = frame.data::<ScreencopyFrameData>() {
        let (with_damage, region) = {
            let inner = user.inner.lock().unwrap();
            (inner.with_damage, inner.region)
        };
        if with_damage {
            frame.damage(
                0,
                0,
                region.size.w.max(0) as u32,
                region.size.h.max(0) as u32,
            );
        }
    }
    send_ready(frame);
}

/// Send the `ready` event with a wall-clock presentation timestamp split into
/// the protocol's `tv_sec_hi`/`tv_sec_lo`/`tv_nsec` triplet.
fn send_ready(frame: &ZwlrScreencopyFrameV1) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    frame.ready(
        (secs >> 32) as u32,
        (secs & 0xFFFF_FFFF) as u32,
        now.subsec_nanos(),
    );
}

fn render_into<R>(
    damage_tracker: &mut OutputDamageTracker,
    renderer: &mut R,
    framebuffer: &mut R::Framebuffer<'_>,
    output: &Output,
    space: &Space<smithay::desktop::Window>,
    cursor_elements: &[crate::drawing::PointerRenderElement<R>],
) -> Result<(), String>
where
    R: Renderer + ImportAll + ImportMem,
    R::Error: std::fmt::Display,
    <R as RendererSuper>::TextureId: Clone + 'static,
    crate::drawing::PointerRenderElement<R>: RenderElement<R>,
    smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement<R>: RenderElement<R>,
{
    smithay::desktop::space::render_output::<_, crate::drawing::PointerRenderElement<R>, _, _>(
        output,
        renderer,
        framebuffer,
        1.0,
        0,
        [space],
        cursor_elements,
        damage_tracker,
        [0.0, 0.0, 0.0, 1.0],
    )
    .map(|_| ())
    .map_err(|e| format!("{e:?}"))
}

fn copy_region<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    region: Rectangle<i32, BufferCoord>,
    format: Fourcc,
    stride: i32,
    dst: &WlBuffer,
) -> Result<(), String>
where
    R: ExportMem,
    R::Error: std::fmt::Display,
{
    let mapping = renderer
        .copy_framebuffer(framebuffer, region, format)
        .map_err(|e| format!("copy_framebuffer: {e}"))?;
    let src: &[u8] = renderer
        .map_texture(&mapping)
        .map_err(|e| format!("map_texture: {e}"))?;

    let row_bytes = (region.size.w * 4) as usize;
    let height = region.size.h as usize;
    let expected_src_len = row_bytes * height;
    if src.len() < expected_src_len {
        return Err(format!(
            "src too small: have {} need {expected_src_len}",
            src.len()
        ));
    }
    let _ = stride; // dst stride is read from BufferData below.

    let res: Result<(), String> = with_buffer_contents_mut(dst, |ptr, _len, data| {
        if data.width != region.size.w || data.height != region.size.h {
            return Err(format!(
                "dst dimension mismatch: buffer {}x{} region {}x{}",
                data.width, data.height, region.size.w, region.size.h
            ));
        }
        let dst_stride = data.stride as usize;
        for y in 0..height {
            let src_row = &src[y * row_bytes..y * row_bytes + row_bytes];
            // SAFETY: ptr is valid for data.offset+dst_stride*height bytes
            // for the duration of this callback; we write row_bytes (<=
            // dst_stride) into each row.
            unsafe {
                let dst_row = ptr.add(data.offset as usize + y * dst_stride);
                std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row_bytes);
            }
        }
        Ok(())
    })
    .map_err(|e: BufferAccessError| format!("shm access: {e:?}"))?;
    res
}
