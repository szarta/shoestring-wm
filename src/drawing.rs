//! Pointer (cursor) render element. Bridges the cursor sprite from
//! [`crate::cursor`] (or a client-set `wl_surface`) into a renderer
//! element we can hand to smithay's `render_output`.
//!
//! Two variants because clients can take ownership of the cursor via
//! `wl_pointer.set_cursor`:
//! - [`PointerRenderElement::Memory`] is our default xcursor sprite,
//!   uploaded as a [`MemoryRenderBuffer`].
//! - [`PointerRenderElement::Surface`] is a client surface tree
//!   (terminals' I-beam, browser hand pointer, etc.).

use smithay::{
    backend::renderer::{
        element::{
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::WaylandSurfaceRenderElement,
            AsRenderElements, Kind,
        },
        ImportAll, ImportMem, Renderer, Texture,
    },
    desktop::{
        space::{space_render_elements, SpaceRenderElements},
        Space, Window,
    },
    input::pointer::CursorImageStatus,
    output::Output,
    render_elements,
    utils::{Physical, Point, Scale},
};

#[derive(Clone)]
pub struct PointerElement {
    buffer: Option<MemoryRenderBuffer>,
    status: CursorImageStatus,
}

impl Default for PointerElement {
    fn default() -> Self {
        Self {
            buffer: None,
            status: CursorImageStatus::default_named(),
        }
    }
}

impl PointerElement {
    pub fn set_status(&mut self, status: CursorImageStatus) {
        self.status = status;
    }

    pub fn set_buffer(&mut self, buffer: MemoryRenderBuffer) {
        self.buffer = Some(buffer);
    }
}

render_elements! {
    pub PointerRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Memory=MemoryRenderBufferRenderElement<R>,
}

// Output composition for the udev backend: stacks pointer above the space.
// Winit doesn't need this because `render_output` already takes a separate
// `custom_elements` slot for the cursor.
render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space=smithay::desktop::space::SpaceRenderElements<R, E>,
    Pointer=PointerRenderElement<R>,
    Border=smithay::backend::renderer::element::solid::SolidColorRenderElement,
    // F3-style diagnostics overlay (see [`crate::diag_overlay`]). On-screen
    // only — deliberately absent from `CaptureOverlay` so it never burns into
    // screenshots/screencasts the way the cursor and borders do.
    Overlay=smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement<R>,
}

impl<R: Renderer> std::fmt::Debug for PointerRenderElement<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(e) => f.debug_tuple("Surface").field(e).finish(),
            Self::Memory(e) => f.debug_tuple("Memory").field(e).finish(),
            Self::_GenericCatcher(_) => f.write_str("_GenericCatcher"),
        }
    }
}

// Overlay elements that ride above the window stack: the cursor (or a lock
// surface) and the server-side window borders. The capture path
// ([`crate::screencopy`]) takes a single `RenderElement` type for its custom
// slot, and cursor elements aren't `Clone`, so we fold both into one owned enum
// built once per frame — screencopy borrows it, then the on-screen render
// consumes it via [`CaptureOverlay::into_output_element`]. This is what keeps
// screenshots/screencasts showing the same borders the screen does.
render_elements! {
    pub CaptureOverlay<R> where R: ImportAll + ImportMem;
    Pointer=PointerRenderElement<R>,
    Border=smithay::backend::renderer::element::solid::SolidColorRenderElement,
}

impl<R: Renderer> std::fmt::Debug for CaptureOverlay<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pointer(e) => f.debug_tuple("Pointer").field(e).finish(),
            Self::Border(e) => f.debug_tuple("Border").field(e).finish(),
            Self::_GenericCatcher(_) => f.write_str("_GenericCatcher"),
        }
    }
}

impl<R: Renderer + ImportAll + ImportMem> CaptureOverlay<R> {
    /// Move this overlay into the on-screen [`OutputRenderElements`] enum,
    /// preserving its kind (cursor stays a pointer, border stays a border).
    pub fn into_output_element<E>(self) -> OutputRenderElements<R, E>
    where
        E: smithay::backend::renderer::element::RenderElement<R>,
    {
        match self {
            Self::Pointer(p) => OutputRenderElements::Pointer(p),
            Self::Border(b) => OutputRenderElements::Border(b),
            // `_GenericCatcher` holds `std::convert::Infallible` — uninhabited,
            // so this arm is statically unreachable.
            Self::_GenericCatcher(never) => match never {},
        }
    }
}

/// Build the space-layer render elements for `output` (everything below the
/// cursor). When `fullscreen` names a window — one in
/// [`crate::layout::LayoutState::Fullscreen`] on this output — render only that
/// window, dropping every layer-shell surface (bars/docks on the Top/Overlay
/// layers) and the other windows it covers edge-to-edge. Otherwise fall back to
/// smithay's normal stack (background/bottom layers, windows, top/overlay
/// layers) via `space_render_elements`.
pub fn output_space_elements<R>(
    renderer: &mut R,
    space: &Space<Window>,
    output: &Output,
    fullscreen: Option<&Window>,
) -> Vec<SpaceRenderElements<R, WaylandSurfaceRenderElement<R>>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + Texture + 'static,
{
    if let Some(window) = fullscreen {
        let scale = output.current_scale().fractional_scale();
        // The fullscreen surface sits at the output origin; render it relative
        // to this output's framebuffer (subtract the output's global location).
        let output_loc = space
            .output_geometry(output)
            .map(|g| g.loc)
            .unwrap_or_default();
        let loc = space.element_location(window).unwrap_or_default() - output_loc;
        AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
            window,
            renderer,
            loc.to_physical_precise_round(scale),
            Scale::from(scale),
            1.0,
        )
        .into_iter()
        .map(SpaceRenderElements::Surface)
        .collect()
    } else {
        space_render_elements(renderer, [space], output, 1.0).unwrap_or_default()
    }
}

impl<T, R> AsRenderElements<R> for PointerElement
where
    T: Texture + Clone + Send + 'static,
    R: Renderer<TextureId = T> + ImportAll + ImportMem,
{
    type RenderElement = PointerRenderElement<R>;

    fn render_elements<E>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<E>
    where
        E: From<PointerRenderElement<R>>,
    {
        match &self.status {
            CursorImageStatus::Hidden => vec![],
            CursorImageStatus::Named(_) => {
                let Some(buffer) = self.buffer.as_ref() else {
                    return vec![];
                };
                let Ok(el) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location.to_f64(),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) else {
                    return vec![];
                };
                vec![PointerRenderElement::<R>::from(el).into()]
            }
            CursorImageStatus::Surface(surface) => {
                let elements: Vec<PointerRenderElement<R>> =
                    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                        renderer,
                        surface,
                        location,
                        scale,
                        alpha,
                        Kind::Cursor,
                    );
                elements.into_iter().map(E::from).collect()
            }
        }
    }
}
