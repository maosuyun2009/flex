//! Toolkit-free glue between a Rex server and the rexshell UI.
//!
//! Everything here is plain data and callbacks so it can be unit tested, or
//! driven by a different UI toolkit, without a GPU or a display.

mod grid;

use std::path::PathBuf;
use std::sync::Arc;

pub use grid::{fit_grid, Metrics};
pub use shrew_core::{
    AppState, BlockState, Connectivity, CoreError, Notification, ServerState, SessionState,
    TerminalListener, TerminalSession, WindowState,
};
use shrew_core::{Config, Listener, Shrew};

/// Called on an engine thread whenever [`Engine::state`] may be stale.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One connection to Rex's engine. Keep it alive: dropping it closes every
/// connection and stream it owns.
pub struct Engine {
    shrew: Shrew,
}

struct Waker(Wake);

impl Listener for Waker {
    fn state_changed(&self) {
        (self.0)()
    }
    fn notify(&self, _: Notification) {}
}

impl Engine {
    /// `data_dir` keeps the saved server list; `None` keeps nothing on disk.
    pub fn new(data_dir: Option<PathBuf>, device_label: &str, wake: Wake) -> Self {
        let config = Config {
            data_dir,
            client_name: "rexshell".into(),
            device_label: Some(device_label.to_owned()),
            ..Default::default()
        };
        Self {
            shrew: Shrew::new(config, Arc::new(Waker(wake))),
        }
    }

    /// Escape hatch for engine calls the shell does not wrap (splits, actions…).
    pub fn shrew(&self) -> &Shrew {
        &self.shrew
    }

    pub fn state(&self) -> AppState {
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
