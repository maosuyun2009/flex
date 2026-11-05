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
    ZoomPane,
    Find,
    Copy,
    Paste,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    NextPane,
    PrevPane,
    NextTab,
    PrevTab,
    SwapPane,
    ClearPane,
    NewTab,
    ToggleSidebar,
    Settings,
    /// Arms a one-shot prefix: the next bare key picks a tool.
    Prefix,
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
    (
        "ctrl-shift-enter",
        Action::ZoomPane,
        "Zoom the pane, or restore the layout",
    ),
    ("ctrl-h", Action::FocusLeft, "Focus the pane to the left"),
    ("ctrl-j", Action::FocusDown, "Focus the pane below"),
    ("ctrl-k", Action::FocusUp, "Focus the pane above"),
    ("ctrl-l", Action::FocusRight, "Focus the pane to the right"),
    ("ctrl-tab", Action::NextPane, "Focus the next pane"),
    (
        "ctrl-shift-tab",
        Action::PrevPane,
        "Focus the previous pane",
    ),
    ("ctrl-pagedown", Action::NextTab, "Show the next tab"),
    ("ctrl-pageup", Action::PrevTab, "Show the previous tab"),
    (
        "ctrl-shift-x",
        Action::SwapPane,
        "Swap this pane with the next one",
    ),
    ("ctrl-shift-k", Action::ClearPane, "Clear the pane's screen"),
    ("ctrl-shift-t", Action::NewTab, "Open a new tab"),
    ("ctrl-shift-h", Action::Find, "Find in the terminal"),
    ("ctrl-shift-c", Action::Copy, "Copy the selection"),
    (
        "ctrl-shift-v",
        Action::Paste,
        "Paste the clipboard into the terminal",
    ),
    ("ctrl-b", Action::ToggleSidebar, "Show or hide the sidebar"),
    ("ctrl-,", Action::Settings, "Settings"),
    (
        "ctrl-shift-space",
        Action::Prefix,
        "Prefix, then g lazygit · f file · d directory · z either",
    ),
];

pub fn lookup(keystroke: &Keystroke) -> Option<Action> {
    // Enter arrives as "\r" on Windows; the table spells it "enter".
    let key = match keystroke.key.to_ascii_lowercase().as_str() {
        "\r" | "\n" => "enter".to_owned(),
        other => other.to_owned(),
    };
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
