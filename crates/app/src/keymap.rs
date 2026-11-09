//! GPUI keystroke → the key a Rex terminal expects.

use gpui::Keystroke;
use shrew_core::Key;

/// The key plus its modifiers, in the order `TerminalSession::send_key` wants.
pub struct Press {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

pub fn translate(k: &Keystroke) -> Option<Press> {
    let mut key = named(&k.key).or_else(|| {
        let s = k.key_char.as_deref().unwrap_or(&k.key);
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Key::Char(c)),
            _ => None,
        }
    })?;
    // Some IMEs report the unshifted letter in key_char; Shift still means uppercase.
    if k.modifiers.shift && !k.modifiers.control && !k.modifiers.alt && !k.modifiers.platform {
        if let Key::Char(c) = key {
            if c.is_ascii_lowercase() {
                key = Key::Char(c.to_ascii_uppercase());
            }
        }
    }
    Some(Press {
        key,
        ctrl: k.modifiers.control,
        alt: k.modifiers.alt,
        shift: k.modifiers.shift,
        meta: k.modifiers.platform,
    })
}

impl Press {
    /// Printable text goes to the pty as text; key encoding is for control keys.
    pub fn typed_text(&self) -> Option<char> {
        if self.ctrl || self.alt || self.meta {
            return None;
        }
        match self.key {
            Key::Char(c) if !c.is_control() => Some(c),
            _ => None,
        }
    }
}

pub fn binding_key(k: &Keystroke) -> String {
    let mut keys = Vec::new();
    if k.modifiers.control {
        keys.push("ctrl");
    }
    if k.modifiers.alt {
        keys.push("alt");
    }
    if k.modifiers.shift {
        keys.push("shift");
    }
    if k.modifiers.platform {
        keys.push("cmd");
    }
    keys.push(&k.key);
    flex_engine::keybindings::normalize(&keys.join("+"))
}

fn named(key: &str) -> Option<Key> {
    Some(match key {
        // Windows reports the space bar as a named key with no character, so
        // without this arm it was dropped and terminals could not type a space.
        "space" => Key::Char(' '),
        "enter" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "delete" => Key::Delete,
        "insert" => Key::Insert,
        other => {
            // A numeric keypad reports "numpad7" and friends.
            if let Some(rest) = other.strip_prefix("numpad") {
                let mut chars = rest.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    return Some(Key::Char(c));
                }
            }
            let n = other.strip_prefix('f')?.parse::<u8>().ok()?;
            if !(1..=24).contains(&n)
                || other.len() < 2
                || !other[1..].chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            Key::F(n)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

    fn stroke(key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: key_char.map(str::to_owned),
            modifiers: Modifiers::default(),
        }
    }

    /// Windows sends the space bar as `key = "space"`, `key_char = None`.
    #[test]
    fn space_types_a_space() {
        let press = translate(&stroke("space", None)).expect("space was dropped");
        assert_eq!(press.key, Key::Char(' '));
        assert!(!press.ctrl && !press.alt && !press.shift && !press.meta);
    }

    #[test]
    fn letters_and_punctuation_come_through() {
        assert_eq!(
            translate(&stroke("a", Some("a"))).unwrap().key,
            Key::Char('a')
        );
        assert_eq!(
            translate(&stroke("period", Some("."))).unwrap().key,
            Key::Char('.')
        );
        assert_eq!(
            translate(&stroke("numpad7", None)).unwrap().key,
            Key::Char('7')
        );
        assert_eq!(translate(&stroke("f5", None)).unwrap().key, Key::F(5));
    }

    #[test]
    fn printable_characters_are_text_and_modified_keys_are_not() {
        let mut press = stroke("c", Some("c"));
        press.modifiers.shift = true;
        assert_eq!(translate(&press).unwrap().typed_text(), Some('C'));
        assert_eq!(translate(&stroke("c", Some("c"))).unwrap().typed_text(), Some('c'));
        assert_eq!(translate(&stroke("space", None)).unwrap().typed_text(), Some(' '));
        let mut ctrl = stroke("c", Some("c"));
        ctrl.modifiers.control = true;
        assert_eq!(translate(&ctrl).unwrap().typed_text(), None);
        assert_eq!(translate(&stroke("enter", None)).unwrap().typed_text(), None);
    }

    #[test]
    fn shift_with_a_lowercase_ime_echo_still_types_uppercase() {
        let mut press = stroke("c", Some("c"));
        press.modifiers.shift = true;
        let press = translate(&press).unwrap();
        assert_eq!(press.key, Key::Char('C'));
        assert!(press.shift);
    }

    /// A bare modifier must not be sent as text.
    #[test]
    fn modifiers_are_not_keys() {
        assert!(translate(&stroke("shift", None)).is_none());
        assert!(translate(&stroke("control", None)).is_none());
    }
}
