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
    input::pointer::CursorImageStatus,
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
