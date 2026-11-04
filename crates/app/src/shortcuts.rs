//! The shell's own keys.
//!
//! One table drives both the matcher and the settings page, so the list a user
//! reads is the list that works. A pane checks it before sending a keystroke to
//! the server, and the workspace root acts on whatever the pane let through.

use gpui::Keystroke;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    SplitRight,
    SplitDown,
    ClosePane,
    ToggleFloat,
    NewTab,
    ToggleSidebar,
    Settings,
}

/// `(keys, action, what it does)`. Keep the key order stable; the settings page
/// prints this as-is.
pub const BINDINGS: &[(&str, Action, &str)] = &[
    (
        "ctrl-shift-d",
        Action::SplitRight,
        "Split the pane to the right",
    ),
    (
        "ctrl-shift-e",
        Action::SplitDown,
        "Split the pane downwards",
    ),
    ("ctrl-shift-w", Action::ClosePane, "Close the pane"),
    (
        "ctrl-shift-f",
        Action::ToggleFloat,
        "Float the pane, or tile it again",
    ),
    ("ctrl-shift-t", Action::NewTab, "Open a new tab"),
    ("ctrl-b", Action::ToggleSidebar, "Show or hide the sidebar"),
    ("ctrl-,", Action::Settings, "Settings"),
];

pub fn lookup(keystroke: &Keystroke) -> Option<Action> {
    let key = keystroke.key.to_ascii_lowercase();
    let modifiers = &keystroke.modifiers;
    BINDINGS.iter().find_map(|(keys, action, _)| {
        let (ctrl, shift, alt, wanted) = parse(keys);
        (modifiers.control == ctrl
            && modifiers.shift == shift
            && modifiers.alt == alt
            && key == wanted)
            .then_some(*action)
    })
}

/// `ctrl-shift-d` → (true, true, false, "d").
fn parse(keys: &str) -> (bool, bool, bool, String) {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key = String::new();
    for part in keys.split('-') {
        match part {
            "ctrl" => ctrl = true,
            "shift" => shift = true,
            "alt" => alt = true,
            other => key = other.to_ascii_lowercase(),
        }
    }
    (ctrl, shift, alt, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn stroke(key: &str, ctrl: bool, shift: bool) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: None,
            modifiers: Modifiers {
                control: ctrl,
                shift,
                ..Default::default()
            },
        }
    }

    #[test]
    fn every_binding_is_reachable_and_unique() {
        for (keys, action, _) in BINDINGS {
            let (ctrl, shift, alt, key) = parse(keys);
            let found = lookup(&Keystroke {
                key: key.clone(),
                key_char: None,
                modifiers: Modifiers {
                    control: ctrl,
                    shift,
                    alt,
                    ..Default::default()
                },
            });
            assert_eq!(found, Some(*action), "{keys} did not resolve");
        }
    }

    #[test]
    fn plain_typing_is_not_a_shortcut() {
        assert!(lookup(&stroke("d", false, false)).is_none());
        assert!(lookup(&stroke("d", true, false)).is_none());
        assert!(lookup(&stroke("b", false, false)).is_none());
    }
}
