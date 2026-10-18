//! Key chords as agents write them: `ctrl+l`, `shift+tab`, `super`, `f5`.

use input_event::scancode::Linux;

/// Parses `chord` (names joined by `+`, case-insensitive) into the keys to
/// press in order. Besides the friendly names, every evdev name works as
/// spelled in the config (`KeyLeftCtrl`).
pub fn parse_chord(chord: &str) -> Result<Vec<Linux>, String> {
    let keys = chord
        .split('+')
        .map(|name| {
            let name = name.trim();
            key(name).ok_or_else(|| format!("unknown key `{name}` in `{chord}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() || chord.trim().is_empty() {
        return Err("empty key chord".into());
    }
    Ok(keys)
}

fn key(name: &str) -> Option<Linux> {
    if name.is_empty() {
        return None;
    }
    // evdev names as in the config (`KeyLeftCtrl`)
    if let Ok(key) = serde_json::from_value(serde_json::Value::String(name.to_owned())) {
        return Some(key);
    }
    let lower = name.to_ascii_lowercase();
    let mut chars = lower.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(key) = single(c) {
            return Some(key);
        }
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return function(n);
    }
    Some(match lower.as_str() {
        "ctrl" | "control" | "lctrl" => Linux::KeyLeftCtrl,
        "rctrl" => Linux::KeyRightCtrl,
        "shift" | "lshift" => Linux::KeyLeftShift,
        "rshift" => Linux::KeyRightShift,
        "alt" | "option" | "opt" | "lalt" => Linux::KeyLeftAlt,
        "altgr" | "ralt" => Linux::KeyRightalt,
        "super" | "meta" | "win" | "windows" | "cmd" | "command" => Linux::KeyLeftMeta,
        "enter" | "return" => Linux::KeyEnter,
        "esc" | "escape" => Linux::KeyEsc,
        "tab" => Linux::KeyTab,
        "space" => Linux::KeySpace,
        "backspace" => Linux::KeyBackspace,
        "delete" | "del" => Linux::KeyDelete,
        "insert" | "ins" => Linux::KeyInsert,
        "home" => Linux::KeyHome,
        "end" => Linux::KeyEnd,
        "pageup" | "pgup" => Linux::KeyPageup,
        "pagedown" | "pgdn" => Linux::KeyPagedown,
        "up" => Linux::KeyUp,
        "down" => Linux::KeyDown,
        "left" => Linux::KeyLeft,
        "right" => Linux::KeyRight,
        "capslock" => Linux::KeyCapsLock,
        "printscreen" | "print" | "sysrq" => Linux::KeySysrq,
        "pause" => Linux::KeyPause,
        "menu" | "compose" => Linux::KeyCompose,
        "minus" => Linux::KeyMinus,
        "equal" | "equals" => Linux::KeyEqual,
        "comma" => Linux::KeyComma,
        "period" | "dot" => Linux::KeyDot,
        "slash" => Linux::KeySlash,
        "backslash" => Linux::KeyBackslash,
        "semicolon" => Linux::KeySemicolon,
        "apostrophe" | "quote" => Linux::KeyApostrophe,
        "grave" | "backtick" => Linux::KeyGrave,
        "bracketleft" | "lbracket" => Linux::KeyLeftbrace,
        "bracketright" | "rbracket" => Linux::KeyRightbrace,
        _ => return None,
    })
}

/// a letter, digit or US-layout punctuation key by its unshifted character
fn single(c: char) -> Option<Linux> {
    const LETTERS: [Linux; 26] = [
        Linux::KeyA,
        Linux::KeyB,
        Linux::KeyC,
        Linux::KeyD,
        Linux::KeyE,
        Linux::KeyF,
        Linux::KeyG,
        Linux::KeyH,
        Linux::KeyI,
        Linux::KeyJ,
        Linux::KeyK,
        Linux::KeyL,
        Linux::KeyM,
        Linux::KeyN,
        Linux::KeyO,
        Linux::KeyP,
        Linux::KeyQ,
        Linux::KeyR,
        Linux::KeyS,
        Linux::KeyT,
        Linux::KeyU,
        Linux::KeyV,
        Linux::KeyW,
        Linux::KeyX,
        Linux::KeyY,
        Linux::KeyZ,
    ];
    const DIGITS: [Linux; 10] = [
        Linux::Key0,
        Linux::Key1,
        Linux::Key2,
        Linux::Key3,
        Linux::Key4,
        Linux::Key5,
        Linux::Key6,
        Linux::Key7,
        Linux::Key8,
        Linux::Key9,
    ];
    Some(match c {
        'a'..='z' => LETTERS[c as usize - 'a' as usize],
        '0'..='9' => DIGITS[c as usize - '0' as usize],
        '-' => Linux::KeyMinus,
        '=' => Linux::KeyEqual,
        ',' => Linux::KeyComma,
        '.' => Linux::KeyDot,
        '/' => Linux::KeySlash,
        '\\' => Linux::KeyBackslash,
        ';' => Linux::KeySemicolon,
        '\'' => Linux::KeyApostrophe,
        '`' => Linux::KeyGrave,
        '[' => Linux::KeyLeftbrace,
        ']' => Linux::KeyRightbrace,
        _ => return None,
    })
}

fn function(n: u8) -> Option<Linux> {
    const KEYS: [Linux; 12] = [
        Linux::KeyF1,
        Linux::KeyF2,
        Linux::KeyF3,
        Linux::KeyF4,
        Linux::KeyF5,
        Linux::KeyF6,
        Linux::KeyF7,
        Linux::KeyF8,
        Linux::KeyF9,
        Linux::KeyF10,
        Linux::KeyF11,
        Linux::KeyF12,
    ];
    KEYS.get(usize::from(n).checked_sub(1)?).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_names_parse_case_insensitively_in_order() {
        assert_eq!(
            parse_chord("Ctrl+Shift+T").unwrap(),
            [Linux::KeyLeftCtrl, Linux::KeyLeftShift, Linux::KeyT]
        );
        assert_eq!(
            parse_chord("cmd + ]").unwrap(),
            [Linux::KeyLeftMeta, Linux::KeyRightbrace]
        );
        assert_eq!(parse_chord("f12").unwrap(), [Linux::KeyF12]);
        assert_eq!(
            parse_chord("KeyLeftCtrl+KeyL").unwrap(),
            [Linux::KeyLeftCtrl, Linux::KeyL]
        );
    }

    #[test]
    fn unknown_or_empty_keys_are_refused() {
        assert!(parse_chord("ctrl+wobble").is_err());
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("").is_err());
        assert!(parse_chord("f13").is_err());
        assert!(parse_chord("f0").is_err());
    }
}
