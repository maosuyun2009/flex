//! The window: a sidebar of servers and sessions, a tab bar of windows, and the
//! pane area of the window you picked.
//!
//! Every pane is placed from `BlockState::rect`, which the server sends as
//! fractions of its window, so splits, floats and zoom all come for free.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{
    div, px, relative, rgb, AnyElement, Context, Entity, IntoElement, Render, SharedString, Task,
    Window,
};
use rexshell_engine::{AppState, BlockState, Engine, SessionState, WindowState};

use crate::terminal::TerminalView;

pub struct Workspace {
    engine: Arc<Engine>,
    state: AppState,
    server_id: Option<String>,
    session_id: Option<String>,
    window_id: Option<String>,
    terminals: HashMap<String, Entity<TerminalView>>,
    focused_once: bool,
    _wake: Option<Task<()>>,
}

impl Workspace {
    pub fn new(engine: Arc<Engine>, wake: Receiver<()>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            engine,
            state: AppState::default(),
            server_id: None,
            session_id: None,
            window_id: None,
            terminals: HashMap::new(),
            focused_once: false,
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

    /// Re-read the engine's state and drop selections and terminals that went away.
    fn reload(&mut self, cx: &mut Context<Self>) {
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
        self.terminals.retain(|id, _| live.contains(id));
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
        let view = cx.new(|cx| TerminalView::new(&engine, server_id, session_id, block_id, cx));
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

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = Vec::new();
        for server in &self.state.servers {
            let status = match server.connectivity {
                rexshell_engine::Connectivity::Online => format!(
                    "{} · {}",
                    server.name,
                    server.version.clone().unwrap_or_default()
                ),
                rexshell_engine::Connectivity::Connecting => {
                    format!("{} · connecting", server.name)
                }
                rexshell_engine::Connectivity::Offline { .. } => {
                    format!("{} · offline", server.name)
                }
                rexshell_engine::Connectivity::Idle => server.name.clone(),
            };
            rows.push(
                div()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .child(status)
                    .into_any_element(),
            );

            for session in &server.sessions {
                let id = session.id.clone();
                let selected = self.session_id.as_deref() == Some(id.as_str());
                let label = format!("{}  ({})", session.label, session.block_count());
                rows.push(
                    div()
                        .id(SharedString::from(id.clone()))
                        .px_3()
                        .py_1()
                        .when(selected, |el| el.bg(rgb(0x2a2f38)))
                        .hover(|el| el.bg(rgb(0x22262d)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.session_id = Some(id.clone());
                            this.window_id = None;
                            this.reload(cx);
                        }))
                        .into_any_element(),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .w(px(240.0))
            .h_full()
            .border_r_1()
            .border_color(rgb(0x2a2f38))
            .children(rows)
            .into_any_element()
    }

    fn panes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(session_id) = self.session_id.clone() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child("No session")
                .into_any_element();
        };
        let Some(server_id) = self.server_id.clone() else {
            return div().size_full().into_any_element();
        };

        let selected = self.window_id.clone();
        let tabs: Vec<AnyElement> = self
            .windows()
            .into_iter()
            .map(|window| {
                let id = window.id.clone();
                let active = selected.as_deref() == Some(id.as_str());
                div()
                    .id(SharedString::from(id.clone()))
                    .px_3()
                    .py_1()
                    .when(active, |el| el.bg(rgb(0x2a2f38)))
                    .hover(|el| el.bg(rgb(0x22262d)))
                    .child(window.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.window_id = Some(id.clone());
                        this.focus_window(&id, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        let views: Vec<([f64; 4], Entity<TerminalView>)> = self
            .blocks()
            .into_iter()
            .map(|block| {
                let view = self.terminal(&server_id, &session_id, &block.id, cx);
                (block.rect, view)
            })
            .collect();

        // A window with nothing focused swallows every keystroke, and nothing
        // else here can take focus, so give it to the first pane.
        if !self.focused_once {
            if let Some((_, view)) = views.first() {
                window.focus(&view.read(cx).handle());
                self.focused_once = true;
            }
        }

        let panes: Vec<AnyElement> = views
            .into_iter()
            .map(|(rect, view)| {
                let [x, y, w, h] = rect;
                div()
                    .absolute()
                    .left(relative(x as f32))
                    .top(relative(y as f32))
                    .w(relative(w as f32))
                    .h(relative(h as f32))
                    .child(view)
                    .into_any_element()
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(
                div()
                    .flex()
                    .h(px(30.0))
                    .border_b_1()
                    .border_color(rgb(0x2a2f38))
                    .children(tabs),
            )
            .child(div().relative().flex_1().children(panes))
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.sidebar(cx);
        let panes = self.panes(window, cx);
        div()
            .flex()
            .size_full()
            .bg(rgb(0x14161a))
            .text_color(rgb(0xd8dee9))
            .text_sm()
            .child(sidebar)
            .child(panes)
    }
}
