//! The window: a title bar of our own (GPUI draws no system one on Windows),
//! the session list beside it, tabs for the session's windows, the panes of the
//! window you picked, and the settings page over them.
//!
//! Every pane is placed from `BlockState::rect`, which the server sends as
//! fractions of its window, so splits, floats and zoom need no layout code here.

#[path = "workspace_automation.rs"]
mod automation;
#[path = "workspace_menu.rs"]
mod menu;
#[path = "workspace_model.rs"]
mod model;
use model::{move_before, tool_layer_request, FloatHints, PendingTool, ServerCache, ToolProgress};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_channel::Receiver;
use flex_engine::{AppState, BlockState, Connectivity, Engine, SessionState, WindowState};
use gpui::prelude::*;
use gpui::{
    canvas, div, hsla, linear_color_stop, linear_gradient, point, px, relative, size, AnyElement,
    App, Bounds, Context, Entity, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, SharedString, Size, Task, Window,
};
use serde_json::json;

use crate::icons::{self, Icon};
use crate::settings::{Settings, FONT_MAX, FONT_MIN};
use crate::shortcuts::{self, Action};
use crate::terminal::{TerminalEvent, TerminalView};
use crate::theme;
use flex_engine::keybindings::KeyBindings;
use flex_engine::Notification;

/// Printed by a picker pane once it has a choice, so the client can find it in
/// the pane's text without guessing at fzf's redraws.
const PICK_MARK: &str = "@@pick@@";

const PICK_FILES: &str =
    "p=$(rg --files --hidden -g '!.git' 2>/dev/null | fzf --height=100% --layout=reverse); \
[ -n \"$p\" ] && printf '\\n@@pick@@%s\\n' \"$p\"; sleep 0.2";
const PICK_DIRS: &str = "p=$(find . -mindepth 1 -type d -not -path './.git*' -not -path '*/node_modules*' 2>/dev/null \
| sed 's|^\\./||' | fzf --height=100% --layout=reverse); [ -n \"$p\" ] && printf '\\n@@pick@@%s\\n' \"$p\"; sleep 0.2";
const PICK_ANY: &str = "p=$({ rg --files --hidden -g '!.git' 2>/dev/null; find . -mindepth 1 -type d -not -path './.git*' \
-not -path '*/node_modules*' 2>/dev/null | sed 's|^\\./||'; } | fzf --height=100% --layout=reverse); \
[ -n \"$p\" ] && printf '\\n@@pick@@%s\\n' \"$p\"; sleep 0.2";

/// What the prefix key can open.
#[derive(Clone, Copy, PartialEq)]
enum Tool {
    /// A lazygit pane that disappears again when you quit it.
    Lazygit,
    Files,
    Dirs,
    /// Files and directories in one list.
    Anything,
}

/// A detached block's identity stays tied to the server and session that own it.
#[derive(Clone)]
struct ParkedBlock {
    server: String,
    session: String,
    id: String,
    label: String,
}

/// Where a pane lands when it is floated: centred, leaving the tiled panes
/// visible around it, the way a floating terminal is usually wanted.
const FLOAT_BOUNDS: [f64; 4] = [0.14, 0.12, 0.72, 0.7];

/// What the pointer grabbed on a floating pane.
#[derive(Clone, Copy, PartialEq)]
enum Grab {
    Move,
    Resize,
}

/// A floating pane being dragged or resized. The rect is applied locally while
/// the pointer moves and pushed to the server once, on release: Rex needs a
/// `layer_id` this shell does not carry in its state, so the commit looks it up
/// in the same round trip.
#[derive(Clone)]
struct Pull {
    block_id: String,
    local_key: Option<model::LocalFloatKey>,
    grab: Grab,
    /// Pointer position in window coordinates when the drag started.
    start: Point<Pixels>,
    /// The layer's rect at that moment, as window fractions.
    rect: [f64; 4],
    /// Where the pane should be drawn now.
    live: [f64; 4],
}

/// A divider being dragged: the two panes it separates, and which way it moves.
struct Divider {
    /// "horizontal" for side by side, "vertical" for stacked.
    direction: &'static str,
    before: String,
    after: String,
    /// The left/top pane and the right/bottom pane, as drawn.
    before_rect: [f64; 4],
    after_rect: [f64; 4],
}

/// What a one-line prompt is renaming.
#[derive(Clone, PartialEq, Eq)]
enum Ask {
    RenameTab,
    RenamePane,
    RenameSession(String, String, String),
    CloseLayer(String, String, String),
    RenameTailscale,
    EditServer(String, String, String),
    /// The address of the machine the session is created on.
    NewSession(String),
    /// Session id and label.
    DeleteSession(String, String, String),
    NewHost,
    /// Host id and label.
    DeleteHost(String, String),
    AddServer,
    CreateBlock {
        server: String,
        session: String,
        anchor: Option<String>,
        flavor: String,
        schema: serde_json::Value,
    },
    RunAction {
        server: String,
        session: String,
        block: Option<String>,
        name: String,
        schema: serde_json::Value,
    },
    /// Server id and name.
    DeleteServer(String, String),
}

impl Ask {
    /// Whether the prompt types something, or only asks yes or no.
    fn wants_text(&self) -> bool {
        matches!(
            self,
            Ask::RenameTab
                | Ask::RenamePane
                | Ask::RenameSession(..)
                | Ask::NewSession(..)
                | Ask::NewHost
                | Ask::AddServer
                | Ask::RenameTailscale
                | Ask::EditServer(..)
                | Ask::CreateBlock { .. }
                | Ask::RunAction { .. }
        )
    }

    fn hint(&self) -> &'static str {
        match self {
            Ask::DeleteSession(..) | Ask::CloseLayer(..) => "Enter to delete · Esc to cancel",
            Ask::DeleteHost(..) | Ask::DeleteServer(..) => "Enter to remove · Esc to cancel",
            Ask::NewHost | Ask::AddServer => "Enter to add · Esc to cancel",
            Ask::CreateBlock { .. } | Ask::RunAction { .. } => {
                "Paste JSON with Ctrl-V · Enter to submit · Esc to cancel"
            }
            _ => "Enter to save · Esc to cancel",
        }
    }
}

/// A one-line text prompt: Rex names windows, blocks, sessions and hosts, and
/// the shell has no text field, so this is the smallest thing that can type one.
struct Prompt {
    what: Ask,
    label: String,
    value: String,
    scope: Option<menu::Target>,
}

#[derive(Clone)]
struct ScriptDraft {
    name: String,
    source: String,
}

#[derive(Clone)]
struct PaletteActionItem {
    server: String,
    session: String,
    block: Option<String>,
    name: String,
    title: String,
    description: String,
    args_schema: Option<serde_json::Value>,
}

/// Where a palette entry takes you.
#[derive(Clone)]
enum Jump {
    Block {
        server: String,
        session: String,
        window: String,
        block: String,
    },
    Window {
        server: String,
        session: String,
        window: String,
    },
    File(String),
    Action {
        server: String,
        session: String,
        block: Option<String>,
        name: String,
        schema: Option<serde_json::Value>,
    },
}

/// The jump list: tabs, panes and the files beside the pane, filtered as you type.
struct Palette {
    query: String,
    cursor: usize,
    /// Files from `list_dir`, for the pane the palette was opened on.
    files: Vec<String>,
    actions: Vec<PaletteActionItem>,
    origin: (String, String, Option<String>),
}

/// One notification on screen, and when it should go away.
struct Toast {
    kind: shrew_core::NotificationKind,
    title: String,
    body: String,
    server_id: String,
    session_id: String,
    block_id: String,
    until: std::time::Instant,
}

/// Which page of the Settings tab is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Appearance,
    Terminal,
    Keys,
    Connection,
}

pub struct Workspace {
    engine: Arc<Engine>,
    state: AppState,
    settings: Settings,
    /// The terminal font, resolved once at startup from what is installed.
    font: SharedString,
    /// The picture behind the terminals, decoded once when the setting changes.
    picture: Option<Arc<gpui::RenderImage>>,
    server_id: Option<String>,
    session_id: Option<String>,
    window_id: Option<String>,
    /// The pane that actions apply to. Input focus is separate: clicking a pane
    /// sets both, and the server tells us which pane it believes is focused.
    active_block: Option<String>,
    /// A pane to take focus once it exists, e.g. the one a split just made.
    pending_focus: Option<String>,
    navigation: Option<(String, String, String)>,
    terminals: HashMap<String, Entity<TerminalView>>,
    styled: bool,
    show_settings: bool,
    page: Page,
    /// Keys only reach the window's listeners while something holds focus. The
    /// panes hold it while they are up; the Settings page takes it so that Esc
    /// and Ctrl-, keep working once they are unmounted.
    focus: gpui::FocusHandle,
    pull: Option<Pull>,
    /// Keeps a released pane where it was dropped until the server confirms.
    settled: Option<(String, [f64; 4])>,
    /// Where each floating pane sat before it was floated, so tiling it again
    /// can put it back on the same side with roughly the same share.
    float_rects: FloatHints,
    local_floats: model::LocalFloats,
    /// Prefix armed: the next bare key picks a tool.
    prefix: Arc<std::sync::atomic::AtomicBool>,
    /// A tool request or pane, including its scoped focus and exit lifecycle.
    pending_tool: Option<PendingTool>,
    /// The pane a tool was opened from, dimmed while the tool is up.
    tinted: Option<String>,
    /// The last press on the title bar, for spotting a double-click. Windows
    /// would send `WM_NCLBUTTONDBLCLK`, but we hand the press to the shell for
    /// dragging, which swallows the second click, so it is detected here.
    title_press: Option<(std::time::Instant, gpui::Point<gpui::Pixels>)>,
    /// Notifications the engine queued: bells, program messages, agents that
    /// stopped. Drained on every reload and shown as toasts.
    /// Where the pane area sits in the window, so a drag can be turned into the
    /// fraction the server wants.
    area_geometry: Arc<std::sync::Mutex<Option<(Point<Pixels>, Size<Pixels>)>>>,
    /// A divider being dragged between two tiled panes.
    divider: Option<Divider>,
    /// A name being typed.
    prompt: Option<Prompt>,
    /// The window's scale factor and bounds at the last frame, to notice a move
    /// to a monitor with a different DPI.
    last_geometry: Option<(f32, gpui::Bounds<Pixels>)>,
    /// `terminal.modes` for the active pane, when the Terminal page asks.
    pane_modes: Option<serde_json::Value>,
    /// `hosts.list`: the other Rex servers this one can reach.
    hosts: ServerCache<serde_json::Value>,
    /// The jump list, when it is open.
    palette: Option<Palette>,
    menu: Option<menu::Menu>,
    /// Whether this client has told the server its label yet.
    named_client: HashSet<String>,
    /// `server.whoami` and `server.status`, fetched when Connection is opened.
    server_info: ServerCache<serde_json::Value>,
    /// `client.keymap`: the bindings Rex itself would apply, from init.lua.
    keymap: ServerCache<serde_json::Value>,
    keybindings: ServerCache<Arc<std::sync::Mutex<KeyBindings>>>,
    configured_actions: Vec<(String, serde_json::Value)>,
    keymap_dirty: HashSet<String>,
    keymap_pending: HashSet<String>,
    creators: ServerCache<serde_json::Value>,
    script: Option<ScriptDraft>,
    script_diagnostics: ServerCache<Vec<String>>,
    client_requests: automation::ClientRequests,
    /// Panes this shell parked: alive, out of every window, in this list.
    parked: Vec<ParkedBlock>,
    parked_key: Option<(String, String)>,
    block_details: HashMap<(String, String, String), serde_json::Value>,
    notices: Arc<std::sync::Mutex<Vec<Notification>>>,
    toasts: std::collections::VecDeque<Toast>,
    /// A press on the title bar with the window not moved yet. The drag only
    /// starts once the pointer actually moves: handing Windows
    /// `WM_NCLBUTTONDOWN` on press restores a maximized window the moment the
    /// pointer twitches, which made double-click-to-restore impossible.
    title_armed: Option<gpui::Point<gpui::Pixels>>,
    _wake: Option<Task<()>>,
}

impl Workspace {
    pub(super) fn input_overlay_open(&self) -> bool {
        self.prompt.is_some() || self.palette.is_some() || self.menu.is_some()
    }

    pub fn new(
        engine: Arc<Engine>,
        wake: Receiver<()>,
        notices: Arc<std::sync::Mutex<Vec<Notification>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = Settings::load(&engine);
        let font = crate::fonts::resolve(cx);
        let picture = settings.background.as_deref().and_then(|path| {
            let image = crate::background::load(path);
            if image.is_none() {
                eprintln!("flex: could not load background {path}");
            }
            image
        });
        if std::env::var_os("FLEX_DEBUG").is_some() {
            eprintln!("[font] using {font}");
        }
        let mut this = Self {
            engine,
            state: AppState::default(),
            settings,
            font,
            picture,
            server_id: None,
            session_id: None,
            window_id: None,
            active_block: None,
            pending_focus: None,
            navigation: None,
            terminals: HashMap::new(),
            styled: false,
            show_settings: false,
            page: Page::Appearance,
            focus: cx.focus_handle(),
            pull: None,
            settled: None,
            float_rects: FloatHints::default(),
            local_floats: model::LocalFloats::default(),
            prefix: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_tool: None,
            tinted: None,
            area_geometry: Arc::default(),
            divider: None,
            prompt: None,
            pane_modes: None,
            hosts: ServerCache::default(),
            palette: None,
            menu: None,
            last_geometry: None,
            named_client: HashSet::new(),
            server_info: ServerCache::default(),
            keymap: ServerCache::default(),
            keybindings: ServerCache::default(),
            configured_actions: Vec::new(),
            keymap_dirty: HashSet::new(),
            keymap_pending: HashSet::new(),
            creators: ServerCache::default(),
            script: None,
            script_diagnostics: ServerCache::default(),
            client_requests: automation::ClientRequests::default(),
            parked: Vec::new(),
            parked_key: None,
            block_details: HashMap::new(),
            notices,
            toasts: std::collections::VecDeque::new(),
            title_press: None,
            title_armed: None,
            _wake: None,
        };
        this.reload(cx);
        this._wake = Some(cx.spawn(async move |this, cx| {
            while wake.recv().await.is_ok() {
                if this.update(cx, |me, cx| me.reload(cx)).is_err() {
                    break;
                }
            }
        }));
        this
    }

    /// Take what the engine queued while we were not looking.
    fn absorb_notices(&mut self, cx: &mut Context<Self>) {
        let queued: Vec<Notification> = self
            .notices
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default();
        if queued.is_empty() {
            return;
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(7);
        for notice in queued {
            self.toasts.push_back(Toast {
                kind: notice.kind,
                title: notice.title,
                body: notice.body,
                server_id: notice.server_id,
                session_id: notice.session_id,
                block_id: notice.block_id,
                until,
            });
        }
        // Three at a time: more than that is a wall, not a hint.
        while self.toasts.len() > 3 {
            self.toasts.pop_front();
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(7))
                .await;
            this.update(cx, |me, cx| {
                let now = std::time::Instant::now();
                me.toasts.retain(|toast| toast.until > now);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Go to the pane a notification came from.
    fn jump_to_block(&mut self, server: &str, session: &str, block: &str, cx: &mut Context<Self>) {
        let window = self
            .state
            .server(server)
            .and_then(|server| server.sessions.iter().find(|s| s.id == session))
            .and_then(|s| {
                s.windows
                    .iter()
                    .find(|w| w.blocks.iter().any(|b| b.id == block))
            })
            .map(|w| w.id.clone());
        if let Some(window) = window {
            self.navigate(
                server.to_owned(),
                session.to_owned(),
                window,
                Some(block.to_owned()),
                cx,
            );
        }
    }

    /// Publish the complete destination before reload can adopt the old server focus.
    fn navigate(
        &mut self,
        server: String,
        session: String,
        window: String,
        block: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.server_id = Some(server.clone());
        self.session_id = Some(session.clone());
        self.window_id = Some(window.clone());
        let destination = self
            .state
            .server(&server)
            .and_then(|s| s.sessions.iter().find(|s| s.id == session))
            .and_then(|s| s.windows.iter().find(|w| w.id == window));
        let initial = destination.and_then(|w| {
            visible_focus(&w.blocks, block.as_deref(), w.focused_block_id.as_deref())
        });
        self.active_block = initial.clone();
        self.pending_focus = initial;
        self.show_settings = false;
        self.navigation = Some((server.clone(), session.clone(), window.clone()));
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                engine
                    .shrew()
                    .focus_window(&server, &session, &window)
                    .await?;
                if let Some(block) = &block {
                    engine.shrew().focus_block(&server, &session, block).await?;
                }
                engine
                    .call_server(&server, "session.view", json!({"session_id": session}))
                    .await
            }
            .await;
            this.update(cx, |me, cx| {
                if me.navigation.as_ref()
                    != Some(&(server.clone(), session.clone(), window.clone()))
                    || me.server_id.as_ref() != Some(&server)
                    || me.session_id.as_ref() != Some(&session)
                    || me.window_id.as_ref() != Some(&window)
                {
                    return;
                }
                me.reload(cx);
                match result {
                    Ok(view) => {
                        if view.get("active_window_id").and_then(|v| v.as_str())
                            == Some(window.as_str())
                        {
                            me.adopt_focus(&view);
                            me.active_block =
                                visible_focus(&me.blocks(), me.active_block.as_deref(), None);
                            me.pending_focus = me.active_block.clone();
                        }
                    }
                    Err(e) => me.rpc_error("Focus destination", e, cx),
                }
                me.navigation = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn rpc_error(
        &mut self,
        operation: &str,
        error: impl std::fmt::Display,
        cx: &mut Context<Self>,
    ) {
        eprintln!("flex: {operation}: {error}");
        self.toasts.push_back(Toast {
            kind: shrew_core::NotificationKind::AgentError,
            title: operation.to_owned(),
            body: error.to_string(),
            server_id: String::new(),
            session_id: String::new(),
            block_id: String::new(),
            until: std::time::Instant::now() + std::time::Duration::from_secs(10),
        });
        while self.toasts.len() > 3 {
            self.toasts.pop_front();
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(10))
                .await;
            this.update(cx, |me, cx| {
                me.toasts
                    .retain(|toast| toast.until > std::time::Instant::now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the engine's state and drop selections and terminals that went away.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.absorb_notices(cx);
        self.state = self.engine.state();
        self.local_floats.prune(&self.state);
        for event in self.engine.take_cache_invalidations() {
            match event.kind.as_str() {
                "keymap_changed" => {
                    self.keymap_dirty.insert(event.server_id);
                }
                "hosts_changed" => {
                    self.hosts.remove(&event.server_id);
                }
                "client_setup_failed" => self.rpc_error("Client setup", event.payload, cx),
                _ => {}
            }
        }

        if self
            .state
            .server(self.server_id.as_deref().unwrap_or_default())
            .is_none()
        {
            self.server_id = self.state.servers.first().map(|s| s.id.clone());
        }
        let sessions = self.sessions();
        if !sessions
            .iter()
            .any(|s| Some(&s.id) == self.session_id.as_ref())
        {
            self.session_id = sessions.first().map(|s| s.id.clone());
        }
        let windows = self.windows();
        if !windows
            .iter()
            .any(|w| Some(&w.id) == self.window_id.as_ref())
        {
            self.window_id = windows.first().map(|w| w.id.clone());
        }
        // The server decides which tab is active — a new one from Ctrl-Shift-T,
        // or a switch made in another client — and the shell has to agree, or
        // its tab strip and its panes point at different windows.
        if let Some(active) = self
            .sessions()
            .into_iter()
            .find(|s| Some(&s.id) == self.session_id.as_ref())
            .and_then(|s| s.active_window_id)
        {
            if self
                .navigation
                .as_ref()
                .is_some_and(|(server, session, target)| {
                    self.server_id.as_ref() != Some(server)
                        || self.session_id.as_ref() != Some(session)
                        || self.window_id.as_ref() != Some(target)
                })
            {
                self.navigation = None;
            }
            if self.navigation.is_none() && windows.iter().any(|w| w.id == active) {
                self.window_id = Some(active);
            }
        }
        if self.hosts.get(self.server_id.as_deref()).is_none() && self.session_id.is_some() {
            self.refresh_hosts(cx);
        }
        if self.keymap.get(self.server_id.as_deref()).is_none()
            || self
                .server_id
                .as_ref()
                .is_some_and(|s| self.keymap_dirty.contains(s))
        {
            self.refresh_keymap(cx);
        }
        let parked_key = self.server_id.clone().zip(self.session_id.clone());
        if parked_key != self.parked_key {
            self.parked_key = parked_key;
            self.parked.clear();
            self.pane_modes = None;
            self.refresh_parked(cx);
            if self.keymap.get(self.server_id.as_deref()).is_none() {
                self.refresh_keymap(cx);
            }
            if self.show_settings && self.page == Page::Connection {
                self.refresh_info(cx);
            }
            if self.creators.get(self.server_id.as_deref()).is_none() {
                self.refresh_creators(cx);
            }
        }
        if self.session_id.is_some()
            && self
                .server_id
                .as_ref()
                .is_some_and(|id| !self.named_client.contains(id))
        {
            self.named_client.insert(self.server_id.clone().unwrap());
            let label = format!("flex on {}", self.device());
            self.name_client(label, cx);
        }
        let tool_waiting = self.reconcile_tool(cx);
        let blocks = self.blocks();
        let focused = windows
            .iter()
            .find(|w| Some(&w.id) == self.window_id.as_ref())
            .and_then(|w| w.focused_block_id.as_deref());
        let active = visible_focus(&blocks, self.active_block.as_deref(), focused);
        if !tool_waiting && self.active_block != active {
            self.active_block = active.clone();
            self.pending_focus = active;
        }
        if self.pending_focus.as_ref().is_some_and(|id| {
            blocks
                .iter()
                .any(|b| b.id == *id && b.layer_state == "minimized")
        }) {
            self.pending_focus = self.active_block.clone();
        }

        // Drop only blocks the engine no longer has anywhere. Pruning to the
        // visible window re-opened a stream on every state change, which fed
        // itself: each open changed state and pruned the terminal again.
        let live: HashSet<String> = self
            .state
            .servers
            .iter()
            .flat_map(|s| &s.sessions)
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.blocks)
            .map(|b| b.id.clone())
            .collect();
        // Tool reconciliation reads the picker text before terminals are pruned.
        self.terminals.retain(|id, _| live.contains(id));

        // A dropped pane stops being drawn from our own copy as soon as the
        // server reports a rect that matches it.
        if let Some((block_id, want)) = &self.settled {
            let blocks = self.blocks();
            let agreed = blocks
                .iter()
                .find(|b| &b.id == block_id)
                .is_some_and(|b| b.rect.iter().zip(want).all(|(a, b)| (a - b).abs() < 0.005));
            if agreed {
                self.settled = None;
            }
        }
        cx.notify();
    }

    fn sessions(&self) -> Vec<SessionState> {
        self.state
            .server(self.server_id.as_deref().unwrap_or_default())
            .map(|s| s.sessions.clone())
            .unwrap_or_default()
    }

    fn windows(&self) -> Vec<WindowState> {
        self.sessions()
            .into_iter()
            .find(|s| Some(&s.id) == self.session_id.as_ref())
            .map(|s| s.windows)
            .unwrap_or_default()
    }

    fn local_float_key(&self, block: &str) -> Option<model::LocalFloatKey> {
        Some((self.server_id.clone()?, self.session_id.clone()?, self.window_id.clone()?, block.to_owned()))
    }

    fn is_local_float(&self, block: &str) -> bool {
        self.local_float_key(block).is_some_and(|key| self.local_floats.0.contains_key(&key))
    }

    fn blocks(&self) -> Vec<BlockState> {
        let mut blocks = self.windows()
            .into_iter()
            .find(|w| Some(&w.id) == self.window_id.as_ref())
            .map(|w| w.blocks)
            .unwrap_or_default();
        for block in &mut blocks {
            if let Some(key) = self.local_float_key(&block.id) {
                self.local_floats.present(&key, block);
            }
        }
        blocks
    }

    fn active(&self) -> Option<BlockState> {
        let blocks: Vec<_> = self
            .blocks()
            .into_iter()
            .filter(|b| b.layer_state != "minimized")
            .collect();
        blocks
            .iter()
            .find(|b| Some(&b.id) == self.active_block.as_ref())
            .or_else(|| blocks.first())
            .cloned()
    }

    /// What this machine calls itself in the strip and in the client list.
    fn device(&self) -> String {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "this device".into())
    }

    fn address(&self) -> Option<String> {
        self.state
            .server(self.server_id.as_deref().unwrap_or_default())
            .map(|s| s.address.clone())
    }

    fn terminal(
        &mut self,
        server_id: &str,
        session_id: &str,
        block_id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        if let Some(view) = self.terminals.get(block_id) {
            if let Some(bindings) = self.keybindings.get(Some(server_id)) {
                view.update(cx, |terminal, _| terminal.set_keybindings(bindings.clone()));
            }
            return view.clone();
        }
        let engine = self.engine.clone();
        let font_size = self.settings.font_size;
        let font = self.font.clone();
        let prefix = self.prefix.clone();
        let over_picture = self.picture.is_some();
        let picture_opacity = self.settings.background_opacity;
        let scrollback = self.settings.scrollback_lines;
        let view = cx.new(|cx| {
            TerminalView::new(
                &engine,
                server_id,
                session_id,
                block_id,
                font,
                font_size,
                prefix,
                over_picture,
                picture_opacity,
                scrollback,
                cx,
            )
        });
        if let Some(bindings) = self.keybindings.get(Some(server_id)) {
            view.update(cx, |terminal, _| terminal.set_keybindings(bindings.clone()));
        }
        let source_server = server_id.to_owned();
        let source_session = session_id.to_owned();
        cx.subscribe(&view, move |this, _, event: &TerminalEvent, cx| {
            if this.server_id.as_ref() != Some(&source_server)
                || this.session_id.as_ref() != Some(&source_session)
            {
                return;
            }
            match event {
                TerminalEvent::ConfiguredAction { name, args } => {
                    this.configured_actions.push((name.clone(), args.clone()))
                }
            }
            cx.notify();
        })
        .detach();
        self.terminals.insert(block_id.to_owned(), view.clone());
        view
    }

    /// Make other clients follow this shell's tab choice.
    fn focus_window(&self, window_id: &str, cx: &mut Context<Self>) {
        let (Some(server_id), Some(session_id)) = (self.server_id.clone(), self.session_id.clone())
        else {
            return;
        };
        let engine = self.engine.clone();
        let window_id = window_id.to_owned();
        cx.spawn(async move |_, _| {
            let _ = engine
                .shrew()
                .focus_window(&server_id, &session_id, &window_id)
                .await;
        })
        .detach();
    }

    /// Leave the Settings page and hand the keyboard back to the pane that had
    /// it, because the page unmounts every terminal while it is up.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings = false;
        if self.input_overlay_open() {
            window.focus(&self.focus);
        } else if let Some(view) = self
            .active()
            .filter(|block| block.layer_state != "minimized" && block.is_terminal())
            .and_then(|block| self.terminals.get(&block.id))
        {
            window.focus(&view.read(cx).handle());
        } else {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    pub(crate) fn run_configured_action(
        &mut self,
        name: &str,
        args: serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(action) = shortcuts::configured_action(name, &args) {
            self.run(action, window, cx);
            return;
        }
        let Some(server) = self.server_id.clone() else {
            return;
        };
        let mut payload = json!({"action": name, "origin": "key"});
        if let Some(session) = &self.session_id {
            payload["session_id"] = json!(session);
        }
        if let Some(block) = &self.active_block {
            payload["block_id"] = json!(block);
        }
        if !args.is_null() {
            payload["args"] = args;
        }
        payload["servers"] = self.client_action_servers(&server);
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = engine.call_server(&server, "client.action", payload).await {
                this.update(cx, |me, cx| me.rpc_error("client.action", e, cx))
                    .ok();
            }
        })
        .detach();
    }

    // ---- actions -----------------------------------------------------------

    fn run(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_settings {
            // The panes are not on screen, so nothing else has anything to act on.
            if action == Action::Settings {
                self.close_settings(window, cx);
            }
            return;
        }
        match action {
            Action::SplitRight => self.split(false, cx),
            Action::SplitDown => self.split(true, cx),
            Action::ClosePane => self.close_pane(cx),
            Action::ToggleFloat => self.toggle_float(cx),
            Action::NextPane => self.step_focus("session.focus_next_pane", cx),
            Action::PrevPane => self.step_focus("session.focus_previous_pane", cx),
            Action::NextTab => self.step_focus("session.focus_next_window", cx),
            Action::PrevTab => self.step_focus("session.focus_previous_window", cx),
            Action::SwapPane => self.swap_pane(cx),
            Action::ClearPane => self.clear_pane(cx),
            Action::RenameTab => {
                // Empty, not prefilled: there is no select-all, so a prefilled
                // name would just get appended to.
                self.open_prompt(Ask::RenameTab, "Rename tab", window);
                cx.notify();
            }
            Action::RenamePane => {
                self.open_prompt(Ask::RenamePane, "Rename pane", window);
                cx.notify();
            }
            Action::MoveTabLeft => self.move_tab(-1, cx),
            Action::MoveTabRight => self.move_tab(1, cx),
            Action::MinimizeFloat => self.toggle_minimized(cx),
            Action::RaiseFloat => self.stack_float(true, cx),
            Action::LowerFloat => self.stack_float(false, cx),
            Action::ParkPane => self.park_pane(cx),
            Action::Palette => self.open_palette(window, cx),
            Action::CopyTranscript => self.copy_transcript(cx),
            Action::NewTab => self.new_tab(cx),
            Action::ZoomPane => self.zoom_pane(cx),
            Action::Find => self.start_find(cx),
            Action::Copy => self.copy(cx),
            Action::Paste => self.paste(cx),
            Action::FocusLeft => self.focus_direction("left", cx),
            Action::FocusDown => self.focus_direction("down", cx),
            Action::FocusUp => self.focus_direction("up", cx),
            Action::FocusRight => self.focus_direction("right", cx),
            Action::ToggleSidebar => {
                self.settings.sidebar = !self.settings.sidebar;
                self.settings.save(&self.engine);
                cx.notify();
            }
            Action::Settings => {
                self.show_settings = !self.show_settings;
                if self.show_settings {
                    window.focus(&self.focus);
                }
                cx.notify();
            }
            Action::Prefix => {
                let armed = !self.prefix.load(std::sync::atomic::Ordering::Relaxed);
                self.prefix
                    .store(armed, std::sync::atomic::Ordering::Relaxed);
                cx.notify();
            }
        }
        let _ = window;
    }

    fn split(&mut self, vertical: bool, cx: &mut Context<Self>) {
        let (Some(server_id), Some(session_id), Some(block)) = (
            self.server_id.clone(),
            self.session_id.clone(),
            self.active(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        let block_id = block.id;
        let address = self.address();
        cx.spawn(async move |this, cx| {
            // A split lands where the pane it came from is, not in `$HOME`.
            let cwd = match &address {
                Some(address) => engine.block_cwd(address, &session_id, &block_id).await,
                None => None,
            };
            match engine
                .shrew()
                .split(&server_id, &session_id, &block_id, vertical, cwd.as_deref())
                .await
            {
                Ok(Some(new_block)) => {
                    this.update(cx, |me, cx| {
                        me.active_block = Some(new_block.clone());
                        me.pending_focus = Some(new_block);
                        cx.notify();
                    })
                    .ok();
                }
                Ok(None) => {}
                Err(e) => eprintln!("flex: split: {e}"),
            }
        })
        .detach();
    }

    fn close_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(server_id), Some(session_id), Some(block)) = (
            self.server_id.clone(),
            self.session_id.clone(),
            self.active(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        let block_id = block.id;
        cx.spawn(async move |_, _| {
            if let Err(e) = engine
                .shrew()
                .close_block(&server_id, &session_id, &block_id)
                .await
            {
                eprintln!("flex: close: {e}");
            }
        })
        .detach();
    }

    /// Close a tab: the window and every pane inside it.
    fn close_window(&mut self, window: String, cx: &mut Context<Self>) {
        let (Some(server_id), Some(session_id)) = (self.server_id.clone(), self.session_id.clone())
        else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |_, _| {
            if let Err(e) = engine
                .shrew()
                .close_window(&server_id, &session_id, &window)
                .await
            {
                eprintln!("flex: close window: {e}");
            }
        })
        .detach();
    }

    /// Fill the window with the active pane, or put the layout back. Rex
    /// toggles when no `zoom` argument is given.
    fn zoom_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(server), Some(session), Some(block)) = (
            self.server_id.clone(),
            self.session_id.clone(),
            self.active(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |_, _| {
            if let Err(e) = engine
                .shrew()
                .zoom_block(&server, &session, &block.id)
                .await
            {
                eprintln!("flex: zoom: {e}");
            }
        })
        .detach();
    }

    /// Move focus one pane over, the way vim and tmux-navigator do. Rex picks the
    /// neighbour from the layout and reports it in the session view; the shell
    /// then moves its own focus handle, which is what actually routes keystrokes.
    fn focus_direction(&mut self, direction: &'static str, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(from)) =
            (self.address(), self.session_id.clone(), self.active())
        else {
            return;
        };
        let engine = self.engine.clone();
        let from = from.id;
        cx.spawn(async move |this, cx| {
            // Start from this client's pane: the server's idea of the focused
            // block may be stale, and moving from the wrong one goes nowhere.
            let here = json!({"session_id": session, "block_id": from});
            let _ = engine.call(&address, "session.focus_block", here).await;
            let payload = json!({"session_id": session, "direction": direction});
            if let Err(e) = engine
                .call(&address, "session.focus_direction", payload)
                .await
            {
                eprintln!("flex: focus_direction: {e}");
                return;
            }
            // The method answers with a revision; the new focus is in the view.
            let view = engine
                .call(&address, "session.view", json!({"session_id": session}))
                .await;
            let Ok(view) = view else { return };
            let to = view
                .get("windows")
                .and_then(|w| w.as_array())
                .and_then(|windows| {
                    let active = view.get("active_window_id").and_then(|id| id.as_str());
                    windows
                        .iter()
                        .find(|w| w.get("window_id").and_then(|id| id.as_str()) == active)
                        .or_else(|| windows.first())
                })
                .and_then(|w| w.get("focused_block_id"))
                .and_then(|id| id.as_str())
                .map(str::to_owned);
            let Some(to) = to else { return };
            let to_window = view
                .get("active_window_id")
                .and_then(|id| id.as_str())
                .map(str::to_owned);
            this.update(cx, |me, cx| {
                if me.address().as_deref() != Some(address.as_str())
                    || me.session_id.as_ref() != Some(&session)
                {
                    return;
                }
                me.active_block = Some(to.clone());
                me.pending_focus = Some(to);
                if let Some(window) = to_window {
                    me.window_id = Some(window);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Take the server's word for where focus went, which is also what other
    /// clients see.
    fn adopt_focus(&mut self, view: &serde_json::Value) {
        let active_window = view.get("active_window_id").and_then(|id| id.as_str());
        let focused = view
            .get("windows")
            .and_then(|w| w.as_array())
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|w| w.get("window_id").and_then(|id| id.as_str()) == active_window)
                    .or_else(|| windows.first())
            })
            .and_then(|w| w.get("focused_block_id"))
            .and_then(|id| id.as_str())
            .map(str::to_owned);
        if let Some(focused) = focused {
            self.active_block = Some(focused.clone());
            self.pending_focus = Some(focused);
        }
        if let Some(window) = active_window {
            self.window_id = Some(window.to_owned());
        }
    }

    /// Run a session method that moves focus, then follow where it landed.
    fn step_focus(&mut self, method: &'static str, cx: &mut Context<Self>) {
        let (Some(address), Some(session)) = (self.address(), self.session_id.clone()) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "wrap": true});
            if let Err(e) = engine.call(&address, method, payload).await {
                eprintln!("flex: {method}: {e}");
                return;
            }
            let Ok(view) = engine
                .call(&address, "session.view", json!({"session_id": session}))
                .await
            else {
                return;
            };
            this.update(cx, |me, cx| {
                if me.address().as_deref() != Some(address.as_str())
                    || me.session_id.as_ref() != Some(&session)
                {
                    return;
                }
                me.adopt_focus(&view);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Exchange this pane with the next one in the window.
    fn swap_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(here)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        let blocks = self.blocks();
        let Some(other) = blocks
            .iter()
            .map(|b| b.id.clone())
            .filter(|id| *id != here)
            .next()
        else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "block_id": here,
                "other_block_id": other,
            });
            if let Err(e) = engine.call(&address, "session.swap_blocks", payload).await {
                eprintln!("flex: swap: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Empty the screen and scrollback, the way Ctrl-L and `clear` do.
    fn clear_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "block_id": block, "args": {}});
            if let Err(e) = engine
                .call(&address, "com.superlogical.terminal.clear", payload)
                .await
            {
                eprintln!("flex: clear: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn open_prompt(&mut self, what: Ask, label: &str, window: &mut Window) {
        self.prompt = Some(Prompt {
            what,
            label: label.to_owned(),
            value: String::new(),
            scope: None,
        });
        // The pane under the overlay would keep the keyboard otherwise, and
        // typing a name would also type it into the shell.
        window.focus(&self.focus);
        window.refresh();
    }

    /// Type into the prompt. Returns true when the key belonged to it.
    fn prompt_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(prompt) = &mut self.prompt else {
            return false;
        };
        let modifiers = &event.keystroke.modifiers;
        if (modifiers.control || modifiers.platform)
            && event.keystroke.key.eq_ignore_ascii_case("v")
        {
            if let Some(text) = crate::win32::clipboard_text() {
                prompt.paste(&text);
            }
            cx.notify();
            return true;
        }
        if modifiers.control || modifiers.platform || modifiers.alt {
            return true;
        }
        match event.keystroke.key.as_str() {
            // Space is a named key, so it never comes with a key_char.
            "space" if prompt.what.wants_text() => {
                prompt.value.push(' ');
                cx.notify();
                true
            }
            "escape" => {
                self.prompt = None;
                cx.notify();
                true
            }
            "enter" => {
                let prompt = self.prompt.take().unwrap();
                self.commit_prompt(prompt, cx);
                true
            }
            "backspace" => {
                prompt.value.pop();
                cx.notify();
                true
            }
            _ => {
                let typed = event
                    .keystroke
                    .key_char
                    .clone()
                    .filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
                match typed {
                    Some(text) if prompt.what.wants_text() => {
                        prompt.value.push_str(&text);
                        cx.notify();
                        true
                    }
                    Some(_) => true,
                    // Chords with a modifier are the shell's, not the prompt's.
                    None => {
                        !event.keystroke.modifiers.control
                            && !event.keystroke.modifiers.alt
                            && !event.keystroke.modifiers.platform
                    }
                }
            }
        }
    }

    fn refresh_creators(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.server_id.clone() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .call_server(&server, "block_creator.list", json!({}))
                .await;
            this.update(cx, |me, cx| {
                match result {
                    Ok(value) => {
                        me.creators.insert(server, value);
                    }
                    Err(e) => me.rpc_error("block_creator.list", e, cx),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn choose_script(&mut self, cx: &mut Context<Self>) {
        let options = gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a Lua script".into()),
        };
        cx.spawn(async move |this, cx| {
            let Ok(receiver) = cx.update(|cx| cx.prompt_for_paths(options)) else {
                return;
            };
            let picked = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = picked else { return };
            let result = cx
                .background_executor()
                .spawn(async move {
                    std::fs::read(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|bytes| script_draft(&path, &bytes))
                })
                .await;
            match result {
                Ok(script) => this
                    .update(cx, |me, cx| {
                        me.script = Some(script);
                        cx.notify();
                    })
                    .ok(),
                Err(e) => this
                    .update(cx, |me, cx| me.rpc_error("Read script", e, cx))
                    .ok(),
            };
        })
        .detach();
    }

    fn set_script(&mut self, unload: bool, cx: &mut Context<Self>) {
        let Some(server) = self.server_id.clone() else {
            return;
        };
        if !unload && self.script.is_none() {
            self.rpc_error("Load script", "Choose a .lua file first", cx);
            return;
        }
        let payload = script_request(if unload { None } else { self.script.as_ref() });
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            match engine
                .call_server(&server, "client.set_script", payload)
                .await
            {
                Ok(reply) => this
                    .update(cx, |me, cx| {
                        let mut diagnostics: Vec<String> = reply
                            .get("diagnostics")
                            .and_then(|v| v.as_array())
                            .map(|a| a.iter().map(|d| d.to_string()).collect())
                            .unwrap_or_default();
                        if diagnostics.is_empty() {
                            diagnostics.push(if unload {
                                "Scripts unloaded".into()
                            } else {
                                "Script loaded".into()
                            });
                        }
                        me.script_diagnostics.insert(server.clone(), diagnostics);
                        me.keymap_dirty.insert(server.clone());
                        me.refresh_keymap_for(server.clone(), cx);
                        cx.notify();
                    })
                    .ok(),
                Err(e) => this
                    .update(cx, |me, cx| {
                        me.rpc_error("client.set_script", e, cx);
                        me.refresh_keymap_for(server, cx);
                    })
                    .ok(),
            };
        })
        .detach();
    }

    fn run_client_action(
        &mut self,
        server: String,
        session: String,
        block: Option<String>,
        name: String,
        args: Option<serde_json::Value>,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine.clone();
        let servers = self.client_action_servers(&server);
        cx.spawn(async move |this, cx| {
            let mut payload = action_request(&session, block.as_deref(), &name, args);
            payload["servers"] = servers;
            if let Err(e) = engine.call_server(&server, "client.action", payload).await {
                this.update(cx, |me, cx| me.rpc_error("client.action", e, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn create_block(
        &mut self,
        server: String,
        session: String,
        anchor: Option<String>,
        flavor: String,
        options_text: String,
        cx: &mut Context<Self>,
    ) {
        let options = match json_object(&options_text) {
            Ok(value) => value,
            Err(e) => {
                self.rpc_error("Create block", e, cx);
                return;
            }
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let (method, payload) = creator_request(&session, anchor.as_deref(), &flavor, options);
            match engine.call_server(&server, method, payload).await {
                Ok(_) => this.update(cx, |me, cx| me.reload(cx)).ok(),
                Err(e) => this
                    .update(cx, |me, cx| me.rpc_error("Create block", e, cx))
                    .ok(),
            };
        })
        .detach();
    }

    fn commit_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        if prompt
            .scope
            .as_ref()
            .is_some_and(|target| !target.valid(&self.engine.state()))
        {
            self.rpc_error(
                "Menu action",
                "The original target is no longer available",
                cx,
            );
            return;
        }
        let value = prompt.value.trim().to_owned();
        if matches!(
            &prompt.what,
            Ask::CreateBlock { .. } | Ask::RunAction { .. }
        ) {
            if let Err(error) = json_object(&value) {
                self.rpc_error("JSON input", error, cx);
                self.prompt = Some(prompt);
                return;
            }
        }
        if prompt.what.wants_text() && value.is_empty() {
            cx.notify();
            return;
        }

        // Servers are this client's own list, not something Rex keeps.
        match &prompt.what {
            Ask::AddServer => {
                let mut words: Vec<&str> = value.split_whitespace().collect();
                let Some(address) = words.pop() else {
                    return;
                };
                let name = words.join(" ");
                if let Err(e) = self.engine.add_server(&name, address) {
                    self.rpc_error("Add server", e, cx);
                    return;
                }
                self.state = self.engine.state();
                self.server_id = self
                    .state
                    .servers
                    .iter()
                    .find(|s| s.address.trim() == address.trim())
                    .map(|s| s.id.clone());
                self.reload(cx);
                return;
            }
            Ask::DeleteServer(id, _) => {
                self.engine.shrew().remove_server(id);
                self.state = self.engine.state();
                if self.server_id.as_deref() == Some(id.as_str()) {
                    self.server_id = self.state.servers.first().map(|s| s.id.clone());
                    self.session_id = None;
                    self.window_id = None;
                    self.active_block = None;
                }
                self.reload(cx);
                return;
            }
            _ => {}
        }

        if let Ask::CreateBlock {
            server,
            session,
            anchor,
            flavor,
            ..
        } = &prompt.what
        {
            self.create_block(
                server.clone(),
                session.clone(),
                anchor.clone(),
                flavor.clone(),
                value,
                cx,
            );
            return;
        }
        if let Ask::RunAction {
            server,
            session,
            block,
            name,
            ..
        } = &prompt.what
        {
            let Ok(args) = json_object(&value) else {
                self.rpc_error("Run action", "Arguments must be a JSON object", cx);
                return;
            };
            self.run_client_action(
                server.clone(),
                session.clone(),
                block.clone(),
                name.clone(),
                Some(args),
                cx,
            );
            return;
        }
        if let Ask::CloseLayer(server, session, layer) = &prompt.what {
            let Some(address) = self.state.server(server).map(|s| s.address.clone()) else {
                return;
            };
            let session = session.clone();
            let engine = self.engine.clone();
            let block = layer.clone();
            cx.spawn(async move |this, cx| {
                let view = match engine
                    .call(&address, "session.view", json!({"session_id": session}))
                    .await
                {
                    Ok(view) => view,
                    Err(e) => {
                        this.update(cx, |me, cx| me.rpc_error("session.view", e, cx))
                            .ok();
                        return;
                    }
                };
                let Some(layer) = floating_layer_id(&view, &block) else {
                    this.update(cx, |me, cx| {
                        me.rpc_error("Close layer", "Floating layer not found", cx)
                    })
                    .ok();
                    return;
                };
                match engine
                    .call(
                        &address,
                        "session.close_layer",
                        json!({"session_id": session, "layer_id": layer}),
                    )
                    .await
                {
                    Ok(_) => this
                        .update(cx, |me, cx| {
                            me.reload(cx);
                        })
                        .ok(),
                    Err(e) => this
                        .update(cx, |me, cx| me.rpc_error("session.close_layer", e, cx))
                        .ok(),
                };
            })
            .detach();
            return;
        }

        // Everything else is a Rex call. A session id is not a precondition:
        // creating the first session has to work when there is none.
        let address = prompt
            .scope
            .as_ref()
            .map(|target| target.address.clone())
            .or_else(|| match &prompt.what {
                Ask::DeleteSession(server, ..) | Ask::RenameSession(server, ..) => {
                    self.state.server(server).map(|s| s.address.clone())
                }
                _ => self.address(),
            });
        let Some(address) = address else { return };
        let source_server = prompt
            .scope
            .as_ref()
            .map(|target| target.server.clone())
            .or_else(|| {
                self.state
                    .servers
                    .iter()
                    .find(|s| s.address == address)
                    .map(|s| s.id.clone())
            });
        let session = self.session_id.clone();
        let fresh_session = matches!(&prompt.what, Ask::NewSession(..));
        let touched_hosts = matches!(&prompt.what, Ask::NewHost | Ask::DeleteHost(..));
        let deleted_session = matches!(&prompt.what, Ask::DeleteSession(_, id, _) if self.session_id.as_ref() == Some(id));
        let engine = self.engine.clone();
        let (target, method, payload) = match &prompt.what {
            Ask::RenameSession(server, id, _) => {
                let Some(address) = self.state.server(server).map(|s| s.address.clone()) else {
                    return;
                };
                (
                    address,
                    "session.set_label",
                    json!({"session_id": id, "label": value}),
                )
            }
            Ask::RenameTailscale => (
                address.clone(),
                "server.tailscale.rename",
                json!({"hostname": value}),
            ),
            Ask::EditServer(id, _, _) => {
                let mut words = value.split_whitespace();
                let Some(new_address) = words.next_back() else {
                    return;
                };
                let new_name = words.collect::<Vec<_>>().join(" ");
                if let Err(e) = self
                    .engine
                    .shrew()
                    .update_server(id, &new_name, new_address)
                {
                    self.rpc_error("Update server", e, cx);
                }
                self.hosts.remove(id);
                self.reload(cx);
                return;
            }
            Ask::RenameTab => {
                let (Some(window_id), Some(session)) = (self.window_id.clone(), session) else {
                    return;
                };
                (
                    address.clone(),
                    "session.set_window_label",
                    json!({"session_id": session, "window_id": window_id, "label": value}),
                )
            }
            Ask::RenamePane => {
                let (Some(block_id), Some(session)) = (self.active_block.clone(), session) else {
                    return;
                };
                (
                    address.clone(),
                    "session.set_block_label",
                    json!({"session_id": session, "block_id": block_id, "label": value}),
                )
            }
            Ask::NewSession(machine) => {
                (machine.clone(), "session.create", json!({"label": value}))
            }
            Ask::DeleteSession(_, id, _) => (
                address.clone(),
                "session.destroy",
                json!({"session_id": id}),
            ),
            Ask::NewHost => {
                // "studio studio.tail1234.ts.net": the last word is the address.
                let mut words: Vec<&str> = value.split_whitespace().collect();
                let Some(endpoint) = words.pop() else {
                    return;
                };
                let label = words.join(" ");
                let label = if label.is_empty() {
                    endpoint.to_owned()
                } else {
                    label
                };
                (
                    address.clone(),
                    "hosts.put",
                    json!({"host": {"label": label, "endpoints": [endpoint]}}),
                )
            }
            Ask::DeleteHost(id, _) => (address.clone(), "hosts.remove", json!({"id": id})),
            Ask::AddServer
            | Ask::DeleteServer(..)
            | Ask::CloseLayer(..)
            | Ask::CreateBlock { .. }
            | Ask::RunAction { .. } => return,
        };
        cx.spawn(async move |this, cx| {
            let reply = engine.call(&target, method, payload).await;
            if let Err(e) = &reply {
                this.update(cx, |me, cx| me.rpc_error(method, e, cx)).ok();
                return;
            }
            let created = reply.ok().and_then(|reply| {
                reply
                    .get("session_id")
                    .or_else(|| reply.pointer("/session/session_id"))
                    .and_then(|id| id.as_str())
                    .map(str::to_owned)
            });
            this.update(cx, |me, cx| {
                if touched_hosts {
                    if let Some(server) = &source_server {
                        me.hosts.remove(server);
                    }
                    if me.server_id == source_server {
                        me.refresh_hosts(cx);
                    }
                }
                if fresh_session {
                    if let Some(created) = created {
                        // A brand-new session has no window yet; give it one,
                        // and switch to it so the tab bar follows.
                        if let Some(server) = me
                            .state
                            .servers
                            .iter()
                            .find(|s| s.address.trim() == target.trim())
                        {
                            me.server_id = Some(server.id.clone());
                        }
                        me.session_id = Some(created);
                        me.window_id = None;
                        me.active_block = None;
                        me.new_tab(cx);
                    }
                }
                if deleted_session && me.server_id == source_server {
                    me.window_id = None;
                    me.active_block = None;
                }
                me.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    fn move_session(&mut self, server: String, session: String, step: i32, cx: &mut Context<Self>) {
        let Some(server_state) = self.state.server(&server) else {
            return;
        };
        let ids: Vec<String> = server_state.sessions.iter().map(|s| s.id.clone()).collect();
        let Some(before) = move_before(&ids, &session, step) else {
            return;
        };
        let address = server_state.address.clone();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "before_session_id": before});
            if let Err(e) = engine.call(&address, "session.move", payload).await {
                this.update(cx, |me, cx| me.rpc_error("session.move", e, cx))
                    .ok();
            } else {
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        })
        .detach();
    }

    /// Move this tab one place along, past its neighbour.
    fn move_tab(&mut self, step: i32, cx: &mut Context<Self>) {
        let (Some(address), Some(session)) = (self.address(), self.session_id.clone()) else {
            return;
        };
        let windows = self.windows();
        let Some(at) = windows
            .iter()
            .position(|w| Some(&w.id) == self.window_id.as_ref())
        else {
            return;
        };
        let to = at as i32 + step;
        if to < 0 || to as usize >= windows.len() {
            return;
        }
        // Where it should land: beside the window now next to it.
        let before = if step < 0 {
            windows.get(to as usize).map(|w| w.id.clone())
        } else {
            windows.get(to as usize + 1).map(|w| w.id.clone())
        };
        let Some(window_id) = self.window_id.clone() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "window_id": window_id,
                "before_window_id": before,
            });
            if let Err(e) = engine.call(&address, "session.move_window", payload).await {
                eprintln!("flex: move_window: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Commit layer state remotely before changing what this client shows.
    fn toggle_minimized(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.blocks()
                .into_iter()
                .find(|b| self.active_block.as_ref() == Some(&b.id)),
        ) else {
            return;
        };
        if !block.floating {
            return;
        }
        if let Some(local) = self.local_float_key(&block.id).and_then(|key| self.local_floats.0.get_mut(&key)) {
            local.minimized = !local.minimized;
            self.pending_focus = if local.minimized { None } else { Some(block.id.clone()) };
            cx.notify();
            return;
        }
        let restore = block.layer_state == "minimized";
        let block = block.id;
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let view = engine.call(&address, "session.view", json!({"session_id": session})).await?;
                let layer = floating_layer_id(&view, &block).ok_or_else(|| flex_engine::CoreError::Server("Floating layer not found".into()))?;
                engine.call(&address, "session.set_layer_state", json!({"session_id": session,"layer_id": layer,"state": if restore { "normal" } else { "minimized" },"focus": restore})).await
            }.await;
            this.update(cx, |me, cx| {
                match result {
                    Ok(_) => {
                        if !restore && me.active_block.as_deref() == Some(block.as_str()) {
                            me.active_block = me.blocks().iter().find(|b| b.layer_state != "minimized" && b.id != block).map(|b| b.id.clone());
                            me.pending_focus = me.active_block.clone();
                        }
                        cx.notify();
                    }
                    Err(e) => me.rpc_error("session.set_layer_state", e, cx),
                }
            }).ok();
        }).detach();
    }

    fn connection_call(
        &mut self,
        method: &'static str,
        payload: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let (Some(address), Some(server)) = (self.address(), self.server_id.clone()) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            match engine.call(&address, method, payload).await {
                Ok(reply) => this
                    .update(cx, |me, cx| {
                        if let Some(diagnostics) = reply
                            .get("diagnostics")
                            .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
                        {
                            me.rpc_error(method, diagnostics, cx);
                        }
                        me.server_info.remove(&server);
                        me.keymap_dirty.insert(server.clone());
                        if me.server_id.as_ref() == Some(&server) {
                            me.refresh_info(cx);
                            me.refresh_keymap(cx);
                            me.refresh_hosts(cx);
                        }
                    })
                    .ok(),
                Err(e) => this.update(cx, |me, cx| me.rpc_error(method, e, cx)).ok(),
            };
        })
        .detach();
    }

    /// Ask the server who we are and what it is running, for the Connection page.
    fn refresh_info(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(server)) = (self.address(), self.server_id.clone()) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let whoami = engine.call(&address, "server.whoami", json!({})).await;
            let status = engine.call(&address, "server.status", json!({})).await;
            let clients = engine.call(&address, "client.list", json!({})).await;
            this.update(cx, |me, cx| {
                for (operation, reply) in [
                    ("server.whoami", &whoami),
                    ("server.status", &status),
                    ("client.list", &clients),
                ] {
                    if let Err(e) = reply {
                        me.rpc_error(operation, e, cx);
                    }
                }
                me.server_info.insert(
                    server,
                    json!({
                        "whoami": whoami.ok(),
                        "status": status.ok(),
                        "clients": clients.ok(),
                    }),
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ask the server for the keymap it would apply, which includes whatever
    /// the user wrote in init.lua.
    fn refresh_keymap(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.server_id.clone() else {
            return;
        };
        self.refresh_keymap_for(server, cx);
    }

    fn refresh_keymap_for(&mut self, server: String, cx: &mut Context<Self>) {
        if !self
            .state
            .server(&server)
            .is_some_and(|s| s.connectivity == Connectivity::Online)
        {
            return;
        }
        if !self.keymap_pending.insert(server.clone()) {
            return;
        }
        self.keymap_dirty.remove(&server);
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let keymap = engine
                .call_server(&server, "client.keymap", shortcuts::keymap_request())
                .await;
            this.update(cx, |me, cx| {
                me.keymap_pending.remove(&server);
                if me.keymap_dirty.contains(&server) {
                    me.refresh_keymap_for(server.clone(), cx);
                    return;
                }
                match keymap {
                    Ok(value) => {
                        if keymap_resolution_changed(me.keymap.get(Some(&server)), &value) {
                            let bindings = me
                                .keybindings
                                .get(Some(&server))
                                .cloned()
                                .unwrap_or_default();
                            if let Ok(mut map) = bindings.lock() {
                                map.update(value.clone());
                            }
                            me.keybindings.insert(server.clone(), bindings);
                        }
                        if let Some(palette) = &mut me.palette {
                            if palette.origin.0 == server {
                                palette.actions = palette_actions(
                                    &value,
                                    &palette.origin.0,
                                    &palette.origin.1,
                                    palette.origin.2.clone(),
                                );
                            }
                        }
                        me.keymap.insert(server.clone(), value);
                    }
                    Err(e) => me.rpc_error("refresh_keymap", e, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn tailscale_call(&mut self, method: &'static str, cx: &mut Context<Self>) {
        self.connection_call(method, json!({}), cx);
    }

    /// Turn on the server's built-in Tailscale, or ask it for a sign-in link.
    fn tailscale(&mut self, sign_in: bool, cx: &mut Context<Self>) {
        let Some(address) = self.address() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let method = if sign_in {
                "server.tailscale.login"
            } else {
                "server.tailscale.enable"
            };
            match engine.call(&address, method, json!({})).await {
                Ok(reply) => {
                    // login answers with a URL to open; enable just starts it.
                    if let Some(url) = reply
                        .get("url")
                        .or_else(|| reply.get("login_url"))
                        .and_then(|u| u.as_str())
                    {
                        crate::win32::open_url(url);
                    }
                }
                Err(e) => {
                    this.update(cx, |me, cx| me.rpc_error(method, e, cx)).ok();
                }
            }
            this.update(cx, |me, cx| me.refresh_info(cx)).ok();
        })
        .detach();
    }

    /// Name this client, so other clients list it as something readable.
    fn name_client(&mut self, label: String, cx: &mut Context<Self>) {
        let Some(address) = self.address() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = engine
                .call(&address, "client.set_label", json!({"label": label}))
                .await
            {
                this.update(cx, |me, cx| me.rpc_error("client.set_label", e, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Ask the active pane what its terminal modes are: alternate screen,
    /// mouse reporting, bracketed paste. None of it needs the terminal awake.
    fn refresh_modes(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "block_id": block, "args": {}});
            let modes = engine
                .call(&address, "com.superlogical.terminal.modes", payload)
                .await;
            this.update(cx, |me, cx| {
                if me.address().as_deref() != Some(address.as_str())
                    || me.session_id.as_ref() != Some(&session)
                    || me.active_block.as_ref() != Some(&block)
                {
                    return;
                }
                me.pane_modes = modes.ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ask the server which other servers it can reach.
    fn refresh_hosts(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(server)) = (self.address(), self.server_id.clone()) else {
            return;
        };
        self.refresh_hosts_for(address, server, cx);
    }

    fn refresh_hosts_for(&mut self, address: String, server: String, cx: &mut Context<Self>) {
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let hosts = engine.call(&address, "hosts.list", json!({})).await;
            this.update(cx, |me, cx| {
                if !me
                    .state
                    .server(&server)
                    .is_some_and(|s| s.address == address)
                {
                    return;
                }
                match hosts {
                    Ok(value) => {
                        me.hosts.insert(server.clone(), value);
                        me.update_directory_menu(&server);
                    }
                    Err(e) => {
                        me.directory_failed(&server);
                        me.rpc_error("refresh_hosts", e, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the jump list and ask the pane for the files next to it.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(Palette {
            query: String::new(),
            cursor: 0,
            files: Vec::new(),
            actions: Vec::new(),
            origin: (
                self.server_id.clone().unwrap_or_default(),
                self.session_id.clone().unwrap_or_default(),
                self.active_block.clone(),
            ),
        });
        window.focus(&self.focus);
        cx.notify();

        if let Some(server) = self.server_id.clone() {
            let session = self.session_id.clone().unwrap_or_default();
            let block = self.active_block.clone();
            if let Some(map) = self.keymap.get(Some(&server)) {
                if let Some(palette) = &mut self.palette {
                    palette.actions = palette_actions(map, &server, &session, block);
                }
            } else {
                self.refresh_keymap(cx);
            }
        }
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "block_id": block,
                "args": {"files": true, "limit": 400},
            });
            let Ok(reply) = engine
                .call(&address, "com.superlogical.terminal.list_dir", payload)
                .await
            else {
                return;
            };
            let root = reply
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or_default()
                .to_owned();
            let files: Vec<String> = reply
                .get("entries")
                .and_then(|e| e.as_array())
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|entry| {
                            let name = entry.get("name").and_then(|n| n.as_str())?;
                            let is_file =
                                entry.get("file").and_then(|f| f.as_bool()).unwrap_or(false);
                            Some(if is_file {
                                format!("{root}/{name}")
                            } else {
                                format!("{root}/{name}/")
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            this.update(cx, |me, cx| {
                if me.address().as_deref() != Some(address.as_str())
                    || me.session_id.as_ref() != Some(&session)
                    || me.active_block.as_ref() != Some(&block)
                {
                    return;
                }
                if let Some(palette) = &mut me.palette {
                    palette.files = files;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// What the palette would show right now, filtered by the query.
    fn palette_items(&self) -> Vec<(String, Jump)> {
        let Some(palette) = &self.palette else {
            return Vec::new();
        };
        let actions_only = palette.query.starts_with('>');
        let needle = palette.query.trim_start_matches('>').trim().to_lowercase();
        let keep = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
        let mut items: Vec<(String, Jump)> = Vec::new();
        for server in self.state.servers.iter().filter(|_| !actions_only) {
            for session in &server.sessions {
                for window in &session.windows {
                    if keep(&window.label) {
                        items.push((
                            format!("{} · {}", session.label, window.label),
                            Jump::Window {
                                server: server.id.clone(),
                                session: session.id.clone(),
                                window: window.id.clone(),
                            },
                        ));
                    }
                    for block in &window.blocks {
                        let name = block.display_name();
                        if !keep(&name) && !keep(&window.label) {
                            continue;
                        }
                        items.push((
                            format!("{} · {}", window.label, name),
                            Jump::Block {
                                server: server.id.clone(),
                                session: session.id.clone(),
                                window: window.id.clone(),
                                block: block.id.clone(),
                            },
                        ));
                    }
                }
            }
        }
        for file in palette.files.iter().filter(|_| !actions_only) {
            if keep(file) {
                items.push((file.clone(), Jump::File(file.clone())));
            }
        }
        for action in &palette.actions {
            if keep(&action.title) || keep(&action.name) || keep(&action.description) {
                items.push((
                    format!("{} · {}", action.title, action.description),
                    Jump::Action {
                        server: action.server.clone(),
                        session: action.session.clone(),
                        block: action.block.clone(),
                        name: action.name.clone(),
                        schema: action.args_schema.clone(),
                    },
                ));
            }
        }
        items
    }

    /// Type into the palette. Returns true when the key belonged to it.
    fn palette_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.palette.is_none() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "space" => {
                if let Some(palette) = &mut self.palette {
                    palette.query.push(' ');
                    palette.cursor = 0;
                }
                cx.notify();
                true
            }
            "escape" => {
                self.palette = None;
                cx.notify();
                true
            }
            "up" => {
                if let Some(palette) = &mut self.palette {
                    palette.cursor = palette.cursor.saturating_sub(1);
                }
                cx.notify();
                true
            }
            "down" => {
                let count = self.palette_items().len();
                if let Some(palette) = &mut self.palette {
                    palette.cursor = (palette.cursor + 1).min(count.saturating_sub(1));
                }
                cx.notify();
                true
            }
            "backspace" => {
                if let Some(palette) = &mut self.palette {
                    palette.query.pop();
                    palette.cursor = 0;
                }
                cx.notify();
                true
            }
            "enter" => {
                let items = self.palette_items();
                let cursor = self.palette.as_ref().map(|p| p.cursor).unwrap_or(0);
                self.palette = None;
                if let Some((_, jump)) = items.into_iter().nth(cursor) {
                    self.take_jump(jump, window, cx);
                }
                cx.notify();
                true
            }
            _ => {
                let typed = event
                    .keystroke
                    .key_char
                    .clone()
                    .filter(|text| !text.is_empty() && !text.chars().any(char::is_control));
                match typed {
                    Some(text) => {
                        if let Some(palette) = &mut self.palette {
                            palette.query.push_str(&text);
                            palette.cursor = 0;
                        }
                        cx.notify();
                        true
                    }
                    None => {
                        !event.keystroke.modifiers.control
                            && !event.keystroke.modifiers.alt
                            && !event.keystroke.modifiers.platform
                    }
                }
            }
        }
    }

    fn take_jump(&mut self, jump: Jump, window: &mut Window, cx: &mut Context<Self>) {
        match jump {
            Jump::Block {
                server,
                session,
                window,
                block,
            } => {
                self.server_id = Some(server.clone());
                self.session_id = Some(session.clone());
                self.window_id = Some(window);
                self.jump_to_block(&server, &session, &block, cx);
            }
            Jump::Window {
                server,
                session,
                window,
            } => {
                self.navigate(server, session, window, None, cx);
            }
            Jump::Action {
                server,
                session,
                block,
                name,
                schema,
            } => {
                if let Some(schema) = schema {
                    self.open_prompt(
                        Ask::RunAction {
                            server,
                            session,
                            block,
                            name,
                            schema,
                        },
                        "Action arguments (JSON object)",
                        window,
                    );
                    cx.notify();
                } else {
                    self.run_client_action(server, session, block, name, None, cx);
                }
            }
            Jump::File(path) => {
                // Type it into the pane, the way the fzf picker does.
                let (Some(address), Some(session), Some(block)) = (
                    self.address(),
                    self.session_id.clone(),
                    self.active_block.clone(),
                ) else {
                    return;
                };
                let engine = self.engine.clone();
                cx.spawn(async move |_, _| {
                    let payload = json!({
                        "session_id": session,
                        "block_id": block,
                        "args": {"data": path},
                    });
                    let _ = engine
                        .call(&address, "com.superlogical.terminal.write", payload)
                        .await;
                })
                .detach();
            }
        }
    }

    /// Move this floating pane to the front or the back of the stack.
    fn stack_float(&mut self, front: bool, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        if self.is_local_float(&block) {
            self.rpc_error("Local floating pane", "Local panes have no server layer order; restore with Ctrl-Shift-F first.", cx);
            return;
        }
        let engine = self.engine.clone();
        cx.spawn(async move |_, _| {
            let Ok(view) = engine
                .call(&address, "session.view", json!({"session_id": session}))
                .await
            else {
                return;
            };
            let Some(layer) = find_layer(&view, &block) else {
                return;
            };
            let method = if front {
                "session.raise_layer"
            } else {
                "session.lower_layer"
            };
            let payload = json!({"session_id": session, "layer_id": layer});
            if let Err(e) = engine.call(&address, method, payload).await {
                eprintln!("flex: {method}: {e}");
            }
        })
        .detach();
    }

    /// Park this pane: the program keeps running, the window is out of the way.
    fn park_pane(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(server), Some(session), Some(block)) = (
            self.address(),
            self.server_id.clone(),
            self.session_id.clone(),
            self.active(),
        ) else {
            return;
        };
        let id = block.id.clone();
        let label = block.display_name();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "block_id": id});
            if let Err(e) = engine.call(&address, "session.detach_block", payload).await {
                this.update(cx, |me, cx| me.rpc_error("session.detach_block", e, cx))
                    .ok();
                return;
            }
            this.update(cx, |me, cx| {
                if me.server_id.as_ref() != Some(&server)
                    || me.session_id.as_ref() != Some(&session)
                {
                    return;
                }
                me.parked.push(ParkedBlock {
                    server: server.clone(),
                    session: session.clone(),
                    id: id.clone(),
                    label: label.clone(),
                });
                if me.active_block.as_deref() == Some(id.as_str()) {
                    me.active_block = me.blocks().first().map(|b| b.id.clone());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Put a parked pane back beside the active one.
    fn refresh_parked(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(server), Some(session)) = (
            self.address(),
            self.server_id.clone(),
            self.session_id.clone(),
        ) else {
            return;
        };
        self.refresh_parked_for(address, server, session, cx);
    }

    fn refresh_parked_for(
        &mut self,
        address: String,
        server: String,
        session: String,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let reply = engine
                .call(
                    &address,
                    "session.list_blocks",
                    json!({"session_id": session}),
                )
                .await;
            this.update(cx, |me, cx| {
                if me.server_id.as_deref() != Some(server.as_str())
                    || me.session_id.as_deref() != Some(session.as_str())
                    || !me
                        .state
                        .server(&server)
                        .is_some_and(|s| s.address == address)
                {
                    return;
                }
                match reply {
                    Ok(value) => {
                        me.parked = value
                            .get("blocks")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|block| {
                                if matches!(
                                    block.get("placement").and_then(|v| v.as_str()),
                                    Some("placed")
                                ) {
                                    return None;
                                }
                                let id = block
                                    .get("block_id")
                                    .or_else(|| block.get("id"))
                                    .and_then(|v| v.as_str())?
                                    .to_owned();
                                let label = block
                                    .get("label")
                                    .or_else(|| block.get("title"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("Detached block")
                                    .to_owned();
                                Some(ParkedBlock {
                                    server: server.clone(),
                                    session: session.clone(),
                                    id,
                                    label,
                                })
                            })
                            .collect();
                    }
                    Err(e) => me.rpc_error("session.list_blocks", e, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Put a parked pane back beside the active one, creating an empty anchor window if needed.
    fn unpark(&mut self, parked: ParkedBlock, cx: &mut Context<Self>) {
        let Some(address) = self.state.server(&parked.server).map(|s| s.address.clone()) else {
            return;
        };
        let session = parked.session.clone();
        let engine = self.engine.clone();
        let id = parked.id.clone();
        let anchor = if self.server_id.as_ref() == Some(&parked.server)
            && self.session_id.as_ref() == Some(&parked.session)
        {
            self.active().map(|b| b.id)
        } else {
            None
        };
        cx.spawn(async move |this, cx| {
            let anchor = match anchor {
                Some(anchor) => anchor,
                None => match engine.call(&address, "session.new_window", json!({
                    "session_id": session, "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}}
                })).await {
                    Ok(view) => match view.pointer("/block_ids/0").and_then(|v| v.as_str()) { Some(id) => id.to_owned(), None => { this.update(cx, |me, cx| me.rpc_error("session.new_window", "no anchor returned", cx)).ok(); return; } },
                    Err(e) => { this.update(cx, |me, cx| me.rpc_error("session.new_window", e, cx)).ok(); return; }
                }
            };
            let result = engine.call(&address, "session.move_block", json!({"session_id": session, "block_id": id, "anchor_block_id": anchor, "side": "after", "direction": "horizontal", "ratio": 0.5})).await;
            this.update(cx, |me, cx| match result {
                Ok(_) if me.server_id.as_ref() == Some(&parked.server) && me.session_id.as_ref() == Some(&parked.session) => { me.parked.retain(|p| p.id != parked.id || p.server != parked.server || p.session != parked.session); me.pending_focus = Some(parked.id); cx.notify(); }
                Ok(_) => {},
                Err(e) => me.rpc_error("session.move_block", e, cx),
            }).ok();
        }).detach();
    }

    /// Copy everything the pane can still show, from the server's own
    /// scrollback rather than from the frames this shell happens to hold.
    fn copy_transcript(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session), Some(block)) = (
            self.address(),
            self.session_id.clone(),
            self.active_block.clone(),
        ) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "block_id": block,
                "args": {"format": "text", "trim": true},
            });
            match engine
                .call(&address, "com.superlogical.terminal.format", payload)
                .await
            {
                Ok(reply) => {
                    if let Some(text) = reply.get("content").and_then(|c| c.as_str()) {
                        crate::win32::set_clipboard_text(text);
                    }
                }
                Err(e) => eprintln!("flex: format: {e}"),
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Draggable strips along the active pane's own edges, one per neighbour.
    /// Four pixels, inside the border: a grab zone that reaches into the text
    /// would make the first column unclickable.
    fn divider_zones(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let blocks = self.blocks();
        let Some(me) = self.active().filter(|b| !b.floating) else {
            return Vec::new();
        };
        let me = blocks.iter().find(|b| b.id == me.id).unwrap_or(&me);
        let [x, y, w, h] = me.rect;
        let close = 0.004;
        let mut zones: Vec<AnyElement> = Vec::new();
        for other in blocks.iter().filter(|b| !b.floating && b.id != me.id) {
            let [ox, oy, ow, oh] = other.rect;
            let overlaps_y = oy < y + h - close && oy + oh > y + close;
            let overlaps_x = ox < x + w - close && ox + ow > x + close;
            // (direction, the pane on the before side, the pane on the after side)
            let found: Option<(&'static str, &BlockState, &BlockState, f64)> =
                if overlaps_y && (ox + ow - x).abs() < close {
                    Some(("horizontal", other, me, 0.0))
                } else if overlaps_y && (ox - (x + w)).abs() < close {
                    Some(("horizontal", me, other, 1.0))
                } else if overlaps_x && (oy + oh - y).abs() < close {
                    Some(("vertical", other, me, 0.0))
                } else if overlaps_x && (oy - (y + h)).abs() < close {
                    Some(("vertical", me, other, 1.0))
                } else {
                    None
                };
            let Some((direction, before, after, side)) = found else {
                continue;
            };
            let (before_id, after_id) = (before.id.clone(), after.id.clone());
            let (before_rect, after_rect) = (before.rect, after.rect);
            let zone = div()
                .id(SharedString::from(format!(
                    "divider:{before_id}:{after_id}"
                )))
                .absolute();
            let zone = match (direction, side) {
                ("horizontal", 0.0) => zone
                    .left(relative(x as f32))
                    .top(relative(y as f32))
                    .w(px(4.0))
                    .h(relative(h as f32))
                    .cursor_ew_resize(),
                ("horizontal", _) => zone
                    .left(relative((x + w) as f32))
                    .ml(px(-4.0))
                    .top(relative(y as f32))
                    .w(px(4.0))
                    .h(relative(h as f32))
                    .cursor_ew_resize(),
                ("vertical", 0.0) => zone
                    .left(relative(x as f32))
                    .top(relative(y as f32))
                    .w(relative(w as f32))
                    .h(px(4.0))
                    .cursor_ns_resize(),
                _ => zone
                    .left(relative(x as f32))
                    .top(relative((y + h) as f32))
                    .mt(px(-4.0))
                    .w(relative(w as f32))
                    .h(px(4.0))
                    .cursor_ns_resize(),
            };
            zones.push(
                zone.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.divider = Some(Divider {
                            direction,
                            before: before_id.clone(),
                            after: after_id.clone(),
                            before_rect,
                            after_rect,
                        });
                        // The pane underneath must not take the press as a click.
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .into_any_element(),
            );
        }
        zones
    }

    /// The pointer let go: turn where it is into the ratio Rex wants.
    fn commit_divider(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(divider) = self.divider.take() else {
            return;
        };
        let Some(address) = self.address() else {
            return;
        };
        let Some(session) = self.session_id.clone() else {
            return;
        };
        let geometry = *self.area_geometry.lock().unwrap();
        let Some((origin, size)) = geometry else {
            return;
        };
        let width = f32::from(size.width).max(1.0);
        let height = f32::from(size.height).max(1.0);
        let fx = (f32::from(at.x) - f32::from(origin.x)) / width;
        let fy = (f32::from(at.y) - f32::from(origin.y)) / height;
        let [bx, by, bw, bh] = divider.before_rect;
        let [ax, ay, aw, ah] = divider.after_rect;
        let _ = (ax, ay);
        let ratio = match divider.direction {
            "horizontal" => ((fx as f64 - bx) / (bw + aw)).clamp(0.08, 0.92),
            _ => ((fy as f64 - by) / (bh + ah)).clamp(0.08, 0.92),
        };
        let (before, after) = (divider.before, divider.after);
        let direction = divider.direction;
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "direction": direction,
                "before_block_id": before,
                "after_block_id": after,
                "ratio": ratio,
            });
            if let Err(e) = engine.call(&address, "session.resize_pane", payload).await {
                eprintln!("flex: resize_pane: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        cx.notify();
    }

    /// Open the find bar on the pane that has the keyboard.
    fn start_find(&mut self, cx: &mut Context<Self>) {
        let Some(block) = self.active_block.clone() else {
            return;
        };
        if let Some(view) = self.terminals.get(&block) {
            view.update(cx, |terminal, cx| terminal.start_find(cx));
        }
        cx.notify();
    }

    /// Copy the focused pane's selection, or paste into it.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(block) = self.active_block.clone() else {
            return;
        };
        if let Some(view) = self.terminals.get(&block) {
            view.update(cx, |terminal, cx| terminal.copy(cx));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(block) = self.active_block.clone() else {
            return;
        };
        if let Some(view) = self.terminals.get(&block) {
            view.update(cx, |terminal, cx| terminal.paste(cx));
        }
    }

    fn new_tab(&mut self, cx: &mut Context<Self>) {
        let (Some(server_id), Some(session_id)) = (self.server_id.clone(), self.session_id.clone())
        else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |_, _| {
            if let Err(e) = engine
                .shrew()
                .new_window(&server_id, &session_id, None)
                .await
            {
                eprintln!("flex: new tab: {e}");
            }
        })
        .detach();
    }

    /// Float the focused pane over the tiled ones, or tile it again.
    fn toggle_float(&mut self, cx: &mut Context<Self>) {
        let (Some(address), Some(session_id), Some(block)) =
            (self.address(), self.session_id.clone(), self.active())
        else {
            return;
        };
        let engine = self.engine.clone();
        let server = self.server_id.clone().unwrap_or_default();
        let block_id = block.id.clone();
        let window_id = self.window_id.clone();
        let floating = block.floating;
        let Some(raw_window) = self.windows().into_iter().find(|w| Some(&w.id) == window_id.as_ref()) else { return; };
        let Some(key) = self.local_float_key(&block_id) else { return; };
        if self.local_floats.toggle(key, &raw_window, FLOAT_BOUNDS) {
            self.pull = None;
            self.settled = None;
            self.pending_focus = Some(block_id.clone());
            cx.notify();
            return;
        }
        // Never create a process or move tabs just to provide a retile anchor.
        let anchor = model::tiled_anchor(&raw_window, &block_id);
        if floating && anchor.is_none() {
            self.rpc_error("Restore floating pane", "This is a server floating pane with no tiled neighbour in this tab. Rex cannot retile it into an empty layer; no new pane was created.", cx);
            return;
        }
        if !floating {
            self.float_rects.remember(&server, &block_id, block.rect);
        }
        let hint = self.float_rects.get(&server, &block_id);

        cx.spawn(async move |this, cx| {
            if !floating {
                let payload = json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "bounds": FLOAT_BOUNDS,
                });
                if let Err(e) = engine.call(&address, "session.float_block", payload).await {
                    this.update(cx, |me, cx| me.rpc_error("session.float_block", e, cx))
                        .ok();
                    return;
                }
            } else {
                let Some(anchor) = anchor else { return };
                // Put it back on the side it came from, with roughly the share it
                // had. Rex only takes a side and a ratio, not a saved layout.
                let (side, direction, ratio) =
                    hint.map(split_hint).unwrap_or(("after", "horizontal", 0.5));
                let payload = json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "anchor_block_id": anchor,
                    "side": side,
                    "direction": direction,
                    "ratio": ratio,
                });
                if let Err(e) = engine.call(&address, "session.move_block", payload).await {
                    this.update(cx, |me, cx| me.rpc_error("session.move_block", e, cx))
                        .ok();
                    return;
                }
            }
            this.update(cx, |me, cx| {
                if floating {
                    me.float_rects.finish_tiling(&server, &block_id, true);
                }
                if me.server_id.as_ref() == Some(&server)
                    && me.session_id.as_ref() == Some(&session_id)
                    && me.window_id == window_id
                    && me.active_block.as_ref() == Some(&block_id)
                {
                    me.publish_focus(&block_id);
                    me.pending_focus = Some(block_id.clone());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a tool beside the active pane: lazygit, or an fzf picker whose choice
    /// is typed back into the pane it came from.
    fn run_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        if self.pending_tool.is_some() {
            return;
        }
        let (Some(address), Some(server), Some(session), Some(window), Some(origin)) = (
            self.address(),
            self.server_id.clone(),
            self.session_id.clone(),
            self.window_id.clone(),
            self.active(),
        ) else {
            return;
        };
        let (command, suffix) = match tool {
            Tool::Lazygit => (vec!["lazygit".to_owned()], None),
            Tool::Files => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_FILES.to_owned()],
                Some(" "),
            ),
            Tool::Dirs => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_DIRS.to_owned()],
                Some("/"),
            ),
            Tool::Anything => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_ANY.to_owned()],
                Some(" "),
            ),
        };
        let request = PendingTool {
            request: Default::default(),
            server,
            session,
            window,
            origin: origin.id,
            target: None,
            suffix,
            observed: false,
            confirmed_absent: false,
            checking: false,
        };
        self.pending_tool = Some(request.clone());
        self.pending_focus = None;
        cx.notify();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            // Ask for the foreground process directory when OSC 7 is unavailable.
            let mut options = json!({"shell": "none", "command": command});
            if let Some(cwd) = engine
                .block_cwd(&address, &request.session, &request.origin)
                .await
            {
                options["cwd"] = json!(cwd);
            }
            let current = this
                .update(cx, |me, _| {
                    request.matches_context((
                        me.server_id.as_deref(),
                        me.session_id.as_deref(),
                        me.window_id.as_deref(),
                    )) && me.active_block.as_deref() == Some(request.origin.as_str())
                        && me
                            .pending_tool
                            .as_ref()
                            .is_some_and(|pending| Arc::ptr_eq(&pending.request, &request.request))
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let payload = tool_layer_request(&request.session, &request.window, options);
            let result = engine.call(&address, "session.new_layer", payload).await;
            this.update(cx, |me, cx| {
                // A reply from a tool opened in another tab/server must not
                // change this context's active pane or keyboard owner.
                let current = request.matches_context((
                    me.server_id.as_deref(),
                    me.session_id.as_deref(),
                    me.window_id.as_deref(),
                )) && me
                    .pending_tool
                    .as_ref()
                    .is_some_and(|pending| Arc::ptr_eq(&pending.request, &request.request));
                let block = match result {
                    Ok(view) => view
                        .pointer("/block_ids/0")
                        .and_then(|id| id.as_str())
                        .map(str::to_owned),
                    Err(e) => {
                        me.rpc_error("session.new_layer", e, cx);
                        None
                    }
                };
                if !current {
                    return;
                }
                if let Some(block) = block {
                    if let Some(pending) = &mut me.pending_tool {
                        pending.target = Some(block);
                    }
                    me.tinted = Some(request.origin);
                    me.reconcile_tool(cx);
                } else {
                    me.pending_tool = None;
                    me.pending_focus = me.active_block.clone();
                    me.drop_tint(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reconcile both reload and render against the same scoped tool lifecycle.
    /// Returns true while no target view exists, so the workspace holds keys.
    fn reconcile_tool(&mut self, cx: &mut Context<Self>) -> bool {
        let blocks = self.blocks();
        let visible: Vec<_> = blocks
            .iter()
            .map(|block| (block.id.as_str(), block.exited.is_some()))
            .collect();
        let Some(tool) = &mut self.pending_tool else {
            return false;
        };
        let progress = tool.reconcile(
            (
                self.server_id.as_deref(),
                self.session_id.as_deref(),
                self.window_id.as_deref(),
            ),
            &visible,
            &mut self.active_block,
            &mut self.pending_focus,
        );
        match progress {
            ToolProgress::Waiting => {
                if tool.target.is_some() && !tool.checking {
                    tool.checking = true;
                    let request = tool.clone();
                    let engine = self.engine.clone();
                    cx.spawn(async move |this, cx| {
                        // A view fetched after creation distinguishes "not delivered
                        // yet" from a short-lived tool that has already disappeared.
                        let result = engine
                            .call_server(
                                &request.server,
                                "session.view",
                                json!({
                                    "session_id": request.session,
                                }),
                            )
                            .await;
                        this.update(cx, |me, cx| {
                            if !request.matches_context((
                                me.server_id.as_deref(),
                                me.session_id.as_deref(),
                                me.window_id.as_deref(),
                            )) {
                                return;
                            }
                            let Some(pending) = &mut me.pending_tool else {
                                return;
                            };
                            if !Arc::ptr_eq(&pending.request, &request.request) {
                                return;
                            }
                            pending.checking = false;
                            match result {
                                Ok(view) => {
                                    let present = view
                                        .get("windows")
                                        .and_then(|v| v.as_array())
                                        .into_iter()
                                        .flatten()
                                        .filter_map(|w| w.get("layers").and_then(|v| v.as_array()))
                                        .flatten()
                                        .filter_map(|l| l.get("blocks").and_then(|v| v.as_array()))
                                        .flatten()
                                        .any(|b| {
                                            b.get("block_id").and_then(|v| v.as_str())
                                                == request.target.as_deref()
                                        });
                                    if !present {
                                        pending.confirmed_absent = true;
                                        me.reconcile_tool(cx);
                                        cx.notify();
                                    }
                                }
                                Err(e) => {
                                    me.pending_tool = None;
                                    me.pending_focus = me.active_block.clone();
                                    me.tinted = None;
                                    me.rpc_error("Check tool", e, cx);
                                }
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
                return true;
            }
            ToolProgress::Running => return false,
            ToolProgress::Cancelled => {
                self.pending_tool = None;
                self.tinted = None;
                return false;
            }
            ToolProgress::Finished => {}
        }
        let tool = self.pending_tool.take().unwrap();
        self.tinted = None;
        let target = tool.target.as_deref().unwrap();
        // The helper has already checked server/session/window. Also require
        // the origin still to exist before typing a choice or restoring focus.
        if visible.iter().any(|(id, _)| *id == tool.origin) {
            if let Some(suffix) = tool.suffix {
                let text = self
                    .terminals
                    .get(target)
                    .map(|view| view.read(cx).text())
                    .unwrap_or_default();
                let choice = text
                    .lines()
                    .rev()
                    .find_map(|line| {
                        line.split_once(PICK_MARK)
                            .map(|(_, path)| path.trim().to_owned())
                    })
                    .filter(|path| !path.is_empty());
                if let Some(path) = choice {
                    if let Some(origin) = self.terminals.get(&tool.origin) {
                        origin.read(cx).send(&format!("{path}{suffix}"));
                    }
                }
            }
            if self.active_block.as_deref() == Some(tool.origin.as_str()) {
                self.publish_focus(&tool.origin);
            }
        }
        if blocks.iter().any(|block| block.id == target) {
            let engine = self.engine.clone();
            cx.spawn(async move |this, cx| {
                if let Err(e) = engine
                    .shrew()
                    .close_block(&tool.server, &tool.session, tool.target.as_deref().unwrap())
                    .await
                {
                    this.update(cx, |me, cx| me.rpc_error("Close tool", e, cx))
                        .ok();
                }
            })
            .detach();
        }
        false
    }

    /// Tell the server which pane this client is on, so focus changes and splits
    /// start from the right place.
    fn publish_focus(&self, block_id: &str) {
        let (Some(address), Some(session), block) =
            (self.address(), self.session_id.clone(), block_id.to_owned())
        else {
            return;
        };
        let engine = self.engine.clone();
        // Fire and forget on the engine's own runtime; the click must not wait.
        std::mem::drop(engine.shrew().runtime().clone().spawn(async move {
            let payload = json!({"session_id": session, "block_id": block});
            if let Err(e) = engine.call(&address, "session.focus_block", payload).await {
                eprintln!("flex: focus_block: {e}");
            }
        }));
    }

    /// Ask for a picture, then show it behind every terminal.
    fn choose_background(&mut self, cx: &mut Context<Self>) {
        let options = gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a background picture".into()),
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            // `prompt_for_paths` lives on App, not on the async context.
            let Ok(receiver) = cx.update(|cx| cx.prompt_for_paths(options)) else {
                return;
            };
            let picked = receiver.await;
            let path = match picked {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let Some(path) = path else { return };
            let text = path.display().to_string();
            let picture = crate::background::load(&text);
            if picture.is_none() {
                eprintln!("flex: could not read {text} as an image");
                return;
            }
            this.update(cx, |me, cx| {
                me.picture = picture;
                me.settings.background = Some(text);
                me.settings.save(&engine);
                me.apply_background(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_background(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        self.picture = path.as_deref().and_then(crate::background::load);
        self.settings.background = path;
        self.settings.save(&self.engine);
        self.apply_background(cx);
        cx.notify();
    }

    fn set_background_opacity(&mut self, value: f32, cx: &mut Context<Self>) {
        self.settings.background_opacity = value.clamp(0.05, 0.95);
        self.settings.save(&self.engine);
        self.apply_background(cx);
        cx.notify();
    }

    fn set_scrollback(&mut self, lines: u64, cx: &mut Context<Self>) {
        self.settings.scrollback_lines =
            lines.clamp(crate::settings::SCROLL_MIN, crate::settings::SCROLL_MAX);
        self.settings.save(&self.engine);
        let value = self.settings.scrollback_lines;
        for view in self.terminals.values() {
            view.update(cx, |terminal, cx| terminal.set_scrollback(value, cx));
        }
        cx.notify();
    }

    fn apply_background(&mut self, cx: &mut Context<Self>) {
        let over_picture = self.picture.is_some();
        let opacity = self.settings.background_opacity;
        for view in self.terminals.values() {
            view.update(cx, |terminal, cx| {
                terminal.set_background(over_picture, opacity, cx)
            });
        }
    }

    /// True when this press completes a double-click: same spot, twice, quickly.
    fn is_double_press(&mut self, at: gpui::Point<gpui::Pixels>) -> bool {
        const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);
        const SLOP: f32 = 8.0;
        let now = std::time::Instant::now();
        let doubled = self.title_press.is_some_and(|(when, then)| {
            now.duration_since(when) < DOUBLE_CLICK
                && (f32::from(at.x) - f32::from(then.x)).abs() < SLOP
                && (f32::from(at.y) - f32::from(then.y)).abs() < SLOP
        });
        // Reset after a double so a third click starts a new pair.
        self.title_press = if doubled { None } else { Some((now, at)) };
        doubled
    }

    fn drop_tint(&mut self, cx: &mut Context<Self>) {
        self.tinted = None;
        cx.notify();
    }

    fn chrome(&self) -> theme::Chrome {
        theme::Chrome {
            glass: self.settings.acrylic,
        }
    }

    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.settings.font_size = size.clamp(FONT_MIN, FONT_MAX);
        self.settings.save(&self.engine);
        for view in self.terminals.values() {
            let size = self.settings.font_size;
            view.update(cx, |terminal, cx| terminal.set_font_size(size, cx));
        }
        cx.notify();
    }

    // ---- floating panes ----------------------------------------------------

    fn begin_pull(
        &mut self,
        block: &BlockState,
        grab: Grab,
        start: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.active_block = Some(block.id.clone());
        self.pull = Some(Pull {
            block_id: block.id.clone(),
            local_key: self.local_float_key(&block.id).filter(|key| self.local_floats.0.contains_key(key)),
            grab,
            start,
            rect: block.rect,
            live: block.rect,
        });
        cx.notify();
    }

    /// Follow the pointer. Only the local copy moves; the server is told once, on
    /// release, so a drag costs no round trips.
    fn drag_to(&mut self, position: Point<Pixels>, area: Size<Pixels>, cx: &mut Context<Self>) {
        let Some(pull) = &mut self.pull else { return };
        let area_w = f64::from(area.width).max(1.0);
        let area_h = f64::from(area.height).max(1.0);
        let dx = f64::from(position.x - pull.start.x) / area_w;
        let dy = f64::from(position.y - pull.start.y) / area_h;
        let [x, y, w, h] = pull.rect;
        pull.live = match pull.grab {
            Grab::Move => [
                (x + dx).clamp(0.0, (1.0 - w).max(0.0)),
                (y + dy).clamp(0.0, (1.0 - h).max(0.0)),
                w,
                h,
            ],
            Grab::Resize => [
                x,
                y,
                (w + dx).clamp(0.2, 1.0 - x),
                (h + dy).clamp(0.2, 1.0 - y),
            ],
        };
        cx.notify();
    }

    fn end_pull(&mut self, cx: &mut Context<Self>) {
        let Some(pull) = self.pull.take() else { return };
        if pull.live == pull.rect {
            cx.notify();
            return;
        }
        if let Some(key) = &pull.local_key {
            if let Some(local) = self.local_floats.0.get_mut(key) {
                local.rect = pull.live;
            }
            cx.notify();
            return;
        }
        self.settled = Some((pull.block_id.clone(), pull.live));
        let (Some(address), Some(session_id)) = (self.address(), self.session_id.clone()) else {
            cx.notify();
            return;
        };
        let engine = self.engine.clone();
        let block_id = pull.block_id;
        let bounds = pull.live;
        cx.spawn(async move |this, cx| {
            // Rex moves a *layer*, and a layer id is not part of this shell's
            // state, so resolve it from the session view in the same trip.
            let view = match engine
                .call(&address, "session.view", json!({"session_id": session_id}))
                .await
            {
                Ok(view) => view,
                Err(e) => {
                    eprintln!("flex: session.view: {e}");
                    this.update(cx, |me, cx| {
                        me.settled = None;
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Some(layer_id) = floating_layer_id(&view, &block_id) else {
                this.update(cx, |me, cx| {
                    me.settled = None;
                    cx.notify();
                })
                .ok();
                return;
            };
            let payload = json!({
                "session_id": session_id,
                "layer_id": layer_id,
                "bounds": bounds,
            });
            if let Err(e) = engine.call(&address, "session.resize_layer", payload).await {
                eprintln!("flex: session.resize_layer: {e}");
                this.update(cx, |me, cx| {
                    me.settled = None;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// The rect to draw a block at: our own copy while it is being pulled or has
    /// just been dropped, the server's otherwise.
    fn rect_of(&self, block: &BlockState) -> [f64; 4] {
        if let Some(pull) = &self.pull {
            if pull.block_id == block.id {
                return pull.live;
            }
        }
        if let Some((block_id, rect)) = &self.settled {
            if block_id == &block.id {
                return *rect;
            }
        }
        block.rect
    }

    // ---- chrome ------------------------------------------------------------

    /// A window of ours, since GPUI draws no system title bar on Windows.
    /// Everything but the buttons drags the window.
    fn title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let maximized = crate::win32::is_maximized();
        // A press drags the window, except the second press of a double-click,
        // which maximizes or restores it — what people expect of a title bar.
        let chips = self.tab_chips(cx);
        let chrome = self.chrome();

        div()
            .flex()
            .items_center()
            .h(theme::TITLE_H)
            .pl_1()
            .bg(chrome.surface())
            .border_b_1()
            .border_color(theme::border())
            .child(
                icons::button("toggle-sidebar", Icon::Sidebar, self.settings.sidebar).on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::ToggleSidebar, window, cx)),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    .pl_1()
                    .pr_2()
                    .gap_2()
                    .child(div().size(px(8.0)).rounded_sm().bg(theme::accent()))
                    .child(div().text_color(theme::muted()).child("flex"))
                    .on_mouse_down(MouseButton::Left, cx.listener(title_press)),
            )
            .child(div().w(px(1.0)).h(px(16.0)).mx_2().bg(theme::border()))
            .children(chips)
            .child(
                icons::button("new-tab", Icon::Plus, false).on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::NewTab, window, cx)),
                ),
            )
            .child(div().w(px(6.0)))
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .on_mouse_down(MouseButton::Left, cx.listener(title_press)),
            )
            .child(
                icons::button("settings", Icon::Sliders, self.show_settings).on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::Settings, window, cx)),
                ),
            )
            .child(control("min", Shape::Minimize, false, |_, _| {
                crate::win32::minimize()
            }))
            .child(control(
                "max",
                if maximized {
                    Shape::Restore
                } else {
                    Shape::Maximize
                },
                false,
                |_, _| crate::win32::toggle_maximize(),
            ))
            .child(control("close", Shape::Close, true, |window, _| {
                window.remove_window()
            }))
            .child(div().w(px(4.0)).h_full())
            .into_any_element()
    }

    /// A one-line prompt for a name, over everything else.
    fn prompt_overlay(&self) -> AnyElement {
        let Some(prompt) = &self.prompt else {
            return div().into_any_element();
        };
        let chrome = self.chrome();
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(hsla(0.0, 0.0, 0.0, 0.5))
            .occlude()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .map(|mut el| {
                el.interactivity().on_any_mouse_up(|_, _, cx| cx.stop_propagation());
                el
            })
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(620.0))
                    .rounded_lg()
                    .bg(theme::color(theme::SURFACE))
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_color(theme::text()).child(prompt.label.clone()))
                    .when_some(
                        match &prompt.what {
                            Ask::CreateBlock { schema, .. } | Ask::RunAction { schema, .. } => {
                                Some(schema.to_string())
                            }
                            _ => None,
                        },
                        |el, schema| {
                            el.child(
                                div()
                                    .max_h(px(120.0))
                                    .overflow_hidden()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(format!("Schema (validated by server): {schema}")),
                            )
                        },
                    )
                    .when(prompt.what.wants_text(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .max_h(px(240.0))
                                .overflow_hidden()
                                .bg(chrome.active())
                                .text_color(theme::text())
                                .child(format!("{}▌", prompt.value)),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(prompt.what.hint()),
                    ),
            )
            .into_any_element()
    }

    /// The jump list, over everything else.
    fn palette_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(palette) = &self.palette else {
            return div().into_any_element();
        };
        let chrome = self.chrome();
        let items = self.palette_items();
        let rows: Vec<AnyElement> = items
            .iter()
            .take(12)
            .enumerate()
            .map(|(index, (label, jump))| {
                let selected = index == palette.cursor;
                let jump = jump.clone();
                div()
                    .id(SharedString::from(format!("palette-{index}")))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .when(selected, |el| {
                        el.bg(chrome.active()).text_color(theme::text())
                    })
                    .when(!selected, |el| el.text_color(theme::muted()))
                    .child(div().flex_1().truncate().child(label.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.palette = None;
                            this.take_jump(jump.clone(), window, cx);
                            cx.notify();
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        let more = items.len().saturating_sub(12);
        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(hsla(0.0, 0.0, 0.0, 0.5))
            .occlude()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .map(|mut el| {
                el.interactivity().on_any_mouse_up(|_, _, cx| cx.stop_propagation());
                el
            })
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(520.0))
                    .rounded_lg()
                    .bg(theme::color(theme::SURFACE))
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(chrome.active())
                            .text_color(theme::text())
                            .child(format!("{}▌", palette.query)),
                    )
                    .children(rows)
                    .when(more > 0, |el| {
                        el.child(
                            div()
                                .px_3()
                                .pt_1()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(format!("{more} more — keep typing")),
                        )
                    })
                    .child(
                        div()
                            .px_3()
                            .pt_1()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(
                            "↑↓ to pick · Enter to go · > actions (no arguments) · Esc to close",
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Notifications as cards in the top right corner: a bell, a program asking
    /// for attention, an agent that stopped for a decision. Clicking one jumps
    /// to the pane it came from.
    fn toast_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.toasts.is_empty() {
            return div().into_any_element();
        }
        let chrome = self.chrome();
        let cards: Vec<AnyElement> = self
            .toasts
            .iter()
            .map(|toast| {
                let colour = match toast.kind {
                    shrew_core::NotificationKind::AgentBlocked => theme::danger(),
                    shrew_core::NotificationKind::AgentError => {
                        theme::agent(shrew_core::AgentState::Error)
                    }
                    shrew_core::NotificationKind::AgentDone => {
                        theme::agent(shrew_core::AgentState::Done)
                    }
                    shrew_core::NotificationKind::Program => theme::accent(),
                    shrew_core::NotificationKind::Bell => theme::accent(),
                    shrew_core::NotificationKind::ProcessExited => theme::muted(),
                };
                let (server, session, block) = (
                    toast.server_id.clone(),
                    toast.session_id.clone(),
                    toast.block_id.clone(),
                );
                div()
                    .id(SharedString::from(format!("toast-{block}")))
                    .w(px(300.0))
                    .rounded_md()
                    .bg(chrome.surface())
                    .border_1()
                    .border_color(colour)
                    .shadow_lg()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(theme::dot(colour))
                            .child(div().flex_1().truncate().child(toast.title.clone())),
                    )
                    .when(!toast.body.is_empty(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(toast.body.clone()),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.jump_to_block(&server, &session, &block, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .absolute()
            .top(theme::TITLE_H + px(10.0))
            .right(px(12.0))
            .flex_col()
            .gap_2()
            .children(cards)
            .into_any_element()
    }

    fn describe_block(
        &mut self,
        server: String,
        session: String,
        block: String,
        cx: &mut Context<Self>,
    ) {
        let Some(address) = self.state.server(&server).map(|s| s.address.clone()) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .call(
                    &address,
                    "session.describe_block",
                    json!({"session_id":session,"block_id":block}),
                )
                .await;
            this.update(cx, |me, cx| {
                match result {
                    Ok(value) => {
                        me.block_details.insert((server, session, block), value);
                    }
                    Err(e) => me.rpc_error("session.describe_block", e, cx),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let chrome = self.chrome();
        let mut rows: Vec<AnyElement> = Vec::new();
        rows.push(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .pt_3()
                .pb_1()
                .child(div().text_xs().text_color(theme::muted()).child("SERVERS"))
                .child(
                    div()
                        .id("server-add")
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .hover(|el| el.bg(chrome.hover()))
                        .child("+")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, event: &gpui::ClickEvent, window, cx| {
                            this.open_add_menu(window, cx);
                            if let Some(menu) = &mut this.menu {
                                menu.anchor = Some(event.position());
                            }
                            cx.stop_propagation();
                        })),
                )
                .into_any_element(),
        );
        for server in &self.state.servers {
            let target = menu::Target {
                server: server.id.clone(),
                address: server.address.clone(),
                session: None,
                block: None,
            };
            let selected = self.server_id.as_ref() == Some(&server.id);
            let dot = match server.connectivity {
                Connectivity::Online => theme::accent(),
                Connectivity::Connecting => theme::muted(),
                Connectivity::Offline { .. } => theme::danger(),
                Connectivity::Idle => theme::border(),
            };
            let id = server.id.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!("server:{id}")))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .when(selected, |el| el.bg(chrome.active()))
                    .hover(|el| el.bg(chrome.hover()))
                    .child(theme::dot(dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .child(server.name.clone()),
                    )
                    .child(self.menu_button(format!("server-menu:{id}"), target, cx))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if this.server_id.as_ref() != Some(&id) {
                                this.server_id = Some(id.clone());
                                this.session_id = None;
                                this.window_id = None;
                                this.active_block = None;
                                this.reload(cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element(),
            );
            for session in &server.sessions {
                let target = menu::Target {
                    server: server.id.clone(),
                    address: server.address.clone(),
                    session: Some(session.id.clone()),
                    block: None,
                };
                let active = selected && self.session_id.as_ref() == Some(&session.id);
                let (server_id, id) = (server.id.clone(), session.id.clone());
                rows.push(
                    div()
                        .id(SharedString::from(format!("session:{server_id}:{id}")))
                        .ml_4()
                        .mr_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w(px(0.0))
                        .when(active, |el| el.bg(chrome.active()))
                        .hover(|el| el.bg(chrome.hover()))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .child(session.label.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(session.block_count().to_string()),
                        )
                        .child(self.menu_button(
                            format!("session-menu:{server_id}:{id}"),
                            target,
                            cx,
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.server_id = Some(server_id.clone());
                                this.session_id = Some(id.clone());
                                this.window_id = None;
                                this.active_block = None;
                                this.reload(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .into_any_element(),
                );
            }
        }
        // Attention stays visible below server/session navigation.
        let agents = self.state.attention();
        if !agents.is_empty() {
            rows.push(theme::section("AGENTS").into_any_element());
            for item in agents {
                let agent = item.block.agent.clone();
                let Some(agent) = agent else { continue };
                let (server, session, window, block) = (
                    item.server_id.clone(),
                    item.session_id.clone(),
                    item.window_id.clone(),
                    item.block.id.clone(),
                );
                let name = if agent.app.is_empty() {
                    item.block.display_name()
                } else {
                    agent.app.clone()
                };
                let state_label = match agent.state {
                    shrew_core::AgentState::Idle => "idle",
                    shrew_core::AgentState::Working => "working",
                    shrew_core::AgentState::Blocked => "blocked",
                    shrew_core::AgentState::Error => "failed",
                    shrew_core::AgentState::Done => "done",
                };
                let detail = agent
                    .waiting_for
                    .clone()
                    .unwrap_or_else(|| item.session_label.clone());
                rows.push(
                    div()
                        .id(SharedString::from(format!("agent:{block}")))
                        .mx_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme::text())
                        .hover(|el| el.bg(chrome.hover()))
                        .child(theme::dot(theme::agent(agent.state)))
                        // The name keeps its width; the detail gives way, so an
                        // agent is never shown as "claud".
                        .child(div().flex_none().child(name))
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(format!("{state_label} · {detail}")),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.navigate(
                                    server.clone(),
                                    session.clone(),
                                    window.clone(),
                                    Some(block.clone()),
                                    cx,
                                );
                            }),
                        )
                        .into_any_element(),
                );
            }
        }

        let parked: Vec<_> = self
            .parked
            .iter()
            .filter(|p| {
                self.server_id.as_ref() == Some(&p.server)
                    && self.session_id.as_ref() == Some(&p.session)
            })
            .cloned()
            .collect();
        if !parked.is_empty() {
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(theme::section("Parked panes"))
                    .child(
                        div()
                            .id("refresh-parked")
                            .min_w(px(24.0))
                            .h(px(24.0))
                            .mr_2()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_parked(cx))),
                    )
                    .into_any_element(),
            );
            for parked in parked {
                let id = parked.id.clone();
                let label = parked.label.clone();
                let flat = parked.clone();
                rows.push(
                    div()
                        .id(SharedString::from(format!("parked:{id}")))
                        .mx_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_color(theme::text())
                        .hover(|el| el.bg(chrome.hover()))
                        .child(div().flex_1().truncate().child(label))
                        .child(div().text_xs().text_color(theme::muted()).child("parked"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| this.unpark(flat.clone(), cx)),
                        )
                        .into_any_element(),
                );
            }
        }

        let tucked: Vec<AnyElement> = self
            .blocks()
            .into_iter()
            .filter(|block| block.layer_state == "minimized")
            .map(|block| {
                let id = block.id.clone();
                div()
                    .id(SharedString::from(format!("tucked:{id}")))
                    .mx_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_color(theme::text())
                    .hover(|el| el.bg(chrome.hover()))
                    .child(div().flex_1().truncate().child(block.display_name()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("minimized"),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.active_block = Some(id.clone());
                            this.toggle_minimized(cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        if !tucked.is_empty() {
            rows.push(theme::section("Minimized panes").into_any_element());
            rows.extend(tucked);
        }

        div()
            .w(theme::SIDEBAR_W)
            .flex_none()
            .min_w(px(0.0))
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(chrome.surface())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    fn tab_chips(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.window_id.clone();
        let chrome = self.chrome();
        let settings_open = self.show_settings;
        let mut chips: Vec<AnyElement> = self
            .windows()
            .into_iter()
            .map(|window| {
                let id = window.id.clone();
                let active = !settings_open && selected.as_deref() == Some(id.as_str());
                // What the programs in this tab are doing: a shell leaves
                // `Window 1495d30d` behind, an editor or a build does not.
                let head = window
                    .focused_block_id
                    .as_ref()
                    .and_then(|id| window.blocks.iter().find(|b| &b.id == id))
                    .or_else(|| window.blocks.first());
                let shell = ["zsh", "bash", "sh", "fish", "nu", "pwsh", "powershell"];
                let name = match head {
                    Some(block) if window.label.starts_with("Window ") => {
                        // Shells title themselves `user@host:~/dir`, which as a
                        // tab name is noise; the directory's own name is not.
                        let shown = block.display_name();
                        if shown.contains('/') || shown.contains('@') || shown.contains('\\') {
                            shown
                                .rsplit('/')
                                .next()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&shown)
                                .to_owned()
                        } else {
                            shown
                        }
                    }
                    _ => window.label.clone(),
                };
                let process = head
                    .map(|block| block.process.clone())
                    .filter(|p| !p.is_empty() && !shell.contains(&p.as_str()) && *p != name)
                    .unwrap_or_default();
                let progress = head.and_then(|block| block.progress);
                let bell = head.is_some_and(|block| block.has_bell);
                let mut tint = theme::accent();
                tint.a = 0.16;
                div()
                    .id(SharedString::from(id.clone()))
                    .relative()
                    .h(px(22.0))
                    .px_3()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .when(active, |el| {
                        el.bg(chrome.active()).text_color(theme::text())
                    })
                    .when(!active && bell, |el| {
                        el.bg(tint).text_color(theme::accent())
                    })
                    .when(!active && !bell, |el| {
                        el.text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                    })
                    .when_some(window_agent(&window), |el, state| {
                        el.child(theme::dot(theme::agent(state)))
                    })
                    .child(
                        // A shell titles itself with whatever is running, which
                        // can be a whole command line; the strip still has to fit.
                        div().max_w(px(220.0)).truncate().child(name),
                    )
                    .when(!process.is_empty(), |el| {
                        el.child(
                            div()
                                .text_color(theme::muted())
                                .child(format!("· {process}")),
                        )
                    })
                    .when_some(progress, |el, percent| {
                        let mut fill = theme::accent();
                        fill.a = 0.85;
                        el.child(
                            div()
                                .absolute()
                                .left(px(4.0))
                                .right(px(4.0))
                                .bottom(px(1.0))
                                .h(px(2.0))
                                .rounded_full()
                                .bg(theme::border())
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(percent as f32 / 100.0))
                                        .rounded_full()
                                        .bg(fill),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("close-{id}")))
                            .ml_2()
                            .size(px(13.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_size(px(11.0))
                            .text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                            .child("×")
                            // On press, not on click: choosing a tab rebuilds the
                            // strip, and a click that spans the rebuild is lost.
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        this.close_window(id.clone(), cx);
                                        cx.notify();
                                        cx.stop_propagation();
                                    }
                                }),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.window_id = Some(id.clone());
                            this.active_block = None;
                            this.focus_window(&id, cx);
                            this.close_settings(window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();
        if settings_open {
            chips.push(
                div()
                    .id("tab-settings")
                    .h(px(22.0))
                    .px_3()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .text_xs()
                    .bg(chrome.active())
                    .text_color(theme::text())
                    .child("Settings")
                    .child(
                        div()
                            .id("close-settings")
                            .ml_2()
                            .size(px(13.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .text_size(px(11.0))
                            .text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                            .child("×")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    )
                    .into_any_element(),
            );
        }
        chips
    }

    fn panes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(session_id) = self.session_id.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::muted())
                .child("Waiting for a session")
                .into_any_element();
        };
        let Some(server_id) = self.server_id.clone() else {
            return div().flex_1().into_any_element();
        };

        // A zoomed block fills its window. For a tiled pane the server already
        // reports it that way; a floating one keeps its layer bounds, so the
        // shell has to enlarge it itself.
        let zoomed = self
            .windows()
            .into_iter()
            .find(|window| Some(&window.id) == self.window_id.as_ref())
            .and_then(|window| window.zoomed_block_id);

        // The amber ring only earns its keep when there is a choice to show: with
        // one pane, or a zoomed one filling the window, it is just a frame.
        let ring = self.blocks().len() > 1 && zoomed.is_none();

        let tool_waiting = self.reconcile_tool(cx);
        let all_blocks = self.blocks();
        let blocks: Vec<_> = all_blocks
            .iter()
            .filter(|b| pane_visible(b, zoomed.as_deref()))
            .cloned()
            .collect();
        let visible = visible_focus(&blocks, self.active_block.as_deref(), None);
        if !tool_waiting && self.active_block != visible {
            self.active_block = visible.clone();
            if self.pending_focus.is_none() {
                self.pending_focus = visible;
            }
        }
        if self.pending_focus.as_ref().is_some_and(|id| {
            all_blocks
                .iter()
                .any(|b| b.id == *id && !pane_visible(b, zoomed.as_deref()))
        }) {
            self.pending_focus = self.active_block.clone();
        }
        let views: Vec<(BlockState, Option<Entity<TerminalView>>)> = blocks
            .into_iter()
            .map(|block| {
                let view = block
                    .is_terminal()
                    .then(|| self.terminal(&server_id, &session_id, &block.id, cx));
                (block, view)
            })
            .collect();

        // Keep *something* focused. Closing a pane takes its focus handle with
        // it, and GPUI dispatches keys along the focused element's path: with
        // nothing focused even the shell's own shortcuts stop arriving. So a
        // freshly split pane takes focus, and if focus has gone missing it goes
        // to the active pane, or the first one left.
        // An open prompt or jump list owns the keyboard. Focusing a pane here
        // would take it back mid-typing, and the text would land in the shell
        // as well as in the field.
        let held = self.input_overlay_open();
        if held || tool_waiting {
            window.focus(&self.focus);
        }
        if !held && views.is_empty() {
            window.focus(&self.focus);
        }
        let focused_somewhere = views.iter().any(|(_, view)| {
            view.as_ref()
                .is_some_and(|view| view.read(cx).handle().is_focused(window))
        }) || (self.focus.is_focused(window)
            && views
                .iter()
                .any(|(b, view)| view.is_none() && self.active_block.as_ref() == Some(&b.id)));
        let wanted = if held || tool_waiting {
            None
        } else {
            self.pending_focus.take().or_else(|| {
                if focused_somewhere {
                    None
                } else {
                    self.active_block
                        .clone()
                        .or_else(|| views.first().map(|(block, _)| block.id.clone()))
                }
            })
        };
        if let Some(wanted) = wanted {
            if let Some((_, view)) = views.iter().find(|(block, _)| block.id == wanted) {
                if let Some(view) = view {
                    window.focus(&view.read(cx).handle());
                } else {
                    window.focus(&self.focus);
                }
            } else {
                self.pending_focus = Some(wanted);
            }
        }

        // The pane with the keyboard owns its terminal's size, so zooming it or
        // opening it from a shortcut reflows the remote side to the new box.
        for (_, view) in &views {
            let Some(view) = view else { continue };
            let view = view.read(cx);
            if view.handle().is_focused(window) {
                view.take_control();
            }
        }

        let active = self.active_block.clone();
        let chrome = self.chrome();
        let mut elements: Vec<AnyElement> = Vec::with_capacity(views.len());
        // Floating layers paint last, so they sit above the tiled panes.
        for floating_pass in [false, true] {
            for (block, view) in views.iter().filter(|(b, _)| b.floating == floating_pass) {
                // A zoomed block is the only one the window shows.
                let [x, y, w, h] = pane_geometry(block, zoomed.as_deref(), self.rect_of(block));
                let is_active = active.as_deref() == Some(block.id.as_str());
                let outline = if is_active && ring {
                    theme::accent()
                } else {
                    theme::border()
                };
                let block_id = block.id.clone();
                let floating = block.floating;
                let activate_id = block_id.clone();
                let unsupported = view.is_none();
                let content = if let Some(view) = view {
                    div().size_full().child(view.clone())
                } else {
                    let server = server_id.clone();
                    let session = session_id.clone();
                    let id = block.id.clone();
                    let details = self
                        .block_details
                        .get(&(server.clone(), session.clone(), id.clone()))
                        .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                        .unwrap_or_default();
                    div()
                        .size_full()
                        .p_4()
                        .bg(chrome.surface())
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(div().text_color(theme::text()).child(block.display_name()))
                        .child(div().text_xs().text_color(theme::muted()).child(format!(
                            "Creator: {} — no renderer installed",
                            block.creator_name
                        )))
                        .child(
                            div()
                                .id(SharedString::from(format!("describe:{}", block.id)))
                                .text_xs()
                                .child("Describe block")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.describe_block(
                                        server.clone(),
                                        session.clone(),
                                        id.clone(),
                                        cx,
                                    )
                                })),
                        )
                        .child(div().text_xs().text_color(theme::muted()).child(details))
                };

                let body = if floating {
                    // A floating pane carries its own little title bar: that is
                    // where it is grabbed, and the corner is where it resizes.
                    let drag_block = block.clone();
                    let resize_block = block.clone();
                    div()
                        .flex()
                        .flex_col()
                        .size_full()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .flex_none()
                                .h(px(24.0))
                                .px_2()
                                .bg(chrome.surface())
                                .border_b_1()
                                .border_color(theme::border())
                                .child(icons::icon(Icon::Float, px(11.0), theme::muted()))
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(if block.layer_kind == "local" { format!("{} · local", block.display_name()) } else { block.display_name() }),
                                )
                                .when(is_active, |el| {
                                    el.child(self.menu_button(
                                        format!("pane-menu:{}", block.id),
                                        menu::Target {
                                            server: server_id.clone(),
                                            address: self.address().unwrap_or_default(),
                                            session: Some(session_id.clone()),
                                            block: Some(block.id.clone()),
                                        },
                                        cx,
                                    ))
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                        this.begin_pull(&drag_block, Grab::Move, event.position, cx)
                                    }),
                                ),
                        )
                        .child(div().flex_1().min_h(px(0.0)).child(content))
                        .child(
                            div()
                                .absolute()
                                .right(px(0.0))
                                .bottom(px(0.0))
                                .size(px(14.0))
                                .child(
                                    div()
                                        .absolute()
                                        .right(px(2.0))
                                        .bottom(px(2.0))
                                        .w(px(8.0))
                                        .h(px(8.0))
                                        .border_r_1()
                                        .border_b_1()
                                        .border_color(theme::muted()),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                        this.begin_pull(
                                            &resize_block,
                                            Grab::Resize,
                                            event.position,
                                            cx,
                                        )
                                    }),
                                ),
                        )
                } else if is_active {
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(24.0))
                                .flex_none()
                                .px_2()
                                .flex()
                                .items_center()
                                .justify_between()
                                .bg(chrome.surface())
                                .border_b_1()
                                .border_color(theme::border())
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(block.display_name()),
                                )
                                .child(self.menu_button(
                                    format!("pane-menu:{}", block.id),
                                    menu::Target {
                                        server: server_id.clone(),
                                        address: self.address().unwrap_or_default(),
                                        session: Some(session_id.clone()),
                                        block: Some(block.id.clone()),
                                    },
                                    cx,
                                )),
                        )
                        .child(div().flex_1().min_h(px(0.0)).child(content))
                } else {
                    content
                };

                let pane = div()
                    .id(SharedString::from(block_id.clone()))
                    .absolute()
                    .left(relative(x as f32))
                    .top(relative(y as f32))
                    .w(relative(w as f32))
                    .h(relative(h as f32))
                    .border_1()
                    .border_color(outline)
                    .when(floating, |el| el.shadow_lg().rounded_md())
                    .when(
                        self.tinted.as_deref() == Some(block.id.as_str()) && floating_pass,
                        |el| el.opacity(0.45),
                    )
                    .child(body)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.active_block = Some(activate_id.clone());
                            this.pending_focus = None;
                            if unsupported {
                                window.focus(&this.focus);
                            }
                            // The server keeps the authoritative focus; a click has
                            // to reach it too, or focus moves from the wrong pane.
                            this.publish_focus(&activate_id);
                            // GPUI walks every hitbox under the pointer until one
                            // stops the event. A floating pane sits over a tiled
                            // one, so without this the tiled terminal below also
                            // got the click and took focus from the floating one.
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                elements.push(pane.into_any_element());
            }
        }

        // Left transparent so the window's gradient reaches the terminal: an
        // opaque layer here would flatten the glass. One picture is painted
        // across the whole area, before the panes, so it spans the terminal
        // instead of repeating inside every split.
        let mut area = div().relative().flex_1().overflow_hidden();
        // Armed on the title bar: a movement starts the drag, a release without
        // movement stays a click.
        if let Some(start) = self.title_armed {
            let workspace = cx.entity().downgrade();
            let mover = workspace.clone();
            area = area.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _cx| {
                        let this = mover.clone();
                        window.on_mouse_event(
                            move |event: &MouseMoveEvent, _phase, _window, cx| {
                                if let Some(from) = start.into() {
                                    let moved = (f32::from(event.position.x) - f32::from(from.x))
                                        .abs()
                                        + (f32::from(event.position.y) - f32::from(from.y)).abs();
                                    if moved > 4.0 {
                                        let _ = this.update(cx, |me, cx| {
                                            me.title_armed = None;
                                            crate::win32::drag();
                                            cx.notify();
                                        });
                                    }
                                }
                            },
                        );
                        let this = workspace.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, _phase, _window, cx| {
                            if event.button == MouseButton::Left {
                                let _ = this.update(cx, |me, cx| {
                                    if me.title_armed.take().is_some() {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full(),
            );
        }

        // Where the area is, in window coordinates: a divider drag ends with a
        // pointer position that has to become a fraction of this box.
        let geometry = self.area_geometry.clone();
        area = area.child(
            canvas(
                |_, _, _| (),
                move |bounds, _, _, _| {
                    *geometry.lock().unwrap() = Some((bounds.origin, bounds.size));
                },
            )
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full(),
        );
        if self.divider.is_some() {
            let workspace = cx.entity().downgrade();
            let this = workspace.clone();
            area = area.child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _cx| {
                        let workspace = this.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, _phase, _window, cx| {
                            if event.button != MouseButton::Left {
                                return;
                            }
                            let at = event.position;
                            workspace
                                .update(cx, |me, cx| me.commit_divider(at, cx))
                                .ok();
                        });
                    },
                )
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full(),
            );
        }

        if let Some(image) = self.picture.clone() {
            area = area.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _cx| {
                        let natural = image.size(0);
                        let (iw, ih) = (natural.width.0 as f32, natural.height.0 as f32);
                        if iw < 1.0 || ih < 1.0 {
                            return;
                        }
                        let (bw, bh) =
                            (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        // Cover fit to the whole area, cropped, never distorted.
                        let scale = (bw / iw).max(bh / ih);
                        let (w, h) = (px(iw * scale), px(ih * scale));
                        let cover = Bounds::new(
                            point(
                                bounds.origin.x + (px(bw) - w) * 0.5,
                                bounds.origin.y + (px(bh) - h) * 0.5,
                            ),
                            size(w, h),
                        );
                        window.paint_layer(bounds, |window| {
                            let _ = window.paint_image(
                                cover,
                                gpui::Corners::default(),
                                image.clone(),
                                0,
                                false,
                            );
                        });
                    },
                )
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full(),
            );
        }
        let mut area = area.children(elements);
        // After the panes, so the strips sit on top of them: a pane drawn later
        // owns the hitbox under the pointer, and the grab would never land.
        for zone in self.divider_zones(cx) {
            area = area.child(zone);
        }

        // While a pane is being pulled, the window reports pointer movement: a
        // drag that leaves the pane still has to track, and the listeners only
        // live for one frame, so they are registered from a paint callback.
        if self.pull.is_some() {
            let workspace = cx.entity().downgrade();
            let mover = workspace.clone();
            area = area.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _cx| {
                        let size = bounds.size;
                        let this = mover.clone();
                        window.on_mouse_event(
                            move |event: &MouseMoveEvent, _phase, _window, cx| {
                                let _ =
                                    this.update(cx, |me, cx| me.drag_to(event.position, size, cx));
                            },
                        );
                        let this = workspace.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, _phase, _window, cx| {
                            if event.button == MouseButton::Left {
                                let _ = this.update(cx, |me, cx| me.end_pull(cx));
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        }

        area.into_any_element()
    }

    /// The Settings tab: a page of the workspace rather than a dialog, so the
    /// rows can run past the bottom of a short window and the wheel scrolls
    /// them like any other list. Left column picks the page, as in Pebrel.
    fn settings_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let row = |label: &'static str, detail: String, control: AnyElement| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .py_2()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_color(theme::text()).child(label))
                        .child(div().text_xs().text_color(theme::muted()).child(detail)),
                )
                .child(control)
                .into_any_element()
        };

        let toggle = |id: &'static str,
                      on: bool,
                      cx: &mut Context<Self>,
                      pick: fn(&mut Settings) -> &mut bool| {
            let knob_left = if on { px(14.0) } else { px(2.0) };
            let knob = hsla(0.0, 0.0, 1.0, 1.0);
            let track = if on { theme::accent() } else { theme::hover() };
            div()
                .id(id)
                .w(px(30.0))
                .h(px(17.0))
                .rounded_full()
                .relative()
                .bg(track)
                .child(
                    div()
                        .absolute()
                        .top(px(2.0))
                        .left(knob_left)
                        .size(px(13.0))
                        .rounded_full()
                        .bg(knob),
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    let value = pick(&mut this.settings);
                    *value = !*value;
                    this.settings.save(&this.engine);
                    cx.notify();
                }))
                .into_any_element()
        };

        let stepper = |cx: &mut Context<Self>, id: &'static str, glyph: &'static str| {
            let delta = if glyph == "+" { 1.0 } else { -1.0 };
            div()
                .id(id)
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_color(theme::text())
                .hover(|el| el.bg(theme::hover()))
                .child(glyph)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let size = this.settings.font_size + delta;
                    this.set_font_size(size, cx);
                }))
                .into_any_element()
        };

        let chrome = self.chrome();
        let font = self.settings.font_size;
        let acrylic = toggle("toggle-acrylic", self.settings.acrylic, cx, |s| {
            &mut s.acrylic
        });
        let sidebar = toggle("toggle-sidebar-setting", self.settings.sidebar, cx, |s| {
            &mut s.sidebar
        });
        let down = stepper(cx, "font-down", "−");
        let up = stepper(cx, "font-up", "+");

        let mut nav: Vec<AnyElement> = Vec::new();
        for (page, label) in [
            (Page::Appearance, "Appearance"),
            (Page::Terminal, "Terminal"),
            (Page::Keys, "Shortcuts"),
            (Page::Connection, "Connection"),
        ] {
            let selected = self.page == page;
            nav.push(
                div()
                    .id(SharedString::from(format!("settings-page-{label}")))
                    .mx_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .when(selected, |el| {
                        el.bg(chrome.active()).text_color(theme::text())
                    })
                    .when(!selected, |el| {
                        el.text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                    })
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.page = page;
                        if page == Page::Connection
                            && this.server_info.get(this.server_id.as_deref()).is_none()
                        {
                            this.refresh_info(cx);
                        }
                        if page == Page::Connection
                            && this.creators.get(this.server_id.as_deref()).is_none()
                        {
                            this.refresh_creators(cx);
                        }
                        if page == Page::Terminal {
                            this.refresh_modes(cx);
                        }
                        if page == Page::Keys
                            && this.keymap.get(this.server_id.as_deref()).is_none()
                        {
                            this.refresh_keymap(cx);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }

        let mut rows: Vec<AnyElement> = Vec::new();
        match self.page {
            Page::Appearance => {
                rows.push(row(
                    "Translucent",
                    "Layer the chrome over a gradient instead of flat colours".into(),
                    acrylic,
                ));
                rows.push(row("Sidebar", "Show the session list".into(), sidebar));
                rows.push(theme::section("BACKGROUND").into_any_element());
                rows.push(row(
                    "Picture",
                    self.settings
                        .background
                        .as_deref()
                        .and_then(|path| path.rsplit(['\\', '/']).next())
                        .unwrap_or("None")
                        .to_owned(),
                    {
                        let clear = self.settings.background.as_ref().map(|_| {
                            div()
                                .id("bg-clear")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                                .child("Clear")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_background(None, cx);
                                }))
                                .into_any_element()
                        });
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("bg-choose")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(theme::text())
                                    .bg(chrome.active())
                                    .hover(|el| el.bg(chrome.hover()))
                                    .child("Choose…")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_background(cx)),
                                    ),
                            )
                            .children(clear)
                            .into_any_element()
                    },
                ));
                rows.push(row(
                    "Picture opacity",
                    format!("{:.0}%", self.settings.background_opacity * 100.0),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("bg-less")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("−")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let value = (this.settings.background_opacity - 0.1).max(0.05);
                                    this.set_background_opacity(value, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("bg-more")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("+")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let value = (this.settings.background_opacity + 0.1).min(0.95);
                                    this.set_background_opacity(value, cx);
                                })),
                        )
                        .into_any_element(),
                ));
            }
            Page::Terminal => {
                rows.push(row(
                    "Font size",
                    format!("{font:.0} pt"),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(down)
                        .child(up)
                        .into_any_element(),
                ));
                rows.push(row(
                    "Scrollback",
                    format!("{} lines", self.settings.scrollback_lines),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("scroll-less")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("−")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_scrollback(this.settings.scrollback_lines / 2, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("scroll-more")
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("+")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_scrollback(this.settings.scrollback_lines * 2, cx);
                                })),
                        )
                        .into_any_element(),
                ));
                rows.push(theme::section("FONT").into_any_element());
                rows.push(row(
                    "Terminal",
                    "The first installed family the shell offers".into(),
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(self.font.to_string())
                        .into_any_element(),
                ));
                if let Some(modes) = &self.pane_modes {
                    let screen = modes
                        .get("screen")
                        .and_then(|s| s.as_str())
                        .unwrap_or("primary");
                    let on = |name: &str| {
                        modes
                            .get("modes")
                            .and_then(|m| m.as_array())
                            .map(|list| {
                                list.iter()
                                    .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(name))
                                    .and_then(|m| m.get("set"))
                                    .and_then(|s| s.as_bool())
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false)
                    };
                    let state = |yes: bool| if yes { "on" } else { "off" };
                    rows.push(theme::section("THIS PANE").into_any_element());
                    rows.push(row(
                        "Screen",
                        if screen == "alternate" {
                            "alternate — a full-screen program owns it".into()
                        } else {
                            "primary".into()
                        },
                        div().into_any_element(),
                    ));
                    rows.push(row(
                        "Mouse reporting",
                        format!(
                            "{} · bracketed paste {}",
                            state(on("mouse_reporting") || on("mouse_normal")),
                            state(on("bracketed_paste"))
                        ),
                        div().into_any_element(),
                    ));
                }
            }
            Page::Connection => {
                let status = self
                    .server_info
                    .get(self.server_id.as_deref())
                    .and_then(|i| i.get("status"));
                let whoami = self
                    .server_info
                    .get(self.server_id.as_deref())
                    .and_then(|i| i.get("whoami"));
                let text = |value: Option<&serde_json::Value>, pointer: &str| -> String {
                    value
                        .and_then(|v| v.pointer(pointer))
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_else(|| "—".into())
                };
                rows.push(row(
                    "Server",
                    self.address().unwrap_or_else(|| "—".into()),
                    div().into_any_element(),
                ));
                rows.push(row(
                    "Server actions",
                    "Connection and configuration".into(),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("reconnect")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Reconnect")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(id) = this.server_id.clone() {
                                        this.engine.reconnect(&id);
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            div()
                                .id("probe")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Probe")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.server_info = ServerCache::default();
                                    this.refresh_info(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("config-reload")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Reload config")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.connection_call("server.config_reload", json!({}), cx)
                                })),
                        )
                        .into_any_element(),
                ));
                rows.push(row(
                    "Signed in as",
                    text(whoami, "/account"),
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(text(whoami, "/principal/kind"))
                        .into_any_element(),
                ));
                rows.push(row(
                    "Connection",
                    text(whoami, "/principal/transport"),
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(text(whoami, "/principal/remote_addr"))
                        .into_any_element(),
                ));
                rows.push(row(
                    "Rex server",
                    format!("API revision {}", text(status, "/api_revision")),
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(format!("{} sessions", text(status, "/session_count")))
                        .into_any_element(),
                ));
                rows.push(row(
                    "Tailscale",
                    text(status, "/tailscale/state"),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("ts-enable")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .bg(chrome.active())
                                .text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("Enable")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tailscale(false, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("ts-login")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                                .child("Sign in")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tailscale(true, cx);
                                })),
                        )
                        .child(
                            div()
                                .id("ts-logout")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .child("Logout")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tailscale_call("server.tailscale.logout", cx)
                                })),
                        )
                        .child(
                            div()
                                .id("ts-disable")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .child("Disable")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tailscale_call("server.tailscale.disable", cx)
                                })),
                        )
                        .child(
                            div()
                                .id("ts-rename")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .child("Rename")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_prompt(
                                        Ask::RenameTailscale,
                                        "Tailscale hostname",
                                        window,
                                    );
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                ));
                rows.push(row(
                    "This client",
                    "flex".into(),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("refresh-info")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::text())
                                .bg(chrome.active())
                                .hover(|el| el.bg(chrome.hover()))
                                .child("Refresh")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(server) = &this.server_id {
                                        this.server_info.remove(server);
                                    }
                                    this.refresh_info(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("name-client")
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .text_color(theme::muted())
                                .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                                .child("Name this client")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let label = format!("flex on {}", this.device());
                                    this.name_client(label, cx);
                                })),
                        )
                        .into_any_element(),
                ));
                rows.push(row(
                    "Allow remote client actions (trusted peers only)",
                    "Remote peers can request local UI actions. Disabled by default.".into(),
                    toggle(
                        "allow-remote-actions",
                        self.settings.allow_remote_actions,
                        cx,
                        |s| &mut s.allow_remote_actions,
                    ),
                ));
                rows.push(theme::section("SERVER DIRECTORY").into_any_element());
                if let Some(server) = self.server_id.as_ref().and_then(|id| self.state.server(id)) {
                    let target = menu::Target {
                        server: server.id.clone(),
                        address: server.address.clone(),
                        session: None,
                        block: None,
                    };
                    let source = target.clone();
                    rows.push(row(
                        "Selected server",
                        format!("{} — {}", server.name, server.address),
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                div()
                                    .id("directory-refresh")
                                    .min_w(px(24.0))
                                    .h(px(24.0))
                                    .px_2()
                                    .text_xs()
                                    .child("Refresh")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.refresh_hosts_for(
                                            source.address.clone(),
                                            source.server.clone(),
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                div()
                                    .id("directory-add")
                                    .min_w(px(24.0))
                                    .h(px(24.0))
                                    .px_2()
                                    .text_xs()
                                    .child("Add entry…")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_prompt(
                                            Ask::NewHost,
                                            "Add directory entry — name and endpoint",
                                            window,
                                        );
                                        this.prompt.as_mut().unwrap().scope = Some(target.clone());
                                        cx.notify();
                                    })),
                            )
                            .into_any_element(),
                    ));
                    if let Some(hosts) = self.hosts.get(Some(&server.id)) {
                        let hosts = hosts.get("hosts").unwrap_or(hosts).as_array();
                        if hosts.is_none_or(|list| list.is_empty()) {
                            rows.push(row(
                                "Directory",
                                "No directory entries".into(),
                                div().into_any_element(),
                            ));
                        }
                        for host in hosts.into_iter().flatten() {
                            let label = host
                                .get("label")
                                .and_then(|v| v.as_str())
                                .unwrap_or("Server")
                                .to_owned();
                            let endpoints = host
                                .get("endpoints")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                                .filter_map(|v| v.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let mut actions = div().into_any_element();
                            if let Some(id) = host.get("id").and_then(|v| v.as_str()) {
                                let (id, name) = (id.to_owned(), label.clone());
                                let target = menu::Target {
                                    server: server.id.clone(),
                                    address: server.address.clone(),
                                    session: None,
                                    block: None,
                                };
                                actions = div()
                                    .id(SharedString::from(format!("directory-remove:{id}")))
                                    .min_w(px(24.0))
                                    .h(px(24.0))
                                    .px_2()
                                    .text_xs()
                                    .child("Remove…")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_prompt(
                                            Ask::DeleteHost(id.clone(), name.clone()),
                                            &format!("Remove directory entry {name}?"),
                                            window,
                                        );
                                        this.prompt.as_mut().unwrap().scope = Some(target.clone());
                                        cx.notify();
                                    }))
                                    .into_any_element();
                            }
                            rows.push(row(
                                "Directory entry",
                                format!("{label} — {endpoints}"),
                                actions,
                            ));
                        }
                    } else {
                        rows.push(row(
                            "Directory",
                            "Not loaded — choose Refresh".into(),
                            div().into_any_element(),
                        ));
                    }
                } else {
                    rows.push(row(
                        "Directory",
                        "No selected server".into(),
                        div().into_any_element(),
                    ));
                }
                rows.push(theme::section("CLIENT LUA SCRIPTS").into_any_element());
                rows.push(row(
                    "Script",
                    self.script
                        .as_ref()
                        .map(|s| format!("{} — Load executes on the selected server", s.name))
                        .unwrap_or("No script selected".into()),
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .id("script-choose")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Choose .lua…")
                                .on_click(cx.listener(|this, _, _, cx| this.choose_script(cx))),
                        )
                        .child(
                            div()
                                .id("script-load")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Load")
                                .on_click(cx.listener(|this, _, _, cx| this.set_script(false, cx))),
                        )
                        .child(
                            div()
                                .id("script-unload")
                                .px_2()
                                .py_1()
                                .text_xs()
                                .child("Unload")
                                .on_click(cx.listener(|this, _, _, cx| this.set_script(true, cx))),
                        )
                        .into_any_element(),
                ));
                for diagnostic in self
                    .script_diagnostics
                    .get(self.server_id.as_deref())
                    .into_iter()
                    .flatten()
                {
                    rows.push(
                        div()
                            .px_4()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(diagnostic.clone())
                            .into_any_element(),
                    );
                }
                rows.push(row(
                    "Block creators",
                    "Choose a flavor, then provide its options as a JSON object.".into(),
                    div()
                        .id("creators-refresh")
                        .px_2()
                        .py_1()
                        .text_xs()
                        .child("Discover / Refresh")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_creators(cx)))
                        .into_any_element(),
                ));
                if let Some(server) = self.server_id.clone() {
                    if let Some(creators) = self
                        .creators
                        .get(Some(&server))
                        .and_then(|v| v.get("creators"))
                        .and_then(|v| v.as_array())
                    {
                        for creator in creators {
                            for flavor in creator
                                .get("flavors")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                            {
                                let Some(name) = flavor
                                    .get("flavor")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned)
                                else {
                                    continue;
                                };
                                let schema =
                                    flavor.get("options_schema").cloned().unwrap_or(json!({}));
                                let title = flavor
                                    .get("title")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&name)
                                    .to_owned();
                                let description = flavor
                                    .get("description")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_owned();
                                let target_server = server.clone();
                                rows.push(row(
                                    "Create block",
                                    format!("{title} · {description}"),
                                    div()
                                        .id(SharedString::from(format!("create:{name}")))
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .child("Create…")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            if this.server_id.as_ref() != Some(&target_server) {
                                                this.rpc_error("Create block", "Server selection changed; choose the flavor again", cx);
                                                return;
                                            }
                                    let Some(session) = this.session_id.clone() else {
                                                this.rpc_error(
                                                    "Create block",
                                                    "Select or create a session first",
                                                    cx,
                                                );
                                                return;
                                            };
                                            let anchor = this.active().map(|b| b.id);
                                            this.open_prompt(
                                                Ask::CreateBlock {
                                                    server: target_server.clone(),
                                                    session,
                                                    anchor,
                                                    flavor: name.clone(),
                                                    schema: schema.clone(),
                                                },
                                                "New block options (JSON object, e.g. {})",
                                                window,
                                            );
                                            cx.notify();
                                        }))
                                        .into_any_element(),
                                ));
                            }
                        }
                    }
                }
                if let Some(clients) = self
                    .server_info
                    .get(self.server_id.as_deref())
                    .and_then(|v| v.get("clients"))
                {
                    rows.push(theme::section("CONNECTED CLIENTS").into_any_element());
                    rows.push(
                        div()
                            .px_4()
                            .py_2()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(serde_json::to_string_pretty(clients).unwrap_or_default())
                            .into_any_element(),
                    );
                }
            }
            Page::Keys => {
                rows.push(theme::section("SHORTCUTS").into_any_element());
                if let Some(keymap) = self.keymap.get(self.server_id.as_deref()) {
                    let config = keymap.get("config").cloned().unwrap_or_default();
                    let path = config
                        .get("path")
                        .and_then(|p| p.as_str())
                        .unwrap_or("init.lua")
                        .to_owned();
                    let exists = config
                        .get("exists")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false);
                    rows.push(theme::section("REX").into_any_element());
                    rows.push(row(
                        "init.lua",
                        if exists {
                            "loaded — its bindings are listed below".into()
                        } else {
                            "not found; flex's own table is what you get".into()
                        },
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(path)
                            .into_any_element(),
                    ));
                    let bindings = keymap
                        .get("bindings")
                        .and_then(|b| b.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for binding in bindings {
                        let keys = binding
                            .get("keys")
                            .map(|k| match k {
                                serde_json::Value::Array(list) => list
                                    .iter()
                                    .filter_map(|k| k.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        let action = binding
                            .get("action")
                            .or_else(|| binding.get("name"))
                            .map(|a| match a {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        let binding = &binding;
                        rows.push(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_4()
                                .py_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(binding.to_string()),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .bg(chrome.active())
                                        .text_xs()
                                        .text_color(theme::text())
                                        .child(format!("{keys} → {action}")),
                                )
                                .into_any_element(),
                        );
                    }
                }
                rows.extend(shortcuts::BINDINGS.iter().map(|(keys, _, label)| {
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_4()
                        .py_1()
                        .child(div().text_xs().text_color(theme::muted()).child(*label))
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(chrome.active())
                                .text_xs()
                                .text_color(theme::text())
                                .child(keys.replace("ctrl", "Ctrl").replace("shift", "Shift")),
                        )
                        .into_any_element()
                }));
            }
        }

        div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .w(theme::SIDEBAR_W)
                    .h_full()
                    .flex_col()
                    .bg(chrome.surface())
                    .border_r_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .id("settings-back")
                            .mx_2()
                            .mt_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                            .child("← Workspace")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    )
                    .child(theme::section("SETTINGS"))
                    .children(nav),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    // Without this a flex item refuses to shrink below its
                    // content, and the page never has anything to scroll.
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex_col()
                            .max_w(px(720.0))
                            .py_6()
                            .child(div().px_4().pb_2().text_color(theme::text()).child(
                                match self.page {
                                    Page::Appearance => "Appearance",
                                    Page::Terminal => "Terminal",
                                    Page::Keys => "Shortcuts",
                                    Page::Connection => "Connection",
                                },
                            ))
                            .children(rows),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.process_client_requests(window, cx);
        for (name, args) in std::mem::take(&mut self.configured_actions) {
            self.run_configured_action(&name, args, window, cx);
        }
        if !self.styled {
            self.styled = true;
            crate::win32::round_corners();
        }
        // Moving the window to a monitor with another DPI gives GPUI a new size
        // and scale in one message; the frame that follows is sometimes laid out
        // against the old renderer size, and the right-hand window buttons end up
        // past the edge until another redraw happens (switching apps does that).
        // Ask for a couple more frames after the geometry changes.
        let geometry = (window.scale_factor(), window.bounds());
        if self.last_geometry != Some(geometry) {
            let first = self.last_geometry.is_none();
            self.last_geometry = Some(geometry);
            if !first {
                cx.spawn(async move |this, cx| {
                    for delay in [60_u64, 220] {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(delay))
                            .await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
        }
        let title_bar = self.title_bar(cx);
        let toasts = self.toast_strip(cx);
        let prompt = self.prompt_overlay();
        let palette = self.palette_overlay(cx);
        let menu = self.menu_overlay(window, cx);
        let body = if self.show_settings {
            self.settings_page(cx)
        } else {
            let sidebar = if self.settings.sidebar {
                Some(self.sidebar(cx))
            } else {
                None
            };
            let panes = self.panes(window, cx);
            div()
                .flex()
                .flex_1()
                .min_h(px(0.0))
                .children(sidebar)
                .child(div().flex().flex_col().flex_1().min_w(px(0.0)).child(panes))
                .into_any_element()
        };

        // Semi-transparent, not see-through: GPUI paints opaque pixels on
        // Windows, so the glass is the shell's own — a gradient behind surfaces
        // that are only partly opaque. `Chrome::glass` carries the alphas.
        let background = if self.settings.acrylic {
            linear_gradient(
                180.0,
                linear_color_stop(hsla(0.58, 0.22, 0.27, 1.0), 0.0),
                linear_color_stop(hsla(0.62, 0.16, 0.09, 1.0), 1.0),
            )
        } else {
            theme::bg().into()
        };
        let root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(background);

        root.text_color(theme::text())
            .text_sm()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.menu.is_some() {
                    this.menu_key(event, window, cx);
                    cx.stop_propagation();
                    return;
                }
                if this.palette.is_some() && this.palette_key(event, window, cx) {
                    cx.stop_propagation();
                    return;
                }
                if this.prompt.is_some() && this.prompt_key(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                if this.show_settings && event.keystroke.key == "escape" {
                    this.close_settings(window, cx);
                    return;
                }
                // The terminal already resolved its keymap. Let ordinary keys propagate to
                // Windows/IME without resurrecting a removed default shortcut here.
                if !this.show_settings
                    && !this.prefix.load(std::sync::atomic::Ordering::Relaxed)
                    && this.terminals.values().any(|view| view.read(cx).owns_keyboard(window))
                {
                    return;
                }
                if let Some(action) = shortcuts::lookup(&event.keystroke) {
                    this.run(action, window, cx);
                    return;
                }
                // Armed prefix: the next bare key picks a tool.
                if this.prefix.load(std::sync::atomic::Ordering::Relaxed) {
                    this.prefix
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    let modifiers = &event.keystroke.modifiers;
                    let bare = !modifiers.control && !modifiers.alt && !modifiers.platform;
                    if bare {
                        match event.keystroke.key.to_ascii_lowercase().as_str() {
                            "g" => this.run_tool(Tool::Lazygit, cx),
                            "f" => this.run_tool(Tool::Files, cx),
                            "d" => this.run_tool(Tool::Dirs, cx),
                            "z" => this.run_tool(Tool::Anything, cx),
                            _ => {}
                        }
                    }
                    if this.pending_tool.as_ref().is_some_and(|tool| !tool.observed) {
                        window.focus(&this.focus);
                    }
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(title_bar)
            .child(body)
            .child(prompt)
            .child(palette)
            .child(toasts)
            .child(menu)
    }
}

/// Window controls are drawn, not typed: `─ □ ✕` live in fonts that GPUI's
/// Windows text system does not always fall back to, and drew nothing.
#[derive(Clone, Copy)]
enum Shape {
    Minimize,
    Maximize,
    Restore,
    Close,
}

fn shape(shape: Shape) -> AnyElement {
    let stroke = theme::muted();
    match shape {
        Shape::Minimize => div().w(px(10.0)).h(px(1.0)).bg(stroke).into_any_element(),
        Shape::Maximize => div()
            .size(px(9.0))
            .border_1()
            .border_color(stroke)
            .into_any_element(),
        Shape::Restore => div()
            .relative()
            .size(px(10.0))
            .child(
                div()
                    .absolute()
                    .left(px(2.0))
                    .top(px(2.0))
                    .size(px(7.0))
                    .border_1()
                    .border_color(stroke),
            )
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .size(px(7.0))
                    .border_1()
                    .border_color(stroke),
            )
            .into_any_element(),
        Shape::Close => div().text_size(px(15.0)).child("×").into_any_element(),
    }
}

/// A title bar button: no drag, hover highlight, close goes red.
fn control(
    id: &'static str,
    glyph: Shape,
    danger: bool,
    action: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    div()
        .id(id)
        .h_full()
        .w(px(44.0))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme::muted())
        .hover(move |el| {
            let el = el.bg(if danger {
                theme::danger()
            } else {
                theme::hover()
            });
            if danger {
                el.text_color(theme::text())
            } else {
                el
            }
        })
        .child(shape(glyph))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| action(window, cx))
        .into_any_element()
}

/// A press on the title bar: drag the window, or, on the second press of a
/// double-click, maximize or restore it. Windows would send `WM_NCLBUTTONDBLCLK`,
/// but the first press is handed to the shell for dragging, which swallows the
/// second, so the pair is spotted here instead.
fn title_press(
    this: &mut Workspace,
    event: &MouseDownEvent,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if this.is_double_press(event.position) {
        this.title_armed = None;
        crate::win32::toggle_maximize();
    } else {
        // Wait for movement before handing the window to the drag loop.
        this.title_armed = Some(event.position);
    }
    cx.notify();
}

/// The most urgent agent state among a window's panes, if any pane reports one.
fn window_agent(window: &WindowState) -> Option<shrew_core::AgentState> {
    window
        .blocks
        .iter()
        .filter_map(|block| block.agent.as_ref().map(|agent| agent.state))
        .max()
}

/// The floating layer that holds `block_id`, from a `session.view` result.
fn floating_layer_id(view: &serde_json::Value, block_id: &str) -> Option<String> {
    for window in view.get("windows")?.as_array()? {
        for layer in window.get("layers")?.as_array()? {
            if layer.get("kind").and_then(|k| k.as_str()) != Some("floating") {
                continue;
            }
            let holds = layer
                .get("blocks")
                .and_then(|b| b.as_array())
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b.get("block_id").and_then(|v| v.as_str()) == Some(block_id))
                });
            if holds {
                return layer
                    .get("layer_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
            }
        }
    }
    None
}

/// Which side of its neighbour a pane sat on, and how much of the space it took,
/// from the rect it had before being floated.
///
/// `move_block` takes a side and the share the *before* half gets, so a pane on
/// the right of a 30/70 split comes back as `after` with a ratio of 0.3.
/// The floating layer a block sits in, which `set_layer_state` wants.
fn find_layer(view: &serde_json::Value, block_id: &str) -> Option<String> {
    for window in view.get("windows")?.as_array()? {
        for layer in window.get("layers")?.as_array()? {
            let holds = layer
                .get("blocks")?
                .as_array()?
                .iter()
                .any(|b| b.get("block_id").and_then(|id| id.as_str()) == Some(block_id));
            if holds {
                return layer
                    .get("layer_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned);
            }
        }
    }
    None
}

fn split_hint([x, y, w, h]: [f64; 4]) -> (&'static str, &'static str, f64) {
    // Taller than wide means it was divided left/right.
    let (side, direction, ratio) = if h >= w {
        if x < 0.5 {
            ("before", "horizontal", w)
        } else {
            ("after", "horizontal", x)
        }
    } else if y < 0.5 {
        ("before", "vertical", h)
    } else {
        ("after", "vertical", y)
    };
    (side, direction, ratio.clamp(0.05, 0.95))
}

#[cfg(test)]
mod split_tests {
    use super::split_hint;

    #[test]
    fn remembers_the_side_and_share() {
        // Right half of an even split.
        assert_eq!(
            split_hint([0.5, 0.0, 0.5, 1.0]),
            ("after", "horizontal", 0.5)
        );
        // Left 30%.
        assert_eq!(
            split_hint([0.0, 0.0, 0.3, 1.0]),
            ("before", "horizontal", 0.3)
        );
        // Top 40%.
        assert_eq!(
            split_hint([0.0, 0.0, 1.0, 0.4]),
            ("before", "vertical", 0.4)
        );
        // Bottom quarter.
        assert_eq!(
            split_hint([0.0, 0.75, 1.0, 0.25]),
            ("after", "vertical", 0.75)
        );
        // A lone pane cannot ask for the whole space back.
        assert_eq!(split_hint([0.0, 0.0, 1.0, 1.0]).2, 0.95);
    }
}

#[cfg(test)]
mod management_tests {
    use super::*;

    #[test]
    fn palette_preserves_lua_schema_and_never_prompts_for_fixed_creator_args() {
        let schema = json!({"type":"object","properties":{"cwd":{"type":"string"}}});
        let map = json!({"actions":[
            {"name":"dev_layout","kind":"lua","title":"Layout","args":schema},
            {"name":"com.example.reset","kind":"block","title":"Reset","args":{"fixed":true}},
            {"name":"pane.split","kind":"client","title":"Split"},
            {"name":"com.example.method","kind":"block","args":{"type":"object"}}
        ]});
        let actions = palette_actions(
            &map,
            "original-server",
            "original-session",
            Some("original-block".into()),
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].args_schema, Some(schema));
        assert_eq!(actions[0].server, "original-server");
        assert_eq!(actions[0].session, "original-session");
        assert_eq!(actions[0].block.as_deref(), Some("original-block"));
        assert_eq!(actions[1].args_schema, None);
    }

    #[test]
    fn creator_options_go_to_split_or_new_window_without_shell_interpolation() {
        let options = json!({"command":["echo","$HOME; rm -rf /"],"cwd":"D:/some folder"});
        let (method, payload) =
            creator_request("session", Some("anchor"), "creator.flavor", options.clone());
        assert_eq!(method, "session.new_split");
        assert_eq!(payload["anchor_id"], "anchor");
        assert_eq!(payload["layout"]["block"]["options"], options);
        let (method, payload) = creator_request("session", None, "creator.flavor", json!({}));
        assert_eq!(method, "session.new_window");
        assert!(payload.get("anchor_id").is_none());
    }

    #[test]
    fn fixed_action_omits_args_and_global_lua_action_omits_session() {
        let fixed = action_request("session", Some("block"), "creator.reset", None);
        assert!(fixed.get("args").is_none());
        assert_eq!(fixed["session_id"], "session");
        assert_eq!(fixed["block_id"], "block");
        let args = json!({"cwd":"D:/work"});
        let configured = action_request("", None, "layout", Some(args.clone()));
        assert!(configured.get("session_id").is_none());
        assert_eq!(configured["args"], args);
        assert_eq!(configured["origin"], "palette");
    }

    #[test]
    fn cached_keymap_generation_only_change_keeps_active_modes() {
        let old = json!({"bindings":[],"modes":[{"name":"resize"}],"config":{"generation":1,"hash":"same","instance_id":"same"}});
        let mut new = old.clone();
        new["config"]["generation"] = json!(2);
        assert!(!keymap_resolution_changed(Some(&old), &new));
        new["bindings"] = json!([{"action":"pane.close","keys":["ctrl+w"]}]);
        assert!(keymap_resolution_changed(Some(&old), &new));
    }

    #[test]
    fn script_selection_only_stages_utf8_lua_and_load_payload_is_explicit() {
        let script = script_draft(
            std::path::Path::new("D:/work/custom.lua"),
            b"print('unchanged source')",
        )
        .unwrap();
        assert_eq!(script.name, "custom.lua");
        assert_eq!(script.source, "print('unchanged source')");
        assert_eq!(
            script_request(Some(&script)),
            json!({"scripts":[{"name":"custom.lua","source":"print('unchanged source')"}]})
        );
        assert_eq!(script_request(None), json!({"scripts":[]}));
        assert!(script_draft(std::path::Path::new("bad.txt"), b"print(1)").is_err());
        assert!(script_draft(std::path::Path::new("bad.lua"), &[0xff]).is_err());
    }

    #[test]
    fn json_prompts_accept_multiline_paste_without_touching_confirmation_prompts() {
        let mut prompt = Prompt {
            what: Ask::CreateBlock {
                server: "server".into(),
                session: "session".into(),
                anchor: None,
                flavor: "flavor".into(),
                schema: json!({}),
            },
            label: "Options".into(),
            value: String::new(),
            scope: None,
        };
        prompt.paste("{\n\"path\":\"C:/a folder\"\n}");
        assert_eq!(
            json_object(&prompt.value).unwrap(),
            json!({"path":"C:/a folder"})
        );
        prompt.what = Ask::CloseLayer("server".into(), "session".into(), "layer".into());
        let value = prompt.value.clone();
        prompt.paste("do not append");
        assert_eq!(prompt.value, value);
    }

    #[test]
    fn options_and_action_params_must_be_json_objects() {
        assert_eq!(
            json_object(" {\n\"cwd\": \"D:/work\"\n} ").unwrap(),
            json!({"cwd":"D:/work"})
        );
        for invalid in ["", "[]", "null", "42", "{oops}"] {
            assert!(json_object(invalid).is_err(), "accepted {invalid}");
        }
    }
}

fn json_object(text: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if !value.is_object() {
        return Err("Expected a JSON object, such as {}".into());
    }
    Ok(value)
}

fn palette_actions(
    map: &serde_json::Value,
    server: &str,
    session: &str,
    block: Option<String>,
) -> Vec<PaletteActionItem> {
    map.get("actions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let title = a.get("title")?.as_str()?.to_owned();
            if title.is_empty() || a.get("kind").and_then(|v| v.as_str()) == Some("client") {
                return None;
            }
            let name = a.get("name")?.as_str()?.to_owned();
            // Creator actions already carry fixed arguments; only Lua actions accept user parameters.
            let args_schema = if a.get("kind").and_then(|v| v.as_str()) == Some("lua") {
                a.get("args")
                    .filter(|v| !v.is_null() && **v != serde_json::Value::Bool(false))
                    .cloned()
            } else {
                None
            };
            Some(PaletteActionItem {
                server: server.to_owned(),
                session: session.to_owned(),
                block: block.clone(),
                name,
                title,
                description: a
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                args_schema,
            })
        })
        .collect()
}

fn creator_request(
    session: &str,
    anchor: Option<&str>,
    flavor: &str,
    options: serde_json::Value,
) -> (&'static str, serde_json::Value) {
    let mut payload =
        json!({"session_id":session,"layout":{"block":{"flavor":flavor,"options":options}}});
    if let Some(anchor) = anchor {
        payload["anchor_id"] = json!(anchor);
        payload["direction"] = json!("horizontal");
        payload["side"] = json!("after");
        payload["ratio"] = json!(0.5);
        ("session.new_split", payload)
    } else {
        ("session.new_window", payload)
    }
}

fn action_request(
    session: &str,
    block: Option<&str>,
    name: &str,
    args: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut payload = json!({"action":name,"origin":"palette"});
    if !session.is_empty() {
        payload["session_id"] = json!(session);
        if let Some(block) = block {
            payload["block_id"] = json!(block);
        }
    }
    if let Some(args) = args {
        payload["args"] = args;
    }
    payload
}

fn keymap_resolution_changed(old: Option<&serde_json::Value>, new: &serde_json::Value) -> bool {
    old.is_none_or(|old| {
        [
            "/bindings",
            "/modes",
            "/actions",
            "/config/hash",
            "/config/instance_id",
        ]
        .iter()
        .any(|path| old.pointer(path) != new.pointer(path))
    })
}

fn script_draft(path: &std::path::Path, bytes: &[u8]) -> Result<ScriptDraft, String> {
    if !path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.eq_ignore_ascii_case("lua"))
    {
        return Err("Choose a .lua file".into());
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Script file name is not UTF-8")?
        .to_owned();
    let source = std::str::from_utf8(bytes)
        .map_err(|_| "Script must contain UTF-8 text")?
        .to_owned();
    Ok(ScriptDraft { name, source })
}

fn script_request(script: Option<&ScriptDraft>) -> serde_json::Value {
    json!({"scripts":script.map(|s|vec![json!({"name":s.name,"source":s.source})]).unwrap_or_default()})
}

impl Prompt {
    fn paste(&mut self, text: &str) {
        if self.what.wants_text() {
            self.value.push_str(text);
        }
    }
}

#[cfg(test)]
mod focus_regression_tests {
    use super::*;

    fn block(id: &str, minimized: bool, floating: bool, creator: &str) -> BlockState {
        BlockState {
            id: id.into(),
            layer_state: if minimized { "minimized" } else { "normal" }.into(),
            floating,
            creator_name: creator.into(),
            rect: [0.2, 0.1, 0.4, 0.5],
            ..Default::default()
        }
    }

    #[test]
    fn window_jump_prefers_its_focused_pane_over_the_first_pane() {
        let blocks = vec![
            block("first", false, false, ""),
            block("focused", false, false, ""),
        ];
        assert_eq!(
            visible_focus(&blocks, None, Some("focused")),
            Some("focused".into())
        );
    }

    #[test]
    fn minimized_blocks_never_win_selection_or_pending_focus() {
        let blocks = vec![
            block("hidden", true, true, ""),
            block("visible", false, false, ""),
        ];
        assert_eq!(
            visible_focus(&blocks, Some("hidden"), Some("hidden")),
            Some("visible".into())
        );
        assert_eq!(visible_focus(&blocks[..1], Some("hidden"), None), None);
        assert!(!pane_visible(&blocks[0], None));
    }

    #[test]
    fn unsupported_floating_blocks_follow_the_same_zoom_and_visibility_rules() {
        let unknown = block("unknown", false, true, "example.unsupported");
        assert!(!unknown.is_terminal());
        assert!(pane_visible(&unknown, None));
        assert!(!pane_visible(&unknown, Some("other")));
        assert_eq!(
            pane_geometry(&unknown, Some("unknown"), unknown.rect),
            [0.0, 0.0, 1.0, 1.0]
        );
        assert_eq!(pane_geometry(&unknown, None, unknown.rect), unknown.rect);
        assert_eq!(
            visible_focus(&[unknown], None, None),
            Some("unknown".into())
        );
    }
}

fn visible_focus(
    blocks: &[BlockState],
    preferred: Option<&str>,
    focused: Option<&str>,
) -> Option<String> {
    let visible = |b: &&BlockState| b.layer_state != "minimized";
    preferred
        .into_iter()
        .chain(focused)
        .find_map(|id| blocks.iter().filter(visible).find(|b| b.id == id))
        .or_else(|| blocks.iter().find(|b| b.layer_state != "minimized"))
        .map(|b| b.id.clone())
}

fn pane_visible(block: &BlockState, zoomed: Option<&str>) -> bool {
    block.layer_state != "minimized" && zoomed.is_none_or(|id| id == block.id)
}

fn pane_geometry(block: &BlockState, zoomed: Option<&str>, live: [f64; 4]) -> [f64; 4] {
    if zoomed == Some(block.id.as_str()) {
        [0.0, 0.0, 1.0, 1.0]
    } else {
        live
    }
}
