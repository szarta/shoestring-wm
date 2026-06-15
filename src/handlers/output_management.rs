//! Dispatch impls for `zwlr_output_management_unstable_v1`.
//! State and data types live in [`crate::output_management`].

use std::sync::{Arc, Mutex};

use smithay::{
    reexports::{
        wayland_protocols_wlr::output_management::v1::server::{
            zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
            zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
            zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
            zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
            zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
        },
        wayland_server::{Client, DataInit, DisplayHandle, New, Resource},
    },
    utils::{Logical, Point, Transform},
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{
    output_management::{
        ConfigHeadData, ConfigurationData, HeadData, HeadIntent, ModeChoice, ModeData,
        OutputManagerData,
    },
    state::ShoestringWm,
};

// ── Manager global ────────────────────────────────────────────────────────────

impl GlobalDispatch2<ZwlrOutputManagerV1, ShoestringWm> for OutputManagerData {
    fn bind(
        &self,
        state: &mut ShoestringWm,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        let manager = data_init.init(resource, OutputManagerData);

        // Announce every current output as a head and track the objects.
        let outputs: Vec<_> = state.space.outputs().cloned().collect();
        for output in &outputs {
            if let Some(head) = crate::output_management::announce_head(&manager, output, dh) {
                state.output_management.heads.push(head);
            }
        }

        let serial = state.output_management.serial;
        manager.done(serial);

        state.output_management.managers.push(manager);
    }
}

impl Dispatch2<ZwlrOutputManagerV1, ShoestringWm> for OutputManagerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(
                    id,
                    ConfigurationData {
                        serial,
                        heads: Mutex::new(Vec::new()),
                    },
                );
            }
            zwlr_output_manager_v1::Request::Stop => {
                // Client is done — remove from broadcast list.
                // The resource is destroyed after this returns.
            }
            _ => {}
        }
        let _ = state;
    }

    fn destroyed(
        &self,
        state: &mut ShoestringWm,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &ZwlrOutputManagerV1,
    ) {
        state.output_management.managers.retain(|m| m != resource);
    }
}

// ── Head (server → client events only; no client requests except `release`) ───

impl Dispatch2<ZwlrOutputHeadV1, ShoestringWm> for HeadData {
    fn request(
        &self,
        _state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        if let zwlr_output_head_v1::Request::Release = request {}
    }
}

// ── Mode (server → client events only; no client requests except `release`) ───

impl Dispatch2<ZwlrOutputModeV1, ShoestringWm> for ModeData {
    fn request(
        &self,
        _state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        if let zwlr_output_mode_v1::Request::Release = request {}
    }
}

// ── Configuration ─────────────────────────────────────────────────────────────

impl Dispatch2<ZwlrOutputConfigurationV1, ShoestringWm> for ConfigurationData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(head_data) = head.data::<HeadData>() else {
                    return;
                };
                let intent = Arc::new(Mutex::new(HeadIntent {
                    enabled: true,
                    mode: None,
                    position: None,
                    transform: None,
                    scale: None,
                }));
                data_init.init(
                    id,
                    ConfigHeadData {
                        output: head_data.output.clone(),
                        intent: intent.clone(),
                    },
                );
                // Push the intent now that ConfigurationData.heads is a Mutex.
                if let Some(cd) = resource.data::<ConfigurationData>() {
                    cd.heads
                        .lock()
                        .unwrap()
                        .push((head_data.output.clone(), intent));
                }
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let Some(head_data) = head.data::<HeadData>() else {
                    return;
                };
                let intent = Arc::new(Mutex::new(HeadIntent {
                    enabled: false,
                    mode: None,
                    position: None,
                    transform: None,
                    scale: None,
                }));
                if let Some(cd) = resource.data::<ConfigurationData>() {
                    cd.heads
                        .lock()
                        .unwrap()
                        .push((head_data.output.clone(), intent));
                }
            }
            zwlr_output_configuration_v1::Request::Apply => {
                apply_configuration(state, resource, false);
            }
            zwlr_output_configuration_v1::Request::Test => {
                apply_configuration(state, resource, true);
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// ── Configuration head ────────────────────────────────────────────────────────

impl Dispatch2<ZwlrOutputConfigurationHeadV1, ShoestringWm> for ConfigHeadData {
    fn request(
        &self,
        _state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        let mut intent = self.intent.lock().unwrap();
        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if let Some(mode_data) = mode.data::<ModeData>() {
                    intent.mode = Some(ModeChoice::Named {
                        size: mode_data.size,
                        refresh: mode_data.refresh,
                    });
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                intent.mode = Some(ModeChoice::Custom {
                    size: (width, height).into(),
                    refresh,
                });
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                intent.position = Some(Point::<i32, Logical>::from((x, y)));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform {
                transform: smithay::reexports::wayland_server::WEnum::Value(t),
            } => {
                intent.transform = Some(smithay_transform(t));
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                intent.scale = Some(scale);
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { .. } => {
                // Not yet implemented.
            }
            _ => {}
        }
    }
}

// ── Apply logic ───────────────────────────────────────────────────────────────

/// Apply (or test) the configuration the client submitted.
///
/// For each enabled head we apply, in order:
///   1. Mode change (udev/DRM path only — delegates to `udev::change_output_mode`)
///   2. Transform / scale / position (compositor-space via `Output::change_current_state`
///      + `Space::map_output`)
///
/// Disabled heads are skipped for now (turning off a CRTC requires tearing down
/// the DrmOutput, which is a larger change).
fn apply_configuration(
    state: &mut ShoestringWm,
    config: &ZwlrOutputConfigurationV1,
    test_only: bool,
) {
    let config_data = match config.data::<ConfigurationData>() {
        Some(d) => d,
        None => {
            config.failed();
            return;
        }
    };

    // Reject stale serial.
    if config_data.serial != state.output_management.serial {
        tracing::warn!(
            client_serial = config_data.serial,
            compositor_serial = state.output_management.serial,
            "output configuration serial mismatch; cancelling"
        );
        config.cancelled();
        return;
    }

    // Snapshot the intent list so we don't hold the lock across state borrows.
    let heads: Vec<(smithay::output::Output, HeadIntent)> = config_data
        .heads
        .lock()
        .unwrap()
        .iter()
        .map(|(output, intent)| {
            let i = intent.lock().unwrap();
            let intent_copy = HeadIntent {
                enabled: i.enabled,
                mode: match &i.mode {
                    None => None,
                    Some(ModeChoice::Named { size, refresh }) => Some(ModeChoice::Named {
                        size: *size,
                        refresh: *refresh,
                    }),
                    Some(ModeChoice::Custom { size, refresh }) => Some(ModeChoice::Custom {
                        size: *size,
                        refresh: *refresh,
                    }),
                },
                position: i.position,
                transform: i.transform,
                scale: i.scale,
            };
            (output.clone(), intent_copy)
        })
        .collect();

    if test_only {
        config.succeeded();
        return;
    }

    // Only the tty backend's branches below flip this; under winit it stays
    // false (all the mutation sites are `#[cfg(feature = "tty")]`).
    #[cfg_attr(not(feature = "tty"), allow(unused_mut))]
    let mut any_failed = false;

    for (output, intent) in &heads {
        if !intent.enabled {
            #[cfg(feature = "tty")]
            {
                if !crate::backend::udev::disable_output(state, output) {
                    tracing::warn!(output = output.name(), "output disable failed");
                    any_failed = true;
                }
            }
            #[cfg(not(feature = "tty"))]
            {
                tracing::debug!(
                    output = output.name(),
                    "output disable ignored (winit backend)"
                );
            }
            continue;
        }

        // ── Mode change (DRM backend only) ──────────────────────────────────
        if let Some(mode_choice) = &intent.mode {
            let (req_size, req_refresh) = match mode_choice {
                ModeChoice::Named { size, refresh } | ModeChoice::Custom { size, refresh } => {
                    (*size, *refresh)
                }
            };
            #[cfg(feature = "tty")]
            {
                if !crate::backend::udev::change_output_mode(state, output, req_size, req_refresh) {
                    tracing::warn!(output = output.name(), "mode change failed");
                    any_failed = true;
                    continue;
                }
            }
            #[cfg(not(feature = "tty"))]
            {
                let _ = (req_size, req_refresh);
                tracing::debug!(
                    output = output.name(),
                    "mode change ignored (winit backend)"
                );
            }
        }

        // ── Transform / scale / position (compositor-space) ─────────────────
        let new_transform = intent.transform;
        let new_scale = intent.scale.map(smithay::output::Scale::Fractional);
        let new_position = intent.position;

        output.change_current_state(None, new_transform, new_scale, new_position);

        if let Some(pos) = new_position {
            state.space.map_output(output, pos);
        }

        tracing::info!(
            output = output.name(),
            ?new_transform,
            ?new_scale,
            ?new_position,
            "output configuration applied"
        );

        // Kick a render on all CRTCs of this output.
        #[cfg(feature = "tty")]
        {
            use crate::backend::udev::UdevOutputId;
            if let Some(udev_id) = output.user_data().get::<UdevOutputId>() {
                let node = udev_id.device_id;
                let crtc = udev_id.crtc;
                state.loop_handle.insert_idle(move |s| {
                    s.render_surface(node, crtc);
                });
            }
        }
    }

    if any_failed {
        config.failed();
    } else {
        config.succeeded();
        crate::output_management::broadcast_done(state);
    }
}

fn smithay_transform(
    t: smithay::reexports::wayland_server::protocol::wl_output::Transform,
) -> Transform {
    use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlT;
    match t {
        WlT::Normal => Transform::Normal,
        WlT::_90 => Transform::_90,
        WlT::_180 => Transform::_180,
        WlT::_270 => Transform::_270,
        WlT::Flipped => Transform::Flipped,
        WlT::Flipped90 => Transform::Flipped90,
        WlT::Flipped180 => Transform::Flipped180,
        WlT::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}
