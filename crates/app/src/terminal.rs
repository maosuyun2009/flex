//! One Rex block, drawn as a pane.

use std::sync::Arc;

use async_channel::Sender;
use flex_engine::{fit_grid, Engine, Metrics, TerminalListener, TerminalSession};
use gpui::prelude::*;
use gpui::{
    canvas, div, px, App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, MouseButton, Point, Render, SharedString, UTF16Selection,
    Window,
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

pub struct TerminalView {
    /// An input method is composing. Its pinyin letters arrive as ordinary key
    /// events and must not reach the terminal; only the committed text should,
    /// and that comes through `replace_text_in_range` instead.
    composing: bool,
    /// The shell's prefix key is armed, so the next key belongs to the chrome.
    prefix: Arc<std::sync::atomic::AtomicBool>,
    /// Where a selection drag started, in cells. `None` outside a drag.
    selecting: Option<(u16, u16)>,
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
    leaked: String,
    session: Option<Arc<TerminalSession>>,
    error: Option<String>,
    frame: Frame,
    focus: FocusHandle,
    palette: paint::Palette,
}

struct Notifier(Sender<()>);

impl TerminalListener for Notifier {
    fn frame_ready(&self) {
        let _ = self.0.try_send(());
    }
    fn status_changed(&self, _: TerminalStatus) {}
    fn clipboard(&self, _: String) {}
    fn size_changed(&self, _cols: u16, _rows: u16) {}
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
    // Folding can change the char count (ß becomes ss), which would put the
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
            index += needle.len();
        } else {
            index += 1;
        }
    }
    found
}

/// The plain text of a row, and the cell column each character starts at.
fn row_cells(row: &Row) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut columns = Vec::new();
    for run in &row.runs {
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
struct Find {
    query: String,
    matches: Vec<Match>,
    current: usize,
    case_sensitive: bool,
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
                while rx.recv().await.is_ok() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        Self {
            composing: false,
            prefix,
            selecting: None,
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
    pub fn paste(&mut self, _cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let Some(text) = crate::win32::clipboard_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        session.send_text(&text);
    }

    /// Open the find bar, or move to the next match if it is already open.
    pub fn start_find(&mut self, cx: &mut Context<Self>) {
        match &mut self.find {
            // The bar is open, so the same chord closes it.
            Some(_) => self.close_find(cx),
            None => {
                self.find = Some(Find {
                    query: String::new(),
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
                            to,
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
        let earliest = bar
            .total
            .saturating_sub((self.scrollback_lines as u64).min(bar.total));
        let latest = bar.total.saturating_sub(bar.len);
        let target = wanted.clamp(earliest, latest);
        let delta = target as i64 - bar.offset as i64;
        if delta != 0 {
            session.scroll(delta as i32);
        }
        self.frame = session.frame();
        // Highlight just the matched cells, once the viewport has moved.
        let row = found.row.saturating_sub(target) as u16;
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
        if key == "escape" {
            self.close_find(cx);
            return true;
        }
        if key == "enter" {
            self.find_step(true, cx);
            return true;
        }
        if key == "up" || key == "f3" {
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
            self.refind(cx);
            return true;
        }
        if !modifiers.alt {
            if let Some(text) = event.keystroke.key_char.as_deref().or(Some(key)) {
                let printable: String = text.chars().filter(|c| !c.is_control()).collect();
                if !printable.is_empty() && printable != "space" {
                    find.query.push_str(&printable);
                    self.refind(cx);
                    return true;
                }
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

    fn begin_selection(&mut self, at: (u16, u16), cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.clear_selection();
            session.select_range(at, at);
        }
        self.selecting = Some(at);
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
        let Some(from) = self.selecting else { return };
        let (origin, metrics) = *self.geometry.lock().unwrap();
        let to = TerminalView::cell_at(origin, metrics, at);
        if to != from {
            self.extend_selection(from, to, cx);
        }
    }

    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = None;
        let Some(session) = &self.session else { return };
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
    fn wheel(&mut self, notches: f32, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let bar = self.frame.scrollbar;
        // One notch is three lines, the usual terminal step.
        let delta = -(notches * 3.0) as i64;
        let limit = (self.scrollback_lines as u64).min(bar.total);
        let earliest = bar.total.saturating_sub(limit) as i64;
        let latest = bar.total.saturating_sub(bar.len) as i64;
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

    fn press(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.find_key(event, cx) {
            return;
        }
        // An armed prefix means the next key is the shell's, not the pty's.
        if self.prefix.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // While an IME is composing, the keys belong to it.
        if self.composing {
            return;
        }
        // Shell shortcuts are not the terminal's: leave them to bubble up.
        if shortcuts::lookup(&event.keystroke).is_some() {
            return;
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
                session.send_key(p.key, p.ctrl, p.alt, p.shift, p.meta);
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
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(session) = self.session.clone() else {
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "terminal closed".into());
            return div().size_full().p_2().child(message).into_any_element();
        };

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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    if this.find.is_some() {
                        this.close_find(cx);
                    }
                    // Claim the size so the server reflows this terminal to this window.
                    if let Some(session) = &this.session {
                        session.clone().take_control();
                    }
                    let (origin, metrics) = *this.geometry.lock().unwrap();
                    let cell = TerminalView::cell_at(origin, metrics, event.position);
                    this.begin_selection(cell, cx);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_selection(cx)),
            )
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                // Vertical wheel scrolls the scrollback; horizontal is ignored.
                let notches: f32 = match event.delta {
                    gpui::ScrollDelta::Lines(lines) => lines.y,
                    gpui::ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 44.0,
                };
                this.wheel(notches, cx);
            }))
            .when(self.selecting.is_some(), |el| {
                let view = cx.entity().downgrade();
                el.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _cx| {
                            let this = view.clone();
                            window.on_mouse_event(
                                move |event: &gpui::MouseMoveEvent, _phase, _window, cx| {
                                    let _ = this
                                        .update(cx, |me, cx| me.drag_selection(event.position, cx));
                                },
                            );
                        },
                    )
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .size_full(),
                )
            })
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
        None
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.composing = false;
    }

    /// The committed string, after the candidate window closes.
    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.composing = false;
        if std::env::var_os("FLEX_DEBUG").is_some() {
            eprintln!("[ime] commit {text:?}");
        }
        // ASCII already arrives as key events; sending it here would type every
        // character twice. Only the IME's own output takes this path.
        if !text.chars().any(|c| !c.is_ascii()) {
            return;
        }
        self.leaked.clear();
        if let Some(session) = &self.session {
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
        _cx: &mut Context<Self>,
    ) {
        if _new_text.is_empty() {
            return;
        }
        if !self.composing {
            // The composition has just started, and the pinyin letters sent
            // before this message are the last thing on the line: take them back.
            if !self.leaked.is_empty() {
                if let Some(session) = &self.session {
                    session.send_text(&"\u{7f}".repeat(self.leaked.len()));
                }
                self.leaked.clear();
            }
        }
        // Composition in progress: swallow key events until it commits.
        self.composing = true;
    }

    fn bounds_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _element_bounds: Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        None
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
}
