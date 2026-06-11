//! Apply the `[input]` config section ([`shoestring_config::Input`]) to
//! libinput devices.
//!
//! Settings are pushed to each device when it connects (`DeviceAdded` in the
//! udev backend) and re-pushed to every connected device on config hot-reload.
//!
//! The section is **declarative**: each knob is always set to either the
//! configured value or — when the key is absent — the device's own libinput
//! default. So removing a key and reloading resets that knob rather than
//! leaving the last value applied, and the live device state always matches
//! the current config. A knob the hardware doesn't support is skipped (its
//! default getter returns `None`) or no-ops, so one global section is safe to
//! apply blindly to every device (the wired mouse ignores tap-to-click, the
//! touchpad applies it). udev/TTY backend only — the nested winit backend has
//! no real libinput devices.

use shoestring_config::{
    AccelProfile as CfgAccelProfile, ClickMethod as CfgClickMethod, Input,
    ScrollMethod as CfgScrollMethod, TapButtonMap as CfgTapButtonMap,
};
use smithay::reexports::input::{
    self as libinput, AccelProfile, ClickMethod, DeviceConfigError, DragLockState, ScrollMethod,
    TapButtonMap,
};

/// Apply `cfg` to `device`, setting every knob to the configured value or the
/// device's libinput default when unset.
pub fn apply(device: &mut libinput::Device, cfg: &Input) {
    let name = device.name().to_string();
    // `Unsupported` just means the device lacks that knob — expected when a
    // global section meets mixed hardware, so swallow it. Anything else (e.g.
    // `Invalid`) is worth a debug line.
    let log = |setting: &str, r: libinput::DeviceConfigResult| match r {
        Ok(()) | Err(DeviceConfigError::Unsupported) => {}
        Err(e) => tracing::debug!(device = %name, setting, error = ?e, "input: setting rejected"),
    };

    // Bool / scalar knobs: the default getter always yields a value.
    let tap = cfg
        .tap_to_click
        .unwrap_or_else(|| device.config_tap_default_enabled());
    log("tap_to_click", device.config_tap_set_enabled(tap));

    let drag = cfg
        .tap_and_drag
        .unwrap_or_else(|| device.config_tap_default_drag_enabled());
    log("tap_and_drag", device.config_tap_set_drag_enabled(drag));

    let drag_lock = match cfg.drag_lock {
        Some(true) => DragLockState::EnabledTimeout,
        Some(false) => DragLockState::Disabled,
        None => device.config_tap_default_drag_lock_enabled(),
    };
    log(
        "drag_lock",
        device.config_tap_set_drag_lock_enabled(drag_lock),
    );

    let natural = cfg
        .natural_scroll
        .unwrap_or_else(|| device.config_scroll_default_natural_scroll_enabled());
    log(
        "natural_scroll",
        device.config_scroll_set_natural_scroll_enabled(natural),
    );

    let scroll_button = cfg
        .scroll_button
        .unwrap_or_else(|| device.config_scroll_default_button());
    log(
        "scroll_button",
        device.config_scroll_set_button(scroll_button),
    );

    let accel_speed = cfg
        .accel_speed
        .map(|s| s.clamp(-1.0, 1.0))
        .unwrap_or_else(|| device.config_accel_default_speed());
    log("accel_speed", device.config_accel_set_speed(accel_speed));

    let dwt = cfg
        .disable_while_typing
        .unwrap_or_else(|| device.config_dwt_default_enabled());
    log("disable_while_typing", device.config_dwt_set_enabled(dwt));

    let left_handed = cfg
        .left_handed
        .unwrap_or_else(|| device.config_left_handed_default());
    log("left_handed", device.config_left_handed_set(left_handed));

    let middle = cfg
        .middle_emulation
        .unwrap_or_else(|| device.config_middle_emulation_default_enabled());
    log(
        "middle_emulation",
        device.config_middle_emulation_set_enabled(middle),
    );

    // Enum knobs: the default getter returns `None` when the device can't be
    // configured for it, which is exactly when we should skip.
    let tap_map = match cfg.tap_button_map {
        Some(CfgTapButtonMap::LeftRightMiddle) => Some(TapButtonMap::LeftRightMiddle),
        Some(CfgTapButtonMap::LeftMiddleRight) => Some(TapButtonMap::LeftMiddleRight),
        None => device.config_tap_default_button_map(),
    };
    if let Some(map) = tap_map {
        log("tap_button_map", device.config_tap_set_button_map(map));
    }

    let scroll_method = match cfg.scroll_method {
        Some(CfgScrollMethod::None) => Some(ScrollMethod::NoScroll),
        Some(CfgScrollMethod::TwoFinger) => Some(ScrollMethod::TwoFinger),
        Some(CfgScrollMethod::Edge) => Some(ScrollMethod::Edge),
        Some(CfgScrollMethod::OnButtonDown) => Some(ScrollMethod::OnButtonDown),
        None => device.config_scroll_default_method(),
    };
    if let Some(method) = scroll_method {
        log("scroll_method", device.config_scroll_set_method(method));
    }

    let click_method = match cfg.click_method {
        Some(CfgClickMethod::ButtonAreas) => Some(ClickMethod::ButtonAreas),
        Some(CfgClickMethod::Clickfinger) => Some(ClickMethod::Clickfinger),
        None => device.config_click_default_method(),
    };
    if let Some(method) = click_method {
        log("click_method", device.config_click_set_method(method));
    }

    let accel_profile = match cfg.accel_profile {
        Some(CfgAccelProfile::Adaptive) => Some(AccelProfile::Adaptive),
        Some(CfgAccelProfile::Flat) => Some(AccelProfile::Flat),
        None => device.config_accel_default_profile(),
    };
    if let Some(profile) = accel_profile {
        log("accel_profile", device.config_accel_set_profile(profile));
    }
}
