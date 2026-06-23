//! Handler impls for `ext-image-copy-capture-v1` / `ext-image-capture-source-v1`.
//! Smithay owns the wire dispatch (via `delegate_dispatch2!` in
//! [`crate::handlers`]); we only supply policy: which sources are capturable,
//! their buffer constraints, and the actual render. State + the render-path
//! capture live in [`crate::ext_screencopy`].

use smithay::{
    output::Output,
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, Frame, ImageCopyCaptureHandler, ImageCopyCaptureState, Session,
            SessionRef,
        },
    },
};

use crate::{
    ext_screencopy::{output_of_session, CaptureFailureReason, PendingExtFrame},
    state::ShoestringWm,
};

impl ImageCaptureSourceHandler for ShoestringWm {
    fn source_destroyed(&mut self, _source: ImageCaptureSource) {}
}

impl OutputCaptureSourceHandler for ShoestringWm {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.ext_screencopy.output_capture_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        // Stash the output so `capture_constraints`/`frame` can resolve it.
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for ShoestringWm {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.ext_screencopy.image_copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        // Authoritative gate check: the filter hides the global from clients
        // that connect while capture is off, but one that bound it earlier can
        // still reach here — refuse so no session is ever created while off.
        if !self.screen_capture_enabled {
            return None;
        }
        let output = output_of_source(source)?;
        self.ext_capture_constraints_for_output(&output)
    }

    fn new_session(&mut self, session: Session) {
        // Keep the owned handle alive; smithay already pushed a SessionRef into
        // its own routing list and sent the initial constraints.
        self.ext_screencopy.sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        if !self.screen_capture_enabled {
            frame.fail(CaptureFailureReason::Stopped);
            return;
        }
        let Some(output) = output_of_session(session) else {
            // Non-output (e.g. toplevel) source, or the output is gone.
            frame.fail(CaptureFailureReason::Unknown);
            return;
        };
        // Queue for the target output's next render and wake it. The live
        // "screen is being read" indicator fires here, throttled.
        self.ext_screencopy.pending.push(PendingExtFrame {
            frame,
            output: output.clone(),
        });
        self.kick_render_for_screencopy(&output);
        self.note_screen_capture(&output.name());
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.ext_screencopy.sessions.retain(|s| *s != session);
    }
}

/// Resolve the output backing a capture source, if it is an output source.
fn output_of_source(source: &ImageCaptureSource) -> Option<Output> {
    source
        .user_data()
        .get::<smithay::output::WeakOutput>()?
        .upgrade()
}
