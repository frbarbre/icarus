//! The kitty keyboard protocol (https://sw.kovidgoyal.net/kitty/keyboard-protocol/), for programs
//! that turned it on with `CSI > flags u`. Only key presses are reported.

use gpui::Keystroke;

use crate::Modes;

pub const DISAMBIGUATE: u8 = 1;
pub const ALTERNATE_KEYS: u8 = 4;
pub const ALL_KEYS_AS_ESCAPES: u8 = 8;
pub const ASSOCIATED_TEXT: u8 = 16;

/// A functional key's number and final byte (`CSI number ; modifiers final`).
fn functional(key: &str) -> Option<(u32, char)> {
    Some(match key {
        "escape" => (27, 'u'),
        "enter" => (13, 'u'),
        "tab" => (9, 'u'),
        "backspace" => (127, 'u'),
        "insert" => (2, '~'),
        "delete" => (3, '~'),
        "pageup" => (5, '~'),
        "pagedown" => (6, '~'),
        "up" => (1, 'A'),
        "down" => (1, 'B'),
        "right" => (1, 'C'),
        "left" => (1, 'D'),
        "home" => (1, 'H'),
        "end" => (1, 'F'),
        "f1" => (1, 'P'),
        "f2" => (1, 'Q'),
        "f3" => (13, '~'),
        "f4" => (1, 'S'),
        "f5" => (15, '~'),
        "f6" => (17, '~'),
        "f7" => (18, '~'),
        "f8" => (19, '~'),
        "f9" => (20, '~'),
        "f10" => (21, '~'),
        "f11" => (23, '~'),
        "f12" => (24, '~'),
        other => {
            let number: u32 = other.strip_prefix('f')?.parse().ok()?;
            if !(13..=35).contains(&number) {
                return None;
            }
            (57376 + number - 13, 'u')
        }
    })
}

fn modifier_bits(keystroke: &Keystroke) -> u32 {
    let modifiers = &keystroke.modifiers;
    u32::from(modifiers.shift)
        | u32::from(modifiers.alt) << 1
        | u32::from(modifiers.control) << 2
        | u32::from(modifiers.platform) << 3
}

fn text_key(keystroke: &Keystroke) -> Option<char> {
    if keystroke.key == "space" {
        return Some(' ');
    }
    let mut chars = keystroke.key.chars();
    let key = chars.next()?;
    chars
        .next()
        .is_none()
        .then(|| key.to_lowercase().next().unwrap_or(key))
}

fn sequence(
    code: u32,
    shifted: Option<char>,
    modifiers: u32,
    text: Option<&str>,
    last: char,
) -> String {
    let mut out = format!("\x1b[{code}");
    if let Some(shifted) = shifted {
        out.push_str(&format!(":{}", u32::from(shifted)));
    }
    let text: Option<String> = text
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .map(|text| {
            text.chars()
                .map(|c| u32::from(c).to_string())
                .collect::<Vec<_>>()
                .join(":")
        });
    if modifiers > 0 || text.is_some() {
        out.push_str(&format!(";{}", modifiers + 1));
    }
    if let Some(text) = text {
        out.push_str(&format!(";{text}"));
    }
    out.push(last);
    out
}

/// The bytes for `keystroke` under `flags`, or `None` when the key is sent the legacy way (plain
/// text, an unmodified Enter, Tab, Backspace or arrow).
pub fn encode(
    keystroke: &Keystroke,
    flags: u8,
    mode: Modes,
    option_as_meta: bool,
) -> Option<String> {
    if flags & (DISAMBIGUATE | ALL_KEYS_AS_ESCAPES) == 0 {
        return None;
    }
    let all_keys = flags & ALL_KEYS_AS_ESCAPES != 0;
    let mut modifiers = modifier_bits(keystroke);
    if let Some((number, last)) = functional(&keystroke.key) {
        let legacy_ok = !all_keys && modifiers == 0 && keystroke.key != "escape";
        if legacy_ok {
            return None;
        }
        // Unmodified cursor keys keep their DECCKM form; the program asked for nothing else.
        if !all_keys
            && modifiers == 0
            && last != 'u'
            && last != '~'
            && mode.contains(Modes::APP_CURSOR)
        {
            return None;
        }
        return Some(if number == 1 && modifiers == 0 {
            format!("\x1b[{last}")
        } else {
            sequence(number, None, modifiers, None, last)
        });
    }
    let key = text_key(keystroke)?;
    let composes = cfg!(target_os = "macos")
        && !option_as_meta
        && keystroke.modifiers.alt
        && !keystroke.modifiers.control
        && !keystroke.modifiers.platform;
    if composes {
        // Option types its own character on macOS unless it's Meta; that's text, not a key.
        if !all_keys {
            return None;
        }
        modifiers &= !0b10;
    }
    if !all_keys && modifiers & !1 == 0 {
        return None;
    }
    let shifted = (flags & ALTERNATE_KEYS != 0 && keystroke.modifiers.shift)
        .then(|| key.to_uppercase().next())
        .flatten()
        .filter(|shifted| *shifted != key);
    let text = (all_keys && flags & ASSOCIATED_TEXT != 0 && modifiers & !1 == 0)
        .then_some(keystroke.key_char.as_deref())
        .flatten();
    Some(sequence(u32::from(key), shifted, modifiers, text, 'u'))
}

/// Typed text when the program wants every key as an escape (`ALL_KEYS_AS_ESCAPES`).
pub fn encode_text(text: &str, flags: u8) -> Option<String> {
    if flags & ALL_KEYS_AS_ESCAPES == 0 {
        return None;
    }
    let mut out = String::new();
    for c in text.chars() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        let shift = c.is_uppercase() && lower != c;
        let shifted = (shift && flags & ALTERNATE_KEYS != 0).then_some(c);
        let mut buffer = [0; 4];
        let text = (flags & ASSOCIATED_TEXT != 0).then(|| &*c.encode_utf8(&mut buffer));
        out.push_str(&sequence(
            u32::from(lower),
            shifted,
            u32::from(shift),
            text,
            'u',
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &'static str) -> Keystroke {
        Keystroke::parse(text).expect("keystroke")
    }

    fn enc(text: &'static str, flags: u8) -> Option<String> {
        encode(&key(text), flags, Modes::NONE, true)
    }

    #[test]
    fn disambiguate_encodes_modified_and_ambiguous_keys_only() {
        assert_eq!(enc("a", 1), None);
        assert_eq!(enc("shift-a", 1), None);
        assert_eq!(enc("enter", 1), None);
        assert_eq!(enc("backspace", 1), None);
        assert_eq!(enc("up", 1), None);
        assert_eq!(enc("escape", 1).as_deref(), Some("\x1b[27u"));
        assert_eq!(enc("ctrl-a", 1).as_deref(), Some("\x1b[97;5u"));
        assert_eq!(enc("ctrl-shift-a", 1).as_deref(), Some("\x1b[97;6u"));
        assert_eq!(enc("alt-backspace", 1).as_deref(), Some("\x1b[127;3u"));
        assert_eq!(enc("shift-enter", 1).as_deref(), Some("\x1b[13;2u"));
        assert_eq!(enc("ctrl-i", 1).as_deref(), Some("\x1b[105;5u"));
        assert_eq!(enc("alt-left", 1).as_deref(), Some("\x1b[1;3D"));
        assert_eq!(enc("alt-delete", 1).as_deref(), Some("\x1b[3;3~"));
        assert_eq!(enc("shift-f3", 1).as_deref(), Some("\x1b[13;2~"));
        assert_eq!(enc("cmd-k", 1).as_deref(), Some("\x1b[107;9u"));
        assert_eq!(enc("ctrl-space", 1).as_deref(), Some("\x1b[32;5u"));
        assert_eq!(enc("f13", 1), None);
        assert_eq!(enc("shift-f13", 1).as_deref(), Some("\x1b[57376;2u"));
    }

    #[test]
    fn report_all_keys_turns_text_and_enter_into_escapes() {
        assert_eq!(enc("a", 8).as_deref(), Some("\x1b[97u"));
        assert_eq!(enc("shift-a", 8).as_deref(), Some("\x1b[97;2u"));
        assert_eq!(enc("shift-a", 8 | 4).as_deref(), Some("\x1b[97:65;2u"));
        assert_eq!(enc("enter", 8).as_deref(), Some("\x1b[13u"));
        assert_eq!(enc("up", 8).as_deref(), Some("\x1b[A"));
        let mut a = key("a");
        a.key_char = Some("a".into());
        assert_eq!(
            encode(&a, 8 | 16, Modes::NONE, true).as_deref(),
            Some("\x1b[97;1;97u")
        );
        assert_eq!(encode_text("aB", 8).as_deref(), Some("\x1b[97u\x1b[98;2u"));
        assert_eq!(encode_text("a", 1), None);
    }

    #[test]
    fn unmodified_cursor_keys_keep_application_mode() {
        assert_eq!(encode(&key("up"), 1, Modes::APP_CURSOR, true), None);
        assert_eq!(
            encode(&key("ctrl-up"), 1, Modes::APP_CURSOR, true).as_deref(),
            Some("\x1b[1;5A")
        );
    }

    #[test]
    fn option_composes_text_on_macos_unless_it_is_meta() {
        let composed = encode(&key("alt-e"), 1, Modes::NONE, false);
        if cfg!(target_os = "macos") {
            assert_eq!(composed, None);
        } else {
            assert_eq!(composed.as_deref(), Some("\x1b[101;3u"));
        }
        assert_eq!(enc("alt-e", 1).as_deref(), Some("\x1b[101;3u"));
    }
}
