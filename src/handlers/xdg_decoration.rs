//! `xdg-decoration-unstable-v1`: every toplevel is forced into
//! `ServerSide`, and the WM draws nothing. Borderless tiled look; no
//! wasted titlebar pixels. Close / maximize / minimize / workspace
//! navigation are all keybind-driven and the title is exposed via the
//! bar + menu, so there's nothing left for a titlebar to carry.
//!
//! Client `set_mode(ClientSide)` requests are deliberately ignored —
//! letting clients flip to CSD at runtime triggers buggy
//! buffer-allocation paths in several real-world clients (alacritty
//! 0.16 calls `wl_shm_pool.create_buffer(0, 0)` mid-switch, which the
//! WM correctly rejects as a protocol error and the client dies).
//! Locking the mode avoids the whole class of problem.

use smithay::{
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    wayland::shell::xdg::{decoration::XdgDecorationHandler, ToplevelSurface},
};

use crate::state::ShoestringWm;

fn force_server_side(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(Mode::ServerSide);
    });
    if toplevel.is_initial_configure_sent() {
        toplevel.send_pending_configure();
    }
}

impl XdgDecorationHandler for ShoestringWm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        force_server_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        force_server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        force_server_side(&toplevel);
    }
}
