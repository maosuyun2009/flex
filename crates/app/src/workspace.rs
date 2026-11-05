//! The window: a title bar of our own (GPUI draws no system one on Windows),
//! the session list beside it, tabs for the session's windows, the panes of the
//! window you picked, and the settings page over them.
//!
//! Every pane is placed from `BlockState::rect`, which the server sends as
//! fractions of its window, so splits, floats and zoom need no layout code here.

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
use crate::terminal::TerminalView;
use crate::theme;
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

/// A picker pane that is running: when its program exits, the choice is read out
/// of its text and typed into the pane it was opened from.
#[derive(Clone)]
struct PendingPick {
    origin_block: String,
    pick_block: String,
    suffix: &'static str,
}

/// Where a pane lands when it is floated: centred, leaving the tiled panes
/// visible around it, the way a floating terminal is usually wanted.
const FLOAT_BOUNDS: [f64; 4] = [0.14, 0.12, 0.72, 0.7];

/// Where a tool (lazygit, a picker) opens as a floating terminal: centred, with
/// the panes still visible around it.
const TOOL_BOUNDS: [f64; 4] = [0.18, 0.1, 0.64, 0.72];

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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rename {
    Tab,
    Pane,
}

/// A one-line text prompt: Rex names windows and blocks, and the shell has no
/// text field, so this is the smallest thing that can type a name.
struct Prompt {
    what: Rename,
    label: String,
    value: String,
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
    float_rects: std::collections::HashMap<String, [f64; 4]>,
    /// Prefix armed: the next bare key picks a tool.
    prefix: Arc<std::sync::atomic::AtomicBool>,
    /// A picker pane that has not answered yet.
    pending_pick: Option<PendingPick>,
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
    /// Whether this client has told the server its label yet.
    named_client: bool,
    /// `server.whoami` and `server.status`, fetched when Connection is opened.
    server_info: Option<serde_json::Value>,
    /// `client.keymap`: the bindings Rex itself would apply, from init.lua.
    keymap: Option<serde_json::Value>,
    /// Panes this shell parked: alive, out of every window, in this list.
    parked: Vec<(String, String)>,
    /// Floating layers this shell tucked away. Rex keeps the state, but
    /// shrew-core's snapshot drops it, so the shell remembers its own.
    minimized: std::collections::HashSet<String>,
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
            terminals: HashMap::new(),
            styled: false,
            show_settings: false,
            page: Page::Appearance,
            focus: cx.focus_handle(),
            pull: None,
            settled: None,
            float_rects: std::collections::HashMap::new(),
            prefix: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_pick: None,
            tinted: None,
            area_geometry: Arc::default(),
            divider: None,
            prompt: None,
            named_client: false,
            server_info: None,
            keymap: None,
            parked: Vec::new(),
            minimized: std::collections::HashSet::new(),
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
            .servers
            .iter()
            .flat_map(|server| server.sessions.iter())
            .find(|s| s.id == session)
            .and_then(|s| {
                s.windows
                    .iter()
                    .find(|w| w.blocks.iter().any(|b| b.id == block))
            })
            .map(|w| w.id.clone());
        self.server_id = Some(server.to_owned());
        self.session_id = Some(session.to_owned());
        if let Some(window) = window {
            self.window_id = Some(window);
        }
        self.active_block = Some(block.to_owned());
        self.pending_focus = Some(block.to_owned());
        self.show_settings = false;
        self.reload(cx);
    }

    /// Re-read the engine's state and drop selections and terminals that went away.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.absorb_notices(cx);
        self.state = self.engine.state();

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
            if windows.iter().any(|w| w.id == active) {
                self.window_id = Some(active);
            }
        }
        if !self.named_client && self.session_id.is_some() && self.address().is_some() {
            self.named_client = true;
            let label = format!("flex on {}", self.device());
            self.name_client(label, cx);
        }
        let blocks = self.blocks();
        if !blocks
            .iter()
            .any(|b| Some(&b.id) == self.active_block.as_ref())
        {
            self.active_block = blocks
                .iter()
                .find(|b| {
                    Some(&b.id)
                        == windows
                            .iter()
                            .find(|w| Some(&w.id) == self.window_id.as_ref())
                            .and_then(|w| w.focused_block_id.as_ref())
                })
                .or_else(|| blocks.first())
                .map(|b| b.id.clone());
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
        // The picker is read before pruning: a pane that closed on exit is gone
        // from the state, but its text is still in our terminal.
        self.settle_pick(&live, cx);
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

    fn blocks(&self) -> Vec<BlockState> {
        self.windows()
            .into_iter()
            .find(|w| Some(&w.id) == self.window_id.as_ref())
            .map(|w| w.blocks)
            .unwrap_or_default()
    }

    fn active(&self) -> Option<BlockState> {
        let blocks = self.blocks();
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
        if let Some(view) = self
            .active()
            .and_then(|block| self.terminals.get(&block.id))
        {
            window.focus(&view.read(cx).handle());
        }
        cx.notify();
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
            Action::SplitRight => self.split(true, cx),
            Action::SplitDown => self.split(false, cx),
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
                self.open_prompt(Rename::Tab, "Rename tab");
                cx.notify();
            }
            Action::RenamePane => {
                self.open_prompt(Rename::Pane, "Rename pane");
                cx.notify();
            }
            Action::MoveTabLeft => self.move_tab(-1, cx),
            Action::MoveTabRight => self.move_tab(1, cx),
            Action::MinimizeFloat => self.toggle_minimized(cx),
            Action::RaiseFloat => self.stack_float(true, cx),
            Action::LowerFloat => self.stack_float(false, cx),
            Action::ParkPane => self.park_pane(cx),
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

    fn open_prompt(&mut self, what: Rename, label: &str) {
        self.prompt = Some(Prompt {
            what,
            label: label.to_owned(),
            value: String::new(),
        });
    }

    /// Type into the prompt. Returns true when the key belonged to it.
    fn prompt_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(prompt) = &mut self.prompt else {
            return false;
        };
        match event.keystroke.key.as_str() {
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
                    Some(text) => {
                        prompt.value.push_str(&text);
                        cx.notify();
                        true
                    }
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

    fn commit_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let (Some(address), Some(session)) = (self.address(), self.session_id.clone()) else {
            return;
        };
        let label = prompt.value.trim().to_owned();
        if label.is_empty() {
            cx.notify();
            return;
        }
        let payload = match prompt.what {
            Rename::Tab => match self.window_id.clone() {
                Some(window_id) => json!({
                    "session_id": session,
                    "window_id": window_id,
                    "label": label,
                }),
                None => return,
            },
            Rename::Pane => match self.active_block.clone() {
                Some(block_id) => json!({
                    "session_id": session,
                    "block_id": block_id,
                    "label": label,
                }),
                None => return,
            },
        };
        let method = match prompt.what {
            Rename::Tab => "session.set_window_label",
            Rename::Pane => "session.set_block_label",
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = engine.call(&address, method, payload).await {
                eprintln!("flex: {method}: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
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

    /// Tuck the floating layer under this pane away, or bring it back.
    ///
    /// Rex would move the *layer* to `minimized`, but that method needs control
    /// API revision 9 and shrew-core (pinned) speaks 8, so the shell hides it
    /// itself and only tells the server when the server can hear it. Another
    /// client keeps seeing the float, which is the cost of the old protocol.
    fn toggle_minimized(&mut self, cx: &mut Context<Self>) {
        let Some(block) = self.active_block.clone() else {
            return;
        };
        let restore = self.minimized.contains(&block);
        if restore {
            self.minimized.remove(&block);
        } else {
            self.minimized.insert(block.clone());
            // Send focus somewhere visible, or the keyboard goes to a hidden pane.
            if let Some(next) = self
                .blocks()
                .into_iter()
                .find(|b| !self.minimized.contains(&b.id))
            {
                self.active_block = Some(next.id.clone());
                self.pending_focus = Some(next.id);
            }
        }
        cx.notify();

        let (Some(address), Some(session)) = (self.address(), self.session_id.clone()) else {
            return;
        };
        let state = if restore { "normal" } else { "minimized" };
        let engine = self.engine.clone();
        cx.spawn(async move |_, _| {
            // Rex wants a layer id, and this shell's snapshot has only blocks.
            let Ok(view) = engine
                .call(&address, "session.view", json!({"session_id": session}))
                .await
            else {
                return;
            };
            let Some(layer) = find_layer(&view, &block) else {
                return;
            };
            let payload = json!({
                "session_id": session,
                "layer_id": layer,
                "state": state,
                "focus": restore,
            });
            if let Err(e) = engine
                .call(&address, "session.set_layer_state", payload)
                .await
            {
                eprintln!("flex: set_layer_state: {e}");
            }
        })
        .detach();
    }

    /// Ask the server who we are and what it is running, for the Connection page.
    fn refresh_info(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.address() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let whoami = engine.call(&address, "server.whoami", json!({})).await;
            let status = engine.call(&address, "server.status", json!({})).await;
            this.update(cx, |me, cx| {
                me.server_info = Some(json!({
                    "whoami": whoami.ok(),
                    "status": status.ok(),
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ask the server for the keymap it would apply, which includes whatever
    /// the user wrote in init.lua.
    fn refresh_keymap(&mut self, cx: &mut Context<Self>) {
        let Some(address) = self.address() else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let keymap = engine.call(&address, "client.keymap", json!({})).await;
            this.update(cx, |me, cx| {
                me.keymap = keymap.ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
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
                Err(e) => eprintln!("flex: {method}: {e}"),
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
        cx.spawn(async move |_, _| {
            let _ = engine
                .call(&address, "client.set_label", json!({"label": label}))
                .await;
        })
        .detach();
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
        let (Some(address), Some(session), Some(block)) =
            (self.address(), self.session_id.clone(), self.active())
        else {
            return;
        };
        let id = block.id.clone();
        let label = block.display_name();
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({"session_id": session, "block_id": id});
            if let Err(e) = engine.call(&address, "session.detach_block", payload).await {
                eprintln!("flex: detach_block: {e}");
                return;
            }
            this.update(cx, |me, cx| {
                me.parked.push((id.clone(), label.clone()));
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
    fn unpark(&mut self, id: String, cx: &mut Context<Self>) {
        let (Some(address), Some(session)) = (self.address(), self.session_id.clone()) else {
            return;
        };
        let Some(anchor) = self.active().map(|b| b.id) else {
            return;
        };
        let engine = self.engine.clone();
        cx.spawn(async move |this, cx| {
            let payload = json!({
                "session_id": session,
                "block_id": id,
                "anchor_block_id": anchor,
                "side": "after",
                "direction": "horizontal",
                "ratio": 0.5,
            });
            if let Err(e) = engine.call(&address, "session.move_block", payload).await {
                eprintln!("flex: move_block: {e}");
                return;
            }
            this.update(cx, |me, cx| {
                me.parked.retain(|(parked, _)| parked != &id);
                me.pending_focus = Some(id.clone());
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        let block_id = block.id.clone();
        let floating = block.floating;
        // Tiling again means placing the pane beside a *tiled* one, and Rex
        // needs a real anchor id. Prefer this window, then another tab in the
        // session; if the whole session is floating, a fresh tab's shell is
        // created to sit beside.
        let anchor = self.tiled_anchor(&block_id);
        if !floating {
            self.float_rects.insert(block_id.clone(), block.rect);
        }
        let hint = self.float_rects.remove(&block_id);

        cx.spawn(async move |this, cx| {
            if !floating {
                let payload = json!({
                    "session_id": session_id,
                    "block_id": block_id,
                    "bounds": FLOAT_BOUNDS,
                });
                if let Err(e) = engine.call(&address, "session.float_block", payload).await {
                    eprintln!("flex: session.float_block: {e}");
                }
            } else {
                let anchor = match anchor {
                    Some(anchor) => Some(anchor),
                    None => {
                        // A split needs an anchor, and the whole session is
                        // floating, so open a tab with a shell to sit beside.
                        let payload = json!({
                            "session_id": session_id,
                            "layout": {"block": {"flavor": "com.superlogical.terminal.shell"}},
                        });
                        match engine.call(&address, "session.new_window", payload).await {
                            Ok(view) => view
                                .get("block_ids")
                                .and_then(|ids| ids.as_array())
                                .and_then(|ids| ids.first())
                                .and_then(|id| id.as_str())
                                .map(str::to_owned),
                            Err(e) => {
                                eprintln!("flex: session.new_window: {e}");
                                None
                            }
                        }
                    }
                };
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
                    eprintln!("flex: session.move_block: {e}");
                }
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Open a tool beside the active pane: lazygit, or an fzf picker whose choice
    /// is typed back into the pane it came from.
    fn run_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        let (Some(address), Some(session_id), Some(origin)) =
            (self.address(), self.session_id.clone(), self.active())
        else {
            return;
        };

        let (command, keep_open, suffix) = match tool {
            Tool::Lazygit => (vec!["lazygit".to_owned()], false, ""),
            Tool::Files => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_FILES.to_owned()],
                true,
                " ",
            ),
            Tool::Dirs => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_DIRS.to_owned()],
                true,
                "/",
            ),
            Tool::Anything => (
                vec!["sh".to_owned(), "-c".to_owned(), PICK_ANY.to_owned()],
                true,
                " ",
            ),
        };

        let engine = self.engine.clone();
        let origin_block = origin.id.clone();
        let window_id = self.window_id.clone();

        cx.spawn(async move |this, cx| {
            // In the pane's own directory, not in `$HOME`: the block's reported
            // pwd is empty unless the shell speaks OSC 7, so ask the server where
            // the foreground process actually is.
            let mut options = json!({"shell": "none", "command": command});
            if let Some(cwd) = engine.block_cwd(&address, &session_id, &origin_block).await {
                options["cwd"] = json!(cwd);
            }
            // A floating terminal, not a split: it covers the work, and closing it
            // leaves the layout exactly as it was.
            let payload = json!({
                "session_id": session_id,
                "window_id": window_id,
                "bounds": TOOL_BOUNDS,
                "layout": {
                    "block": {
                        "flavor": "com.superlogical.terminal.shell",
                        "options": options,
                    }
                },
            });
            let result = engine.call(&address, "session.new_layer", payload).await;
            let block = match result {
                Ok(view) => view
                    .pointer("/block_ids/0")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned),
                Err(e) => {
                    eprintln!("flex: session.new_layer: {e}");
                    None
                }
            };
            let Some(block) = block else {
                this.update(cx, |me, cx| {
                    me.drop_tint(cx);
                })
                .ok();
                return;
            };
            // Whatever the tool, it takes the keyboard: it is what you type into.
            // The server gives a new block focus on its own, but this client also
            // has to move its own focus handle, or keystrokes keep going to the
            // pane the tool was opened from.
            let pick = keep_open.then(|| PendingPick {
                origin_block: origin_block.clone(),
                pick_block: block.clone(),
                suffix,
            });
            this.update(cx, |me, cx| {
                me.active_block = Some(block.clone());
                me.pending_focus = Some(block.clone());
                // Dim the pane the tool covers: the floating one has the keyboard.
                me.tinted = Some(origin_block);
                if let Some(pick) = pick {
                    me.pending_pick = Some(pick);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A picker's program has exited: take the choice out of its text, type it
    /// into the pane it came from, and close the picker.
    fn settle_pick(&mut self, live: &HashSet<String>, cx: &mut Context<Self>) {
        let Some(pick) = self.pending_pick.clone() else {
            return;
        };
        let present = self
            .windows()
            .into_iter()
            .flat_map(|window| window.blocks)
            .find(|block| block.id == pick.pick_block);
        // Gone from the session means it closed on exit; still present means we
        // wait until its process reports having exited.
        let finished = present
            .as_ref()
            .map_or(true, |block| block.exited.is_some());
        if !finished {
            return;
        }
        let still_open = present.is_some();
        self.pending_pick = None;
        self.tinted = None;

        let text = self
            .terminals
            .get(&pick.pick_block)
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
        if std::env::var_os("FLEX_DEBUG").is_some() {
            eprintln!("[pick] finished open={still_open} choice={choice:?}");
        }
        if let Some(path) = choice {
            if let Some(origin) = self.terminals.get(&pick.origin_block) {
                origin.read(cx).send(&format!("{path}{}", pick.suffix));
            }
        }

        // Only a pane that is still open needs closing; one that closed on exit
        // is already gone, and `live` says so.
        if still_open && live.contains(&pick.pick_block) {
            if let (Some(server), Some(session)) = (self.server_id.clone(), self.session_id.clone())
            {
                let engine = self.engine.clone();
                let block = pick.pick_block;
                cx.spawn(async move |_, _| {
                    let _ = engine.shrew().close_block(&server, &session, &block).await;
                })
                .detach();
            }
        }
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

    /// A tiled block to place `block_id` beside: this window first, then any
    /// other window in the session.
    fn tiled_anchor(&self, block_id: &str) -> Option<String> {
        let mut fallback = None;
        for window in self.windows() {
            let here = Some(&window.id) == self.window_id.as_ref();
            for block in window.blocks {
                if block.floating || block.id == block_id {
                    continue;
                }
                if here {
                    return Some(block.id);
                }
                fallback.get_or_insert(block.id);
            }
        }
        fallback
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
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(360.0))
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
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(chrome.active())
                            .text_color(theme::text())
                            .child(format!("{}▌", prompt.value)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Enter to save · Esc to cancel"),
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

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let chrome = self.chrome();
        let mut rows: Vec<AnyElement> = Vec::new();

        // Agents first: the whole point is to see who needs you without
        // hunting through sessions and tabs. Ordered by urgency by the engine.
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
                                this.server_id = Some(server.clone());
                                this.session_id = Some(session.clone());
                                this.window_id = Some(window.clone());
                                this.active_block = Some(block.clone());
                                this.pending_focus = Some(block.clone());
                                this.reload(cx);
                            }),
                        )
                        .into_any_element(),
                );
            }
        }

        let tucked: Vec<AnyElement> = self
            .blocks()
            .into_iter()
            .filter(|block| self.minimized.contains(&block.id))
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
            rows.push(theme::section("TUCKED AWAY").into_any_element());
            rows.extend(tucked);
        }
        if !self.parked.is_empty() {
            rows.push(theme::section("PARKED").into_any_element());
            for (id, label) in self.parked.clone() {
                let flat = id.clone();
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

        rows.push(theme::section("SESSIONS").into_any_element());
        for server in &self.state.servers {
            let dot = match server.connectivity {
                Connectivity::Online => theme::accent(),
                Connectivity::Connecting => theme::muted(),
                Connectivity::Offline { .. } => theme::danger(),
                Connectivity::Idle => theme::border(),
            };
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(div().size(px(6.0)).rounded_full().bg(dot))
                    .child(server.name.clone())
                    .into_any_element(),
            );

            for session in &server.sessions {
                let id = session.id.clone();
                let active = self.session_id.as_deref() == Some(id.as_str());
                rows.push(
                    div()
                        .id(SharedString::from(id.clone()))
                        .mx_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when(active, |el| {
                            el.bg(chrome.active()).text_color(theme::text())
                        })
                        .when(!active, |el| {
                            el.text_color(theme::text())
                                .hover(|el| el.bg(chrome.hover()))
                        })
                        .when_some(session.agent_state(), |el, state| {
                            el.child(theme::dot(theme::agent(state)))
                        })
                        .child(div().flex_1().truncate().child(session.label.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(session.block_count().to_string()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.session_id = Some(id.clone());
                                this.window_id = None;
                                this.active_block = None;
                                this.reload(cx);
                            }),
                        )
                        .into_any_element(),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .w(theme::SIDEBAR_W)
            .h_full()
            .bg(chrome.surface())
            .border_r_1()
            .border_color(theme::border())
            .children(rows)
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

        let blocks = self.blocks();
        let views: Vec<(BlockState, Entity<TerminalView>)> = blocks
            .into_iter()
            .map(|block| {
                let view = self.terminal(&server_id, &session_id, &block.id, cx);
                (block, view)
            })
            .collect();

        // Keep *something* focused. Closing a pane takes its focus handle with
        // it, and GPUI dispatches keys along the focused element's path: with
        // nothing focused even the shell's own shortcuts stop arriving. So a
        // freshly split pane takes focus, and if focus has gone missing it goes
        // to the active pane, or the first one left.
        let focused_somewhere = views
            .iter()
            .any(|(_, view)| view.read(cx).handle().is_focused(window));
        let wanted = self.pending_focus.take().or_else(|| {
            if focused_somewhere {
                None
            } else {
                self.active_block
                    .clone()
                    .or_else(|| views.first().map(|(block, _)| block.id.clone()))
            }
        });
        if let Some(wanted) = wanted {
            if let Some((_, view)) = views.iter().find(|(block, _)| block.id == wanted) {
                window.focus(&view.read(cx).handle());
            } else {
                self.pending_focus = Some(wanted);
            }
        }

        // The pane with the keyboard owns its terminal's size, so zooming it or
        // opening it from a shortcut reflows the remote side to the new box.
        for (_, view) in &views {
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
                if zoomed.is_some() && Some(&block.id) != zoomed.as_ref() {
                    continue;
                }
                if self.minimized.contains(&block.id) {
                    continue;
                }
                let [x, y, w, h] = if Some(&block.id) == zoomed.as_ref() {
                    [0.0, 0.0, 1.0, 1.0]
                } else {
                    self.rect_of(block)
                };
                let is_active = active.as_deref() == Some(block.id.as_str());
                let outline = if is_active && ring {
                    theme::accent()
                } else {
                    theme::border()
                };
                let block_id = block.id.clone();
                let floating = block.floating;
                let activate_id = block_id.clone();

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
                                        .child(block.display_name()),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                        this.begin_pull(&drag_block, Grab::Move, event.position, cx)
                                    }),
                                ),
                        )
                        .child(view.clone())
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
                } else {
                    div().size_full().child(view.clone())
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
                        cx.listener(move |this, _, _, cx| {
                            this.active_block = Some(activate_id.clone());
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
                        if page == Page::Connection && this.server_info.is_none() {
                            this.refresh_info(cx);
                        }
                        if page == Page::Keys && this.keymap.is_none() {
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
            }
            Page::Connection => {
                let status = self.server_info.as_ref().and_then(|i| i.get("status"));
                let whoami = self.server_info.as_ref().and_then(|i| i.get("whoami"));
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
                                    this.server_info = None;
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
            }
            Page::Keys => {
                rows.push(theme::section("SHORTCUTS").into_any_element());
                if let Some(keymap) = &self.keymap {
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
        if !self.styled {
            self.styled = true;
            crate::win32::round_corners();
        }
        let title_bar = self.title_bar(cx);
        let toasts = self.toast_strip(cx);
        let prompt = self.prompt_overlay();
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
                if this.prompt.is_some() && this.prompt_key(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                if this.show_settings && event.keystroke.key == "escape" {
                    this.close_settings(window, cx);
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
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(title_bar)
            .child(body)
            .child(prompt)
            .child(toasts)
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
