//! One Rex block, drawn as a pane.

use std::collections::HashMap;
use std::sync::Arc;

use async_channel::Sender;
use flex_engine::keybindings::{KeyBindings, Resolution};
use flex_engine::{fit_grid, Engine, Metrics, TerminalListener, TerminalSession};
use gpui::prelude::*;
use gpui::{
    canvas, div, px, App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, KeyUpEvent, MouseButton, Point, Render, SharedString,
    UTF16Selection, Window,
};
use shrew_core::{ColorTheme, Frame, Rgb, Row, TerminalStatus};

use crate::{keymap, paint, shortcuts};

const PAD: f32 = 4.0;

/// The terminal's colours, so it belongs to the same window as the chrome: the
/// settings page's greys, the amber accent as the cursor, and a muted ANSI set
/// built from the same hues.
const THEME: ColorTheme = ColorTheme {
    foreground: Rgb::new(0xc9, 0xce, 0xd8),
    background: Rgb::new(0x0f, 0x11, 0x14),
    cursor: Rgb::new(0xf0, 0xb4, 0x29),
    ansi: [
        Rgb::new(0x1a, 0x1c, 0x21),
        Rgb::new(0xc4, 0x56, 0x4f),
        Rgb::new(0x8f, 0xbf, 0x6b),
        Rgb::new(0xf0, 0xb4, 0x29),
        Rgb::new(0x6e, 0xa8, 0xd8),
        Rgb::new(0xb4, 0x8e, 0xad),
        Rgb::new(0x7f, 0xc8, 0xc0),
        Rgb::new(0xc9, 0xce, 0xd8),
        Rgb::new(0x4a, 0x4f, 0x5a),
        Rgb::new(0xe0, 0x7b, 0x74),
        Rgb::new(0xa9, 0xd9, 0x8a),
        Rgb::new(0xf6, 0xc8, 0x5f),
        Rgb::new(0x8f, 0xc3, 0xea),
        Rgb::new(0xcb, 0xa8, 0xc7),
        Rgb::new(0xa3, 0xde, 0xd6),
        Rgb::new(0xf2, 0xf5, 0xf9),
    ],
};

/// Line height in em, i.e. a multiple of the font size. The glyph box
/// (ascent + descent) is only ~0.78 em, so using it alone sets lines almost
/// touching; 1.3 em is a comfortable terminal pitch. Raise this for more air.
const LINE_HEIGHT_EM: f32 = 1.3;

pub enum TerminalEvent {
    ConfiguredAction {
        name: String,
        args: serde_json::Value,
    },
}

impl gpui::EventEmitter<TerminalEvent> for TerminalView {}

pub struct TerminalView {
    /// An input method is composing. Its pinyin letters arrive as ordinary key
    /// events and must not reach the terminal; only the committed text should,
    /// and that comes through `replace_text_in_range` instead.
    composition: Composition,
    /// The shell's prefix key is armed, so the next key belongs to the chrome.
    prefix: Arc<std::sync::atomic::AtomicBool>,
    /// Where a selection drag started, in cells. `None` outside a drag.
    /// A selection being dragged. Nothing is selected until the pointer moves:
    /// a click that only switches panes must not select or copy anything.
    selecting: Option<Selecting>,
    mouse_buttons: Vec<MouseButton>,
    control_released: bool,
    focused: bool,
    forwarded: ForwardedKeys,
    keybindings: Option<Arc<std::sync::Mutex<KeyBindings>>>,
    /// The pane's text origin and cell size, written by each paint and read by
    /// the mouse handlers. Shared because paint cannot touch the view.
    geometry: Arc<std::sync::Mutex<(gpui::Point<gpui::Pixels>, Metrics)>>,
    /// How many lines above the screen scrolling may reach.
    scrollback_lines: u64,
    /// The find bar is open, with its query and the lines that match it.
    find: Option<Find>,
    /// Whether a picture is painted behind the pane, and how much of it shows.
    /// The picture itself is drawn once by the pane area, so it spans the whole
    /// terminal instead of repeating in every split.
    over_picture: bool,
    background_opacity: f32,
    /// ASCII letters sent since the last non-letter key. The first pinyin
    /// letters arrive before the composition starts, so they have already
    /// reached the terminal by the time we know better; the composition start
    /// retracts them with backspaces.
    // shortcut: PTY IME rollback is heuristic; replace it only with confirmed
    // platform composition-start semantics, not a key-delay timer.
    leaked: String,
    /// The key last sent to the pty that an input method may answer for: the
    /// plain `,` a Chinese IME turns into `，`, or the space that picks a
    /// candidate and is swallowed. The plain one has to go when that happens.
    last_punct: Option<Sent>,
    /// The key the input method is about to answer for, when its text arrived
    /// first. Windows does not always deliver the keystroke before the commit,
    /// and when it does not, the key is still on its way and has to be dropped
    /// instead of typed.
    ime_answer: Option<Pending>,
    session: Option<Arc<TerminalSession>>,
    error: Option<String>,
    frame: Frame,
    focus: FocusHandle,
    palette: paint::Palette,
}

enum Wake {
    Changed,
    Actions(Vec<(String, serde_json::Value)>),
}

struct Notifier(Sender<Wake>);

impl TerminalListener for Notifier {
    fn frame_ready(&self) {
        let _ = self.0.try_send(Wake::Changed);
    }
    fn status_changed(&self, _: TerminalStatus) {
        let _ = self.0.try_send(Wake::Changed);
    }
    /// A program copied text with OSC 52, or another client selected some. The
    /// text never passes through a GPUI action, so it goes to the clipboard the
    /// direct way.
    fn clipboard(&self, text: String) {
        crate::win32::set_clipboard_text(&text);
    }
    fn size_changed(&self, _cols: u16, _rows: u16) {}
    fn client_actions(&self, actions: Vec<(String, serde_json::Value)>) {
        if !actions.is_empty() {
            let _ = self.0.try_send(Wake::Actions(actions));
        }
    }
}

/// Whether a committed string has to be typed into the pty.
///
/// ASCII normally arrives as key events, so echoing it here would type every
/// character twice. But while a composition was in progress those key events
/// were swallowed, so an English word the IME commits (the user kept the pinyin
/// or switched the method to English mid-composition) has no other way in.
fn commit_needs_typing(was_composing: bool, text: &str) -> bool {
    was_composing || text.chars().any(|c| !c.is_ascii())
}

/// What the shell sent that an input method may answer for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sent {
    /// A punctuation key, which a Chinese IME commits as its own mark: `,` as
    /// `，`, `"` as `“`, `\\` as `、`.
    Punct(char),
    /// The space that picks a candidate. Windows sends the key on, then the IME
    /// commits the text and never a space, so the space is left behind.
    Space,
}

/// A key the input method has already answered for, still in flight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pending {
    kind: Kind,
    at: std::time::Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A punctuation key, answered with its full-width mark.
    Punct,
    /// The space that picked a candidate, answered with the candidate.
    Space,
}

impl Pending {
    /// Whether `key` is the keystroke this commit answered.
    ///
    /// The window is the point: the key that caused a commit follows it within
    /// the same message loop turn, while the next hand-typed key is tens of
    /// times later. Nothing is delayed — a key outside the window is typed as
    /// usual.
    fn answers(self, key: char) -> bool {
        const WINDOW: std::time::Duration = std::time::Duration::from_millis(150);
        self.at.elapsed() < WINDOW
            && match self.kind {
                Kind::Space => key == ' ',
                Kind::Punct => key.is_ascii_punctuation(),
            }
    }
}

/// The key a commit answers, when its text is the only evidence of it.
fn pending_answer(text: &str, composing: bool) -> Option<Pending> {
    if text.is_empty() {
        return None;
    }
    let kind = if text.chars().all(is_cjk_punctuation) {
        Kind::Punct
    } else if composing && text != " " {
        // Chinese chosen out of a composition was picked with a key, and that key
        // is usually the space that also takes the first candidate.
        Kind::Space
    } else {
        return None;
    };
    Some(Pending {
        kind,
        at: std::time::Instant::now(),
    })
}

/// Whether the committed text is the input method's answer to that key.
///
/// A Chinese IME answers a punctuation key with punctuation only, so any run of
/// full-width marks is its version of the key — the exact pair cannot be listed,
/// because an IME picks `“` or `”` by what came before, commits `——` for two
/// dashes, and has its own idea of every other key.
fn replaces_sent(sent: Option<Sent>, text: &str) -> bool {
    match sent {
        // shortcut: a plain key we sent is taken back when the IME's answer is
        // punctuation and nothing else. A mark typed in English mode and then
        // replaced by an IME commit in the same breath would be lost.
        Some(Sent::Punct(_)) => !text.is_empty() && text.chars().all(is_cjk_punctuation),
        Some(Sent::Space) => !text.is_empty() && text != " ",
        None => false,
    }
}

/// Full-width marks: what a Chinese IME commits for a plain key. Written as the
/// ranges they come in, since each keyboard key maps to a different block.
fn is_cjk_punctuation(c: char) -> bool {
    matches!(
        c as u32,
        0x2013..=0x201F   // – — ‘ ’ “ ”
            | 0x2026      // …
            | 0x3000..=0x303F // 、。〈〉《》「」
            | 0xFF01..=0xFF0F // ！＂＃＄％＆＇（）＊＋，－．／
            | 0xFF1A..=0xFF20 // ：；＜＝＞？
            | 0xFF3B..=0xFF40 // ［＼］＾＿｀
            | 0xFF5B..=0xFF65 // ｛｜｝～…
            | 0xFFE0..=0xFFE6 // ￥
    )
}

/// The key an input method may answer for, if this is one of them.
fn sent_key(c: Option<char>) -> Option<Sent> {
    match c? {
        ' ' => Some(Sent::Space),
        c if c.is_ascii_punctuation() => Some(Sent::Punct(c)),
        _ => None,
    }
}

/// How much of what we already sent the IME is about to take back.
///
/// Pinyin letters arrive as key events before the composition starts, so the
/// text is already in the pty. Only the letters this composition actually
/// typed belong to it — the longest tail of `sent` that the composition
/// begins with. Retracting all of `sent` deleted text typed in another input
/// mode, e.g. English typed before switching back to Chinese.
fn retract_len(sent: &str, composition: &str) -> usize {
    let sent: Vec<char> = sent.chars().flat_map(|c| c.to_lowercase()).collect();
    let composition: Vec<char> = composition.chars().flat_map(|c| c.to_lowercase()).collect();
    let max = sent.len().min(composition.len());
    for n in (1..=max).rev() {
        if composition.starts_with(&sent[sent.len() - n..]) {
            return n;
        }
    }
    0
}

/// The link that covers one character of a line, if there is one.
fn url_token(chars: &[char], index: usize) -> Option<String> {
    if chars.is_empty() || index >= chars.len() || chars[index].is_whitespace() {
        return None;
    }
    let mut from = index;
    while from > 0 && !chars[from - 1].is_whitespace() {
        from -= 1;
    }
    let mut to = index;
    while to + 1 < chars.len() && !chars[to + 1].is_whitespace() {
        to += 1;
    }
    let token: String = chars[from..=to].iter().collect();
    let token = token.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | ')' | ']' | '}' | '"' | '\'' | '>'
        )
    });
    if token.starts_with("http://") || token.starts_with("https://") {
        Some(token.to_owned())
    } else {
        token
            .strip_prefix("www.")
            .map(|rest| format!("https://www.{rest}"))
    }
}

/// Every occurrence of the query in one row, as (first char, char count).
/// Matching is per row, so a query never spans a line break.
fn find_all(text: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let haystack = if case_sensitive {
        text.to_owned()
    } else {
        text.to_lowercase()
    };
    // Lowercasing can change the char count (İ becomes i + combining dot), which would put the
    // highlight on the wrong cells, so skip such a row rather than mislead.
    if haystack.chars().count() != text.chars().count() {
        return Vec::new();
    }
    let needle: Vec<char> = if case_sensitive {
        needle.chars().collect()
    } else {
        needle.to_lowercase().chars().collect()
    };
    let haystack: Vec<char> = haystack.chars().collect();
    let mut found = Vec::new();
    let mut index = 0;
    while index + needle.len() <= haystack.len() {
        if haystack[index..index + needle.len()] == needle[..] {
            found.push((index, needle.len()));
        }
        index += 1;
    }
    found
}

/// The plain text of a row, and the cell column each character starts at.
fn row_cells(row: &Row) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for run in &row.runs {
        // shortcut: Run widths count graphemes, not scalars; skip ambiguous rows
        // until the public frame exposes grapheme boundaries.
        if run.text.chars().count() != run.widths.len() {
            return (String::new(), Vec::new());
        }
        let mut column = run.col;
        for (index, ch) in run.text.chars().enumerate() {
            text.push(ch);
            columns.push(column);
            column += u16::from(*run.widths.get(index).unwrap_or(&1));
        }
    }
    (text, columns)
}

/// Where one match sits in the buffer.
#[derive(Clone, Copy)]
struct Match {
    /// Buffer row, counting from the oldest line.
    row: u64,
    /// First and last cell of the match.
    from: u16,
    to: u16,
}

/// An open find bar: the query, the buffer lines that contain it, and which one
/// is being shown.
#[derive(Default)]
struct Composition {
    active: bool,
    canceled_search: bool,
    marked_utf16: usize,
    pending_result: bool,
    key_echo: Option<String>,
}

impl Composition {
    fn start_typing(&mut self) {
        if self.canceled_search {
            self.canceled_search = false;
            self.pending_result = false;
        }
    }
    fn mark(&mut self, text: &str) {
        if self.canceled_search { return; }
        self.marked_utf16 = text.encode_utf16().count();
        self.active = !text.is_empty();
        if self.active {
            self.pending_result = true;
            self.canceled_search = false;
            self.key_echo = None;
        }
    }
    fn should_type(&mut self, composing: bool, text: &str) -> bool {
        let duplicate = self.key_echo.take().as_deref() == Some(text);
        composing || (!duplicate && commit_needs_typing(false, text))
    }
    fn marked_range(&self) -> Option<std::ops::Range<usize>> {
        self.active.then_some(0..self.marked_utf16)
    }
    fn close_search(&mut self) {
        self.canceled_search |= self.active;
        self.active = false;
    }
    fn commit(&mut self, text: &str) -> Option<bool> {
        if self.canceled_search && text.is_empty() {
            return None;
        }
        let composing = self.active || self.pending_result;
        self.active = false;
        self.pending_result = false;
        if std::mem::take(&mut self.canceled_search) {
            None
        } else {
            Some(composing)
        }
    }
}

/// A press that may become a text selection.
struct Selecting {
    /// The cell the press landed on, the anchor of the selection.
    from: (u16, u16),
    /// Where the pointer was, in pixels, to tell a drag from a click.
    pressed: gpui::Point<gpui::Pixels>,
    /// Whether it has moved far enough to be a drag.
    dragging: bool,
}

/// Whether the pointer has moved far enough for this to be a drag rather than a
/// click: switching panes jitters a pixel or two, and that must select nothing.
fn is_drag(pressed: gpui::Point<gpui::Pixels>, at: gpui::Point<gpui::Pixels>) -> bool {
    const SLOP: f32 = 4.0;
    (f32::from(at.x) - f32::from(pressed.x)).abs() > SLOP
        || (f32::from(at.y) - f32::from(pressed.y)).abs() > SLOP
}

#[derive(Default)]
struct ForwardedKeys {
    keys: HashMap<String, (gpui::Keystroke, Vec<String>)>,
}

impl ForwardedKeys {
    fn press(&mut self, key: gpui::Keystroke, prefix: Vec<String>) {
        self.keys.insert(key.key.clone(), (key, prefix));
    }
    fn repeat(&self, key: &str) -> Option<&(gpui::Keystroke, Vec<String>)> {
        self.keys.get(key)
    }
    fn release(&mut self, key: &str) -> Option<(gpui::Keystroke, Vec<String>)> {
        self.keys.remove(key)
    }
}

struct Find {
    query: String,
    key_echo: Option<String>,
    matches: Vec<Match>,
    current: usize,
    case_sensitive: bool,
}

fn mouse_button(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Middle => Some("middle"),
        MouseButton::Right => Some("right"),
        _ => None,
    }
}

/// Named keys are not query text ("left" must never become four letters).
fn query_key_text<'a>(key: &'a str, text: Option<&'a str>) -> Option<&'a str> {
    let text = text.unwrap_or(if key == "space" { " " } else { key });
    (!text.is_empty()
        && !text.chars().any(char::is_control)
        && (text.chars().count() == 1 || key != text))
        .then_some(text)
}

impl Find {
    fn mark(&mut self, sent: &str, composition: &str) {
        self.key_echo = None;
        let take = retract_len(sent, composition);
        for _ in 0..take {
            self.query.pop();
        }
    }

    fn commit(&mut self, composing: bool, text: &str, sent: Option<Sent>) {
        let echo = self.key_echo.take();
        if !composing && echo.as_deref() == Some(text) {
            return;
        }
        if replaces_sent(sent, text) {
            let plain = match sent {
                Some(Sent::Punct(c)) => Some(c),
                Some(Sent::Space) => Some(' '),
                None => None,
            };
            if plain.is_some() && self.query.chars().last() == plain {
                self.query.pop();
            }
        }
        self.query.extend(text.chars().filter(|c| !c.is_control()));
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            session.close();
        }
    }
}

impl TerminalView {
    pub fn new(
        engine: &Engine,
        server: &str,
        session: &str,
        block: &str,
        font: SharedString,
        font_size: f32,
        prefix: Arc<std::sync::atomic::AtomicBool>,
        over_picture: bool,
        background_opacity: f32,
        scrollback_lines: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let opened = engine.open_terminal(server, session, block, Arc::new(Notifier(tx)));
        let (session, error) = match opened {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        };
        if let Some(session) = &session {
            session.set_theme(Some(THEME));
        }
        if session.is_some() {
            cx.spawn(async move |this, cx| {
                while let Ok(wake) = rx.recv().await {
                    if this
                        .update(cx, |_, cx| {
                            if let Wake::Actions(actions) = wake {
                                for (name, args) in actions {
                                    if shortcuts::configured_action(&name, &args).is_some() {
                                        cx.emit(TerminalEvent::ConfiguredAction { name, args });
                                    }
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        Self {
            composition: Composition::default(),
            prefix,
            selecting: None,
            mouse_buttons: Vec::new(),
            control_released: false,
            focused: false,
            forwarded: ForwardedKeys::default(),
            keybindings: None,
            geometry: Arc::new(std::sync::Mutex::new((
                gpui::Point::default(),
                Metrics {
                    cell_w: 8.0,
                    cell_h: 16.0,
                },
            ))),
            scrollback_lines: scrollback_lines.max(1),
            find: None,
            over_picture,
            background_opacity,
            leaked: String::new(),
            last_punct: None,
            ime_answer: None,
            session,
            error,
            frame: empty_frame(),
            focus: cx.focus_handle(),
            palette: paint::Palette {
                family: font,
                font_size: px(font_size),
            },
        }
    }

    pub fn handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Size the remote terminal from this view. Called for the pane that holds
    /// the keyboard rather than from a click, because a pane opened by a
    /// shortcut — a tool from the prefix menu, a fresh split — never sees one,
    /// and a terminal without control is never resized when it is zoomed.
    pub fn take_control(&self) {
        if self.control_released {
            return;
        }
        if let Some(session) = &self.session {
            if !session.has_control() {
                session.clone().take_control();
            }
        }
    }

    pub fn set_keybindings(&mut self, bindings: Arc<std::sync::Mutex<KeyBindings>>) {
        self.keybindings = Some(bindings);
    }

    pub fn force_control(&mut self) {
        self.control_released = false;
        if let Some(session) = &self.session {
            session.take_control();
        }
    }

    pub fn request_control(&mut self) {
        // An explicit request must not become a forced take on the next paint.
        self.control_released = true;
        if let Some(session) = &self.session {
            session.request_control();
        }
    }

    pub fn release_control(&mut self) {
        self.control_released = true;
        if let Some(session) = &self.session {
            session.release_control();
        }
    }

    pub fn set_background(&mut self, over_picture: bool, opacity: f32, cx: &mut Context<Self>) {
        self.over_picture = over_picture;
        self.background_opacity = opacity;
        cx.notify();
    }

    pub fn set_scrollback(&mut self, lines: u64, cx: &mut Context<Self>) {
        self.scrollback_lines = lines.max(1);
        cx.notify();
    }

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.palette.font_size = px(size);
        cx.notify();
    }

    /// Put the selection on the clipboard. Releasing the mouse already copies,
    /// so this is for a keyboard-only copy of a selection made earlier.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let Some(text) = session.selection_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    /// Type the clipboard into the terminal, line breaks and all.
    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        if let Some(find) = &mut self.find {
            find.key_echo = None;
            find.query.push_str(text);
            self.leaked.clear();
            self.last_punct = None;
            self.refind(cx);
            return;
        }
        let Some(session) = &self.session else { return };
        // `paste`, not `send_text`: a program that turned on bracketed paste
        // (mode 2004 — vim, agents, readline) needs the text wrapped, or it
        // treats every newline as Enter and runs half the paste.
        session.paste(text);
    }

    /// The link under a cell, if the text there is one.
    ///
    /// shrew-core keeps runs of styled text, not the OSC 8 hyperlink a program
    /// may have attached, so this reads the visible line and takes the token
    /// that looks like a URL. Ctrl-click is the usual gesture, and the shell
    /// never grabs plain clicks for it.
    fn url_at(&self, col: u16, row: u16) -> Option<String> {
        let line = self.frame.lines.get(row as usize)?;
        let (text, starts) = row_cells(line);
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return None;
        }
        // The character under the pointer: the last cell that starts at or before it.
        let mut index = 0;
        for (i, start) in starts.iter().enumerate() {
            if *start <= col {
                index = i;
            } else {
                break;
            }
        }
        url_token(&chars, index)
    }

    /// Open the find bar, or move to the next match if it is already open.
    pub fn start_find(&mut self, cx: &mut Context<Self>) {
        match &mut self.find {
            // The bar is open, so the same chord closes it.
            Some(_) => self.close_find(cx),
            None => {
                self.leaked.clear();
                self.last_punct = None;
                self.find = Some(Find {
                    query: String::new(),
                    key_echo: None,
                    matches: Vec::new(),
                    current: 0,
                    case_sensitive: false,
                });
                cx.notify();
            }
        }
    }

    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find = None;
        self.composition.close_search();
        self.leaked.clear();
        self.last_punct = None;
        if let Some(session) = &self.session {
            session.clear_selection();
        }
        cx.notify();
    }

    /// Re-run the query over the buffer. The core has no search, so walk the
    /// scrollback a screen at a time and read each page's rows.
    fn refind(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(find) = &self.find else { return };
        let (needle, case) = (find.query.clone(), find.case_sensitive);
        let mut matches = Vec::new();
        if !needle.is_empty() {
            session.scroll_to_bottom();
            let mut bar = session.frame().scrollbar;
            let latest = bar.total.saturating_sub(bar.len);
            let top = latest.saturating_sub(self.scrollback_lines.min(latest));
            loop {
                let frame = session.frame();
                bar = frame.scrollbar;
                for (index, row) in frame.lines.iter().enumerate() {
                    if bar.offset + (index as u64) < top {
                        continue;
                    }
                    let (text, columns) = row_cells(row);
                    for (start, len) in find_all(&text, &needle, case) {
                        let (Some(&from), Some(&to)) =
                            (columns.get(start), columns.get(start + len - 1))
                        else {
                            continue;
                        };
                        matches.push(Match {
                            row: bar.offset + index as u64,
                            from,
                            to: columns
                                .get(start + len)
                                .copied()
                                .unwrap_or_else(|| {
                                    row.runs
                                        .last()
                                        .map(|run| run.col.saturating_add(run.cells))
                                        .unwrap_or(to + 1)
                                })
                                .saturating_sub(1),
                        });
                    }
                }
                if bar.offset <= top || bar.len == 0 {
                    break;
                }
                let step = (bar.offset - top).min(bar.len) as i32;
                session.scroll(-step);
            }
            // The scan left the view at the top of the buffer; put it back.
            session.scroll_to_bottom();
            self.frame = session.frame();
        }
        if let Some(find) = &mut self.find {
            matches.sort_by_key(|m| (m.row, m.from, m.to));
            matches.dedup_by_key(|m| (m.row, m.from, m.to));
            find.matches = matches;
            find.current = 0;
        }
        self.find_next(cx);
    }

    /// Scroll to the current match and highlight the matched characters.
    fn find_next(&mut self, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let Some(find) = &mut self.find else { return };
        let Some(found) = find.matches.get(find.current).copied() else {
            session.clear_selection();
            cx.notify();
            return;
        };
        let bar = session.frame().scrollbar;
        // Put the match near the middle of the view when it is far away.
        let wanted = found.row.saturating_sub(bar.len / 2);
        let latest = bar.total.saturating_sub(bar.len);
        let earliest = latest.saturating_sub(self.scrollback_lines);
        let target = wanted.clamp(earliest, latest);
        let delta = target as i64 - bar.offset as i64;
        if delta != 0 {
            session.scroll(delta as i32);
        }
        self.frame = session.frame();
        // Highlight just the matched cells, once the viewport has moved.
        let row = found.row.saturating_sub(self.frame.scrollbar.offset) as u16;
        session.select_range((found.from, row), (found.to, row));
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else { return };
        if find.matches.is_empty() {
            return;
        }
        let count = find.matches.len();
        find.current = if forward {
            (find.current + 1) % count
        } else {
            (find.current + count - 1) % count
        };
        self.find_next(cx);
    }

    fn toggle_case(&mut self, cx: &mut Context<Self>) {
        if let Some(find) = &mut self.find {
            find.case_sensitive = !find.case_sensitive;
        }
        self.refind(cx);
    }

    /// Keys while the find bar is open belong to it, not to the pty.
    fn find_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(find) = &mut self.find else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;
        if self.composition.active {
            return true;
        }
        if key == "escape" {
            self.close_find(cx);
            return true;
        }
        if key == "enter" {
            self.find_step(!modifiers.shift, cx);
            return true;
        }
        if key == "f3" {
            self.find_step(!modifiers.shift, cx);
            return true;
        }
        if key == "up" {
            self.find_step(false, cx);
            return true;
        }
        if key == "down" {
            self.find_step(true, cx);
            return true;
        }
        // Anything with control or command is a shortcut for the app, not a
        // character for the query; let it through so the window keeps working.
        // Ctrl-Shift-H reaches the window's own handler, which toggles the bar
        // closed; closing it here as well would only reopen it.
        if modifiers.control || modifiers.platform {
            return false;
        }
        if key == "backspace" {
            find.query.pop();
            find.key_echo = None;
            self.leaked.clear();
            self.last_punct = None;
            self.refind(cx);
            return true;
        }
        if !modifiers.alt {
            if let Some(text) = query_key_text(key, event.keystroke.key_char.as_deref()) {
                if std::env::var_os("FLEX_DEBUG").is_some() {
                    eprintln!("[find] key {text:?} for {key:?}");
                }
                find.key_echo = Some(text.to_owned());
                find.query.push_str(text);
                if text.chars().all(|c| c.is_ascii_alphabetic()) {
                    self.leaked.push_str(text);
                } else {
                    self.leaked.clear();
                }
                self.last_punct = sent_key(text.chars().next());
                self.refind(cx);
                return true;
            }
        }
        // Anything else is still swallowed: it is not terminal input while the
        // bar is open.
        true
    }

    /// The cell under a pointer position, given the pane's origin and metrics.
    fn cell_at(
        origin: gpui::Point<gpui::Pixels>,
        metrics: Metrics,
        at: gpui::Point<gpui::Pixels>,
    ) -> (u16, u16) {
        let x = (f32::from(at.x) - f32::from(origin.x)).max(0.0);
        let y = (f32::from(at.y) - f32::from(origin.y)).max(0.0);
        let col = (x / metrics.cell_w.max(1.0) as f32).floor() as u16;
        let row = (y / metrics.cell_h.max(1.0) as f32).floor() as u16;
        (col, row)
    }

    fn mouse_cell(&self, at: gpui::Point<gpui::Pixels>) -> (u16, u16) {
        let (origin, metrics) = *self.geometry.lock().unwrap();
        let (col, row) = Self::cell_at(origin, metrics, at);
        (
            col.min(self.frame.cols.saturating_sub(1)),
            row.min(self.frame.rows.saturating_sub(1)),
        )
    }

    fn forward_mouse(
        &self,
        action: &str,
        button: Option<MouseButton>,
        at: (u16, u16),
        mods: gpui::Modifiers,
    ) {
        let Some(session) = &self.session else { return };
        let buttons: Vec<_> = self
            .mouse_buttons
            .iter()
            .filter_map(|b| mouse_button(*b))
            .collect();
        session.mouse_event(
            action,
            button.and_then(mouse_button),
            &buttons,
            at.0,
            at.1,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        );
    }

    fn mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The press that gives this pane the keyboard belongs to the chrome: a
        // program that acts on clicks (a chat TUI copies the message under the
        // pointer) must not see the click that only switched panes.
        let switching = !self.focused;
        window.focus(&self.focus);
        self.take_control();
        if switching {
            // Nothing else: the press that gives this pane the keyboard belongs
            // to the chrome. Forwarding it lets a program act on a click meant
            // to switch panes, and starting a selection on it turns a slightly
            // moving click into a copied swath of text.
            if std::env::var_os("FLEX_DEBUG").is_some() {
                eprintln!("[mouse] switch: focus only");
            }
            cx.notify();
            return;
        }
        if self.find.is_some() {
            self.close_find(cx);
        }
        let cell = self.mouse_cell(event.position);
        if event.button == MouseButton::Left
            && (event.modifiers.control || event.modifiers.platform)
        {
            if let Some(url) = self.url_at(cell.0, cell.1) {
                crate::win32::open_url(&url);
                cx.stop_propagation();
                return;
            }
        }
        if !event.modifiers.shift && self.session.as_ref().is_some_and(|s| s.mouse_tracking()) {
            if std::env::var_os("FLEX_DEBUG").is_some() {
                eprintln!("[mouse] forward press {cell:?} (the program tracks the mouse)");
            }
            if !self.mouse_buttons.contains(&event.button) {
                self.mouse_buttons.push(event.button);
            }
            self.forward_mouse("press", Some(event.button), cell, event.modifiers);
            cx.notify();
        } else if event.button == MouseButton::Left {
            if std::env::var_os("FLEX_DEBUG").is_some() {
                eprintln!("[mouse] down cell={cell:?} at={:?}", event.position);
            }
            self.begin_selection(cell, event.position, cx);
        }
    }

    fn mouse_up(&mut self, event: &gpui::MouseUpEvent, cx: &mut Context<Self>) {
        if self.mouse_buttons.contains(&event.button) {
            self.mouse_buttons.retain(|b| *b != event.button);
            // Release even if Shift was pressed mid-drag: don't leave a remote button stuck.
            self.forward_mouse(
                "release",
                Some(event.button),
                self.mouse_cell(event.position),
                event.modifiers,
            );
            cx.notify();
        } else if event.button == MouseButton::Left && self.selecting.is_some() {
            self.drag_selection(event.position, cx);
            self.finish_selection(cx);
        }
    }

    fn mouse_move(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if self.selecting.is_some() {
            // A click can be quicker than a frame, and then the release never
            // reaches the overlay that waits for it. Without this, the stale
            // anchor turns the next pointer movement into a selection dragged
            // across half the screen.
            if std::env::var_os("FLEX_DEBUG").is_some() {
                eprintln!("[mouse] move pressed={:?}", event.pressed_button);
            }
            if event.pressed_button != Some(MouseButton::Left) {
                self.cancel_selection(cx);
                return;
            }
            self.drag_selection(event.position, cx);
        } else if !event.modifiers.shift
            && self.session.as_ref().is_some_and(|s| s.mouse_tracking())
        {
            self.forward_mouse(
                "move",
                self.mouse_buttons.last().copied(),
                self.mouse_cell(event.position),
                event.modifiers,
            );
        }
    }

    fn begin_selection(
        &mut self,
        at: (u16, u16),
        pressed: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.selecting = Some(Selecting {
            from: at,
            pressed,
            dragging: false,
        });
        cx.notify();
    }

    fn extend_selection(&mut self, from: (u16, u16), to: (u16, u16), cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.select_range(from, to);
        }
        cx.notify();
    }

    /// Finish a drag: put the selection on the clipboard, the way a terminal does.
    /// While a selection is being dragged, extend it with the pointer.
    fn drag_selection(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some(selecting) = &mut self.selecting else {
            return;
        };
        let from = selecting.from;
        if !selecting.dragging {
            if !is_drag(selecting.pressed, at) {
                return;
            }
            if std::env::var_os("FLEX_DEBUG").is_some() {
                eprintln!("[mouse] drag starts from={from:?} at={at:?}");
            }
            selecting.dragging = true;
            if let Some(session) = &self.session {
                session.clear_selection();
                session.select_range(from, from);
            }
        }
        let (origin, metrics) = *self.geometry.lock().unwrap();
        let to = TerminalView::cell_at(origin, metrics, at);
        if to != from {
            self.extend_selection(from, to, cx);
        }
    }

    /// Drop a selection that nothing is holding down any more.
    fn cancel_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = None;
        if let Some(session) = &self.session {
            session.clear_selection();
        }
        cx.notify();
    }

    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selecting) = self.selecting.take() else {
            return;
        };
        let Some(session) = &self.session else { return };
        if std::env::var_os("FLEX_DEBUG").is_some() {
            let text = session.selection_text().unwrap_or_default();
            eprintln!(
                "[mouse] finish dragging={} text={:?}",
                selecting.dragging,
                text.chars().take(40).collect::<String>()
            );
        }
        if !selecting.dragging {
            // A click, not a drag: it switched panes and nothing is selected.
            session.clear_selection();
            cx.notify();
            return;
        }
        let Some(text) = session.selection_text() else {
            session.clear_selection();
            cx.notify();
            return;
        };
        if !text.is_empty() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        session.clear_selection();
        cx.notify();
    }

    /// Scroll by `lines` notches, never further back than the configured cap.
    fn wheel(&mut self, notches: f32, at: (u16, u16), shift: bool, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        // A program that tracks the mouse — pi, vim, less — scrolls itself, and
        // the alternate screen it draws in has no scrollback to move anyway.
        if session.mouse_tracking() && !shift {
            session.wheel(at.0, at.1, -notches * 3.0);
            return;
        }
        let bar = self.frame.scrollbar;
        // One notch is three lines, the usual terminal step.
        let delta = -(notches * 3.0) as i64;
        let latest = bar.total.saturating_sub(bar.len) as i64;
        let earliest = (latest as u64).saturating_sub(self.scrollback_lines) as i64;
        let want = (bar.offset as i64 + delta).clamp(earliest, latest);
        let actual = want - bar.offset as i64;
        if actual != 0 {
            session.scroll(actual as i32);
            cx.notify();
        }
    }

    /// The pane's whole screen and scrollback, as text.
    pub fn text(&self) -> String {
        self.session
            .as_ref()
            .map(|session| session.all_text())
            .unwrap_or_default()
    }

    /// Type text into the pty, as a paste would.
    pub fn send(&self, text: &str) {
        if let Some(session) = &self.session {
            session.send_text(text);
        }
    }

    /// Bubbled terminal keys must reach Windows translation, not the chrome's fallback bindings.
    pub fn owns_keyboard(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
            && self.find.is_none()
            && (self.keybindings.is_some() || self.composition.active)
    }

    fn press(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.find.is_some() {
            self.find_key(event, cx);
            return;
        }
        if !event.keystroke.modifiers.control && !event.keystroke.modifiers.alt && !event.keystroke.modifiers.platform
            && keymap::translate(&event.keystroke).is_some_and(|p| matches!(p.key, shrew_core::Key::Char(c) if !c.is_whitespace()))
        {
            self.composition.start_typing();
        }
        // While an IME is composing, the keys belong to it.
        if self.composition.active {
            return;
        }
        // An armed shell prefix belongs to the workspace, even with a loaded map.
        if self.prefix.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // The commit beat the keystroke here: that key belongs to the input
        // method and must not reach the terminal as well.
        if let (Some(answer), Some(p)) = (self.ime_answer.take(), keymap::translate(&event.keystroke))
        {
            if let shrew_core::Key::Char(c) = p.key {
                if !p.ctrl && !p.alt && !p.meta && answer.answers(c) {
                    if std::env::var_os("FLEX_DEBUG").is_some() {
                        eprintln!("[ime] dropped {c:?} — already answered with {:?}", answer.kind);
                    }
                    cx.stop_propagation();
                    return;
                }
            }
        }
        if event.is_held {
            let mut translate = false;
            if let Some((stroke, prefix)) = self.forwarded.repeat(&event.keystroke.key) {
                translate = prefix.is_empty();
                if let (Some(p), Some(session)) = (keymap::translate(stroke), &self.session) {
                    self.composition.key_echo = match p.key {
                        shrew_core::Key::Char(c) if !p.ctrl && !p.alt && !p.meta => Some(c.to_string()),
                        _ => None,
                    };
                    session.send_key_event(
                        p.key,
                        (p.ctrl, p.alt, p.shift, p.meta),
                        "repeat",
                        prefix,
                    );
                }
            }
            if !translate {
                cx.stop_propagation();
            }
            return;
        }
        self.composition.key_echo = None;
        // A new physical key after an empty preedit is not the previous IME result.
        self.composition.pending_result = false;
        let mut sequence = None;
        if let Some(bindings) = &self.keybindings {
            let resolved = bindings.lock().unwrap().resolve_for_creator(
                &keymap::binding_key(&event.keystroke),
                "com.superlogical.terminal",
                |name, args| shortcuts::configured_action(name, args).is_some(),
            );
            match resolved {
                Resolution::Consumed => {
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                Resolution::Action { name, args } => {
                    self.leaked.clear();
                    self.last_punct = None;
                    if shortcuts::configured_action(&name, &args).is_some() {
                        cx.emit(TerminalEvent::ConfiguredAction { name, args });
                    } else if let Some(session) = &self.session {
                        session.send_action(&name, args);
                    }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                Resolution::Forward { prefix } => {
                    sequence = Some(prefix);
                    cx.stop_propagation();
                }
                Resolution::Pass => {}
            }
            // GPUI calls TranslateMessage only for propagated keys; unbound typing must pass.
            cx.notify();
        } else {
            if shortcuts::lookup(&event.keystroke).is_some() {
                return;
            }
        }
        let Some(session) = &self.session else { return };
        match keymap::translate(&event.keystroke) {
            Some(p) => {
                let plain_letter = !p.ctrl
                    && !p.alt
                    && !p.meta
                    && matches!(p.key, shrew_core::Key::Char(c) if c.is_ascii_alphabetic());
                if !plain_letter {
                    // Anything that is not a bare letter ends the pinyin run, so
                    // nothing is left to retract.
                    self.leaked.clear();
                } else if let shrew_core::Key::Char(c) = p.key {
                    self.leaked.push(c);
                }
                self.last_punct = match (p.key, p.ctrl || p.alt || p.meta) {
                    (shrew_core::Key::Char(c), false) => sent_key(Some(c)),
                    _ => None,
                };
                self.composition.key_echo = match p.key {
                    shrew_core::Key::Char(c) if !p.ctrl && !p.alt && !p.meta => Some(c.to_string()),
                    _ => None,
                };
                let prefix = sequence.unwrap_or_default();
                if std::env::var_os("FLEX_DEBUG").is_some() {
                    eprintln!(
                        "[key] send {:?} ctrl={} alt={} meta={}",
                        p.key, p.ctrl, p.alt, p.meta
                    );
                }
                session.send_key_event(p.key, (p.ctrl, p.alt, p.shift, p.meta), "press", &prefix);
                self.forwarded.press(event.keystroke.clone(), prefix);
            }
            None => {
                if std::env::var_os("FLEX_DEBUG").is_some() {
                    let k = &event.keystroke;
                    eprintln!(
                        "[key] dropped key={:?} char={:?} mods={:?}",
                        k.key, k.key_char, k.modifiers
                    );
                }
            }
        }
    }
    fn release(&mut self, event: &KeyUpEvent) {
        let Some((stroke, prefix)) = self.forwarded.release(&event.keystroke.key) else {
            return;
        };
        if let (Some(p), Some(session)) = (keymap::translate(&stroke), &self.session) {
            session.send_key_event(p.key, (p.ctrl, p.alt, p.shift, p.meta), "release", &prefix);
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "terminal closed".into());
            return div().size_full().p_2().child(message).into_any_element();
        };

        let focused = self.focus.is_focused(window);
        if self.focused != focused {
            self.focused = focused;
            session.focus_event(focused);
        }
        self.frame = session.frame();
        // FLEX_DEBUG=1 explains an empty pane: no frames means the stream is
        // dead, frames with no lines means the snapshot failed to restore.
        if debug() {
            let f = &self.frame;
            eprintln!(
                "[frame] {}x{} lines={} cursor={:?}",
                f.cols,
                f.rows,
                f.lines.len(),
                f.cursor
            );
        }
        let frame = self.frame.clone();
        let palette = paint::Palette {
            family: self.palette.family.clone(),
            font_size: self.palette.font_size,
        };
        let prepaint_palette = palette.clone();
        let prepaint_session = session.clone();
        let geometry = self.geometry.clone();
        // The find bar floats over the top of the pane, so the grid never shifts
        // under it while typing.
        let find_bar = self.find.is_some().then(|| {
            let find = self.find.as_ref().unwrap();
            let placeholder = find.query.is_empty();
            let count = if find.matches.is_empty() {
                format!("0 / 0")
            } else {
                format!("{} / {}", find.current + 1, find.matches.len())
            };
            let query = if placeholder {
                "查找文本".to_owned()
            } else {
                find.query.clone()
            };
            let field = div()
                .flex_1()
                .truncate()
                .text_sm()
                .text_color(if placeholder {
                    gpui::rgb(0x78808f)
                } else {
                    gpui::rgb(0xe4e9f2)
                })
                .child(query);
            let counter = div().text_xs().text_color(gpui::rgb(0xa9b1c1)).child(count);
            let button = |id: &'static str,
                          label: String,
                          active: bool,
                          cx: &mut Context<Self>,
                          f: fn(&mut Self, &mut Context<Self>)| {
                div()
                    .id(id)
                    .h(px(20.0))
                    .min_w(px(20.0))
                    .px_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_xs()
                    .when(active, |el| el.text_color(gpui::rgb(0xf0b429)))
                    .when(!active, |el| el.text_color(gpui::rgb(0x9aa3b2)))
                    .hover(|el| el.bg(gpui::rgb(0x353a47)).text_color(gpui::rgb(0xe4e9f2)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| f(this, cx)))
                    .into_any_element()
            };
            let up = button("find-prev", "↑".to_owned(), false, cx, |this, cx| {
                this.find_step(false, cx)
            });
            let down = button("find-next", "↓".to_owned(), false, cx, |this, cx| {
                this.find_step(true, cx)
            });
            let case = button(
                "find-case",
                "Aa".to_owned(),
                find.case_sensitive,
                cx,
                |this, cx| this.toggle_case(cx),
            );
            let close = button("find-close", "×".to_owned(), false, cx, |this, cx| {
                this.close_find(cx)
            });

            div()
                .absolute()
                .top(px(6.0))
                .right(px(8.0))
                .w(px(360.0))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(30.0))
                .rounded_md()
                .bg(gpui::rgb(0x21242e))
                .border_1()
                .border_color(gpui::rgb(0x3a3f52))
                .shadow_lg()
                .child(field)
                .child(counter)
                .child(up)
                .child(down)
                .child(case)
                .child(close)
                .into_any_element()
        });
        let binding_status = self.keybindings.as_ref().and_then(|bindings| {
            let bindings = bindings.lock().unwrap();
            let prefix = bindings.pending().join(" → ");
            match (bindings.mode(), prefix.is_empty()) {
                (Some(mode), false) => Some(format!("{mode}: {prefix}")),
                (Some(mode), true) => Some(mode.to_owned()),
                (None, false) => Some(prefix),
                (None, true) => None,
            }
        });
        let status_bar = match session.status() {
            TerminalStatus::Live => binding_status,
            TerminalStatus::Connecting => Some("Connecting…".to_owned()),
            TerminalStatus::Reconnecting => Some("Reconnecting…".to_owned()),
            TerminalStatus::Exited { code } => Some(format!("Exited ({code})")),
            TerminalStatus::Ended { reason } => Some(format!("Ended: {reason}")),
        }
        .map(|text| {
            div()
                .absolute()
                .bottom(px(6.0))
                .left(px(8.0))
                .px_2()
                .py_1()
                .rounded_sm()
                .bg(gpui::rgb(0x21242e))
                .text_xs()
                .text_color(gpui::rgb(0xf0b429))
                .child(text)
        });
        let view = cx.entity();
        let focus = self.focus.clone();
        let over_picture = self.over_picture;
        let picture_opacity = self.background_opacity;

        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Debug hook: exercises the IME commit path without a real IME.
                if debug() && event.keystroke.key == "f12" {
                    this.replace_text_in_range(None, "中文测试", window, cx);
                    return;
                }
                this.press(event, cx)
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| this.release(event)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| this.mouse_up(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| this.mouse_down(event, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event, window, cx| this.mouse_down(event, window, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event, window, cx| this.mouse_down(event, window, cx)),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.selecting.is_none() && this.mouse_buttons.is_empty() {
                    this.mouse_move(event, cx);
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                // Vertical wheel scrolls the scrollback; horizontal is ignored.
                let notches: f32 = match event.delta {
                    gpui::ScrollDelta::Lines(lines) => lines.y,
                    gpui::ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 44.0,
                };
                let (origin, metrics) = *this.geometry.lock().unwrap();
                let at = TerminalView::cell_at(origin, metrics, event.position);
                this.wheel(notches, at, event.modifiers.shift, cx);
            }))
            .when(
                self.selecting.is_some() || !self.mouse_buttons.is_empty(),
                |el| {
                    let view = cx.entity().downgrade();
                    el.child(
                        canvas(
                            |_, _, _| (),
                            move |_, _, window, _cx| {
                                let up_view = view.clone();
                                window.on_mouse_event(
                                    move |event: &gpui::MouseUpEvent, phase, _, cx| {
                                        if phase == gpui::DispatchPhase::Bubble {
                                            let _ =
                                                up_view.update(cx, |me, cx| me.mouse_up(event, cx));
                                        }
                                    },
                                );
                                let this = view.clone();
                                window.on_mouse_event(
                                    move |event: &gpui::MouseMoveEvent, _phase, _window, cx| {
                                        if _phase == gpui::DispatchPhase::Bubble {
                                            let _ =
                                                this.update(cx, |me, cx| me.mouse_move(event, cx));
                                        }
                                    },
                                );
                            },
                        )
                        .absolute()
                        .top(px(0.0))
                        .left(px(0.0))
                        .size_full(),
                    )
                },
            )
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let w: f32 = bounds.size.width.into();
                        let h: f32 = bounds.size.height.into();
                        let metrics = measure(cx, &prepaint_palette);
                        let (cols, rows) = fit_grid(w as f64, h as f64, PAD as f64, metrics);
                        prepaint_session.set_device_grid(
                            cols,
                            rows,
                            metrics.cell_w.round() as u32,
                            metrics.cell_h.round() as u32,
                        );
                        metrics
                    },
                    move |bounds, metrics, window, cx| {
                        *geometry.lock().unwrap() = (
                            gpui::point(bounds.origin.x + px(PAD), bounds.origin.y + px(PAD)),
                            metrics,
                        );
                        // IME text never arrives as a keystroke: the platform hands
                        // it to whichever element registered an input handler for
                        // the focused element, so do that here, during paint.
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, view.clone()),
                            cx,
                        );
                        paint::paint(
                            &frame,
                            bounds,
                            metrics,
                            px(PAD),
                            &palette,
                            over_picture,
                            picture_opacity,
                            window,
                            cx,
                        )
                    },
                )
                .size_full(),
            )
            .children(find_bar)
            .children(status_bar)
            .into_any_element()
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// One cell's size in pixels for the current font, measured off a capital M.
fn measure(cx: &mut App, palette: &paint::Palette) -> Metrics {
    let text_system = cx.text_system().clone();
    let font_id = text_system.resolve_font(&gpui::font(palette.family.clone()));
    let advance: f32 = text_system
        .advance(font_id, palette.font_size, 'M')
        .map(|s| s.width.into())
        .unwrap_or(8.0);
    let ascent: f32 = text_system.ascent(font_id, palette.font_size).into();
    let descent: f32 = text_system.descent(font_id, palette.font_size).into();
    let glyph_box = ascent + descent;
    let font_size: f32 = palette.font_size.into();
    Metrics {
        cell_w: advance as f64,
        cell_h: (font_size * LINE_HEIGHT_EM).max(glyph_box + 1.0) as f64,
    }
}

fn debug() -> bool {
    std::env::var_os("FLEX_DEBUG").is_some()
}

fn empty_frame() -> Frame {
    Frame {
        cols: 0,
        rows: 0,
        background: shrew_core::Rgb::new(0, 0, 0),
        foreground: shrew_core::Rgb::new(0xdd, 0xdd, 0xdd),
        cursor_color: None,
        cursor: None,
        lines: Vec::new(),
        scrollbar: Default::default(),
        alternate_screen: false,
    }
}

/// Text that arrives from an input method — Chinese, Japanese, Korean — never
/// comes through `on_key_down`. The platform hands the composed string to the
/// focused element's input handler instead, and the terminal has no editable
/// buffer of its own, so the committed text is simply typed into the pty.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.composition.marked_range().map(|r| r.end).unwrap_or(0);
        Some(UTF16Selection { range: end..end, reversed: false })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.composition.marked_range()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.composition.active = false;
    }

    /// The committed string, after the candidate window closes.
    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(was_composing) = self.composition.commit(text) else {
            return;
        };
        let debug = std::env::var_os("FLEX_DEBUG").is_some();
        if let Some(find) = &mut self.find {
            find.commit(was_composing, text, self.last_punct.take());
            self.leaked.clear();
            self.refind(cx);
            return;
        }
        // Punctuation reaches us twice: Windows sends the plain key, then the
        // IME commits its full-width version. Take the plain one back.
        let sent = self.last_punct.take();
        let take_back = replaces_sent(sent, text);
        // Either the key came first (take it back) or it is still coming (drop it
        // when it arrives). Both orders are seen on the same Windows box.
        self.ime_answer = if take_back { None } else { pending_answer(text, was_composing) };
        if debug {
            // The ordering of a key and the commit it causes is what decides
            // whether the plain key can be taken back at all.
            eprintln!("[ime] commit {text:?} composing={was_composing} sent={sent:?} take_back={take_back}");
        }
        if !self.composition.should_type(was_composing, text) {
            return;
        }
        self.leaked.clear();
        if let Some(session) = &self.session {
            if take_back {
                session.send_text("\u{7f}");
            }
            session.send_text(text);
        }
    }

    /// The composition in progress. A terminal shows it in the IME's own window,
    /// not in the buffer, so there is nothing to do until it is committed.
    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        _new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.composition.canceled_search { return; }
        if _new_text.is_empty() {
            self.composition.mark("");
            return;
        }
        if self.find.is_some() {
            if !self.composition.active {
                self.find.as_mut().unwrap().mark(&self.leaked, _new_text);
                self.leaked.clear();
                self.last_punct = None;
                self.refind(cx);
            }
            self.composition.mark(_new_text);
            return;
        }
        if !self.composition.active {
            // The composition has just started. Take back the letters it typed
            // before this message arrived — and only those.
            let take = retract_len(&self.leaked, _new_text);
            if take > 0 {
                if let Some(session) = &self.session {
                    session.send_text(&"\u{7f}".repeat(take));
                }
            }
            self.leaked.clear();
        }
        // GPUI uses the marked range to route subsequent keys directly through IME.
        self.composition.mark(_new_text);
    }

    fn bounds_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _element_bounds: Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        let (origin, metrics) = *self.geometry.lock().unwrap();
        let (col, row) = self.frame.cursor.as_ref().map(|c| (c.col, c.row)).unwrap_or((0, 0));
        let origin = if self.find.is_some() {
            _element_bounds.origin
        } else {
            origin + gpui::point(px(col as f32 * metrics.cell_w as f32), px(row as f32 * metrics.cell_h as f32))
        };
        Some(Bounds::new(origin, gpui::size(px(metrics.cell_w.max(1.0) as f32), px(metrics.cell_h.max(1.0) as f32))))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{commit_needs_typing, is_cjk_punctuation, pending_answer, replaces_sent,
        retract_len, sent_key, url_token, Kind, Pending, Sent};

    fn line(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn a_click_that_switches_panes_is_not_a_drag() {
        use gpui::{point, px};
        let at = |x: f32, y: f32| point(px(x), px(y));
        // A click jitters a pixel or two; that must not select text.
        assert!(!super::is_drag(at(100.0, 50.0), at(101.0, 51.0)));
        assert!(!super::is_drag(at(100.0, 50.0), at(104.0, 54.0)));
        assert!(!super::is_drag(at(100.0, 50.0), at(100.0, 50.0)));
        // Crossing the slop in either axis is a drag.
        assert!(super::is_drag(at(100.0, 50.0), at(105.0, 50.0)));
        assert!(super::is_drag(at(100.0, 50.0), at(100.0, 45.0)));
    }

    #[test]
    fn platform_sees_marked_utf16_text_until_commit_or_cancel() {
        let mut composition = super::Composition::default();
        composition.mark("ni好🙂");
        assert_eq!(composition.marked_range(), Some(0..5));
        assert_eq!(composition.commit("你好"), Some(true));
        assert_eq!(composition.marked_range(), None);
        composition.mark("ni");
        composition.mark("");
        assert_eq!(composition.marked_range(), None);
    }

    #[test]
    fn empty_preedit_keeps_ascii_result_provenance() {
        let mut composition = super::Composition::default();
        composition.mark("abc");
        composition.mark("");
        assert_eq!(composition.marked_range(), None);
        assert_eq!(composition.commit("abc"), Some(true));
        assert_eq!(composition.commit("a"), Some(false));
    }

    #[test]
    fn new_composition_after_search_cancellation_is_not_discarded() {
        let mut composition = super::Composition::default();
        composition.mark("ni");
        composition.close_search();
        assert_eq!(composition.commit(""), None);
        composition.start_typing();
        composition.mark("hao");
        assert_eq!(composition.marked_range(), Some(0..3));
        assert_eq!(composition.commit("好"), Some(true));
    }

    #[test]
    fn layout_character_echo_is_not_typed_twice_but_ime_commit_is() {
        let mut composition = super::Composition::default();
        composition.key_echo = Some("é".into());
        assert!(!composition.should_type(false, "é"));
        assert!(composition.should_type(false, "中文"));
        composition.key_echo = Some("a".into());
        assert!(!composition.should_type(false, "a"));
        composition.mark("abc");
        assert!(composition.should_type(true, "abc"));
    }

    #[test]
    fn english_committed_from_a_composition_is_typed() {
        // Keys were swallowed while composing, so this is the only way in.
        assert!(commit_needs_typing(true, "abc"));
        assert!(commit_needs_typing(true, "ABC"));
        // No composition: the key events already typed it.
        assert!(!commit_needs_typing(false, "abc"));
        // Chinese always goes through this path.
        assert!(commit_needs_typing(false, "中文"));
        assert!(commit_needs_typing(true, "中文"));
    }

    #[test]
    fn the_imes_punctuation_replaces_the_plain_key() {
        let comma = Some(Sent::Punct(','));
        assert!(replaces_sent(comma, "，"));
        assert!(replaces_sent(Some(Sent::Punct('.')), "。"));
        // An IME picks the mark itself: a quote comes back as “ or ”, two dashes
        // as one long dash.
        assert!(replaces_sent(Some(Sent::Punct('"')), "“"));
        assert!(replaces_sent(Some(Sent::Punct('"')), "”"));
        assert!(replaces_sent(Some(Sent::Punct('-')), "——"));
        assert!(replaces_sent(Some(Sent::Punct('\\')), "、"));
        assert!(replaces_sent(Some(Sent::Punct('~')), "～"));
        // A Chinese character is not a punctuation replacement.
        assert!(!replaces_sent(comma, "中"));
        assert!(!replaces_sent(comma, "中，"));
        // Nothing was sent.
        assert!(!replaces_sent(None, "，"));
    }

    #[test]
    fn the_space_that_picks_a_candidate_is_taken_back() {
        assert!(replaces_sent(Some(Sent::Space), "你好"));
        assert!(replaces_sent(Some(Sent::Space), "nǐhǎo"));
        // The IME committed a space of its own: the key was not swallowed twice.
        assert!(!replaces_sent(Some(Sent::Space), " "));
        assert!(!replaces_sent(Some(Sent::Space), ""));
        // A letter key is never a replacement source.
        assert_eq!(sent_key(Some('n')), None);
        assert_eq!(sent_key(Some(' ')), Some(Sent::Space));
        assert_eq!(sent_key(Some(',')), Some(Sent::Punct(',')));
    }

    #[test]
    fn a_commit_that_came_first_drops_the_key_behind_it() {
        // Typing `,` on a Chinese IME: the `，` sometimes arrives first.
        let punct = pending_answer("，", true).expect("punctuation answers a key");
        assert!(punct.answers(','));
        assert!(punct.answers('['));
        assert!(!punct.answers('a'));
        assert!(!punct.answers(' '));
        // Chinese picked with the space: the space is still coming.
        let space = pending_answer("欧文", true).expect("a composition answers the space");
        assert!(space.answers(' '));
        assert!(!space.answers(','));
        // Text that no key answers for.
        assert_eq!(pending_answer(" ", true), None);
        assert_eq!(pending_answer("ouwen", false), None);
        // Too late to be the key that caused it.
        let stale = Pending {
            kind: Kind::Space,
            at: std::time::Instant::now() - std::time::Duration::from_millis(400),
        };
        assert!(!stale.answers(' '));
    }

    #[test]
    fn full_width_marks_are_recognised_by_range() {
        for mark in ['，', '。', '、', '“', '”', '—', '…', '～', '￥', '（', '！'] {
            assert!(is_cjk_punctuation(mark), "{mark:?}");
        }
        for other in ['中', 'a', ',', ' ', '1', '🙂'] {
            assert!(!is_cjk_punctuation(other), "{other:?}");
        }
    }

    #[test]
    fn only_the_compositions_own_letters_are_retracted() {
        // Nothing of the pinyin is ours yet.
        assert_eq!(retract_len("", "n"), 0);
        // The usual case: one letter leaked before the composition started.
        assert_eq!(retract_len("n", "n"), 1);
        assert_eq!(retract_len("ni", "nihao"), 2);
        // English typed in between is not the composition's, so it stays.
        assert_eq!(retract_len("abc", "n"), 0);
        assert_eq!(retract_len("abcni", "nihao"), 2);
        // A commit in between clears the run, so only the tail can match.
        assert_eq!(retract_len("abcnihao", "nihao"), 5);
        assert_eq!(retract_len("abc", "abcd"), 3);
    }

    #[test]
    fn links_only_come_from_word_shaped_tokens() {
        let text = "see https://example.com/a?b=1, then www.example.org.";
        let chars = line(text);
        let at = text.find("example.com").unwrap() + 2;
        assert_eq!(
            url_token(&chars, at).as_deref(),
            Some("https://example.com/a?b=1")
        );
        let www = text.find("www.example.org").unwrap();
        assert_eq!(
            url_token(&chars, www).as_deref(),
            Some("https://www.example.org")
        );
        // Not a link: plain prose.
        assert_eq!(url_token(&chars, 0), None);
        assert_eq!(url_token(&chars, text.find(' ').unwrap()), None);
    }

    use super::find_all;

    #[test]
    fn find_all_matches_every_occurrence() {
        assert_eq!(find_all("a-17 b-17 c", "17", false), vec![(2, 2), (7, 2)]);
    }

    #[test]
    fn find_all_is_case_insensitive_unless_asked_otherwise() {
        assert_eq!(find_all("item ITEM", "ITEM", false), vec![(0, 4), (5, 4)]);
        assert_eq!(find_all("item ITEM", "ITEM", true), vec![(5, 4)]);
    }

    #[test]
    fn find_all_reports_nothing_for_an_empty_query() {
        assert!(find_all("anything", "", false).is_empty());
    }
    #[test]
    fn search_includes_overlapping_matches() {
        assert_eq!(find_all("banana", "ana", true), vec![(1, 3), (3, 3)]);
        assert_eq!(find_all("aaaa", "aa", true), vec![(0, 2), (1, 2), (2, 2)]);
    }

    #[test]
    fn search_text_ignores_named_keys_and_accepts_space() {
        assert_eq!(super::query_key_text("space", None), Some(" "));
        assert_eq!(super::query_key_text("left", None), None);
        assert_eq!(super::query_key_text("f8", None), None);
        assert_eq!(super::query_key_text("period", Some(".")), Some("."));
    }

    #[test]
    fn search_ime_replaces_only_local_preedit_and_punctuation() {
        let mut find = super::Find {
            query: "Englishni".into(),
            key_echo: None,
            matches: vec![],
            current: 0,
            case_sensitive: false,
        };
        find.mark("ni", "nihao");
        find.commit(true, "你好", None);
        assert_eq!(find.query, "English你好");
        find.query.push(',');
        find.commit(false, "，", Some(super::Sent::Punct(',')));
        // The space that picked the candidate is not wanted in the query either.
        find.query.push(' ');
        find.commit(false, "好", Some(super::Sent::Space));
        assert_eq!(find.query, "English你好，好");
        find.commit(false, "abc", None);
        assert_eq!(find.query, "English你好，好abc");
        find.query.truncate("English你好，".len());
        assert_eq!(find.query, "English你好，");
        find.commit(false, "abc", None);
        assert_eq!(find.query, "English你好，abc");
        find.query.push('x');
        find.key_echo = Some("x".into());
        find.commit(false, "x", None);
        assert_eq!(find.query, "English你好，abcx");
        find.commit(false, "\r\t\u{8}", None);
        assert_eq!(find.query, "English你好，abcx");
    }
    #[test]
    fn closing_search_discards_its_late_commit_even_after_unmark() {
        let mut composition = super::Composition::default();
        composition.active = true;
        composition.close_search();
        composition.active = false; // platform unmark may precede the commit.
        composition.mark("late search preedit");
        assert_eq!(composition.marked_range(), None);
        assert_eq!(composition.commit(""), None);
        assert_eq!(composition.commit("中文"), None);
        assert_eq!(composition.commit("next"), Some(false));
    }

    #[test]
    fn forwarded_keys_keep_sequence_for_repeat_and_release_but_not_shortcuts() {
        let mut keys = super::ForwardedKeys::default();
        let stroke = gpui::Keystroke {
            key: "x".into(),
            key_char: Some("x".into()),
            modifiers: gpui::Modifiers::default(),
        };
        assert!(keys.repeat(&stroke.key).is_none()); // consumed shortcut has no release.
        keys.press(stroke.clone(), vec!["ctrl+b".into()]);
        assert_eq!(keys.repeat("x").unwrap().1, vec!["ctrl+b"]);
        let (pressed, prefix) = keys.release("x").unwrap();
        assert_eq!(pressed.key_char.as_deref(), Some("x"));
        assert_eq!(prefix, vec!["ctrl+b"]);
        assert!(keys.release("x").is_none());
    }
}
