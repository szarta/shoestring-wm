//! wlr-layer-shell handling. Lets bars/panels (waybar, yambar, ...) attach
//! to an output, reserve exclusive screen space, and stay z-ordered above
//! or below tiled windows according to their `Layer`.
//!
//! Smithay does the heavy lifting:
//! - [`LayerMap`] (per-output, via [`layer_map_for_output`]) tracks mapped
//!   layer surfaces, computes per-output [`non_exclusive_zone`], and is what
//!   `space_render_elements` walks to draw bars in the right z-order.
//! - [`LayerSurface`]'s `arrange` recomputes positions when geometry changes;
//!   we just have to call it after `map_layer` and on commit.
//!
//! [`LayerMap`]: smithay::desktop::LayerMap
//! [`layer_map_for_output`]: smithay::desktop::layer_map_for_output
//! [`non_exclusive_zone`]: smithay::desktop::LayerMap::non_exclusive_zone
//! [`LayerSurface`]: smithay::desktop::LayerSurface

use smithay::{
    desktop::{layer_map_for_output, LayerSurface, WindowSurfaceType},
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
                WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use crate::state::ShoestringWm;

impl WlrLayerShellHandler for ShoestringWm {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // Client may name a specific output; otherwise we just stick the
        // layer on whichever output happens to be first. M9 might honor
        // a config-driven preference here (e.g. bar always on primary).
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            tracing::warn!(
                %namespace,
                "no outputs available — rejecting layer surface",
            );
            return;
        };
        let layer = LayerSurface::new(surface, namespace);
        {
            let mut map = layer_map_for_output(&output);
            if let Err(e) = map.map_layer(&layer) {
                tracing::warn!(error = ?e, "map_layer failed");
            }
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        let _ = self
            .popups
            .track_popup(smithay::desktop::PopupKind::Xdg(popup));
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // Was this the focused surface? If so, after unmapping we need to
        // hand focus back to the previously-focused window on the active
        // workspace (otherwise the keyboard goes nowhere and the user
        // can't type into anything).
        let had_focus = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .and_then(|f| f.surface())
            .as_ref()
            == Some(surface.wl_surface());

        if let Some((mut map, layer)) = self.space.outputs().find_map(|o| {
            let map = layer_map_for_output(o);
            let layer = map
                .layers()
                .find(|&l| l.layer_surface() == &surface)
                .cloned();
            layer.map(|l| (map, l))
        }) {
            map.unmap_layer(&layer);
        }

        if had_focus {
            let active = self.workspaces.active();
            let pick = loop {
                let Some(w) = self.workspaces.last_focused(active) else {
                    break None;
                };
                if self.space.elements().any(|el| el == &w) {
                    break Some(w);
                }
                self.workspaces.discard_top_focus(active);
            };
            match pick {
                Some(w) => self.focus_window(&w),
                None => self.clear_focus(),
            }
        }
    }
}

/// Layer-surface commit handler. Mirrors the xdg-side `handle_commit`: send
/// the initial configure once the client has stamped a size/anchors, and
/// re-`arrange` on any subsequent state change so exclusive zones stay in sync.
pub fn handle_commit(state: &mut ShoestringWm, surface: &WlSurface) -> bool {
    // Find the output owning this layer surface (if any).
    let Some(output) = state
        .space
        .outputs()
        .find(|o| {
            layer_map_for_output(o)
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .is_some()
        })
        .cloned()
    else {
        return false;
    };

    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .map(|d| d.lock().unwrap().initial_configure_sent)
            .unwrap_or(false)
    });

    let mut map = layer_map_for_output(&output);
    // Arrange first so the initial configure reflects exclusive-zone math.
    map.arrange();
    let interactivity = map
        .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
        .map(|layer| {
            layer
                .layer_surface()
                .with_cached_state(|s| s.keyboard_interactivity)
        })
        .unwrap_or(KeyboardInteractivity::None);
    if !initial_configure_sent {
        if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
            layer.layer_surface().send_configure();
        }
    }
    drop(map);

    // Transfer keyboard focus to layers that request it (e.g.
    // shoestring-menu's input strip). `layer_destroyed` restores focus
    // to the previously-focused window when the layer goes away. Skip
    // if focus is already on this surface so commits during typing
    // don't churn the focus serial.
    // ...but never while locked: the lock surface owns keyboard focus
    // then, and an exclusive layer surface (e.g. a menu/notification)
    // committing after the lock must not steal it out from under the
    // locker's password prompt.
    if interactivity == KeyboardInteractivity::Exclusive && !state.is_locked() {
        // Layer surfaces are always Wayland.
        let target = crate::focus::KeyboardFocusTarget::Wayland(surface.clone());
        if let Some(kb) = state.seat.get_keyboard() {
            let already = kb.current_focus().as_ref() == Some(&target);
            if !already {
                let serial = SERIAL_COUNTER.next_serial();
                kb.set_focus(state, Some(target), serial);
            }
        }
    }
    true
}
