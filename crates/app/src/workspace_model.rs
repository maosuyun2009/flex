//! Toolkit-free workspace state: server-scoped caches and float restore hints.
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct ServerCache<T> {
    values: HashMap<String, T>,
}

impl<T> ServerCache<T> {
    pub fn get(&self, server: Option<&str>) -> Option<&T> {
        server.and_then(|server| self.values.get(server))
    }

    pub fn insert(&mut self, server: String, value: T) {
        self.values.insert(server, value);
    }

    pub fn remove(&mut self, server: &str) {
        self.values.remove(server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_reply_from_previous_server_does_not_replace_current_cache() {
        let mut cache = ServerCache::default();
        cache.insert("current".into(), "current hosts");
        cache.insert("previous".into(), "late hosts");
        assert_eq!(cache.get(Some("current")), Some(&"current hosts"));
        assert_eq!(cache.get(Some("previous")), Some(&"late hosts"));
        assert_eq!(cache.get(Some("new")), None);
        cache.remove("current");
        assert_eq!(cache.get(Some("current")), None);
    }
}

/// The insertion point for moving an item one place, or no move at an edge.
pub(super) fn move_before(ids: &[String], id: &str, step: i32) -> Option<Option<String>> {
    let at = ids.iter().position(|item| item == id)?;
    let to = at as i64 + i64::from(step);
    if to < 0 || to >= ids.len() as i64 {
        return None;
    }
    Some(
        ids.get(if step < 0 {
            to as usize
        } else {
            to as usize + 1
        })
        .cloned(),
    )
}

#[cfg(test)]
mod ordering_tests {
    use super::move_before;

    #[test]
    fn reorders_without_crossing_server_list_edges() {
        let ids = ["a", "b", "c"].map(String::from);
        assert_eq!(move_before(&ids, "b", -1), Some(Some("a".into())));
        assert_eq!(move_before(&ids, "b", 1), Some(None));
        assert_eq!(move_before(&ids, "a", -1), None);
        assert_eq!(move_before(&ids, "c", 1), None);
        assert_eq!(move_before(&ids, "foreign session", 1), None);
    }
}

#[cfg(test)]
mod float_tests {
    use super::FloatHints;

    #[test]
    fn float_hint_survives_failure_and_other_server_with_same_block_id() {
        let mut hints = FloatHints::default();
        let rect = [0.5, 0.0, 0.5, 1.0];
        hints.remember("one", "block", rect);
        hints.remember("two", "block", [0.0, 0.0, 1.0, 0.4]);
        assert_eq!(hints.get("one", "block"), Some(rect));
        hints.finish_tiling("one", "block", false);
        assert_eq!(hints.get("one", "block"), Some(rect));
        hints.finish_tiling("one", "block", true);
        assert_eq!(hints.get("one", "block"), None);
        assert!(hints.get("two", "block").is_some());
    }
}

#[derive(Default)]
pub(super) struct FloatHints(HashMap<(String, String), [f64; 4]>);

impl FloatHints {
    pub fn remember(&mut self, server: &str, block: &str, rect: [f64; 4]) {
        self.0.entry((server.into(), block.into())).or_insert(rect);
    }

    pub fn get(&self, server: &str, block: &str) -> Option<[f64; 4]> {
        self.0.get(&(server.into(), block.into())).copied()
    }

    pub fn finish_tiling(&mut self, server: &str, block: &str, succeeded: bool) {
        if succeeded {
            self.0.remove(&(server.into(), block.into()));
        }
    }
}

/// Last-tiled-pane floating is presentation only: Rex cannot retile an empty layer.
pub(super) type LocalFloatKey = (String, String, String, String);

pub(super) struct LocalFloat {
    pub rect: [f64; 4],
    pub minimized: bool,
    layer: Option<String>,
}

#[derive(Default)]
pub(super) struct LocalFloats(pub HashMap<LocalFloatKey, LocalFloat>);

impl LocalFloats {
    pub fn toggle(
        &mut self,
        key: LocalFloatKey,
        window: &flex_engine::WindowState,
        rect: [f64; 4],
    ) -> bool {
        if self.0.remove(&key).is_some() {
            return true;
        }
        let tiled: Vec<_> = window.blocks.iter().filter(|b| !b.floating).collect();
        if tiled.len() != 1 || tiled[0].id != key.3 {
            return false;
        }
        self.0.insert(
            key,
            LocalFloat {
                rect,
                minimized: false,
                layer: tiled[0].layer_id.clone(),
            },
        );
        true
    }

    pub fn present(&self, key: &LocalFloatKey, block: &mut flex_engine::BlockState) {
        if let Some(local) = self.0.get(key) {
            block.rect = local.rect;
            block.floating = true;
            block.layer_kind = "local".into();
            block.layer_id = None;
            block.layer_state = if local.minimized {
                "minimized"
            } else {
                "normal"
            }
            .into();
        }
    }

    pub fn prune(&mut self, state: &flex_engine::AppState) {
        self.0.retain(|(server, session, window, block), local| {
            state
                .server(server)
                .and_then(|s| s.sessions.iter().find(|s| &s.id == session))
                .and_then(|s| s.windows.iter().find(|w| &w.id == window))
                .is_some_and(|w| {
                    let mut tiled = w.blocks.iter().filter(|b| !b.floating);
                    tiled
                        .next()
                        .is_some_and(|b| &b.id == block && b.layer_id == local.layer)
                        && tiled.next().is_none()
                })
        });
    }
}

pub(super) fn tiled_anchor(window: &flex_engine::WindowState, block: &str) -> Option<String> {
    window
        .blocks
        .iter()
        .find(|b| !b.floating && b.id != block)
        .map(|b| b.id.clone())
}

#[cfg(test)]
mod local_float_tests {
    use super::*;
    use flex_engine::{AppState, BlockState, ServerState, SessionState, WindowState};

    fn fixture() -> (AppState, LocalFloatKey) {
        (
            AppState {
                servers: vec![ServerState {
                    id: "server".into(),
                    sessions: vec![SessionState {
                        id: "session".into(),
                        windows: vec![WindowState {
                            id: "tab".into(),
                            blocks: vec![BlockState {
                                id: "pane".into(),
                                rect: [0.0, 0.0, 1.0, 1.0],
                                layer_id: Some("tiled".into()),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            },
            (
                "server".into(),
                "session".into(),
                "tab".into(),
                "pane".into(),
            ),
        )
    }

    #[test]
    fn solo_float_restore_keeps_exact_window_pane_and_server_placement() {
        let (state, key) = fixture();
        let window = &state.servers[0].sessions[0].windows[0];
        let mut locals = LocalFloats::default();
        assert!(locals.toggle(key.clone(), window, [0.1, 0.1, 0.8, 0.8]));
        let mut visible = window.blocks[0].clone();
        locals.present(&key, &mut visible);
        assert!(visible.floating);
        assert_eq!(visible.layer_kind, "local");
        assert!(!window.blocks[0].floating);
        assert!(locals.toggle(key.clone(), window, [0.1, 0.1, 0.8, 0.8]));
        let mut restored = window.blocks[0].clone();
        locals.present(&key, &mut restored);
        assert!(!restored.floating);
        assert_eq!(restored.rect, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(restored.id, "pane");
        assert_eq!(window.id, "tab");
        assert_eq!(window.blocks.len(), 1);
    }

    #[test]
    fn local_drag_minimize_and_scope_do_not_change_remote_block() {
        let (state, key) = fixture();
        let w = &state.servers[0].sessions[0].windows[0];
        let mut locals = LocalFloats::default();
        locals.toggle(key.clone(), w, [0.1, 0.1, 0.8, 0.8]);
        let local = locals.0.get_mut(&key).unwrap();
        local.rect = [0.2, 0.2, 0.5, 0.5];
        local.minimized = true;
        let mut visible = w.blocks[0].clone();
        locals.present(&key, &mut visible);
        assert_eq!(visible.rect, [0.2, 0.2, 0.5, 0.5]);
        assert_eq!(visible.layer_state, "minimized");
        let mut other = w.blocks[0].clone();
        let mut other_key = key.clone();
        other_key.0 = "other server".into();
        locals.present(&other_key, &mut other);
        assert!(!other.floating);
        assert_eq!(other.rect, [0.0, 0.0, 1.0, 1.0]);
        assert!(!w.blocks[0].floating);
    }

    #[test]
    fn other_tabs_and_server_floats_do_not_become_retile_anchors() {
        let (mut state, key) = fixture();
        let other = flex_engine::WindowState {
            id: "other".into(),
            blocks: vec![flex_engine::BlockState {
                id: "other-pane".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        state.servers[0].sessions[0].windows.push(other);
        let window = &mut state.servers[0].sessions[0].windows[0];
        window.blocks.push(flex_engine::BlockState {
            id: "tool".into(),
            floating: true,
            ..Default::default()
        });
        assert!(tiled_anchor(window, "pane").is_none());
        let mut locals = LocalFloats::default();
        assert!(locals.toggle(key.clone(), window, [0.1, 0.1, 0.8, 0.8]));
        locals.prune(&state);
        assert!(locals.0.contains_key(&key));
        let window = &mut state.servers[0].sessions[0].windows[0];
        assert!(locals.toggle(key.clone(), window, [0.1, 0.1, 0.8, 0.8]));
        window.blocks.push(flex_engine::BlockState {
            id: "same-tab".into(),
            ..Default::default()
        });
        assert!(!locals.toggle(key, window, [0.1, 0.1, 0.8, 0.8]));
        assert_eq!(tiled_anchor(window, "pane").as_deref(), Some("same-tab"));
    }

    #[test]
    fn remote_split_move_float_or_close_invalidates_local_override() {
        for change in 0..4 {
            let (mut state, key) = fixture();
            let mut locals = LocalFloats::default();
            locals.toggle(
                key.clone(),
                &state.servers[0].sessions[0].windows[0],
                [0.1, 0.1, 0.8, 0.8],
            );
            let w = &mut state.servers[0].sessions[0].windows[0];
            match change {
                0 => w.blocks.push(BlockState {
                    id: "second".into(),
                    ..Default::default()
                }),
                1 => w.id = "other tab".into(),
                2 => w.blocks[0].floating = true,
                _ => w.blocks.clear(),
            }
            locals.prune(&state);
            assert!(locals.0.is_empty());
        }
    }
}

/// A tool request and its pane stay scoped to the context that opened them.
#[derive(Clone)]
pub(super) struct PendingTool {
    pub request: std::sync::Arc<()>,
    pub server: String,
    pub session: String,
    pub window: String,
    pub origin: String,
    pub target: Option<String>,
    pub suffix: Option<&'static str>,
    pub observed: bool,
    pub confirmed_absent: bool,
    pub checking: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum ToolProgress {
    Waiting,
    Running,
    Finished,
    Cancelled,
}

impl PendingTool {
    pub fn reconcile(
        &mut self,
        context: (Option<&str>, Option<&str>, Option<&str>),
        blocks: &[(&str, bool)],
        active: &mut Option<String>,
        pending_focus: &mut Option<String>,
    ) -> ToolProgress {
        if !self.matches_context(context)
            || (!self.observed
                && active
                    .as_deref()
                    .is_some_and(|id| id != self.origin && Some(id) != self.target.as_deref()))
        {
            return ToolProgress::Cancelled;
        }
        let present = self
            .target
            .as_deref()
            .and_then(|target| blocks.iter().find(|(id, _)| *id == target));
        if let Some((_, exited)) = present {
            if !self.observed {
                *active = self.target.clone();
                *pending_focus = self.target.clone();
                self.observed = true;
            }
            if !exited {
                return ToolProgress::Running;
            }
        } else if !self.observed && !self.confirmed_absent {
            // No view exists yet. Keep keys off the origin while waiting.
            *pending_focus = None;
            return ToolProgress::Waiting;
        }
        if active
            .as_deref()
            .is_none_or(|id| id == self.origin || Some(id) == self.target.as_deref())
            && blocks.iter().any(|(id, _)| *id == self.origin)
        {
            *active = Some(self.origin.clone());
            *pending_focus = active.clone();
        }
        ToolProgress::Finished
    }

    pub fn matches_context(&self, context: (Option<&str>, Option<&str>, Option<&str>)) -> bool {
        context
            == (
                Some(self.server.as_str()),
                Some(self.session.as_str()),
                Some(self.window.as_str()),
            )
    }
}

#[cfg(test)]
mod tool_tests {
    use super::{PendingTool, ToolProgress};

    fn tool() -> PendingTool {
        PendingTool {
            request: Default::default(),
            server: "server".into(),
            session: "session".into(),
            window: "window".into(),
            origin: "origin".into(),
            target: Some("tool".into()),
            suffix: Some(" "),
            observed: false,
            confirmed_absent: false,
            checking: false,
        }
    }

    #[test]
    fn response_before_snapshot_survives_stale_reload_and_render_then_focuses_tool() {
        let mut tool = tool();
        let mut active = Some("origin".into());
        let mut focus = None;
        let context = (Some("server"), Some("session"), Some("window"));
        // Both reload and render must preserve the transition instead of settling
        // the picker or handing the keyboard back to the stale origin snapshot.
        for _ in 0..2 {
            assert_eq!(
                tool.reconcile(context, &[("origin", false)], &mut active, &mut focus),
                ToolProgress::Waiting
            );
            assert!(!tool.observed);
            assert_eq!(active.as_deref(), Some("origin"));
            assert_eq!(focus, None);
        }
        assert_eq!(
            tool.reconcile(
                context,
                &[("origin", false), ("tool", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Running
        );
        assert!(tool.observed);
        assert_eq!(active.as_deref(), Some("tool"));
        assert_eq!(focus.as_deref(), Some("tool"));
    }
}

#[cfg(test)]
mod tool_exit_tests {
    use super::{PendingTool, ToolProgress};

    fn tool() -> PendingTool {
        PendingTool {
            request: Default::default(),
            server: "server".into(),
            session: "session".into(),
            window: "window".into(),
            origin: "origin".into(),
            target: Some("tool".into()),
            suffix: None,
            observed: true,
            confirmed_absent: false,
            checking: false,
        }
    }

    #[test]
    fn visiting_another_pane_keeps_picker_lifecycle_without_stealing_focus() {
        let mut tool = tool();
        tool.suffix = Some(" ");
        let mut active = Some("other".into());
        let mut focus = None;
        let context = (Some("server"), Some("session"), Some("window"));
        assert_eq!(
            tool.reconcile(
                context,
                &[("origin", false), ("tool", false), ("other", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Running
        );
        assert_eq!(active.as_deref(), Some("other"));
        assert_eq!(focus, None);
        assert_eq!(
            tool.reconcile(
                context,
                &[("origin", false), ("other", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Finished
        );
        assert_eq!(active.as_deref(), Some("other"));
        assert_eq!(focus, None);
    }

    #[test]
    fn gone_after_seen_restores_origin_not_first_visible_pane() {
        let mut tool = tool();
        let mut active = Some("tool".into());
        let mut focus = None;
        assert_eq!(
            tool.reconcile(
                (Some("server"), Some("session"), Some("window")),
                &[("other", false), ("origin", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Finished
        );
        assert_eq!(active.as_deref(), Some("origin"));
        assert_eq!(focus.as_deref(), Some("origin"));
    }

    #[test]
    fn switched_server_session_or_tab_cancels_without_changing_focus() {
        for context in [
            (Some("other"), Some("session"), Some("window")),
            (Some("server"), Some("other"), Some("window")),
            (Some("server"), Some("session"), Some("other")),
        ] {
            let mut tool = tool();
            let mut active = Some("new destination".into());
            let mut focus = active.clone();
            assert_eq!(
                tool.reconcile(context, &[("origin", false)], &mut active, &mut focus),
                ToolProgress::Cancelled
            );
            assert_eq!(active.as_deref(), Some("new destination"));
            assert_eq!(focus, active);
        }
    }
    #[test]
    fn authoritative_absence_finishes_a_tool_that_exited_before_any_snapshot() {
        let mut tool = tool();
        tool.observed = false;
        tool.confirmed_absent = true;
        let mut active = Some("origin".into());
        let mut focus = None;
        assert_eq!(
            tool.reconcile(
                (Some("server"), Some("session"), Some("window")),
                &[("origin", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Finished
        );
        assert_eq!(focus.as_deref(), Some("origin"));
    }

    #[test]
    fn moving_to_another_pane_while_request_is_pending_cancels_focus_transfer() {
        let mut tool = tool();
        tool.observed = false;
        let mut active = Some("other pane".into());
        let mut focus = active.clone();
        assert_eq!(
            tool.reconcile(
                (Some("server"), Some("session"), Some("window")),
                &[("tool", false), ("origin", false)],
                &mut active,
                &mut focus
            ),
            ToolProgress::Cancelled
        );
        assert_eq!(focus.as_deref(), Some("other pane"));
    }
}

/// The server does not focus new floating layers unless explicitly requested.
pub(super) fn tool_layer_request(
    session: &str,
    window: &str,
    options: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": session,
        "window_id": window,
        "focus": true,
        "bounds": [0.18, 0.1, 0.64, 0.72],
        "layout": {"block": {
            "flavor": "com.superlogical.terminal.shell", "options": options,
        }},
    })
}

#[cfg(test)]
mod tool_request_tests {
    use super::tool_layer_request;
    use serde_json::json;

    #[test]
    fn floating_tool_explicitly_requests_server_focus_in_its_own_window() {
        let options = json!({"shell": "none", "command": ["lazygit"], "cwd": "/repo"});
        let payload = tool_layer_request("session", "window", options.clone());
        assert_eq!(payload["focus"], true);
        assert_eq!(payload["session_id"], "session");
        assert_eq!(payload["window_id"], "window");
        assert_eq!(payload["bounds"], json!([0.18, 0.1, 0.64, 0.72]));
        assert_eq!(payload["layout"]["block"]["options"], options);
    }
}

/// Windows paths written the way a WSL server names them: `C:\a\b.png` becomes
/// `/mnt/c/a/b.png`, and a quoted path keeps its quotes. Only absolute paths
/// with a drive letter are touched, so ordinary pasted text is left alone.
pub(super) fn to_wsl_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' if token.is_empty() => quoted = !quoted,
            '"' => {
                out.push_str(&convert_token(&token, quoted));
                token.clear();
                quoted = !quoted;
            }
            ' ' if !quoted => {
                out.push_str(&convert_token(&token, false));
                token.clear();
                out.push(' ');
            }
            _ => token.push(c),
        }
    }
    out.push_str(&convert_token(&token, quoted));
    out
}

fn convert_token(token: &str, quoted: bool) -> String {
    match (windows_to_wsl(token), quoted) {
        (Some(path), true) => format!("\"{path}\""),
        (Some(path), false) => path,
        (None, true) => format!("\"{token}\""),
        (None, false) => token.to_owned(),
    }
}

fn windows_to_wsl(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    let absolute = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    absolute.then(|| {
        format!(
            "/mnt/{}{}",
            (bytes[0] as char).to_ascii_lowercase(),
            token[2..].replace('\\', "/")
        )
    })
}

#[cfg(test)]
mod path_tests {
    use super::to_wsl_paths;

    #[test]
    fn windows_paths_become_wsl_mounts() {
        assert_eq!(
            to_wsl_paths(r"C:\Users\me\shot.png"),
            "/mnt/c/Users/me/shot.png"
        );
        assert_eq!(to_wsl_paths(r"D:\rexbuild\flex"), "/mnt/d/rexbuild/flex");
        // Several files, and a path with a space keeps its quotes.
        assert_eq!(
            to_wsl_paths(r#"C:\a.png D:\b.png"#),
            "/mnt/c/a.png /mnt/d/b.png"
        );
        assert_eq!(
            to_wsl_paths(r#""C:\my shots\a.png" C:\b.png"#),
            "\"/mnt/c/my shots/a.png\" /mnt/c/b.png"
        );
    }

    #[test]
    fn everything_else_is_left_alone() {
        for text in [
            "plain text",
            "C:",
            r"foo\bar",
            "https://example.com",
            "/mnt/c/x",
        ] {
            assert_eq!(to_wsl_paths(text), text);
        }
    }
}
