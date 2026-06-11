//! Apply the `[input]` config section ([`shoestring_config::Input`]) to
//! libinput devices.
//!
//! Settings are pushed to each device when it connects (`DeviceAdded` in the
//! udev backend) and re-pushed to every connected device on config hot-reload.
//! Only set fields are touched, so an unconfigured device keeps libinput's own
//! defaults; a setting the hardware doesn't support is logged at debug and
//! skipped, which is why a single global section can be applied blindly to
//! every device (the wired mouse ignores tap-to-click, the touchpad applies
//! it). This is the udev/TTY backend only — the nested winit backend has no
//! real libinput devices.

use shoestring_config::{
    AccelProfile as CfgAccelProfile, ClickMethod as CfgClickMethod, Input,
    ScrollMethod as CfgScrollMethod, TapButtonMap as CfgTapButtonMap,
};
use smithay::reexports::input::{
    self as libinput, AccelProfile, ClickMethod, DragLockState, ScrollMethod, TapButtonMap,
};

/// Apply every set field of `cfg` to `device`. Unset fields and unsupported
/// settings are skipped.
pub fn apply(device: &mut libinput::Device, cfg: &Input) {
    let name = device.name().to_string();
    // Log (at debug — `Unsupported` is normal for mismatched device/setting
    // pairs) but never fail: a rejected setting must not stop the others.
    let log = |setting: &str, r: libinput::DeviceConfigResult| {
        if let Err(e) = r {
            tracing::debug!(device = %name, setting, error = ?e, "input: setting not applied");
        }
    };

    if let Some(enabled) = cfg.tap_to_click {
        log("tap_to_click", device.config_tap_set_enabled(enabled));
    }
    if let Some(map) = cfg.tap_button_map {
        let map = match map {
            CfgTapButtonMap::LeftRightMiddle => TapButtonMap::LeftRightMiddle,
            CfgTapButtonMap::LeftMiddleRight => TapButtonMap::LeftMiddleRight,
        };
        log("tap_button_map", device.config_tap_set_button_map(map));
    }
    if let Some(enabled) = cfg.tap_and_drag {
        log("tap_and_drag", device.config_tap_set_drag_enabled(enabled));
    }
    if let Some(enabled) = cfg.drag_lock {
        // `EnabledTimeout` is the classic drag-lock behaviour; sticky mode is
        // a newer libinput addition we don't expose.
        let state = if enabled {
            DragLockState::EnabledTimeout
        } else {
            DragLockState::Disabled
        };
        log("drag_lock", device.config_tap_set_drag_lock_enabled(state));
    }
    if let Some(enabled) = cfg.natural_scroll {
        log(
            "natural_scroll",
            device.config_scroll_set_natural_scroll_enabled(enabled),
        );
    }
    if let Some(method) = cfg.scroll_method {
        let method = match method {
            CfgScrollMethod::None => ScrollMethod::NoScroll,
            CfgScrollMethod::TwoFinger => ScrollMethod::TwoFinger,
            CfgScrollMethod::Edge => ScrollMethod::Edge,
            CfgScrollMethod::OnButtonDown => ScrollMethod::OnButtonDown,
        };
        log("scroll_method", device.config_scroll_set_method(method));
    }
    if let Some(button) = cfg.scroll_button {
        log("scroll_button", device.config_scroll_set_button(button));
    }
    if let Some(method) = cfg.click_method {
        let method = match method {
            CfgClickMethod::ButtonAreas => ClickMethod::ButtonAreas,
            CfgClickMethod::Clickfinger => ClickMethod::Clickfinger,
        };
        log("click_method", device.config_click_set_method(method));
    }
    if let Some(speed) = cfg.accel_speed {
        // libinput rejects anything outside [-1.0, 1.0]; clamp rather than let
        // a typo silently no-op the whole setting.
        log(
            "accel_speed",
            device.config_accel_set_speed(speed.clamp(-1.0, 1.0)),
        );
    }
    if let Some(profile) = cfg.accel_profile {
        let profile = match profile {
            CfgAccelProfile::Adaptive => AccelProfile::Adaptive,
            CfgAccelProfile::Flat => AccelProfile::Flat,
        };
        log("accel_profile", device.config_accel_set_profile(profile));
    }
    if let Some(enabled) = cfg.disable_while_typing {
        log(
            "disable_while_typing",
            device.config_dwt_set_enabled(enabled),
        );
    }
    if let Some(enabled) = cfg.left_handed {
        log("left_handed", device.config_left_handed_set(enabled));
    }
    if let Some(enabled) = cfg.middle_emulation {
        log(
            "middle_emulation",
            device.config_middle_emulation_set_enabled(enabled),
        );
    }
}
