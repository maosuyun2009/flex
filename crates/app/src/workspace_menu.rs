//! Workspace-only menu state; targets never follow the current selection.
use super::Ask;
use flex_engine::{AppState, BlockState};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub server: String,
    pub address: String,
    pub session: Option<String>,
    pub block: Option<String>,
}

impl Target {
    pub fn valid(&self, state: &AppState) -> bool {
        let Some(server) = state
            .server(&self.server)
            .filter(|s| s.address == self.address)
        else {
            return false;
        };
        let Some(session) = &self.session else {
            return self.block.is_none();
        };
        let Some(session) = server.sessions.iter().find(|s| s.id == *session) else {
            return false;
        };
        self.block.as_ref().is_none_or(|id| {
            session
                .windows
                .iter()
                .any(|w| w.blocks.iter().any(|b| b.id == *id))
        })
    }
    pub fn block<'a>(&self, state: &'a AppState) -> Option<&'a BlockState> {
        if !self.valid(state) {
            return None;
        }
        state
            .server(&self.server)?
            .sessions
            .iter()
            .find(|s| Some(&s.id) == self.session.as_ref())?
            .windows
            .iter()
            .flat_map(|w| &w.blocks)
            .find(|b| Some(&b.id) == self.block.as_ref())
    }
}

#[derive(Clone)]
pub(super) enum Action {
    Prompt(Ask, String),
    Directory,
    Connect(String, String),
    Reconnect,
    Move(i32),
    Discover,
    Control(&'static str),
    RestoreLocal,
    /// This machine's WSL server: start it if need be, then connect.
    OpenWsl,
}

#[derive(Clone)]
pub(super) struct Item {
    pub label: String,
    pub action: Option<Action>,
}
impl Item {
    pub fn enabled(label: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            action: Some(action),
        }
    }
    pub fn disabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            action: None,
        }
    }
}

pub(super) struct Menu {
    pub title: String,
    pub target: Option<Target>,
    pub directory: bool,
    pub items: Vec<Item>,
    pub cursor: Option<usize>,
    pub anchor: Option<gpui::Point<gpui::Pixels>>,
    pub scroll: gpui::ScrollHandle,
}
impl Menu {
    pub fn new(title: impl Into<String>, target: Option<Target>, items: Vec<Item>) -> Self {
        let cursor = items.iter().position(|item| item.action.is_some());
        Self {
            title: title.into(),
            target,
            directory: false,
            items,
            cursor,
            anchor: None,
            scroll: gpui::ScrollHandle::new(),
        }
    }
    pub fn step(&mut self, direction: i32) {
        let count = self.items.len();
        if count == 0 {
            return;
        }
        let start = self
            .cursor
            .unwrap_or(if direction > 0 { count - 1 } else { 0 });
        self.cursor = (1..=count)
            .map(|offset| {
                (start as i64 + i64::from(direction) * offset as i64).rem_euclid(count as i64)
                    as usize
            })
            .find(|&at| self.items[at].action.is_some());
        if let Some(cursor) = self.cursor {
            self.scroll.scroll_to_item(cursor);
        }
    }
    pub fn replace_directory(&mut self, items: Vec<Item>) {
        let endpoint = |item: &Item| match &item.action {
            Some(Action::Connect(_, address)) => shrew_core::Endpoint::parse(address).ok(),
            _ => None,
        };
        let selected = self
            .cursor
            .and_then(|at| self.items.get(at))
            .and_then(endpoint);
        self.cursor = selected
            .and_then(|selected| {
                items
                    .iter()
                    .position(|item| endpoint(item).as_ref() == Some(&selected))
            })
            .or_else(|| items.iter().position(|item| item.action.is_some()));
        self.items = items;
        if let Some(cursor) = self.cursor {
            self.scroll.scroll_to_item(cursor);
        }
    }

    pub fn chosen(&self) -> Option<(Option<Target>, Action)> {
        Some((
            self.target.clone(),
            self.items.get(self.cursor?)?.action.clone()?,
        ))
    }
}

pub(super) fn directory_entries(
    hosts: &serde_json::Value,
    saved: &[String],
) -> Vec<(String, String)> {
    let mut seen: std::collections::HashSet<_> = saved
        .iter()
        .filter_map(|address| shrew_core::Endpoint::parse(address).ok())
        .collect();
    hosts
        .get("hosts")
        .unwrap_or(hosts)
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|host| {
            let label = host
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("Server");
            host.get("endpoints")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(move |address| {
                    address
                        .as_str()
                        .map(|address| (label.to_owned(), address.trim().to_owned()))
                })
        })
        .filter(|(_, address)| {
            shrew_core::Endpoint::parse(address)
                .ok()
                .is_some_and(|endpoint| seen.insert(endpoint))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flex_engine::{ServerState, SessionState, WindowState};
    use serde_json::json;

    fn target() -> Target {
        Target {
            server: "source".into(),
            address: "source:7878".into(),
            session: Some("session".into()),
            block: Some("pane".into()),
        }
    }
    fn state() -> AppState {
        AppState {
            servers: vec![ServerState {
                id: "source".into(),
                address: "source:7878".into(),
                sessions: vec![SessionState {
                    id: "session".into(),
                    windows: vec![WindowState {
                        blocks: vec![BlockState {
                            id: "pane".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn menu_keeps_original_target_without_selecting_another_row() {
        let selected_server = "other";
        let menu = Menu::new(
            "Pane",
            Some(target()),
            vec![Item::enabled("Take now", Action::Control("take"))],
        );
        let (captured, _) = menu.chosen().unwrap();
        assert_eq!(captured, Some(target()));
        assert_ne!(captured.unwrap().server, selected_server);
    }

    #[test]
    fn keyboard_wraps_skipping_disabled_items_and_empty_menus() {
        let mut menu = Menu::new(
            "Session",
            None,
            vec![
                Item::disabled("Move up"),
                Item::enabled("Edit", Action::Reconnect),
                Item::disabled("Move down"),
                Item::enabled("Delete", Action::Reconnect),
            ],
        );
        assert_eq!(menu.cursor, Some(1));
        menu.step(1);
        assert_eq!(menu.cursor, Some(3));
        menu.step(1);
        assert_eq!(menu.cursor, Some(1));
        menu.step(-1);
        assert_eq!(menu.cursor, Some(3));
        let mut empty = Menu::new("Empty", None, vec![Item::disabled("Loading…")]);
        empty.step(1);
        assert!(empty.chosen().is_none());
    }

    #[test]
    fn directory_filters_equivalent_saved_blank_invalid_and_duplicate_endpoints() {
        let hosts = json!({"hosts":[
            {"label":"Saved", "endpoints":[" http://saved:7878/ "]},
            {"label":"Available", "endpoints":["", " ", "ftp://bad", "new:7878", "http://new:7878/", "https://second"]}
        ]});
        assert_eq!(
            directory_entries(&hosts, &["saved".into()]),
            vec![
                ("Available".into(), "new:7878".into()),
                ("Available".into(), "https://second".into())
            ]
        );
        assert_eq!(
            directory_entries(&json!([{"label":"Bare", "endpoints":["bare"]}]), &[]),
            vec![("Bare".into(), "bare".into())]
        );
    }

    #[test]
    fn directory_refresh_preserves_selected_endpoint_across_reordering_and_renaming() {
        let connect = |label: &str, address: &str| {
            Item::enabled(label, Action::Connect(label.into(), address.into()))
        };
        let mut menu = Menu::new(
            "Directory",
            None,
            vec![connect("A", "first"), connect("B", "second")],
        );
        menu.step(1);
        menu.replace_directory(vec![
            connect("New", "new"),
            connect("A", "first"),
            connect("Renamed", "http://second:7878/"),
        ]);
        assert_eq!(menu.cursor, Some(2));
        let (_, Action::Connect(label, _)) = menu.chosen().unwrap() else {
            panic!("connection expected")
        };
        assert_eq!(label, "Renamed");
        menu.replace_directory(vec![Item::disabled("Unavailable"), connect("A", "first")]);
        assert_eq!(menu.cursor, Some(1));
        menu.replace_directory(vec![Item::disabled("No new servers")]);
        assert!(menu.chosen().is_none());
    }

    #[test]
    fn target_expires_if_server_address_session_or_block_disappears() {
        let target = target();
        let original = state();
        assert!(target.valid(&original));
        let mut changed = original.clone();
        changed.servers[0].address = "replacement:7878".into();
        assert!(!target.valid(&changed));
        let mut changed = original.clone();
        changed.servers[0].sessions[0].windows[0].blocks.clear();
        assert!(!target.valid(&changed));
        let mut changed = original.clone();
        changed.servers[0].sessions.clear();
        assert!(!target.valid(&changed));
        assert!(!target.valid(&AppState::default()));
    }
}

use super::Workspace;
use crate::theme;
use gpui::prelude::*;
use gpui::{div, hsla, px, AnyElement, Context, KeyDownEvent, MouseButton, SharedString, Window};

impl Workspace {
    fn show_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = Some(menu);
        self.prefix
            .store(false, std::sync::atomic::Ordering::Relaxed);
        window.focus(&self.focus);
        cx.notify();
    }

    pub(super) fn open_add_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .server_id
            .as_ref()
            .and_then(|id| self.state.server(id))
            .map(|server| Target {
                server: server.id.clone(),
                address: server.address.clone(),
                session: None,
                block: None,
            });
        self.show_menu(
            Menu::new(
                "Add server",
                target,
                vec![
                    Item::enabled(
                        "Enter address…",
                        Action::Prompt(Ask::AddServer, "Add server — name and address".into()),
                    ),
                    Item::enabled("From server directory…", Action::Directory),
                    Item::enabled("This machine's WSL…", Action::OpenWsl),
                ],
            ),
            window,
            cx,
        );
    }

    /// Connect to this machine's WSL server, starting it when it is not up yet.
    ///
    /// The work runs off the UI thread: `wsl.exe` costs a second or two.
    fn open_wsl(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async { crate::wsl::connect() })
                .await;
            this.update(cx, |me, cx| match found {
                Ok(address) => me.connect_wsl(address, cx),
                Err(e) => me.rpc_error("Open WSL", e, cx),
            })
            .ok();
        })
        .detach();
    }

    /// A saved WSL entry is a promise this shell can keep: when nothing answers
    /// on its port, offer to start the server. Every other address is left
    /// untouched — there is no server on it this shell could start.
    pub(super) fn revive_wsl(&mut self, server_id: &str, window: &mut Window) {
        let Some(server) = self.state.server(server_id) else {
            return;
        };
        // Starting is not always enough: rex gives the port only to the server
        // it launches, so a running one may have to be restarted, which ends the
        // programs in its sessions. Ask before that.
        match crate::wsl::click(&server.address, crate::wsl::listening()) {
            None => {}
            Some(crate::wsl::Click::Reconnect) => self.engine.reconnect(server_id),
            Some(crate::wsl::Click::Ask) => self.open_prompt(
                Ask::StartWsl(server_id.to_owned()),
                "Connect to WSL — a server already running without the TCP port is restarted, and that ends the programs in its sessions",
                window,
            ),
        }
    }

    /// Show the saved entry for `address`, or add it, and select it.
    fn connect_wsl(&mut self, address: String, cx: &mut Context<Self>) {
        self.state = self.engine.state();
        let saved = self
            .state
            .servers
            .iter()
            .find(|s| s.address.trim() == address)
            .map(|s| s.id.clone());
        let id = match saved {
            Some(id) => id,
            None => match self.engine.add_server("WSL", &address) {
                Ok(id) => id,
                Err(e) => return self.rpc_error("Add server", e, cx),
            },
        };
        self.state = self.engine.state();
        self.server_id = Some(id);
        self.session_id = None;
        self.window_id = None;
        self.active_block = None;
        self.reload(cx);
    }

    pub(super) fn menu_button(
        &self,
        id: String,
        target: Target,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chrome = self.chrome();
        div()
            .id(SharedString::from(id))
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .text_color(theme::muted())
            .hover(|el| el.bg(chrome.hover()).text_color(theme::text()))
            .child("⋯")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    this.open_target_menu(target.clone(), window, cx);
                    if let Some(menu) = &mut this.menu {
                        menu.anchor = Some(event.position());
                    }
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }

    fn open_target_menu(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if !target.valid(&self.state) {
            return;
        }
        let server = self.state.server(&target.server).unwrap();
        let prompt = |label: &str, ask: Ask, question: String| {
            Item::enabled(label, Action::Prompt(ask, question))
        };
        let (title, items) = if let Some(block) = target.block(&self.state) {
            let local = self.server_id.as_ref() == Some(&target.server)
                && self.session_id == target.session
                && self.is_local_float(&block.id);
            let mut items = Vec::new();
            if local {
                items.push(Item::disabled(
                    "Floating locally — other clients keep their layout",
                ));
                items.push(Item::enabled("Restore pane", Action::RestoreLocal));
            }
            if block.is_terminal() {
                items.push(Item::disabled(format!(
                    "Terminal size — owner: {}",
                    if block.owned_by_me {
                        "this client"
                    } else {
                        block.owner.as_deref().unwrap_or("none")
                    }
                )));
                items.push(Item::disabled("Dimensions only; input remains available"));
                items.push(Item::enabled(
                    "Request when free",
                    Action::Control("request"),
                ));
                items.push(Item::enabled("Take now", Action::Control("take")));
                items.push(Item::enabled("Release", Action::Control("release")));
            }
            if block.floating && !local {
                items.push(prompt(
                    "Close floating layer…",
                    Ask::CloseLayer(
                        target.server.clone(),
                        target.session.clone().unwrap(),
                        block.id.clone(),
                    ),
                    "Close floating layer and all its running blocks?".into(),
                ));
            }
            if items.is_empty() {
                items.push(Item::disabled("No pane actions available"));
            }
            (block.display_name(), items)
        } else if let Some(id) = &target.session {
            let session = server.sessions.iter().find(|s| s.id == *id).unwrap();
            let ids: Vec<_> = server.sessions.iter().map(|s| s.id.clone()).collect();
            let mut items = vec![prompt(
                "Edit session name…",
                Ask::RenameSession(server.id.clone(), id.clone(), session.label.clone()),
                "Rename session".into(),
            )];
            for (step, label) in [(-1, "Move up"), (1, "Move down")] {
                items.push(if super::move_before(&ids, id, step).is_some() {
                    Item::enabled(label, Action::Move(step))
                } else {
                    Item::disabled(label)
                });
            }
            items.push(Item::enabled("Discover parked panes", Action::Discover));
            items.push(prompt(
                "Delete session…",
                Ask::DeleteSession(server.id.clone(), id.clone(), session.label.clone()),
                format!("Delete session {}?", session.label),
            ));
            (session.label.clone(), items)
        } else {
            (
                server.name.clone(),
                vec![
                    prompt(
                        "New session…",
                        Ask::NewSession(server.address.clone()),
                        "New session — name it".into(),
                    ),
                    prompt(
                        "Edit server…",
                        Ask::EditServer(
                            server.id.clone(),
                            server.name.clone(),
                            server.address.clone(),
                        ),
                        "Edit server — name and address".into(),
                    ),
                    Item::enabled("Reconnect", Action::Reconnect),
                    prompt(
                        "Remove saved connection…",
                        Ask::DeleteServer(server.id.clone(), server.name.clone()),
                        format!(
                            "Remove saved connection {}? Remote server keeps running.",
                            server.name
                        ),
                    ),
                ],
            )
        };
        self.show_menu(Menu::new(title, Some(target), items), window, cx);
    }

    fn directory_items(&self, target: Option<&Target>) -> Vec<Item> {
        let Some(target) = target else {
            return vec![Item::disabled("No source server — enter an address first")];
        };
        if !target.valid(&self.state) {
            return vec![Item::disabled("Source server is no longer available")];
        }
        let Some(hosts) = self.hosts.get(Some(&target.server)) else {
            return vec![Item::disabled("Loading server directory…")];
        };
        let saved: Vec<_> = self
            .state
            .servers
            .iter()
            .map(|s| s.address.clone())
            .collect();
        let entries = directory_entries(hosts, &saved);
        if entries.is_empty() {
            return vec![Item::disabled("No new valid endpoints in this directory")];
        }
        entries
            .into_iter()
            .map(|(label, endpoint)| {
                Item::enabled(
                    format!("{label} — {endpoint}"),
                    Action::Connect(label, endpoint),
                )
            })
            .collect()
    }

    pub(super) fn update_directory_menu(&mut self, server: &str) {
        if let Some(menu) = &self.menu {
            if !menu.directory || menu.target.as_ref().is_none_or(|t| t.server != server) {
                return;
            }
            let items = self.directory_items(menu.target.as_ref());
            let menu = self.menu.as_mut().unwrap();
            menu.replace_directory(items);
        }
    }

    pub(super) fn directory_failed(&mut self, server: &str) {
        if let Some(menu) = &mut self.menu {
            if menu.directory && menu.target.as_ref().is_some_and(|t| t.server == server) {
                menu.items = vec![Item::disabled("Directory unavailable — close and retry")];
                menu.cursor = None;
            }
        }
    }

    fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.pending_focus = self.active_block.clone();
        window.focus(&self.focus);
        cx.notify();
    }

    fn choose_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((target, action)) = self.menu.as_ref().and_then(Menu::chosen) else {
            return;
        };
        let anchor = self.menu.as_ref().and_then(|menu| menu.anchor);
        self.close_menu(window, cx);
        // Address entry is local and does not depend on a directory's source.
        let needs_target = !matches!(
            action,
            Action::Prompt(Ask::AddServer, _) | Action::Directory | Action::OpenWsl
        );
        if needs_target
            && target
                .as_ref()
                .is_some_and(|t| !t.valid(&self.engine.state()))
        {
            self.rpc_error(
                "Menu action",
                "The original target is no longer available",
                cx,
            );
            return;
        }
        match action {
            Action::Prompt(ask, question) => {
                let local = matches!(ask, Ask::AddServer);
                self.open_prompt(ask, &question, window);
                self.prompt.as_mut().unwrap().scope = if local { None } else { target };
            }
            Action::Directory => {
                let title = target
                    .as_ref()
                    .map(|t| format!("Server directory — {}", t.address))
                    .unwrap_or_else(|| "Server directory".into());
                let items = self.directory_items(target.as_ref());
                let mut menu = Menu::new(title, target.clone(), items);
                menu.directory = true;
                menu.anchor = anchor;
                self.show_menu(menu, window, cx);
                if let Some(target) = target.filter(|t| t.valid(&self.state)) {
                    self.refresh_hosts_for(target.address, target.server, cx);
                }
            }
            Action::OpenWsl => self.open_wsl(cx),
            Action::Connect(name, endpoint) => {
                // Revalidate against the latest saved list: another request may have added it.
                let saved: Vec<_> = self
                    .engine
                    .state()
                    .servers
                    .into_iter()
                    .map(|s| s.address)
                    .collect();
                if directory_entries(
                    &serde_json::json!([{"label":name,"endpoints":[endpoint]}]),
                    &saved,
                )
                .is_empty()
                {
                    return;
                }
                match self.engine.add_server(&name, &endpoint) {
                    Ok(id) => {
                        self.server_id = Some(id);
                        self.session_id = None;
                        self.window_id = None;
                        self.active_block = None;
                        self.reload(cx);
                    }
                    Err(e) => self.rpc_error("Add server", e, cx),
                }
            }
            Action::Reconnect => {
                if let Some(target) = target {
                    self.engine.reconnect(&target.server);
                    self.revive_wsl(&target.server, window);
                }
            }
            Action::Move(step) => {
                if let Some(target) = target {
                    if let Some(session) = target.session {
                        self.move_session(target.server, session, step, cx);
                    }
                }
            }
            Action::Discover => {
                if let Some(target) = target {
                    if let Some(session) = target.session {
                        if self.server_id.as_ref() != Some(&target.server)
                            || self.session_id.as_ref() != Some(&session)
                        {
                            self.server_id = Some(target.server);
                            self.session_id = Some(session);
                            self.window_id = None;
                            self.active_block = None;
                            // reload discovers parked panes when the selected server/session changes.
                            self.reload(cx);
                        } else {
                            self.refresh_parked_for(target.address, target.server, session, cx);
                        }
                    }
                }
            }
            Action::RestoreLocal => {
                if let Some(target) = target {
                    if self.server_id.as_ref() == Some(&target.server)
                        && self.session_id == target.session
                    {
                        if let Some(block) = target.block {
                            if let Some(key) = self.local_float_key(&block) {
                                self.local_floats.0.remove(&key);
                                self.pending_focus = Some(block);
                            }
                        }
                    }
                }
            }
            Action::Control(mode) => {
                if let Some(target) = target {
                    // Terminal entities are keyed by block id for the visible server/session only.
                    if self.server_id.as_ref() != Some(&target.server)
                        || self.session_id != target.session
                    {
                        return;
                    }
                    if let Some(block) = target.block(&self.state).filter(|b| b.is_terminal()) {
                        if let Some(view) = self.terminals.get(&block.id).cloned() {
                            view.update(cx, |terminal, _| match mode {
                                "request" => terminal.request_control(),
                                "release" => terminal.release_control(),
                                _ => terminal.force_control(),
                            });
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    pub(super) fn menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_menu(window, cx),
            "up" => {
                if let Some(menu) = &mut self.menu {
                    menu.step(-1);
                }
            }
            "down" => {
                if let Some(menu) = &mut self.menu {
                    menu.step(1);
                }
            }
            "enter" => self.choose_menu(window, cx),
            _ => {} // All text and shortcuts belong to the menu, never the PTY.
        }
        cx.notify();
    }

    pub(super) fn menu_overlay(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = &self.menu else {
            return div().into_any_element();
        };
        let chrome = self.chrome();
        let viewport = window.viewport_size();
        let width = (if menu.directory { 440.0_f32 } else { 300.0_f32 })
            .min(f32::from(viewport.width) - 16.0)
            .max(0.0);
        let max_height = (f32::from(viewport.height) - 16.0).max(0.0);
        let estimated_height = (76.0 + menu.items.len() as f32 * 52.0).min(max_height);
        let anchor = menu.anchor.unwrap_or(gpui::point(px(8.0), px(40.0)));
        let left = f32::from(anchor.x)
            .min((f32::from(viewport.width) - width - 8.0).max(8.0))
            .max(8.0);
        let top = f32::from(anchor.y)
            .min((f32::from(viewport.height) - estimated_height - 8.0).max(8.0))
            .max(8.0);
        let rows = menu
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let enabled = item.action.is_some();
                div()
                    .id(SharedString::from(format!("context-item:{index}")))
                    .min_h(px(28.0))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .when(menu.cursor == Some(index), |el| el.bg(chrome.active()))
                    .when(enabled, |el| el.hover(|el| el.bg(chrome.hover())))
                    .text_color(if enabled {
                        theme::text()
                    } else {
                        theme::muted()
                    })
                    .child(item.label.clone())
                    .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            if enabled {
                                if let Some(menu) = &mut this.menu {
                                    menu.cursor = Some(index);
                                }
                                this.choose_menu(window, cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        div()
            .id("menu-overlay")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size_full()
            .occlude()
            .bg(hsla(0.0, 0.0, 0.0, 0.1))
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .map(|mut el| {
                el.interactivity().on_any_mouse_up(cx.listener(
                    |this, event: &gpui::MouseUpEvent, window, cx| {
                        if event.button == MouseButton::Left {
                            this.close_menu(window, cx);
                        }
                        cx.stop_propagation();
                    },
                ));
                el
            })
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("context-menu")
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(width))
                    .max_h(px(max_height))
                    .p_2()
                    .bg(theme::color(theme::SURFACE))
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(menu.title.clone()),
                    )
                    .child(
                        div()
                            .id("context-scroll")
                            .min_h(px(0.0))
                            .flex_shrink()
                            .overflow_y_scroll()
                            .track_scroll(&menu.scroll)
                            .children(rows),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("↑↓ to choose · Enter · Esc to close"),
                    ),
            )
            .into_any_element()
    }
}
