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
    RenameTab,
    RenamePane,
    MoveTabLeft,
    MoveTabRight,
    MinimizeFloat,
    RaiseFloat,
    LowerFloat,
    ParkPane,
    Palette,
    CopyTranscript,
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
    ("ctrl-shift-r", Action::RenameTab, "Rename this tab"),
    ("ctrl-alt-r", Action::RenamePane, "Rename this pane"),
    ("ctrl-shift-left", Action::MoveTabLeft, "Move this tab left"),
    (
        "ctrl-shift-right",
        Action::MoveTabRight,
        "Move this tab right",
    ),
    (
        "ctrl-shift-m",
        Action::MinimizeFloat,
        "Tuck the floating pane away, or bring it back",
    ),
    (
        "ctrl-shift-up",
        Action::RaiseFloat,
        "Bring this floating pane to the front",
    ),
    (
        "ctrl-shift-down",
        Action::LowerFloat,
        "Send this floating pane behind the others",
    ),
    (
        "ctrl-shift-o",
        Action::Palette,
        "Jump to a tab, a pane or a file",
    ),
    (
        "ctrl-shift-p",
        Action::ParkPane,
        "Park this pane: it keeps running, out of the layout",
    ),
    (
        "ctrl-shift-y",
        Action::CopyTranscript,
        "Copy the pane's screen and scrollback",
    ),
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

/// Stable action names are shared by the keymap and remote client.perform.
fn action_name(action: Action) -> String {
    let debug = format!("{action:?}");
    let mut name = String::from("flex.");
    for (i, c) in debug.chars().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            name.push('_');
        }
        name.push(c.to_ascii_lowercase());
    }
    name
}

pub fn configured_action(name: &str, args: &serde_json::Value) -> Option<Action> {
    use serde_json::Value;
    let empty = args.is_null() || args.as_object().is_some_and(|a| a.is_empty());
    if empty {
        if let Some((_, action, _)) = BINDINGS.iter().find(|(_, a, _)| action_name(*a) == name) {
            return Some(*action);
        }
        return match name {
            "client.tab.new" => Some(Action::NewTab),
            "client.tab.next" => Some(Action::NextTab),
            "client.tab.previous" => Some(Action::PrevTab),
            "pane.close" => Some(Action::ClosePane),
            "pane.zoom" => Some(Action::ZoomPane),
            "pane.float" => Some(Action::ToggleFloat),
            _ => None,
        };
    }
    let object = args.as_object()?;
    if object.len() != 1 {
        return None;
    }
    match (name, object.get("direction").and_then(Value::as_str)?) {
        ("pane.split", "right") => Some(Action::SplitRight),
        ("pane.split", "down") => Some(Action::SplitDown),
        ("pane.focus", "left") => Some(Action::FocusLeft),
        ("pane.focus", "right") => Some(Action::FocusRight),
        ("pane.focus", "up") => Some(Action::FocusUp),
        ("pane.focus", "down") => Some(Action::FocusDown),
        _ => None,
    }
}

pub fn keymap_request() -> serde_json::Value {
    use serde_json::json;
    let empty = json!({"type":"object","additionalProperties":false});
    let mut actions: Vec<_> = BINDINGS
        .iter()
        .map(|(_, action, title)| {
            json!({
                "name":action_name(*action), "kind":"client", "title":title, "args":empty
            })
        })
        .collect();
    for name in [
        "client.tab.new",
        "client.tab.next",
        "client.tab.previous",
        "pane.close",
        "pane.zoom",
        "pane.float",
    ] {
        actions.push(json!({"name":name,"kind":"client","args":empty}));
    }
    for (name, directions) in [
        ("pane.split", vec!["right", "down"]),
        ("pane.focus", vec!["left", "right", "up", "down"]),
    ] {
        actions.push(json!({"name":name,"kind":"client","args":{
            "type":"object","properties":{"direction":{"type":"string","enum":directions}},
            "required":["direction"],"additionalProperties":false
        }}));
    }
    actions.push(json!({"name":"client.mode.enter","kind":"client","args":{
        "type":"object","properties":{"name":{"type":"string"},"once":{"type":"boolean"}},"required":["name"],"additionalProperties":false
    }}));
    for name in ["client.mode.exit", "client.mode.exit_all"] {
        actions.push(json!({"name":name,"kind":"client","args":empty}));
    }
    let bindings: Vec<_> = BINDINGS
        .iter()
        .map(|(keys, action, _)| {
            json!({
                "keys":[keys.replace('-', "+")], "action":action_name(*action)
            })
        })
        .collect();
    json!({"actions":actions,"defaults":[{"name":"flex","bindings":bindings}],"server_actions":"all"})
}

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
    fn configured_actions_validate_direction_and_reject_unknown_names() {
        assert_eq!(
            configured_action("pane.split", &serde_json::json!({"direction":"right"})),
            Some(Action::SplitRight)
        );
        assert_eq!(
            configured_action("pane.split", &serde_json::json!({"direction":"down"})),
            Some(Action::SplitDown)
        );
        assert_eq!(
            configured_action("pane.split", &serde_json::json!({"direction":"diagonal"})),
            None
        );
        assert_eq!(configured_action("shell.run", &serde_json::json!({})), None);
        assert_eq!(
            configured_action("flex.close_pane", &serde_json::json!({"unexpected":true})),
            None
        );
    }

    #[test]
    fn registered_defaults_can_all_be_executed() {
        let request = keymap_request();
        for binding in request["defaults"][0]["bindings"].as_array().unwrap() {
            assert!(
                configured_action(binding["action"].as_str().unwrap(), &binding["args"]).is_some()
            );
            assert!(binding["keys"][0].as_str().unwrap().contains('+'));
        }
        assert_eq!(
            request["defaults"][0]["bindings"].as_array().unwrap().len(),
            BINDINGS.len()
        );
    }

    #[test]
    fn plain_typing_is_not_a_shortcut() {
        assert!(lookup(&stroke("d", false, false)).is_none());
        assert!(lookup(&stroke("d", true, false)).is_none());
        assert!(lookup(&stroke("b", false, false)).is_none());
    }
}
