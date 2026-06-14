//! Apply `[[window_rules]]` to freshly-committed toplevels.
//!
//! Rules fire exactly once per window — on the first commit handler
//! call after `new_toplevel`. By then most well-behaved clients have
//! already set `app_id` / `title`, which is what makes matching
//! useful. Clients that delay those fields will miss any rule that
//! depends on them; if a real user hits that, we can extend the
//! evaluation window to "until initial_configure_sent" later. The
//! one-shot rule keeps the data flow predictable: no risk of a rule
//! firing again when an app updates its title (e.g. terminal tabs).

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Size},
    wayland::{compositor::with_states, shell::xdg::XdgToplevelSurfaceData},
};

use crate::state::ShoestringWm;

impl ShoestringWm {
    /// Match this window against `config.window_rules` and apply the
    /// first matching rule's actions. Records the window as
    /// rule-evaluated so subsequent commits do nothing.
    pub fn try_apply_window_rules(&mut self, window: &Window) {
        if self.rules_applied.contains(window) {
            return;
        }
        // Mark applied before we run actions so a re-entrant commit
        // (some Smithay code paths trigger one during configure) cannot
        // double-fire the same rule.
        self.rules_applied.insert(window.clone());

        if self.config.window_rules.is_empty() {
            return;
        }
        let (app_id, title) =
            if let Some(surface) = window.toplevel().map(|t| t.wl_surface().clone()) {
                with_states(&surface, |states| {
                    let data = states.data_map.get::<XdgToplevelSurfaceData>();
                    match data {
                        Some(d) => {
                            let g = d.lock().unwrap();
                            (
                                g.app_id.clone().unwrap_or_default(),
                                g.title.clone().unwrap_or_default(),
                            )
                        }
                        None => (String::new(), String::new()),
                    }
                })
            } else if let Some(x11) = window.x11_surface() {
                // X11 doesn't have an app_id; the closest analogue is the
                // resource class (`WM_CLASS` second field — e.g. "Gimp",
                // "Firefox"). Match rules read app_id from there so existing
                // `[[window_rules]]` configs work for both window kinds.
                (x11.class(), x11.title())
            } else {
                return;
            };

        // Cloning the matched rule decouples the borrow on
        // `self.config` from the &mut self methods we call below.
        // Rule sets are tiny (handful of entries in typical configs)
        // so the clone is irrelevant.
        let Some(rule) = self
            .config
            .window_rules
            .iter()
            .find(|r| r.matcher.matches(&app_id, &title))
            .cloned()
        else {
            return;
        };

        tracing::info!(
            app_id = %app_id,
            title = %title,
            ?rule.actions,
            "window rule matched",
        );

        // Apply stickiness before the workspace move: a sticky window
        // ignores move-to-workspace (see `move_window_to_workspace`), so a
        // rule that sets both `sticky = true` and `workspace` keeps the
        // window on all workspaces rather than pinning it to one.
        if let Some(sticky) = rule.actions.sticky {
            self.set_sticky(window, sticky);
        }

        if let Some(on) = rule.actions.always_on_top {
            self.set_always_on_top(window, on);
        }

        if let Some(idx) = rule.actions.workspace {
            match self.workspaces.from_one_based(idx) {
                Some(target) => self.move_window_to_workspace(window, target),
                None => tracing::warn!(
                    idx,
                    count = self.workspaces.count(),
                    "window rule: workspace index out of range"
                ),
            }
        }

        // position + size require the window to still be mapped.
        // `move_window_to_workspace` may have unmapped it if the rule
        // also routed it off the active workspace; in that case the
        // saved_location captured by the move covers the placement and
        // we skip the live re-map. The size action still applies — it
        // just rides on the next configure, whether or not the window
        // is currently in the space.
        let still_mapped = self.space.elements().any(|w| w == window);

        if let Some([x, y]) = rule.actions.position {
            let pos: Point<i32, Logical> = (x, y).into();
            if still_mapped {
                self.space.map_element(window.clone(), pos, false);
            }
            // Update saved_location so later workspace switches restore
            // to the rule-specified spot rather than the auto-centered
            // location captured at map time.
            self.workspaces.record_location(window, pos);
            // Rule won the placement fight — skip the post-commit
            // recentering pass that would otherwise overwrite it.
            self.pending_initial_center.remove(window);
        }

        if let Some([w, h]) = rule.actions.size {
            if w > 0 && h > 0 {
                let size: Size<i32, Logical> = (w, h).into();
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size = Some(size);
                    });
                    toplevel.send_configure();
                } else if let Some(x11) = window.x11_surface() {
                    // Keep the current position; just change the size.
                    let geo = smithay::utils::Rectangle::new(x11.geometry().loc, size);
                    let _ = x11.configure(geo);
                }
            } else {
                tracing::warn!(w, h, "window rule: size must be positive");
            }
        }
    }
}
