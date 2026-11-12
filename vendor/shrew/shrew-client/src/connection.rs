use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use shrew_proto::{
    API_MIN_REVISION, API_REVISION, CONTROL_SUBPROTOCOL, ClientInfo, ClientMessage, Event, Response, ServerMessage, SessionSummary,
    SessionView, TERMINAL_CREATOR, TERMINAL_SUBPROTOCOL, Welcome,
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::{ClientError, Endpoint};

/// How often we ping, and how long silence may last before we give up.
/// Phones drop off networks without a FIN, so detect it quickly.
const PING_EVERY: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(40);

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub client: ClientInfo,
    pub reconnect_token: Option<String>,
    pub timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            client: ClientInfo {
                kind: Some("app".into()),
                platform: Some(platform_name().into()),
                name: Some("Shrew".into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                label: None,
            },
            reconnect_token: None,
            timeout: Duration::from_secs(10),
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ConnEvent {
    Event { seq: u64, event: Event },
    Closed { reason: String },
}

struct Inner {
    out: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<String, oneshot::Sender<Response>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    /// When the server was last heard from, as ms since `born`.
    heard_ms: AtomicU64,
    born: Instant,
    welcome: Welcome,
    endpoint: Endpoint,
    timeout: Duration,
}

/// The reply to a request that [`Connection::start`] already queued.
pub type Reply = std::pin::Pin<Box<dyn Future<Output = Result<(u64, Value), ClientError>> + Send>>;

/// A live control connection. Cheap to clone; all clones share one socket.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").field("endpoint", &self.inner.endpoint).finish()
    }
}

impl Connection {
    pub async fn connect(endpoint: &Endpoint, opts: ConnectOptions) -> Result<(Self, mpsc::UnboundedReceiver<ConnEvent>), ClientError> {
        let fut = Self::connect_inner(endpoint, opts.clone());
        tokio::time::timeout(opts.timeout, fut).await.map_err(|_| ClientError::Timeout)?
    }

    async fn connect_inner(endpoint: &Endpoint, opts: ConnectOptions) -> Result<(Self, mpsc::UnboundedReceiver<ConnEvent>), ClientError> {
        let mut ws = endpoint.websocket("/api/control", CONTROL_SUBPROTOCOL).await?;
        let hello = ClientMessage::Hello {
            reconnect_token: opts.reconnect_token.clone(),
            client: opts.client.clone(),
            api_revision: API_REVISION,
            api_min_revision: API_MIN_REVISION,
        };
        ws.send(Message::text(serde_json::to_string(&hello)?)).await?;
        let welcome = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => match ServerMessage::parse(&t)? {
                    ServerMessage::Welcome(w) => break w,
                    _ => return Err(ClientError::Protocol("first message was not a welcome".into())),
                },
                Some(Ok(Message::Close(f))) => {
                    return Err(ClientError::Protocol(f.map(|f| f.reason.to_string()).unwrap_or_else(|| "closed".into())));
                }
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => return Err(ClientError::Closed),
            }
        };
        if welcome.protocol != CONTROL_SUBPROTOCOL {
            return Err(ClientError::Protocol(format!("unexpected protocol {}", welcome.protocol)));
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            out: out_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            heard_ms: AtomicU64::new(0),
            born: Instant::now(),
            welcome,
            endpoint: endpoint.clone(),
            timeout: opts.timeout,
        });

        let (mut sink, mut stream) = ws.split();
        let reader_inner = inner.clone();
        tokio::spawn(async move {
            let mut last_heard = Instant::now();
            let mut ticker = tokio::time::interval(PING_EVERY);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let reason = loop {
                tokio::select! {
                    msg = stream.next() => {
                        last_heard = Instant::now();
                        reader_inner.heard_ms.store(reader_inner.born.elapsed().as_millis() as u64, Ordering::Relaxed);
                        match msg {
                            Some(Ok(Message::Text(t))) => match ServerMessage::parse(&t) {
                                Ok(ServerMessage::Response(r)) => {
                                    let tx = reader_inner.pending.lock().unwrap().remove(&r.id);
                                    if let Some(tx) = tx { let _ = tx.send(r); }
                                }
                                Ok(ServerMessage::Event(e)) => {
                                    let _ = ev_tx.send(ConnEvent::Event { seq: e.seq, event: Event::decode(e.payload) });
                                }
                                Ok(_) => {}
                                Err(e) => tracing::warn!("bad message from server: {e}"),
                            },
                            Some(Ok(Message::Close(f))) => break f.map(|f| format!("{} {}", u16::from(f.code), f.reason)).unwrap_or_else(|| "closed".into()),
                            Some(Ok(_)) => {}
                            Some(Err(e)) => break e.to_string(),
                            None => break "connection lost".into(),
                        }
                    }
                    out = out_rx.recv() => match out {
                        Some(m) => if let Err(e) = sink.send(m).await { break e.to_string(); },
                        None => { let _ = sink.send(Message::Close(None)).await; break "closed by client".into(); }
                    },
                    _ = ticker.tick() => {
                        if last_heard.elapsed() > DEAD_AFTER { break "server stopped answering".into(); }
                        if let Err(e) = sink.send(Message::Ping(Default::default())).await { break e.to_string(); }
                    }
                }
            };
            reader_inner.closed.store(true, Ordering::SeqCst);
            reader_inner.pending.lock().unwrap().clear(); // drops senders => callers see Closed
            let _ = ev_tx.send(ConnEvent::Closed { reason });
        });

        Ok((Self { inner }, ev_rx))
    }

    pub fn welcome(&self) -> &Welcome {
        &self.inner.welcome
    }

    pub fn client_id(&self) -> Option<&str> {
        self.inner.welcome.client.as_ref().map(|c| c.id.as_str())
    }

    pub fn reconnect_token(&self) -> Option<&str> {
        self.inner.welcome.client.as_ref().map(|c| c.reconnect_token.as_str())
    }

    pub fn resumed(&self) -> bool {
        self.inner.welcome.client.as_ref().is_some_and(|c| c.resumed)
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// How long since anything arrived from the server. Pongs arrive every
    /// [`PING_EVERY`], so much more than that means the socket is probably dead
    /// (e.g. the app was frozen in the background) even if it isn't closed yet.
    pub fn silent_for(&self) -> Duration {
        let heard = Duration::from_millis(self.inner.heard_ms.load(Ordering::Relaxed));
        self.inner.born.elapsed().saturating_sub(heard)
    }

    /// True when the connection is open and has heard from the server recently.
    pub fn is_fresh(&self) -> bool {
        !self.is_closed() && self.silent_for() < PING_EVERY + Duration::from_secs(5)
    }

    /// Close politely (with a close frame, which ends the client on the server).
    pub fn close(&self) {
        let _ = self.inner.out.send(Message::Close(None));
    }

    /// Call a method; returns the response's `seq` and its payload.
    pub async fn call_seq(&self, method: &str, payload: Value) -> Result<(u64, Value), ClientError> {
        self.start(method, payload)?.await
    }

    /// Queue a request on the socket now and return its reply to await later.
    /// Requests reach the server in the order `start` was called, so several
    /// can be in flight at once. Rex runs `client.key`, `client.action`,
    /// `block.event` and the focus/layout methods in arrival order; other
    /// methods (terminal `write` among them) run concurrently and must be
    /// awaited one at a time when their order matters.
    pub fn start(&self, method: &str, payload: Value) -> Result<Reply, ClientError> {
        if self.is_closed() {
            return Err(ClientError::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id.clone(), tx);
        let req = ClientMessage::Request {
            id: id.clone(),
            method: method.to_owned(),
            payload: if payload.is_null() { None } else { Some(payload) },
        };
        let text = serde_json::to_string(&req)?;
        if text.len() as u64 > self.inner.welcome.limits.max_message_bytes {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(ClientError::Protocol("request too large".into()));
        }
        if self.inner.out.send(Message::text(text)).is_err() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(ClientError::Closed);
        }
        let inner = self.inner.clone();
        let pending = PendingRegistration { inner: Arc::downgrade(&inner), id: id.clone() };
        Ok(Box::pin(async move {
            let _pending = pending;
            let resp = match tokio::time::timeout(inner.timeout * 3, rx).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => return Err(ClientError::Closed),
                Err(_) => {
                    inner.pending.lock().unwrap().remove(&id);
                    return Err(ClientError::Timeout);
                }
            };
            if let Some(e) = resp.error {
                return Err(ClientError::Api(e));
            }
            Ok((resp.seq, resp.payload.unwrap_or(Value::Null)))
        }))
    }

    pub async fn call(&self, method: &str, payload: Value) -> Result<Value, ClientError> {
        self.call_seq(method, payload).await.map(|(_, v)| v)
    }

    pub async fn call_as<T: for<'de> Deserialize<'de>>(&self, method: &str, payload: Value) -> Result<T, ClientError> {
        Ok(serde_json::from_value(self.call(method, payload).await?)?)
    }

    // ---- typed helpers -------------------------------------------------------

    pub async fn bootstrap(&self, attach: Option<&[String]>, preferred: Option<&str>) -> Result<Bootstrap, ClientError> {
        let mut p = json!({
            "event_scope": "all",
            "data_tickets": [{"creator_name": TERMINAL_CREATOR, "data_ref_id": "terminal", "subprotocol": TERMINAL_SUBPROTOCOL}],
        });
        if let Some(ids) = attach {
            p["attach_session_ids"] = json!(ids);
        }
        if let Some(pref) = preferred {
            p["preferred_session_id"] = json!(pref);
        }
        let (seq, v) = self.call_seq("session.bootstrap", p).await?;
        let mut b: Bootstrap = serde_json::from_value(v)?;
        b.seq = seq;
        Ok(b)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default)]
            sessions: Option<Vec<SessionSummary>>,
        }
        Ok(self.call_as::<R>("session.list", json!({})).await?.sessions.unwrap_or_default())
    }

    pub async fn attach(&self, session_id: &str) -> Result<(u64, SessionView), ClientError> {
        let (seq, v) = self.call_seq("session.attach", json!({"session_id": session_id})).await?;
        let view = v.get("view").cloned().unwrap_or(v);
        Ok((seq, serde_json::from_value(view)?))
    }

    pub async fn detach(&self, session_id: &str) -> Result<(), ClientError> {
        self.call("session.detach", json!({"session_id": session_id})).await.map(drop)
    }

    pub async fn view(&self, session_id: &str) -> Result<(u64, SessionView), ClientError> {
        let (seq, v) = self.call_seq("session.view", json!({"session_id": session_id})).await?;
        Ok((seq, serde_json::from_value(v)?))
    }

    /// Create a session with one shell window. Returns (session_id, block_id).
    pub async fn create_session(&self, label: Option<&str>, cwd: Option<&str>) -> Result<(String, Option<String>), ClientError> {
        let mut p = json!({"initial_windows": [{"layout": shell_layout(cwd)}]});
        if let Some(l) = label.filter(|l| !l.is_empty()) {
            p["label"] = json!(l);
        }
        let v = self.call("session.create", p).await?;
        let sid = v.get("session_id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let bid = v.pointer("/initial_windows/0/block_ids/0").and_then(Value::as_str).map(str::to_owned);
        Ok((sid, bid))
    }

    pub async fn destroy_session(&self, session_id: &str) -> Result<(), ClientError> {
        self.call("session.destroy", json!({"session_id": session_id})).await.map(drop)
    }

    pub async fn rename_session(&self, session_id: &str, label: &str) -> Result<(), ClientError> {
        self.call("session.set_label", json!({"session_id": session_id, "label": label})).await.map(drop)
    }

    pub async fn new_window(&self, session_id: &str, cwd: Option<&str>) -> Result<Value, ClientError> {
        self.call("session.new_window", json!({"session_id": session_id, "layout": shell_layout(cwd)})).await
    }

    pub async fn close_window(&self, session_id: &str, window_id: &str) -> Result<(), ClientError> {
        self.call("session.close_window", json!({"session_id": session_id, "window_id": window_id})).await.map(drop)
    }

    pub async fn rename_window(&self, session_id: &str, window_id: &str, label: &str) -> Result<(), ClientError> {
        self.call("session.set_window_label", json!({"session_id": session_id, "window_id": window_id, "label": label})).await.map(drop)
    }

    pub async fn focus_window(&self, session_id: &str, window_id: &str) -> Result<(), ClientError> {
        self.call("session.focus_window", json!({"session_id": session_id, "window_id": window_id})).await.map(drop)
    }

    pub async fn focus_block(&self, session_id: &str, block_id: &str) -> Result<(), ClientError> {
        self.call("session.focus_block", json!({"session_id": session_id, "block_id": block_id})).await.map(drop)
    }

    /// Split `block_id`; `vertical` stacks the new block below.
    pub async fn split(&self, session_id: &str, block_id: &str, vertical: bool, cwd: Option<&str>) -> Result<Value, ClientError> {
        self.call(
            "session.new_split",
            json!({
                "session_id": session_id, "anchor_id": block_id,
                "direction": if vertical { "vertical" } else { "horizontal" },
                "side": "after", "layout": shell_layout(cwd), "focus": true,
            }),
        )
        .await
    }

    pub async fn close_block(&self, session_id: &str, block_id: &str) -> Result<(), ClientError> {
        self.call("block.close", json!({"session_id": session_id, "block_id": block_id})).await.map(drop)
    }

    pub async fn zoom_block(&self, session_id: &str, block_id: &str) -> Result<(), ClientError> {
        self.call("session.zoom_block", json!({"session_id": session_id, "block_id": block_id})).await.map(drop)
    }

    pub async fn own_block(&self, session_id: &str, block_id: &str, take: bool) -> Result<Value, ClientError> {
        self.call(
            "session.own_block",
            json!({"session_id": session_id, "block_ids": [block_id], "mode": if take { "take" } else { "request" }}),
        )
        .await
    }

    pub async fn disown_block(&self, session_id: &str, block_id: &str) -> Result<Value, ClientError> {
        self.call("session.disown_block", json!({"session_id": session_id, "block_ids": [block_id]})).await
    }

    pub async fn terminal(&self, session_id: &str, block_id: &str, method: &str, args: Value) -> Result<Value, ClientError> {
        let mut p = json!({"session_id": session_id, "block_id": block_id});
        if !args.is_null() {
            p["args"] = args;
        }
        self.call(&format!("{TERMINAL_CREATOR}.{method}"), p).await
    }

    /// Type text as if the user typed it. Text larger than one message is sent
    /// in order, one piece at a time: Rex runs a connection's requests
    /// concurrently, so the next piece waits for the previous reply.
    pub async fn write(&self, session_id: &str, block_id: &str, text: &str) -> Result<(), ClientError> {
        for piece in split_for_messages(text, self.text_budget()) {
            self.terminal(session_id, block_id, "write", json!({"data": piece})).await?;
        }
        Ok(())
    }

    /// Paste (respects bracketed paste mode). A paste larger than one message
    /// arrives as consecutive pastes, in order.
    pub async fn paste(&self, session_id: &str, block_id: &str, text: &str) -> Result<(), ClientError> {
        for piece in split_for_messages(text, self.text_budget()) {
            self.call(
                "block.event",
                json!({"session_id": session_id, "block_id": block_id, "event": {"type": "paste", "payload": {"text": piece}}}),
            )
            .await?;
        }
        Ok(())
    }

    /// How much escaped text one request can carry, leaving room for its envelope.
    fn text_budget(&self) -> usize {
        let max = usize::try_from(self.inner.welcome.limits.max_message_bytes).unwrap_or(usize::MAX);
        max.saturating_sub(ENVELOPE_BYTES).max(MIN_TEXT_BUDGET)
    }

    /// Send a key press (and release), routed through the server's key bindings.
    pub async fn key(&self, session_id: &str, block_id: &str, key: &KeyPress) -> Result<(), ClientError> {
        for action in ["press", "release"] {
            self.call("client.key", json!({"session_id": session_id, "block_id": block_id, "key": key.to_json(action)})).await?;
        }
        Ok(())
    }

    pub async fn resize(
        &self,
        session_id: &str,
        block_id: &str,
        cols: u32,
        rows: u32,
        cell_px: Option<(u32, u32)>,
    ) -> Result<(), ClientError> {
        let mut args = json!({"columns": cols, "rows": rows});
        if let Some((w, h)) = cell_px {
            args["cell_width_px"] = json!(w);
            args["cell_height_px"] = json!(h);
        }
        self.terminal(session_id, block_id, "resize", args).await.map(drop)
    }

    pub async fn data_ticket(&self, session_id: &str, block_id: &str) -> Result<String, ClientError> {
        let v = self
            .call(
                "data.connect",
                json!({"session_id": session_id, "block_id": block_id, "data_ref_id": "terminal", "subprotocol": TERMINAL_SUBPROTOCOL}),
            )
            .await?;
        v.get("ticket").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| ClientError::Protocol("no ticket".into()))
    }

    /// Plain-text screen and scrollback (for previews and search).
    pub async fn screen_text(&self, session_id: &str, block_id: &str, max_bytes: Option<u64>) -> Result<String, ClientError> {
        let mut args = json!({"format": "plain", "trim": true, "unwrap": true});
        if let Some(m) = max_bytes {
            args["max_bytes"] = json!(m);
        }
        let v = self.terminal(session_id, block_id, "format", args).await?;
        Ok(v.get("content").and_then(Value::as_str).unwrap_or_default().to_owned())
    }

    pub async fn server_status(&self) -> Result<Value, ClientError> {
        self.call("server.status", json!({})).await
    }
}

/// Dropping even an unpolled request must not leak its response sender.
struct PendingRegistration {
    inner: std::sync::Weak<Inner>,
    id: String,
}
impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Ok(mut pending) = inner.pending.lock() {
                pending.remove(&self.id);
            }
        }
    }
}

/// Room left in a request for everything but its text: id, method, ids, keys.
const ENVELOPE_BYTES: usize = 4096;
const MIN_TEXT_BUDGET: usize = 256;

/// Bytes `c` takes inside a JSON string, as serde_json escapes it.
fn json_escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Split `text` at character boundaries so each piece, JSON-escaped, fits in
/// `budget` bytes. Empty text yields nothing.
fn split_for_messages(text: &str, budget: usize) -> Vec<&str> {
    // Room for any one character: the longest escape (`\u00XX`) is six bytes.
    let budget = budget.max(6);
    let mut pieces = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (at, c) in text.char_indices() {
        let len = json_escaped_len(c);
        if used + len > budget {
            pieces.push(&text[start..at]);
            (start, used) = (at, 0);
        }
        used += len;
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

pub fn platform_name() -> &'static str {
    match std::env::consts::OS {
        os @ ("macos" | "ios" | "linux" | "windows" | "freebsd") => os,
        _ => "other",
    }
}

fn shell_layout(cwd: Option<&str>) -> Value {
    let mut block = json!({"flavor": shrew_proto::SHELL_FLAVOR});
    if let Some(c) = cwd.filter(|c| !c.is_empty()) {
        block["options"] = json!({"cwd": c});
    }
    json!({"block": block})
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AttachedSession {
    pub session: SessionSummary,
    pub view: SessionView,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DataTicket {
    pub session_id: String,
    pub block_id: String,
    pub ticket: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AttachAll {
    #[serde(default)]
    pub sessions: Option<Vec<SessionSummary>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Bootstrap {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub server_status: Value,
    #[serde(default)]
    pub attach_all: AttachAll,
    #[serde(default)]
    pub selected_session: Option<AttachedSession>,
    #[serde(default)]
    pub attached_sessions: Option<Vec<AttachedSession>>,
    #[serde(default)]
    pub data_tickets: Option<Vec<DataTicket>>,
    #[serde(skip)]
    pub seq: u64,
}

impl Bootstrap {
    pub fn sessions(&self) -> &[SessionSummary] {
        self.attach_all.sessions.as_deref().unwrap_or_default()
    }
    pub fn attached(&self) -> Vec<&AttachedSession> {
        self.selected_session.iter().chain(self.attached_sessions.iter().flatten()).collect()
    }
}

/// A key press in Rex's vocabulary (W3C codes in snake_case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    pub key: String,
    pub mods: Mods,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Mods {
    fn render(&self) -> String {
        let mut v = Vec::new();
        if self.shift {
            v.push("shift");
        }
        if self.ctrl {
            v.push("ctrl");
        }
        if self.alt {
            v.push("alt");
        }
        if self.meta {
            v.push("super");
        }
        v.join("+")
    }
}

impl KeyPress {
    pub fn named(key: &str) -> Self {
        Self { key: key.into(), mods: Mods::default(), text: String::new() }
    }
    /// ctrl+<letter>, e.g. `KeyPress::ctrl('c')`.
    pub fn ctrl(c: char) -> Self {
        let c = c.to_ascii_lowercase();
        Self { key: char_code(c), mods: Mods { ctrl: true, ..Default::default() }, text: String::new() }
    }
    fn to_json(&self, action: &str) -> Value {
        let unshifted = self
            .text
            .chars()
            .next()
            .map(|c| c.to_ascii_lowercase() as u32)
            .or_else(|| self.key.strip_prefix("key_").and_then(|k| k.chars().next()).map(|c| c as u32));
        json!({
            "key": self.key,
            "mods": self.mods.render(),
            "consumed_mods": "",
            "text": if action == "release" { "" } else { &self.text },
            "unshifted_codepoint": unshifted.unwrap_or(0),
            "composing": false,
            "action": action,
        })
    }
}

/// Map a printable ASCII character to its physical key code (US layout).
pub fn char_code(c: char) -> String {
    match c {
        'a'..='z' => format!("key_{c}"),
        'A'..='Z' => format!("key_{}", c.to_ascii_lowercase()),
        '0'..='9' => format!("digit_{c}"),
        ' ' => "space".into(),
        '[' | '{' => "bracket_left".into(),
        ']' | '}' => "bracket_right".into(),
        '\\' | '|' => "backslash".into(),
        '`' | '~' => "backquote".into(),
        '-' | '_' => "minus".into(),
        '=' | '+' => "equal".into(),
        ';' | ':' => "semicolon".into(),
        '\'' | '"' => "quote".into(),
        ',' | '<' => "comma".into(),
        '.' | '>' => "period".into(),
        '/' | '?' => "slash".into(),
        _ => "unidentified".into(),
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    fn escaped(piece: &str) -> usize {
        serde_json::to_string(piece).unwrap().len() - 2
    }

    #[test]
    fn small_text_is_one_piece_and_empty_text_none() {
        assert_eq!(split_for_messages("ls -la\r", 1024), vec!["ls -la\r"]);
        assert!(split_for_messages("", 1024).is_empty());
    }

    #[test]
    fn pieces_fit_the_budget_after_escaping_and_rejoin_exactly() {
        let text = "a\"b\\c\n\u{1}中文😀\t".repeat(500);
        for budget in [6, 7, 64, 1000] {
            let pieces = split_for_messages(&text, budget);
            assert_eq!(pieces.concat(), text);
            for piece in &pieces {
                assert!(!piece.is_empty());
                assert!(escaped(piece) <= budget, "{} > {budget}", escaped(piece));
            }
        }
    }

    #[test]
    fn escaped_length_matches_serde_json() {
        for c in ['a', '"', '\\', '\n', '\u{1f}', '\u{7f}', '中', '😀', '/'] {
            assert_eq!(json_escaped_len(c), escaped(&c.to_string()), "{c:?}");
        }
    }
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response as Handshake};
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    /// One-client Rex stand-in that answers nothing until `hold` requests have
    /// arrived, then answers them all. Resolves with the `n` of each request.
    async fn holding_server(hold: usize) -> (Endpoint, tokio::task::JoinHandle<Vec<u64>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, |_: &Request, mut reply: Handshake| {
                reply.headers_mut().insert("Sec-WebSocket-Protocol", HeaderValue::from_static(CONTROL_SUBPROTOCOL));
                Ok(reply)
            })
            .await
            .unwrap();
            ws.next().await.unwrap().unwrap(); // hello
            let welcome = json!({
                "type": "welcome", "protocol": CONTROL_SUBPROTOCOL, "limits": {"max_message_bytes": 1 << 20},
                "client": {"id": "client:x", "reconnect_token": "t"}, "api_revision": API_REVISION,
            });
            ws.send(Message::text(welcome.to_string())).await.unwrap();
            let (mut ids, mut seen) = (Vec::new(), Vec::new());
            while ids.len() < hold {
                let Message::Text(text) = ws.next().await.unwrap().unwrap() else { continue };
                let request: Value = serde_json::from_str(&text).unwrap();
                ids.push(request["id"].as_str().unwrap().to_owned());
                seen.push(request["payload"]["n"].as_u64().unwrap());
            }
            for id in ids {
                ws.send(Message::text(json!({"type": "response", "id": id, "seq": 0, "payload": {}}).to_string())).await.unwrap();
            }
            // Stay until the client closes: dropping the socket with one of its
            // pings unread resets the connection and can take the replies with it.
            while let Some(Ok(_)) = ws.next().await {}
            seen
        });
        (Endpoint::parse(&format!("127.0.0.1:{port}")).unwrap(), task)
    }

    #[tokio::test]
    async fn cancelling_an_unpolled_reply_releases_pending_registration() {
        let (endpoint, server) = holding_server(2).await;
        let (conn, _) = Connection::connect(&endpoint, ConnectOptions::default()).await.unwrap();
        let reply = conn.start("session.describe_block", json!({"n":0})).unwrap();
        assert_eq!(conn.inner.pending.lock().unwrap().len(), 1);
        drop(reply);
        assert!(conn.inner.pending.lock().unwrap().is_empty());
        conn.close(); server.abort();
    }

    #[tokio::test]
    async fn started_requests_are_on_the_wire_in_order_before_any_reply() {
        let (endpoint, server) = holding_server(3).await;
        let (conn, _events) = Connection::connect(&endpoint, ConnectOptions::default()).await.unwrap();
        // A caller that awaited each reply before sending the next would hang here.
        let replies: Vec<Reply> = (0..3).map(|n| conn.start("client.key", json!({"n": n})).unwrap()).collect();
        tokio::time::timeout(Duration::from_secs(5), async {
            for reply in replies {
                reply.await.unwrap();
            }
        })
        .await
        .expect("requests were not pipelined");
        conn.close();
        assert_eq!(server.await.unwrap(), [0, 1, 2]);
    }
}
