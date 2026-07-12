//! Streaming damage-capture of a **single window** — the native damage-push
//! primitive behind remote-desktop *graft mode* (task 180 / C2, pull one remote
//! window into a local workspace).
//!
//! Where [`crate::capture_stream`] streams a whole `Output` by reading back the
//! damaged regions of the already-rendered scanout, graft renders **just one
//! window's surface tree to its own offscreen buffer**. That makes the stream
//! correct even when the window is occluded, minimized, or on a non-active
//! workspace — none of which a scanout crop could handle — and yields
//! window-local tile coordinates the client can blit straight into a
//! window-sized buffer.
//!
//! A subscriber sends [`shoestring_ipc::Request::CaptureWindow`]; the IPC layer
//! (`src/ipc.rs`) gates it on the screen-capture flag, replies `Ok`, and marks
//! the connection a [`WindowCaptureSub`]. From then on the connection carries a
//! binary stream of `shoestring_remote::ServerMessage` frames — `Ready`, a
//! `Meta` (app_id/title), then `Frame`/`Resize`/`Bye` — exactly like serve mode
//! but scoped to one window.
//!
//! Each render tick, the backend calls [`push_window_capture`]. A per-subscriber
//! [`OutputDamageTracker`] over a reused offscreen texture turns the full-window
//! render into **incremental** damage tiles, so a window that isn't committing
//! produces no frames (the per-sub `dirty` flag, set from the commit hook in
//! `src/handlers/compositor.rs`, gates the work before any GPU cost).

use std::fmt::Display;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement,
            element::AsRenderElements, element::RenderElement, gles::GlesTexture, Bind, ExportMem,
            ImportAll, ImportMem, Offscreen, Renderer, RendererSuper, Texture,
        },
    },
    desktop::{space::SpaceRenderElements, Window},
    utils::{Physical, Point, Rectangle, Scale, Size, Transform},
};

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use shoestring_remote::{write_framed, PixelFormat, ServerMessage, Tile};

use crate::capture_stream::{clamp_to_buffer, readback, tile_for_region};
use crate::drawing::OutputRenderElements;
use crate::ipc::{write_response, Client, ClientId};
use crate::state::ShoestringWm;

/// A pending one-shot `screenshot_window` request, parked on
/// [`ShoestringWm::pending_window_screenshots`] by the IPC handler until the next
/// render tick can render the window and reply. Holds the connection open (its
/// `Rc<RefCell<Client>>`) so the deferred [`shoestring_ipc::Response::Screenshot`]
/// still has somewhere to go.
pub(crate) struct PendingWindowShot {
    /// IPC connection id, for `drop_ipc_client` after the reply is written.
    pub id: ClientId,
    /// Foreign-toplevel id of the window to capture.
    pub ft_id: String,
    /// The connection to reply on (kept alive past `handle_readable`).
    pub client: Rc<RefCell<Client>>,
}

/// A transparent clear so the window's own alpha (rounded/CSD corners, shadows
/// the client draws into its buffer) survives to the viewer, which composites
/// tiles over a memset buffer.
const CLEAR_TRANSPARENT: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// Per-connection state for a window-capture (graft) subscriber, held on the IPC
/// [`Client`]. Owns the reused offscreen texture + damage tracker so successive
/// pushes send only the window's changed tiles.
pub struct WindowCaptureSub {
    /// Foreign-toplevel identifier of the window being streamed (as in
    /// [`shoestring_ipc::WindowSummary::id`]).
    pub ft_id: String,
    /// Frames sent so far; increments per `Frame` (logging / loss detection).
    pub seq: u32,
    /// Set once the initial `Ready` + `Meta` + full frame has gone out.
    pub sent_ready: bool,
    /// Window physical pixel size at the last push; a change triggers a `Resize`
    /// + a fresh texture/tracker (full repaint).
    pub last_size: (u32, u32),
    /// Last (app_id, title) sent as `Meta`; a change re-sends it so the local
    /// toplevel stays labelled.
    pub last_meta: (String, String),
    /// Set by the compositor commit hook when this window commits; gates the
    /// per-tick render so an idle window costs nothing. Set on first push too.
    pub dirty: bool,
    /// Reused offscreen render target, sized to the window's physical pixels.
    /// `None` until the first push; recreated on resize.
    texture: Option<GlesTexture>,
    /// Damage tracker paired with `texture`; recreated alongside it.
    tracker: Option<OutputDamageTracker>,
    /// Buffer age for the tracker: `0` right after a (re)create (full repaint),
    /// `1` on every subsequent render of the same texture.
    age: usize,
}

impl WindowCaptureSub {
    pub fn new(ft_id: String) -> Self {
        WindowCaptureSub {
            ft_id,
            seq: 0,
            sent_ready: false,
            last_size: (0, 0),
            last_meta: (String::new(), String::new()),
            // Force the first push to render + send a full frame.
            dirty: true,
            texture: None,
            tracker: None,
            age: 0,
        }
    }
}

/// The scale to render the window at: its current output's fractional scale, or
/// the first output's, or 1.0 — mirroring how serve mode takes the output scale.
fn window_scale(state: &ShoestringWm, window: &Window) -> f64 {
    let on = crate::ipc::window_output(state, window);
    state
        .space
        .outputs()
        .find(|o| on.as_deref() == Some(&o.name()))
        .or_else(|| state.space.outputs().next())
        .map(|o| o.current_scale().fractional_scale())
        .unwrap_or(1.0)
}

/// Build the render-element list for a single window at physical origin `(0,0)`,
/// so the damage tracker reports window-local pixel coordinates. Only the
/// window's own surface tree — no occluders, no layers, no wallpaper.
fn window_elements<R>(
    renderer: &mut R,
    window: &Window,
    scale: f64,
) -> Vec<OutputRenderElements<R, WaylandSurfaceRenderElement<R>>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + Texture + 'static,
{
    AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
        window,
        renderer,
        Point::<i32, Physical>::from((0, 0)),
        Scale::from(scale),
        1.0,
    )
    .into_iter()
    .map(|e| OutputRenderElements::Space(SpaceRenderElements::Surface(e)))
    .collect()
}

/// Render one window offscreen and push its damaged tiles to every window-capture
/// subscriber. Call once per render tick, next to the screencopy passes and
/// *before* the main output framebuffer is bound (this binds its own target).
///
/// - No-op when the capture gate is off or there are no window-capture
///   subscribers.
/// - A sub whose window has vanished gets a `Bye` and is dropped.
/// - An idle window (no commit since last push ⇒ `dirty == false`) is skipped
///   before any GPU work, so it produces zero frames.
pub fn push_window_capture<R>(state: &mut ShoestringWm, renderer: &mut R)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Bind<GlesTexture> + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    if !state.screen_capture_enabled {
        return;
    }
    let Some(server) = state.ipc.as_ref() else {
        return;
    };
    let subs = server.window_capture_subscribers();
    if subs.is_empty() {
        return;
    }

    let mut to_drop: Vec<ClientId> = Vec::new();
    let mut any_pushed = false;

    for (id, client) in subs {
        // Snapshot what we need before resolving the window / rendering.
        let Some(ft_id) = client
            .borrow()
            .window_capture_sub
            .as_ref()
            .map(|s| s.ft_id.clone())
        else {
            continue;
        };

        // Resolve the window (foreign_toplevels holds every window regardless of
        // mapping). Gone ⇒ tell the client and drop the sub.
        let Some(window) = crate::ipc::window_by_ft_id(state, &ft_id) else {
            let _ = write_framed(client.borrow_mut().stream_mut(), &ServerMessage::Bye);
            to_drop.push(id);
            continue;
        };

        let scale = window_scale(state, &window);
        let logical = window.geometry().size;
        let phys: Size<i32, Physical> = logical.to_f64().to_physical(scale).to_i32_round();
        let (pw, ph) = (phys.w.max(1), phys.h.max(1));
        let (app_id, title) = crate::ipc::effective_title_app_id(state, &window);

        // Skip idle windows: nothing committed since the last push and we've
        // already sent the initial frame.
        {
            let c = client.borrow();
            if let Some(s) = c.window_capture_sub.as_ref() {
                if s.sent_ready && !s.dirty && s.last_size == (pw as u32, ph as u32) {
                    continue;
                }
            }
        }

        if render_and_push(&window, renderer, &client, pw, ph, scale, &app_id, &title) {
            any_pushed = true;
        } else {
            to_drop.push(id);
        }
    }

    for id in to_drop {
        state.drop_ipc_client(id);
    }
    if any_pushed {
        // Light the "your screen is being read" indicator, throttled.
        state.note_screen_capture("graft");
    }
}

/// Render `window` into the subscriber's offscreen texture and write its
/// lifecycle prelude + one damage `Frame`. Returns `false` if the client could
/// not be written (caller drops it) or a fatal render error occurred.
#[allow(clippy::too_many_arguments)]
fn render_and_push<R>(
    window: &Window,
    renderer: &mut R,
    client: &std::rc::Rc<std::cell::RefCell<Client>>,
    pw: i32,
    ph: i32,
    scale: f64,
    app_id: &str,
    title: &str,
) -> bool
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Bind<GlesTexture> + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    let elements = window_elements(renderer, window, scale);

    let mut c = client.borrow_mut();
    let Some(sub) = c.window_capture_sub.as_mut() else {
        return true;
    };

    let size_changed = sub.sent_ready && sub.last_size != (pw as u32, ph as u32);
    let need_new_target = sub.texture.is_none() || size_changed;
    if need_new_target {
        match renderer.create_buffer(Fourcc::Argb8888, (pw, ph).into()) {
            Ok(tex) => {
                sub.texture = Some(tex);
                sub.tracker = Some(OutputDamageTracker::new(
                    Size::<i32, Physical>::from((pw, ph)),
                    scale,
                    Transform::Normal,
                ));
                sub.age = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "window_capture: offscreen create_buffer failed");
                return true; // transient; try again next tick
            }
        }
    }

    // Render the window into the offscreen texture, collecting damage rects.
    let texture = sub.texture.as_mut().expect("texture set above");
    let tracker = sub.tracker.as_mut().expect("tracker set above");
    let age = sub.age;

    let damage: Vec<smithay::utils::Rectangle<i32, Physical>> = {
        let mut fb = match renderer.bind(texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "window_capture: bind offscreen failed");
                return true;
            }
        };
        match tracker.render_output(renderer, &mut fb, age, &elements, CLEAR_TRANSPARENT) {
            Ok(result) => result.damage.map(|d| d.to_vec()).unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = ?e, "window_capture: render_output failed");
                return true;
            }
        }
    };
    sub.age = 1;

    // On the first frame (or after a resize) the tracker reports full damage;
    // otherwise only the changed regions. Nothing changed ⇒ no tiles.
    let mut messages: Vec<ServerMessage> = Vec::new();
    if !sub.sent_ready {
        messages.push(ServerMessage::Ready {
            width: pw as u32,
            height: ph as u32,
            scale,
            format: PixelFormat::Argb8888,
        });
    } else if size_changed {
        messages.push(ServerMessage::Resize {
            width: pw as u32,
            height: ph as u32,
            scale,
        });
    }
    if !sub.sent_ready || sub.last_meta != (app_id.to_string(), title.to_string()) {
        messages.push(ServerMessage::Meta {
            app_id: app_id.to_string(),
            title: title.to_string(),
        });
    }

    // Read back the damaged regions from the just-rendered texture.
    let tiles: Vec<Tile> = {
        let fb = match renderer.bind(texture) {
            Ok(fb) => fb,
            Err(e) => {
                tracing::warn!(error = %e, "window_capture: rebind for readback failed");
                return true;
            }
        };
        damage
            .iter()
            .filter_map(|r| clamp_to_buffer(*r, pw, ph))
            .filter_map(|region| tile_for_region(renderer, &fb, region))
            .collect()
    };

    // An established sub with no damage this frame: dirty is cleared, send nothing.
    if sub.sent_ready && messages.is_empty() && tiles.is_empty() {
        sub.dirty = false;
        return true;
    }

    let next_seq = sub.seq.wrapping_add(1);
    messages.push(ServerMessage::Frame {
        seq: next_seq,
        tiles,
    });

    let wrote = messages
        .iter()
        .try_for_each(|m| write_framed(c.stream_mut(), m));
    if wrote.is_err() {
        return false;
    }

    if let Some(sub) = c.window_capture_sub.as_mut() {
        sub.sent_ready = true;
        sub.last_size = (pw as u32, ph as u32);
        sub.last_meta = (app_id.to_string(), title.to_string());
        sub.seq = next_seq;
        sub.dirty = false;
    }
    true
}

impl ShoestringWm {
    /// Tear down every window-capture subscriber: send a final `Bye` and drop
    /// the connection. Called alongside the output capture teardown when the
    /// screen-capture gate is revoked (capture must never outlive consent).
    pub fn teardown_window_capture_subscribers(&mut self) {
        let Some(server) = self.ipc.as_ref() else {
            return;
        };
        for (id, client) in server.window_capture_subscribers() {
            let _ = write_framed(client.borrow_mut().stream_mut(), &ServerMessage::Bye);
            self.drop_ipc_client(id);
        }
    }

    /// Mark every window-capture subscriber whose target window is `window` as
    /// dirty, so the next render tick streams its new content. Called from the
    /// commit hook — including for off-workspace windows the space wouldn't see.
    pub fn mark_window_capture_dirty(&mut self, window: &Window) {
        let Some(id) = self.foreign_toplevels.get(window).map(|h| h.identifier()) else {
            return;
        };
        let Some(server) = self.ipc.as_ref() else {
            return;
        };
        for (_, client) in server.window_capture_subscribers() {
            if let Some(sub) = client.borrow_mut().window_capture_sub.as_mut() {
                if sub.ft_id == id {
                    sub.dirty = true;
                }
            }
        }
    }

    /// `true` if any connection is a window-capture subscriber — lets the commit
    /// hook skip its extra lookup when graft isn't in use.
    pub fn has_window_capture_subscribers(&self) -> bool {
        self.ipc
            .as_ref()
            .is_some_and(|s| !s.window_capture_subscribers().is_empty())
    }
}

/// Render `window` to a fresh offscreen buffer once and read the whole thing back
/// as tightly-packed little-endian `Argb8888` (BGRA-in-memory) bytes. Returns
/// `(width, height, bytes)` in physical pixels. The single-frame sibling of
/// [`push_window_capture`]: no damage tracking or subscriber state, just one full
/// render for the one-shot `screenshot_window` path.
fn render_window_argb<R>(
    state: &ShoestringWm,
    renderer: &mut R,
    window: &Window,
) -> Result<(u32, u32, Vec<u8>), String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Bind<GlesTexture> + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    let scale = window_scale(state, window);
    let logical = window.geometry().size;
    let phys: Size<i32, Physical> = logical.to_f64().to_physical(scale).to_i32_round();
    let (pw, ph) = (phys.w.max(1), phys.h.max(1));

    let elements = window_elements(renderer, window, scale);
    let mut texture = renderer
        .create_buffer(Fourcc::Argb8888, (pw, ph).into())
        .map_err(|e| format!("create_buffer: {e}"))?;

    // Render the full window (fresh tracker ⇒ full damage) into the target.
    let mut tracker = OutputDamageTracker::new(
        Size::<i32, Physical>::from((pw, ph)),
        scale,
        Transform::Normal,
    );
    {
        let mut fb = renderer
            .bind(&mut texture)
            .map_err(|e| format!("bind offscreen: {e}"))?;
        tracker
            .render_output(renderer, &mut fb, 0, &elements, CLEAR_TRANSPARENT)
            .map_err(|e| format!("render_output: {e}"))?;
    }

    // Read the whole buffer back (rebind: the previous framebuffer borrow ended
    // with the block above).
    let fb = renderer
        .bind(&mut texture)
        .map_err(|e| format!("rebind for readback: {e}"))?;
    let region = Rectangle::new(Point::from((0, 0)), Size::from((pw, ph)));
    let bytes = readback(renderer, &fb, region)?;
    Ok((pw as u32, ph as u32, bytes))
}

/// Encode a tightly-packed `Argb8888` (little-endian BGRA-in-memory) buffer as an
/// RGBA PNG. Swizzles B/R and keeps alpha, matching `shoestring-screenshot`'s
/// output-capture path so window and full-screen captures encode identically.
fn argb8888_to_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let (w, h) = (width as usize, height as usize);
    let expected = w * h * 4;
    if bytes.len() < expected {
        return Err(format!("short buffer: {} < {expected}", bytes.len()));
    }
    let mut rgba = vec![0u8; expected];
    for i in 0..(w * h) {
        let s = i * 4;
        rgba[s] = bytes[s + 2]; // R
        rgba[s + 1] = bytes[s + 1]; // G
        rgba[s + 2] = bytes[s]; // B
        rgba[s + 3] = bytes[s + 3]; // A
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| format!("png write_header: {e}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| format!("png write_image_data: {e}"))?;
    }
    Ok(out)
}

/// Service every parked one-shot `screenshot_window` request: render the window,
/// encode a PNG, write it to disk, and reply with its path (or an `error`). Call
/// once per render tick from a backend that supports offscreen window rendering
/// (winit, headless), next to [`push_window_capture`]. A no-op when the queue is
/// empty, so it costs nothing when unused.
pub fn process_pending_window_screenshots<R>(state: &mut ShoestringWm, renderer: &mut R)
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Bind<GlesTexture> + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    if state.pending_window_screenshots.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut state.pending_window_screenshots);
    let mut captured_any = false;
    for shot in pending {
        let response = match capture_window_to_file(state, renderer, &shot.ft_id) {
            Ok(path) => {
                captured_any = true;
                shoestring_ipc::Response::Screenshot {
                    path: path.to_string_lossy().into_owned(),
                }
            }
            Err(message) => shoestring_ipc::Response::Error { message },
        };
        let _ = write_response(&shot.client, &response);
        state.drop_ipc_client(shot.id);
    }
    if captured_any {
        // Same "your screen is being read" signal as the other capture paths.
        state.note_screen_capture("window-screenshot");
    }
}

/// Resolve, render, encode, and write one window screenshot; returns the file
/// path on success. Split out so [`process_pending_window_screenshots`] stays a
/// thin drain loop.
fn capture_window_to_file<R>(
    state: &ShoestringWm,
    renderer: &mut R,
    ft_id: &str,
) -> Result<PathBuf, String>
where
    R: Renderer + ImportAll + ImportMem + ExportMem + Bind<GlesTexture> + Offscreen<GlesTexture>,
    R::TextureId: Clone + Texture + 'static,
    <R as RendererSuper>::Error: Display,
    OutputRenderElements<R, WaylandSurfaceRenderElement<R>>: RenderElement<R>,
{
    let window = crate::ipc::window_by_ft_id(state, ft_id)
        .ok_or_else(|| format!("screenshot_window: no window with id {ft_id}"))?;
    let (w, h, bytes) = render_window_argb(state, renderer, &window)?;
    let png = argb8888_to_png(&bytes, w, h)?;
    let path = ShoestringWm::auto_screenshot_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(&path, &png).map_err(|e| format!("write {}: {e}", path.display()))?;
    tracing::info!(?path, ft_id, w, h, "wrote window screenshot");
    Ok(path)
}
