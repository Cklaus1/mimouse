//! Evdev key semantics shared by the backends that repeat keys themselves
//! (Windows and macOS do not repeat injected keys): which keys are
//! modifiers, and which key repeats.

use input_event::scancode::Linux;

/// X11 modifier masks, as carried by `KeyboardEvent::Modifiers`.
pub(crate) const SHIFT: u32 = 1 << 0;
pub(crate) const LOCK: u32 = 1 << 1;
pub(crate) const CONTROL: u32 = 1 << 2;
pub(crate) const MOD1: u32 = 1 << 3;
pub(crate) const MOD4: u32 = 1 << 6;

/// The X11 modifier a key changes, `None` for ordinary keys.
pub(crate) fn modifier_mask(key: u32) -> Option<u32> {
    let mask = match Linux::try_from(key).ok()? {
        Linux::KeyLeftShift | Linux::KeyRightShift => SHIFT,
        Linux::KeyCapsLock => LOCK,
        Linux::KeyLeftCtrl | Linux::KeyRightCtrl => CONTROL,
        Linux::KeyLeftAlt | Linux::KeyRightalt => MOD1,
        Linux::KeyLeftMeta | Linux::KeyRightmeta => MOD4,
        _ => return None,
    };
    Some(mask)
}

/// What a key event does to the software key repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    /// start repeating this key (evdev code), replacing any other
    Start(u32),
    /// stop the repeat
    Stop,
    /// leave the repeat as it is
    Keep,
}

/// Software key repeat: the last pressed ordinary key repeats until it is
/// released. Modifiers never repeat and never stop a repeat (holding `a`
/// and tapping Shift keeps `a` repeating), and releasing another key keeps
/// it too.
#[derive(Debug, Default)]
pub(crate) struct RepeatPlanner {
    repeating: Option<u32>,
}

impl RepeatPlanner {
    /// Plans a key event; `key` is the evdev code.
    pub(crate) fn key(&mut self, key: u32, pressed: bool) -> Repeat {
        if modifier_mask(key).is_some() {
            Repeat::Keep
        } else if pressed {
            self.repeating = Some(key);
            Repeat::Start(key)
        } else if self.repeating == Some(key) {
            self.repeating = None;
            Repeat::Stop
        } else {
            Repeat::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Linux) -> u32 {
        k as u32
    }

    #[test]
    fn modifiers_neither_repeat_nor_stop_the_repeat() {
        let mut p = RepeatPlanner::default();
        let a = key(Linux::KeyA);
        assert_eq!(p.key(a, true), Repeat::Start(a));
        assert_eq!(p.key(key(Linux::KeyLeftShift), true), Repeat::Keep);
        assert_eq!(p.key(key(Linux::KeyLeftShift), false), Repeat::Keep);
        assert_eq!(p.key(a, false), Repeat::Stop);
    }

    #[test]
    fn releasing_another_key_keeps_the_repeat() {
        let mut p = RepeatPlanner::default();
        let (a, b) = (key(Linux::KeyA), key(Linux::KeyB));
        p.key(a, true);
        assert_eq!(p.key(b, true), Repeat::Start(b));
        // A is released while B repeats: B must keep repeating
        assert_eq!(p.key(a, false), Repeat::Keep);
        assert_eq!(p.key(b, false), Repeat::Stop);
        assert_eq!(p.key(b, false), Repeat::Keep);
    }
}
