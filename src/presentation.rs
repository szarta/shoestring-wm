//! `wp_presentation` glue: attribute each rendered surface to the output it
//! scanned out on, then collect the per-output feedback objects so the backend
//! can mark them `presented` once the frame actually reaches the screen.
//!
//! The `wp_presentation` global itself is created in [`crate::state`]; this
//! module is the render-path side, shared by the udev (accurate, hardware
//! vblank) and winit (best-effort, submit-time) backends.

use smithay::{
    backend::renderer::element::{default_primary_scanout_output_compare, RenderElementStates},
    desktop::{
        layer_map_for_output,
        utils::{
            surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
            update_surface_primary_scanout_output, OutputPresentationFeedback,
        },
        Space, Window,
    },
    output::Output,
};

/// Record, for every surface that contributed to this frame, which output it
/// primarily scanned out on. Must run before [`take_presentation_feedback`]
/// (and before frame callbacks) so the surface→output attribution the feedback
/// collection relies on is up to date. Covers mapped windows and the output's
/// layer-shell surfaces (bars/docks); the cursor and drag icons are not
/// `wp_presentation` consumers, so they are left out.
pub fn update_primary_scanout_output(
    space: &Space<Window>,
    output: &Output,
    render_element_states: &RenderElementStates,
) {
    for window in space.elements() {
        window.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }
    let map = layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.with_surfaces(|surface, states| {
            update_surface_primary_scanout_output(
                surface,
                output,
                states,
                None,
                render_element_states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

/// Collect the committed `wp_presentation` feedback for every surface on
/// `output` into one [`OutputPresentationFeedback`]. The backend stashes the
/// returned value with the queued frame and, once the frame is on screen,
/// calls [`OutputPresentationFeedback::presented`] (or drops it, which
/// discards). [`update_primary_scanout_output`] must have run for this frame
/// first.
pub fn take_presentation_feedback(
    space: &Space<Window>,
    output: &Output,
    render_element_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut feedback = OutputPresentationFeedback::new(output);

    for window in space.elements() {
        if space.outputs_for_element(window).contains(output) {
            window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }
    }
    let map = layer_map_for_output(output);
    for layer_surface in map.layers() {
        layer_surface.take_presentation_feedback(
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| {
                surface_presentation_feedback_flags_from_states(
                    surface,
                    None,
                    render_element_states,
                )
            },
        );
    }

    feedback
}
