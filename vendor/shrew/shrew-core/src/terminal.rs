//! A terminal a UI is showing: its stream, its emulator, and its input queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt};
use futures_util::stream::{FuturesOrdered, StreamExt};
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
    /// Typed input, a paste, or another queued request did not reach the
    /// server. At most once per batch of queued input.
    fn input_failed(&self, _message: String) {}
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

/// The emulator and the last frame built from it, behind one lock. Every
/// mutable access to the emulator drops the frame, so no call site can leave
/// a stale one behind.
struct Screen(Mutex<(Terminal, Option<Arc<Frame>>)>);

struct ScreenGuard<'a>(parking_lot::MutexGuard<'a, (Terminal, Option<Arc<Frame>>)>);

impl Screen {
    fn new(term: Terminal) -> Self {
        Self(Mutex::new((term, None)))
    }

    fn lock(&self) -> ScreenGuard<'_> {
        ScreenGuard(self.0.lock())
    }
}

impl ScreenGuard<'_> {
    fn frame(&mut self) -> Arc<Frame> {
        let (term, frame) = &mut *self.0;
        frame.get_or_insert_with(|| Arc::new(term.frame())).clone()
    }
}

impl std::ops::Deref for ScreenGuard<'_> {
    type Target = Terminal;
    fn deref(&self) -> &Terminal {
        &self.0.0
    }
}

impl std::ops::DerefMut for ScreenGuard<'_> {
    fn deref_mut(&mut self) -> &mut Terminal {
        self.0.1 = None;
        &mut self.0.0
    }
}

/// What a pipelined request carried, which decides what its reply is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sent {
    Key,
    /// A configured action: its reply may ask for client-side effects.
    Action,
    /// Mouse, wheel or focus. A failure is logged, not reported: nothing the
    /// user typed was lost, and a pointer crossing a pane is not worth a notice.
    Event,
}

/// The reply to a request, with what the request was.
type Answer = (Sent, Result<serde_json::Value, shrew_client::ClientError>);

/// Requests sent and not yet answered, oldest first.
type InFlight = FuturesOrdered<BoxFuture<'static, Answer>>;

/// How many unanswered requests may be out before the next one waits. Enough
/// that typing and dragging never wait; a bound so a stalled link cannot queue
/// without limit.
const MAX_IN_FLIGHT: usize = 32;

pub struct TerminalSession {
    server: Arc<ServerEntry>,
    pub session_id: String,
    pub block_id: String,
    term: Screen,
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
            term: Screen::new(term),
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

    /// The frame to draw. Clears the "frame ready" latch. Built once per
    /// change: a view that redraws for any other reason (focus, a neighbour's
    /// output, the chrome) gets the same frame back instead of a new walk over
    /// every cell.
    pub fn frame(&self) -> Arc<Frame> {
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
        let mut in_flight = InFlight::new();
        loop {
            // Wait for input, taking replies as they come.
            let first = loop {
                tokio::select! {
                    biased;
                    item = rx.recv() => match item {
                        Some(item) => break item,
                        None => return,
                    },
                    Some(reply) = in_flight.next(), if !in_flight.is_empty() => {
                        // A dead connection fails everything in flight at once: one report.
                        let mut failure = self.settle(reply);
                        while let Some(Some(reply)) = in_flight.next().now_or_never() {
                            failure = failure.or(self.settle(reply));
                        }
                        if let Some(message) = failure {
                            self.listener.input_failed(message);
                        }
                    }
                }
            };
            // Merge queued text so fast typing over slow links costs one round trip.
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                match (batch.last_mut(), more) {
                    (Some(Input::Text(t)), Input::Text(m)) => t.push_str(&m),
                    (_, m) => batch.push(m),
                }
            }
            let Some(conn) = self.server.wait_connection(Duration::from_secs(10)).await else {
                // Whatever queued up while waiting is just as undeliverable. Drop it
                // all and say so: replaying stale keystrokes into a shell after a
                // reconnect is worse than losing them visibly.
                let mut dropped = batch.len();
                while rx.try_recv().is_ok() {
                    dropped += 1;
                }
                self.control_pending.store(false, Ordering::Release);
                self.listener.input_failed(format!(
                    "Not connected: {dropped} queued input{} not sent",
                    if dropped == 1 { " was" } else { "s were" }
                ));
                continue;
            };
            let mut failure: Option<String> = None;
            for item in batch {
                match item {
                    // Rex runs keys, actions and block events in arrival order, so
                    // they go out without waiting for each other's replies: a held
                    // arrow key or a mouse drag is not paced by the round trip.
                    Input::Key(..) | Input::KeySequence(..) | Input::KeyEvent(..) | Input::Action(..) | Input::Event(_) => {
                        let sent = match item {
                            Input::Action(..) => Sent::Action,
                            Input::Event(_) => Sent::Event,
                            _ => Sent::Key,
                        };
                        for (method, payload) in self.requests(&item) {
                            if in_flight.len() >= MAX_IN_FLIGHT
                                && let Some(reply) = in_flight.next().await
                            {
                                failure = failure.or(self.settle(reply));
                            }
                            match conn.start(method, payload) {
                                Ok(reply) => in_flight.push_back(Box::pin(async move { (sent, reply.await.map(|(_, value)| value)) })),
                                Err(e) => failure = failure.or(report(sent, &e)),
                            }
                        }
                    }
                    // A paste is a block event too, so it keeps its place behind
                    // the keys in flight; a large one is several, in order.
                    Input::Paste(text) => {
                        if let Err(e) = conn.paste(&self.session_id, &self.block_id, &text).await {
                            failure = failure.or(report(Sent::Key, &e));
                        }
                    }
                    // Rex runs `write` concurrently with everything else, so text
                    // keeps its place only if all before it has been answered.
                    Input::Text(text) => {
                        failure = failure.or(self.settle_all(&mut in_flight).await);
                        if let Err(e) = conn.write(&self.session_id, &self.block_id, &text).await {
                            failure = failure.or(report(Sent::Key, &e));
                        }
                    }
                    Input::Control(take, control_conn) => {
                        failure = failure.or(self.settle_all(&mut in_flight).await);
                        // Ownership is the terminal's size, not input: logged only.
                        if let Err(e) = self.change_control(take, &control_conn).await {
                            tracing::warn!("control: {e}");
                        }
                    }
                }
            }
            if let Some(message) = failure {
                self.listener.input_failed(message);
            }
        }
    }

    /// The ordered requests that deliver a key, action or block event.
    fn requests(&self, item: &Input) -> Vec<(&'static str, serde_json::Value)> {
        let (sid, bid) = (&self.session_id, &self.block_id);
        let key = |key: &KeyPress, prefix: &[String], action: &str| ("client.key", sequence_payload(sid, bid, key, prefix, action));
        match item {
            Input::Key(k) => vec![key(k, &[], "press"), key(k, &[], "release")],
            Input::KeySequence(k, prefix) => vec![key(k, prefix, "press"), key(k, prefix, "release")],
            Input::KeyEvent(k, action, prefix) => vec![key(k, prefix, action)],
            Input::Action(name, args) => vec![(
                "client.action",
                json!({"session_id": sid, "block_id": bid, "action": name, "args": args, "origin": "key"}),
            )],
            Input::Event(event) => vec![("block.event", json!({"session_id": sid, "block_id": bid, "event": event}))],
            // Not pipelined: `run_input` sends these itself.
            Input::Text(_) | Input::Paste(_) | Input::Control(..) => Vec::new(),
        }
    }

    /// Take one reply. Returns what to tell the user if the request failed.
    fn settle(&self, (sent, result): Answer) -> Option<String> {
        match result {
            Ok(reply) => {
                if sent == Sent::Action {
                    let actions = reply["client_actions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|a| Some((a["action"].as_str()?.to_owned(), a["args"].clone())))
                        .collect();
                    self.listener.client_actions(actions);
                }
                None
            }
            Err(e) => report(sent, &e),
        }
    }

    /// Wait until everything in flight is answered. Returns the first failure.
    async fn settle_all(&self, in_flight: &mut InFlight) -> Option<String> {
        let mut failure = None;
        while let Some(reply) = in_flight.next().await {
            failure = failure.or(self.settle(reply));
        }
        failure
    }

    /// Take, request or release ownership on `conn`, the connection the user
    /// asked on: a delayed request must never acquire ownership on a replacement.
    async fn change_control(&self, take: Option<bool>, conn: &shrew_client::Connection) -> Result<(), shrew_client::ClientError> {
        let result = match take {
            Some(take) => conn.own_block(&self.session_id, &self.block_id, take).await,
            None => conn.disown_block(&self.session_id, &self.block_id).await,
        };
        self.control_pending.store(false, Ordering::Release);
        let reply = result?;
        self.claimed.store(take.is_some(), Ordering::Release);
        if let Some(owner) = reply_owner(&reply, &self.block_id) {
            let mut model = self.server.model.lock();
            self.control.store(model.client_id.as_deref() == owner && owner.is_some(), Ordering::Release);
            if let Some(view) = model.sessions.get_mut(&self.session_id).and_then(|s| s.view.as_mut()) {
                for block in view.windows.iter_mut().flat_map(|w| &mut w.layers).flat_map(|l| &mut l.blocks) {
                    if block.block_id.as_deref() == Some(self.block_id.as_str()) {
                        block.owner = owner.map(str::to_owned);
                    }
                }
            }
        }
        self.resize_gen.fetch_add(1, Ordering::AcqRel);
        self.listener.frame_ready();
        if take.is_some() {
            let device = *self.device.lock();
            if let Some((cols, rows, cw, ch)) = device {
                let px = (cw > 0 && ch > 0).then_some((cw, ch));
                if let Err(e) = conn.resize(&self.session_id, &self.block_id, cols as u32, rows as u32, px).await {
                    tracing::warn!("requested resize: {e}");
                }
            }
        }
        Ok(())
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
    /// `action` is Rex's: `press`, `release`, or `motion` for a move or drag.
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

/// Log a failed input request, and word it for the user when it is worth
/// telling them: something they typed did not arrive, and the pane it was
/// for is still there.
fn report(sent: Sent, e: &shrew_client::ClientError) -> Option<String> {
    tracing::warn!("input: {e}");
    (sent != Sent::Event && !target_gone(e)).then(|| crate::server::friendly(e))
}

/// The block or session the input was for no longer exists. A key released
/// just after the one that closed its pane ends up here; that is not a loss.
fn target_gone(e: &shrew_client::ClientError) -> bool {
    match e {
        shrew_client::ClientError::Api(api) => {
            matches!(api.code.as_str(), "not_found" | "session_not_found")
                || api.message.contains("is not in the session")
        }
        _ => false,
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

    #[derive(Default)]
    struct Failures(Mutex<Vec<String>>);
    impl TerminalListener for Failures {
        fn frame_ready(&self) {}
        fn status_changed(&self, _: TerminalStatus) {}
        fn clipboard(&self, _: String) {}
        fn size_changed(&self, _: u16, _: u16) {}
        fn input_failed(&self, message: String) {
            self.0.lock().push(message);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn input_that_cannot_be_delivered_is_reported_once_not_dropped_silently() {
        let server = ServerEntry::new(crate::store::Profile {
            id: "test".into(),
            name: "test".into(),
            address: "localhost:9999".into(),
        });
        let failures = Arc::new(Failures::default());
        let session = TerminalSession::open(server, "s".into(), "b".into(), failures.clone());
        session.send_text("ls");
        tokio::task::yield_now().await;
        // Queued behind the batch that is waiting for a connection.
        session.send_key(Key::Enter, false, false, false, false);
        session.paste("more");
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(*failures.0.lock(), vec!["Not connected: 3 queued inputs were not sent".to_owned()]);
        session.close();
    }

    #[tokio::test]
    async fn a_frame_is_rebuilt_only_after_the_terminal_changed() {
        let (session, _queue) = queued_session();
        let first = session.frame();
        // Reading, and typing while already at the bottom, change nothing.
        let _ = (session.size(), session.title(), session.mouse_tracking(), session.selection_text());
        session.send_text("x");
        assert!(Arc::ptr_eq(&first, &session.frame()), "an unchanged terminal was walked again");

        session.term.lock().write_vt(b"hello");
        let second = session.frame();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.lines[0].runs[0].text.trim_end(), "hello");
        assert!(Arc::ptr_eq(&second, &session.frame()));

        // Scrolling and selecting are changes too.
        session.select_range((0, 0), (4, 0));
        let third = session.frame();
        assert!(!Arc::ptr_eq(&second, &third));
        session.clear_selection();
        assert!(!Arc::ptr_eq(&third, &session.frame()));
    }

    #[test]
    fn mouse_payload_preserves_buttons_and_modifiers() {
        let event = mouse_payload(
            "motion",
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
        assert_eq!(event["payload"]["action"], "motion");
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
    fn only_lost_typing_for_a_pane_that_still_exists_is_reported() {
        use shrew_client::ClientError;
        let api = |code: &str, message: &str| {
            ClientError::Api(shrew_client::proto::ApiError { code: code.into(), message: message.into(), status: 400 })
        };
        assert!(report(Sent::Key, &ClientError::Timeout).is_some());
        assert!(report(Sent::Action, &api("bad_request", "unknown action")).is_some());
        // A pointer event the server refused is not typing.
        assert_eq!(report(Sent::Event, &api("bad_request", "unknown mouse action")), None);
        // The pane closed under the key: nothing to tell.
        assert_eq!(report(Sent::Key, &api("not_found", "block not found")), None);
        assert_eq!(report(Sent::Key, &api("bad_request", "block block:1 is not in the session")), None);
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
                term: Screen::new(Terminal::new(80, 24)),
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
