//! Streaming damage-capture subscription — the native damage-push primitive
//! behind remote-desktop *serve mode* (task A2).
//!
//! A subscriber sends [`shoestring_ipc::Request::CaptureStream`]; the IPC layer
//! (`src/ipc.rs`) gates it on the screen-capture flag, replies `Ok`, and marks
//! the connection a [`CaptureSub`]. From then on the connection carries a
//! binary stream of `shoestring_remote::ServerMessage` frames instead of
//! newline-JSON.
//!
//! Each render, the backend calls [`push_capture`] with the just-rendered
//! framebuffer and the frame's damage. We read back **only the damaged regions**
//! (or the whole output for a fresh subscriber), encode them as zlib tiles via
//! [`shoestring_remote::encode_tile`], and push one `Frame`. An idle desktop has
//! no damage, so it produces no frames — the whole point of "both ends are
//! shoestring-wm".
//!
//! Contrast `wlr-screencopy` (`src/screencopy.rs`), which re-renders a full
//! offscreen copy per one-shot request: here we read the live scanout/offscreen
//! framebuffer and send only what changed.

use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{ExportMem, Renderer},
    },
    output::Output,
    utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size},
};

use shoestring_remote::{
    encode_tile, write_framed, PixelFormat, Rect as RemoteRect, ServerMessage, Tile, TileEncoding,
};

use crate::ipc::{Client, ClientId};
use crate::state::ShoestringWm;

/// Per-connection state for a capture-stream subscriber, held on the IPC
/// `Client`. Tracks what the subscriber has already been told so each render
/// sends the minimum: a `Ready` + full frame on first push (or after a resize),
/// then incremental damage tiles.
pub struct CaptureSub {
    /// Which output this subscriber is watching (matched by `Output::name`).
    pub output_name: String,
    /// Frames sent so far; increments per `Frame` (logging / loss detection).
    pub seq: u32,
    /// Set once the initial `Ready` + full-output frame has gone out.
    pub sent_ready: bool,
    /// Output pixel size at the last push; a change triggers a `Resize` + full
    /// frame (the subscriber must reallocate its surface).
    pub last_size: (u32, u32),
}

impl CaptureSub {
    pub fn new(output_name: String) -> Self {
        CaptureSub {
            output_name,
            seq: 0,
            sent_ready: false,
            last_size: (0, 0),
        }
    }
}

/// Clamp a physical damage rectangle to the framebuffer and convert it to
/// buffer coordinates. Returns `None` if it lies fully outside or is empty
/// after clamping. (For the un-transformed served output, physical and buffer
/// pixels coincide.)
pub(crate) fn clamp_to_buffer(
    rect: Rectangle<i32, Physical>,
    w: i32,
    h: i32,
) -> Option<Rectangle<i32, BufferCoord>> {
    let x0 = rect.loc.x.max(0);
    let y0 = rect.loc.y.max(0);
    let x1 = (rect.loc.x + rect.size.w).min(w);
    let y1 = (rect.loc.y + rect.size.h).min(h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rectangle::new(
        Point::from((x0, y0)),
        Size::from((x1 - x0, y1 - y0)),
    ))
}

fn to_remote_rect(r: Rectangle<i32, BufferCoord>) -> RemoteRect {
    RemoteRect::new(
        r.loc.x as u16,
        r.loc.y as u16,
        r.size.w as u16,
        r.size.h as u16,
    )
}

/// Read back one region of the bound framebuffer into a tightly-packed,
/// row-major `Argb8888` byte buffer.
pub(crate) fn readback<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    region: Rectangle<i32, BufferCoord>,
) -> Result<Vec<u8>, String>
where
    R: Renderer + ExportMem,
    <R as smithay::backend::renderer::RendererSuper>::Error: Display,
{
    let expected = region.size.w as usize * region.size.h as usize * 4;
    let mapping = renderer
        .copy_framebuffer(framebuffer, region, Fourcc::Argb8888)
        .map_err(|e| format!("copy_framebuffer: {e}"))?;
    let src = renderer
        .map_texture(&mapping)
        .map_err(|e| format!("map_texture: {e}"))?;
    if src.len() < expected {
        return Err(format!("short readback: {} < {expected}", src.len()));
    }
    Ok(src[..expected].to_vec())
}

/// Encode a tile for one region by reading it back from the framebuffer.
pub(crate) fn tile_for_region<R>(
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    region: Rectangle<i32, BufferCoord>,
) -> Option<Tile>
where
    R: Renderer + ExportMem,
    <R as smithay::backend::renderer::RendererSuper>::Error: Display,
{
    match readback(renderer, framebuffer, region) {
        Ok(bytes) => Some(encode_tile(
            to_remote_rect(region),
            bytes,
            TileEncoding::Zlib,
        )),
        Err(e) => {
            tracing::warn!(error = %e, ?region, "capture: region readback failed");
            None
        }
    }
}

/// Push a damage frame to every capture subscriber of `output`, reading back
/// the changed regions from the just-rendered `framebuffer`. Call once per
/// render, after `render_output`, while the framebuffer is still bound.
///
/// - No-op when the capture gate is off or there are no subscribers for this
///   output (so an unused capability costs nothing).
/// - A fresh subscriber (or one after a resize) gets `Ready`/`Resize` plus a
///   full-output frame; established ones get only the damaged tiles, and
///   nothing at all when `damage` is empty.
/// - A subscriber we can't write to is dropped (drop-on-pressure, matching the
///   event/metrics streams), since a half-written binary frame can't be
///   recovered.
pub fn push_capture<R>(
    state: &mut ShoestringWm,
    output: &Output,
    renderer: &mut R,
    framebuffer: &R::Framebuffer<'_>,
    damage: Option<&[Rectangle<i32, Physical>]>,
) where
    R: Renderer + ExportMem,
    <R as smithay::backend::renderer::RendererSuper>::Error: Display,
{
    if !state.screen_capture_enabled {
        return;
    }
    let Some(server) = state.ipc.as_ref() else {
        return;
    };
    let subs = server.capture_subscribers(&output.name());
    if subs.is_empty() {
        return;
    }
    let Some(mode) = output.current_mode() else {
        return;
    };
    let (w, h) = (mode.size.w, mode.size.h);
    let scale = output.current_scale().fractional_scale();

    let mut to_drop: Vec<ClientId> = Vec::new();
    let mut any_pushed = false;

    for (id, client) in subs {
        let (sent_ready, last_size, seq) = {
            let c = client.borrow();
            match c.capture_sub.as_ref() {
                Some(s) => (s.sent_ready, s.last_size, s.seq),
                None => continue,
            }
        };
        let size_changed = sent_ready && last_size != (w as u32, h as u32);
        let need_full = !sent_ready || size_changed;

        // Decide which regions to read back.
        let regions: Vec<Rectangle<i32, BufferCoord>> = if need_full {
            vec![Rectangle::new(Point::from((0, 0)), Size::from((w, h)))]
        } else {
            match damage {
                Some(rects) => rects
                    .iter()
                    .filter_map(|r| clamp_to_buffer(*r, w, h))
                    .collect(),
                None => Vec::new(),
            }
        };
        // Established subscriber with no damage this frame: send nothing.
        if !need_full && regions.is_empty() {
            continue;
        }

        // Build the message sequence: lifecycle prelude, then the frame.
        let mut messages: Vec<ServerMessage> = Vec::new();
        if !sent_ready {
            messages.push(ServerMessage::Ready {
                width: w as u32,
                height: h as u32,
                scale,
                format: PixelFormat::Argb8888,
            });
        } else if size_changed {
            messages.push(ServerMessage::Resize {
                width: w as u32,
                height: h as u32,
                scale,
            });
        }
        let tiles: Vec<Tile> = regions
            .into_iter()
            .filter_map(|r| tile_for_region(renderer, framebuffer, r))
            .collect();
        let next_seq = seq.wrapping_add(1);
        messages.push(ServerMessage::Frame {
            seq: next_seq,
            tiles,
        });

        // Write the frames; a single failure drops the subscriber.
        let wrote = {
            let mut c = client.borrow_mut();
            messages
                .iter()
                .try_for_each(|m| write_framed(c.stream_mut(), m))
        };
        if wrote.is_err() {
            to_drop.push(id);
            continue;
        }
        if let Some(s) = client.borrow_mut().capture_sub.as_mut() {
            s.sent_ready = true;
            s.last_size = (w as u32, h as u32);
            s.seq = next_seq;
        }
        any_pushed = true;
    }

    for id in to_drop {
        state.drop_ipc_client(id);
    }
    if any_pushed {
        // Light the "your screen is being read" indicator, throttled.
        let name = output.name();
        state.note_screen_capture(&name);
    }
}

impl ShoestringWm {
    /// Tear down every capture-stream subscriber: send a final `Bye` and drop
    /// the connection. Called when the screen-capture gate is turned off so a
    /// revoked stream stops immediately (privacy: capture must never outlive
    /// consent).
    pub fn teardown_capture_subscribers(&mut self) {
        let Some(server) = self.ipc.as_ref() else {
            return;
        };
        let subs: Vec<(ClientId, Rc<RefCell<Client>>)> = server.all_capture_subscribers();
        for (id, client) in subs {
            let _ = write_framed(client.borrow_mut().stream_mut(), &ServerMessage::Bye);
            self.drop_ipc_client(id);
        }
    }
}
