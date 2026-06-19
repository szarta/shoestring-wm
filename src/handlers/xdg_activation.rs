//! `xdg_activation_v1` — cross-client focus handoff with focus-stealing
//! prevention.
//!
//! The protocol lets one client request that another client's surface be
//! brought to the front: a launcher (or any foreground app) mints an
//! activation *token* via `get_activation_token` — ideally attaching the
//! seat + input serial of the user action that triggered the launch — and
//! hands it to the app it spawns (conventionally through the
//! `XDG_ACTIVATION_TOKEN` env var or a D-Bus call). That app then calls
//! `activate(token, its_surface)`, which lands in
//! [`XdgActivationHandler::request_activation`] here.
//!
//! **Focus-stealing prevention.** We honor an activation only when it looks
//! user-driven; otherwise focus stays put and we emit a
//! [`Event::WindowActivationRequested`] with `granted: false` so a bar can
//! flag the window as demanding attention. An activation is trusted when:
//!
//! - the token carries a seat + serial (the requester set it from a real
//!   input/focus event — the canonical signal the spec calls out), **or**
//! - the requesting surface is the currently focused window (the foreground
//!   app is handing focus to something it just launched);
//!
//! and in either case the token is not stale (older than
//! [`ACTIVATION_VALID_FOR`]). A background app that activates itself with no
//! user interaction behind it satisfies neither and is refused.
//!
//! Tokens are single-use: we drop the token from the pool once an
//! `activate` referencing it has been handled, granted or not.

use std::time::Duration;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

use crate::state::ShoestringWm;

/// Tokens older than this are treated as untrusted regardless of how they
/// were created — a stale token shouldn't be able to yank focus minutes
/// after the user action that (supposedly) prompted it. Generous enough to
/// cover a slow-starting app launched by a real click.
const ACTIVATION_VALID_FOR: Duration = Duration::from_secs(10);

impl XdgActivationHandler for ShoestringWm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Single-use: whatever we decide below, this token is spent.
        self.xdg_activation_state.remove_token(&token);

        let Some(window) = self.window_for_surface(&surface) else {
            tracing::debug!("xdg_activation: activate for an untracked surface; ignoring");
            return;
        };

        // While locked, honor nothing: `activate_window` would switch
        // workspaces before `focus_window`'s lock-guard blocks the focus
        // step, leaving the locker's workspace shuffled out from under it.
        let trusted = !self.is_locked() && self.activation_is_trusted(&token_data);
        let id = self.foreign_toplevels.get(&window).map(|h| h.identifier());
        tracing::debug!(
            granted = trusted,
            has_serial = token_data.serial.is_some(),
            app_id = ?token_data.app_id,
            "xdg_activation: activate request"
        );

        if trusted {
            // Honors minimized restore + cross-workspace switch + focus.
            self.activate_window(&window);
        }

        // Observability hook (only for windows we can name): report the
        // verdict so a bar can either follow focus or flag attention.
        if let Some(id) = id {
            self.emit_ipc(shoestring_ipc::Event::WindowActivationRequested {
                id,
                granted: trusted,
            });
        }
    }
}

impl ShoestringWm {
    /// Focus-stealing-prevention verdict for an activation token: trust it
    /// only when it's recent *and* either carries a real input serial or was
    /// requested by the currently focused surface. See the module docs.
    fn activation_is_trusted(&self, token_data: &XdgActivationTokenData) -> bool {
        // No serial → fall back to "did the focused window ask for this?".
        let requester_is_focused = token_data.surface.as_ref().is_some_and(|requester| {
            self.focused_window()
                .is_some_and(|focused| crate::window_ext::matches_surface(&focused, requester))
        });
        activation_trusted(
            token_data.timestamp.elapsed() <= ACTIVATION_VALID_FOR,
            token_data.serial.is_some(),
            requester_is_focused,
        )
    }
}

/// The focus-stealing-prevention decision, factored out of any compositor
/// state so the policy is unit-testable: a token is trusted iff it's recent
/// and was either backed by a real input serial or requested by the
/// currently focused surface.
fn activation_trusted(recent: bool, has_serial: bool, requester_is_focused: bool) -> bool {
    recent && (has_serial || requester_is_focused)
}

#[cfg(test)]
mod tests {
    use super::activation_trusted;

    #[test]
    fn stale_tokens_are_never_trusted() {
        // Even a serial-backed, focused-requester token is refused once stale.
        assert!(!activation_trusted(false, true, true));
        assert!(!activation_trusted(false, true, false));
        assert!(!activation_trusted(false, false, true));
    }

    #[test]
    fn recent_token_trusted_on_serial_or_focused_requester() {
        // A real input serial is sufficient on its own.
        assert!(activation_trusted(true, true, false));
        // So is the foreground app handing off (no serial, requester focused).
        assert!(activation_trusted(true, false, true));
        // Both signals present is of course fine too.
        assert!(activation_trusted(true, true, true));
    }

    #[test]
    fn recent_but_unsolicited_is_refused() {
        // Recent, but neither user-driven nor from the focused app — this is
        // exactly the self-activating background app we prevent.
        assert!(!activation_trusted(true, false, false));
    }
}
