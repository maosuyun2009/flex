//! A terminal a UI is showing: its stream, its emulator, and its input queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::json;
use shrew_client::{DataEvent, DataStream, KeyPress, Mods};
use shrew_term::{Frame, Rgb, Terminal};
use tokio::sync::{mpsc, watch};

use crate::server::ServerEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStatus {
    Connecting,
    Live,
    Reconnecting,
    /// The program exited; the block is still open.
    Exited {
        code: i32,
    },
    /// The block closed or the stream can't continue.
    Ended {
        reason: String,
    },
}

pub trait TerminalListener: Send + Sync {
    /// New content is ready; call [`TerminalSession::frame`] on the next display refresh.
    fn frame_ready(&self);
    fn status_changed(&self, status: TerminalStatus);
    /// A program copied text (OSC 52) or the user finished a selection remotely.
    fn clipboard(&self, text: String);
    /// The remote terminal size changed (cols, rows).
    fn size_changed(&self, cols: u16, rows: u16);
    /// Client-side effects requested by an ordered configured server action.
    fn client_actions(&self, _actions: Vec<(String, serde_json::Value)>) {}
}

/// Named keys a touch UI or hardware keyboard can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
    /// A character key, e.g. `Char('c')` with ctrl for ^C.
    Char(char),
}

impl Key {
    fn press(self, mods: Mods) -> KeyPress {
        let (code, text): (String, String) = match self {
            Key::Enter => ("enter".into(), "\r".into()),
            Key::Tab => ("tab".into(), "\t".into()),
            Key::Backspace => ("backspace".into(), String::new()),
            Key::Escape => ("escape".into(), String::new()),
            Key::Up => ("arrow_up".into(), String::new()),
            Key::Down => ("arrow_down".into(), String::new()),
            Key::Left => ("arrow_left".into(), String::new()),
            Key::Right => ("arrow_right".into(), String::new()),
            Key::Home => ("home".into(), String::new()),
            Key::End => ("end".into(), String::new()),
            Key::PageUp => ("page_up".into(), String::new()),
            Key::PageDown => ("page_down".into(), String::new()),
            Key::Delete => ("delete".into(), String::new()),
            Key::Insert => ("insert".into(), String::new()),
            Key::F(n) => (format!("f{}", n.clamp(1, 24)), String::new()),
            Key::Char(c) => {
                let text = if mods.ctrl || mods.alt || mods.meta {
                    String::new()
                } else {
                    c.to_string()
                };
                (shrew_client::char_code(c), text)
            }
        };
        let mods = Mods {
            shift: mods.shift || matches!(self, Key::Char(c) if c.is_ascii_uppercase()),
            ..mods
        };
        KeyPress {
            key: code,
            mods,
            text,
        }
    }
}

/// A color theme the user picked in the app. While set, it wins over the
/// server's theme so every terminal looks the same.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTheme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

pub const DEFAULT_FG: Rgb = Rgb::new(0xe6, 0xed, 0xf3);
pub const DEFAULT_BG: Rgb = Rgb::new(0x0d, 0x11, 0x17);
pub const DEFAULT_CURSOR: Rgb = Rgb::new(0x34, 0xd3, 0x99);

fn apply(t: &mut Terminal, th: &ColorTheme) {
    t.set_default_colors(Some(th.foreground), Some(th.background), Some(th.cursor));
    t.set_ansi_palette(Some(&th.ansi));
}

enum Input {
    Text(String),
    Key(KeyPress),
    KeySequence(KeyPress, Vec<String>),
    KeyEvent(KeyPress, String, Vec<String>),
    Action(String, serde_json::Value),
    Event(serde_json::Value),
    Control(Option<bool>, shrew_client::Connection),
    Paste(String),
}

pub struct TerminalSession {
    server: Arc<ServerEntry>,
    pub session_id: String,
    pub block_id: String,
    term: Mutex<Terminal>,
    status: Mutex<TerminalStatus>,
    pending: AtomicBool,
    listener: Arc<dyn TerminalListener>,
    input: mpsc::UnboundedSender<Input>,
    stop: watch::Sender<bool>,
    /// The device's own grid size, for "fit to this screen".
    device: Mutex<Option<(u16, u16, u32, u32)>>,
    /// Bumped on every grid change; a pending resize only fires if it's still current.
    resize_gen: Arc<AtomicU64>,
    control: AtomicBool,
    control_pending: AtomicBool,
    claimed: AtomicBool,
    forced: Mutex<Option<ColorTheme>>,
    /// Methods are called from UI threads, so spawn on the engine's runtime explicitly.
    rt: tokio::runtime::Handle,
}

impl TerminalSession {
    pub(crate) fn open(
        server: Arc<ServerEntry>,
        session_id: String,
        block_id: String,
        listener: Arc<dyn TerminalListener>,
    ) -> Arc<Self> {
        let (input, input_rx) = mpsc::unbounded_channel();
        let mut term = Terminal::new(80, 24);
        term.set_scrollback_limit(if cfg!(any(target_os = "ios", target_os = "android")) {
            4 << 20
        } else {
            16 << 20
        });
        let this = Arc::new(Self {
            server,
            session_id,
            block_id,
            term: Mutex::new(term),
            status: Mutex::new(TerminalStatus::Connecting),
            pending: AtomicBool::new(false),
            listener,
            input,
            stop: watch::channel(false).0,
            device: Mutex::new(None),
            resize_gen: Arc::new(AtomicU64::new(0)),
            control: AtomicBool::new(false),
            control_pending: AtomicBool::new(false),
            claimed: AtomicBool::new(false),
            forced: Mutex::new(None),
            rt: tokio::runtime::Handle::current(),
        });
        this.rt.spawn(this.clone().stream_task());
        this.rt.spawn(this.clone().input_task(input_rx));
        this
    }

    pub fn status(&self) -> TerminalStatus {
        self.status.lock().clone()
    }

    fn set_status(&self, s: TerminalStatus) {
        let mut cur = self.status.lock();
        if *cur != s {
            *cur = s.clone();
            drop(cur);
            self.listener.status_changed(s);
        }
    }

    fn dirty(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.listener.frame_ready();
        }
    }

    /// Build the frame to draw. Clears the "frame ready" latch.
    pub fn frame(&self) -> Frame {
        self.pending.store(false, Ordering::Release);
        self.term.lock().frame()
    }

    pub fn size(&self) -> (u16, u16) {
        self.term.lock().size()
    }

    pub fn title(&self) -> Option<String> {
        self.term.lock().title()
    }

    pub fn close(&self) {
        if self.stop.send_replace(true) {
            return;
        }
        self.resize_gen.fetch_add(1, Ordering::AcqRel);
        self.control_pending.store(false, Ordering::Release);
        self.control.store(false, Ordering::Release);
        self.claimed.store(false, Ordering::Release);
        // A successful request can leave us waiting in the ownership queue.
        // Disown that claim too, on this connection only (never after reconnect).
        if let Some(conn) = self.server.connection() {
            let (sid, bid) = (self.session_id.clone(), self.block_id.clone());
            self.rt.spawn(async move {
                let _ = conn.disown_block(&sid, &bid).await;
            });
        }
    }

    async fn stream_task(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        if *stop.borrow() {
            return;
        }
        tokio::select! {
            biased;
            _ = stop.changed() => {},
            _ = self.run_stream() => {},
        }
    }

    async fn run_stream(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        // Get a ticket (needs a live, attached control connection).
        let ticket = loop {
            if *stop.borrow() {
                return;
            }
            match self.server.wait_connection(Duration::from_secs(30)).await {
                Some(c) => match c.data_ticket(&self.session_id, &self.block_id).await {
                    Ok(t) => break t,
                    Err(e)
                        if e.api_code() == Some("not_found")
                            || e.api_code() == Some("session_not_found") =>
                    {
                        self.set_status(TerminalStatus::Ended {
                            reason: "This terminal no longer exists".into(),
                        });
                        return;
                    }
                    Err(e) => {
                        // Not attached yet (conflict) or a hiccup: try shortly.
                        tracing::debug!("ticket: {e}");
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                },
                None => self.set_status(TerminalStatus::Reconnecting),
            }
        };
        let endpoint = match shrew_client::Endpoint::parse(&self.server.profile.lock().address) {
            Ok(e) => e,
            Err(e) => {
                return self.set_status(TerminalStatus::Ended {
                    reason: e.to_string(),
                });
            }
        };
        let (ds, mut rx) = DataStream::open(endpoint, ticket);
        loop {
            tokio::select! {
                _ = stop.changed() => { drop(ds); return; }
                ev = rx.recv() => {
                    let Some(ev) = ev else { return self.set_status(TerminalStatus::Ended { reason: "Terminal stream closed".into() }); };
                    match ev {
                        DataEvent::Started => self.set_status(TerminalStatus::Connecting),
                        DataEvent::Snapshot(s) => {
                            let r = {
                                let mut t = self.term.lock();
                                let r = t.restore(&s);
                                // Shrew's palette unless the server sends a theme (which follows).
                                t.set_default_colors(Some(DEFAULT_FG), Some(DEFAULT_BG), Some(DEFAULT_CURSOR));
                                if let Some(th) = *self.forced.lock() {
                                    apply(&mut t, &th);
                                }
                                r
                            };
                            if let Err(e) = r { tracing::warn!("restore: {e}"); }
                            let (c, r) = self.size();
                            self.listener.size_changed(c, r);
                            self.set_status(TerminalStatus::Live);
                            self.dirty();
                        }
                        DataEvent::Output(o) => {
                            self.term.lock().write_vt(&o);
                            self.dirty();
                        }
                        DataEvent::Resize { cols, rows } => {
                            let changed = {
                                let mut t = self.term.lock();
                                let (oc, or) = t.size();
                                if (oc, or) != (cols as u16, rows as u16) {
                                    t.resize(cols as u16, rows as u16, 0, 0);
                                    true
                                } else { false }
                            };
                            if changed { self.listener.size_changed(cols as u16, rows as u16); self.dirty(); }
                        }
                        DataEvent::Theme { .. } if self.forced.lock().is_some() => {}
                        DataEvent::Theme { foreground, background, cursor, .. } => {
                            let p = |s: &Option<String>, d: Rgb| s.as_deref().and_then(Rgb::parse_hex).or(Some(d));
                            self.term.lock().set_default_colors(p(&foreground, DEFAULT_FG), p(&background, DEFAULT_BG), p(&cursor, DEFAULT_CURSOR));
                            self.dirty();
                        }
                        DataEvent::ChildExited { exit_code } => self.set_status(TerminalStatus::Exited { code: exit_code }),
                        DataEvent::Reconnecting => self.set_status(TerminalStatus::Reconnecting),
                        DataEvent::Resumed => self.set_status(TerminalStatus::Live),
                        DataEvent::NeedsTicket => {
                            self.set_status(TerminalStatus::Reconnecting);
                            let this = self.clone();
                            let ticket = loop {
                                if *stop.borrow() { return; }
                                if let Some(c) = this.server.wait_connection(Duration::from_secs(30)).await { match c.data_ticket(&this.session_id, &this.block_id).await {
                                    Ok(t) => break t,
                                    Err(e) if e.api_code() == Some("not_found") || e.api_code() == Some("session_not_found") => {
                                        return this.set_status(TerminalStatus::Ended { reason: "This terminal closed".into() });
                                    }
                                    Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
                                } }
                            };
                            ds.restart(ticket);
                        }
                        DataEvent::Ended { reason } => {
                            let reason = if reason.contains("block closed") { "This terminal closed".to_owned() } else { reason };
                            return self.set_status(TerminalStatus::Ended { reason });
                        }
                    }
                }
            }
        }
    }

    async fn input_task(self: Arc<Self>, rx: mpsc::UnboundedReceiver<Input>) {
        let mut stop = self.stop.subscribe();
        if *stop.borrow() {
            return;
        }
        tokio::select! {
            biased;
            _ = stop.changed() => {},
            _ = self.run_input(rx) => {},
        }
    }

    async fn run_input(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Input>) {
        while let Some(first) = rx.recv().await {
            // Merge queued text so fast typing over slow links costs one round trip.
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                match (batch.last_mut(), more) {
                    (Some(Input::Text(t)), Input::Text(m)) => t.push_str(&m),
                    (_, m) => batch.push(m),
                }
            }
            let Some(conn) = self.server.wait_connection(Duration::from_secs(10)).await else {
                self.control_pending.store(false, Ordering::Release);
                continue;
            };
            for item in batch {
                let r = match &item {
                    Input::Text(t) => conn.write(&self.session_id, &self.block_id, t).await,
                    Input::Key(k) => conn.key(&self.session_id, &self.block_id, k).await,
                    Input::KeySequence(key, prefix) => {
                        let mut result = Ok(());
                        for action in ["press", "release"] {
                            result = conn.call("client.key", sequence_payload(&self.session_id,
                                &self.block_id, key, prefix, action)).await.map(|_| ());
                            if result.is_err() { break; }
                        }
                        result
                    }
                    Input::KeyEvent(key, action, prefix) => conn.call("client.key", sequence_payload(
                        &self.session_id, &self.block_id, key, prefix, action)).await.map(|_| ()),
                    Input::Action(name, args) => conn.call("client.action", json!({
                        "session_id": self.session_id, "block_id": self.block_id,
                        "action": name, "args": args, "origin": "key",
                    })).await.map(|reply| {
                        let actions = reply["client_actions"].as_array().into_iter().flatten()
                            .filter_map(|a| Some((a["action"].as_str()?.to_owned(), a["args"].clone())))
                            .collect();
                        self.listener.client_actions(actions);
                    }),
                    Input::Event(event) => conn.call("block.event", json!({
                        "session_id": self.session_id, "block_id": self.block_id, "event": event,
                    })).await.map(|_| ()),
                    Input::Control(take, control_conn) => {
                        // A delayed request must never acquire ownership on a replacement connection.
                        let result = match take {
                            Some(take) => control_conn.own_block(&self.session_id, &self.block_id, *take).await,
                            None => control_conn.disown_block(&self.session_id, &self.block_id).await,
                        };
                        self.control_pending.store(false, Ordering::Release);
                        if let Ok(reply) = &result {
                            self.claimed.store(take.is_some(), Ordering::Release);
                            let mut model = self.server.model.lock();
                            if let Some(owner) = reply_owner(reply, &self.block_id) {
                                self.control.store(model.client_id.as_deref() == owner
                                    && owner.is_some(), Ordering::Release);
                                if let Some(view) = model.sessions.get_mut(&self.session_id).and_then(|s| s.view.as_mut()) {
                                    for block in view.windows.iter_mut().flat_map(|w| &mut w.layers).flat_map(|l| &mut l.blocks) {
                                        if block.block_id.as_deref() == Some(self.block_id.as_str()) {
                                            block.owner = owner.map(str::to_owned);
                                        }
                                    }
                                }
                            }
                            drop(model);
                            self.resize_gen.fetch_add(1, Ordering::AcqRel);
                            self.listener.frame_ready();
                        }
                        if result.is_ok() && take.is_some() {
                            let device = *self.device.lock();
                            if let Some((cols, rows, cw, ch)) = device {
                                let px = (cw > 0 && ch > 0).then_some((cw, ch));
                                if let Err(e) = control_conn.resize(&self.session_id, &self.block_id,
                                    cols as u32, rows as u32, px).await {
                                    tracing::warn!("requested resize: {e}");
                                }
                            }
                        }
                        result.map(|_| ())
                    }
                    Input::Paste(p) => conn.paste(&self.session_id, &self.block_id, p).await,
                };
                if let Err(e) = r {
                    tracing::warn!("input: {e}");
                }
            }
        }
    }

    /// Type text (soft keyboard, IME commit, dictation).
    pub fn send_text(&self, text: &str) {
        if !text.is_empty() {
            self.snap_to_bottom();
            let _ = self.input.send(Input::Text(text.to_owned()));
        }
    }

    pub fn send_key(&self, key: Key, ctrl: bool, alt: bool, shift: bool, meta: bool) {
        self.snap_to_bottom();
        let _ = self.input.send(Input::Key(key.press(Mods {
            ctrl,
            alt,
            shift,
            meta,
        })));
    }

    /// Send the platform's actual key action; release is never synthesized here.
    pub fn send_key_event(
        &self,
        key: Key,
        mods: (bool, bool, bool, bool),
        action: &str,
        prefix: &[String],
    ) {
        let (ctrl, alt, shift, meta) = mods;
        let mods = Mods {
            ctrl,
            alt,
            shift,
            meta,
        };
        self.snap_to_bottom();
        let _ = self.input.send(Input::KeyEvent(
            key.press(mods),
            action.to_owned(),
            prefix.to_vec(),
        ));
    }

    /// Queue a server-side configured action with the same ordering as keys.
    pub fn send_action(&self, name: &str, args: serde_json::Value) {
        let _ = self.input.send(Input::Action(name.to_owned(), args));
    }

    /// Complete a key sequence without replaying its prefix into the terminal.
    pub fn send_key_sequence(
        &self,
        key: Key,
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
        prefix: &[String],
    ) {
        self.snap_to_bottom();
        let _ = self.input.send(Input::KeySequence(
            key.press(Mods {
                ctrl,
                alt,
                shift,
                meta,
            }),
            prefix.to_vec(),
        ));
    }

    pub fn paste(&self, text: &str) {
        if !text.is_empty() {
            self.snap_to_bottom();
            let _ = self.input.send(Input::Paste(text.to_owned()));
        }
    }

    fn snap_to_bottom(&self) {
        let mut t = self.term.lock();
        if !t.scrollbar().at_bottom() {
            t.scroll_to_bottom();
            drop(t);
            self.dirty();
        }
    }

    /// Scroll local history by rows (negative = up).
    pub fn scroll(&self, rows: i32) {
        self.term.lock().scroll(rows as isize);
        self.dirty();
    }

    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_to_bottom();
        self.dirty();
    }

    pub fn jump_prompt(&self, delta: i32) {
        self.term.lock().scroll_prompts(delta as isize);
        self.dirty();
    }

    pub fn select_word(&self, col: u16, row: u16) -> bool {
        let ok = self.term.lock().select_word(col, row);
        self.dirty();
        ok
    }

    pub fn select_line(&self, row: u16) -> bool {
        let ok = self.term.lock().select_line(row);
        self.dirty();
        ok
    }

    pub fn select_range(&self, from: (u16, u16), to: (u16, u16)) -> bool {
        let ok = self.term.lock().select(from, to);
        self.dirty();
        ok
    }

    pub fn clear_selection(&self) {
        self.term.lock().clear_selection();
        self.dirty();
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_text()
    }

    pub fn all_text(&self) -> String {
        self.term.lock().all_text()
    }

    /// Tell the core how big this device's view is, in cells and pixels.
    /// If this device has control, the remote terminal is resized to match.
    pub fn set_device_grid(&self, cols: u16, rows: u16, cell_w: u32, cell_h: u32) {
        let prev = self.device.lock().replace((cols, rows, cell_w, cell_h));
        if (self.has_control() || self.claimed.load(Ordering::Acquire))
            && prev.map(|p| (p.0, p.1)) != Some((cols, rows))
        {
            self.push_resize_settled();
        }
    }

    /// Force a theme (or `None` to go back to the server's colors).
    pub fn set_theme(&self, theme: Option<ColorTheme>) {
        *self.forced.lock() = theme;
        let mut t = self.term.lock();
        match theme {
            Some(th) => apply(&mut t, &th),
            None => {
                t.set_ansi_palette(None);
                t.set_default_colors(Some(DEFAULT_FG), Some(DEFAULT_BG), Some(DEFAULT_CURSOR));
            }
        }
        drop(t);
        self.dirty();
    }

    pub fn has_control(&self) -> bool {
        let model = self.server.model.lock();
        if let Some(view) = model
            .sessions
            .get(&self.session_id)
            .and_then(|s| s.view.as_ref())
        {
            if let Some(block) = view
                .windows
                .iter()
                .flat_map(|w| w.blocks())
                .find(|b| b.block_id.as_deref() == Some(self.block_id.as_str()))
            {
                return model.client_id.is_some() && block.owner == model.client_id;
            }
        }
        self.control.load(Ordering::Acquire)
    }

    /// Force ownership. Ownership only changes locally after a successful RPC.
    pub fn take_control(&self) {
        self.claim_control(true);
    }

    /// Ask for ownership without taking it away from another client.
    pub fn request_control(&self) {
        self.claim_control(false);
    }

    fn claim_control(&self, take: bool) {
        if *self.stop.borrow() {
            return;
        }
        let Some(conn) = self.server.connection() else {
            return;
        };
        if !self.control_pending.swap(true, Ordering::AcqRel) {
            let _ = self.input.send(Input::Control(Some(take), conn));
        }
    }

    pub fn release_control(&self) {
        self.claimed.store(false, Ordering::Release);
        self.resize_gen.fetch_add(1, Ordering::AcqRel);
        if let Some(conn) = self.server.connection() {
            let _ = self.input.send(Input::Control(None, conn));
        }
    }

    /// Resize once the grid stops changing. While the keyboard slides in, the
    /// view's height changes every frame; sending each step would make the
    /// program redraw a dozen times (visible stutter and reflow). 120 ms of
    /// quiet is shorter than a person notices, longer than an animation frame.
    fn push_resize_settled(&self) {
        let ticket = self.resize_gen.fetch_add(1, Ordering::AcqRel) + 1;
        let latest = self.resize_gen.clone();
        let device = *self.device.lock();
        let (server, sid, bid) = (
            self.server.clone(),
            self.session_id.clone(),
            self.block_id.clone(),
        );
        self.rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            if latest.load(Ordering::Acquire) != ticket {
                return; // a newer size superseded this one
            }
            let Some((cols, rows, cw, ch)) = device else {
                return;
            };
            if let Some(c) = server.connection() {
                let px = (cw > 0 && ch > 0).then_some((cw, ch));
                if let Err(e) = c.resize(&sid, &bid, cols as u32, rows as u32, px).await {
                    tracing::warn!("resize: {e}");
                }
            }
        });
    }

    /// Ask the remote side for the full text (including history the stream didn't carry).
    pub async fn remote_text(&self) -> Option<String> {
        let c = self.server.connection()?;
        let (sid, bid) = (self.session_id.clone(), self.block_id.clone());
        self.rt
            .spawn(async move { c.screen_text(&sid, &bid, Some(4 << 20)).await.ok() })
            .await
            .ok()
            .flatten()
    }

    /// Mouse coordinates are zero-based cells; buttons are left/middle/right.
    pub fn mouse_event(
        &self,
        action: &str,
        button: Option<&str>,
        buttons: &[&str],
        col: u16,
        row: u16,
        ctrl: bool,
        alt: bool,
        shift: bool,
        meta: bool,
    ) {
        let _ = self.input.send(Input::Event(mouse_payload(
            action, button, buttons, col, row, ctrl, alt, shift, meta,
        )));
    }

    pub fn click(&self, col: u16, row: u16) {
        self.mouse_event(
            "press",
            Some("left"),
            &["left"],
            col,
            row,
            false,
            false,
            false,
            false,
        );
        self.mouse_event(
            "release",
            Some("left"),
            &[],
            col,
            row,
            false,
            false,
            false,
            false,
        );
    }

    /// Positive wheel delta is down. Shares the keyboard's ordered queue.
    pub fn wheel(&self, col: u16, row: u16, lines: f32) {
        let _ = self
            .input
            .send(Input::Event(json!({"type": "wheel", "payload": {
                "delta_x": 0, "delta_y": lines, "delta_mode": "line", "x": col, "y": row,
            }})));
    }

    /// Notify programs using focus reporting, ordered with their input.
    pub fn focus_event(&self, focused: bool) {
        let _ = self
            .input
            .send(Input::Event(json!({"type": "focus", "payload": {
                "action": if focused { "in" } else { "out" },
            }})));
    }

    pub fn mouse_tracking(&self) -> bool {
        self.term.lock().mouse_tracking()
    }

    pub(crate) fn on_clipboard(&self, text: String) {
        self.listener.clipboard(text);
    }
}

fn reply_owner<'a>(reply: &'a serde_json::Value, block: &str) -> Option<Option<&'a str>> {
    reply
        .get("blocks")?
        .as_array()?
        .iter()
        .find(|b| b.get("block_id").and_then(|v| v.as_str()) == Some(block))
        .map(|b| b.get("owner").and_then(|v| v.as_str()))
}

fn sequence_payload(
    sid: &str,
    bid: &str,
    key: &KeyPress,
    prefix: &[String],
    action: &str,
) -> serde_json::Value {
    let mods = modifier_names(key.mods.ctrl, key.mods.alt, key.mods.shift, key.mods.meta);
    let unshifted = key
        .text
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase() as u32)
        .or_else(|| {
            key.key
                .strip_prefix("key_")
                .and_then(|k| k.chars().next())
                .map(|c| c as u32)
        })
        .unwrap_or(0);
    json!({"session_id": sid, "block_id": bid, "prefix": prefix, "key": {
        "key": key.key, "mods": mods, "consumed_mods": "",
        "text": if action == "release" { "" } else { &key.text },
        "unshifted_codepoint": unshifted, "composing": false, "action": action,
    }})
}

fn modifier_names(ctrl: bool, alt: bool, shift: bool, meta: bool) -> String {
    [
        (shift, "shift"),
        (ctrl, "ctrl"),
        (alt, "alt"),
        (meta, "super"),
    ]
    .iter()
    .filter_map(|(held, name)| held.then_some(*name))
    .collect::<Vec<_>>()
    .join("+")
}

fn mouse_payload(
    action: &str,
    button: Option<&str>,
    buttons: &[&str],
    col: u16,
    row: u16,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> serde_json::Value {
    let held: serde_json::Map<String, serde_json::Value> = buttons
        .iter()
        .filter(|b| matches!(**b, "left" | "middle" | "right"))
        .map(|b| ((*b).to_owned(), json!(true)))
        .collect();
    let mods = modifier_names(ctrl, alt, shift, meta);
    json!({"type": "mouse", "payload": {"action": action, "button": button,
        "buttons": held, "x": col, "y": row, "mods": mods}})
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.resize_gen.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Listener;
    impl TerminalListener for Listener {
        fn frame_ready(&self) {}
        fn status_changed(&self, _: TerminalStatus) {}
        fn clipboard(&self, _: String) {}
        fn size_changed(&self, _: u16, _: u16) {}
    }

    #[tokio::test]
    async fn close_releases_tasks_even_while_waiting_for_a_connection() {
        let server = ServerEntry::new(crate::store::Profile {
            id: "test".into(),
            name: "test".into(),
            address: "localhost:9999".into(),
        });
        let session = TerminalSession::open(server, "s".into(), "b".into(), Arc::new(Listener));
        let weak = Arc::downgrade(&session);
        session.send_text("queued");
        // Close before either task has subscribed: this must not lose the stop.
        session.close();
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed session retained by background tasks");
    }

    #[tokio::test]
    async fn close_cancels_an_already_waiting_input_and_stream_task() {
        let server = ServerEntry::new(crate::store::Profile {
            id: "test".into(),
            name: "test".into(),
            address: "localhost:9999".into(),
        });
        let session = TerminalSession::open(server, "s".into(), "b".into(), Arc::new(Listener));
        let weak = Arc::downgrade(&session);
        session.send_key(Key::Enter, false, false, false, false);
        tokio::task::yield_now().await;
        session.close();
        drop(session);
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connection wait ignored close");
    }

    #[test]
    fn mouse_payload_preserves_buttons_and_modifiers() {
        let event = mouse_payload(
            "move",
            Some("right"),
            &["right", "middle"],
            8,
            4,
            true,
            false,
            true,
            false,
        );
        assert_eq!(event["type"], "mouse");
        assert_eq!(event["payload"]["action"], "move");
        assert_eq!(event["payload"]["buttons"]["right"], true);
        assert_eq!(event["payload"]["buttons"]["middle"], true);
        assert_eq!(event["payload"]["mods"], "shift+ctrl");
        assert_eq!(event["payload"]["x"], 8);
    }
    #[test]
    fn sequence_prefix_is_metadata_not_replayed_keystrokes() {
        let key = Key::Char('x').press(Mods::default());
        let payload = sequence_payload("s", "b", &key, &["ctrl+b".into()], "press");
        assert_eq!(payload["prefix"], json!(["ctrl+b"]));
        assert_eq!(payload["key"]["key"], "key_x");
        assert_eq!(payload["key"]["text"], "x");
        let released = sequence_payload("s", "b", &key, &["ctrl+b".into()], "release");
        assert_eq!(released["key"]["text"], "");
    }

    #[test]
    fn ownership_reply_does_not_assume_a_successful_request_granted_control() {
        let reply = json!({"blocks":[{"block_id":"b","owner":"another-client"}]});
        assert_eq!(reply_owner(&reply, "b"), Some(Some("another-client")));
        assert_eq!(reply_owner(&reply, "missing"), None);
        assert_eq!(
            reply_owner(&json!({"blocks":[{"block_id":"b"}]}), "b"),
            Some(None)
        );
    }
    fn queued_session() -> (Arc<TerminalSession>, mpsc::UnboundedReceiver<Input>) {
        let server = ServerEntry::new(crate::store::Profile {
            id: "test".into(),
            name: "test".into(),
            address: "localhost:9999".into(),
        });
        let (input, rx) = mpsc::unbounded_channel();
        (
            Arc::new(TerminalSession {
                server,
                session_id: "s".into(),
                block_id: "b".into(),
                term: Mutex::new(Terminal::new(80, 24)),
                status: Mutex::new(TerminalStatus::Connecting),
                pending: AtomicBool::new(false),
                listener: Arc::new(Listener),
                input,
                stop: watch::channel(false).0,
                device: Mutex::new(None),
                resize_gen: Arc::new(AtomicU64::new(0)),
                control: AtomicBool::new(false),
                control_pending: AtomicBool::new(false),
                claimed: AtomicBool::new(false),
                forced: Mutex::new(None),
                rt: tokio::runtime::Handle::current(),
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn hardware_actions_and_server_action_share_the_input_fifo() {
        let (session, mut queue) = queued_session();
        let mods = (false, false, false, false);
        session.send_key_event(Key::Char('x'), mods, "press", &["ctrl+b".into()]);
        session.send_key_event(Key::Char('x'), mods, "repeat", &["ctrl+b".into()]);
        session.send_action("clear", json!({}));
        session.send_key_event(Key::Char('x'), mods, "release", &["ctrl+b".into()]);
        for action in ["press", "repeat"] {
            match queue.try_recv().unwrap() {
                Input::KeyEvent(key, actual, prefix) => {
                    assert_eq!(actual, action);
                    assert_eq!(prefix, vec!["ctrl+b"]);
                    let payload = sequence_payload("s", "b", &key, &prefix, &actual);
                    assert_eq!(payload["key"]["action"], action);
                }
                _ => panic!("hardware action reordered or synthesized"),
            }
        }
        assert!(matches!(queue.try_recv().unwrap(), Input::Action(name, _) if name == "clear"));
        assert!(
            matches!(queue.try_recv().unwrap(), Input::KeyEvent(_, action, _) if action == "release")
        );
        assert!(queue.try_recv().is_err()); // no synthesized release
    }

    #[tokio::test]
    async fn offline_control_request_cannot_become_a_ghost_claim_after_reconnect() {
        let (session, mut queue) = queued_session();
        session.request_control();
        session.take_control();
        assert!(queue.try_recv().is_err());
        assert!(!session.control_pending.load(Ordering::Acquire));
        session.close();
        session.request_control();
        assert!(queue.try_recv().is_err());
    }
}
