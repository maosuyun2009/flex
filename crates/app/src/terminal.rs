//! One Rex block, drawn as a pane.

use std::sync::Arc;

use async_channel::Sender;
use gpui::prelude::*;
use gpui::{
    canvas, div, px, App, Context, FocusHandle, Focusable, IntoElement, KeyDownEvent, MouseButton,
    Render, Window,
};
use rexshell_engine::{fit_grid, Engine, Metrics, TerminalListener, TerminalSession};
use shrew_core::{Frame, TerminalStatus};

use crate::{keymap, paint};

const PAD: f32 = 4.0;

pub struct TerminalView {
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
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let opened = engine.open_terminal(server, session, block, Arc::new(Notifier(tx)));
        let (session, error) = match opened {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e.to_string())),
        };
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
            session,
            error,
            frame: empty_frame(),
            focus: cx.focus_handle(),
            palette: paint::Palette {
                family: "monospace".into(),
                font_size: px(14.0),
            },
        }
    }

    pub fn handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn press(&self, event: &KeyDownEvent) {
        let Some(session) = &self.session else { return };
        if let Some(p) = keymap::translate(&event.keystroke) {
            session.send_key(p.key, p.ctrl, p.alt, p.shift, p.meta);
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

        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, _cx| this.press(event)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _cx| {
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
                            metrics.cell_w as u32,
                            metrics.cell_h as u32,
                        );
                        metrics
                    },
                    move |bounds, metrics, window, cx| {
                        paint::paint(&frame, bounds, metrics, px(PAD), &palette, window, cx)
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
    Metrics {
        cell_w: advance as f64,
        cell_h: (ascent + descent + 1.0) as f64,
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
