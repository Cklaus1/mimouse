//! Typing text: the characters an agent types and, for backends that type
//! with key events, the keys that produce a character in the target's
//! keymap.

/// The characters of `text` to type: `"\r\n"` and a lone `'\r'` are one
/// newline.
pub(crate) fn chars(text: &str) -> impl Iterator<Item = char> + '_ {
    let mut chars = text.chars().peekable();
    std::iter::from_fn(move || {
        let c = chars.next()?;
        if c == '\r' {
            chars.next_if_eq(&'\n');
            return Some('\n');
        }
        Some(c)
    })
}

/// The evdev keys that type one character: `modifiers` (0 = unused slot)
/// are held around `key`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Keystroke {
    pub(crate) modifiers: [u32; 3],
    pub(crate) key: u32,
    /// The depressed modifier mask the `modifiers` make, for backends
    /// whose modifier state is set apart from keys (the wlroots virtual
    /// keyboard).
    pub(crate) depressed: u32,
}

impl Keystroke {
    /// the modifiers to hold, in press order
    pub(crate) fn modifiers(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.modifiers.iter().copied().filter(|&m| m != 0)
    }
}

/// The keysym that types `c`: a newline is Return (X11 maps it to
/// Linefeed, which applications do not take as Enter).
#[cfg(any(libei, wlroots, x11, rdp))]
pub(crate) fn keysym(c: char) -> Option<xkeysym::Keysym> {
    let keysym = match c {
        '\n' => xkeysym::Keysym::Return,
        c => xkeysym::Keysym::from_char(c),
    };
    (keysym != xkeysym::Keysym::NoSymbol).then_some(keysym)
}

/// A piece of text typed exactly (as Unicode, not through a layout).
#[cfg(any(windows, target_os = "macos", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    /// UTF-16 units of whole characters
    Units(Vec<u16>),
    /// a newline, as the Return key: applications take no newline
    /// character for Enter
    Return,
    Tab,
}

/// The pieces that type `text`, each at most `max_units` UTF-16 units
/// (at least one character), and the characters that are not typed:
/// control characters other than newline and tab.
#[cfg(any(windows, target_os = "macos", test))]
pub(crate) fn exact_pieces(text: &str, max_units: usize) -> (Vec<Piece>, String) {
    let mut pieces = Vec::new();
    let mut unmapped = String::new();
    let mut units: Vec<u16> = Vec::new();
    for c in chars(text) {
        let key = match c {
            '\n' => Piece::Return,
            '\t' => Piece::Tab,
            c if c.is_control() => {
                unmapped.push(c);
                continue;
            }
            c => {
                if !units.is_empty() && units.len() + c.len_utf16() > max_units {
                    pieces.push(Piece::Units(std::mem::take(&mut units)));
                }
                units.extend(c.encode_utf16(&mut [0; 2]).iter());
                continue;
            }
        };
        if !units.is_empty() {
            pieces.push(Piece::Units(std::mem::take(&mut units)));
        }
        pieces.push(key);
    }
    if !units.is_empty() {
        pieces.push(Piece::Units(units));
    }
    (pieces, unmapped)
}

#[cfg(test)]
mod exact_tests {
    use super::{Piece, exact_pieces};

    fn units(s: &str) -> Piece {
        Piece::Units(s.encode_utf16().collect())
    }

    #[test]
    fn keys_split_the_text_and_controls_are_not_typed() {
        let (pieces, unmapped) = exact_pieces("ab\r\n\tc\u{7}d\re", 20);
        let expected = [
            units("ab"),
            Piece::Return,
            Piece::Tab,
            units("cd"),
            Piece::Return,
            units("e"),
        ];
        assert_eq!(pieces, expected);
        assert_eq!(unmapped, "\u{7}");
    }

    #[test]
    fn pieces_hold_whole_characters_within_the_limit() {
        // '😀' is two UTF-16 units, never split
        let (pieces, _) = exact_pieces("a😀b😀", 2);
        assert_eq!(pieces, [units("a"), units("😀"), units("b"), units("😀")]);
        let (pieces, _) = exact_pieces("abc", 1);
        assert_eq!(pieces, [units("a"), units("b"), units("c")]);
        assert_eq!(exact_pieces("", 20), (vec![], String::new()));
    }
}

#[cfg(any(libei, wlroots))]
pub(crate) use self::layout::Layout;

#[cfg(any(libei, wlroots))]
mod layout {
    use std::collections::HashMap;
    use std::os::raw::c_char;

    use xkbcommon_dl::{
        XkbCommon, xkb_context_flags, xkb_key_direction, xkb_keymap, xkb_keymap_compile_flags,
        xkb_keymap_format, xkb_log_level, xkb_state, xkb_state_component, xkbcommon_option,
    };
    use xkeysym::Keysym;

    use super::{Keystroke, keysym};

    /// xkb keycodes are evdev codes + 8
    const EVDEV_OFFSET: u32 = 8;
    /// the highest keycode X11 carries
    const X11_MAX_KEYCODE: u32 = 255;

    /// Modifier keys pressed to reach a level; Control, Alt and Super are
    /// never used, they would trigger shortcuts.
    const LEVEL_MODIFIERS: [Keysym; 3] = [
        Keysym::Shift_L,
        Keysym::ISO_Level3_Shift,
        Keysym::ISO_Level5_Shift,
    ];

    /// Which keys type which keysym in the first layout of a keymap,
    /// without Lock (Caps Lock) and with as few modifiers as possible.
    #[derive(Debug, Default)]
    pub(crate) struct Layout {
        pub(super) keys: HashMap<u32, Keystroke>,
    }

    impl Layout {
        /// Reads an XKB text keymap as the compositor or EIS server sends
        /// it (trailing NULs allowed). `None` if libxkbcommon is not
        /// installed (it is loaded at runtime, never linked) or the keymap
        /// does not compile.
        pub(crate) fn new(keymap: &[u8]) -> Option<Layout> {
            let xkb = xkbcommon_option()?;
            let end = keymap.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            let keymap = &keymap[..end];
            // SAFETY: each pointer is checked for null and released by
            // `Owned` after everything made from it
            unsafe {
                let context = Owned::new(
                    (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_DEFAULT_INCLUDES),
                    *xkb.xkb_context_unref,
                )?;
                (xkb.xkb_context_set_log_level)(context.0, xkb_log_level::XKB_LOG_LEVEL_CRITICAL);
                let keymap = Owned::new(
                    (xkb.xkb_keymap_new_from_buffer)(
                        context.0,
                        keymap.as_ptr().cast::<c_char>(),
                        keymap.len(),
                        xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                        xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
                    ),
                    *xkb.xkb_keymap_unref,
                )?;
                Some(Self::read(xkb, keymap.0))
            }
        }

        /// # Safety
        /// `keymap` is a valid keymap.
        unsafe fn read(xkb: &XkbCommon, keymap: *mut xkb_keymap) -> Layout {
            let (min, max) = unsafe {
                (
                    (xkb.xkb_keymap_min_keycode)(keymap).max(EVDEV_OFFSET + 1),
                    (xkb.xkb_keymap_max_keycode)(keymap),
                )
            };
            let new_state =
                || unsafe { Owned::new((xkb.xkb_state_new)(keymap), *xkb.xkb_state_unref) };
            let Some(base) = new_state() else {
                return Layout::default();
            };
            let one_sym = |state: &Owned<xkb_state>, code: u32| unsafe {
                Keysym::new((xkb.xkb_state_key_get_one_sym)(state.0, code))
            };
            // the first key with each level modifier, by keycode
            let mut modifiers = [None; LEVEL_MODIFIERS.len()];
            for code in min..=max {
                let sym = one_sym(&base, code);
                if let Some(i) = LEVEL_MODIFIERS.iter().position(|&m| m == sym) {
                    modifiers[i].get_or_insert(code);
                }
            }
            let is_modifier = |code| modifiers.contains(&Some(code));

            let mut keys = HashMap::new();
            // combinations of the level modifiers, fewest first
            let mut combinations: Vec<u32> = (0..1 << LEVEL_MODIFIERS.len()).collect();
            combinations.sort_by_key(|c| c.count_ones());
            // keys X11 can carry (Xwayland clients) before any above them
            let x11_max = max.min(X11_MAX_KEYCODE);
            for codes in [min..=x11_max, (x11_max + 1).max(min)..=max] {
                'combinations: for &combination in &combinations {
                    let mut stroke = Keystroke::default();
                    let Some(state) = new_state() else {
                        break;
                    };
                    let mut slot = 0;
                    for (i, modifier) in modifiers.iter().enumerate() {
                        if combination & (1 << i) == 0 {
                            continue;
                        }
                        let Some(code) = modifier else {
                            continue 'combinations;
                        };
                        unsafe {
                            (xkb.xkb_state_update_key)(
                                state.0,
                                *code,
                                xkb_key_direction::XKB_KEY_DOWN,
                            );
                        }
                        stroke.modifiers[slot] = code - EVDEV_OFFSET;
                        slot += 1;
                    }
                    stroke.depressed = unsafe {
                        (xkb.xkb_state_serialize_mods)(
                            state.0,
                            xkb_state_component::XKB_STATE_MODS_DEPRESSED,
                        )
                    };
                    for code in codes.clone().filter(|&code| !is_modifier(code)) {
                        let sym = one_sym(&state, code);
                        if sym != Keysym::NoSymbol {
                            keys.entry(sym.raw()).or_insert(Keystroke {
                                key: code - EVDEV_OFFSET,
                                ..stroke
                            });
                        }
                    }
                }
            }
            Layout { keys }
        }

        /// the keys that type `c`, `None` if the layout has no key for it
        pub(crate) fn keystroke(&self, c: char) -> Option<Keystroke> {
            self.keys.get(&keysym(c)?.raw()).copied()
        }
    }

    /// an xkbcommon object, unreferenced on drop
    struct Owned<T>(*mut T, unsafe extern "C" fn(*mut T));

    impl<T> Owned<T> {
        /// `None` for a null pointer
        fn new(ptr: *mut T, unref: unsafe extern "C" fn(*mut T)) -> Option<Self> {
            (!ptr.is_null()).then_some(Self(ptr, unref))
        }
    }

    impl<T> Drop for Owned<T> {
        fn drop(&mut self) {
            // SAFETY: xkbcommon returned the pointer with a reference this
            // value owns
            unsafe { (self.1)(self.0) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_returns_are_newlines() {
        let typed: String = chars("a\r\nb\rc\n\r\n").collect();
        assert_eq!(typed, "a\nb\nc\n\n");
    }

    #[test]
    fn keystroke_modifiers_skip_unused_slots() {
        let stroke = Keystroke {
            modifiers: [42, 0, 0],
            key: 30,
            depressed: 1,
        };
        assert_eq!(stroke.modifiers().collect::<Vec<_>>(), [42]);
        assert_eq!(Keystroke::default().modifiers().count(), 0);
    }

    #[cfg(any(libei, wlroots, x11, rdp))]
    #[test]
    fn newlines_are_return() {
        use xkeysym::Keysym;
        assert_eq!(keysym('\n'), Some(Keysym::Return));
        assert_eq!(keysym('\t'), Some(Keysym::Tab));
        assert_eq!(keysym('a'), Some(Keysym::a));
        assert_eq!(keysym('€'), Some(Keysym::EuroSign));
        // a lone surrogate cannot reach a `char`; a noncharacter has no keysym
        assert_eq!(keysym('\u{fffe}'), None);
    }

    #[cfg(any(libei, wlroots))]
    mod layout {
        use super::super::*;

        /// a complete keymap as servers send it: Shift, AltGr (Level3) on
        /// RALT, a FOUR_LEVEL key, an ALPHABETIC one and Control
        const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "test" {
    minimum = 8;
    maximum = 708;
    <AE01> = 10;
    <TAB> = 23;
    <AD03> = 26;
    <RTRN> = 36;
    <LCTL> = 37;
    <AC01> = 38;
    <LFSH> = 50;
    <AB01> = 52;
    <SPCE> = 65;
    <RALT> = 108;
    <I435> = 443;
    <I436> = 444;
};
xkb_types "test" {
    virtual_modifiers LevelThree;
    type "ONE_LEVEL" {
        modifiers = none;
        level_name[Level1] = "Any";
    };
    type "TWO_LEVEL" {
        modifiers = Shift;
        map[Shift] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Shift";
    };
    type "ALPHABETIC" {
        modifiers = Shift+Lock;
        map[Shift] = Level2;
        map[Lock] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Caps";
    };
    type "FOUR_LEVEL" {
        modifiers = Shift+LevelThree;
        map[None] = Level1;
        map[Shift] = Level2;
        map[LevelThree] = Level3;
        map[Shift+LevelThree] = Level4;
        level_name[Level1] = "Base";
        level_name[Level2] = "Shift";
        level_name[Level3] = "Alt Base";
        level_name[Level4] = "Shift Alt";
    };
};
xkb_compatibility "test" {
    virtual_modifiers LevelThree;
    interpret Shift_L { action = SetMods(modifiers=Shift); };
    interpret Control_L { action = SetMods(modifiers=Control); };
    interpret ISO_Level3_Shift {
        useModMapMods = level1;
        virtualModifier = LevelThree;
        action = SetMods(modifiers=LevelThree);
    };
};
xkb_symbols "test" {
    key <AE01> { [ 1, exclam ] };
    key <TAB> { [ Tab, ISO_Left_Tab ] };
    key <AD03> { type = "FOUR_LEVEL", [ e, E, EuroSign, cent ] };
    key <RTRN> { [ Return ] };
    key <LCTL> { [ Control_L ] };
    key <AC01> { type = "ALPHABETIC", [ a, A ] };
    key <LFSH> { [ Shift_L ] };
    key <AB01> { type = "TWO_LEVEL", [ z, Z ] };
    key <SPCE> { [ space ] };
    key <RALT> { type = "ONE_LEVEL", [ ISO_Level3_Shift ] };
    key <I435> { [ EuroSign ] };
    key <I436> { [ ssharp ] };
    modifier_map Shift { <LFSH> };
    modifier_map Control { <LCTL> };
    modifier_map Mod5 { <RALT> };
};
};
"#;

        const KEY_LEFTSHIFT: u32 = 42;
        const KEY_RIGHTALT: u32 = 100;
        /// real modifier masks: Shift, and Mod5 that LevelThree maps to
        const SHIFT: u32 = 1 << 0;
        const MOD5: u32 = 1 << 7;

        fn stroke(modifiers: &[u32], key: u32) -> Option<Keystroke> {
            let mut stroke = Keystroke {
                key,
                ..Default::default()
            };
            stroke.modifiers[..modifiers.len()].copy_from_slice(modifiers);
            for modifier in modifiers {
                stroke.depressed |= match *modifier {
                    KEY_LEFTSHIFT => SHIFT,
                    KEY_RIGHTALT => MOD5,
                    _ => unreachable!(),
                };
            }
            Some(stroke)
        }

        /// the layout of `keymap`, `None` (test skipped) without
        /// libxkbcommon
        fn layout(keymap: &[u8]) -> Option<Layout> {
            if xkbcommon_dl::xkbcommon_option().is_none() {
                eprintln!("libxkbcommon is not installed: skipping");
                return None;
            }
            Some(Layout::new(keymap).expect("the keymap compiles"))
        }

        #[test]
        fn characters_map_to_their_level() {
            let Some(layout) = layout(KEYMAP.as_bytes()) else {
                return;
            };
            assert_eq!(layout.keystroke('1'), stroke(&[], 2));
            assert_eq!(layout.keystroke('!'), stroke(&[KEY_LEFTSHIFT], 2));
            assert_eq!(layout.keystroke('a'), stroke(&[], 30));
            assert_eq!(layout.keystroke('A'), stroke(&[KEY_LEFTSHIFT], 30));
            assert_eq!(layout.keystroke('e'), stroke(&[], 18));
            assert_eq!(layout.keystroke('E'), stroke(&[KEY_LEFTSHIFT], 18));
            // a key X11 cannot carry only when no other types it
            assert_eq!(layout.keystroke('€'), stroke(&[KEY_RIGHTALT], 18));
            assert_eq!(layout.keystroke('ß'), stroke(&[], 436));
            assert_eq!(
                layout.keystroke('¢'),
                stroke(&[KEY_LEFTSHIFT, KEY_RIGHTALT], 18)
            );
            assert_eq!(layout.keystroke(' '), stroke(&[], 57));
            assert_eq!(layout.keystroke('\n'), stroke(&[], 28));
            assert_eq!(layout.keystroke('\t'), stroke(&[], 15));
        }

        #[test]
        fn characters_without_a_key_are_unmapped() {
            let Some(layout) = layout(KEYMAP.as_bytes()) else {
                return;
            };
            for c in ['b', 'ä', '😀', '\u{7}'] {
                assert_eq!(layout.keystroke(c), None, "{c:?}");
            }
            // the level modifiers type nothing themselves
            let modifier_keys = [KEY_LEFTSHIFT, KEY_RIGHTALT];
            assert!(
                layout
                    .keys
                    .values()
                    .all(|k| !modifier_keys.contains(&k.key)),
                "{:?}",
                layout.keys
            );
        }

        #[test]
        fn trailing_nuls_are_ignored_and_bad_keymaps_rejected() {
            if xkbcommon_dl::xkbcommon_option().is_none() {
                eprintln!("libxkbcommon is not installed: skipping");
                return;
            }
            let mut sent = KEYMAP.as_bytes().to_vec();
            sent.extend([0, 0]);
            assert!(Layout::new(&sent).is_some());
            assert!(Layout::new(b"xkb_keymap { us };").is_none());
            assert!(Layout::new(b"").is_none());
        }
    }
}
