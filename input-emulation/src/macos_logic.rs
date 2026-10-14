//! Platform-independent part of the macOS emulation: how keyboard events
//! become `CGEvent`s. Kept free of CoreGraphics types so it is tested on
//! every platform.

use crate::keys::{CONTROL, LOCK, MOD1, MOD4, Repeat, RepeatPlanner, SHIFT, modifier_mask};

/// AltGr / ISO_Level3_Shift on most XKB layouts.
pub(crate) const MOD5: u32 = 1 << 7;

/// `CGEventFlags` bits.
pub(crate) const CG_ALPHA_SHIFT: u64 = 0x0001_0000;
pub(crate) const CG_SHIFT: u64 = 0x0002_0000;
pub(crate) const CG_CONTROL: u64 = 0x0004_0000;
pub(crate) const CG_ALTERNATE: u64 = 0x0008_0000;
pub(crate) const CG_COMMAND: u64 = 0x0010_0000;

/// `CGEventFlags` bits for a set of X11 modifiers.
pub(crate) fn cg_flags(mods: u32) -> u64 {
    [
        (SHIFT, CG_SHIFT),
        (LOCK, CG_ALPHA_SHIFT),
        (CONTROL, CG_CONTROL),
        (MOD1, CG_ALTERNATE),
        (MOD5, CG_ALTERNATE),
        (MOD4, CG_COMMAND),
    ]
    .into_iter()
    .filter(|&(x, _)| mods & x != 0)
    .fold(0, |flags, (_, cg)| flags | cg)
}

/// What to post for one key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyAction {
    /// A modifier changed: post a flags-changed event carrying the
    /// modifier's own key code and the new flags. Modifiers never repeat.
    FlagsChanged {
        code: u16,
        pressed: bool,
        flags: u64,
    },
    /// Post a key-down; `code` becomes the only repeating key.
    Press { code: u16, flags: u64 },
    /// Post a key-up; stop the repeat first if `code` is the repeating key.
    Release {
        code: u16,
        flags: u64,
        stop_repeat: bool,
    },
}

/// Modifier and key-repeat state of the emulated keyboard.
#[derive(Debug, Default)]
pub(crate) struct KeyPlanner {
    /// effective modifiers: the sender's mask, updated by modifier keys
    mods: u32,
    /// modifiers of the modifier keys held down
    held: u32,
    repeat: RepeatPlanner,
}

impl KeyPlanner {
    /// Plans a key event; `key` is the evdev code, `code` its macOS key code.
    pub(crate) fn key(&mut self, key: u32, code: u16, pressed: bool) -> KeyAction {
        if let Some(mask) = modifier_mask(key) {
            if pressed {
                self.held |= mask;
                self.mods |= mask;
            } else {
                self.held &= !mask;
                // the sender's mask may name this key differently (e.g.
                // Control for Caps Lock with ctrl:nocaps): keep only what
                // a held modifier key still accounts for
                self.mods &= self.held;
            }
            return KeyAction::FlagsChanged {
                code,
                pressed,
                flags: self.flags(),
            };
        }
        let flags = self.flags();
        let repeat = self.repeat.key(key, pressed);
        if pressed {
            KeyAction::Press { code, flags }
        } else {
            KeyAction::Release {
                code,
                flags,
                stop_repeat: repeat == Repeat::Stop,
            }
        }
    }

    /// Takes the sender's depressed modifiers. Posts nothing: the flags
    /// apply to the next key event.
    pub(crate) fn set_modifiers(&mut self, depressed: u32) {
        self.mods = depressed;
    }

    pub(crate) fn flags(&self) -> u64 {
        cg_flags(self.mods)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::scancode::Linux;

    const MAC_A: u16 = 0x00;
    const MAC_B: u16 = 0x0B;
    const MAC_X: u16 = 0x07;
    const MAC_CONTROL: u16 = 0x3B;
    const MAC_OPTION: u16 = 0x3A;

    fn key(k: Linux) -> u32 {
        k as u32
    }

    #[test]
    fn mod5_is_option() {
        assert_eq!(cg_flags(MOD5), CG_ALTERNATE);
        assert_eq!(cg_flags(MOD1 | MOD5), CG_ALTERNATE);
        assert_eq!(
            cg_flags(SHIFT | CONTROL | MOD4),
            CG_SHIFT | CG_CONTROL | CG_COMMAND
        );
        assert_eq!(cg_flags(LOCK), CG_ALPHA_SHIFT);
        assert_eq!(cg_flags(0), 0);
    }

    #[test]
    fn modifiers_post_flags_changed_with_their_own_code() {
        let mut p = KeyPlanner::default();
        // Ctrl+Alt+X: two flag changes with the modifier key codes, then X
        // with both flags; no key-down for a modifier (the phantom 'A' was
        // a flags event built without a key code, i.e. code 0)
        assert_eq!(
            p.key(key(Linux::KeyLeftCtrl), MAC_CONTROL, true),
            KeyAction::FlagsChanged {
                code: MAC_CONTROL,
                pressed: true,
                flags: CG_CONTROL
            }
        );
        assert_eq!(
            p.key(key(Linux::KeyLeftAlt), MAC_OPTION, true),
            KeyAction::FlagsChanged {
                code: MAC_OPTION,
                pressed: true,
                flags: CG_CONTROL | CG_ALTERNATE
            }
        );
        assert_eq!(
            p.key(key(Linux::KeyX), MAC_X, true),
            KeyAction::Press {
                code: MAC_X,
                flags: CG_CONTROL | CG_ALTERNATE
            }
        );
        assert_eq!(
            p.key(key(Linux::KeyLeftCtrl), MAC_CONTROL, false),
            KeyAction::FlagsChanged {
                code: MAC_CONTROL,
                pressed: false,
                flags: CG_ALTERNATE
            }
        );
    }

    #[test]
    fn modifiers_never_repeat() {
        let mut p = KeyPlanner::default();
        p.key(key(Linux::KeyA), MAC_A, true);
        // holding shift must not stop or replace the repeating key
        p.key(key(Linux::KeyLeftShift), 0x38, true);
        p.key(key(Linux::KeyLeftShift), 0x38, false);
        assert!(matches!(
            p.key(key(Linux::KeyA), MAC_A, false),
            KeyAction::Release {
                stop_repeat: true,
                ..
            }
        ));
    }

    #[test]
    fn releasing_another_key_keeps_the_repeat() {
        let mut p = KeyPlanner::default();
        p.key(key(Linux::KeyA), MAC_A, true);
        p.key(key(Linux::KeyB), MAC_B, true);
        // A is released while B repeats: B must keep repeating
        assert_eq!(
            p.key(key(Linux::KeyA), MAC_A, false),
            KeyAction::Release {
                code: MAC_A,
                flags: 0,
                stop_repeat: false
            }
        );
        assert_eq!(
            p.key(key(Linux::KeyB), MAC_B, false),
            KeyAction::Release {
                code: MAC_B,
                flags: 0,
                stop_repeat: true
            }
        );
    }

    #[test]
    fn modifiers_event_only_updates_flags() {
        let mut p = KeyPlanner::default();
        p.set_modifiers(SHIFT | MOD5);
        assert_eq!(
            p.key(key(Linux::KeyA), MAC_A, true),
            KeyAction::Press {
                code: MAC_A,
                flags: CG_SHIFT | CG_ALTERNATE
            }
        );
        p.set_modifiers(0);
        assert_eq!(p.flags(), 0);
    }

    /// a remapped modifier (ctrl:nocaps): the sender reports Control while
    /// Caps Lock is held; releasing it must clear Control too
    #[test]
    fn remapped_modifier_release_clears_the_sender_mask() {
        let mut p = KeyPlanner::default();
        const MAC_CAPS: u16 = 0x39;
        p.key(key(Linux::KeyCapsLock), MAC_CAPS, true);
        p.set_modifiers(CONTROL);
        assert_eq!(p.flags(), CG_CONTROL);
        assert_eq!(
            p.key(key(Linux::KeyCapsLock), MAC_CAPS, false),
            KeyAction::FlagsChanged {
                code: MAC_CAPS,
                pressed: false,
                flags: 0
            }
        );
        assert_eq!(p.flags(), 0);
    }
}
