//! wlr-gamma-control dispatch. State lives in [`crate::gamma_control`]; the
//! DRM read/apply/restore glue is on [`ShoestringWm`] in
//! [`crate::backend::udev`].

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{
            backend::ClientId, protocol::wl_output::WlOutput, Client, DataInit, DisplayHandle, New,
            Resource,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{
    gamma_control::{
        read_ramp_from_fd, GammaControl, GammaControlData, GammaControlManagerData, GammaOwner,
        GammaRamp, GammaReadError,
    },
    state::ShoestringWm,
};

// ---------------- Manager ----------------

impl GlobalDispatch2<ZwlrGammaControlManagerV1, ShoestringWm> for GammaControlManagerData {
    fn bind(
        &self,
        _state: &mut ShoestringWm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        data_init.init(resource, GammaControlManagerData);
    }
}

impl Dispatch2<ZwlrGammaControlManagerV1, ShoestringWm> for GammaControlManagerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_gamma_control_manager_v1::Request;
        match request {
            Request::GetGammaControl { id, output } => {
                create_control(state, id, &output, data_init);
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

fn create_control(
    state: &mut ShoestringWm,
    new_ctrl: New<ZwlrGammaControlV1>,
    wl_output: &WlOutput,
    data_init: &mut DataInit<'_, ShoestringWm>,
) {
    let output = Output::from_resource(wl_output);
    let target = output
        .as_ref()
        .and_then(|o| state.gamma_target_for_output(o));

    let Some((node, crtc, size)) = target else {
        // Unknown or non-KMS output: hand back a resource and immediately fail
        // it so the client tears down (per the protocol's `failed` event).
        let ctrl = data_init.init(
            new_ctrl,
            GammaControlData {
                output_name: String::new(),
            },
        );
        ctrl.failed();
        return;
    };

    let name = output.unwrap().name();
    let ctrl = data_init.init(
        new_ctrl,
        GammaControlData {
            output_name: name.clone(),
        },
    );

    // Exclusivity: at most one control per output. A new request transfers
    // control — the previous holder is failed, but we carry its snapshot of the
    // true original ramp forward so a later release still restores it.
    let prior_original = state.gamma_control.controls.remove(&name).map(|old| {
        if let GammaOwner::Client(r) = &old.owner {
            r.failed();
        }
        old.original
    });
    let original = prior_original
        .or_else(|| state.read_gamma(node, crtc, size))
        .unwrap_or_else(|| GammaRamp::identity(size));

    state.gamma_control.controls.insert(
        name,
        GammaControl {
            owner: GammaOwner::Client(ctrl.clone()),
            node,
            crtc,
            size,
            original,
            current: None,
        },
    );
    ctrl.gamma_size(size);
}

// ---------------- Control ----------------

impl Dispatch2<ZwlrGammaControlV1, ShoestringWm> for GammaControlData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_gamma_control_v1::Request;
        match request {
            Request::SetGamma { fd } => {
                // Resolve the live control. If it's gone (already failed or
                // transferred away), the request is a no-op.
                let Some((node, crtc, size, is_active)) = state
                    .gamma_control
                    .controls
                    .get(&self.output_name)
                    .map(|c| (c.node, c.crtc, c.size, c.owner.is_client(resource)))
                else {
                    return;
                };
                if !is_active {
                    return;
                }

                match read_ramp_from_fd(fd, size) {
                    Ok(ramp) => {
                        if state.apply_gamma(node, crtc, &ramp) {
                            if let Some(c) = state.gamma_control.controls.get_mut(&self.output_name)
                            {
                                c.current = Some(ramp);
                            }
                        } else {
                            // The CRTC rejected the ramp: fail the control and
                            // put the original table back.
                            resource.failed();
                            state.release_gamma_control(&self.output_name, resource);
                        }
                    }
                    Err(GammaReadError::BadSize) => {
                        resource.post_error(
                            zwlr_gamma_control_v1::Error::InvalidGamma,
                            "gamma table has the wrong size",
                        );
                    }
                    Err(GammaReadError::Io) => {
                        resource.failed();
                        state.release_gamma_control(&self.output_name, resource);
                    }
                }
            }
            Request::Destroy => {}
            _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: ClientId,
        resource: &ZwlrGammaControlV1,
    ) {
        // Destroying the control restores the output's original gamma.
        state.release_gamma_control(&self.output_name, resource);
    }
}
