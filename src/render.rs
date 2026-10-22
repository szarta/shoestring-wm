//! Renderer plumbing shared by the backends and the capture paths.
//!
//! Offscreen captures (screencopy, ext-image-copy, window capture, probes)
//! render into a target of the renderer's own type: a [`GlesTexture`] for the
//! GLES renderers, a pixman `Image` for the headless backend's CPU renderer.
//! [`OffscreenRenderer`] names that type, so the capture code is written once
//! for every renderer.

use smithay::backend::renderer::{
    gles::{GlesRenderer, GlesTexture},
    Bind, Offscreen, Renderer,
};

/// A renderer that can render into offscreen targets of its own type.
pub trait OffscreenRenderer: Renderer + Bind<Self::Target> + Offscreen<Self::Target> {
    /// The offscreen render target. `'static` so capture state can hold one
    /// without naming the renderer (it is stored type-erased).
    type Target: 'static;
}

impl OffscreenRenderer for GlesRenderer {
    type Target = GlesTexture;
}

#[cfg(feature = "headless")]
impl OffscreenRenderer for smithay::backend::renderer::pixman::PixmanRenderer {
    type Target = smithay::reexports::pixman::Image<'static, 'static>;
}
