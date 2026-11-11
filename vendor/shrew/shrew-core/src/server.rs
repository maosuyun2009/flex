//! One supervised connection to one Rex server, and the live model it keeps.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use shrew_client::{ConnEvent, ConnectOptions, Connection, Endpoint};
use shrew_proto::{AgentState, Event, ProgramStatus, SessionEvent, SessionSummary, SessionView, TerminalEvent};
use tokio::sync::{Notify, watch};

use crate::state::*;
use crate::store::Profile;

#[derive(Default)]
pub(crate) struct BlockMeta {
    pub title: String,
    pub pwd: String,
    pub process: String,
    pub statuses: BTreeMap<String, ProgramStatus>,
    pub exited: Option<i32>,
    pub progress: Option<u8>,
    pub bell: bool,
    pub fetched: bool,
}

impl BlockMeta {
    fn agent(&self) -> Option<AgentStatus> {
        let (_, top) = self.statuses.iter().max_by_key(|(_, r)| (AgentState::parse(&r.state), r.updated_at.clone()))?;
        let state = AgentState::parse(&top.state);
        let app = top.effective_app.clone().or_else(|| top.app.clone()).unwrap_or_default();
        Some(AgentStatus {
            state,
            waiting_for: top.kind.clone(),
            app,
            title: top.title.clone().unwrap_or_default(),
            message: top.msg.clone().unwrap_or_default(),
            progress: top.progress,
            updated_at: top.updated_at.clone().unwrap_or_default(),
        })
    }
}

#[derive(Default)]
pub(crate) struct SessionModel {
    pub summary: SessionSummary,
    pub view: Option<SessionView>,
    pub clients: HashMap<String, String>,
}

#[derive(Default)]
pub(crate) struct ServerModel {
    pub connectivity: Connectivity,
    pub latency_ms: Option<u32>,
    pub version: Option<String>,
    pub host_name: Option<String>,
    pub order: Vec<String>,
    pub sessions: HashMap<String, SessionModel>,
    pub meta: HashMap<String, BlockMeta>,
    pub client_id: Option<String>,
    pub account: Option<String>,
}

/// A lock that remembers whether its value may have changed: every mutable
/// access marks it, so no call site can forget to. A snapshot is rebuilt only
/// when something it shows was touched.
pub(crate) struct Tracked<T> {
    value: Mutex<T>,
    changed: AtomicBool,
}

pub(crate) struct TrackedGuard<'a, T> {
    guard: parking_lot::MutexGuard<'a, T>,
    changed: &'a AtomicBool,
}

impl<T> Tracked<T> {
    fn new(value: T) -> Self {
        Self { value: Mutex::new(value), changed: AtomicBool::new(true) }
    }

    pub fn lock(&self) -> TrackedGuard<'_, T> {
        TrackedGuard { guard: self.value.lock(), changed: &self.changed }
    }

    /// Whether the value was mutably accessed since the last call.
    fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
}

impl<T> std::ops::Deref for TrackedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for TrackedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.changed.store(true, Ordering::Release);
        &mut self.guard
    }
}

pub(crate) struct ServerEntry {
    pub profile: Tracked<Profile>,
    pub conn: Mutex<Option<Connection>>,
    /// Fires whenever a new connection is ready (terminals wait on it).
    pub conn_tx: watch::Sender<u64>,
    pub model: Tracked<ServerModel>,
    setup: Mutex<BTreeMap<String, Value>>,
    pub wake: Notify,
    pub stop: watch::Sender<bool>,
    /// Open terminals, for routing block events (clipboard) to them.
    pub terminals: Mutex<HashMap<String, std::sync::Weak<crate::terminal::TerminalSession>>>,
}

pub(crate) trait Sink: Send + Sync {
    fn changed(&self);
    fn notify(&self, n: Notification);
    fn client_options(&self) -> ConnectOptions;
    fn foreground(&self) -> watch::Receiver<bool>;
    fn client_request(&self, server_id: String, conn: Connection, body: shrew_proto::EventBody);
    fn invalidate(&self, invalidation: CacheInvalidation);
}

impl ServerEntry {
    pub fn new(profile: Profile) -> Arc<Self> {
        Arc::new(Self {
            profile: Tracked::new(profile),
            conn: Mutex::new(None),
            conn_tx: watch::channel(0).0,
            model: Tracked::new(ServerModel::default()),
            setup: Mutex::new(BTreeMap::new()),
            wake: Notify::new(),
            stop: watch::channel(false).0,
            terminals: Mutex::new(HashMap::new()),
        })
    }

    pub fn id(&self) -> String {
        self.profile.lock().id.clone()
    }

    pub(crate) fn forget_connection(&self) {
        self.conn.lock().take();
    }

    pub fn connection(&self) -> Option<Connection> {
        self.conn.lock().clone().filter(|c| !c.is_closed())
    }

    /// Keep only successful identity-specific setup, not ordinary keymap queries.
    pub fn remember_setup(&self, method: &str, payload: &Value) {
        let keep = match method {
            "client.set_script" | "client.set_label" => true,
            "client.keymap" => ["actions", "defaults", "overrides"].iter().any(|k| payload.get(k).is_some()),
            _ => false,
        };
        if keep {
            let mut setup = self.setup.lock();
            if method == "client.keymap" {
                let previous = setup.entry(method.into()).or_insert_with(|| json!({}));
                if let Some(fields) = payload.as_object() {
                    for (key, value) in fields {
                        previous[key] = value.clone();
                    }
                }
            } else {
                setup.insert(method.into(), payload.clone());
            }
        }
    }

    /// Wait (up to `timeout`) for a live connection.
    pub async fn wait_connection(&self, timeout: Duration) -> Option<Connection> {
        if let Some(c) = self.connection() {
            return Some(c);
        }
        self.wake.notify_one();
        let mut rx = self.conn_tx.subscribe();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                r = rx.changed() => {
                    if r.is_err() {
                        return None;
                    }
                    if let Some(c) = self.connection() {
                        return Some(c);
                    }
                }
                _ = &mut deadline => return None,
            }
        }
    }

    /// Whether [`Self::snapshot`] may differ from the last time this was asked.
    pub fn take_changed(&self) -> bool {
        // Not `||`: both marks have to be cleared.
        self.profile.take_changed() | self.model.take_changed()
    }

    pub fn snapshot(&self) -> ServerState {
        let p = self.profile.lock().clone();
        let m = self.model.lock();
        let me = m.client_id.clone();
        let sessions = m
            .order
            .iter()
            .filter_map(|id| m.sessions.get(id))
            .map(|s| {
                let windows = s
                    .view
                    .as_ref()
                    .map(|v| {
                        v.windows
                            .iter()
                            .map(|w| {
                                let placed = w.placed_blocks();
                                let blocks = w
                                    .layers
                                    .iter()
                                    .flat_map(|layer| layer.blocks.iter().map(move |b| (layer, b)))
                                    .map(|(layer, b)| {
                                        let id = b.block_id.clone().unwrap_or_default();
                                        let meta = m.meta.get(&id);
                                        let pl = placed.iter().find(|p| p.block_id == id);
                                        BlockState {
                                            creator_name: b.creator_name.clone(),
                                            layer_id: layer.layer_id.clone(),
                                            layer_state: layer.state.clone(),
                                            layer_kind: layer.kind.clone(),
                                            layer_bounds: layer.bounds,
                                            label: b.label.clone(),
                                            title: meta.map(|m| m.title.clone()).unwrap_or_default(),
                                            pwd: meta.map(|m| m.pwd.clone()).unwrap_or_default(),
                                            process: meta.map(|m| m.process.clone()).unwrap_or_default(),
                                            rect: pl.map(|p| p.rect).unwrap_or([0.0, 0.0, 1.0, 1.0]),
                                            floating: pl.is_some_and(|p| p.floating),
                                            owned_by_me: me.is_some() && b.owner == me,
                                            // An owner that's no longer connected (e.g. the CLI that created
                                            // the session) shouldn't stop us taking the size. Only judge once
                                            // the client list has loaded, which always includes us.
                                            owner: b.owner.clone().filter(|o| {
                                                let loaded = me.as_ref().is_some_and(|m| s.clients.contains_key(m));
                                                !loaded || Some(o) == me.as_ref() || s.clients.contains_key(o)
                                            }),
                                            exited: meta.and_then(|m| m.exited),
                                            agent: meta.and_then(BlockMeta::agent),
                                            progress: meta.and_then(|m| m.progress),
                                            has_bell: meta.is_some_and(|m| m.bell),
                                            id,
                                        }
                                    })
                                    .collect();
                                WindowState {
                                    id: w.id().to_owned(),
                                    label: w.label.clone(),
                                    focused_block_id: w.focused_block_id.clone(),
                                    zoomed_block_id: w.zoomed_block_id.clone(),
                                    blocks,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut clients: Vec<String> =
                    s.clients.iter().filter(|(id, _)| Some(*id) != me.as_ref()).map(|(_, d)| d.clone()).collect();
                clients.sort();
                SessionState {
                    id: s.summary.session_id.clone(),
                    label: s.summary.label.clone(),
                    active_window_id: s.view.as_ref().and_then(|v| v.active_window_id.clone()),
                    windows,
                    clients,
                }
            })
            .collect();
        // A bare IP isn't a great name; "account@ip" (or the tailnet name) reads better.
        let host = Endpoint::parse(&p.address)
            .ok()
            .map(|e| match e {
                Endpoint::Unix(_) => "This computer".to_owned(),
                Endpoint::Url(u) => u.host_str().unwrap_or("server").to_owned(),
            })
            .unwrap_or_else(|| p.address.clone());
        let raw = if p.name.trim().is_empty() { host } else { p.name.clone() };
        let name = if raw.parse::<std::net::IpAddr>().is_ok() {
            match (&m.host_name, &m.account) {
                (Some(h), _) => h.clone(),
                (None, Some(a)) => format!("{a}@{raw}"),
                _ => raw,
            }
        } else {
            raw
        };
        ServerState {
            id: p.id.clone(),
            name,
            address: p.address.clone(),
            connectivity: m.connectivity.clone(),
            latency_ms: m.latency_ms,
            version: m.version.clone(),
            host_name: m.host_name.clone(),
            sessions,
            is_local: Endpoint::parse(&p.address).map(|e| e.is_local()).unwrap_or(false),
        }
    }

    fn set_connectivity(&self, c: Connectivity, sink: &dyn Sink) {
        let mut m = self.model.lock();
        if m.connectivity != c {
            m.connectivity = c;
            drop(m);
            sink.changed();
        }
    }

    /// The supervisor: connect, mirror state, reconnect forever until stopped.
    pub async fn run(self: Arc<Self>, sink: Arc<dyn Sink>) {
        let mut stop = self.stop.subscribe();
        let mut fg = sink.foreground();
        let mut token: Option<String> = None;
        let mut backoff = Backoff::default();
        loop {
            if *stop.borrow() {
                return;
            }
            // Don't hold sockets open while a phone app is in the background.
            while !*fg.borrow() {
                if let Some(c) = self.conn.lock().take() {
                    c.close();
                }
                self.set_connectivity(Connectivity::Idle, &*sink);
                tokio::select! {
                    _ = fg.changed() => {}
                    _ = stop.changed() => return,
                }
            }
            let address = self.profile.lock().address.clone();
            let endpoint = match Endpoint::parse(&address) {
                Ok(e) => e,
                Err(e) => {
                    self.set_connectivity(Connectivity::Offline { error: e.to_string(), retry_in_ms: 0 }, &*sink);
                    tokio::select! { _ = self.wake.notified() => continue, _ = stop.changed() => return }
                }
            };
            self.set_connectivity(Connectivity::Connecting, &*sink);
            let mut opts = sink.client_options();
            opts.reconnect_token = token.take();
            let started = Instant::now();
            match Connection::connect(&endpoint, opts).await {
                Ok((conn, events)) => {
                    token = conn.reconnect_token().map(str::to_owned);
                    let rtt = started.elapsed().as_millis().min(60_000) as u32;
                    match self.clone().serve(conn, events, &sink, rtt, &mut stop, &mut token).await {
                        Served::Stopped => return,
                        Served::Lost { reason, online_for } => {
                            tracing::info!("connection to {address} lost: {reason}");
                            // A connection that stayed up reconnects at once (the server keeps
                            // us for 60s); one that failed setup or dropped quickly backs off,
                            // or a server that accepts sockets but fails bootstrap is hammered.
                            let Some(retry) = backoff.lost(online_for) else {
                                self.set_connectivity(Connectivity::Offline { error: reason, retry_in_ms: 0 }, &*sink);
                                continue;
                            };
                            if self.wait_retry(retry, reason, &*sink, &mut backoff, &mut fg, &mut stop).await {
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let retry = backoff.failed();
                    if self.wait_retry(retry, friendly(&e), &*sink, &mut backoff, &mut fg, &mut stop).await {
                        return;
                    }
                }
            }
        }
    }

    /// Show the retry and wait it out. A manual reconnect cuts the wait and the
    /// backoff short. Returns true when the supervisor was stopped.
    async fn wait_retry(
        &self,
        retry: Duration,
        error: String,
        sink: &dyn Sink,
        backoff: &mut Backoff,
        fg: &mut watch::Receiver<bool>,
        stop: &mut watch::Receiver<bool>,
    ) -> bool {
        self.set_connectivity(Connectivity::Offline { error, retry_in_ms: retry.as_millis() as u64 }, sink);
        tokio::select! {
            _ = tokio::time::sleep(retry) => false,
            _ = self.wake.notified() => { backoff.reset(); false }
            _ = fg.changed() => false,
            _ = stop.changed() => true,
        }
    }

    async fn serve(
        self: Arc<Self>,
        conn: Connection,
        mut events: tokio::sync::mpsc::UnboundedReceiver<ConnEvent>,
        sink: &Arc<dyn Sink>,
        rtt: u32,
        stop: &mut watch::Receiver<bool>,
        token: &mut Option<String>,
    ) -> Served {
        // Lua setup may itself wait for a client.reply: forward requests even during bootstrap/setup.
        let (state_tx, mut state_events) = tokio::sync::mpsc::unbounded_channel();
        let (entry, request_conn, request_sink) = (self.clone(), conn.clone(), sink.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        Some(ConnEvent::Event { event: Event::ClientRequest(body), .. }) =>
                            request_sink.client_request(entry.id(), request_conn.clone(), body),
                        Some(event) => if state_tx.send(event).is_err() { return; },
                        None => return,
                    },
                    _ = state_tx.closed() => return,
                }
            }
        });
        let boot = match conn.bootstrap(Some(&[]), None).await {
            Ok(b) => b,
            Err(e) => {
                conn.close();
                return Served::Lost { reason: friendly(&e), online_for: None };
            }
        };
        // Time a tiny call for a true round-trip figure (connect time includes TLS etc.).
        let t = Instant::now();
        let version = conn.call("server.version", json!({})).await.ok();
        let rtt = (t.elapsed().as_millis() as u32).min(rtt.max(1));
        let account =
            conn.call("server.whoami", json!({})).await.ok().and_then(|v| v.get("account").and_then(Value::as_str).map(str::to_owned));
        {
            let mut m = self.model.lock();
            m.client_id = Some(boot.client_id.clone()).filter(|s| !s.is_empty()).or_else(|| conn.client_id().map(str::to_owned));
            m.latency_ms = Some(rtt);
            m.version = version.as_ref().and_then(|v| v.get("rex_version")).and_then(Value::as_str).map(short_version);
            m.host_name = boot
                .server_status
                .pointer("/tailscale/dns_name")
                .and_then(Value::as_str)
                .map(|s| s.split('.').next().unwrap_or(s).to_owned());
            m.account = account;
            m.order = boot.sessions().iter().map(|s| s.session_id.clone()).collect();
            let keep: std::collections::HashSet<_> = m.order.iter().cloned().collect();
            m.sessions.retain(|k, _| keep.contains(k));
            for s in boot.sessions() {
                m.sessions.entry(s.session_id.clone()).or_default().summary = s.clone();
            }
        }
        let setup = self.setup.lock().clone();
        for method in ["client.set_label", "client.set_script", "client.keymap"] {
            // Hello may reset the label on resume, but re-running Lua would repeat side effects.
            if method == "client.set_script" && conn.resumed() {
                continue;
            }
            if let Some(payload) = setup.get(method) {
                if let Err(error) = conn.call(method, payload.clone()).await {
                    sink.invalidate(CacheInvalidation {
                        server_id: self.id(),
                        kind: "client_setup_failed".into(),
                        payload: json!({"method": method, "error": error.to_string()}),
                    });
                }
            }
        }
        sink.invalidate(CacheInvalidation { server_id: self.id(), kind: "keymap_changed".into(), payload: json!({}) });
        sink.invalidate(CacheInvalidation { server_id: self.id(), kind: "hosts_changed".into(), payload: json!({}) });
        *self.conn.lock() = Some(conn.clone());
        self.conn_tx.send_modify(|v| *v += 1);
        for s in boot.sessions() {
            self.clone().attach(conn.clone(), s.session_id.clone(), sink.clone());
        }
        self.set_connectivity(Connectivity::Online, &**sink);
        let online_at = Instant::now();

        loop {
            tokio::select! {
                ev = state_events.recv() => match ev {
                    Some(ConnEvent::Event { event, .. }) => self.clone().apply(&conn, event, sink),
                    Some(ConnEvent::Closed { reason }) => {
                        *self.conn.lock() = None;
                        *token = conn.reconnect_token().map(str::to_owned);
                        return Served::Lost { reason, online_for: Some(online_at.elapsed()) };
                    }
                    None => {
                        *self.conn.lock() = None;
                        *token = conn.reconnect_token().map(str::to_owned);
                        return Served::Lost { reason: "connection lost".into(), online_for: Some(online_at.elapsed()) };
                    }
                },
                _ = stop.changed() => {
                    conn.close();
                    return Served::Stopped;
                }
                _ = self.wake.notified() => {
                    // A manual "reconnect" while online: re-read everything.
                    for id in self.model.lock().order.clone() {
                        self.clone().refresh_view(conn.clone(), id, sink.clone());
                    }
                }
            }
        }
    }

    fn attach(self: Arc<Self>, conn: Connection, session_id: String, sink: Arc<dyn Sink>) {
        tokio::spawn(async move {
            match conn.attach(&session_id).await {
                Ok((_, view)) => {
                    let blocks: Vec<String> = view
                        .all_blocks()
                        .filter(|(_, b)| b.creator_name.is_empty() || b.creator_name == shrew_proto::TERMINAL_CREATOR)
                        .filter_map(|(_, b)| b.block_id.clone())
                        .collect();
                    {
                        let mut m = self.model.lock();
                        let s = m.sessions.entry(session_id.clone()).or_default();
                        s.summary.session_id = session_id.clone();
                        if s.summary.label.is_empty() {
                            s.summary.label = view.label.clone();
                        }
                        s.view = Some(view);
                    }
                    sink.changed();
                    self.fetch_meta(&conn, &session_id, blocks, &sink).await;
                    if let Ok(v) = conn.call("session.list_clients", json!({"session_id": session_id})).await {
                        let mut m = self.model.lock();
                        if let Some(s) = m.sessions.get_mut(&session_id) {
                            s.clients = v
                                .get("clients")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|c| {
                                    let id = c.get("client_id")?.as_str()?.to_owned();
                                    Some((id, describe_client(c)))
                                })
                                .collect();
                        }
                        drop(m);
                        sink.changed();
                    }
                }
                Err(e) => tracing::warn!("attach {session_id}: {e}"),
            }
        });
    }

    fn refresh_view(self: Arc<Self>, conn: Connection, session_id: String, sink: Arc<dyn Sink>) {
        tokio::spawn(async move {
            if let Ok((_, view)) = conn.view(&session_id).await {
                let new_blocks: Vec<String> = {
                    let mut m = self.model.lock();
                    let known: Vec<String> = view
                        .all_blocks()
                        .filter(|(_, b)| b.creator_name.is_empty() || b.creator_name == shrew_proto::TERMINAL_CREATOR)
                        .filter_map(|(_, b)| b.block_id.clone())
                        .filter(|id| !m.meta.get(id).is_some_and(|x| x.fetched))
                        .collect();
                    if let Some(s) = m.sessions.get_mut(&session_id)
                        && s.view.as_ref().is_none_or(|old| old.revision <= view.revision)
                    {
                        s.summary.label = view.label.clone();
                        s.view = Some(view);
                    }
                    known
                };
                sink.changed();
                if !new_blocks.is_empty() {
                    self.fetch_meta(&conn, &session_id, new_blocks, &sink).await;
                }
            }
        });
    }

    /// Title, cwd, process and agent status for blocks we haven't seen yet.
    async fn fetch_meta(&self, conn: &Connection, session_id: &str, blocks: Vec<String>, sink: &Arc<dyn Sink>) {
        for chunk in blocks.chunks(8) {
            let futs = chunk.iter().map(|b| async move {
                let (t, p, st, pr) = tokio::join!(
                    conn.terminal(session_id, b, "title", Value::Null),
                    conn.terminal(session_id, b, "pwd", Value::Null),
                    conn.terminal(session_id, b, "program_status", Value::Null),
                    conn.terminal(session_id, b, "process", Value::Null),
                );
                (b.clone(), t.ok(), p.ok(), st.ok(), pr.ok())
            });
            let results = futures_util::future::join_all(futs).await;
            let mut m = self.model.lock();
            for (b, t, p, st, pr) in results {
                let meta = m.meta.entry(b).or_default();
                meta.fetched = true;
                if let Some(t) = t.as_ref().and_then(|v| v.get("title")).and_then(Value::as_str) {
                    meta.title = t.to_owned();
                }
                if let Some(p) = p.as_ref().and_then(|v| v.get("pwd")).and_then(Value::as_str) {
                    meta.pwd = p.to_owned();
                }
                if let Some(name) = pr.as_ref().and_then(|v| v.pointer("/foreground/name")).and_then(Value::as_str) {
                    meta.process = name.to_owned();
                }
                if let Some(code) = pr.as_ref().and_then(|v| v.pointer("/last_exit/exit_code")).and_then(Value::as_i64)
                    && pr.as_ref().and_then(|v| v.get("child")).is_none_or(Value::is_null)
                {
                    meta.exited = Some(code as i32);
                }
                if let Some(recs) = st.as_ref().and_then(|v| v.get("records")).and_then(Value::as_array) {
                    meta.statuses.clear();
                    for r in recs {
                        if let Ok(rec) = serde_json::from_value::<ProgramStatus>(r.get("record").cloned().unwrap_or_else(|| r.clone())) {
                            let id = r.get("id").and_then(Value::as_str).map(str::to_owned).or_else(|| rec.id.clone()).unwrap_or_default();
                            meta.statuses.insert(id, rec);
                        }
                    }
                }
            }
            drop(m);
            sink.changed();
        }
    }

    fn apply(self: Arc<Self>, conn: &Connection, event: Event, sink: &Arc<dyn Sink>) {
        match event {
            Event::SessionCreated(s) => {
                {
                    let mut m = self.model.lock();
                    if !m.order.contains(&s.session_id) {
                        m.order.push(s.session_id.clone());
                    }
                    m.sessions.entry(s.session_id.clone()).or_default().summary = s.clone();
                }
                sink.changed();
                self.attach(conn.clone(), s.session_id, sink.clone());
            }
            Event::SessionDestroyed { session_id } => {
                let mut m = self.model.lock();
                m.order.retain(|s| *s != session_id);
                m.sessions.remove(&session_id);
                drop(m);
                sink.changed();
            }
            Event::SessionMoved { .. } => {
                let (this, conn, sink) = (self.clone(), conn.clone(), sink.clone());
                tokio::spawn(async move {
                    if let Ok(list) = conn.list_sessions().await {
                        this.model.lock().order = list.into_iter().map(|s| s.session_id).collect();
                        sink.changed();
                    }
                });
            }
            Event::Session { session_id, event } => self.apply_session(conn, session_id, event, sink),
            Event::ClientRequest(body) => sink.client_request(self.id(), conn.clone(), body),
            Event::KeymapChanged { payload } => {
                sink.invalidate(CacheInvalidation { server_id: self.id(), kind: "keymap_changed".into(), payload })
            }
            Event::HostsChanged { payload } => {
                sink.invalidate(CacheInvalidation { server_id: self.id(), kind: "hosts_changed".into(), payload })
            }
            _ => {}
        }
    }

    fn apply_session(self: Arc<Self>, conn: &Connection, session_id: String, event: SessionEvent, sink: &Arc<dyn Sink>) {
        match event {
            SessionEvent::ViewChanged { revision } => {
                let stale = self.model.lock().sessions.get(&session_id).and_then(|s| s.view.as_ref()).is_none_or(|v| v.revision < revision);
                if stale {
                    self.refresh_view(conn.clone(), session_id, sink.clone());
                }
            }
            SessionEvent::LabelChanged { label } => {
                if let Some(s) = self.model.lock().sessions.get_mut(&session_id) {
                    s.summary.label = label;
                }
                sink.changed();
            }
            SessionEvent::BlockOwnerChanged { block_id, owner } => {
                if let Some(view) = self.model.lock().sessions.get_mut(&session_id).and_then(|s| s.view.as_mut()) {
                    for block in view.windows.iter_mut().flat_map(|w| &mut w.layers).flat_map(|l| &mut l.blocks) {
                        if block.block_id.as_deref() == Some(&block_id) {
                            block.owner = owner.clone();
                        }
                    }
                }
                sink.changed();
            }
            SessionEvent::BlockClosed { block_id } => {
                self.model.lock().meta.remove(&block_id);
                sink.changed();
            }
            SessionEvent::ClientConnected { client_id, display } => {
                let (this, conn, sink, sid) = (self.clone(), conn.clone(), sink.clone(), session_id.clone());
                let _ = display;
                tokio::spawn(async move {
                    if let Ok(v) = conn.call("client.inspect", json!({"client_id": client_id})).await {
                        let d = describe_client(v.get("client").unwrap_or(&v));
                        if let Some(s) = this.model.lock().sessions.get_mut(&sid) {
                            s.clients.insert(client_id, d);
                        }
                        sink.changed();
                    }
                });
            }
            SessionEvent::ClientDisconnected { client_id } => {
                if let Some(s) = self.model.lock().sessions.get_mut(&session_id) {
                    s.clients.remove(&client_id);
                }
                sink.changed();
            }
            SessionEvent::Block { block_id, creator, event } if creator.is_empty() || creator == shrew_proto::TERMINAL_CREATOR => {
                self.apply_block(session_id, block_id, event, sink)
            }
            _ => {}
        }
    }

    fn apply_block(&self, session_id: String, block_id: String, event: TerminalEvent, sink: &Arc<dyn Sink>) {
        let server_id = self.id();
        if let TerminalEvent::Clipboard { text: Some(t), .. } = &event {
            if let Some(term) = self.terminals.lock().get(&block_id).and_then(|w| w.upgrade()) {
                term.on_clipboard(t.clone());
            }
            return;
        }
        let mut note: Option<Notification> = None;
        {
            let mut m = self.model.lock();
            let meta = m.meta.entry(block_id.clone()).or_default();
            let label_for = |meta: &BlockMeta| notification_name(&meta.title, &meta.process);
            match event {
                TerminalEvent::TitleChanged { title } => meta.title = title,
                TerminalEvent::PwdChanged { pwd } => meta.pwd = pwd,
                TerminalEvent::ProcessChanged { foreground } => {
                    if let Some(p) = foreground {
                        meta.process = p.name;
                        meta.exited = None;
                    }
                }
                TerminalEvent::Progress { state, progress } => {
                    meta.progress = if state == "remove" || state == "none" { None } else { progress.or(Some(0)) };
                }
                TerminalEvent::Bell => {
                    meta.bell = true;
                    note = Some(Notification {
                        kind: NotificationKind::Bell,
                        server_id,
                        session_id,
                        block_id,
                        title: label_for(meta),
                        body: "Bell".into(),
                    });
                }
                TerminalEvent::ChildExited { exit_code, .. } => {
                    meta.exited = Some(exit_code);
                    note = Some(Notification {
                        kind: NotificationKind::ProcessExited,
                        server_id,
                        session_id,
                        block_id,
                        title: label_for(meta),
                        body: if exit_code == 0 { "Exited".into() } else { format!("Exited with status {exit_code}") },
                    });
                }
                TerminalEvent::Notification { title, body } => {
                    note = Some(Notification {
                        kind: NotificationKind::Program,
                        server_id,
                        session_id,
                        block_id,
                        title: if title.is_empty() { label_for(meta) } else { title },
                        body,
                    });
                }
                TerminalEvent::StatusChanged { id, record } => {
                    let before = meta.statuses.get(&id).map(|r| AgentState::parse(&r.state));
                    let after = AgentState::parse(&record.state);
                    let app = record.effective_app.clone().or_else(|| record.app.clone()).unwrap_or_else(|| label_for(meta));
                    let msg = record.msg.clone().or_else(|| record.title.clone()).unwrap_or_default();
                    if before != Some(after) {
                        let kind = match after {
                            AgentState::Blocked => Some(NotificationKind::AgentBlocked),
                            AgentState::Done if before == Some(AgentState::Working) || before == Some(AgentState::Blocked) => {
                                Some(NotificationKind::AgentDone)
                            }
                            AgentState::Error => Some(NotificationKind::AgentError),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            let body = match (&kind, record.kind.as_deref()) {
                                (NotificationKind::AgentBlocked, Some("permission")) if msg.is_empty() => {
                                    "Wants permission to continue".into()
                                }
                                (NotificationKind::AgentBlocked, Some("question")) if msg.is_empty() => "Has a question for you".into(),
                                (NotificationKind::AgentBlocked, Some("auth")) if msg.is_empty() => "Needs credentials".into(),
                                (NotificationKind::AgentDone, _) if msg.is_empty() => "Finished".into(),
                                _ => msg.clone(),
                            };
                            note = Some(Notification { kind, server_id, session_id, block_id, title: app, body });
                        }
                    }
                    meta.statuses.insert(id, record);
                }
                TerminalEvent::StatusRemoved { ids } => {
                    for id in ids {
                        meta.statuses.remove(&id);
                    }
                }
                _ => return,
            }
        }
        if let Some(n) = note {
            sink.notify(n);
        }
        sink.changed();
    }

    pub fn clear_bell(&self, block_id: &str) {
        let mut model = self.model.lock();
        // Look before writing: this runs on every focus, and a mutable access
        // alone marks the model changed.
        if model.meta.get(block_id).is_some_and(|m| m.bell)
            && let Some(m) = model.meta.get_mut(block_id)
        {
            m.bell = false;
        }
    }
}

pub(crate) enum Served {
    Stopped,
    /// `online_for` is `None` when the connection never finished setup.
    Lost { reason: String, online_for: Option<Duration> },
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A connection that lasted this long was healthy; losing it is not a failure.
const STABLE_AFTER: Duration = Duration::from_secs(10);

/// Delay before the next connection attempt, doubling on each failure.
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: BACKOFF_MIN }
    }
}

impl Backoff {
    /// A failed attempt: wait the current delay, and double it for next time.
    pub fn failed(&mut self) -> Duration {
        let wait = self.next;
        self.next = (self.next * 2).min(BACKOFF_MAX);
        wait
    }

    pub fn reset(&mut self) {
        self.next = BACKOFF_MIN;
    }

    /// A served connection ended. `None` means reconnect at once.
    pub fn lost(&mut self, online_for: Option<Duration>) -> Option<Duration> {
        match online_for {
            Some(up) if up >= STABLE_AFTER => {
                self.reset();
                None
            }
            _ => Some(self.failed()),
        }
    }
}

fn describe_client(c: &Value) -> String {
    let info = c.get("info");
    let name = info.and_then(|i| i.get("name")).and_then(Value::as_str).unwrap_or("");
    let label = info.and_then(|i| i.get("label")).and_then(Value::as_str).unwrap_or("");
    let who = c.pointer("/principal/node").or_else(|| c.pointer("/principal/display")).and_then(Value::as_str).unwrap_or("");
    let base = if !label.is_empty() {
        label
    } else if !name.is_empty() {
        name
    } else {
        "client"
    };
    if who.is_empty() { base.to_owned() } else { format!("{base} · {who}") }
}

pub(crate) fn friendly(e: &shrew_client::ClientError) -> String {
    use shrew_client::ClientError as E;
    match e {
        E::Io(io) => match io.kind() {
            std::io::ErrorKind::ConnectionRefused => "Server isn't running there".into(),
            std::io::ErrorKind::NotFound => "No server at that address".into(),
            std::io::ErrorKind::TimedOut => "Timed out — is the server reachable?".into(),
            _ => format!("Can't reach the server ({io})"),
        },
        E::Timeout => "Timed out — check the address and your network (Tailscale?)".into(),
        E::WebSocket(w) => {
            let s = w.to_string();
            if s.contains("403") {
                "The server refused this device (Tailscale access rules?)".into()
            } else {
                format!("Connection failed: {s}")
            }
        }
        E::Protocol(p) if p.contains("no common API revision") => "Server version isn't supported".into(),
        other => other.to_string(),
    }
}

/// `0.0.1793420730-g6d0d9a` → `g6d0d9a` style builds read better as their commit.
pub(crate) fn short_version(v: &str) -> String {
    match v.rsplit_once("-g") {
        Some((base, commit)) if base.starts_with("0.0.") => format!("build {commit}"),
        _ => v.to_owned(),
    }
}

/// A short name for a terminal in a notification title. Shells like fish title
/// the terminal with the whole command line plus the directory
/// (`printf '\e]7501;…' ~`); a person wants just the command (`printf`).
fn notification_name(title: &str, process: &str) -> String {
    let t = title.trim();
    if t.is_empty() || t.chars().any(char::is_control) {
        return if process.is_empty() { "Terminal".into() } else { process.to_owned() };
    }
    let mut words: Vec<&str> = t.split_whitespace().collect();
    // Drop a trailing directory, which fish and zsh themes append.
    if words.len() > 1 && words.last().is_some_and(|w| w.starts_with('~') || w.starts_with('/')) {
        words.pop();
    }
    let rest = words.join(" ");
    // A command line (quotes, escapes, pipes, flags) names its program first.
    let commandish = rest.contains(['\'', '"', '\\', '|', ';', '$']) || rest.split_whitespace().skip(1).any(|w| w.starts_with('-'));
    let name = if commandish { words.first().copied().unwrap_or(t).to_owned() } else { rest };
    let name: String = name.chars().take(40).collect();
    if name.is_empty() { process.to_owned() } else { name }
}

#[cfg(test)]
mod name_tests {
    use super::notification_name as n;

    #[test]
    fn names_for_notifications() {
        assert_eq!(n("printf '\\e]7501;stat ~", "fish"), "printf");
        assert_eq!(n("exit ~", "fish"), "exit");
        assert_eq!(n("✳ Claude Code", "node"), "✳ Claude Code");
        assert_eq!(n("vim notes.md ~/src", "vim"), "vim notes.md");
        assert_eq!(n("cargo test -p core /home/me/x", "cargo"), "cargo");
        assert_eq!(n("", "htop"), "htop");
        assert_eq!(n("bad\u{7}bell", "zsh"), "zsh");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    fn entry_with_view() -> Arc<ServerEntry> {
        let entry = ServerEntry::new(Profile { id: "server-a".into(), name: "A".into(), address: "localhost:7878".into() });
        let view: SessionView = serde_json::from_value(json!({
            "revision": 1, "windows": [{"window_id": "w", "layers": [{
                "layer_id": "floating-1", "kind": "floating", "bounds": [0.1, 0.2, 0.3, 0.4], "state": "minimized",
                "layout": {"rect": [0.1, 0.2, 0.3, 0.4], "block_id": "b"},
                "blocks": [{"block_id": "b", "creator_name": "example.browser", "owner": "client-a"}]
            }]}]
        }))
        .unwrap();
        {
            let mut m = entry.model.lock();
            m.order.push("s".into());
            m.sessions.insert(
                "s".into(),
                SessionModel {
                    summary: SessionSummary { session_id: "s".into(), label: String::new() },
                    view: Some(view),
                    ..Default::default()
                },
            );
        }
        entry
    }

    #[test]
    fn a_connection_that_fails_setup_backs_off_instead_of_spinning() {
        let mut backoff = Backoff::default();
        // Bootstrap failed every time: each retry waits longer, up to the cap.
        let waits: Vec<_> = (0..8).map(|_| backoff.lost(None).expect("must wait")).collect();
        assert_eq!(waits[0], Duration::from_millis(500));
        assert_eq!(waits[1], Duration::from_secs(1));
        assert!(waits.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(*waits.last().unwrap(), BACKOFF_MAX);
        // Dropped right after going online: still a failure.
        assert_eq!(backoff.lost(Some(Duration::from_secs(1))), Some(BACKOFF_MAX));
    }

    #[test]
    fn losing_a_stable_connection_reconnects_at_once_and_resets() {
        let mut backoff = Backoff::default();
        backoff.failed();
        backoff.failed();
        assert_eq!(backoff.lost(Some(STABLE_AFTER)), None);
        assert_eq!(backoff.failed(), BACKOFF_MIN);
    }

    #[test]
    fn setup_queries_do_not_replace_registered_actions() {
        let entry = entry_with_view();
        entry.remember_setup("client.keymap", &json!({"actions": [{"name": "pane.split"}], "defaults": [{"name": "default"}]}));
        entry.remember_setup("client.keymap", &json!({"server_actions": "all"}));
        entry.remember_setup("client.keymap", &json!({"overrides": []}));
        let setup = entry.setup.lock();
        assert_eq!(
            setup["client.keymap"],
            json!({"actions": [{"name": "pane.split"}], "defaults": [{"name": "default"}], "overrides": []})
        );
    }

    #[test]
    fn only_mutable_access_marks_an_entry_changed() {
        let entry = entry_with_view();
        assert!(entry.take_changed(), "a new entry has never been shown");
        assert!(!entry.take_changed());
        // Reading (a snapshot, an ownership check) leaves it clean.
        let _ = entry.snapshot();
        let _ = entry.model.lock().sessions.get("s").is_some();
        let _ = entry.id();
        assert!(!entry.take_changed());
        entry.clear_bell("b");
        assert!(!entry.take_changed(), "no such block meta: nothing was written");
        entry.model.lock().latency_ms = Some(3);
        assert!(entry.take_changed());
        entry.profile.lock().name = "B".into();
        assert!(entry.take_changed());
        assert!(!entry.take_changed());
    }

    #[test]
    fn snapshot_preserves_unknown_creator_blocks() {
        let state = entry_with_view().snapshot();
        assert_eq!(state.sessions[0].windows[0].blocks.len(), 1);
        assert_eq!(state.sessions[0].windows[0].blocks[0].id, "b");
    }

    #[test]
    fn snapshot_exposes_creator_and_minimized_layer() {
        let state = entry_with_view().snapshot();
        let block = &state.sessions[0].windows[0].blocks[0];
        assert_eq!(block.creator_name, "example.browser");
        assert!(!block.is_terminal());
        assert_eq!(block.layer_id.as_deref(), Some("floating-1"));
        assert_eq!(block.layer_state, "minimized");
        assert_eq!(block.layer_kind, "floating");
        assert_eq!(block.layer_bounds, [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(block.display_name(), "example.browser");
    }
}
