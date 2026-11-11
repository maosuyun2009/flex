//! Toolkit-free glue between a Rex server and the flex UI.
//!
//! Everything here is plain data and callbacks so it can be unit tested, or
//! driven by a different UI toolkit, without a GPU or a display.

mod grid;
pub mod keybindings;

use std::path::PathBuf;
use std::sync::Arc;

pub use grid::{fit_grid, Metrics};
pub use shrew_core::{
    AppState, BlockState, CacheInvalidation, ClientReplyError, ClientRequest, Connectivity,
    CoreError, Notification, ServerState, SessionState, TerminalListener, TerminalSession,
    WindowState,
};
use shrew_core::{Config, Listener, Shrew};

/// Called on an engine thread whenever [`Engine::state`] may be stale.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Called on an engine thread when something is worth showing the user: a bell,
/// a program asking for attention, an agent that stopped for a decision.
pub type Notifier = Arc<dyn Fn(Notification) + Send + Sync>;

/// One connection to Rex's engine. Keep it alive: dropping it closes every
/// connection and stream it owns.
pub struct Engine {
    shrew: Shrew,
}

struct Waker(Wake, Notifier);

impl Listener for Waker {
    fn state_changed(&self) {
        (self.0)()
    }
    fn notify(&self, notification: Notification) {
        (self.1)(notification)
    }
}

impl Engine {
    /// `data_dir` keeps the saved server list; `None` keeps nothing on disk.
    pub fn new(
        data_dir: Option<PathBuf>,
        device_label: &str,
        wake: Wake,
        notify: Notifier,
    ) -> Self {
        let config = Config {
            data_dir,
            client_name: "flex".into(),
            device_label: Some(device_label.to_owned()),
            ..Default::default()
        };
        Self {
            shrew: Shrew::new(config, Arc::new(Waker(wake, notify))),
        }
    }

    /// Escape hatch for engine calls the shell does not wrap (splits, actions…).
    pub fn shrew(&self) -> &Shrew {
        &self.shrew
    }

    /// The engine's current snapshot. The same `Arc` comes back until
    /// something changes, so asking is cheap.
    pub fn state(&self) -> Arc<AppState> {
        self.shrew.state()
    }

    pub fn add_server(&self, name: &str, address: &str) -> Result<String, CoreError> {
        self.shrew.add_server(name, address)
    }

    pub fn reconnect(&self, server_id: &str) {
        self.shrew.reconnect(server_id)
    }

    /// Add this machine's local Rex server when one is running and none is saved.
    pub fn add_local_server_if_present(&self) -> Option<String> {
        self.shrew.add_local_server_if_present()
    }

    /// Where the block's programs actually are.
    ///
    /// `BlockState::pwd` only carries what the shell *reported* with OSC 7, and
    /// plain zsh or bash never report anything, so a terminal sitting at a
    /// prompt has no pwd at all. The server can read the foreground process's
    /// directory instead, which is the one a new split or tool should start in.
    pub async fn block_cwd(
        &self,
        address: &str,
        session_id: &str,
        block_id: &str,
    ) -> Option<String> {
        let reply = self
            .call(
                address,
                "com.superlogical.terminal.process",
                serde_json::json!({"session_id": session_id, "block_id": block_id}),
            )
            .await
            .ok()?;
        let field = |pointer: &str| {
            reply
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        field("/foreground/cwd").or_else(|| field("/child/cwd"))
    }

    /// Resolve a saved address and use its supervised control connection.
    /// Unsaved/offline addresses never create an ephemeral client identity.
    pub async fn call(
        &self,
        address: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, CoreError> {
        let endpoint = shrew_core::Endpoint::parse(address)
            .map_err(|e| CoreError::InvalidAddress(e.to_string()))?;
        let server_id = self
            .state()
            .servers
            .iter()
            .find(|server| {
                shrew_core::Endpoint::parse(&server.address).ok().as_ref() == Some(&endpoint)
            })
            .map(|server| server.id.clone())
            .ok_or(CoreError::UnknownServer)?;
        self.call_server(&server_id, method, payload).await
    }

    pub async fn call_server(
        &self,
        server_id: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, CoreError> {
        self.shrew.call(server_id, method, payload).await
    }

    pub fn take_client_requests(&self) -> Vec<ClientRequest> {
        self.shrew.take_client_requests()
    }

    pub fn take_cache_invalidations(&self) -> Vec<CacheInvalidation> {
        self.shrew.take_cache_invalidations()
    }

    /// Always send the result/refusal back on the request's original connection.
    pub async fn reply_client_request(
        &self,
        event_id: &str,
        result: Result<serde_json::Value, ClientReplyError>,
    ) -> Result<(), CoreError> {
        self.shrew.reply_client_request(event_id, result).await
    }

    pub fn open_terminal(
        &self,
        server_id: &str,
        session_id: &str,
        block_id: &str,
        listener: Arc<dyn TerminalListener>,
    ) -> Result<Arc<TerminalSession>, CoreError> {
        self.shrew
            .open_terminal(server_id, session_id, block_id, listener)
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn call_resolves_saved_endpoint_and_rejects_unsaved_addresses() {
        let engine = Engine::new(None, "test", Arc::new(|| {}), Arc::new(|_| {}));
        let id = engine.add_server("offline", "127.0.0.1:1").unwrap();
        let rt = engine.shrew();
        assert_eq!(
            rt.block_on(engine.call("http://127.0.0.1:1/", "client.keymap", json!({}))),
            Err(CoreError::NotConnected)
        );
        assert_eq!(
            rt.block_on(engine.call_server(&id, "client.keymap", json!({}))),
            Err(CoreError::NotConnected)
        );
        assert_eq!(
            rt.block_on(engine.call("127.0.0.1:2", "client.keymap", json!({}))),
            Err(CoreError::UnknownServer)
        );
        assert!(matches!(
            rt.block_on(engine.call("", "client.keymap", json!({}))),
            Err(CoreError::InvalidAddress(_))
        ));
    }
}
