//! The window: a title bar of our own (GPUI draws no system one on Windows),
//! the session list beside it, tabs for the session's windows, and the panes of
//! the window you picked.
//!
//! Every pane is placed from `BlockState::rect`, which the server sends as
//! fractions of its window, so splits, floats and zoom need no layout code here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{
    div, px, relative, AnyElement, App, Context, Entity, IntoElement, MouseButton, Render,
    SharedString, Task, Window,
};
use rexshell_engine::{AppState, BlockState, Connectivity, Engine, SessionState, WindowState};

use crate::terminal::TerminalView;
use crate::theme;

pub struct Workspace {
    engine: Arc<Engine>,
    state: AppState,
    server_id: Option<String>,
    session_id: Option<String>,
    window_id: Option<String>,
    terminals: HashMap<String, Entity<TerminalView>>,
    focused_once: bool,
    styled: bool,
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
            styled: false,
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

    /// A window of ours, since GPUI draws no system title bar on Windows.
    /// Everything but the buttons drags the window.
    fn title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let maximized = crate::win32::is_maximized();
        let drag =
            |el: gpui::Div| el.on_mouse_down(MouseButton::Left, |_, _, _| crate::win32::drag());

        div()
            .flex()
            .items_center()
            .h(theme::TITLE_H)
            .bg(theme::color(theme::SURFACE))
            .border_b_1()
            .border_color(theme::color(theme::BORDER))
            .child(
                drag(div().flex().items_center().h_full().pl_3().pr_2().gap_2())
                    .child(
                        div()
                            .size(px(8.0))
                            .rounded_sm()
                            .bg(theme::color(theme::ACCENT)),
                    )
                    .child(
                        div()
                            .text_color(theme::color(theme::MUTED))
                            .child("rexshell"),
                    ),
            )
            .child(
                div()
                    .w(px(1.0))
                    .h(px(16.0))
                    .mx_2()
                    .bg(theme::color(theme::BORDER)),
            )
            .children(self.tab_chips(cx))
            .child(
                drag(div().flex_1().h_full())
                    .text_xs()
                    .text_color(theme::color(theme::MUTED)),
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
            .child(div().w(px(6.0)).h_full())
            .into_any_element()
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut rows: Vec<AnyElement> = vec![theme::section("SESSIONS").into_any_element()];
        for server in &self.state.servers {
            let dot = match server.connectivity {
                Connectivity::Online => theme::ACCENT,
                Connectivity::Connecting => theme::MUTED,
                Connectivity::Offline { .. } => theme::DANGER,
                Connectivity::Idle => theme::BORDER,
            };
            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::color(theme::MUTED))
                    .child(div().size(px(6.0)).rounded_full().bg(theme::color(dot)))
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
                            el.bg(theme::color(theme::ACTIVE))
                                .text_color(theme::color(theme::TEXT))
                        })
                        .when(!active, |el| {
                            el.text_color(theme::color(theme::TEXT))
                                .hover(|el| el.bg(theme::color(theme::HOVER)))
                        })
                        .child(div().flex_1().truncate().child(session.label.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::color(theme::MUTED))
                                .child(session.block_count().to_string()),
                        )
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
            .w(theme::SIDEBAR_W)
            .h_full()
            .bg(theme::color(theme::SURFACE))
            .border_r_1()
            .border_color(theme::color(theme::BORDER))
            .children(rows)
            .into_any_element()
    }

    fn tab_chips(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.window_id.clone();
        let chips: Vec<AnyElement> = self
            .windows()
            .into_iter()
            .map(|window| {
                let id = window.id.clone();
                let active = selected.as_deref() == Some(id.as_str());
                div()
                    .id(SharedString::from(id.clone()))
                    .h(px(22.0))
                    .px_3()
                    .rounded_md()
                    .flex()
                    .items_center()
                    .text_xs()
                    .when(active, |el| {
                        el.bg(theme::color(theme::ACTIVE))
                            .text_color(theme::color(theme::TEXT))
                    })
                    .when(!active, |el| {
                        el.text_color(theme::color(theme::MUTED)).hover(|el| {
                            el.bg(theme::color(theme::HOVER))
                                .text_color(theme::color(theme::TEXT))
                        })
                    })
                    .child(window.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.window_id = Some(id.clone());
                        this.focus_window(&id, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();
        chips
    }

    fn panes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(session_id) = self.session_id.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::color(theme::MUTED))
                .child("Waiting for a session")
                .into_any_element();
        };
        let Some(server_id) = self.server_id.clone() else {
            return div().flex_1().into_any_element();
        };

        let views: Vec<([f64; 4], Entity<TerminalView>, bool)> = self
            .blocks()
            .into_iter()
            .map(|block| {
                let view = self.terminal(&server_id, &session_id, &block.id, cx);
                let focused = view.read(cx).handle().is_focused(window);
                (block.rect, view, focused)
            })
            .collect();

        // A window with nothing focused swallows every keystroke, and nothing
        // else here can take focus, so give it to the first pane.
        if !self.focused_once {
            if let Some((_, view, _)) = views.first() {
                window.focus(&view.read(cx).handle());
                self.focused_once = true;
            }
        }

        let panes: Vec<AnyElement> = views
            .into_iter()
            .map(|(rect, view, focused)| {
                let [x, y, w, h] = rect;
                div()
                    .absolute()
                    .left(relative(x as f32))
                    .top(relative(y as f32))
                    .w(relative(w as f32))
                    .h(relative(h as f32))
                    .border_1()
                    .border_color(theme::color(if focused {
                        theme::ACCENT
                    } else {
                        theme::BORDER
                    }))
                    .child(view)
                    .into_any_element()
            })
            .collect();

        div()
            .relative()
            .flex_1()
            .bg(theme::color(theme::BG))
            .children(panes)
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.styled {
            self.styled = true;
            crate::win32::round_corners();
            if std::env::var_os("REXSHELL_DEBUG").is_some() {
                let b = window.bounds();
                eprintln!("[window] {:?} x{:?}", b.origin, b.size);
            }
        }
        let title_bar = self.title_bar(cx);
        let sidebar = self.sidebar(cx);
        let panes = self.panes(window, cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::color(theme::BG))
            .text_color(theme::color(theme::TEXT))
            .text_sm()
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(sidebar)
                    .child(div().flex().flex_col().flex_1().min_w(px(0.0)).child(panes)),
            )
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
    let stroke = theme::color(theme::MUTED);
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
        .text_color(theme::color(theme::MUTED))
        .hover(move |el| {
            let el = el.bg(theme::color(if danger {
                theme::DANGER
            } else {
                theme::HOVER
            }));
            if danger {
                el.text_color(theme::color(theme::TEXT))
            } else {
                el
            }
        })
        .child(shape(glyph))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| action(window, cx))
        .into_any_element()
}
