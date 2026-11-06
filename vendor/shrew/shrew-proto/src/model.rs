//! Typed views of the server's state and events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::control::EventBody;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct SessionSummary {
    pub session_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct SessionView {
    pub revision: u64,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub active_window_id: Option<String>,
    #[serde(default, deserialize_with = "null_vec")]
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct Window {
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused_block_id: Option<String>,
    #[serde(default)]
    pub zoomed_block_id: Option<String>,
    #[serde(default, deserialize_with = "null_vec")]
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct Layer {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub layer_id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub bounds: [f64; 4],
    #[serde(default)]
    pub layout: Option<LayoutNode>,
    #[serde(default, deserialize_with = "null_vec")]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct LayoutNode {
    pub rect: [f64; 4],
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub split: Option<Box<Split>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Split {
    #[serde(default)]
    pub split_id: Option<String>,
    #[serde(default)]
    pub direction: String,
    pub ratio: f64,
    pub before: LayoutNode,
    pub after: LayoutNode,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct Block {
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub creator_name: String,
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    pub host_id: String,
    #[serde(default)]
    pub owner: Option<String>,
}

/// A block placed at a rectangle (fractions of the window, `[x, y, w, h]`).
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedBlock {
    pub block_id: String,
    pub rect: [f64; 4],
    pub floating: bool,
}

impl Window {
    pub fn id(&self) -> &str {
        self.window_id.as_deref().unwrap_or("")
    }

    /// Every block in this window with where it sits, back to front.
    pub fn placed_blocks(&self) -> Vec<PlacedBlock> {
        let mut out = Vec::new();
        for layer in &self.layers {
            let floating = layer.kind != "tiled";
            if let Some(node) = &layer.layout {
                collect(node, floating, &mut out);
            }
        }
        out
    }

    pub fn block(&self, id: &str) -> Option<&Block> {
        self.layers.iter().flat_map(|l| &l.blocks).find(|b| b.block_id.as_deref() == Some(id))
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.layers.iter().flat_map(|l| &l.blocks)
    }
}

fn collect(node: &LayoutNode, floating: bool, out: &mut Vec<PlacedBlock>) {
    if let Some(id) = &node.block_id {
        out.push(PlacedBlock { block_id: id.clone(), rect: node.rect, floating });
    } else if let Some(split) = &node.split {
        collect(&split.before, floating, out);
        collect(&split.after, floating, out);
    }
}

impl SessionView {
    pub fn active_window(&self) -> Option<&Window> {
        let id = self.active_window_id.as_deref()?;
        self.windows.iter().find(|w| w.window_id.as_deref() == Some(id))
    }

    pub fn find_block(&self, id: &str) -> Option<(&Window, &Block)> {
        self.windows.iter().find_map(|w| w.block(id).map(|b| (w, b)))
    }

    pub fn all_blocks(&self) -> impl Iterator<Item = (&Window, &Block)> {
        self.windows.iter().flat_map(|w| w.blocks().map(move |b| (w, b)))
    }
}

fn null_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

/// What a program in a terminal reports about itself (OSC 7501).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct ProgramStatus {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub progress: Option<u8>,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub effective_app: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub msg: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentState {
    Idle,
    Done,
    Working,
    Error,
    Blocked,
}

impl AgentState {
    pub fn parse(s: &str) -> Self {
        match s {
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct Process {
    #[serde(default)]
    pub pid: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// Events from a terminal block.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    Bell,
    ChildExited { exit_code: i32, error: Option<String> },
    Clipboard { text: Option<String>, id: Option<u64> },
    Notification { title: String, body: String },
    ProcessChanged { foreground: Option<Process> },
    StatusChanged { id: String, record: ProgramStatus },
    StatusRemoved { ids: Vec<String> },
    Progress { state: String, progress: Option<u8> },
    PwdChanged { pwd: String },
    SelectionCopied { text: Option<String>, id: Option<u64> },
    SizeChanged { columns: u32, rows: u32 },
    TitleChanged { title: String },
    UrlClicked { url: String },
    Other { name: String, payload: Value },
}

impl TerminalEvent {
    pub fn parse(name: &str, p: &Value) -> Self {
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        let u = |k: &str| p.get(k).and_then(Value::as_u64);
        match name {
            "bell" => Self::Bell,
            "child_exited" => {
                Self::ChildExited { exit_code: p.get("exit_code").and_then(Value::as_i64).unwrap_or(-1) as i32, error: s("error") }
            }
            "clipboard_written" => Self::Clipboard { text: s("text"), id: u("id") },
            "desktop_notification" => Self::Notification { title: s("title").unwrap_or_default(), body: s("body").unwrap_or_default() },
            "process_changed" => {
                Self::ProcessChanged { foreground: p.get("foreground").and_then(|f| serde_json::from_value(f.clone()).ok()) }
            }
            "program_status_changed" => Self::StatusChanged {
                id: s("id").unwrap_or_default(),
                record: p.get("record").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default(),
            },
            "program_status_removed" => {
                Self::StatusRemoved { ids: p.get("ids").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default() }
            }
            "progress_report" => {
                Self::Progress { state: s("state").unwrap_or_default(), progress: u("progress").map(|v| v.min(100) as u8) }
            }
            "pwd_changed" => Self::PwdChanged { pwd: s("pwd").unwrap_or_default() },
            "selection_copied" => Self::SelectionCopied { text: s("text"), id: u("id") },
            "size_changed" => Self::SizeChanged { columns: u("columns").unwrap_or(0) as u32, rows: u("rows").unwrap_or(0) as u32 },
            "title_changed" => Self::TitleChanged { title: s("title").unwrap_or_default() },
            "url_clicked" => Self::UrlClicked { url: s("url").unwrap_or_default() },
            _ => Self::Other { name: name.to_owned(), payload: p.clone() },
        }
    }
}

/// A server event, decoded as far as Shrew cares.
#[allow(clippy::large_enum_variant)] // short-lived; boxing buys nothing here
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    SessionCreated(SessionSummary),
    SessionDestroyed { session_id: String },
    SessionMoved { session_id: String },
    Session { session_id: String, event: SessionEvent },
    HostsChanged { payload: Value },
    KeymapChanged { payload: Value },
    ClientRequest(EventBody),
    Other(EventBody),
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    ViewChanged { revision: u64 },
    LabelChanged { label: String },
    ActiveWindowChanged { window_id: Option<String> },
    WindowCreated { window_id: String, label: String },
    WindowClosed { window_id: String },
    WindowLabelChanged { window_id: String, label: String },
    BlockCreated { block_id: String },
    BlockClosed { block_id: String },
    BlockOwnerChanged { block_id: String, owner: Option<String> },
    Block { block_id: String, creator: String, event: TerminalEvent },
    ClientConnected { client_id: String, display: String },
    ClientDisconnected { client_id: String },
    Other(EventBody),
}

impl Event {
    pub fn decode(body: EventBody) -> Self {
        let p = &body.payload;
        let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        match body.kind.as_str() {
            "session_created" => p
                .get("session")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .map(Event::SessionCreated)
                .unwrap_or(Event::Other(body)),
            "session_destroyed" => Event::SessionDestroyed { session_id: s("session_id") },
            "session_moved" => Event::SessionMoved { session_id: s("session_id") },
            "hosts_changed" => Event::HostsChanged { payload: body.payload },
            "keymap_changed" => Event::KeymapChanged { payload: body.payload },
            "client_perform_requested" | "client_call_requested" => Event::ClientRequest(body),
            "session_event" => {
                let session_id = s("session_id");
                match p.get("event").and_then(|e| serde_json::from_value::<EventBody>(e.clone()).ok()) {
                    Some(inner) => Event::Session { session_id, event: SessionEvent::decode(inner) },
                    None => Event::Other(body),
                }
            }
            _ => Event::Other(body),
        }
    }
}

impl SessionEvent {
    pub fn decode(body: EventBody) -> Self {
        let p = &body.payload;
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        let req = |k: &str| s(k).unwrap_or_default();
        match body.kind.as_str() {
            "session_view_changed" => Self::ViewChanged { revision: p.get("revision").and_then(Value::as_u64).unwrap_or(0) },
            "session_label_changed" => Self::LabelChanged { label: req("label") },
            "active_window_changed" => Self::ActiveWindowChanged { window_id: s("window_id") },
            "window_created" => Self::WindowCreated { window_id: req("window_id"), label: req("label") },
            "window_closed" => Self::WindowClosed { window_id: req("window_id") },
            "window_label_changed" => Self::WindowLabelChanged { window_id: req("window_id"), label: req("label") },
            "block_created" => Self::BlockCreated { block_id: req("block_id") },
            "block_closed" => Self::BlockClosed { block_id: req("block_id") },
            "block_owner_changed" => Self::BlockOwnerChanged { block_id: req("block_id"), owner: s("owner") },
            "block_event" => Self::Block {
                block_id: req("block_id"),
                creator: req("creator_name"),
                event: TerminalEvent::parse(&req("name"), p.get("payload").unwrap_or(&Value::Null)),
            },
            "client_connected" => Self::ClientConnected {
                client_id: req("client_id"),
                display: p.pointer("/principal/display").and_then(Value::as_str).unwrap_or_default().to_owned(),
            },
            "client_disconnected" => Self::ClientDisconnected { client_id: req("client_id") },
            _ => Self::Other(body),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn view_layout_flattening() {
        let v: SessionView = serde_json::from_value(json!({
            "revision": 3, "session_id": "s", "label": "work", "active_window_id": "w",
            "windows": [{"window_id": "w", "label": "1", "focused_block_id": "b2", "layers": [{
                "layer_id": "l", "kind": "tiled", "bounds": [0,0,1,1],
                "layout": {"rect": [0,0,1,1], "split": {"split_id": "x", "direction": "horizontal", "ratio": 0.5,
                    "before": {"rect": [0,0,0.5,1], "block_id": "b1"},
                    "after": {"rect": [0.5,0,0.5,1], "block_id": "b2"}}},
                "blocks": [{"block_id": "b1"}, {"block_id": "b2", "label": "logs"}]
            }]}]
        }))
        .unwrap();
        let w = v.active_window().unwrap();
        let placed = w.placed_blocks();
        assert_eq!(placed.len(), 2);
        assert_eq!(placed[1].rect, [0.5, 0.0, 0.5, 1.0]);
        assert_eq!(v.find_block("b2").unwrap().1.label, "logs");
    }

    #[test]
    fn client_requests_preserve_the_entire_payload() {
        for kind in ["client_perform_requested", "client_call_requested"] {
            let payload =
                json!({"call_id": "1", "args": [null, {"nested": true}], "requester": {"principal": {"uid": 42}}, "future": "keep"});
            let event = Event::decode(EventBody { kind: kind.into(), payload: payload.clone() });
            assert_eq!(event, Event::ClientRequest(EventBody { kind: kind.into(), payload }));
        }
    }

    #[test]
    fn cache_invalidations_preserve_hash_and_generation() {
        let payload = json!({"hash": "abc", "generation": 2});
        assert_eq!(
            Event::decode(EventBody { kind: "keymap_changed".into(), payload: payload.clone() }),
            Event::KeymapChanged { payload: payload.clone() }
        );
        assert_eq!(Event::decode(EventBody { kind: "hosts_changed".into(), payload: payload.clone() }), Event::HostsChanged { payload });
    }

    #[test]
    fn decodes_nested_block_event() {
        let body: EventBody = serde_json::from_value(json!({"type": "session_event", "payload": {
            "session_id": "s", "event": {"type": "block_event", "payload": {
                "block_id": "b", "creator_name": "com.superlogical.terminal", "name": "program_status_changed",
                "payload": {"id": "", "record": {"state": "blocked", "kind": "permission", "app": "claude"}}}}}}))
        .unwrap();
        match Event::decode(body) {
            Event::Session { event: SessionEvent::Block { event: TerminalEvent::StatusChanged { record, .. }, .. }, .. } => {
                assert_eq!(AgentState::parse(&record.state), AgentState::Blocked);
                assert_eq!(record.kind.as_deref(), Some("permission"));
            }
            e => panic!("{e:?}"),
        }
    }
}
