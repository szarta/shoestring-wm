//! Fractional-scale plumbing for `wp_fractional_scale_v1`.
//!
//! The compositor advertises `wp_fractional_scale_manager_v1` and
//! `wp_viewporter` (see [`crate::state::ShoestringWm::new`]). Together they let
//! a HiDPI-aware client render its buffer natively at the exact fractional
//! output scale (e.g. 1.5) and tell us, via viewporter, the logical size to
//! present it at — instead of the legacy path where the client renders at the
//! rounded integer scale (2) and we downsample to 1.5, which is the right size
//! but slightly soft.
//!
//! Smithay only *carries* the preferred-scale value; it's on us to push the
//! right number to each surface. [`send_preferred_scale`] does that for every
//! surface currently displayed on a given output.

use smithay::{
    desktop::{layer_map_for_output, Space, Window},
    output::Output,
    wayland::fractional_scale::with_fractional_scale,
};

/// Tell every surface displayed on `output` its preferred fractional scale.
///
/// `set_preferred_scale` only emits a `wp_fractional_scale_v1.preferred_scale`
/// event when the value actually changes (and only once the client has created
/// the fractional-scale object), so calling this every frame is cheap and is
/// what keeps clients in sync when an output's scale changes or a window moves
/// between outputs of different scales.
pub fn send_preferred_scale(space: &Space<Window>, output: &Output) {
    let scale = output.current_scale().fractional_scale();

    for window in space.elements() {
        if space.outputs_for_element(window).contains(output) {
            window.with_surfaces(|_surface, states| {
                with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(scale);
                });
            });
        }
    }

    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.with_surfaces(|_surface, states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}
