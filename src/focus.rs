//! Keyboard-focus target.
//!
//! Keyboard focus can land on either a Wayland surface (xdg toplevels,
//! layer-shell surfaces, lock surfaces, popups) or an XWayland
//! [`X11Surface`]. They need different handling: smithay only sends the X
//! server an `XSetInputFocus` / `WM_TAKE_FOCUS` — the handshake that
//! actually gives an X11 client keyboard input — from [`X11Surface`]'s
//! [`KeyboardTarget`] impl. Focusing the bare `wl_surface` of an X11 window
//! therefore leaves the X client deaf: keys go nowhere and games never grab
//! the pointer for mouse-look. So [`SeatHandler::KeyboardFocus`] is this
//! enum, which delegates each [`KeyboardTarget`] call to whichever inner
//! target is correct.
//!
//! Pointer and touch focus stay `WlSurface`: pointer/touch events reach X11
//! clients fine over the surface's `wl_pointer`/`wl_touch`, and only the
//! keyboard needs the X-side input-focus handshake.
//!
//! [`SeatHandler::KeyboardFocus`]: smithay::input::SeatHandler::KeyboardFocus

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::PopupKind,
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        Seat,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::state::ShoestringWm;

/// What currently holds keyboard focus. See the module docs for why X11
/// surfaces can't just be focused by their `wl_surface`.
// X11Surface is larger than WlSurface; boxing it would complicate the
// KeyboardTarget delegation for no real gain (one of these per seat).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    /// A Wayland surface: xdg toplevel, layer-shell surface, lock surface,
    /// or popup.
    Wayland(WlSurface),
    /// An XWayland toplevel. Focusing it drives `XSetInputFocus` /
    /// `WM_TAKE_FOCUS` so the X client actually receives keyboard input.
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// The underlying `wl_surface` (owned), for comparison against
    /// surface-keyed state. An X11 surface exposes its XWayland-side
    /// `wl_surface` once mapped (`None` before then). Distinct from the
    /// [`WaylandFocus::wl_surface`] impl, which borrows.
    pub fn surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(s) => Some(s.clone()),
            Self::X11(x) => x.wl_surface(),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(s: WlSurface) -> Self {
        Self::Wayland(s)
    }
}

impl From<X11Surface> for KeyboardFocusTarget {
    fn from(x: X11Surface) -> Self {
        Self::X11(x)
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(p: PopupKind) -> Self {
        // A popup is always a Wayland surface.
        Self::Wayland(WlSurface::from(p))
    }
}

/// Required by smithay's popup-grab machinery (`PointerFocus: From<KeyboardFocus>`,
/// and our `PointerFocus` is `WlSurface`). Only ever exercised for Wayland
/// popups; a focused X11 surface always has its `wl_surface` by then.
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        match target {
            KeyboardFocusTarget::Wayland(s) => s,
            KeyboardFocusTarget::X11(x) => x
                .wl_surface()
                .expect("X11 keyboard-focus target converted to WlSurface without a surface"),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            Self::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(x) => x.alive(),
        }
    }
}

impl KeyboardTarget<ShoestringWm> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<ShoestringWm>,
        data: &mut ShoestringWm,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<ShoestringWm>, data: &mut ShoestringWm, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<ShoestringWm>,
        data: &mut ShoestringWm,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<ShoestringWm>,
        data: &mut ShoestringWm,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}
