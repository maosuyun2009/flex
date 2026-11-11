//! Draws cell backgrounds, the cursor, then text on the terminal grid.
//! Multi-codepoint grapheme runs retain shaping when scalar widths are ambiguous.

use flex_engine::Metrics;
use gpui::{
    fill, font, point, px, quad, size, App, BorderStyle, Bounds, Font, FontStyle, FontWeight, Hsla,
    Pixels, SharedString, StrikethroughStyle, TextRun, UnderlineStyle, Window,
};
use shrew_core::{CursorStyle, Frame, Rgb, Run, Underline};

#[derive(Clone)]
pub struct Palette {
    pub family: SharedString,
    pub font_size: Pixels,
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    frame: &Frame,
    // Whether this pane has the keyboard. Without it the cursor is drawn
    // hollow, so only one solid cursor is on screen: where typing goes.
    focused: bool,
    bounds: Bounds<Pixels>,
    metrics: Metrics,
    pad: Pixels,
    palette: &Palette,
    over_picture: bool,
    background_opacity: f32,
    preedit: Option<(&str, u16, u16)>,
    window: &mut Window,
    cx: &mut App,
) {
    let cell_w = px(metrics.cell_w as f32);
    let cell_h = px(metrics.cell_h as f32);
    let origin = point(bounds.origin.x + pad, bounds.origin.y + pad);
    let cell = |col: u16, row: u16| {
        point(
            origin.x + cell_w * col as f32,
            origin.y + cell_h * row as f32,
        )
    };

    // Translucent, not opaque: the window background (the shell's gradient) is
    // what the terminal sits on, so the whole window reads as one sheet. With a
    // picture behind it, more of it shows through.
    let backdrop: Hsla = color(frame.background);
    let alpha = if over_picture {
        (1.0 - background_opacity).clamp(0.15, 0.92)
    } else {
        0.72
    };
    window.paint_quad(fill(
        bounds,
        Hsla {
            a: alpha,
            ..backdrop
        },
    ));

    let selection = mix(frame.foreground, frame.background, 0.30);
    for (row, line) in frame.lines.iter().enumerate() {
        for run in &line.runs {
            let background = if run.selected {
                Some(color(selection))
            } else {
                run.bg.map(color)
            };
            if let Some(background) = background {
                let at = cell(run.col, row as u16);
                let w = cell_w * run.cells as f32;
                window.paint_quad(fill(Bounds::new(at, size(w, cell_h)), background));
            }
        }
    }

    if let Some(cursor) = &frame.cursor {
        let at = cell(cursor.col, cursor.row);
        let w = if cursor.wide { cell_w * 2.0 } else { cell_w };
        let shade = frame
            .cursor_color
            .map(color)
            .unwrap_or_else(|| color(frame.foreground));
        let box_ = Bounds::new(at, size(w, cell_h));
        let style = if focused {
            cursor.style
        } else {
            CursorStyle::HollowBlock
        };
        window.paint_quad(match style {
            CursorStyle::Bar => fill(Bounds::new(at, size(px(2.0), cell_h)), shade),
            CursorStyle::Underline => fill(
                Bounds::new(point(at.x, at.y + cell_h - px(2.0)), size(w, px(2.0))),
                shade,
            ),
            CursorStyle::HollowBlock => {
                quad(box_, 0., transparent(), px(1.0), shade, BorderStyle::Solid)
            }
            _ => fill(box_, shade),
        });
    }

    for (row, line) in frame.lines.iter().enumerate() {
        for run in &line.runs {
            for (offset, text) in positioned_text(&run.text, &run.widths) {
                if text.trim().is_empty() && run.underline == Underline::None && !run.strikethrough
                {
                    continue;
                }
                let fg = if run.faint {
                    mix(run.fg, frame.background, 0.5)
                } else {
                    run.fg
                };
                let shape = TextRun {
                    len: text.len(),
                    font: font_for(palette, run),
                    color: color(fg),
                    background_color: None,
                    underline: underline_for(run, fg),
                    strikethrough: run.strikethrough.then(|| StrikethroughStyle {
                        thickness: px(1.0),
                        color: Some(color(fg)),
                    }),
                };
                let shaped = window.text_system().shape_line(
                    SharedString::from(text),
                    palette.font_size,
                    &[shape],
                    None,
                );
                let at = cell(run.col.saturating_add(offset), row as u16);
                let _ = shaped.paint(at, cell_h, window, cx);
            }
        }
    }

    // The pty has no buffer for an IME preedit, so draw it over the cursor cell.
    if let Some((text, col, row)) = preedit {
        if !text.is_empty() {
            let at = cell(col.min(frame.cols.saturating_sub(1)), row.min(frame.rows.saturating_sub(1)));
            let columns = frame.cols.saturating_sub(col) as f32;
            let w = cell_w * columns.max(1.0);
            window.paint_quad(fill(
                Bounds::new(at, size(w, cell_h)),
                Hsla { a: 0.92, ..backdrop },
            ));
            let fg = color(frame.foreground);
            let shape = TextRun {
                len: text.len(),
                font: font(palette.family.clone()),
                color: fg,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(printable(text)),
                palette.font_size,
                &[shape],
                None,
            );
            let _ = shaped.paint(at, cell_h, window, cx);
        }
    }
}

/// Use the core's widths when one scalar is one cell grapheme. Don't guess
/// boundaries in combining/ZWJ sequences: keep those shaped as a whole run.
// shortcut: multi-codepoint grapheme runs can still drift; place them per cell
// when the public frame exposes grapheme boundaries.
fn positioned_text(text: &str, widths: &[u8]) -> Vec<(u16, String)> {
    if text.chars().count() != widths.len() {
        return vec![(0, printable(text))];
    }
    let mut col: u16 = 0;
    text.chars()
        .zip(widths)
        .map(|(ch, width)| {
            let at = col;
            col = col.saturating_add(u16::from(*width));
            (at, printable(&ch.to_string()))
        })
        .collect()
}

fn font_for(palette: &Palette, run: &Run) -> Font {
    let mut f = font(palette.family.clone());
    if run.bold {
        f.weight = FontWeight::BOLD;
    }
    if run.italic {
        f.style = FontStyle::Italic;
    }
    f
}

fn underline_for(run: &Run, fg: Rgb) -> Option<UnderlineStyle> {
    let color = Some(color(run.underline_color.unwrap_or(fg)));
    let (thickness, wavy) = match run.underline {
        Underline::None => return None,
        Underline::Single | Underline::Dotted | Underline::Dashed => (1.0, false),
        Underline::Double => (2.0, false),
        Underline::Curly => (1.0, true),
    };
    Some(UnderlineStyle {
        thickness: px(thickness),
        color,
        wavy,
    })
}

/// `shape_line` panics on a newline, and control bytes must not reach the shaper.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\t' || c.is_control() { ' ' } else { c })
        .collect()
}

fn transparent() -> Hsla {
    gpui::transparent_black()
}

pub fn color(c: Rgb) -> Hsla {
    gpui::rgb((c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32).into()
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| {
        ((x as f32 * (1.0 - t)) + (y as f32 * t))
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Rgb::new(f(a.r, b.r), f(a.g, b.g), f(a.b, b.b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_width_cells_keep_the_following_glyph_on_grid() {
        assert_eq!(
            positioned_text("a中b", &[1, 2, 1]),
            vec![(0, "a".into()), (1, "中".into()), (3, "b".into())]
        );
    }

    #[test]
    fn combining_sequences_are_not_split_into_separate_cells() {
        assert_eq!(
            positioned_text("e\u{301}中", &[1, 2]),
            vec![(0, "e\u{301}中".into())]
        );
        assert_eq!(printable("a\n\tb"), "a  b");
    }
}
