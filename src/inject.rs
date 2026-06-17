//! Synthetic input injection — the WM-side machinery behind the IPC
//! `inject_key` / `inject_text` / `inject_click` requests and their
//! [`shoestring_config::Action`] equivalents.
//!
//! Synthesized events go through the same seat as real input, so the
//! focused client sees them as ordinary key/pointer events. The injection
//! path does NOT consult the WM's binding table — we don't want a
//! scripted `Super+q` to quit the WM. Callers wanting WM actions should
//! use the typed IPC requests directly.
//!
//! Scope (v1):
//! - `inject_key`: any keysym understood by `xkb_keysym_from_name`
//!   (e.g. `"Return"`, `"F5"`, `"BackSpace"`, `"q"`). Resolved against the
//!   currently active keymap.
//! - `inject_text`: ASCII letters (case-sensitive), digits, and ASCII
//!   space. Other codepoints return an error so the caller knows v1
//!   doesn't cover them — extending to the full keymap is a follow-up.
//! - `inject_click`: `"left"` / `"right"` / `"middle"` or a numeric
//!   BTN_* code. Optional `(x, y)` pre-moves the pointer.

use smithay::{
    backend::input::{ButtonState, KeyState},
    input::{
        keyboard::{xkb, FilterResult, Keysym},
        pointer::{ButtonEvent, MotionEvent},
    },
    utils::SERIAL_COUNTER,
};

use crate::state::ShoestringWm;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("unknown keysym name: {0:?}")]
    UnknownKeysym(String),
    #[error("keysym {0:?} is not produced by any key in the current keymap")]
    KeysymNotInKeymap(String),
    #[error("character {0:?} cannot be typed (v1 supports ASCII letters/digits/space)")]
    UnsupportedChar(char),
    #[error("unknown button: {0:?} (expected left/right/middle or a numeric BTN_* code)")]
    UnknownButton(String),
}

impl ShoestringWm {
    pub fn inject_key(
        &mut self,
        keysym_name: &str,
        modifiers: &[String],
    ) -> Result<(), InjectError> {
        // Resolve everything up-front so a typo in any modifier aborts
        // before we've started pressing keys — half-applied chords leave
        // sticky modifiers on the focused surface.
        let mut mod_keycodes: Vec<u32> = Vec::with_capacity(modifiers.len());
        for m in modifiers {
            let resolved = resolve_modifier_alias(m).unwrap_or(m.as_str());
            let sym = parse_keysym_name(resolved)?;
            let key = self
                .resolve_keysym(sym)
                .ok_or_else(|| InjectError::KeysymNotInKeymap(resolved.to_string()))?;
            mod_keycodes.push(key.keycode);
        }
        let sym = parse_keysym_name(keysym_name)?;
        let key = self
            .resolve_keysym(sym)
            .ok_or_else(|| InjectError::KeysymNotInKeymap(keysym_name.to_string()))?;
        tracing::debug!(
            keysym_name,
            keycode = key.keycode,
            modifiers = ?modifiers,
            "inject_key dispatch",
        );

        for kc in &mod_keycodes {
            self.press_key(*kc);
        }
        self.tap_key(key.keycode);
        for kc in mod_keycodes.iter().rev() {
            self.release_key(*kc);
        }
        Ok(())
    }

    pub fn inject_text(&mut self, text: &str) -> Result<(), InjectError> {
        // Validate up-front so we don't half-type a string before failing.
        for ch in text.chars() {
            if !is_supported_ascii(ch) {
                return Err(InjectError::UnsupportedChar(ch));
            }
        }
        let shift_keycode = self.resolve_keysym(Keysym::Shift_L).map(|k| k.keycode);
        for ch in text.chars() {
            let sym = Keysym::from_char(ch);
            let Some(key) = self.resolve_keysym(sym) else {
                // Should be unreachable given the up-front check, but if a
                // keymap really lacks (say) the digit row we surface it
                // rather than silently dropping the character.
                return Err(InjectError::UnsupportedChar(ch));
            };
            let needs_shift = key.shifted;
            tracing::trace!(ch = %ch, keycode = key.keycode, needs_shift, "inject_text char");
            if needs_shift {
                if let Some(kc) = shift_keycode {
                    self.press_key(kc);
                }
            }
            self.tap_key(key.keycode);
            if needs_shift {
                if let Some(kc) = shift_keycode {
                    self.release_key(kc);
                }
            }
        }
        Ok(())
    }

    /// Move the pointer to `(x, y)` without synthesizing a click. Mirrors
    /// the motion half of [`inject_click`] but skips the focus update and
    /// button events — that matches `xdotool mousemove` semantics and lets
    /// callers compose drags (move → press → move → release) without each
    /// step stealing keyboard focus.
    pub fn inject_move_mouse(&mut self, x: f64, y: f64) {
        let pointer = self.seat.get_pointer().expect("seat must have pointer");
        let serial = SERIAL_COUNTER.next_serial();
        let under = self.surface_under((x, y).into());
        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: (x, y).into(),
                serial,
                time: monotonic_msec(),
            },
        );
        let pointer = self.seat.get_pointer().unwrap();
        pointer.frame(self);
        self.last_pointer_focus = under;
    }

    /// Current pointer location in compositor-space logical coords. Returns
    /// `(0.0, 0.0)` if no pointer is bound to the seat — should be
    /// unreachable in normal operation since we install one on startup.
    pub fn pointer_position(&self) -> (f64, f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return (0.0, 0.0);
        };
        let loc = pointer.current_location();
        (loc.x, loc.y)
    }

    pub fn inject_click(
        &mut self,
        button: &str,
        xy: Option<(f64, f64)>,
    ) -> Result<(), InjectError> {
        let code = parse_button(button)?;
        let pointer = self.seat.get_pointer().expect("seat must have pointer");

        if let Some((x, y)) = xy {
            let serial = SERIAL_COUNTER.next_serial();
            let under = self.surface_under((x, y).into());
            pointer.motion(
                self,
                under.clone(),
                &MotionEvent {
                    location: (x, y).into(),
                    serial,
                    time: monotonic_msec(),
                },
            );
            pointer.frame(self);
            self.last_pointer_focus = under;
        }

        // Mirror the WM's normal click-to-focus behavior — without this,
        // an injected click doesn't update keyboard focus the way a real
        // click does. This must match the real-input path (see `crate::input`)
        // for layer-shell surfaces: focus a window only when no Top/Overlay
        // layer covers the point, and never clear focus when *any* surface
        // (a bar, tray menu, or picker) is under the pointer — clearing it
        // would yank that layer surface's own keyboard focus and make a menu
        // dismiss on its own click. The Super+drag carve-out is omitted
        // intentionally: an injected click has no modifier state.
        let pos = self.seat.get_pointer().unwrap().current_location();
        match self.space.element_under(pos).map(|(w, _)| w.clone()) {
            Some(window) if !self.overlay_layer_under(pos) => self.focus_window(&window),
            Some(_) => {}
            None => {
                if self.surface_under(pos).is_none() {
                    self.clear_focus();
                }
            }
        }

        let press_serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.button(
            self,
            &ButtonEvent {
                button: code,
                state: ButtonState::Pressed,
                serial: press_serial,
                time: monotonic_msec(),
            },
        );
        pointer.frame(self);

        let release_serial = SERIAL_COUNTER.next_serial();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.button(
            self,
            &ButtonEvent {
                button: code,
                state: ButtonState::Released,
                serial: release_serial,
                time: monotonic_msec(),
            },
        );
        pointer.frame(self);
        Ok(())
    }

    fn tap_key(&mut self, keycode: u32) {
        self.press_key(keycode);
        self.release_key(keycode);
    }

    fn press_key(&mut self, keycode: u32) {
        self.dispatch_key(keycode, KeyState::Pressed);
    }

    fn release_key(&mut self, keycode: u32) {
        self.dispatch_key(keycode, KeyState::Released);
    }

    /// `keycode` is the X-style keycode (evdev + 8) — same numbering xkb
    /// uses, and what `KeyboardHandle::input` expects (see smithay's
    /// libinput backend, which does `key() + 8` before calling in).
    fn dispatch_key(&mut self, keycode: u32, state: KeyState) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = monotonic_msec();
        let keyboard = self.seat.get_keyboard().expect("seat must have keyboard");
        // FilterResult::Forward unconditionally — injected keys bypass the
        // WM's binding table by design (see module docs).
        keyboard.input::<(), _>(
            self,
            keycode.into(),
            state,
            serial,
            time,
            |_state, _mods, _handle| FilterResult::Forward,
        );
    }

    /// Walk the active xkb keymap looking for any (keycode, layout, level)
    /// that produces `target`. Returns the X-style keycode (matches
    /// `KeyboardHandle::input`'s expectation) and a flag for whether the
    /// level requires shift (level > 0).
    fn resolve_keysym(&mut self, target: Keysym) -> Option<ResolvedKey> {
        let keyboard = self.seat.get_keyboard().expect("seat must have keyboard");
        keyboard.with_xkb_state(self, |context| {
            let xkb_guard = context.xkb().lock().ok()?;
            // SAFETY: the keymap reference is only used inside this closure,
            // which holds the Xkb mutex guard for its entire lifetime — the
            // keymap can't be replaced underneath us until we drop it.
            let keymap = unsafe { xkb_guard.keymap() };
            let min = keymap.min_keycode().raw();
            let max = keymap.max_keycode().raw();
            for kc in min..=max {
                let keycode = xkb::Keycode::new(kc);
                // Skip keycodes the keymap doesn't actually define — they
                // have 0 layouts and asking for levels would be wasted work.
                // Matches the `xkbcommon/how-to-type` example's filter.
                if keymap.key_get_name(keycode).is_none() {
                    continue;
                }
                let num_layouts = keymap.num_layouts_for_key(keycode);
                for layout in 0..num_layouts {
                    let num_levels = keymap.num_levels_for_key(keycode, layout);
                    for level in 0..num_levels {
                        let syms = keymap.key_get_syms_by_level(keycode, layout, level);
                        if syms.len() == 1 && syms[0] == target {
                            return Some(ResolvedKey {
                                keycode: kc,
                                shifted: level > 0,
                            });
                        }
                    }
                }
            }
            None
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedKey {
    /// X-style keycode (evdev + 8). Pass straight to
    /// `KeyboardHandle::input` — it speaks the same numbering.
    keycode: u32,
    shifted: bool,
}

/// Map a short modifier name (case-insensitive) to its canonical xkb
/// keysym name. Aliases match the WM's own keybind parser so chord
/// strings written for `[[bindings]]` work verbatim in IPC requests.
/// Returns `None` for names that aren't an alias — callers fall back to
/// treating the input as a literal keysym name.
fn resolve_modifier_alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "super" | "logo" | "mod4" | "win" => Some("Super_L"),
        "ctrl" | "control" => Some("Control_L"),
        "alt" | "mod1" => Some("Alt_L"),
        "shift" => Some("Shift_L"),
        _ => None,
    }
}

fn parse_keysym_name(name: &str) -> Result<Keysym, InjectError> {
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if sym == Keysym::NoSymbol {
        Err(InjectError::UnknownKeysym(name.to_string()))
    } else {
        Ok(sym)
    }
}

fn parse_button(s: &str) -> Result<u32, InjectError> {
    match s {
        "left" | "Left" => Ok(BTN_LEFT),
        "right" | "Right" => Ok(BTN_RIGHT),
        "middle" | "Middle" => Ok(BTN_MIDDLE),
        other => other
            .parse::<u32>()
            .map_err(|_| InjectError::UnknownButton(s.to_string())),
    }
}

/// True for the v1 `inject_text` whitelist: ASCII A-Z / a-z / 0-9 / space.
fn is_supported_ascii(ch: char) -> bool {
    matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | ' ')
}

/// CLOCK_MONOTONIC-style millisecond timestamp for event time fields.
/// Matches what libinput hands us for real input, so synthesized and real
/// events sort consistently in the wayland queue.
pub(crate) fn monotonic_msec() -> u32 {
    use std::time::Instant;
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_millis() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_button_names_and_codes() {
        assert_eq!(parse_button("left").unwrap(), BTN_LEFT);
        assert_eq!(parse_button("right").unwrap(), BTN_RIGHT);
        assert_eq!(parse_button("middle").unwrap(), BTN_MIDDLE);
        assert_eq!(parse_button("272").unwrap(), 272);
        assert!(matches!(
            parse_button("scroll").unwrap_err(),
            InjectError::UnknownButton(_)
        ));
    }

    #[test]
    fn parse_keysym_name_known_and_unknown() {
        assert_ne!(parse_keysym_name("Return").unwrap(), Keysym::NoSymbol);
        assert_ne!(parse_keysym_name("q").unwrap(), Keysym::NoSymbol);
        assert!(matches!(
            parse_keysym_name("definitely-not-a-keysym").unwrap_err(),
            InjectError::UnknownKeysym(_)
        ));
    }

    #[test]
    fn modifier_aliases_match_keybind_parser() {
        assert_eq!(resolve_modifier_alias("super"), Some("Super_L"));
        assert_eq!(resolve_modifier_alias("Super"), Some("Super_L"));
        assert_eq!(resolve_modifier_alias("LOGO"), Some("Super_L"));
        assert_eq!(resolve_modifier_alias("mod4"), Some("Super_L"));
        assert_eq!(resolve_modifier_alias("win"), Some("Super_L"));
        assert_eq!(resolve_modifier_alias("ctrl"), Some("Control_L"));
        assert_eq!(resolve_modifier_alias("control"), Some("Control_L"));
        assert_eq!(resolve_modifier_alias("alt"), Some("Alt_L"));
        assert_eq!(resolve_modifier_alias("mod1"), Some("Alt_L"));
        assert_eq!(resolve_modifier_alias("shift"), Some("Shift_L"));
        // Non-aliases fall through so raw keysym names still work.
        assert_eq!(resolve_modifier_alias("Hyper_L"), None);
        assert_eq!(resolve_modifier_alias("q"), None);
    }

    #[test]
    fn supported_ascii_set() {
        assert!(is_supported_ascii('a'));
        assert!(is_supported_ascii('Z'));
        assert!(is_supported_ascii('5'));
        assert!(is_supported_ascii(' '));
        assert!(!is_supported_ascii('!'));
        assert!(!is_supported_ascii('\n'));
        assert!(!is_supported_ascii('é'));
    }
}
