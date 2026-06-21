//! Server-side window decorations: a focus-aware solid-color border ring drawn
//! just inside each window's edges.
//!
//! Off by default — only drawn when `[decorations].border_width > 0` in the
//! config (the no-decorations workflow stays the default). The border sits
//! *inside* the window's own logical rect, so it never bleeds onto a
//! neighboring tile; in gapless tiling the shared seam shows each side's
//! border, and the focused window is distinguished by color.
//!
//! Each window's four edge rectangles are backed by persistent
//! [`SolidColorBuffer`]s kept in [`crate::state::ShoestringWm`]. Persistence is
//! load-bearing for damage tracking: a `SolidColorBuffer` only bumps its commit
//! counter when its size/color actually change, so a static border contributes
//! no per-frame damage. Rebuilding the buffers every frame would instead mint a
//! fresh element id each time, marking the whole output damaged forever and
//! defeating the idle path.

use std::collections::HashMap;

use smithay::{
    backend::renderer::element::{
        solid::{SolidColorBuffer, SolidColorRenderElement},
        Kind,
    },
    desktop::{Space, Window},
    output::Output,
    utils::{Logical, Rectangle},
};

/// Persistent solid-color buffers for one window's border ring, in the order
/// `[top, bottom, left, right]`. Reused across frames so element ids stay
/// stable (see the module docs).
#[derive(Debug, Default)]
pub struct WindowBorder {
    edges: [SolidColorBuffer; 4],
}

/// The four edge rectangles of a `width`-thick border drawn *inside* `rect`
/// (global logical coords), in `[top, bottom, left, right]` order. The top and
/// bottom bars span the full width (covering the corners); the left and right
/// bars fill the gap between them. Returns `None` when `rect` is too small to
/// hold the ring without the bars overlapping (`width <= 0`, or either
/// dimension `< 2 * width`) — such a window simply gets no border.
fn ring_rects(rect: Rectangle<i32, Logical>, width: i32) -> Option<[Rectangle<i32, Logical>; 4]> {
    let (x, y) = (rect.loc.x, rect.loc.y);
    let (w, h) = (rect.size.w, rect.size.h);
    if width <= 0 || w < 2 * width || h < 2 * width {
        return None;
    }
    let inner_h = h - 2 * width;
    Some([
        // top
        Rectangle::new((x, y).into(), (w, width).into()),
        // bottom
        Rectangle::new((x, y + h - width).into(), (w, width).into()),
        // left
        Rectangle::new((x, y + width).into(), (width, inner_h).into()),
        // right
        Rectangle::new((x + w - width, y + width).into(), (width, inner_h).into()),
    ])
}

/// Premultiply straight RGBA (`0.0..=1.0`) so the solid fill composites
/// correctly when a border color carries alpha. Opaque colors (`a == 1`) are
/// unchanged.
fn premultiply([r, g, b, a]: [f32; 4]) -> [f32; 4] {
    [r * a, g * a, b * a, a]
}

/// Build the border render elements for every window on `output`, in
/// output-local physical coordinates (the same space the cursor and space
/// elements use). Updates the persistent per-window buffers in `borders` and
/// prunes entries for windows that are no longer mapped.
///
/// Callers gate this on `border_width > 0` and skip it while locked or when a
/// fullscreen window owns the output (a fullscreen surface covers the screen
/// edge-to-edge, leaving no border to draw).
// `Window` hashes by a stable internal id, so its interior mutability is
// irrelevant to its use as a map key — the same reason the rest of the WM keys
// maps on `Window` (foreign_toplevels, layout meta, …).
#[allow(clippy::too_many_arguments, clippy::mutable_key_type)]
pub fn border_elements(
    borders: &mut HashMap<Window, WindowBorder>,
    space: &Space<Window>,
    output: &Output,
    focused: Option<&Window>,
    width: i32,
    focused_color: [f32; 4],
    unfocused_color: [f32; 4],
    scale: f64,
) -> Vec<SolidColorRenderElement> {
    // Drop buffers for windows that have since unmapped (minimized windows are
    // unmapped from the space too, so they fall out here and stop drawing).
    borders.retain(|w, _| space.element_location(w).is_some());

    if width <= 0 {
        return Vec::new();
    }

    let focused_rgba = premultiply(focused_color);
    let unfocused_rgba = premultiply(unfocused_color);
    let output_loc = space
        .output_geometry(output)
        .map(|g| g.loc)
        .unwrap_or_default();
    let output_geo = space.output_geometry(output);

    let mut elements = Vec::new();
    for window in space.elements() {
        // Only windows that actually touch this output (a window may span two).
        let Some(geo) = space.element_geometry(window) else {
            continue;
        };
        if let Some(o) = output_geo {
            if !o.overlaps(geo) {
                continue;
            }
        }
        let Some(rects) = ring_rects(geo, width) else {
            continue;
        };
        let color = if focused == Some(window) {
            focused_rgba
        } else {
            unfocused_rgba
        };

        let border = borders.entry(window.clone()).or_default();
        for (edge, rect) in border.edges.iter_mut().zip(rects) {
            edge.update(rect.size, color);
            // Global logical → output-local logical → physical.
            let local = rect.loc - output_loc;
            elements.push(SolidColorRenderElement::from_buffer(
                edge,
                local.to_physical_precise_round(scale),
                scale,
                1.0,
                Kind::Unspecified,
            ));
        }
    }
    elements
}

impl crate::state::ShoestringWm {
    /// Border render elements for `output`, in output-local physical coords,
    /// ready to stack above the window layer. Returns empty while `locked`,
    /// when `fullscreen` owns the output, or when borders are disabled
    /// (`border_width == 0`, the default). Shared by both backends so the
    /// gating lives in one place.
    pub fn output_border_elements(
        &mut self,
        output: &Output,
        locked: bool,
        fullscreen: Option<&Window>,
    ) -> Vec<SolidColorRenderElement> {
        let width = self.config.decorations.border_width as i32;
        if locked || fullscreen.is_some() || width <= 0 {
            // Still prune buffers so a window that unmapped (or the feature
            // being toggled off via reload) doesn't leak entries.
            self.window_borders
                .retain(|w, _| self.space.element_location(w).is_some());
            return Vec::new();
        }
        let focused = self.focused_window();
        let focused_color = self.config.decorations.focused_rgba();
        let unfocused_color = self.config.decorations.unfocused_rgba();
        let scale = output.current_scale().fractional_scale();
        border_elements(
            &mut self.window_borders,
            &self.space,
            output,
            focused.as_ref(),
            width,
            focused_color,
            unfocused_color,
            scale,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_too_small_returns_none() {
        // width 0 disables; window smaller than 2*width can't hold the ring.
        let r = Rectangle::new((0, 0).into(), (100, 100).into());
        assert!(ring_rects(r, 0).is_none());
        assert!(ring_rects(r, 60).is_none()); // 100 < 2*60
        let thin = Rectangle::new((0, 0).into(), (3, 100).into());
        assert!(ring_rects(thin, 2).is_none()); // width 3 < 2*2
    }

    #[test]
    fn ring_partitions_the_frame() {
        // A 2px ring inside a 100x80 window at (10, 20).
        let r = Rectangle::new((10, 20).into(), (100, 80).into());
        let [top, bottom, left, right] = ring_rects(r, 2).unwrap();

        // Top/bottom span the full width at the very top/bottom edges.
        assert_eq!(top, Rectangle::new((10, 20).into(), (100, 2).into()));
        assert_eq!(bottom, Rectangle::new((10, 98).into(), (100, 2).into()));
        // Left/right are the gap between the bars, full-thickness wide.
        assert_eq!(left, Rectangle::new((10, 22).into(), (2, 76).into()));
        assert_eq!(right, Rectangle::new((108, 22).into(), (2, 76).into()));

        // Every edge stays within the window's own rect (no bleed onto a
        // neighbor) — the whole point of drawing the border inside the rect.
        for edge in [top, bottom, left, right] {
            assert!(edge.loc.x >= r.loc.x && edge.loc.y >= r.loc.y);
            assert!(edge.loc.x + edge.size.w <= r.loc.x + r.size.w);
            assert!(edge.loc.y + edge.size.h <= r.loc.y + r.size.h);
        }
    }

    #[test]
    fn premultiply_scales_rgb_by_alpha() {
        assert_eq!(premultiply([1.0, 0.0, 0.0, 1.0]), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(premultiply([1.0, 0.5, 0.0, 0.5]), [0.5, 0.25, 0.0, 0.5]);
    }
}
