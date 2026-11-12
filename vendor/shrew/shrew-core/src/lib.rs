//! The Shrew engine: everything a frontend needs, independent of UI toolkit.
//!
//! ```text
//!   Shrew ──owns──▶ ServerEntry (one per saved server, self-reconnecting)
//!     │                 └─ live model: sessions → windows → blocks (+ agent status)
//!     └─ open_terminal ─▶ TerminalSession (stream + libghostty-vt + input queue)
//! ```
//! Frontends read [`AppState`] when [`Listener::state_changed`] fires, and pull
//! [`shrew_term::Frame`]s from a [`TerminalSession`] when it says a frame is ready.

mod server;
pub mod state;
mod store;
mod terminal;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use serde_json::json;
pub use shrew_client::{ClientError, Endpoint};
use shrew_client::{ConnectOptions, Connection};
pub use shrew_proto::{AgentState, ClientInfo};
pub use shrew_term::{Cursor, CursorStyle, Frame, Rgb, Row, Run, Scrollbar, Underline};
use tokio::sync::watch;

use server::{ServerEntry, Sink};
pub use state::*;
pub use store::Profile;
use store::Store;
pub use terminal::{ColorTheme, Key, TerminalListener, TerminalSession, TerminalStatus};

/// Callbacks from the engine. Called from background threads.
pub trait Listener: Send + Sync {
    /// Something in [`AppState`] changed. Coalesced to at most ~60 per second.
    fn state_changed(&self);
    /// Something worth telling the user about, even if the app is elsewhere.
    fn notify(&self, notification: Notification);
}

pub struct Config {
    /// Where to keep saved servers. `None` keeps nothing on disk.
    pub data_dir: Option<PathBuf>,
    pub client_name: String,
    pub client_version: String,
    /// One of macos, ios, linux, windows, freebsd, web, other.
    pub platform: String,
    /// A label for this device shown to other clients, e.g. "Teddy's iPhone".
    pub device_label: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: None,
            client_name: "Shrew".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            platform: shrew_client::platform_name().into(),
            device_label: None,
        }
    }
}

struct Inner {
    servers: RwLock<Vec<Arc<ServerEntry>>>,
    store: Mutex<Store>,
    listener: Arc<dyn Listener>,
    dirty: AtomicBool,
    dirty_notify: tokio::sync::Notify,
    foreground: watch::Sender<bool>,
    client: ClientInfo,
    inbox: Mutex<ClientInbox>,
    /// The last snapshot handed out, reused until a server's model changes.
    state: Mutex<Option<Arc<AppState>>>,
}

impl Sink for Inner {
    fn changed(&self) {
        if !self.dirty.swap(true, Ordering::AcqRel) {
            self.dirty_notify.notify_one();
        }
    }
    fn notify(&self, n: Notification) {
        self.listener.notify(n);
    }
    fn client_options(&self) -> ConnectOptions {
        ConnectOptions { client: self.client.clone(), reconnect_token: None, timeout: Duration::from_secs(8) }
    }
    fn foreground(&self) -> watch::Receiver<bool> {
        self.foreground.subscribe()
    }
    fn client_request(&self, server_id: String, conn: Connection, body: shrew_proto::EventBody) {
        let mut inbox = self.inbox.lock();
        inbox.replies.retain(|_, (c, _)| !c.is_closed());
        let event_id = uuid::Uuid::new_v4().to_string();
        let call_id = body.payload.get("call_id").and_then(serde_json::Value::as_str).map(str::to_owned);
        if let Some(call_id) = &call_id {
            inbox.replies.insert(event_id.clone(), (conn, call_id.clone()));
        }
        inbox.requests.push(ClientRequest { event_id, server_id, request_type: body.kind, call_id, payload: body.payload });
        drop(inbox);
        self.changed();
    }
    fn invalidate(&self, invalidation: CacheInvalidation) {
        self.inbox.lock().invalidations.push(invalidation);
        self.changed();
    }
}

#[derive(Default)]
struct ClientInbox {
    requests: Vec<ClientRequest>,
    replies: std::collections::HashMap<String, (Connection, String)>,
    invalidations: Vec<CacheInvalidation>,
}

struct SinkRef(Arc<Inner>);
impl Sink for SinkRef {
    fn changed(&self) {
        self.0.changed()
    }
    fn notify(&self, n: Notification) {
        self.0.notify(n)
    }
    fn client_options(&self) -> ConnectOptions {
        self.0.client_options()
    }
    fn foreground(&self) -> watch::Receiver<bool> {
        self.0.foreground()
    }
    fn client_request(&self, server_id: String, conn: Connection, body: shrew_proto::EventBody) {
        self.0.client_request(server_id, conn, body)
    }
    fn invalidate(&self, invalidation: CacheInvalidation) {
        self.0.invalidate(invalidation)
    }
}

pub struct Shrew {
    rt: tokio::runtime::Runtime,
    inner: Arc<Inner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    UnknownServer,
    NotConnected,
    InvalidAddress(String),
    Server(String),
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownServer => f.write_str("Unknown server"),
            Self::NotConnected => f.write_str("Not connected to that server"),
            Self::InvalidAddress(s) => write!(f, "Invalid address: {s}"),
            Self::Server(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for CoreError {}

impl From<ClientError> for CoreError {
    fn from(e: ClientError) -> Self {
        CoreError::Server(server::friendly(&e))
    }
}

impl Shrew {
    pub fn new(config: Config, listener: Arc<dyn Listener>) -> Self {
        let rt =
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("shrew").enable_all().build().expect("tokio runtime");
        let store = Store::open(config.data_dir.as_deref());
        let inner = Arc::new(Inner {
            servers: RwLock::new(Vec::new()),
            store: Mutex::new(store),
            listener,
            dirty: AtomicBool::new(false),
            dirty_notify: tokio::sync::Notify::new(),
            foreground: watch::channel(true).0,
            inbox: Mutex::new(ClientInbox::default()),
            state: Mutex::new(None),
            client: ClientInfo {
                kind: Some("app".into()),
                platform: Some(config.platform),
                name: Some(config.client_name),
                version: Some(config.client_version),
                label: config.device_label,
            },
        });
        let this = Self { rt, inner };
        // Coalesce change notifications to one per frame.
        let inner = this.inner.clone();
        this.rt.spawn(async move {
            loop {
                inner.dirty_notify.notified().await;
                tokio::time::sleep(Duration::from_millis(16)).await;
                inner.dirty.store(false, Ordering::Release);
                inner.listener.state_changed();
            }
        });
        let profiles = this.inner.store.lock().profiles().to_vec();
        for p in profiles {
            this.start(p);
        }
        this
    }

    fn start(&self, p: Profile) -> Arc<ServerEntry> {
        let entry = ServerEntry::new(p);
        self.inner.servers.write().push(entry.clone());
        self.forget_state();
        let sink: Arc<dyn Sink> = Arc::new(SinkRef(self.inner.clone()));
        self.rt.spawn(entry.clone().run(sink));
        self.inner.changed();
        entry
    }

    fn entry(&self, server_id: &str) -> Result<Arc<ServerEntry>, CoreError> {
        self.inner.servers.read().iter().find(|s| s.id() == server_id).cloned().ok_or(CoreError::UnknownServer)
    }

    fn conn(&self, server_id: &str) -> Result<Connection, CoreError> {
        self.entry(server_id)?.connection().ok_or(CoreError::NotConnected)
    }

    /// Clone the currently supervised connection without reconnecting.
    pub fn persistent_connection(&self, server_id: &str) -> Result<Connection, CoreError> {
        self.conn(server_id)
    }

    /// Run a future on the engine's runtime and wait for it (for sync callers).
    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        self.rt.handle()
    }

    /// The current snapshot. Cheap when nothing changed since the last call:
    /// the same `Arc` comes back, so callers can ask as often as they like.
    pub fn state(&self) -> Arc<AppState> {
        let mut cached = self.inner.state.lock();
        let servers = self.inner.servers.read();
        // Ask every entry (`any` would stop at the first and leave the rest marked).
        let changed = servers.iter().fold(false, |changed, s| s.take_changed() | changed);
        if !changed && let Some(state) = cached.as_ref() {
            return state.clone();
        }
        let state = Arc::new(AppState { servers: servers.iter().map(|s| s.snapshot()).collect() });
        *cached = Some(state.clone());
        state
    }

    /// The server list itself changed (added, removed, reordered).
    fn forget_state(&self) {
        *self.inner.state.lock() = None;
    }

    /// Raw RPC on the supervisor's connection, with its identity and event receiver.
    /// Disconnected servers fail instead of silently creating a different client.
    pub async fn call(&self, server_id: &str, method: &str, payload: serde_json::Value) -> Result<serde_json::Value, CoreError> {
        let entry = self.entry(server_id)?;
        let conn = entry.connection().ok_or(CoreError::NotConnected)?;
        let method = method.to_owned();
        let inner = self.inner.clone();
        self.rt
            .spawn(async move {
                let value = conn.call(&method, payload.clone()).await.map_err(CoreError::from)?;
                entry.remember_setup(&method, &payload);
                if method == "client.set_script" {
                    inner.invalidate(CacheInvalidation { server_id: entry.id(), kind: "keymap_changed".into(), payload: json!({}) });
                }
                Ok(value)
            })
            .await
            .map_err(|e| CoreError::Server(e.to_string()))?
    }

    /// Drain without coalescing: each task/call remains queued until consumed.
    pub fn take_client_requests(&self) -> Vec<ClientRequest> {
        let mut inbox = self.inner.inbox.lock();
        inbox.replies.retain(|_, (c, _)| !c.is_closed());
        std::mem::take(&mut inbox.requests)
    }

    pub fn take_cache_invalidations(&self) -> Vec<CacheInvalidation> {
        std::mem::take(&mut self.inner.inbox.lock().invalidations)
    }

    /// One-shot reply on the exact connection that delivered the event, never a reconnect.
    /// Unknown, already consumed, malformed and disconnected routes fail closed.
    pub async fn reply_client_request(&self, event_id: &str, result: Result<serde_json::Value, ClientReplyError>) -> Result<(), CoreError> {
        let (conn, call_id) = self
            .inner
            .inbox
            .lock()
            .replies
            .remove(event_id)
            .ok_or_else(|| CoreError::Server("Request reply route is no longer available".into()))?;
        if conn.is_closed() {
            return Err(CoreError::NotConnected);
        }
        let payload = match result {
            Ok(value) => json!({"call_id": call_id, "result": value}),
            Err(error) => json!({"call_id": call_id, "error": error}),
        };
        self.rt
            .spawn(async move { conn.call("client.reply", payload).await.map(drop).map_err(CoreError::from) })
            .await
            .map_err(|e| CoreError::Server(e.to_string()))?
    }

    // ---- servers --------------------------------------------------------------

    /// Save and connect to a server. Returns its id.
    pub fn add_server(&self, name: &str, address: &str) -> Result<String, CoreError> {
        let ep = Endpoint::parse(address).map_err(|e| CoreError::InvalidAddress(e.to_string()))?;
        let name = if name.trim().is_empty() { default_name(&ep) } else { name.trim().to_owned() };
        let p = Profile { id: uuid::Uuid::new_v4().to_string(), name, address: address.trim().to_owned() };
        {
            let mut st = self.inner.store.lock();
            let mut all = st.profiles().to_vec();
            all.push(p.clone());
            st.set_profiles(all);
        }
        let id = p.id.clone();
        self.start(p);
        Ok(id)
    }

    /// Try an address without saving it. Resolves with the server's version.
    pub async fn probe(&self, address: &str) -> Result<String, CoreError> {
        let ep = Endpoint::parse(address).map_err(|e| CoreError::InvalidAddress(e.to_string()))?;
        let opts = self.inner.client_options();
        // Everything that touches sockets or timers runs on the engine's runtime,
        // since callers may be on a foreign executor (Kotlin, Swift).
        let r = self
            .rt
            .spawn(async move {
                let (conn, _ev) = Connection::connect(&ep, opts).await?;
                let v = conn.call("server.version", json!({})).await;
                conn.close();
                v
            })
            .await
            .map_err(|e| CoreError::Server(e.to_string()))??;
        Ok(r.get("rex_version").and_then(|v| v.as_str()).map(server::short_version).unwrap_or_else(|| "unknown".into()))
    }

    pub fn remove_server(&self, server_id: &str) {
        let removed = {
            let mut s = self.inner.servers.write();
            let pos = s.iter().position(|e| e.id() == server_id);
            pos.map(|i| s.remove(i))
        };
        self.forget_state();
        if let Some(e) = removed {
            let _ = e.stop.send(true);
        }
        let mut st = self.inner.store.lock();
        let all = st.profiles().iter().filter(|p| p.id != server_id).cloned().collect();
        st.set_profiles(all);
        drop(st);
        self.inner.changed();
    }

    pub fn update_server(&self, server_id: &str, name: &str, address: &str) -> Result<(), CoreError> {
        Endpoint::parse(address).map_err(|e| CoreError::InvalidAddress(e.to_string()))?;
        let e = self.entry(server_id)?;
        {
            let mut p = e.profile.lock();
            p.name = name.trim().to_owned();
            p.address = address.trim().to_owned();
        }
        let mut st = self.inner.store.lock();
        let all = st
            .profiles()
            .iter()
            .map(|p| {
                if p.id == server_id {
                    Profile { name: name.trim().into(), address: address.trim().into(), ..p.clone() }
                } else {
                    p.clone()
                }
            })
            .collect();
        st.set_profiles(all);
        drop(st);
        if let Some(c) = e.conn.lock().take() {
            c.close();
        }
        e.wake.notify_one();
        self.inner.changed();
        Ok(())
    }

    pub fn move_server(&self, from: usize, to: usize) {
        let order: Vec<String> = {
            let mut s = self.inner.servers.write();
            if from >= s.len() || to >= s.len() {
                return;
            }
            let e = s.remove(from);
            s.insert(to, e);
            s.iter().map(|e| e.id()).collect()
        };
        {
            let mut st = self.inner.store.lock();
            let mut all = st.profiles().to_vec();
            all.sort_by_key(|p| order.iter().position(|id| *id == p.id).unwrap_or(usize::MAX));
            st.set_profiles(all);
        }
        // After the list lock is released: `state` takes its cache lock first.
        self.forget_state();
        self.inner.changed();
    }

    /// Retry now instead of waiting out the backoff.
    pub fn reconnect(&self, server_id: &str) {
        if let Ok(e) = self.entry(server_id) {
            e.wake.notify_one();
        }
    }

    /// Mobile apps call this from their lifecycle hooks. In the background,
    /// connections close (the server keeps the client for 60s for a quick resume).
    pub fn set_foreground(&self, fg: bool) {
        self.inner.foreground.send_if_modified(|v| std::mem::replace(v, fg) != fg);
        if fg {
            for e in self.inner.servers.read().iter() {
                e.wake.notify_one();
            }
        }
    }

    /// Forget a server's live connection without closing it, as if its socket
    /// had died while the app was frozen. Tests use this; the supervisor will
    /// reconnect on its own.
    #[doc(hidden)]
    pub fn disconnect_for_test(&self, server_id: &str) {
        if let Ok(e) = self.entry(server_id) {
            e.forget_connection();
        }
    }

    /// Add this machine's local server if one is running and none is saved.
    pub fn add_local_server_if_present(&self) -> Option<String> {
        let ep = Endpoint::local_default()?;
        let addr = ep.display();
        if self.inner.store.lock().profiles().iter().any(|p| Endpoint::parse(&p.address).ok().as_ref() == Some(&ep)) {
            return None;
        }
        self.add_server("This computer", &addr).ok()
    }

    pub fn setting(&self, key: &str) -> Option<serde_json::Value> {
        self.inner.store.lock().setting(key).cloned()
    }

    pub fn set_setting(&self, key: &str, value: serde_json::Value) {
        self.inner.store.lock().set_setting(key, value);
    }

    // ---- sessions & layout -----------------------------------------------------

    pub async fn create_session(
        &self,
        server_id: &str,
        label: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(String, Option<String>), CoreError> {
        let c = self.conn(server_id)?;
        let (label, cwd) = (label.map(str::to_owned), cwd.map(str::to_owned));
        Ok(self
            .rt
            .spawn(async move { c.create_session(label.as_deref(), cwd.as_deref()).await })
            .await
            .map_err(|e| CoreError::Server(e.to_string()))??)
    }

    /// Run a call on the engine runtime so callers on any executor work.
    async fn run<T: Send + 'static>(
        &self,
        server_id: &str,
        f: impl FnOnce(Connection) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, ClientError>> + Send>> + Send + 'static,
    ) -> Result<T, CoreError> {
        let entry = self.entry(server_id)?;
        // The normal path: the supervisor's live connection. If there is none, or
        // it has gone quiet (the app was frozen in the background and its socket
        // died without closing), use a short-lived connection for this request so
        // that e.g. a reply from a notification still gets through.
        if let Some(c) = entry.connection().filter(Connection::is_fresh) {
            return Ok(self.rt.spawn(async move { f(c).await }).await.map_err(|e| CoreError::Server(e.to_string()))??);
        }
        let ep = Endpoint::parse(&entry.profile.lock().address).map_err(|e| CoreError::InvalidAddress(e.to_string()))?;
        let opts = self.inner.client_options();
        Ok(self
            .rt
            .spawn(async move {
                let (c, _ev) = Connection::connect(&ep, opts).await?;
                // Session-scoped methods need the connection attached first.
                c.call("session.attach_all", json!({"event_scope": "session"})).await?;
                let r = f(c.clone()).await;
                c.close();
                r
            })
            .await
            .map_err(|e| CoreError::Server(e.to_string()))??)
    }

    pub async fn destroy_session(&self, server_id: &str, session_id: &str) -> Result<(), CoreError> {
        let s = session_id.to_owned();
        self.run(server_id, move |c| Box::pin(async move { c.destroy_session(&s).await })).await
    }

    pub async fn rename_session(&self, server_id: &str, session_id: &str, label: &str) -> Result<(), CoreError> {
        let (s, l) = (session_id.to_owned(), label.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.rename_session(&s, &l).await })).await
    }

    pub async fn new_window(&self, server_id: &str, session_id: &str, cwd: Option<&str>) -> Result<(), CoreError> {
        let (s, d) = (session_id.to_owned(), cwd.map(str::to_owned));
        self.run(server_id, move |c| Box::pin(async move { c.new_window(&s, d.as_deref()).await.map(drop) })).await
    }

    pub async fn close_window(&self, server_id: &str, session_id: &str, window_id: &str) -> Result<(), CoreError> {
        let (s, w) = (session_id.to_owned(), window_id.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.close_window(&s, &w).await })).await
    }

    pub async fn rename_window(&self, server_id: &str, session_id: &str, window_id: &str, label: &str) -> Result<(), CoreError> {
        let (s, w, l) = (session_id.to_owned(), window_id.to_owned(), label.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.rename_window(&s, &w, &l).await })).await
    }

    pub async fn focus_window(&self, server_id: &str, session_id: &str, window_id: &str) -> Result<(), CoreError> {
        let (s, w) = (session_id.to_owned(), window_id.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.focus_window(&s, &w).await })).await
    }

    pub async fn focus_block(&self, server_id: &str, session_id: &str, block_id: &str) -> Result<(), CoreError> {
        let (s, b) = (session_id.to_owned(), block_id.to_owned());
        if let Ok(e) = self.entry(server_id) {
            e.clear_bell(block_id);
        }
        self.run(server_id, move |c| Box::pin(async move { c.focus_block(&s, &b).await })).await
    }

    pub async fn split(
        &self,
        server_id: &str,
        session_id: &str,
        block_id: &str,
        vertical: bool,
        cwd: Option<&str>,
    ) -> Result<Option<String>, CoreError> {
        let (s, b, d) = (session_id.to_owned(), block_id.to_owned(), cwd.map(str::to_owned));
        self.run(server_id, move |c| {
            Box::pin(async move {
                let v = c.split(&s, &b, vertical, d.as_deref()).await?;
                Ok(v.pointer("/block_ids/0").and_then(|x| x.as_str()).map(str::to_owned))
            })
        })
        .await
    }

    pub async fn close_block(&self, server_id: &str, session_id: &str, block_id: &str) -> Result<(), CoreError> {
        let (s, b) = (session_id.to_owned(), block_id.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.close_block(&s, &b).await })).await
    }

    pub async fn zoom_block(&self, server_id: &str, session_id: &str, block_id: &str) -> Result<(), CoreError> {
        let (s, b) = (session_id.to_owned(), block_id.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.zoom_block(&s, &b).await })).await
    }

    /// Plain text of a block's screen (for previews and the agent inbox).
    pub async fn preview_text(&self, server_id: &str, session_id: &str, block_id: &str, max_bytes: u64) -> Result<String, CoreError> {
        let (s, b) = (session_id.to_owned(), block_id.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.screen_text(&s, &b, Some(max_bytes)).await })).await
    }

    /// Quick reply to an agent without opening the terminal (e.g. "y\r").
    pub async fn send_text(&self, server_id: &str, session_id: &str, block_id: &str, text: &str) -> Result<(), CoreError> {
        let (s, b, t) = (session_id.to_owned(), block_id.to_owned(), text.to_owned());
        self.run(server_id, move |c| Box::pin(async move { c.write(&s, &b, &t).await })).await
    }

    /// Actions the server offers a command palette: titled actions from its
    /// configuration file (`rex.action`) and from block creators.
    pub async fn palette_actions(&self, server_id: &str) -> Result<Vec<PaletteAction>, CoreError> {
        self.run(server_id, move |c| {
            Box::pin(async move {
                let v = c.call("client.keymap", json!({"server_actions": "all"})).await?;
                let mut out: Vec<PaletteAction> = v
                    .get("actions")
                    .and_then(|a| a.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|a| {
                        let kind = a.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                        let title = a.get("title").and_then(|t| t.as_str())?.to_owned();
                        // Client-kind actions are ours to implement; the app lists its own.
                        if kind == "client" || title.is_empty() {
                            return None;
                        }
                        let s = |k: &str| a.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
                        Some(PaletteAction {
                            name: s("name"),
                            title,
                            description: s("description"),
                            category: s("category"),
                            keywords: s("keywords"),
                        })
                    })
                    .collect();
                out.sort_by(|a, b| a.category.cmp(&b.category).then(a.title.cmp(&b.title)));
                Ok(out)
            })
        })
        .await
    }

    /// Run a palette action against a session's focused (or given) block.
    pub async fn run_action(&self, server_id: &str, session_id: &str, block_id: Option<&str>, action: &str) -> Result<(), CoreError> {
        let (s, b, a) = (session_id.to_owned(), block_id.map(str::to_owned), action.to_owned());
        self.run(server_id, move |c| {
            Box::pin(async move {
                let mut p = json!({"action": a, "session_id": s, "origin": "palette"});
                if let Some(b) = b {
                    p["block_id"] = json!(b);
                }
                c.call("client.action", p).await.map(drop)
            })
        })
        .await
    }

    // ---- terminals -------------------------------------------------------------

    pub fn open_terminal(
        &self,
        server_id: &str,
        session_id: &str,
        block_id: &str,
        listener: Arc<dyn TerminalListener>,
    ) -> Result<Arc<TerminalSession>, CoreError> {
        let e = self.entry(server_id)?;
        e.clear_bell(block_id);
        let _g = self.rt.enter();
        let t = TerminalSession::open(e.clone(), session_id.to_owned(), block_id.to_owned(), listener);
        let mut map = e.terminals.lock();
        map.retain(|_, w| w.strong_count() > 0);
        map.insert(block_id.to_owned(), Arc::downgrade(&t));
        Ok(t)
    }
}

impl Drop for Shrew {
    fn drop(&mut self) {
        for e in self.inner.servers.read().iter() {
            let _ = e.stop.send(true);
            if let Some(c) = e.connection() {
                c.close();
            }
        }
    }
}

/// An action a server offers for a command palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAction {
    pub name: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub keywords: String,
}

fn default_name(ep: &Endpoint) -> String {
    match ep {
        Endpoint::Unix(_) => "This computer".into(),
        Endpoint::Url(u) => {
            let h = u.host_str().unwrap_or("server");
            if h.parse::<std::net::IpAddr>().is_ok() { h.to_owned() } else { h.split('.').next().unwrap_or(h).to_owned() }
        }
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    struct Quiet;
    impl Listener for Quiet {
        fn state_changed(&self) {}
        fn notify(&self, _: Notification) {}
    }

    #[test]
    fn raw_call_does_not_create_an_ephemeral_identity_when_disconnected() {
        let shrew = Shrew::new(Config::default(), Arc::new(Quiet));
        // Install an offline entry without starting a supervisor or touching a server.
        shrew.inner.servers.write().push(ServerEntry::new(Profile {
            id: "saved".into(),
            name: "Offline".into(),
            address: "127.0.0.1:1".into(),
        }));
        assert_eq!(shrew.block_on(shrew.call("saved", "client.keymap", json!({}))), Err(CoreError::NotConnected));
        assert_eq!(shrew.block_on(shrew.call("missing", "client.keymap", json!({}))), Err(CoreError::UnknownServer));
    }

    #[test]
    fn state_is_reused_until_something_it_shows_changes() {
        let shrew = Shrew::new(Config::default(), Arc::new(Quiet));
        let entry = ServerEntry::new(Profile { id: "a".into(), name: "A".into(), address: "127.0.0.1:1".into() });
        shrew.inner.servers.write().push(entry.clone());
        shrew.forget_state();
        let first = shrew.state();
        assert!(Arc::ptr_eq(&first, &shrew.state()), "an unchanged model must not be copied again");

        entry.model.lock().latency_ms = Some(7);
        let second = shrew.state();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.servers[0].latency_ms, Some(7));
        assert_eq!(first.servers[0].latency_ms, None, "a snapshot already handed out never changes");

        shrew.update_server("a", "Renamed", "127.0.0.1:1").unwrap();
        assert_eq!(shrew.state().servers[0].name, "Renamed");

        // Out of range changes nothing, and must not hold the list lock into `state`.
        shrew.move_server(0, 9);
        assert_eq!(shrew.state().servers[0].name, "Renamed");

        shrew.remove_server("a");
        assert!(shrew.state().servers.is_empty());
        let id = shrew.add_server("B", "127.0.0.1:2").unwrap();
        assert_eq!(shrew.state().servers[0].id, id);
    }
}
