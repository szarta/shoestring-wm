//! ext-session-lock-v1: compositor side. A privileged lock client (e.g.
//! `shoestring-lock`) binds the manager, calls `lock`, and creates one
//! lock surface per output. While locked, the render path suppresses
//! every normal window/layer surface and shows only the lock surfaces
//! (or a solid black backstop on outputs without one — also covers the
//! lock client crashing mid-session, per protocol).
//!
//! Keyboard input is rerouted to the lock surface of whichever output is
//! "current" so the client can read the password. The pre-lock focus is
//! saved and restored on `unlock`.

use std::collections::HashMap;

use smithay::{
    backend::renderer::{
        element::{surface::WaylandSurfaceRenderElement, Kind},
        ImportAll, Renderer,
    },
    output::Output,
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    utils::{Scale, SERIAL_COUNTER},
    wayland::session_lock::{
        LockSurface, LockSurfaceConfigure, SessionLockHandler, SessionLockManagerState,
        SessionLocker,
    },
};

use crate::state::ShoestringWm;

/// Live lock-session state. Present iff a client has called
/// `ext_session_lock_manager_v1.lock`.
pub struct LockState {
    /// `Some` until we've acknowledged the lock to the client via
    /// [`SessionLocker::lock`]; taken on the first lock-surface commit so
    /// the client only sees `locked` after we've actually composited the
    /// black/lock frame.
    pub pending: Option<SessionLocker>,
    /// Lock surfaces keyed by output. The map is also used by the render
    /// path to know which surface to draw on each output.
    pub surfaces: HashMap<Output, LockSurface>,
    /// Keyboard focus at the moment we locked, restored on unlock.
    pub saved_focus: Option<WlSurface>,
}

impl SessionLockHandler for ShoestringWm {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        if self.lock_session.is_some() {
            // A second client tried to lock while we're already locked.
            // Dropping `confirmation` notifies it with `finished`.
            tracing::warn!("session_lock: already locked; rejecting new locker");
            return;
        }
        let saved_focus = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        // Clear focus so the previously-focused client stops receiving
        // input; the lock client gets focus once it creates a surface.
        if let Some(kb) = self.seat.get_keyboard() {
            let serial = SERIAL_COUNTER.next_serial();
            kb.set_focus(self, Option::<WlSurface>::None, serial);
        }
        // An in-flight window picker would otherwise stay armed across
        // the lock; resolve it as cancelled so the client can move on.
        self.finish_picker(None);
        self.lock_session = Some(LockState {
            pending: Some(confirmation),
            surfaces: HashMap::new(),
            saved_focus,
        });
        tracing::info!("session_lock: locked, awaiting lock surfaces");
    }

    fn unlock(&mut self) {
        let Some(state) = self.lock_session.take() else {
            return;
        };
        tracing::info!("session_lock: unlocked");
        // Restore previous focus if the surface still exists.
        if let (Some(surface), Some(kb)) = (state.saved_focus, self.seat.get_keyboard()) {
            let serial = SERIAL_COUNTER.next_serial();
            kb.set_focus(self, Some(surface), serial);
        }
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            tracing::warn!("session_lock: new_surface for unknown output");
            return;
        };
        // Suggest the output's logical size; client must commit a buffer
        // matching it.
        let size = output
            .current_mode()
            .map(|m| {
                let scale = output.current_scale().integer_scale().max(1);
                let logical = m.size.to_logical(scale);
                (logical.w as u32, logical.h as u32)
            })
            .unwrap_or((0, 0));
        surface.with_pending_state(|state| {
            state.size = Some((size.0, size.1).into());
        });
        surface.send_configure();
        if let Some(lock) = self.lock_session.as_mut() {
            lock.surfaces.insert(output.clone(), surface);
        }
        // Focus the lock client so it can read the password.
        self.focus_lock_surface();
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: LockSurfaceConfigure) {
        // Nothing extra — client will commit a buffer matching the acked size next.
    }
}

impl ShoestringWm {
    /// True if a session lock is currently active.
    pub fn is_locked(&self) -> bool {
        self.lock_session.is_some()
    }

    /// Find the lock surface for the output containing the pointer (or the
    /// first one) and set keyboard focus to it. No-op while unlocked.
    pub fn focus_lock_surface(&mut self) {
        let Some(lock) = self.lock_session.as_ref() else {
            return;
        };
        let surface = lock
            .surfaces
            .values()
            .next()
            .map(|s| s.wl_surface().clone());
        let Some(surface) = surface else {
            return;
        };
        if let Some(kb) = self.seat.get_keyboard() {
            let already = kb.current_focus().as_ref() == Some(&surface);
            if !already {
                let serial = SERIAL_COUNTER.next_serial();
                kb.set_focus(self, Some(surface), serial);
            }
        }
    }

    /// Clone the lock surface for `output`, if any. Useful for callers
    /// that need to render without holding a `&self` borrow (e.g. the
    /// udev backend, which has already mutably re-borrowed `self.udev`
    /// by the time it builds elements).
    pub fn lock_surface_for(&self, output: &Output) -> Option<LockSurface> {
        self.lock_session
            .as_ref()
            .and_then(|l| l.surfaces.get(output).cloned())
    }
}

/// Build the render elements for `surface`, anchored to the output's
/// top-left. Returns empty when `surface` is `None`; callers rely on the
/// cleared (black) framebuffer for the backstop in that case, so they
/// MUST pass `[0, 0, 0, 1]` as the clear color while locked.
pub fn lock_render_elements<R, E>(
    surface: Option<&LockSurface>,
    renderer: &mut R,
    scale: impl Into<Scale<f64>>,
) -> Vec<E>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
    E: From<WaylandSurfaceRenderElement<R>>,
{
    let Some(surface) = surface else {
        return Vec::new();
    };
    smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
        renderer,
        surface.wl_surface(),
        (0i32, 0i32),
        scale,
        1.0,
        Kind::Unspecified,
    )
}

impl ShoestringWm {
    /// Drive the post-lock confirmation: once any lock surface has a
    /// committed buffer (so the next frame will paint real lock content,
    /// not a transparent surface), tell the client the session is locked.
    /// Called from the compositor commit hook.
    pub fn maybe_confirm_lock(&mut self) {
        let Some(lock) = self.lock_session.as_mut() else {
            return;
        };
        if lock.pending.is_none() || lock.surfaces.is_empty() {
            return;
        }
        if let Some(locker) = lock.pending.take() {
            locker.lock();
            tracing::info!("session_lock: confirmed to client");
        }
    }
}
