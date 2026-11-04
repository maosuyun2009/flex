//! The window: a title bar of our own (GPUI draws no system one on Windows),
//! the session list beside it, tabs for the session's windows, the panes of the
//! window you picked, and the settings page over them.
//!
//! Every pane is placed from `BlockState::rect`, which the server sends as
//! fractions of its window, so splits, floats and zoom need no layout code here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_channel::Receiver;
use gpui::prelude::*;
use gpui::{
    canvas, div, hsla, linear_color_stop, linear_gradient, px, relative, AnyElement, App, Context,
    Entity, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Render, SharedString, Size, Task, Window,
};
use rexshell_engine::{AppState, BlockState, Connectivity, Engine, SessionState, WindowState};
use serde_json::json;

use crate::icons::{self, Icon};
use crate::settings::{Settings, FONT_MAX, FONT_MIN};
use crate::shortcuts::{self, Action};
use crate::terminal::TerminalView;
use crate::theme;

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
    grab: Grab,
    /// Pointer position in window coordinates when the drag started.
    start: Point<Pixels>,
    /// The layer's rect at that moment, as window fractions.
    rect: [f64; 4],
    /// Where the pane should be drawn now.
    live: [f64; 4],
}

pub struct Workspace {
    engine: Arc<Engine>,
    state: AppState,
    settings: Settings,
    /// The terminal font, resolved once at startup from what is installed.
    font: SharedString,
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
    pull: Option<Pull>,
    /// Keeps a released pane where it was dropped until the server confirms.
    settled: Option<(String, [f64; 4])>,
    _wake: Option<Task<()>>,
}

impl Workspace {
    pub fn new(engine: Arc<Engine>, wake: Receiver<()>, cx: &mut Context<Self>) -> Self {
        let settings = Settings::load(&engine);
        let font = crate::fonts::resolve(cx);
        if std::env::var_os("REXSHELL_DEBUG").is_some() {
            eprintln!("[font] using {font}");
        }
        let mut this = Self {
            engine,
            state: AppState::default(),
            settings,
            font,
            server_id: None,
            session_id: None,
            window_id: None,
            active_block: None,
            pending_focus: None,
            terminals: HashMap::new(),
            styled: false,
            show_settings: false,
            pull: None,
            settled: None,
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
        let view = cx.new(|cx| {
            TerminalView::new(
                &engine, server_id, session_id, block_id, font, font_size, cx,
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

    // ---- actions -----------------------------------------------------------

    fn run(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            Action::SplitRight => self.split(true, cx),
            Action::SplitDown => self.split(false, cx),
            Action::ClosePane => self.close_pane(cx),
            Action::ToggleFloat => self.toggle_float(cx),
            Action::NewTab => self.new_tab(cx),
            Action::ToggleSidebar => {
                self.settings.sidebar = !self.settings.sidebar;
                self.settings.save(&self.engine);
                cx.notify();
            }
            Action::Settings => {
                self.show_settings = !self.show_settings;
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
        cx.spawn(async move |this, cx| {
            match engine
                .shrew()
                .split(&server_id, &session_id, &block_id, vertical, None)
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
                Err(e) => eprintln!("rexshell: split: {e}"),
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
                eprintln!("rexshell: close: {e}");
            }
        })
        .detach();
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
                eprintln!("rexshell: new tab: {e}");
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
        // Tiling again needs a pane to place this one beside.
        let anchor = self
            .blocks()
            .into_iter()
            .find(|b| !b.floating && b.id != block.id)
            .map(|b| b.id);
        let engine = self.engine.clone();
        let block_id = block.id;
        let floating = block.floating;
        cx.spawn(async move |this, cx| {
            let (method, payload) = if floating {
                let Some(anchor) = anchor else { return };
                (
                    "session.move_block",
                    json!({
                        "session_id": session_id,
                        "block_id": block_id,
                        "anchor_block_id": anchor,
                        "side": "after",
                        "direction": "horizontal",
                        "ratio": 0.5,
                    }),
                )
            } else {
                (
                    "session.float_block",
                    json!({
                        "session_id": session_id,
                        "block_id": block_id,
                        "bounds": FLOAT_BOUNDS,
                    }),
                )
            };
            if let Err(e) = engine.call(&address, method, payload).await {
                eprintln!("rexshell: {method}: {e}");
            }
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
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
                    eprintln!("rexshell: session.view: {e}");
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
                eprintln!("rexshell: session.resize_layer: {e}");
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
        let drag =
            |el: gpui::Div| el.on_mouse_down(MouseButton::Left, |_, _, _| crate::win32::drag());
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
                drag(div().flex().items_center().h_full().pl_1().pr_2().gap_2())
                    .child(div().size(px(8.0)).rounded_sm().bg(theme::accent()))
                    .child(div().text_color(theme::muted()).child("rexshell")),
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
                icons::button("split-right", Icon::SplitRight, false).on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::SplitRight, window, cx)),
                ),
            )
            .child(
                icons::button("split-down", Icon::SplitDown, false).on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::SplitDown, window, cx)),
                ),
            )
            .child(
                icons::button(
                    "float",
                    Icon::Float,
                    self.active().map(|b| b.floating).unwrap_or(false),
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.run(Action::ToggleFloat, window, cx)),
                ),
            )
            .child(icons::button("close-pane", Icon::Close, false).on_click(
                cx.listener(|this, _, window, cx| this.run(Action::ClosePane, window, cx)),
            ))
            .child(drag(div().flex_1().h_full()))
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

    fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let chrome = self.chrome();
        let mut rows: Vec<AnyElement> = vec![theme::section("SESSIONS").into_any_element()];
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.session_id = Some(id.clone());
                            this.window_id = None;
                            this.active_block = None;
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
            .bg(chrome.surface())
            .border_r_1()
            .border_color(theme::border())
            .children(rows)
            .into_any_element()
    }

    fn tab_chips(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let selected = self.window_id.clone();
        let chrome = self.chrome();
        self.windows()
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
                        el.bg(chrome.active()).text_color(theme::text())
                    })
                    .when(!active, |el| {
                        el.text_color(theme::muted())
                            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
                    })
                    .when_some(window_agent(&window), |el, state| {
                        el.child(theme::dot(theme::agent(state)))
                    })
                    .child(window.label.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.window_id = Some(id.clone());
                        this.active_block = None;
                        this.focus_window(&id, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect()
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

        let active = self.active_block.clone();
        let chrome = self.chrome();
        let mut elements: Vec<AnyElement> = Vec::with_capacity(views.len());
        // Floating layers paint last, so they sit above the tiled panes.
        for floating_pass in [false, true] {
            for (block, view) in views.iter().filter(|(b, _)| b.floating == floating_pass) {
                let [x, y, w, h] = self.rect_of(block);
                let is_active = active.as_deref() == Some(block.id.as_str());
                let outline = if is_active {
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
                    .child(body)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.active_block = Some(activate_id.clone());
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
        // opaque layer here would flatten the glass.
        let mut area = div().relative().flex_1().children(elements);

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

    fn settings_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
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

        let stepper = |cx: &mut Context<Self>, delta: f32| {
            let glyph = if delta > 0.0 { "+" } else { "−" };
            div()
                .id(if delta > 0.0 { "font-up" } else { "font-down" })
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
        let acrylic = toggle("toggle-acrylic", self.settings.acrylic, cx, |s| {
            &mut s.acrylic
        });
        let sidebar = toggle("toggle-sidebar-setting", self.settings.sidebar, cx, |s| {
            &mut s.sidebar
        });
        let down = stepper(cx, -1.0);
        let up = stepper(cx, 1.0);
        let font = self.settings.font_size;

        let shortcuts: Vec<AnyElement> = shortcuts::BINDINGS
            .iter()
            .map(|(keys, _, label)| {
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
            })
            .collect();

        div()
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .bg(hsla(0.0, 0.0, 0.0, 0.55))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .w(px(460.0))
                    .rounded_lg()
                    // Opaque on purpose: the chrome is translucent, but a dialog
                    // with a terminal bleeding through it is unreadable.
                    .bg(theme::color(theme::SURFACE))
                    .border_1()
                    .border_color(theme::border())
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(theme::border())
                            .child(div().text_color(theme::text()).child("Settings"))
                            .child(
                                div()
                                    .id("settings-close")
                                    .size(px(22.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .text_color(theme::muted())
                                    .hover(|el| el.bg(theme::hover()).text_color(theme::text()))
                                    .child("×")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_settings = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(row(
                        "Translucent",
                        "Layer the chrome over a gradient instead of flat colours".into(),
                        acrylic,
                    ))
                    .child(row("Sidebar", "Show the session list".into(), sidebar))
                    .child(row(
                        "Font size",
                        format!("{font:.0} pt"),
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(down)
                            .child(up)
                            .into_any_element(),
                    ))
                    .child(theme::section("FONT"))
                    .child(row(
                        "Terminal",
                        self.font.to_string(),
                        div().into_any_element(),
                    ))
                    .child(theme::section("SHORTCUTS"))
                    .children(shortcuts)
                    .child(div().h(px(10.0))),
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
        let panes = self.panes(window, cx);
        let sidebar = if self.settings.sidebar {
            Some(self.sidebar(cx))
        } else {
            None
        };
        let settings = if self.show_settings {
            Some(self.settings_panel(cx))
        } else {
            None
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
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.show_settings && event.keystroke.key == "escape" {
                    this.show_settings = false;
                    cx.notify();
                    return;
                }
                if let Some(action) = shortcuts::lookup(&event.keystroke) {
                    this.run(action, window, cx);
                }
            }))
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .children(sidebar)
                    .child(div().flex().flex_col().flex_1().min_w(px(0.0)).child(panes)),
            )
            .children(settings)
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
