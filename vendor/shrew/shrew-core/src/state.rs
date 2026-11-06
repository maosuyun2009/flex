//! Plain, cloneable snapshots of everything the UI shows. Every frontend
//! re-reads [`AppState`] when told it changed, so these stay simple data.

use shrew_proto::AgentState;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppState {
    pub servers: Vec<ServerState>,
}

impl AppState {
    pub fn server(&self, id: &str) -> Option<&ServerState> {
        self.servers.iter().find(|s| s.id == id)
    }

    /// Every block that wants attention, most urgent first, across all servers.
    pub fn attention(&self) -> Vec<AttentionItem> {
        let mut v: Vec<AttentionItem> = self
            .servers
            .iter()
            .flat_map(|srv| {
                srv.sessions.iter().flat_map(move |s| {
                    s.windows.iter().flat_map(move |w| {
                        w.blocks.iter().filter(|b| b.agent.is_some()).map(move |b| AttentionItem {
                            server_id: srv.id.clone(),
                            server_name: srv.name.clone(),
                            session_id: s.id.clone(),
                            session_label: s.label.clone(),
                            window_id: w.id.clone(),
                            block: b.clone(),
                        })
                    })
                })
            })
            .collect();
        v.sort_by(|a, b| {
            let ka = a.block.agent.as_ref().map(|x| x.state).unwrap_or(AgentState::Idle);
            let kb = b.block.agent.as_ref().map(|x| x.state).unwrap_or(AgentState::Idle);
            kb.cmp(&ka).then_with(|| {
                b.block.agent.as_ref().map(|x| x.updated_at.clone()).cmp(&a.block.agent.as_ref().map(|x| x.updated_at.clone()))
            })
        });
        v
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttentionItem {
    pub server_id: String,
    pub server_name: String,
    pub session_id: String,
    pub session_label: String,
    pub window_id: String,
    pub block: BlockState,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connectivity {
    #[default]
    Idle,
    Connecting,
    Online,
    /// Will retry on its own after `retry_in_ms`.
    Offline {
        error: String,
        retry_in_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerState {
    pub id: String,
    pub name: String,
    pub address: String,
    pub connectivity: Connectivity,
    /// Round-trip time of the last ping-like call, when known.
    pub latency_ms: Option<u32>,
    pub version: Option<String>,
    pub host_name: Option<String>,
    pub sessions: Vec<SessionState>,
    pub is_local: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionState {
    pub id: String,
    pub label: String,
    pub active_window_id: Option<String>,
    pub windows: Vec<WindowState>,
    /// Other clients attached (e.g. "Rex.app on studio").
    pub clients: Vec<String>,
}

impl SessionState {
    pub fn block_count(&self) -> usize {
        self.windows.iter().map(|w| w.blocks.len()).sum()
    }
    /// The most urgent agent state in the session.
    pub fn agent_state(&self) -> Option<AgentState> {
        self.windows.iter().flat_map(|w| &w.blocks).filter_map(|b| b.agent.as_ref().map(|a| a.state)).max()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowState {
    pub id: String,
    pub label: String,
    pub focused_block_id: Option<String>,
    pub zoomed_block_id: Option<String>,
    pub blocks: Vec<BlockState>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockState {
    pub id: String,
    pub creator_name: String,
    pub layer_id: Option<String>,
    /// Empty or unknown means normal; minimized layers must not be drawn.
    pub layer_state: String,
    pub layer_kind: String,
    pub layer_bounds: [f64; 4],
    pub label: String,
    pub title: String,
    pub pwd: String,
    /// Foreground process name, e.g. `zsh`, `claude`, `vim`.
    pub process: String,
    /// `[x, y, w, h]` within the window, as fractions.
    pub rect: [f64; 4],
    pub floating: bool,
    pub owner: Option<String>,
    pub owned_by_me: bool,
    pub exited: Option<i32>,
    pub agent: Option<AgentStatus>,
    /// Progress from OSC 9;4 (0-100), if a program reports it.
    pub progress: Option<u8>,
    pub has_bell: bool,
}

impl BlockState {
    pub fn is_terminal(&self) -> bool {
        self.creator_name.is_empty() || self.creator_name == shrew_proto::TERMINAL_CREATOR
    }

    /// The best short name to show for this block.
    pub fn display_name(&self) -> String {
        let short = if self.pwd.is_empty() { String::new() } else { short_path(&self.pwd) };
        for s in [&self.label, &self.title] {
            let t = s.trim();
            // Shells often title themselves with the cwd; that's not a name.
            let looks_like_path = t.starts_with('~') || t.starts_with('/') || t == short;
            if !t.is_empty() && t != "shell" && !looks_like_path {
                return t.to_owned();
            }
        }
        if !self.process.is_empty() {
            return self.process.clone();
        }
        if !self.pwd.is_empty() {
            return short_path(&self.pwd);
        }
        if self.is_terminal() { "Terminal".into() } else { self.creator_name.clone() }
    }
}

/// Turn `file://host/home/me/src/x` or `/home/me/src/x` into `~/src/x`-ish.
pub fn short_path(pwd: &str) -> String {
    let p = pwd.strip_prefix("file://").map(|r| r.find('/').map(|i| &r[i..]).unwrap_or(r)).unwrap_or(pwd);
    let mut parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 && (parts[0] == "home" || parts[0] == "Users") {
        parts.drain(..2);
        parts.insert(0, "~");
    }
    if parts.is_empty() {
        return "/".into();
    }
    if parts.len() > 3 {
        let tail = parts[parts.len() - 2..].join("/");
        return format!("{}/…/{tail}", parts[0]);
    }
    let s = parts.join("/");
    if s.starts_with('~') { s } else { format!("/{s}") }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStatus {
    pub state: AgentState,
    /// What a blocked program waits for: permission, question, auth.
    pub waiting_for: Option<String>,
    pub app: String,
    pub title: String,
    pub message: String,
    pub progress: Option<u8>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotificationKind {
    /// An agent needs a decision (permission, question, credentials).
    AgentBlocked,
    AgentDone,
    AgentError,
    /// A program asked for a desktop notification (OSC 9 / 777).
    Program,
    Bell,
    ProcessExited,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub kind: NotificationKind,
    pub server_id: String,
    pub session_id: String,
    pub block_id: String,
    pub title: String,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paths() {
        assert_eq!(short_path("file://nixos/home/nixos"), "~");
        assert_eq!(short_path("/Users/a/projects/shrew"), "~/projects/shrew");
        assert_eq!(short_path("/home/a/x/y/z/w"), "~/…/z/w");
        assert_eq!(short_path("/etc/nixos"), "/etc/nixos");
    }
}

/// An inbound action/cross-server request. Nothing executes until the UI validates it.
/// Reply using `event_id`; `call_id` alone is only unique within one connection.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientRequest {
    pub event_id: String,
    pub server_id: String,
    pub request_type: String,
    pub call_id: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientReplyError {
    pub code: String,
    pub message: String,
}

/// Re-fetch the corresponding cache. Payload retains hashes/generations.
/// `client_setup_failed` reports a reconnect setup method/error in the payload.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CacheInvalidation {
    pub server_id: String,
    pub kind: String,
    pub payload: serde_json::Value,
}
