//! wlr-virtual-pointer dispatch. State + frame replay live in
//! [`crate::virtual_pointer`].
//!
//! The manager hands out `zwlr_virtual_pointer_v1` objects; each buffers its
//! motion/button/axis requests and flushes them through the seat pointer on
//! `frame`. The optional `seat` argument is a hint only — we have a single
//! seat, so it's ignored.

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState},
    output::Output,
    reexports::{
        wayland_protocols_wlr::virtual_pointer::v1::server::{
            zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
            zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
        },
        wayland_server::{
            protocol::wl_pointer::{
                Axis as WlAxis, AxisSource as WlAxisSource, ButtonState as WlButtonState,
            },
            Client, DataInit, DisplayHandle, New, Resource, WEnum,
        },
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::{
    state::ShoestringWm,
    virtual_pointer::{VirtualPointerData, VirtualPointerManagerData},
};

// ---------------- Manager ----------------

impl GlobalDispatch2<ZwlrVirtualPointerManagerV1, ShoestringWm> for VirtualPointerManagerData {
    fn bind(
        &self,
        _state: &mut ShoestringWm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        data_init.init(resource, VirtualPointerManagerData);
    }
}

impl Dispatch2<ZwlrVirtualPointerManagerV1, ShoestringWm> for VirtualPointerManagerData {
    fn request(
        &self,
        _state: &mut ShoestringWm,
        _client: &Client,
        _resource: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_virtual_pointer_manager_v1::Request;
        match request {
            // The `seat` arg is a suggestion only (single seat); ignore it.
            Request::CreateVirtualPointer { seat: _, id } => {
                data_init.init(id, VirtualPointerData::new(None));
            }
            Request::CreateVirtualPointerWithOutput {
                seat: _,
                output,
                id,
            } => {
                let output_name = output
                    .as_ref()
                    .and_then(Output::from_resource)
                    .map(|o| o.name());
                data_init.init(id, VirtualPointerData::new(output_name));
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------- Pointer ----------------

impl Dispatch2<ZwlrVirtualPointerV1, ShoestringWm> for VirtualPointerData {
    fn request(
        &self,
        state: &mut ShoestringWm,
        _client: &Client,
        resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, ShoestringWm>,
    ) {
        use zwlr_virtual_pointer_v1::Request;
        match request {
            Request::Motion { dx, dy, .. } => {
                let mut pending = self.pending.lock().unwrap();
                let base = pending.location.unwrap_or_else(|| {
                    state
                        .seat
                        .get_pointer()
                        .map(|p| p.current_location())
                        .unwrap_or_default()
                });
                pending.location = Some((base.x + dx, base.y + dy).into());
            }
            Request::MotionAbsolute {
                x,
                y,
                x_extent,
                y_extent,
                ..
            } => {
                if let Some(loc) = state.virtual_pointer_absolute(
                    self.output_name.as_deref(),
                    x,
                    y,
                    x_extent,
                    y_extent,
                ) {
                    self.pending.lock().unwrap().location = Some(loc);
                }
            }
            Request::Button {
                button,
                state: btn_state,
                ..
            } => {
                if let Some(s) = map_button_state(btn_state) {
                    self.pending.lock().unwrap().buttons.push((button, s));
                }
            }
            Request::Axis { axis, value, .. } => match map_axis(axis) {
                Some(a) => {
                    let mut pending = self.pending.lock().unwrap();
                    pending.has_axis = true;
                    add_axis_value(&mut pending.axis_value, a, value);
                }
                None => {
                    resource.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis")
                }
            },
            Request::AxisDiscrete {
                axis,
                value,
                discrete,
                ..
            } => match map_axis(axis) {
                Some(a) => {
                    let mut pending = self.pending.lock().unwrap();
                    pending.has_axis = true;
                    add_axis_value(&mut pending.axis_value, a, value);
                    // wl_pointer carries discrete steps as v120 units (120 per
                    // step); smithay sends `axis_discrete = v120 / 120` to v5–7
                    // clients, recovering the original step count.
                    let v120 = discrete.saturating_mul(120);
                    match a {
                        Axis::Horizontal => pending.axis_v120.0 = Some(v120),
                        Axis::Vertical => pending.axis_v120.1 = Some(v120),
                    }
                }
                None => {
                    resource.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis")
                }
            },
            Request::AxisStop { axis, .. } => match map_axis(axis) {
                Some(a) => {
                    let mut pending = self.pending.lock().unwrap();
                    pending.has_axis = true;
                    match a {
                        Axis::Horizontal => pending.axis_stop.0 = true,
                        Axis::Vertical => pending.axis_stop.1 = true,
                    }
                }
                None => {
                    resource.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis")
                }
            },
            Request::AxisSource { axis_source } => match map_axis_source(axis_source) {
                Some(s) => {
                    let mut pending = self.pending.lock().unwrap();
                    pending.has_axis = true;
                    pending.axis_source = Some(s);
                }
                None => resource.post_error(
                    zwlr_virtual_pointer_v1::Error::InvalidAxisSource,
                    "invalid axis source",
                ),
            },
            Request::Frame => state.flush_virtual_pointer_frame(self),
            Request::Destroy => {}
            _ => {}
        }
    }
}

fn add_axis_value(acc: &mut (f64, f64), axis: Axis, value: f64) {
    match axis {
        Axis::Horizontal => acc.0 += value,
        Axis::Vertical => acc.1 += value,
    }
}

fn map_axis(w: WEnum<WlAxis>) -> Option<Axis> {
    match w {
        WEnum::Value(WlAxis::HorizontalScroll) => Some(Axis::Horizontal),
        WEnum::Value(WlAxis::VerticalScroll) => Some(Axis::Vertical),
        _ => None,
    }
}

fn map_button_state(w: WEnum<WlButtonState>) -> Option<ButtonState> {
    match w {
        WEnum::Value(WlButtonState::Pressed) => Some(ButtonState::Pressed),
        WEnum::Value(WlButtonState::Released) => Some(ButtonState::Released),
        _ => None,
    }
}

fn map_axis_source(w: WEnum<WlAxisSource>) -> Option<AxisSource> {
    match w {
        WEnum::Value(WlAxisSource::Wheel) => Some(AxisSource::Wheel),
        WEnum::Value(WlAxisSource::Finger) => Some(AxisSource::Finger),
        WEnum::Value(WlAxisSource::Continuous) => Some(AxisSource::Continuous),
        WEnum::Value(WlAxisSource::WheelTilt) => Some(AxisSource::WheelTilt),
        _ => None,
    }
}
