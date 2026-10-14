//! Key chords (release bind, hotkeys): which chord a key press completes.

use input_event::scancode;

/// Keys held together, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord(Vec<u32>);

impl Chord {
    /// `None` for no keys, which would match whenever nothing is held
    pub fn new(keys: impl IntoIterator<Item = scancode::Linux>) -> Option<Self> {
        let mut keys: Vec<u32> = keys.into_iter().map(|k| k as u32).collect();
        keys.sort_unstable();
        keys.dedup();
        (!keys.is_empty()).then_some(Self(keys))
    }

    fn contains(&self, key: u32) -> bool {
        self.0.binary_search(&key).is_ok()
    }
}

/// What a key event is to a [`ChordMatcher`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fed<T> {
    /// not the key of a fired chord: handle it as usual
    Pass,
    /// this press completes the chord for `T`
    Fired(T),
    /// auto-repeat or key-up of a [swallowed](ChordMatcher::swallow) key:
    /// drop it
    Swallowed,
}

/// Tracks the held keys and reports the chord a key press completes.
///
/// A chord fires when its last key goes down and exactly its keys are held:
/// a superset (an extra Shift) does not fire, and neither does auto-repeat,
/// so it fires once per press. The final key of a chord that was acted on
/// can be swallowed until it goes up, auto-repeat included.
#[derive(Clone, Debug)]
pub struct ChordMatcher<T> {
    chords: Vec<(Chord, T)>,
    held: Vec<u32>,
    /// held keys whose events are dropped until they go up
    swallowed: Vec<u32>,
}

impl<T> Default for ChordMatcher<T> {
    fn default() -> Self {
        Self {
            chords: Vec::new(),
            held: Vec::new(),
            swallowed: Vec::new(),
        }
    }
}

impl<T: Copy> ChordMatcher<T> {
    /// replaces the chords; held keys stay tracked
    pub fn set_chords(&mut self, chords: Vec<(Chord, T)>) {
        self.chords = chords;
    }

    /// whether there are no chords to watch
    pub fn is_empty(&self) -> bool {
        self.chords.is_empty()
    }

    /// forget the held keys (key-ups were not seen, e.g. after a switch)
    pub fn reset(&mut self) {
        self.held.clear();
        self.swallowed.clear();
    }

    /// Forgets held keys for which `is_down` is false: their key-ups went
    /// elsewhere (Windows' secure desktop after Ctrl+Alt+Del or Win+L).
    pub fn forget_released(&mut self, mut is_down: impl FnMut(u32) -> bool) {
        self.held.retain(|&k| is_down(k));
        let held = &self.held;
        self.swallowed.retain(|k| held.contains(k));
    }

    /// the keys held now, in the order they went down
    pub fn held(&self) -> &[u32] {
        &self.held
    }

    /// Swallows the held `key` (the final key of a chord that was acted on,
    /// whose press went nowhere): its auto-repeat and key-up feed as
    /// [`Fed::Swallowed`].
    pub fn swallow(&mut self, key: u32) {
        if self.held.contains(&key) && !self.swallowed.contains(&key) {
            self.swallowed.push(key);
        }
    }

    /// Feeds a key event (`state` 0: up, otherwise down). A press reports
    /// the target of the chord it completes; the first matching chord wins.
    pub fn feed(&mut self, key: u32, state: u8) -> Fed<T> {
        if self.held.contains(&key) {
            // auto-repeat, or the key-up
            let swallowed = self.swallowed.contains(&key);
            if state == 0 {
                self.held.retain(|&k| k != key);
                self.swallowed.retain(|&k| k != key);
            }
            return if swallowed { Fed::Swallowed } else { Fed::Pass };
        }
        if state == 0 {
            return Fed::Pass;
        }
        self.held.push(key);
        self.chords
            .iter()
            .find(|(chord, _)| {
                chord.0.len() == self.held.len() && self.held.iter().all(|&k| chord.contains(k))
            })
            .map_or(Fed::Pass, |&(_, target)| Fed::Fired(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Fed::{Fired, Pass, Swallowed};
    use scancode::Linux::*;

    fn matcher() -> ChordMatcher<u8> {
        let mut m = ChordMatcher::default();
        m.set_chords(vec![
            (Chord::new([KeyLeftCtrl, KeyLeftAlt, Key1]).unwrap(), 1),
            (Chord::new([KeyLeftCtrl, KeyLeftAlt, Key2]).unwrap(), 2),
        ]);
        m
    }

    fn press(m: &mut ChordMatcher<u8>, keys: &[scancode::Linux]) -> Vec<Fed<u8>> {
        keys.iter().map(|&k| m.feed(k as u32, 1)).collect()
    }

    #[test]
    fn exact_chord_fires_on_its_last_key() {
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1]),
            [Pass, Pass, Fired(1)]
        );
        // any order
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[Key2, KeyLeftAlt, KeyLeftCtrl]),
            [Pass, Pass, Fired(2)]
        );
    }

    #[test]
    fn superset_does_not_fire() {
        let mut m = matcher();
        let fired = press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, KeyLeftShift, Key1]);
        assert_eq!(fired, [Pass; 4]);
        // releasing the extra key does not fire either: no key went down
        assert_eq!(m.feed(KeyLeftShift as u32, 0), Pass);
        // pressing the last key again with exactly the chord held does
        assert_eq!(m.feed(Key1 as u32, 0), Pass);
        assert_eq!(m.feed(Key1 as u32, 1), Fired(1));
    }

    #[test]
    fn fires_once_per_press() {
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1]),
            [Pass, Pass, Fired(1)]
        );
        // auto-repeat of the last key, and of a modifier
        assert_eq!(m.feed(Key1 as u32, 1), Pass);
        assert_eq!(m.feed(KeyLeftCtrl as u32, 1), Pass);
        // a new press with the modifiers still held fires again
        assert_eq!(m.feed(Key1 as u32, 0), Pass);
        assert_eq!(m.feed(Key1 as u32, 1), Fired(1));
        // switching to the other chord while holding the modifiers
        assert_eq!(m.feed(Key1 as u32, 0), Pass);
        assert_eq!(m.feed(Key2 as u32, 1), Fired(2));
    }

    #[test]
    fn swallowed_final_key_is_dropped_until_its_key_up() {
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1]),
            [Pass, Pass, Fired(1)]
        );
        m.swallow(Key1 as u32);
        // auto-repeat of the final key
        assert_eq!(press(&mut m, &[Key1, Key1]), [Swallowed, Swallowed]);
        // other keys pass, the held modifiers' auto-repeat included
        assert_eq!(press(&mut m, &[KeyLeftCtrl, KeyA]), [Pass, Pass]);
        assert_eq!(m.feed(KeyA as u32, 0), Pass);
        assert_eq!(m.feed(Key1 as u32, 0), Swallowed);
        // after the key-up the key is its own again
        assert_eq!(m.feed(Key1 as u32, 1), Fired(1));
        assert_eq!(m.feed(Key1 as u32, 1), Pass);
        assert_eq!(m.feed(Key1 as u32, 0), Pass);
        // a reset or a forgotten key-up ends the swallowing
        assert_eq!(m.feed(Key2 as u32, 1), Fired(2));
        m.swallow(Key2 as u32);
        m.reset();
        assert_eq!(m.feed(Key2 as u32, 1), Pass);
        m.swallow(Key2 as u32);
        m.forget_released(|k| k != Key2 as u32);
        assert_eq!(m.feed(Key2 as u32, 1), Pass);
        // keys not held cannot be swallowed
        m.swallow(KeyB as u32);
        assert_eq!(m.feed(KeyB as u32, 1), Pass);
    }

    #[test]
    fn left_and_right_modifiers_differ() {
        let mut m = matcher();
        let fired = press(&mut m, &[KeyRightCtrl, KeyLeftAlt, Key1]);
        assert_eq!(fired, [Pass; 3]);
    }

    #[test]
    fn empty_chord_and_stale_keys() {
        assert_eq!(Chord::new([]), None);
        // KeyA's key-up was missed: it blocks the chord until forgotten
        let mut m = matcher();
        press(&mut m, &[KeyA]);
        assert_eq!(press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1])[2], Pass);
        m.reset();
        assert_eq!(press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1])[2], Fired(1));
        // only the keys no longer down are forgotten
        let mut m = matcher();
        press(&mut m, &[KeyA, KeyLeftCtrl, KeyLeftAlt]);
        m.forget_released(|k| k != KeyA as u32);
        assert_eq!(m.feed(Key2 as u32, 1), Fired(2));
    }
}
