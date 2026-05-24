//! Window-action layout: tile-left/right, maximize, minimize, close.
//!
//! Per-window state lives in [`LayoutManager`]'s side-table keyed by `Window`.
//! Toggling the active tile/maximize state restores the window's saved
//! pre-tile rect; minimized windows are unmapped from the space and kept on a
//! LIFO stack so `Unminimize` can restore them in reverse order.

use std::collections::HashMap;

use smithay::{
    desktop::{layer_map_for_output, Space, Window},
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Rectangle},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutState {
    Floating,
    TiledLeft,
    TiledRight,
    Maximized,
}

#[derive(Clone, Debug)]
pub struct WindowMeta {
    pub layout: LayoutState,
    /// Pre-tile rect, captured on Floating → Tiled/Maximized so a re-press of
    /// the same action restores it.
    pub saved_rect: Option<Rectangle<i32, Logical>>,
}

impl Default for WindowMeta {
    fn default() -> Self {
        Self {
            layout: LayoutState::Floating,
            saved_rect: None,
        }
    }
}

#[derive(Default)]
pub struct LayoutManager {
    meta: HashMap<Window, WindowMeta>,
    /// LIFO of (window, rect-at-minimize-time). Popped by `Unminimize`.
    minimized: Vec<(Window, Rectangle<i32, Logical>)>,
}

impl LayoutManager {
    fn entry(&mut self, w: &Window) -> &mut WindowMeta {
        self.meta.entry(w.clone()).or_default()
    }

    pub fn forget(&mut self, w: &Window) {
        self.meta.remove(w);
        self.minimized.retain(|(mw, _)| mw != w);
    }

    pub fn push_minimized(&mut self, w: Window, rect: Rectangle<i32, Logical>) {
        self.minimized.push((w, rect));
    }

    pub fn is_minimized(&self, w: &Window) -> bool {
        self.minimized.iter().any(|(mw, _)| mw == w)
    }

    /// Pop the next still-alive minimized window. Drops any dead entries
    /// encountered on the way (the client exited while minimized).
    pub fn pop_live_minimized(&mut self) -> Option<(Window, Rectangle<i32, Logical>)> {
        let depth = self.minimized.len();
        while let Some((w, r)) = self.minimized.pop() {
            if w.alive() {
                return Some((w, r));
            }
            tracing::warn!("discarding dead window from minimized stack");
        }
        tracing::debug!(depth, "pop_live_minimized: nothing live found");
        None
    }
}

/// Output usable rect for the monitor under the window's center, with any
/// layer-shell exclusive zones (bars, docks) subtracted. Falls back to the
/// first known output if the window sits outside every output.
pub fn usable_rect_for(space: &Space<Window>, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let center = space.element_location(window).map(|loc| {
        let geo = window.geometry();
        (loc.x + geo.size.w / 2, loc.y + geo.size.h / 2)
    });

    let output = center
        .and_then(|(cx, cy)| {
            space.outputs().find(|o| {
                space
                    .output_geometry(o)
                    .map(|geo| {
                        cx >= geo.loc.x
                            && cx < geo.loc.x + geo.size.w
                            && cy >= geo.loc.y
                            && cy < geo.loc.y + geo.size.h
                    })
                    .unwrap_or(false)
            })
        })
        .or_else(|| space.outputs().next())?;

    let geo = space.output_geometry(output)?;
    let zone = layer_map_for_output(output).non_exclusive_zone();
    // LayerMap's zone is in output-local coords; shift into space coords.
    Some(Rectangle::new(geo.loc + zone.loc, zone.size))
}

/// Apply a geometry change to `window`: send the xdg configure with the new
/// size + maximized state, then move it in the space. The client will resize
/// on its next commit; we re-map immediately so the location is correct for
/// the next render even before the client has acked.
pub fn apply_geometry(
    space: &mut Space<Window>,
    window: &Window,
    rect: Rectangle<i32, Logical>,
    maximized: bool,
) {
    let xdg = window.toplevel().unwrap();
    xdg.with_pending_state(|s| {
        s.size = Some(rect.size);
        if maximized {
            s.states.set(xdg_toplevel::State::Maximized);
        } else {
            s.states.unset(xdg_toplevel::State::Maximized);
        }
    });
    xdg.send_pending_configure();
    space.map_element(window.clone(), rect.loc, false);
}

/// Transition `window` toward `target`. If it's already in `target`, restore
/// the saved floating rect instead (the toggle behavior the Openbox bindings
/// have always had).
pub fn set_layout(
    space: &mut Space<Window>,
    layout: &mut LayoutManager,
    window: &Window,
    target: LayoutState,
) {
    let Some(usable) = usable_rect_for(space, window) else {
        return;
    };
    let current_loc = space.element_location(window).unwrap_or_default();
    let current_size = window.geometry().size;
    let current_rect = Rectangle::new(current_loc, current_size);

    let meta = layout.entry(window);

    if meta.layout == target {
        // Toggle off → restore saved floating rect (or stay put if we never had one).
        let restore = meta.saved_rect.take().unwrap_or(current_rect);
        meta.layout = LayoutState::Floating;
        apply_geometry(space, window, restore, false);
        return;
    }

    if meta.layout == LayoutState::Floating {
        meta.saved_rect = Some(current_rect);
    }
    meta.layout = target;

    let half_w = usable.size.w / 2;
    let new_rect = match target {
        LayoutState::TiledLeft => Rectangle::new(usable.loc, (half_w, usable.size.h).into()),
        LayoutState::TiledRight => Rectangle::new(
            (usable.loc.x + half_w, usable.loc.y).into(),
            (usable.size.w - half_w, usable.size.h).into(),
        ),
        LayoutState::Maximized => usable,
        LayoutState::Floating => return,
    };
    tracing::debug!(?target, ?usable, ?new_rect, "tiling window");
    apply_geometry(space, window, new_rect, target == LayoutState::Maximized);
}
