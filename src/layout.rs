//! Window-action layout: tile-left/right, maximize, minimize, close.
//!
//! Per-window state lives in [`LayoutManager`]'s side-table keyed by `Window`.
//! Toggling the active tile/maximize state restores the window's saved
//! pre-tile rect; minimized windows are unmapped from the space and kept on a
//! LIFO stack so `Unminimize` can restore them in reverse order.

use std::collections::HashMap;

use smithay::{
    desktop::{layer_map_for_output, Space, Window},
    output::Output,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Point, Rectangle},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutState {
    Floating,
    TiledLeft,
    TiledRight,
    Maximized,
    /// App-driven fullscreen (xdg `set_fullscreen` / X11 / foreign-toplevel).
    /// Covers the whole output edge-to-edge, ignoring layer-shell exclusive
    /// zones — unlike [`Maximized`], which respects bars/docks. Entered via
    /// [`set_fullscreen`], never the [`set_layout`] toggle path.
    Fullscreen,
}

#[derive(Clone, Debug)]
pub struct WindowMeta {
    pub layout: LayoutState,
    /// Pre-tile rect, captured on Floating → Tiled/Maximized so a re-press of
    /// the same action restores it.
    pub saved_rect: Option<Rectangle<i32, Logical>>,
    /// Layout to return to when fullscreen is unset, captured on entering
    /// [`LayoutState::Fullscreen`]. Lets a maximized window that went
    /// fullscreen fall back to maximized (not floating) on `unset_fullscreen`.
    pub pre_fullscreen: Option<LayoutState>,
}

impl Default for WindowMeta {
    fn default() -> Self {
        Self {
            layout: LayoutState::Floating,
            saved_rect: None,
            pre_fullscreen: None,
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

    /// The window's current tile/maximize state (`Floating` if untracked).
    /// Used by the foreign-toplevel-management protocol to report and toggle
    /// the maximized bit without going through the focused-window path.
    pub fn layout_state(&self, w: &Window) -> LayoutState {
        self.meta
            .get(w)
            .map(|m| m.layout)
            .unwrap_or(LayoutState::Floating)
    }

    /// Remove `w` from the minimized stack (if present) and return the
    /// rect it was minimized at. Used when a specific window is being
    /// restored by name (e.g. a bar click on its entry) rather than by
    /// LIFO order.
    pub fn take_minimized(&mut self, w: &Window) -> Option<Rectangle<i32, Logical>> {
        let pos = self.minimized.iter().position(|(mw, _)| mw == w)?;
        Some(self.minimized.remove(pos).1)
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

/// True if `point` lies within `rect`, half-open on the right and bottom edges:
/// those edges belong to the *next* output. This is what stops a point on the
/// shared seam between two side-by-side outputs from matching both.
fn rect_contains(rect: Rectangle<i32, Logical>, point: Point<i32, Logical>) -> bool {
    point.x >= rect.loc.x
        && point.x < rect.loc.x + rect.size.w
        && point.y >= rect.loc.y
        && point.y < rect.loc.y + rect.size.h
}

/// The output under the window's center, falling back to the first known
/// output if the window sits outside every output.
fn output_under<'a>(space: &'a Space<Window>, window: &Window) -> Option<&'a Output> {
    let center = space.element_location(window).map(|loc| {
        let geo = window.geometry();
        Point::from((loc.x + geo.size.w / 2, loc.y + geo.size.h / 2))
    });

    center
        .and_then(|center| {
            space.outputs().find(|o| {
                space
                    .output_geometry(o)
                    .map(|geo| rect_contains(geo, center))
                    .unwrap_or(false)
            })
        })
        .or_else(|| space.outputs().next())
}

/// Output usable rect for the monitor under the window's center, with any
/// layer-shell exclusive zones (bars, docks) subtracted. Used by the
/// tile/maximize layouts, which respect bars.
pub fn usable_rect_for(space: &Space<Window>, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let output = output_under(space, window)?;
    let geo = space.output_geometry(output)?;
    let zone = layer_map_for_output(output).non_exclusive_zone();
    // LayerMap's zone is in output-local coords; shift into space coords.
    Some(Rectangle::new(geo.loc + zone.loc, zone.size))
}

/// Full output rect (edge-to-edge) for the monitor under the window's center,
/// ignoring layer-shell exclusive zones. Used by [`LayoutState::Fullscreen`],
/// which covers the whole output unlike [`usable_rect_for`].
pub fn output_rect_for(space: &Space<Window>, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let output = output_under(space, window)?;
    space.output_geometry(output)
}

/// Current rect of `window` in the space (location + geometry size).
fn current_rect(space: &Space<Window>, window: &Window) -> Rectangle<i32, Logical> {
    let loc = space.element_location(window).unwrap_or_default();
    Rectangle::new(loc, window.geometry().size)
}

/// Target rect for a tile/maximize state within `usable`. Returns `None` for
/// `Floating` and `Fullscreen`, which don't derive from the usable rect.
fn tiled_rect(
    usable: Rectangle<i32, Logical>,
    target: LayoutState,
) -> Option<Rectangle<i32, Logical>> {
    let half_w = usable.size.w / 2;
    Some(match target {
        LayoutState::TiledLeft => Rectangle::new(usable.loc, (half_w, usable.size.h).into()),
        LayoutState::TiledRight => Rectangle::new(
            (usable.loc.x + half_w, usable.loc.y).into(),
            (usable.size.w - half_w, usable.size.h).into(),
        ),
        LayoutState::Maximized => usable,
        LayoutState::Floating | LayoutState::Fullscreen => return None,
    })
}

/// Apply a geometry change to `window`: send the xdg configure with the new
/// size + the maximized/fullscreen state bits for `state`, then move it in the
/// space. The client will resize on its next commit; we re-map immediately so
/// the location is correct for the next render even before the client has acked.
pub fn apply_geometry(
    space: &mut Space<Window>,
    window: &Window,
    rect: Rectangle<i32, Logical>,
    state: LayoutState,
) {
    use xdg_toplevel::State as St;
    let maximized = state == LayoutState::Maximized;
    let fullscreen = state == LayoutState::Fullscreen;
    if let Some(xdg) = window.toplevel() {
        xdg.with_pending_state(|s| {
            s.size = Some(rect.size);
            // Maximized and Fullscreen are mutually exclusive here; always set
            // exactly the matching bit and clear the other so a transition
            // (e.g. maximized → fullscreen) doesn't leave both set.
            if maximized {
                s.states.set(St::Maximized);
            } else {
                s.states.unset(St::Maximized);
            }
            if fullscreen {
                s.states.set(St::Fullscreen);
            } else {
                s.states.unset(St::Fullscreen);
            }
        });
        xdg.send_pending_configure();
    } else if let Some(x11) = window.x11_surface() {
        // X11: configure synchronously with the new geometry. There's no
        // separate state in the xdg sense — set_maximized/set_fullscreen mark
        // it in the X-side property table so apps can react.
        let _ = x11.set_maximized(maximized);
        let _ = x11.set_fullscreen(fullscreen);
        let _ = x11.configure(rect);
    }
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
    // The toggle path drives the tile/maximize actions only. Floating is the
    // rest state and Fullscreen is app-driven via `set_fullscreen` (no toggle).
    debug_assert!(
        matches!(
            target,
            LayoutState::TiledLeft | LayoutState::TiledRight | LayoutState::Maximized
        ),
        "set_layout target must be a tile/maximize state"
    );
    let Some(usable) = usable_rect_for(space, window) else {
        return;
    };
    let current = current_rect(space, window);

    let meta = layout.entry(window);

    if meta.layout == target {
        // Toggle off → restore saved floating rect (or stay put if we never had one).
        let restore = meta.saved_rect.take().unwrap_or(current);
        meta.layout = LayoutState::Floating;
        apply_geometry(space, window, restore, LayoutState::Floating);
        return;
    }

    if meta.layout == LayoutState::Floating {
        meta.saved_rect = Some(current);
    }
    meta.layout = target;

    let Some(new_rect) = tiled_rect(usable, target) else {
        return;
    };
    tracing::debug!(?target, ?usable, ?new_rect, "tiling window");
    apply_geometry(space, window, new_rect, target);
}

/// Put `window` into fullscreen covering `output_geo` (the full output rect).
/// Idempotent: re-fullscreening an already-fullscreen window just re-applies
/// the geometry rather than toggling back out (the xdg/X11/foreign-toplevel
/// `set_fullscreen` requests are explicit, not toggles). Remembers the prior
/// layout so [`unset_fullscreen`] can return to it.
pub fn set_fullscreen(
    space: &mut Space<Window>,
    layout: &mut LayoutManager,
    window: &Window,
    output_geo: Rectangle<i32, Logical>,
) {
    let current = current_rect(space, window);
    let meta = layout.entry(window);
    if meta.layout != LayoutState::Fullscreen {
        meta.pre_fullscreen = Some(meta.layout);
        // Preserve the floating rect to restore later, exactly as the tile
        // path does on its first Floating → non-Floating transition.
        if meta.layout == LayoutState::Floating {
            meta.saved_rect = Some(current);
        }
        meta.layout = LayoutState::Fullscreen;
    }
    tracing::debug!(?output_geo, "fullscreening window");
    apply_geometry(space, window, output_geo, LayoutState::Fullscreen);
}

/// The window currently fullscreen on `output`, if any. The render path uses
/// this to drop the layer-shell surfaces (bars/docks) and other windows that a
/// fullscreen surface covers edge-to-edge, so they don't paint over it.
pub fn fullscreen_window_on(
    space: &Space<Window>,
    layout: &LayoutManager,
    output: &Output,
) -> Option<Window> {
    space
        .elements()
        .find(|w| {
            layout.layout_state(w) == LayoutState::Fullscreen
                && space.outputs_for_element(w).iter().any(|o| o == output)
        })
        .cloned()
}

/// Leave fullscreen, returning to the layout captured on entry (floating,
/// tiled, or maximized). No-op if `window` is not currently fullscreen.
pub fn unset_fullscreen(space: &mut Space<Window>, layout: &mut LayoutManager, window: &Window) {
    if layout.layout_state(window) != LayoutState::Fullscreen {
        return;
    }
    let Some(usable) = usable_rect_for(space, window) else {
        return;
    };
    let current = current_rect(space, window);
    let meta = layout.entry(window);
    let prev = meta.pre_fullscreen.take().unwrap_or(LayoutState::Floating);
    meta.layout = prev;
    match tiled_rect(usable, prev) {
        // Returned to a tile/maximize state — recompute its rect.
        Some(rect) => apply_geometry(space, window, rect, prev),
        // Floating (or unexpected): restore the saved floating rect.
        None => {
            let restore = meta.saved_rect.take().unwrap_or(current);
            meta.layout = LayoutState::Floating;
            apply_geometry(space, window, restore, LayoutState::Floating);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    prop_compose! {
        /// A usable rect with non-negative, bounded sizes and a bounded origin,
        /// so `loc + size` sums can't overflow `i32`. Covers the realistic
        /// range of output/usable geometry (including degenerate 0-sized rects).
        fn any_usable()(
            x in -1_000_000i32..1_000_000,
            y in -1_000_000i32..1_000_000,
            w in 0i32..1_000_000,
            h in 0i32..1_000_000,
        ) -> Rectangle<i32, Logical> {
            Rectangle::new((x, y).into(), (w, h).into())
        }
    }

    proptest! {
        /// TiledLeft and TiledRight must split the usable rect cleanly: same
        /// height/top, contiguous at the seam (no gap, no overlap), and together
        /// covering the full width edge-to-edge.
        #[test]
        fn tiled_left_right_partition_usable(usable in any_usable()) {
            let left = tiled_rect(usable, LayoutState::TiledLeft).unwrap();
            let right = tiled_rect(usable, LayoutState::TiledRight).unwrap();

            // Both halves span the usable's full height, anchored at its top.
            prop_assert_eq!(left.loc.y, usable.loc.y);
            prop_assert_eq!(right.loc.y, usable.loc.y);
            prop_assert_eq!(left.size.h, usable.size.h);
            prop_assert_eq!(right.size.h, usable.size.h);

            // Left starts at the usable's left edge.
            prop_assert_eq!(left.loc.x, usable.loc.x);
            // Contiguous: left's right edge is exactly right's left edge.
            prop_assert_eq!(left.loc.x + left.size.w, right.loc.x);
            // Right ends exactly at the usable's right edge.
            prop_assert_eq!(right.loc.x + right.size.w, usable.loc.x + usable.size.w);
            // Together they cover the whole width — no pixels lost or doubled.
            prop_assert_eq!(left.size.w + right.size.w, usable.size.w);

            // Neither half is negative-width.
            prop_assert!(left.size.w >= 0);
            prop_assert!(right.size.w >= 0);

            // The split is even to within 1px, with the extra column going to
            // the right half on odd widths (the documented tile-half behavior).
            let diff = right.size.w - left.size.w;
            prop_assert!(diff == 0 || diff == 1);
        }

        /// Maximized fills the usable rect exactly.
        #[test]
        fn maximized_equals_usable(usable in any_usable()) {
            prop_assert_eq!(tiled_rect(usable, LayoutState::Maximized).unwrap(), usable);
        }

        /// Floating and Fullscreen don't derive from the usable rect.
        #[test]
        fn floating_and_fullscreen_have_no_tiled_rect(usable in any_usable()) {
            prop_assert!(tiled_rect(usable, LayoutState::Floating).is_none());
            prop_assert!(tiled_rect(usable, LayoutState::Fullscreen).is_none());
        }

        /// Every tile/maximize target stays inside the usable bounds.
        #[test]
        fn tiled_rects_stay_within_usable(usable in any_usable()) {
            for target in [
                LayoutState::TiledLeft,
                LayoutState::TiledRight,
                LayoutState::Maximized,
            ] {
                let r = tiled_rect(usable, target).unwrap();
                prop_assert!(r.loc.x >= usable.loc.x);
                prop_assert!(r.loc.y >= usable.loc.y);
                prop_assert!(r.loc.x + r.size.w <= usable.loc.x + usable.size.w);
                prop_assert!(r.loc.y + r.size.h <= usable.loc.y + usable.size.h);
            }
        }

        /// Multi-output placement: side-by-side outputs that tile a region must
        /// contain every interior point in exactly one output — `rect_contains`
        /// being half-open means a point on a shared seam isn't claimed twice or
        /// dropped (which would send `output_under` to the wrong monitor).
        #[test]
        fn tiled_outputs_contain_each_point_exactly_once(
            widths in proptest::collection::vec(1i32..3000, 1..6),
            h in 1i32..3000,
            px_num in 0u32..100_000,
            py_num in 0u32..100_000,
        ) {
            let mut rects = Vec::new();
            let mut x = 0i32;
            for w in &widths {
                rects.push(Rectangle::new((x, 0).into(), (*w, h).into()));
                x += *w;
            }
            let total_w = x;
            // Map the random numbers into the tiled span [0, total_w) x [0, h).
            let point: Point<i32, Logical> =
                ((px_num as i32) % total_w, (py_num as i32) % h).into();

            let count = rects.iter().filter(|r| rect_contains(**r, point)).count();
            prop_assert_eq!(count, 1);
        }

        /// A point outside every output's span is contained by none (so
        /// `output_under` falls back to the first output rather than mis-placing).
        #[test]
        fn point_outside_all_outputs_matches_none(
            widths in proptest::collection::vec(1i32..3000, 1..6),
            h in 1i32..3000,
        ) {
            let mut rects = Vec::new();
            let mut x = 0i32;
            for w in &widths {
                rects.push(Rectangle::new((x, 0).into(), (*w, h).into()));
                x += *w;
            }
            let total_w = x;
            // Just past the right edge of the last output.
            let point: Point<i32, Logical> = (total_w, 0).into();
            prop_assert!(rects.iter().all(|r| !rect_contains(*r, point)));
        }
    }
}
