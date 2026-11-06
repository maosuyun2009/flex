//! Client-side resolution of Rex's already-layered keymap (top binding first).
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Pass,
    Consumed,
    Action { name: String, args: Value },
    Forward { prefix: Vec<String> },
}

#[derive(Default)]
pub struct KeyBindings {
    map: Value,
    prefix: Vec<String>,
    modes: Vec<(String, bool)>,
    sequence_mode: Option<String>,
}

impl KeyBindings {
    pub fn update(&mut self, map: Value) {
        self.map = map;
        self.prefix.clear();
        self.modes.clear();
        self.sequence_mode = None;
    }

    pub fn mode(&self) -> Option<&str> {
        self.modes.last().map(|(name, _)| name.as_str())
    }

    pub fn pending(&self) -> &[String] {
        &self.prefix
    }

    pub fn resolve_for_creator(
        &mut self,
        key: &str,
        creator: &str,
        local: impl Fn(&str, &Value) -> bool,
    ) -> Resolution {
        let actions = self.map["actions"].clone();
        self.resolve(key, |name, args| {
            local(name, args)
                || actions.as_array().into_iter().flatten().any(|action| {
                    if action["name"].as_str() != Some(name) {
                        return false;
                    }
                    match action["kind"].as_str() {
                        Some("block")
                            if !name
                                .strip_prefix(creator)
                                .is_some_and(|suffix| suffix.starts_with('.')) =>
                        {
                            return false
                        }
                        Some("lua" | "block") => {}
                        _ => return false,
                    }
                    if action.get("args").is_none() {
                        return args.is_null()
                            || (action["kind"] == "lua"
                                && args.as_object().is_some_and(|o| o.is_empty()));
                    }
                    let schema = &action["args"];
                    let empty = serde_json::json!({});
                    let value = if args.is_null() { &empty } else { args };
                    value.is_object()
                        && schema["required"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .all(|name| name.as_str().is_some_and(|name| value.get(name).is_some()))
                        && (schema["additionalProperties"] != false
                            || value
                                .as_object()
                                .unwrap()
                                .keys()
                                .all(|key| schema["properties"].get(key).is_some()))
                        && value.as_object().unwrap().iter().all(|(key, value)| {
                            let rule = &schema["properties"][key];
                            let typed = match rule["type"].as_str() {
                                Some("string") => value.is_string(),
                                Some("boolean") => value.is_boolean(),
                                Some("integer") => value.is_i64() || value.is_u64(),
                                Some("number") => value.is_number(),
                                Some("object") => value.is_object(),
                                Some("array") => value.is_array(),
                                _ => true,
                            };
                            typed
                                && rule["enum"]
                                    .as_array()
                                    .is_none_or(|allowed| allowed.contains(value))
                        })
                })
        })
    }

    pub fn resolve(&mut self, key: &str, available: impl Fn(&str, &Value) -> bool) -> Resolution {
        let key = normalize(key);
        if !self.prefix.is_empty() && key == "escape" {
            self.prefix.clear();
            self.sequence_mode = None;
            return Resolution::Consumed;
        }
        let mut sequence = self.prefix.clone();
        sequence.push(key.clone());
        let searching = if let Some(mode) = &self.sequence_mode {
            vec![mode.clone()]
        } else {
            self.modes
                .iter()
                .rev()
                .map(|(m, _)| m.clone())
                .chain(std::iter::once(String::new()))
                .collect()
        };
        for mode in searching {
            let declaration = self.map["modes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|m| m["name"].as_str() == Some(mode.as_str()));
            let blocked = declaration.is_some_and(|m| {
                m["blocked"].as_array().into_iter().flatten().any(|keys| {
                    keys.as_array().is_some_and(|keys| {
                        keys.len() == sequence.len()
                            && keys
                                .iter()
                                .zip(&sequence)
                                .all(|(a, b)| a.as_str().is_some_and(|a| normalize(a) == *b))
                    })
                })
            });
            let candidate = self.map["bindings"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|binding| {
                    if binding["mode"].as_str().unwrap_or("") != mode {
                        return false;
                    }
                    let Some(name) = binding["action"].as_str() else {
                        return false;
                    };
                    let internal = matches!(
                        name,
                        "client.key.forward"
                            | "client.mode.enter"
                            | "client.mode.exit"
                            | "client.mode.exit_all"
                    );
                    if !internal && !available(name, &binding["args"]) {
                        return false;
                    }
                    binding["keys"].as_array().is_some_and(|keys| {
                        keys.len() >= sequence.len()
                            && keys
                                .iter()
                                .zip(&sequence)
                                .all(|(a, b)| a.as_str().is_some_and(|a| normalize(a) == *b))
                    })
                })
                .cloned();
            if let Some(binding) = candidate {
                if binding["keys"].as_array().unwrap().len() > sequence.len() {
                    self.prefix = sequence;
                    self.sequence_mode = Some(mode);
                    return Resolution::Consumed;
                }
                let prefix = std::mem::take(&mut self.prefix);
                self.sequence_mode = None;
                let name = binding["action"].as_str().unwrap();
                let args = binding["args"].clone();
                if self.modes.last().is_some_and(|(_, once)| *once) {
                    self.modes.pop();
                }
                return match name {
                    "client.key.forward" => Resolution::Forward { prefix },
                    "client.mode.enter" => {
                        if let Some(name) = args["name"].as_str().filter(|name| {
                            self.map["modes"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|m| m["name"].as_str() == Some(name))
                        }) {
                            self.modes
                                .push((name.to_owned(), args["once"].as_bool().unwrap_or(false)));
                        }
                        Resolution::Consumed
                    }
                    "client.mode.exit" => {
                        self.modes.pop();
                        Resolution::Consumed
                    }
                    "client.mode.exit_all" => {
                        self.modes.clear();
                        Resolution::Consumed
                    }
                    _ => Resolution::Action {
                        name: name.to_owned(),
                        args,
                    },
                };
            }
            if blocked {
                self.finish_sequence();
                return Resolution::Consumed;
            }
            if !mode.is_empty() && key == "escape" {
                self.modes.pop();
                self.finish_sequence();
                return Resolution::Consumed;
            }
            if declaration.is_some_and(|m| m["exclusive"].as_bool() == Some(true)) {
                self.finish_sequence();
                return Resolution::Consumed;
            }
        }
        if !self.prefix.is_empty() {
            self.finish_sequence();
            Resolution::Consumed
        } else {
            Resolution::Pass
        }
    }

    fn finish_sequence(&mut self) {
        self.prefix.clear();
        self.sequence_mode = None;
    }
}

/// Rex accepts modifiers in any order; keep the actual key, including '+', intact.
pub fn normalize(key: &str) -> String {
    let mut rest = key;
    let mut modifiers = Vec::new();
    while let Some((modifier, tail)) = rest.split_once('+') {
        let modifier = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "ctrl",
            "shift" => "shift",
            "alt" | "option" => "alt",
            "cmd" | "super" | "meta" => "cmd",
            _ => break,
        };
        modifiers.push(modifier);
        rest = tail;
    }
    modifiers.sort_unstable();
    modifiers.dedup();
    let key = match rest {
        "esc" => "escape",
        "return" | "\r" | "\n" => "enter",
        "pageup" => "page_up",
        "pagedown" => "page_down",
        " " => "space",
        _ => rest,
    };
    if modifiers.is_empty() {
        key.to_owned()
    } else {
        format!("{}+{}", modifiers.join("+"), key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolver(bindings: Value, modes: Value) -> KeyBindings {
        let mut keys = KeyBindings::default();
        keys.update(json!({"bindings": bindings, "modes": modes}));
        keys
    }

    #[test]
    fn higher_sequence_overrides_single_key_and_forwards_only_its_prefix() {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+b","x"],"action":"client.key.forward"},
                {"keys":["ctrl+b"],"action":"sidebar.toggle"}
            ]),
            json!([]),
        );
        assert_eq!(keys.resolve("ctrl+b", |_, _| true), Resolution::Consumed);
        assert_eq!(
            keys.resolve("x", |_, _| true),
            Resolution::Forward {
                prefix: vec!["ctrl+b".into()]
            }
        );
        assert_eq!(keys.resolve("q", |_, _| true), Resolution::Pass);
    }

    #[test]
    fn unavailable_action_falls_through_but_lua_forward_never_does() {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+k"],"action":"unavailable"},
                {"keys":["ctrl+k"],"action":"client.key.forward"},
                {"keys":["ctrl+k"],"action":"local"}
            ]),
            json!([]),
        );
        assert_eq!(
            keys.resolve("ctrl+k", |name, _| name == "local"),
            Resolution::Forward { prefix: vec![] }
        );
    }

    #[test]
    fn escape_cancels_sequence_and_failed_sequence_does_not_type_its_prefix() {
        let mut keys = resolver(json!([{"keys":["ctrl+b","c"],"action":"new"}]), json!([]));
        assert_eq!(keys.resolve("ctrl+b", |_, _| true), Resolution::Consumed);
        assert_eq!(keys.resolve("escape", |_, _| true), Resolution::Consumed);
        assert_eq!(keys.resolve("c", |_, _| true), Resolution::Pass);
        keys.resolve("ctrl+b", |_, _| true);
        assert_eq!(keys.resolve("q", |_, _| true), Resolution::Consumed);
    }

    #[test]
    fn exclusive_mode_blocks_typing_and_escape_exits_it() {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+r"],"action":"client.mode.enter","args":{"name":"resize"}},
                {"keys":["left"],"action":"resize","mode":"resize"}
            ]),
            json!([{"name":"resize","exclusive":true}]),
        );
        assert_eq!(keys.resolve("ctrl+r", |_, _| true), Resolution::Consumed);
        assert_eq!(keys.resolve("x", |_, _| true), Resolution::Consumed);
        assert_eq!(
            keys.resolve("left", |_, _| true),
            Resolution::Action {
                name: "resize".into(),
                args: Value::Null
            }
        );
        assert_eq!(keys.resolve("escape", |_, _| true), Resolution::Consumed);
        assert_eq!(keys.resolve("x", |_, _| true), Resolution::Pass);
    }

    #[test]
    fn once_mode_survives_sequence_prefix_then_exits_after_action() {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+r"],"action":"client.mode.enter","args":{"name":"once","once":true}},
                {"keys":["g","g"],"action":"go","mode":"once"}
            ]),
            json!([{"name":"once"}]),
        );
        keys.resolve("ctrl+r", |_, _| true);
        assert_eq!(keys.resolve("g", |_, _| true), Resolution::Consumed);
        assert_eq!(
            keys.resolve("g", |_, _| true),
            Resolution::Action {
                name: "go".into(),
                args: Value::Null
            }
        );
        assert_eq!(keys.resolve("g", |_, _| true), Resolution::Pass);
    }

    #[test]
    fn mode_binding_wins_over_a_block_that_only_blocks_fallback() {
        let mut keys = resolver(
            json!([
                {"keys":["r"],"action":"client.mode.enter","args":{"name":"resize"}},
                {"keys":["right"],"action":"move","mode":"resize"}
            ]),
            json!([{"name":"resize","blocked":[["right"]]}]),
        );
        keys.resolve("r", |_, _| true);
        assert_eq!(
            keys.resolve("right", |_, _| true),
            Resolution::Action {
                name: "move".into(),
                args: Value::Null
            }
        );
    }

    #[test]
    fn wrong_creator_action_falls_through_to_supported_action() {
        let mut keys = KeyBindings::default();
        keys.update(json!({"bindings":[
            {"keys":["x"],"action":"com.superlogical.onboarding.set_step","args":{"step":"done"}},
            {"keys":["x"],"action":"local"}
        ],"actions":[{"name":"com.superlogical.onboarding.set_step","kind":"block"}]}));
        assert_eq!(
            keys.resolve("x", |name, _| name == "local"),
            Resolution::Action {
                name: "local".into(),
                args: Value::Null
            }
        );
    }

    #[test]
    fn blocked_key_does_not_fall_through_to_root_binding() {
        let mut keys = resolver(
            json!([
                {"keys":["r"],"action":"client.mode.enter","args":{"name":"resize"}},
                {"keys":["right"],"action":"root"}
            ]),
            json!([{"name":"resize","blocked":[["right"]]}]),
        );
        keys.resolve("r", |_, _| true);
        assert_eq!(keys.resolve("right", |_, _| true), Resolution::Consumed);
    }

    #[test]
    fn equivalent_modifier_order_matches_and_invalid_mode_does_not_trap_input() {
        let mut keys = resolver(
            json!([
                {"keys":["shift+ctrl+d"],"action":"split"},
                {"keys":["r"],"action":"client.mode.enter","args":{"name":"missing"}}
            ]),
            json!([]),
        );
        assert_eq!(
            keys.resolve("ctrl+shift+d", |_, _| true),
            Resolution::Action {
                name: "split".into(),
                args: Value::Null
            }
        );
        keys.resolve("r", |_, _| true);
        assert_eq!(keys.resolve("x", |_, _| true), Resolution::Pass);
    }
}
