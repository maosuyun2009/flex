//! One Rex block, drawn as a pane.

use std::sync::Arc;

use async_channel::Sender;
use gpui::prelude::*;
use gpui::{
    canvas, div, px, App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, IntoElement, KeyDownEvent, MouseButton, Point, Render, SharedString, UTF16Selection,
    Window,
};
use rexshell_engine::{fit_grid, Engine, Metrics, TerminalListener, TerminalSession};
use shrew_core::{ColorTheme, Frame, Rgb, TerminalStatus};

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
    block_id: String,
    /// An input method is composing. Its pinyin letters arrive as ordinary key
    /// events and must not reach the terminal; only the committed text should,
    /// and that comes through `replace_text_in_range` instead.
    composing: bool,
    /// The shell's prefix key is armed, so the next key belongs to the chrome.
    prefix: Arc<std::sync::atomic::AtomicBool>,
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
            block_id: block.to_owned(),
            composing: false,
            prefix,
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

    pub fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.palette.font_size = px(size);
        cx.notify();
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

    fn press(&mut self, event: &KeyDownEvent) {
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
                if std::env::var_os("REXSHELL_DEBUG").is_some() {
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
        // REXSHELL_DEBUG=1 explains an empty pane: no frames means the stream is
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
                this.press(event)
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _cx| {
                    if std::env::var_os("REXSHELL_DEBUG").is_some() {
                        eprintln!("[hit] terminal {}", this.block_id);
                    }
                    window.focus(&this.focus);
                    // Claim the size so the server reflows this terminal to this window.
                    if let Some(session) = &this.session {
                        session.clone().take_control();
                    }
                }),
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
    std::env::var_os("REXSHELL_DEBUG").is_some()
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
        if std::env::var_os("REXSHELL_DEBUG").is_some() {
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
