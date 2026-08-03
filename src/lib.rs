//! Library crate for shoestring-wm.
//!
//! The compositor's modules live here so two consumers can share them: the
//! `shoestring-wm` binary ([`crate`]'s `src/main.rs`) and the out-of-tree WLCS
//! conformance harness (`crates/wlcs-shoestring`, a cdylib that constructs
//! [`state::ShoestringWm`] headless and drives it as a Wayland server under the
//! test suite). Only the items those consumers reach are `pub`; everything else
//! is crate-internal.
#![allow(irrefutable_let_patterns)]

pub mod backend;
pub mod binds;
pub mod capture_stream;
mod clipboard;
mod config_watcher;
mod confirm;
mod cursor;
pub mod decorations;
pub mod diag_overlay;
pub mod drawing;
mod ext_screencopy;
mod ext_workspace;
pub mod focus;
mod foreign_toplevel_mgmt;
#[cfg(feature = "tty")]
mod gamma_control;
mod grabs;
mod handlers;
mod inject;
mod input;
// libinput device tuning — only the udev/TTY backend has real input devices.
#[cfg(feature = "tty")]
mod input_config;
mod ipc;
mod layout;
pub mod metrics;
mod output_management;
mod picker;
mod power;
pub mod presentation;
pub mod profiling;
mod remote;
mod remote_command;
mod remote_screenshot;
mod scale;
mod screencopy;
pub mod state;
mod virtual_pointer;
pub mod wallpaper;
pub mod window_capture;
pub mod window_ext;
mod window_rules;
mod workspace;
mod xwayland;

/// Parse a `WIDTHxHEIGHT` size spec (e.g. `1360x856`, case-insensitive `x`),
/// returning `Some((w, h))` only for two strictly-positive integers. Shared by
/// the `--output-size` CLI validation in `main` and the headless backend's
/// virtual-output sizing so the two never disagree on what a valid spec is.
pub fn parse_output_size(spec: &str) -> Option<(i32, i32)> {
    let (w, h) = spec.split_once(['x', 'X'])?;
    let w: i32 = w.trim().parse().ok()?;
    let h: i32 = h.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Import the named environment variables into the systemd user manager via
/// `systemctl --user import-environment`, so `systemctl --user` units inherit
/// them, **and** into the D-Bus session daemon's own activation-environment
/// cache via `dbus-update-activation-environment --systemd`. The two are
/// independent stores: portal/notification-daemon `.service` files are
/// plain `Exec=` activations (no `SystemdService=`), so D-Bus spawns them
/// from its own cache, not systemd's manager environment — importing into
/// only one leaves the other activation path (and anything it spawns, e.g.
/// `xdg-desktop-portal-shoestring`) without `WAYLAND_DISPLAY`/`DISPLAY` for
/// the lifetime of that D-Bus-activated process. A no-op-with-warning when
/// either tool is absent — non-systemd sessions stay first-class. Called
/// from `main` (for `WAYLAND_DISPLAY`) and from [`xwayland`] (for `DISPLAY`,
/// once Xwayland is up).
pub fn import_systemd_env(vars: &[&str]) {
    match std::process::Command::new("systemctl")
        .arg("--user")
        .arg("import-environment")
        .args(vars)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(status = %s, "systemctl import-environment exited non-zero"),
        Err(e) => tracing::warn!(error = %e, "systemctl import-environment failed"),
    }

    match std::process::Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(vars)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => {
            tracing::warn!(status = %s, "dbus-update-activation-environment exited non-zero")
        }
        Err(e) => tracing::warn!(error = %e, "dbus-update-activation-environment failed"),
    }
}
