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
mod config_watcher;
mod confirm;
mod cursor;
pub mod decorations;
pub mod diag_overlay;
pub mod drawing;
mod ext_screencopy;
mod ext_workspace;
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
mod remote_command;
mod remote_screenshot;
mod scale;
mod screencopy;
pub mod state;
mod virtual_pointer;
pub mod wallpaper;
pub mod window_ext;
mod window_rules;
mod workspace;
mod xwayland;

/// Import the named environment variables into the systemd user manager via
/// `systemctl --user import-environment`, so D-Bus-activated services (portals,
/// notification daemons, …) inherit them. A no-op-with-warning when systemctl
/// is absent — non-systemd sessions stay first-class. Called from `main` (for
/// `WAYLAND_DISPLAY`) and from [`xwayland`] (for `DISPLAY`, once Xwayland is up).
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
}
