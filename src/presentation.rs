//! `wp_presentation` glue: attribute each rendered surface to the output it
//! scanned out on, then collect the per-output feedback objects so the backend
//! can mark them `presented` once the frame actually reaches the screen.
//!
//! The `wp_presentation` global itself is created in [`crate::state`]; this
//! module is the render-path side, shared by the udev (accurate, hardware
//! vblank) and winit (best-effort, submit-time) backends.

use std::time::Duration;

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

/// Send `wl_surface.frame` callbacks to every client drawing on `output`:
/// mapped toplevel windows *and* the output's layer-shell surfaces
/// (bars/docks/panels). Without this, frame-callback-driven layer clients sit
/// idle forever — our own shoestring-bar polls on a timer so it never noticed,
/// but spec-compliant panels (and the WLCS layer-shell suite, which blocks on
/// `wl_surface.frame` before checking surface position) hang. Called once per
/// rendered frame by every backend.
pub fn send_frames(space: &Space<Window>, output: &Output, time: Duration) {
    for window in space.elements() {
        window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
}

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
