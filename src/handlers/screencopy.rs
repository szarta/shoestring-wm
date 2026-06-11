//! wlr-screencopy dispatch. See [`crate::screencopy`] for state and the
//! capture helper called from the render path.

use std::sync::{Arc, Mutex};

use smithay::{
    backend::allocator::Fourcc,
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            backend::ClientId,
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
            Client, DataInit, DisplayHandle, New, Resource,
        },
    },
    utils::{Buffer as BufferCoord, Logical, Rectangle, Size},
    wayland::{shm::with_buffer_contents, Dispatch2, GlobalDispatch2},
};

use crate::{
    screencopy::{FrameInner, ScreencopyFrameData, ScreencopyManagerData},
    state::ShoestringWm,
};

// ---------------- Manager ----------------

impl GlobalDispatch2<ZwlrScreencopyManagerV1, ShoestringWm> for ScreencopyManagerData {
    fn bind(
        &self,
        _state: &mut ShoestringWm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        data_init.init(resource, ScreencopyManagerData);
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, ShoestringWm> for ScreencopyManagerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        match request {
            Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                create_frame(state, frame, &output, overlay_cursor != 0, None, data_init);
            }
            Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let region: Rectangle<i32, Logical> =
                    Rectangle::new((x, y).into(), (width, height).into());
                create_frame(
                    state,
                    frame,
                    &output,
                    overlay_cursor != 0,
                    Some(region),
                    data_init,
                );
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

fn create_frame(
    state: &mut ShoestringWm,
    new_frame: New<ZwlrScreencopyFrameV1>,
    wl_output: &WlOutput,
    overlay_cursor: bool,
    region_logical: Option<Rectangle<i32, Logical>>,
    data_init: &mut DataInit<'_, ShoestringWm>,
) {
    // Resolve the wl_output to one of ours. If unknown, init the frame and
    // immediately fail it so the client cleans up.
    let output = Output::from_resource(wl_output);

    let (region_px, ok) = match output.as_ref().and_then(|o| {
        let mode = o.current_mode()?;
        Some((o, mode))
    }) {
        Some((o, mode)) => {
            let scale = o.current_scale().fractional_scale();
            let full: Rectangle<i32, BufferCoord> =
                Rectangle::from_size(Size::from((mode.size.w, mode.size.h)));
            let rect = if let Some(r) = region_logical {
                let x = (r.loc.x as f64 * scale).round() as i32;
                let y = (r.loc.y as f64 * scale).round() as i32;
                let w = (r.size.w as f64 * scale).round() as i32;
                let h = (r.size.h as f64 * scale).round() as i32;
                let req: Rectangle<i32, BufferCoord> = Rectangle::new((x, y).into(), (w, h).into());
                req.intersection(full).unwrap_or_default()
            } else {
                full
            };
            (rect, true)
        }
        None => (Rectangle::default(), false),
    };

    let format = Fourcc::Argb8888;
    let stride = region_px.size.w * 4;

    let inner = Arc::new(Mutex::new(FrameInner {
        // If output was unknown, stash a placeholder; the frame will be
        // failed below before anyone reads it.
        output: output.clone().unwrap_or_else(|| {
            // Build a throwaway Output just to satisfy the field; never used
            // because we send failed() immediately.
            Output::new(
                "<invalid>".to_string(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: String::new(),
                    model: String::new(),
                    serial_number: String::new(),
                },
            )
        }),
        region: region_px,
        overlay_cursor,
        format,
        stride,
        buffer: None,
        used: false,
    }));
    let frame = data_init.init(
        new_frame,
        ScreencopyFrameData {
            inner: inner.clone(),
        },
    );

    // Screen-capture gate. The global is withdrawn when the gate is off, but a
    // client that bound the manager beforehand keeps its proxy and could still
    // reach here — so refuse explicitly. This is the authoritative check; the
    // global's absence is just so capture can't be *discovered* when off.
    if !state.screen_capture_enabled {
        frame.failed();
        return;
    }

    if !ok || region_px.size.w <= 0 || region_px.size.h <= 0 {
        frame.failed();
        return;
    }

    // Announce supported buffer parameters. SHM only — no dmabuf path.
    frame.buffer(
        wl_shm::Format::Argb8888,
        region_px.size.w as u32,
        region_px.size.h as u32,
        stride as u32,
    );
    if frame.version() >= 3 {
        frame.buffer_done();
    }
    let _ = state; // suppress unused warning until we add fast-path kicks
}

// ---------------- Frame ----------------

impl Dispatch2<ZwlrScreencopyFrameV1, ShoestringWm> for ScreencopyFrameData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_screencopy_frame_v1::Request;
        match request {
            Request::Copy { buffer } | Request::CopyWithDamage { buffer } => {
                // Re-check the gate: the frame may have been created while
                // capture was enabled and the gate flipped off since.
                if !state.screen_capture_enabled {
                    resource.failed();
                    return;
                }
                let (region, stride, output) = {
                    let mut inner = self.inner.lock().unwrap();
                    if inner.used {
                        resource.post_error(
                            zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                            "frame already used",
                        );
                        return;
                    }
                    let valid = validate_shm(&buffer, inner.region, inner.stride);
                    if !valid {
                        resource.post_error(
                            zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                            "buffer attributes don't match",
                        );
                        return;
                    }
                    inner.buffer = Some(buffer);
                    inner.used = true;
                    (inner.region, inner.stride, inner.output.clone())
                };
                let _ = (region, stride); // captured into closure-equivalent state below

                state.screencopy.pending.push(resource.clone());
                state.kick_render_for_screencopy(&output);
                // Live "your screen is being read right now" signal (throttled).
                state.note_screen_capture(&output.name());
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: ClientId,
        resource: &ZwlrScreencopyFrameV1,
    ) {
        state.screencopy.pending.retain(|f| f != resource);
    }
}

fn validate_shm(buffer: &WlBuffer, region: Rectangle<i32, BufferCoord>, stride: i32) -> bool {
    with_buffer_contents(buffer, |_ptr, _len, data| {
        matches!(data.format, wl_shm::Format::Argb8888)
            && data.width == region.size.w
            && data.height == region.size.h
            && data.stride == stride
    })
    .unwrap_or(false)
}
