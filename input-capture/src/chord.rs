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

/// Tracks the held keys and reports the chord a key press completes.
///
/// A chord fires when its last key goes down and exactly its keys are held:
/// a superset (an extra Shift) does not fire, and neither does auto-repeat,
/// so it fires once per press.
#[derive(Clone, Debug)]
pub struct ChordMatcher<T> {
    chords: Vec<(Chord, T)>,
    held: Vec<u32>,
}

impl<T> Default for ChordMatcher<T> {
    fn default() -> Self {
        Self {
            chords: Vec::new(),
            held: Vec::new(),
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
    }

    /// Forgets held keys for which `is_down` is false: their key-ups went
    /// elsewhere (Windows' secure desktop after Ctrl+Alt+Del or Win+L).
    pub fn forget_released(&mut self, mut is_down: impl FnMut(u32) -> bool) {
        self.held.retain(|&k| is_down(k));
    }

    /// the keys held now, in the order they went down
    pub fn held(&self) -> &[u32] {
        &self.held
    }

    /// Feeds a key event (`state` 0: up, otherwise down); returns the target
    /// of the chord this press completes. The first matching chord wins.
    pub fn feed(&mut self, key: u32, state: u8) -> Option<T> {
        if state == 0 {
            self.held.retain(|&k| k != key);
            return None;
        }
        if self.held.contains(&key) {
            // auto-repeat
            return None;
        }
        self.held.push(key);
        self.chords
            .iter()
            .find(|(chord, _)| {
                chord.0.len() == self.held.len() && self.held.iter().all(|&k| chord.contains(k))
            })
            .map(|&(_, target)| target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scancode::Linux::*;

    fn matcher() -> ChordMatcher<u8> {
        let mut m = ChordMatcher::default();
        m.set_chords(vec![
            (Chord::new([KeyLeftCtrl, KeyLeftAlt, Key1]).unwrap(), 1),
            (Chord::new([KeyLeftCtrl, KeyLeftAlt, Key2]).unwrap(), 2),
        ]);
        m
    }

    fn press(m: &mut ChordMatcher<u8>, keys: &[scancode::Linux]) -> Vec<Option<u8>> {
        keys.iter().map(|&k| m.feed(k as u32, 1)).collect()
    }

    #[test]
    fn exact_chord_fires_on_its_last_key() {
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1]),
            [None, None, Some(1)]
        );
        // any order
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[Key2, KeyLeftAlt, KeyLeftCtrl]),
            [None, None, Some(2)]
        );
    }

    #[test]
    fn superset_does_not_fire() {
        let mut m = matcher();
        let fired = press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, KeyLeftShift, Key1]);
        assert_eq!(fired, [None; 4]);
        // releasing the extra key does not fire either: no key went down
        assert_eq!(m.feed(KeyLeftShift as u32, 0), None);
        // pressing the last key again with exactly the chord held does
        assert_eq!(m.feed(Key1 as u32, 0), None);
        assert_eq!(m.feed(Key1 as u32, 1), Some(1));
    }

    #[test]
    fn fires_once_per_press() {
        let mut m = matcher();
        assert_eq!(
            press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1]),
            [None, None, Some(1)]
        );
        // auto-repeat of the last key, and of a modifier
        assert_eq!(m.feed(Key1 as u32, 1), None);
        assert_eq!(m.feed(KeyLeftCtrl as u32, 1), None);
        // a new press with the modifiers still held fires again
        assert_eq!(m.feed(Key1 as u32, 0), None);
        assert_eq!(m.feed(Key1 as u32, 1), Some(1));
        // switching to the other chord while holding the modifiers
        assert_eq!(m.feed(Key1 as u32, 0), None);
        assert_eq!(m.feed(Key2 as u32, 1), Some(2));
    }

    #[test]
    fn left_and_right_modifiers_differ() {
        let mut m = matcher();
        let fired = press(&mut m, &[KeyRightCtrl, KeyLeftAlt, Key1]);
        assert_eq!(fired, [None; 3]);
    }

    #[test]
    fn empty_chord_and_stale_keys() {
        assert_eq!(Chord::new([]), None);
        // KeyA's key-up was missed: it blocks the chord until forgotten
        let mut m = matcher();
        press(&mut m, &[KeyA]);
        assert_eq!(press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1])[2], None);
        m.reset();
        assert_eq!(press(&mut m, &[KeyLeftCtrl, KeyLeftAlt, Key1])[2], Some(1));
        // only the keys no longer down are forgotten
        let mut m = matcher();
        press(&mut m, &[KeyA, KeyLeftCtrl, KeyLeftAlt]);
        m.forget_released(|k| k != KeyA as u32);
        assert_eq!(m.feed(Key2 as u32, 1), Some(2));
    }
}
