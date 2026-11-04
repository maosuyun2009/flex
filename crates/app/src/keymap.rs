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
    let key = named(&k.key).or_else(|| {
        let s = k.key_char.as_deref().unwrap_or(&k.key);
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Key::Char(c)),
            _ => None,
        }
    })?;
    Some(Press {
        key,
        ctrl: k.modifiers.control,
        alt: k.modifiers.alt,
        shift: k.modifiers.shift,
        meta: k.modifiers.platform,
    })
}

fn named(key: &str) -> Option<Key> {
    Some(match key {
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
