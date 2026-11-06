//! Remote requests are untrusted; resolve a concrete owner before doing anything.
use crate::shortcuts::{self, Action};
use flex_engine::{AppState, ClientReplyError, ClientRequest, Connectivity};
use serde_json::{json, Value};

#[derive(Default)]
struct Selection<'a> {
    server: Option<&'a str>,
    session: Option<&'a str>,
    window: Option<&'a str>,
    block: Option<&'a str>,
}

#[derive(Debug, PartialEq)]
enum Plan {
    Local(Action),
    Rpc {
        server: String,
        session: Option<String>,
        calls: Vec<(&'static str, Value)>,
        follow_focus: bool,
    },
}

type Reply = Result<Value, ClientReplyError>;

fn error(code: &str, message: &str) -> ClientReplyError {
    ClientReplyError {
        code: code.into(),
        message: message.into(),
    }
}

fn optional_id<'a>(payload: &'a Value, key: &str) -> Result<Option<&'a str>, ClientReplyError> {
    match payload.get(key) {
        None => Ok(None),
        Some(Value::String(id)) if !id.is_empty() => Ok(Some(id)),
        _ => Err(error("refused", "Invalid session or block identifier")),
    }
}

fn plan(
    enabled: bool,
    state: &AppState,
    selection: Selection<'_>,
    request: &ClientRequest,
) -> Result<Plan, ClientReplyError> {
    if !enabled {
        return Err(error("refused", "Remote client actions are disabled"));
    }
    if request.request_type == "client_call_requested" {
        return relay_plan(state, request);
    }
    if request.request_type != "client_perform_requested" {
        return Err(error("not_supported", "Unsupported client request"));
    }
    let name = request
        .payload
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| error("not_supported", "Missing action name"))?;
    let action = shortcuts::configured_action(name, &request.payload["args"])
        .ok_or_else(|| error("not_supported", "Unknown action or invalid arguments"))?;
    if matches!(
        action,
        Action::Copy
            | Action::Paste
            | Action::CopyTranscript
            | Action::Prefix
            | Action::Palette
            | Action::RenameTab
            | Action::RenamePane
    ) {
        return Err(error(
            "refused",
            "Clipboard, prompts and tool launchers require local interaction",
        ));
    }
    let server = state
        .server(&request.server_id)
        .filter(|s| s.connectivity == Connectivity::Online)
        .ok_or_else(|| error("refused", "Requesting server is not connected"))?;
    let explicit_session = optional_id(&request.payload, "session_id")?;
    let session_id = explicit_session
        .or_else(|| {
            (selection.server == Some(server.id.as_str()))
                .then_some(selection.session)
                .flatten()
        })
        .ok_or_else(|| error("refused", "No selected session on the requesting server"))?;
    let session = server
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .ok_or_else(|| error("refused", "Session does not belong to requesting server"))?;
    let selected =
        selection.server == Some(server.id.as_str()) && selection.session == Some(session_id);
    let explicit_block = optional_id(&request.payload, "block_id")?;
    let selected_window = session.windows.iter().find(|w| {
        Some(w.id.as_str())
            == if selected {
                selection.window
            } else {
                session.active_window_id.as_deref()
            }
    });
    let block_id = explicit_block.or_else(|| {
        if selected {
            selection.block
        } else {
            selected_window.and_then(|w| w.focused_block_id.as_deref())
        }
    });
    let target = block_id.and_then(|id| {
        session
            .windows
            .iter()
            .find_map(|w| w.blocks.iter().find(|b| b.id == id).map(|b| (w, b)))
    });
    if block_id.is_some() && target.is_none() {
        return Err(error(
            "refused",
            "Block does not belong to the requested session",
        ));
    }
    if matches!(
        action,
        Action::ToggleSidebar | Action::Settings | Action::Find
    ) {
        if !selected
            || explicit_block.is_some_and(|b| Some(b) != selection.block)
            || target.is_some_and(|(w, _)| Some(w.id.as_str()) != selection.window)
        {
            return Err(error(
                "refused",
                "Local UI action does not target the selected pane",
            ));
        }
        return Ok(Plan::Local(action));
    }
    if action == Action::NewTab {
        return Ok(Plan::Rpc {
            server: server.id.clone(),
            session: Some(session.id.clone()),
            calls: vec![(
                "session.new_window",
                json!({"session_id":session.id,
                "layout":{"block":{"flavor":"com.superlogical.terminal.shell"}},"focus":true}),
            )],
            follow_focus: true,
        });
    }
    let (window, block) = target.ok_or_else(|| error("refused", "No owned pane to act on"))?;
    let payload = json!({"session_id":session.id,"block_id":block.id});
    let mut follow_focus = false;
    let calls = match action {
        Action::ClosePane => vec![("block.close", payload)],
        Action::ZoomPane => {
            follow_focus = true;
            vec![("session.zoom_block", payload)]
        }
        Action::SplitRight | Action::SplitDown => {
            follow_focus = true;
            let mut layout = json!({"block":{"flavor":"com.superlogical.terminal.shell"}});
            if block.pwd.starts_with('/') && !block.pwd.contains('\0') {
                layout["block"]["options"] = json!({"cwd":block.pwd});
            }
            vec![(
                "session.new_split",
                json!({"session_id":session.id,"anchor_id":block.id,
                "direction":if action == Action::SplitDown { "vertical" } else { "horizontal" },
                "side":"after","layout":layout,"focus":true}),
            )]
        }
        Action::FocusLeft
        | Action::FocusRight
        | Action::FocusUp
        | Action::FocusDown
        | Action::NextPane
        | Action::PrevPane
        | Action::NextTab
        | Action::PrevTab => {
            follow_focus = true;
            let (method, args) = match action {
                Action::FocusLeft => (
                    "session.focus_direction",
                    json!({"session_id":session.id,"direction":"left"}),
                ),
                Action::FocusRight => (
                    "session.focus_direction",
                    json!({"session_id":session.id,"direction":"right"}),
                ),
                Action::FocusUp => (
                    "session.focus_direction",
                    json!({"session_id":session.id,"direction":"up"}),
                ),
                Action::FocusDown => (
                    "session.focus_direction",
                    json!({"session_id":session.id,"direction":"down"}),
                ),
                Action::NextPane => (
                    "session.focus_next_pane",
                    json!({"session_id":session.id,"wrap":true}),
                ),
                Action::PrevPane => (
                    "session.focus_previous_pane",
                    json!({"session_id":session.id,"wrap":true}),
                ),
                Action::NextTab => (
                    "session.focus_next_window",
                    json!({"session_id":session.id,"wrap":true}),
                ),
                _ => (
                    "session.focus_previous_window",
                    json!({"session_id":session.id,"wrap":true}),
                ),
            };
            vec![("session.focus_block", payload), (method, args)]
        }
        Action::SwapPane => {
            let index = window.blocks.iter().position(|b| b.id == block.id).unwrap();
            let other = window
                .blocks
                .get((index + 1) % window.blocks.len())
                .filter(|b| b.id != block.id)
                .ok_or_else(|| error("refused", "No other pane to swap"))?;
            vec![(
                "session.swap_blocks",
                json!({"session_id":session.id,"block_id":block.id,"other_block_id":other.id}),
            )]
        }
        Action::MoveTabLeft | Action::MoveTabRight => {
            let ids: Vec<_> = session.windows.iter().map(|w| w.id.clone()).collect();
            let direction = if action == Action::MoveTabLeft { -1 } else { 1 };
            let before = super::model::move_before(&ids, &window.id, direction)
                .ok_or_else(|| error("refused", "Tab is already at the edge"))?;
            let mut payload = json!({"session_id":session.id,"window_id":window.id});
            if let Some(before) = before {
                payload["before_window_id"] = json!(before);
            }
            vec![("session.move_window", payload)]
        }
        Action::ParkPane => vec![("session.detach_block", payload)],
        Action::MinimizeFloat | Action::RaiseFloat | Action::LowerFloat => {
            follow_focus = action == Action::MinimizeFloat;
            let layer = block
                .layer_id
                .as_ref()
                .filter(|_| block.floating && block.layer_kind == "floating")
                .ok_or_else(|| error("refused", "Pane is not in a floating layer"))?;
            let mut payload = json!({"session_id":session.id,"layer_id":layer});
            let method = match action {
                Action::RaiseFloat => "session.raise_layer",
                Action::LowerFloat => "session.lower_layer",
                _ => {
                    let restore = block.layer_state == "minimized";
                    payload["state"] = json!(if restore { "normal" } else { "minimized" });
                    payload["focus"] = json!(restore);
                    "session.set_layer_state"
                }
            };
            vec![(method, payload)]
        }
        Action::ClearPane => {
            if !block.is_terminal() {
                return Err(error("refused", "Pane is not a terminal"));
            }
            vec![(
                "com.superlogical.terminal.clear",
                json!({"session_id":session.id,"block_id":block.id,"args":{}}),
            )]
        }
        Action::ToggleFloat
            if !block.floating && super::model::tiled_anchor(window, &block.id).is_none() =>
        {
            return Err(error(
                "refused",
                "The last tiled pane floats locally; use this client's pane shortcut",
            ));
        }
        Action::ToggleFloat if !block.floating => vec![(
            "session.float_block",
            json!({
                "session_id":session.id,"block_id":block.id,"bounds":[0.14,0.12,0.72,0.7]
            }),
        )],
        Action::ToggleFloat => {
            let anchor = super::model::tiled_anchor(window, &block.id).ok_or_else(|| {
                error(
                    "refused",
                    "No tiled anchor in this tab; no new pane was created",
                )
            })?;
            vec![(
                "session.move_block",
                json!({"session_id":session.id,"block_id":block.id,
                "anchor_block_id":anchor,"side":"after","direction":"horizontal","ratio":0.5}),
            )]
        }
        _ => {
            return Err(error(
                "not_supported",
                "This action is not remotely implemented",
            ))
        }
    };
    let _ = window;
    Ok(Plan::Rpc {
        server: server.id.clone(),
        session: Some(session.id.clone()),
        calls,
        follow_focus,
    })
}

// API 10 event schema: call_id, server (advertised opaque id), method, args.
// Deliberately read-only: never relay scripts, terminal input or client.action.
fn relay_plan(state: &AppState, request: &ClientRequest) -> Result<Plan, ClientReplyError> {
    let payload = request
        .payload
        .as_object()
        .ok_or_else(|| error("refused", "Invalid relay payload"))?;
    if payload
        .keys()
        .any(|key| !["call_id", "server", "method", "args"].contains(&key.as_str()))
    {
        return Err(error("refused", "Unrecognized relay schema"));
    }
    let server_id = payload
        .get("server")
        .and_then(Value::as_str)
        .ok_or_else(|| error("refused", "Missing saved relay server id"))?;
    let server = state
        .server(server_id)
        .filter(|s| s.connectivity == Connectivity::Online && s.id != request.server_id)
        .ok_or_else(|| {
            error(
                "refused",
                "Relay destination is not another saved connected server",
            )
        })?;
    let args = payload
        .get("args")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let object = args
        .as_object()
        .ok_or_else(|| error("refused", "Relay arguments must be an object"))?;
    let (method, session) = match payload.get("method").and_then(Value::as_str) {
        Some("server.status") if object.is_empty() => ("server.status", None),
        Some("server.whoami") if object.is_empty() => ("server.whoami", None),
        Some("session.list") if object.is_empty() => ("session.list", None),
        Some("session.view") if object.len() == 1 => {
            let session = optional_id(&args, "session_id")?
                .filter(|id| server.sessions.iter().any(|s| s.id == *id))
                .ok_or_else(|| error("refused", "Relay session does not belong to destination"))?;
            ("session.view", Some(session.to_owned()))
        }
        _ => {
            return Err(error(
                "not_supported",
                "Only verified read-only relay methods are supported",
            ))
        }
    };
    Ok(Plan::Rpc {
        server: server.id.clone(),
        session,
        calls: vec![(method, args)],
        follow_focus: false,
    })
}

#[derive(Default)]
pub(super) struct ClientRequests {
    waiting: std::collections::BTreeMap<
        String,
        std::collections::VecDeque<(ClientRequest, std::time::Instant)>,
    >,
    busy: std::collections::HashSet<String>,
}

impl ClientRequests {
    fn enqueue(&mut self, requests: Vec<ClientRequest>) {
        for request in requests {
            self.waiting
                .entry(request.server_id.clone())
                .or_default()
                .push_back((request, std::time::Instant::now()));
        }
    }

    fn ready(&mut self) -> Vec<(ClientRequest, std::time::Instant)> {
        let mut ready = Vec::new();
        for (server, queue) in &mut self.waiting {
            if !self.busy.contains(server) {
                if let Some(request) = queue.pop_front() {
                    self.busy.insert(server.clone());
                    ready.push(request);
                }
            }
        }
        self.waiting.retain(|_, queue| !queue.is_empty());
        ready
    }

    fn finish(&mut self, server: &str) {
        self.busy.remove(server);
    }
}

async fn execute<C, F>(calls: Vec<(&'static str, Value)>, mut call: C) -> Reply
where
    C: FnMut(&'static str, Value) -> F,
    F: std::future::Future<Output = Reply>,
{
    let mut result = Value::Null;
    for (method, payload) in calls {
        result = call(method, payload).await?;
    }
    Ok(result)
}

async fn until<F, T>(work: F, alarm: T) -> Reply
where
    F: std::future::Future<Output = Reply>,
    T: std::future::Future,
{
    use std::{pin::pin, task::Poll};
    let mut work = pin!(work);
    let mut alarm = pin!(alarm);
    std::future::poll_fn(move |cx| {
        if alarm.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(error(
                "internal",
                "Remote request timed out; no further work will be started",
            )));
        }
        work.as_mut().poll(cx)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use flex_engine::{BlockState, ServerState, SessionState, WindowState};

    fn state() -> AppState {
        AppState {
            servers: ["one", "two"]
                .into_iter()
                .map(|id| ServerState {
                    id: id.into(),
                    name: id.into(),
                    connectivity: Connectivity::Online,
                    sessions: vec![SessionState {
                        id: format!("{id}-session"),
                        active_window_id: Some("window".into()),
                        windows: vec![WindowState {
                            id: "window".into(),
                            focused_block_id: Some(format!("{id}-block")),
                            blocks: vec![BlockState {
                                id: format!("{id}-block"),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
        }
    }

    fn selection() -> Selection<'static> {
        Selection {
            server: Some("one"),
            session: Some("one-session"),
            window: Some("window"),
            block: Some("one-block"),
        }
    }

    fn request(payload: Value) -> ClientRequest {
        ClientRequest {
            event_id: "route".into(),
            server_id: "one".into(),
            request_type: "client_perform_requested".into(),
            call_id: Some("call".into()),
            payload,
        }
    }

    #[test]
    fn close_is_an_owned_rpc_not_action_forwarding() {
        assert_eq!(
            plan(
                true,
                &state(),
                selection(),
                &request(json!({"action":"pane.close"}))
            )
            .unwrap(),
            Plan::Rpc {
                server: "one".into(),
                session: Some("one-session".into()),
                calls: vec![(
                    "block.close",
                    json!({"session_id":"one-session","block_id":"one-block"})
                )],
                follow_focus: false,
            }
        );
    }

    #[test]
    fn rejects_cross_server_session_block_and_implicit_selection() {
        for payload in [
            json!({"action":"pane.close","session_id":"two-session"}),
            json!({"action":"pane.close","block_id":"two-block"}),
            json!({"action":"pane.close","session_id":42}),
            json!({"action":"pane.close","block_id":""}),
        ] {
            assert_eq!(
                plan(true, &state(), selection(), &request(payload))
                    .unwrap_err()
                    .code,
                "refused"
            );
        }
        let mut r = request(json!({"action":"pane.close"}));
        r.server_id = "two".into();
        assert_eq!(
            plan(true, &state(), selection(), &r).unwrap_err().code,
            "refused"
        );
        r.payload["session_id"] = json!("two-session");
        assert!(plan(true, &state(), selection(), &r).is_ok());
    }

    #[test]
    fn rejects_unknown_actions_bad_args_and_dangerous_local_actions() {
        for payload in [
            json!({"action":"shell.run"}),
            json!({"action":"pane.close","args":{"unexpected":true}}),
            json!({"action":"pane.split","args":{"direction":"diagonal"}}),
        ] {
            assert_eq!(
                plan(true, &state(), selection(), &request(payload))
                    .unwrap_err()
                    .code,
                "not_supported"
            );
        }
        for name in [
            "flex.copy",
            "flex.paste",
            "flex.copy_transcript",
            "flex.prefix",
            "flex.palette",
            "flex.rename_tab",
            "flex.rename_pane",
        ] {
            assert_eq!(
                plan(
                    true,
                    &state(),
                    selection(),
                    &request(json!({"action":name}))
                )
                .unwrap_err()
                .code,
                "refused",
                "{name}"
            );
        }
    }

    #[test]
    fn registered_pane_and_tab_actions_build_explicit_rpc_calls() {
        for (name, args, method) in [
            (
                "pane.split",
                json!({"direction":"right"}),
                "session.new_split",
            ),
            ("flex.split_down", Value::Null, "session.new_split"),
            ("pane.zoom", Value::Null, "session.zoom_block"),
            ("client.tab.new", Value::Null, "session.new_window"),
            (
                "pane.focus",
                json!({"direction":"left"}),
                "session.focus_direction",
            ),
            ("flex.next_pane", Value::Null, "session.focus_next_pane"),
            (
                "client.tab.previous",
                Value::Null,
                "session.focus_previous_window",
            ),
            (
                "flex.clear_pane",
                Value::Null,
                "com.superlogical.terminal.clear",
            ),
        ] {
            let Plan::Rpc {
                server,
                session,
                calls,
                ..
            } = plan(
                true,
                &state(),
                selection(),
                &request(json!({"action":name,"args":args})),
            )
            .unwrap()
            else {
                panic!("expected RPC for {name}")
            };
            assert_eq!(server, "one");
            assert_eq!(session.as_deref(), Some("one-session"));
            assert_eq!(calls.last().unwrap().0, method);
            assert!(calls.iter().all(|(_, p)| p["session_id"] == "one-session"));
            if method == "session.new_split" {
                assert_eq!(calls[0].1["anchor_id"], "one-block");
            }
            if method.starts_with("session.focus_") {
                assert_eq!(
                    calls[0],
                    (
                        "session.focus_block",
                        json!({"session_id":"one-session","block_id":"one-block"})
                    )
                );
            }
        }
    }

    #[test]
    fn local_ui_actions_cannot_touch_an_unrelated_selected_server() {
        let mut r = request(json!({"action":"flex.settings","session_id":"two-session"}));
        r.server_id = "two".into();
        assert_eq!(
            plan(true, &state(), selection(), &r).unwrap_err().code,
            "refused"
        );
        assert_eq!(
            plan(
                true,
                &state(),
                selection(),
                &request(json!({"action":"flex.toggle_sidebar"}))
            )
            .unwrap(),
            Plan::Local(Action::ToggleSidebar)
        );
    }

    #[test]
    fn relay_accepts_only_saved_online_targets_and_verified_read_only_schemas() {
        let mut r = request(
            json!({"call_id":"call","server":"two","method":"session.view","args":{"session_id":"two-session"}}),
        );
        r.request_type = "client_call_requested".into();
        let Plan::Rpc { server, calls, .. } = plan(true, &state(), selection(), &r).unwrap() else {
            panic!("expected relay")
        };
        assert_eq!(server, "two");
        assert_eq!(
            calls,
            vec![("session.view", json!({"session_id":"two-session"}))]
        );
        for payload in [
            json!({"server":"http://attacker:1234","method":"server.status","args":{}}),
            json!({"server":"two","method":"session.view","args":{"session_id":"one-session"}}),
            json!({"server":"two","method":"session.view","args":{"session_id":"two-session","unexpected":true}}),
            json!({"server":"two","method":"client.action","args":{"action":"run"}}),
            json!({"server":"two","method":"com.superlogical.terminal.write","args":{}}),
        ] {
            r.payload = payload;
            assert!(plan(true, &state(), selection(), &r).is_err());
        }
        r.payload = json!({"server":"two","method":"server.status"});
        let mut offline = state();
        offline.servers[1].connectivity = Connectivity::Idle;
        assert_eq!(
            plan(true, &offline, selection(), &r).unwrap_err().code,
            "refused"
        );
    }

    #[test]
    fn queue_serializes_each_server_while_other_servers_can_progress() {
        let mut queue = ClientRequests::default();
        let a = request(json!({"action":"pane.close"}));
        let mut b = a.clone();
        b.event_id = "second".into();
        let mut other = a.clone();
        other.server_id = "two".into();
        queue.enqueue(vec![a, b, other]);
        let first = queue.ready();
        assert_eq!(first.len(), 2);
        assert!(queue.ready().is_empty());
        queue.finish("one");
        let second = queue.ready();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].0.event_id, "second");
        assert!(queue.ready().is_empty());
    }

    #[test]
    fn execution_awaits_rpc_and_stops_after_failure() {
        use std::{
            cell::Cell,
            future::{poll_fn, Future},
            pin::pin,
            rc::Rc,
            task::{Context, Poll, Waker},
        };
        let ready = Rc::new(Cell::new(false));
        let calls = Rc::new(Cell::new(0));
        let mut future = pin!(execute(
            vec![
                ("first", json!({})),
                ("second", json!({})),
                ("never", json!({}))
            ],
            {
                let ready = ready.clone();
                let calls = calls.clone();
                move |method, _| {
                    calls.set(calls.get() + 1);
                    let ready = ready.clone();
                    poll_fn(move |_| {
                        if !ready.get() {
                            Poll::Pending
                        } else if method == "second" {
                            Poll::Ready(Err(error("internal", "RPC failed")))
                        } else {
                            Poll::Ready(Ok(Value::Null))
                        }
                    })
                }
            }
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(calls.get(), 1);
        ready.set(true);
        assert_eq!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(error("internal", "RPC failed")))
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn expired_deadline_never_starts_work() {
        use std::{
            cell::Cell,
            future::{ready, Future},
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let started = Cell::new(false);
        let work = async {
            started.set(true);
            Ok(Value::Null)
        };
        let mut future = pin!(until(work, ready(())));
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(error(
                "internal",
                "Remote request timed out; no further work will be started"
            )))
        );
        assert!(!started.get());
    }

    #[test]
    fn swap_tab_order_and_floating_layer_actions_use_owned_ids() {
        let mut state = state();
        state.servers[0].sessions[0].windows[0]
            .blocks
            .push(BlockState {
                id: "neighbor".into(),
                ..Default::default()
            });
        state.servers[0].sessions[0].windows.push(WindowState {
            id: "next-window".into(),
            ..Default::default()
        });
        for (action, method, key, value) in [
            (
                "flex.swap_pane",
                "session.swap_blocks",
                "other_block_id",
                "neighbor",
            ),
            (
                "flex.move_tab_right",
                "session.move_window",
                "window_id",
                "window",
            ),
        ] {
            let Plan::Rpc { calls, .. } = plan(
                true,
                &state,
                selection(),
                &request(json!({"action":action})),
            )
            .unwrap() else {
                panic!("RPC expected")
            };
            assert_eq!(calls[0].0, method);
            assert_eq!(calls[0].1[key], value);
        }
        let block = &mut state.servers[0].sessions[0].windows[0].blocks[0];
        block.floating = true;
        block.layer_id = Some("owned-layer".into());
        block.layer_kind = "floating".into();
        for (action, method) in [
            ("flex.minimize_float", "session.set_layer_state"),
            ("flex.raise_float", "session.raise_layer"),
            ("flex.lower_float", "session.lower_layer"),
        ] {
            let Plan::Rpc { calls, .. } = plan(
                true,
                &state,
                selection(),
                &request(json!({"action":action})),
            )
            .unwrap() else {
                panic!("RPC expected")
            };
            assert_eq!(calls[0].0, method);
            assert_eq!(calls[0].1["layer_id"], "owned-layer");
        }
    }

    #[test]
    fn new_tab_can_recover_an_empty_session_without_an_anchor() {
        let mut state = state();
        state.servers[0].sessions[0].windows.clear();
        let selection = Selection {
            server: Some("one"),
            session: Some("one-session"),
            ..Default::default()
        };
        let Plan::Rpc { calls, .. } = plan(
            true,
            &state,
            selection,
            &request(json!({"action":"client.tab.new"})),
        )
        .unwrap() else {
            panic!("RPC expected")
        };
        assert_eq!(calls[0].0, "session.new_window");
        assert_eq!(calls[0].1["session_id"], "one-session");
    }

    #[test]
    fn remote_float_cannot_strand_a_solo_pane_or_move_it_to_another_tab() {
        let mut state = state();
        let r = request(json!({"action":"pane.float"}));
        assert_eq!(
            plan(true, &state, selection(), &r).unwrap_err().code,
            "refused"
        );
        state.servers[0].sessions[0].windows[0].blocks[0].floating = true;
        state.servers[0].sessions[0].windows.push(WindowState {
            id: "elsewhere".into(),
            blocks: vec![BlockState {
                id: "anchor".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(
            plan(true, &state, selection(), &r).unwrap_err().code,
            "refused"
        );
        state.servers[0].sessions[0].windows[0]
            .blocks
            .push(BlockState {
                id: "same-tab-anchor".into(),
                ..Default::default()
            });
        let Plan::Rpc { calls, .. } = plan(true, &state, selection(), &r).unwrap() else {
            panic!("expected RPC")
        };
        assert_eq!(calls[0].1["anchor_block_id"], "same-tab-anchor");
    }

    #[test]
    fn remote_actions_are_refused_by_default() {
        let error = plan(
            false,
            &state(),
            selection(),
            &request(json!({"action":"pane.close"})),
        )
        .unwrap_err();
        assert_eq!(error.code, "refused");
    }
}

// ---- GPUI driver -----------------------------------------------------------
use super::Workspace;
use gpui::{Context, Window};
use std::time::{Duration, Instant};

impl Workspace {
    /// Drain at render, before normal keyboard actions. The server normally sends
    /// one perform at a time; our queue also covers requests after caller timeouts.
    pub(super) fn process_client_requests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.client_requests
            .enqueue(self.engine.take_client_requests());
        for (request, arrived) in self.client_requests.ready() {
            let deadline = arrived + Duration::from_secs(8);
            let prepared = if Instant::now() >= deadline {
                Err(error("refused", "Queued request expired before execution"))
            } else {
                plan(
                    self.settings.allow_remote_actions,
                    &self.engine.state(),
                    Selection {
                        server: self.server_id.as_deref(),
                        session: self.session_id.as_deref(),
                        window: self.window_id.as_deref(),
                        block: self.active_block.as_deref(),
                    },
                    &request,
                )
            };
            // These are the only actions that are entirely synchronous. Never
            // call run(): its RPC-backed branches detach before reporting errors.
            let prepared = match prepared {
                Ok(Plan::Local(_)) if self.input_overlay_open() => Err(error(
                    "refused",
                    "A local input overlay owns keyboard focus",
                )),
                Ok(Plan::Local(action)) => {
                    let result = match action {
                        Action::ToggleSidebar => {
                            self.settings.sidebar = !self.settings.sidebar;
                            self.settings.save(&self.engine);
                            cx.notify();
                            Ok(Value::Null)
                        }
                        Action::Settings => {
                            if self.show_settings {
                                self.close_settings(window, cx);
                            } else {
                                self.show_settings = true;
                                window.focus(&self.focus);
                                cx.notify();
                            }
                            Ok(Value::Null)
                        }
                        Action::Find
                            if !self.show_settings
                                && self
                                    .active_block
                                    .as_ref()
                                    .is_some_and(|b| self.terminals.contains_key(b)) =>
                        {
                            self.start_find(cx);
                            Ok(Value::Null)
                        }
                        _ => Err(error("refused", "The requested local UI is not available")),
                    };
                    result.map(|_| None)
                }
                Ok(rpc) => Ok(Some(rpc)),
                Err(e) => Err(e),
            };
            let engine = self.engine.clone();
            let ui_guard = (
                self.server_id.clone(),
                self.session_id.clone(),
                self.window_id.clone(),
                self.active_block.clone(),
            );
            cx.spawn(async move |this, cx| {
                let result = match prepared {
                    Ok(Some(Plan::Rpc { server, session, calls, follow_focus })) => {
                        until(async {
                            let allowed = this.read_with(cx, |me, _| me.settings.allow_remote_actions).unwrap_or(false);
                            if !allowed { return Err(error("refused", "Remote client actions were disabled")); }
                            let result = execute(calls, |method, payload| {
                                let engine = engine.clone(); let server = server.clone();
                                async move { engine.call_server(&server, method, payload).await.map_err(|e| error("internal", &e.to_string())) }
                            }).await?;
                            if follow_focus {
                                if let Some(session) = &session {
                                    let view = engine.call_server(&server, "session.view", json!({"session_id":session})).await
                                        .map_err(|e| error("internal", &format!("Action completed but focus refresh failed: {e}")))?;
                                    this.update(cx, |me, cx| {
                                        if !me.input_overlay_open() && me.server_id.as_deref() == Some(server.as_str()) && me.session_id.as_deref() == Some(session.as_str())
                                            && (me.server_id.clone(), me.session_id.clone(), me.window_id.clone(), me.active_block.clone()) == ui_guard {
                                            me.adopt_focus(&view);
                                            cx.notify();
                                        }
                                    }).ok();
                                }
                            }
                            Ok(result)
                        }, gpui::Timer::after(deadline.saturating_duration_since(Instant::now()))).await
                    }
                    Ok(_) => Ok(Value::Null),
                    Err(e) => Err(e),
                };
                // A request is answered on its retained original connection,
                // even when disabled, malformed, unsupported or unsuccessful.
                let reply = until(async {
                    engine.reply_client_request(&request.event_id, result).await
                        .map(|_| Value::Null).map_err(|e| error("internal", &e.to_string()))
                }, gpui::Timer::after(Duration::from_secs(2))).await;
                if let Err(e) = reply { eprintln!("flex: client.reply {}: {}", request.event_id, e.message); }
                this.update(cx, |me, cx| {
                    me.client_requests.finish(&request.server_id);
                    cx.notify();
                }).ok();
            }).detach();
        }
    }

    /// Add this list to outgoing client.action as `servers`. Names are saved
    /// labels, IDs are opaque saved profile IDs, never addresses to dial.
    pub(super) fn client_action_servers(&self, origin: &str) -> Value {
        let state = self.engine.state();
        let connected: Vec<_> = state
            .servers
            .iter()
            .filter(|s| {
                s.id != origin && s.connectivity == Connectivity::Online && !s.name.is_empty()
            })
            .collect();
        Value::Array(
            connected
                .iter()
                .filter(|s| {
                    connected
                        .iter()
                        .filter(|other| other.name == s.name)
                        .count()
                        == 1
                })
                .map(|s| json!({"name":s.name,"id":s.id}))
                .collect(),
        )
    }
}
