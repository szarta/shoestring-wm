//! Which keys and buttons the connected viewers hold down.
//!
//! The WM sees every session's input as one source (raw injection), so the
//! server keeps the per-viewer bookkeeping: each session records what it has
//! pressed, and a shared count across sessions decides what reaches the WM.
//! A press goes through only when it is the first viewer to hold that key, a
//! release only when it is the last to let go. When a session ends with keys
//! still down (a dropped connection mid-chord or mid-drag), [`Holds::end`]
//! releases those no other viewer holds, so they don't stay pressed on the
//! served seat while everyone else keeps working.
//!
//! A transition is delivered to the WM while the counts are still locked
//! ([`Holds::transition_then`], [`Holds::end_then`]), so the WM sees them in
//! the order the counts changed. Otherwise one viewer's Super could be
//! counted, then overtaken by another viewer's L, and the WM would see a
//! plain L; or a leaving viewer's release could land after another viewer's
//! new press of the same key and leave it up while that viewer holds it.
//!
//! The WM covers the cases the server can't: when the automation gate closes
//! (the remote gate going off, or this server exiting or crashing) it releases
//! everything injection holds itself. The server then starts a new generation
//! ([`Holds::reset`]), so a session still tearing down from before can't
//! release a key a newer session has pressed since.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

/// A key (evdev keycode) or pointer button (evdev code) a viewer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Held {
    Key(u32),
    Button(u32),
}

/// What one session holds, in the generation it started in.
#[derive(Debug, Default)]
pub struct SessionHolds {
    generation: u64,
    held: BTreeSet<Held>,
}

/// How many sessions of the current generation hold each key or button.
#[derive(Default)]
struct Counts {
    generation: u64,
    counts: HashMap<Held, usize>,
}

#[derive(Default)]
pub struct Holds {
    inner: Mutex<Counts>,
}

impl Holds {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bookkeeping for a new session.
    pub fn session(&self) -> SessionHolds {
        SessionHolds {
            generation: self.inner.lock().unwrap().generation,
            held: BTreeSet::new(),
        }
    }

    /// Record a session's transition. Returns whether it should reach the WM:
    /// the first press of something no other session holds, or the release of
    /// the last hold. A repeated press, a stray release, or anything from a
    /// session of an earlier generation returns `false`.
    #[cfg(test)]
    pub fn transition(&self, mine: &mut SessionHolds, held: Held, pressed: bool) -> bool {
        self.transition_then(mine, held, pressed, || ()).is_some()
    }

    /// Record a session's transition and, when it should reach the WM (see
    /// [`Holds::transition`]), `deliver` it before another session's
    /// transition can be counted. Returns what `deliver` returned, or `None`
    /// when the transition stays with the server.
    pub fn transition_then<T>(
        &self,
        mine: &mut SessionHolds,
        held: Held,
        pressed: bool,
        deliver: impl FnOnce() -> T,
    ) -> Option<T> {
        let mut inner = self.inner.lock().unwrap();
        Self::record(&mut inner, mine, held, pressed).then(deliver)
    }

    fn record(inner: &mut Counts, mine: &mut SessionHolds, held: Held, pressed: bool) -> bool {
        if mine.generation != inner.generation {
            return false;
        }
        if pressed {
            if !mine.held.insert(held) {
                return false;
            }
            let n = inner.counts.entry(held).or_insert(0);
            *n += 1;
            *n == 1
        } else {
            if !mine.held.remove(&held) {
                return false;
            }
            Self::drop_one(&mut inner.counts, held)
        }
    }

    /// A session ended: forget everything it held and return what now has no
    /// holder at all, for the caller to release. Nothing, for a session of an
    /// earlier generation: the WM already released its keys.
    #[cfg(test)]
    pub fn end(&self, mine: &mut SessionHolds) -> Vec<Held> {
        self.end_then(mine, |released| released)
    }

    /// [`Holds::end`], handing what to release to `release` before another
    /// session's transition can be counted.
    pub fn end_then<T>(&self, mine: &mut SessionHolds, release: impl FnOnce(Vec<Held>) -> T) -> T {
        let mut inner = self.inner.lock().unwrap();
        let held = std::mem::take(&mut mine.held);
        let released = if mine.generation == inner.generation {
            held.into_iter()
                .filter(|&h| Self::drop_one(&mut inner.counts, h))
                .collect()
        } else {
            Vec::new()
        };
        release(released)
    }

    /// The gate closed: every session is ending and the WM has released what
    /// they held. Start a new generation so their teardown can't touch the
    /// holds of sessions that connect after the gate reopens.
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        inner.counts.clear();
    }

    /// Drop one holder of `held`; returns whether that was the last.
    fn drop_one(counts: &mut HashMap<Held, usize>, held: Held) -> bool {
        match counts.get_mut(&held) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                counts.remove(&held);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: Held = Held::Key(42);
    const LEFT: Held = Held::Button(0x110);

    #[test]
    fn one_session_passes_presses_and_releases_once() {
        let holds = Holds::new();
        let mut a = holds.session();
        assert!(holds.transition(&mut a, SHIFT, true));
        assert!(!holds.transition(&mut a, SHIFT, true), "repeated press");
        assert!(holds.transition(&mut a, SHIFT, false));
        assert!(!holds.transition(&mut a, SHIFT, false), "stray release");
    }

    #[test]
    fn a_key_held_by_two_sessions_is_released_by_the_last() {
        let holds = Holds::new();
        let (mut a, mut b) = (holds.session(), holds.session());
        assert!(holds.transition(&mut a, SHIFT, true));
        assert!(!holds.transition(&mut b, SHIFT, true), "already down");
        assert!(!holds.transition(&mut a, SHIFT, false), "b still holds it");
        assert!(holds.transition(&mut b, SHIFT, false));
    }

    #[test]
    fn ending_a_session_releases_only_what_nobody_else_holds() {
        let holds = Holds::new();
        let (mut a, mut b) = (holds.session(), holds.session());
        holds.transition(&mut a, SHIFT, true);
        holds.transition(&mut a, LEFT, true);
        holds.transition(&mut b, SHIFT, true);

        assert_eq!(holds.end(&mut a), vec![LEFT]);
        // b's Shift survived a's exit, and b ending releases it.
        assert_eq!(holds.end(&mut b), vec![SHIFT]);
        // Nothing is left: a new press is the first again.
        let mut c = holds.session();
        assert!(holds.transition(&mut c, SHIFT, true));
    }

    /// Press `held` for `session` on its own thread, logging `name` when the
    /// press is delivered, `pause` after it was counted.
    fn press_on_thread(
        holds: &std::sync::Arc<Holds>,
        log: &std::sync::Arc<Mutex<Vec<&'static str>>>,
        mut session: SessionHolds,
        held: Held,
        name: &'static str,
        pause: std::time::Duration,
    ) -> std::thread::JoinHandle<()> {
        let (holds, log) = (holds.clone(), log.clone());
        std::thread::spawn(move || {
            holds.transition_then(&mut session, held, true, || {
                std::thread::sleep(pause);
                log.lock().unwrap().push(name);
            });
        })
    }

    #[test]
    fn a_counted_press_is_delivered_before_a_later_one() {
        let holds = std::sync::Arc::new(Holds::new());
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (a, b) = (holds.session(), holds.session());
        // a's Super is counted first but slow to deliver; b's L follows.
        let super_down = press_on_thread(
            &holds,
            &log,
            a,
            Held::Key(125),
            "super",
            std::time::Duration::from_millis(100),
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        let l_down = press_on_thread(
            &holds,
            &log,
            b,
            Held::Key(38),
            "l",
            std::time::Duration::ZERO,
        );
        super_down.join().unwrap();
        l_down.join().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["super", "l"]);
    }

    #[test]
    fn a_stale_session_cannot_touch_the_next_generation() {
        let holds = Holds::new();
        let mut old = holds.session();
        holds.transition(&mut old, SHIFT, true);
        holds.reset();

        // A session after the reset presses Shift: the first press again.
        let mut new = holds.session();
        assert!(holds.transition(&mut new, SHIFT, true));
        // The old session's late release and teardown leave it alone.
        assert!(!holds.transition(&mut old, SHIFT, false));
        assert_eq!(holds.end(&mut old), vec![]);
        assert!(!holds.transition(&mut new, SHIFT, true), "still held");
        assert!(holds.transition(&mut new, SHIFT, false));
    }
}
