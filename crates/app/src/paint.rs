//! Draws a `shrew_core::Frame` with GPUI: a quad per cell background, then one
//! shaped line per run.
//!
//! Deliberate MVP limits: no blinking cursor, the cursor is a quad painted
//! behind the text rather than an inverted cell, and wide glyphs (CJK, emoji)
//! are shaped as part of their run instead of placed cell by cell, so a long
//! mixed-width line can drift off the grid. `Run::widths` is there when that
//! matters.

use gpui::{
    fill, font, point, px, quad, size, App, BorderStyle, Bounds, Corners, Font, FontStyle,
    FontWeight, Hsla, Pixels, SharedString, StrikethroughStyle, TextRun, UnderlineStyle, Window,
};
use rexshell_engine::Metrics;
use shrew_core::{CursorStyle, Frame, Rgb, Run, Underline};

#[derive(Clone)]
pub struct Palette {
    pub family: SharedString,
    pub font_size: Pixels,
}

#[allow(clippy::too_many_arguments)]
pub fn paint(
    frame: &Frame,
    bounds: Bounds<Pixels>,
    metrics: Metrics,
    pad: Pixels,
    palette: &Palette,
    background: Option<&std::sync::Arc<gpui::RenderImage>>,
    background_opacity: f32,
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

    // A picture behind the text, if one is set. The terminal's own colour is
    // painted over it below, so the text keeps its contrast and the picture reads
    // as a watermark rather than as a background that fights the code.
    if let Some(image) = background {
        // Cover fit: fill the pane without distorting, cropping what overflows.
        let natural = image.size(0);
        let (iw, ih) = (natural.width.0 as f32, natural.height.0 as f32);
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if iw > 1.0 && ih > 1.0 {
            let scale = (bw / iw).max(bh / ih);
            let (w, h) = (px(iw * scale), px(ih * scale));
            let cover = Bounds::new(
                point(
                    bounds.origin.x + (px(bw) - w) * 0.5,
                    bounds.origin.y + (px(bh) - h) * 0.5,
                ),
                size(w, h),
            );
            let _ = window.paint_image(cover, Corners::default(), image.clone(), 0, false);
        }
    }

    // Translucent, not opaque: the window background (the shell's gradient) is
    // what the terminal sits on, so the whole window reads as one sheet. With a
    // picture behind it, more of it shows through.
    let backdrop: Hsla = color(frame.background);
    let alpha = if background.is_some() {
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

    if let Some(cursor) = &frame.cursor {
        let at = cell(cursor.col, cursor.row);
        let w = if cursor.wide { cell_w * 2.0 } else { cell_w };
        let shade = frame
            .cursor_color
            .map(color)
            .unwrap_or_else(|| color(frame.foreground));
        let box_ = Bounds::new(at, size(w, cell_h));
        window.paint_quad(match cursor.style {
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
        for run in &line.runs {
            let text = printable(&run.text);
            if text.trim().is_empty() {
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
            let at = cell(run.col, row as u16);
            let _ = shaped.paint(at, cell_h, window, cx);
        }
    }
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
