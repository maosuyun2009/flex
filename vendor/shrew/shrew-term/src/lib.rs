//! Terminal state for Shrew, backed by libghostty-vt (via the `libghostty-vt` crate).
//!
//! A Rex terminal stream starts with a Ghostty snapshot (`GHOSTSNP...`) and
//! continues with raw program output. [`Terminal::feed_stream`] frames the
//! snapshot out of the byte stream, restores it exactly, and then writes the
//! rest through Ghostty's VT parser. [`Terminal::frame`] turns the viewport into
//! style runs that any renderer (Core Text, Android Canvas, GTK/Pango) can draw.

use libghostty_vt as vt;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::selection::{FormatOptions, Selection, SelectLineOptions, SelectWordOptions};
use libghostty_vt::style::{Palette, RgbColor, StyleColor};
use libghostty_vt::terminal::{Point, PointCoordinate, ScrollViewport};

pub use shrew_proto::snapshot::{Framed, SnapshotFramer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
    pub fn parse_hex(s: &str) -> Option<Self> {
        let s = s.trim().trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let v = u32::from_str_radix(s, 16).ok()?;
        Some(Self::new((v >> 16) as u8, (v >> 8) as u8, v as u8))
    }
    pub fn to_u32(self) -> u32 {
        0xff00_0000 | (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }
    /// Relative luminance, for picking light/dark chrome around a terminal.
    pub fn luminance(self) -> f32 {
        let f = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * f(self.r) + 0.7152 * f(self.g) + 0.0722 * f(self.b)
    }
}

impl From<RgbColor> for Rgb {
    fn from(c: RgbColor) -> Self {
        Self { r: c.r, g: c.g, b: c.b }
    }
}

impl From<Rgb> for RgbColor {
    fn from(c: Rgb) -> Self {
        RgbColor { r: c.r, g: c.g, b: c.b }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
    HollowBlock,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub style: CursorStyle,
    pub blinking: bool,
    /// The program is reading a password (echo off); show a lock, not a caret.
    pub password: bool,
    pub wide: bool,
}

/// A run of cells on one row that share a style.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub col: u16,
    /// Number of cells the run covers.
    pub cells: u16,
    pub text: String,
    /// Cell width (1 or 2) of each grapheme in `text`, in order. Renderers
    /// that place glyphs individually use this to stay on the grid.
    pub widths: Vec<u8>,
    pub fg: Rgb,
    /// None means the terminal's default background (don't paint).
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub underline: Underline,
    pub underline_color: Option<Rgb>,
    pub strikethrough: bool,
    pub overline: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor_color: Option<Rgb>,
    pub cursor: Option<Cursor>,
    /// The cursor cell even when the program hid the cursor, for IME placement.
    pub cursor_position: Option<(u16, u16)>,
    pub lines: Vec<Row>,
    pub scrollbar: Scrollbar,
    pub alternate_screen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scrollbar {
    /// Rows in the whole buffer (scrollback + screen).
    pub total: u64,
    /// First visible row.
    pub offset: u64,
    /// Visible rows.
    pub len: u64,
}

impl Scrollbar {
    pub fn at_bottom(&self) -> bool {
        self.offset + self.len >= self.total
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Waiting for a snapshot to start the stream.
    AwaitingSnapshot,
    /// Writing program output.
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermError {
    Ghostty(String),
    Snapshot(String),
}

impl std::fmt::Display for TermError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ghostty(e) => write!(f, "libghostty-vt: {e}"),
            Self::Snapshot(s) => write!(f, "snapshot could not be restored: {s}"),
        }
    }
}
impl std::error::Error for TermError {}

impl From<vt::Error> for TermError {
    fn from(e: vt::Error) -> Self {
        TermError::Ghostty(e.to_string())
    }
}

/// One terminal emulator instance.
pub struct Terminal {
    raw: vt::Terminal<'static, 'static>,
    render: RenderState<'static>,
    rows_it: RowIterator<'static>,
    cells_it: CellIterator<'static>,
    framer: Option<SnapshotFramer>,
    scrollback_bytes: usize,
    cell_px: (u32, u32),
}

// libghostty-vt handles aren't thread-safe for concurrent use, but every
// method here takes `&mut self` or `&self` behind the engine's mutex, so moving
// the whole terminal between threads is sound.
unsafe impl Send for Terminal {}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        let raw = vt::Terminal::new(cols.max(1), rows.max(1)).expect("ghostty terminal");
        let mut t = Self {
            raw,
            render: RenderState::new().expect("render state"),
            rows_it: RowIterator::new().expect("row iterator"),
            cells_it: CellIterator::new().expect("cell iterator"),
            framer: None,
            scrollback_bytes: 10 << 20,
            cell_px: (0, 0),
        };
        t.apply_options();
        t
    }

    fn apply_options(&mut self) {
        let _ = self.raw.set_scrollback_max_bytes(Some(self.scrollback_bytes));
    }

    /// Cap scrollback memory (bytes). Phones want this lower than desktops.
    pub fn set_scrollback_limit(&mut self, bytes: usize) {
        self.scrollback_bytes = bytes;
        self.apply_options();
    }

    /// Expect a snapshot at the start of the next bytes fed (a new stream).
    pub fn begin_stream(&mut self) {
        self.framer = Some(SnapshotFramer::new());
    }

    pub fn stream_state(&self) -> StreamState {
        if self.framer.is_some() { StreamState::AwaitingSnapshot } else { StreamState::Live }
    }

    /// Feed bytes from a Rex terminal stream (after in-band messages are removed).
    pub fn feed_stream(&mut self, bytes: &[u8]) -> Result<(), TermError> {
        let Some(framer) = self.framer.as_mut() else {
            self.write_vt(bytes);
            return Ok(());
        };
        match framer.push(bytes) {
            Framed::Pending => Ok(()),
            Framed::NotSnapshot(raw) => {
                // Older/other servers may start with plain VT. Treat it as output.
                self.framer = None;
                self.write_vt(&raw);
                Ok(())
            }
            Framed::Complete { snapshot, rest } => {
                self.framer = None;
                self.restore(&snapshot)?;
                if !rest.is_empty() {
                    self.write_vt(&rest);
                }
                Ok(())
            }
            Framed::Invalid(msg) => {
                self.framer = None;
                Err(TermError::Snapshot(msg))
            }
        }
    }

    /// Replace the whole terminal state with a complete Ghostty snapshot.
    pub fn restore(&mut self, snapshot: &[u8]) -> Result<(), TermError> {
        let decoder = vt::snapshot::Decoder::new_buf(snapshot).map_err(|e| TermError::Snapshot(e.to_string()))?;
        self.raw = decoder.decode().map_err(|e| TermError::Snapshot(e.to_string()))?;
        self.apply_options();
        if self.cell_px != (0, 0) {
            let (c, r) = self.size();
            self.resize(c, r, self.cell_px.0, self.cell_px.1);
        }
        Ok(())
    }

    /// Encode the terminal as a Ghostty snapshot (used in tests and handoff).
    pub fn snapshot(&mut self) -> Result<Vec<u8>, TermError> {
        let mut out = Vec::new();
        self.raw.encode_snapshot(&mut out)?;
        Ok(out)
    }

    /// Write raw VT bytes.
    pub fn write_vt(&mut self, bytes: &[u8]) {
        self.raw.vt_write(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32) {
        self.cell_px = (cell_w_px, cell_h_px);
        let _ = self.raw.resize(cols.max(1), rows.max(1), cell_w_px, cell_h_px);
    }

    pub fn size(&self) -> (u16, u16) {
        (self.raw.cols().unwrap_or(80), self.raw.rows().unwrap_or(24))
    }

    pub fn title(&self) -> Option<String> {
        self.raw.title().ok().filter(|t| !t.is_empty()).map(|t| String::from_utf8_lossy(t).into_owned())
    }

    pub fn pwd(&self) -> Option<String> {
        self.raw.pwd().ok().filter(|t| !t.is_empty()).map(|t| String::from_utf8_lossy(t).into_owned())
    }

    pub fn mouse_tracking(&self) -> bool {
        self.raw.is_mouse_tracking().unwrap_or(false)
    }

    pub fn alternate_screen(&self) -> bool {
        matches!(self.raw.active_screen(), Ok(vt::screen::Screen::Alternate))
    }

    pub fn scrollbar(&self) -> Scrollbar {
        self.raw.scrollbar().map(|s| Scrollbar { total: s.total, offset: s.offset, len: s.len }).unwrap_or_default()
    }

    /// Scroll the viewport by `delta` rows (negative is up, into history).
    pub fn scroll(&mut self, delta: isize) {
        self.raw.scroll_viewport(ScrollViewport::Delta(delta));
    }

    /// Jump `delta` shell prompts (needs shell integration).
    pub fn scroll_prompts(&mut self, delta: isize) {
        self.raw.scroll_viewport(ScrollViewport::DeltaPrompt(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.raw.scroll_viewport(ScrollViewport::Bottom);
    }

    pub fn scroll_to_top(&mut self) {
        self.raw.scroll_viewport(ScrollViewport::Top);
    }

    fn viewport(col: u16, row: u16) -> Point {
        Point::Viewport(PointCoordinate { x: col, y: row as u32 })
    }

    /// Select from one viewport cell to another (inclusive).
    pub fn select(&mut self, from: (u16, u16), to: (u16, u16)) -> bool {
        let (Ok(a), Ok(b)) = (self.raw.grid_ref(Self::viewport(from.0, from.1)), self.raw.grid_ref(Self::viewport(to.0, to.1))) else {
            return false;
        };
        let sel = Selection::new(a, b, false);
        self.raw.set_selection(Some(&sel)).is_ok()
    }

    /// Select the word under a viewport cell (double-tap).
    pub fn select_word(&mut self, col: u16, row: u16) -> bool {
        let Ok(r) = self.raw.grid_ref(Self::viewport(col, row)) else { return false };
        match self.raw.select_word(SelectWordOptions::new(r)) {
            Ok(Some(sel)) => self.raw.set_selection(Some(&sel)).is_ok(),
            _ => false,
        }
    }

    /// Select a whole line (triple-tap), trimmed the way Ghostty does.
    pub fn select_line(&mut self, row: u16) -> bool {
        let Ok(r) = self.raw.grid_ref(Self::viewport(0, row)) else { return false };
        match self.raw.select_line(SelectLineOptions::new(r)) {
            Ok(Some(sel)) => self.raw.set_selection(Some(&sel)).is_ok(),
            _ => false,
        }
    }

    pub fn clear_selection(&mut self) {
        let _ = self.raw.set_selection(None);
    }

    pub fn has_selection(&self) -> bool {
        matches!(self.raw.selection(), Ok(Some(_)))
    }

    /// The selected text, unwrapped and trimmed.
    pub fn selection_text(&self) -> Option<String> {
        let opts = || FormatOptions::new().with_emit_format(vt::fmt::Format::Plain).with_unwrap(true).with_trim(true);
        let mut buf = vec![0u8; 4096];
        let n = match self.raw.format_selection_buf(opts(), &mut buf) {
            Ok(Some(n)) => n,
            Ok(None) => return None,
            Err(vt::Error::OutOfSpace { required }) => {
                buf.resize(required, 0);
                self.raw.format_selection_buf(opts(), &mut buf).ok().flatten()?
            }
            Err(_) => return None,
        };
        let s = String::from_utf8_lossy(&buf[..n]).into_owned();
        (!s.is_empty()).then_some(s)
    }

    /// Everything in the terminal (scrollback + screen) as plain text.
    pub fn all_text(&mut self) -> String {
        let ok = match self.raw.select_all() {
            Ok(Some(sel)) => self.raw.set_selection(Some(&sel)).is_ok(),
            _ => false,
        };
        if !ok {
            return String::new();
        }
        let text = self.selection_text().unwrap_or_default();
        self.clear_selection();
        text
    }

    /// Build a drawable frame of the current viewport.
    pub fn frame(&mut self) -> Frame {
        let scrollbar = self.scrollbar();
        let alternate_screen = self.alternate_screen();
        let Ok(snapshot) = self.render.update(&self.raw) else {
            return Frame {
                cols: 0,
                rows: 0,
                background: Rgb::default(),
                foreground: Rgb::new(255, 255, 255),
                cursor_color: None,
                cursor: None,
                cursor_position: None,
                lines: Vec::new(),
                scrollbar,
                alternate_screen,
            };
        };
        let cols = snapshot.cols().unwrap_or(0);
        let rows = snapshot.rows().unwrap_or(0);
        let colors = snapshot.colors().ok();
        let fg_default: Rgb = colors.as_ref().map(|c| c.foreground.into()).unwrap_or(Rgb::new(255, 255, 255));
        let bg_default: Rgb = colors.as_ref().map(|c| c.background.into()).unwrap_or_default();
        let palette = |i: u8| -> Rgb { colors.as_ref().map(|c| c.palette[i as usize].into()).unwrap_or_default() };
        let cur = snapshot.cursor().ok();

        let mut lines = Vec::with_capacity(rows as usize);
        if let Ok(mut row_it) = self.rows_it.update(&snapshot) {
            while let Some(row_iter) = row_it.next() {
                let mut row = Row::default();
                let mut col: u16 = 0;
                let Ok(mut cell_it) = self.cells_it.update(row_iter) else {
                    lines.push(row);
                    continue;
                };
                while let Some(cell) = cell_it.next() {
                    let this_col = col;
                    col += 1;
                    let wide = cell.raw_cell().and_then(|c| c.wide()).unwrap_or(CellWide::Narrow);
                    if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                        continue; // covered by the wide cell before it
                    }
                    let width: u8 = if matches!(wide, CellWide::Wide) { 2 } else { 1 };
                    let Ok(style) = cell.style() else { continue };
                    let selected = cell.is_selected().unwrap_or(false);
                    let mut fg: Rgb = cell.fg_color().ok().flatten().map(Into::into).unwrap_or(fg_default);
                    let mut bg: Option<Rgb> = cell.bg_color().ok().flatten().map(Into::into);
                    if style.inverse {
                        let new_fg = bg.unwrap_or(bg_default);
                        bg = Some(fg);
                        fg = new_fg;
                    }
                    if style.invisible {
                        fg = bg.unwrap_or(bg_default);
                    }
                    let mut text = String::new();
                    match cell.graphemes() {
                        Ok(g) if !g.is_empty() => text.extend(g),
                        _ => text.push(' '),
                    }
                    let underline = match style.underline {
                        vt::style::Underline::None => Underline::None,
                        vt::style::Underline::Single => Underline::Single,
                        vt::style::Underline::Double => Underline::Double,
                        vt::style::Underline::Curly => Underline::Curly,
                        vt::style::Underline::Dotted => Underline::Dotted,
                        vt::style::Underline::Dashed => Underline::Dashed,
                        _ => Underline::Single,
                    };
                    let underline_color = match style.underline_color {
                        StyleColor::Rgb(c) => Some(c.into()),
                        StyleColor::Palette(i) => Some(palette(i.0)),
                        _ => None,
                    };
                    let same = row.runs.last().is_some_and(|r: &Run| {
                        r.col + r.cells == this_col
                            && r.fg == fg
                            && r.bg == bg
                            && r.bold == style.bold
                            && r.italic == style.italic
                            && r.faint == style.faint
                            && r.underline == underline
                            && r.underline_color == underline_color
                            && r.strikethrough == style.strikethrough
                            && r.overline == style.overline
                            && r.selected == selected
                    });
                    if same {
                        let r = row.runs.last_mut().unwrap();
                        r.text.push_str(&text);
                        r.widths.push(width);
                        r.cells += width as u16;
                    } else {
                        row.runs.push(Run {
                            col: this_col,
                            cells: width as u16,
                            text,
                            widths: vec![width],
                            fg,
                            bg,
                            bold: style.bold,
                            italic: style.italic,
                            faint: style.faint,
                            underline,
                            underline_color,
                            strikethrough: style.strikethrough,
                            overline: style.overline,
                            selected,
                        });
                    }
                }
                lines.push(row);
            }
        }

        let cursor_position = cur.as_ref().and_then(|c| c.viewport).map(|vp| (vp.x, vp.y));
        let cursor = cur.and_then(|c| {
            let vp = c.viewport?;
            c.visible.then_some(Cursor {
                col: vp.x,
                row: vp.y,
                style: match c.visual_style {
                    CursorVisualStyle::Bar => CursorStyle::Bar,
                    CursorVisualStyle::Underline => CursorStyle::Underline,
                    CursorVisualStyle::BlockHollow => CursorStyle::HollowBlock,
                    _ => CursorStyle::Block,
                },
                blinking: c.blinking,
                password: c.password_input,
                wide: vp.at_wide_tail,
            })
        });

        Frame {
            cols,
            rows,
            background: bg_default,
            foreground: fg_default,
            cursor_color: colors.as_ref().and_then(|c| c.cursor).map(Into::into),
            cursor,
            cursor_position,
            lines,
            scrollbar,
            alternate_screen,
        }
    }

    /// The visible screen as plain text lines (for tests and previews).
    pub fn screen_lines(&mut self) -> Vec<String> {
        self.frame()
            .lines
            .iter()
            .map(|r| r.runs.iter().map(|run| run.text.as_str()).collect::<String>().trim_end().to_owned())
            .collect()
    }

    /// Replace the 16 ANSI colors (themes), keeping the 256-color cube; `None` restores the built-in palette.
    pub fn set_ansi_palette(&mut self, ansi: Option<&[Rgb; 16]>) {
        let _ = self.raw.set_default_color_palette(None);
        let Some(ansi) = ansi else { return };
        let Ok(Palette(mut pal)) = self.raw.default_color_palette() else { return };
        for (i, c) in ansi.iter().enumerate() {
            pal[i] = (*c).into();
        }
        let _ = self.raw.set_default_color_palette(Some(Palette(pal)));
    }

    /// Set default colors (from the stream's `theme` message or a chosen theme).
    pub fn set_default_colors(&mut self, fg: Option<Rgb>, bg: Option<Rgb>, cursor: Option<Rgb>) {
        let _ = self.raw.set_default_fg_color(fg.map(Into::into));
        let _ = self.raw.set_default_bg_color(bg.map(Into::into));
        let _ = self.raw.set_default_cursor_color(cursor.map(Into::into));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_frames() {
        let mut t = Terminal::new(20, 4);
        t.write_vt(b"hi \x1b[1;31mred\x1b[0m\r\n\xe4\xbd\xa0ok");
        let f = t.frame();
        assert_eq!((f.cols, f.rows), (20, 4));
        let r0 = &f.lines[0].runs;
        assert_eq!(r0[0].text, "hi ");
        let red = r0.iter().find(|r| r.text == "red").unwrap();
        assert!(red.bold);
        assert_ne!(red.fg, f.foreground);
        let r1 = &f.lines[1].runs[0];
        assert_eq!(r1.widths[0], 2, "CJK is wide");
        assert!(r1.text.starts_with("你ok"));
        let c = f.cursor.unwrap();
        assert_eq!((c.col, c.row), (4, 1));
    }

    #[test]
    fn snapshot_roundtrip_through_stream() {
        let mut src = Terminal::new(30, 5);
        src.write_vt(b"$ echo restored\r\nrestored\r\n$ ");
        let snap = src.snapshot().unwrap();
        let mut stream = snap.clone();
        stream.extend_from_slice(b"live!");
        let mut dst = Terminal::new(80, 24);
        dst.begin_stream();
        // Feed in awkward chunks to exercise the framer.
        for chunk in stream.chunks(7) {
            dst.feed_stream(chunk).unwrap();
        }
        assert_eq!(dst.stream_state(), StreamState::Live);
        let lines = dst.screen_lines();
        assert_eq!(lines[0], "$ echo restored");
        assert_eq!(lines[1], "restored");
        assert_eq!(lines[2], "$ live!");
        assert_eq!(dst.size(), (30, 5));
    }

    #[test]
    fn ansi_palette_override() {
        let mut t = Terminal::new(10, 2);
        let mut ansi = [Rgb::new(1, 2, 3); 16];
        ansi[1] = Rgb::new(0xff, 0x00, 0x7f);
        t.set_ansi_palette(Some(&ansi));
        t.write_vt(b"\x1b[31mX");
        assert_eq!(t.frame().lines[0].runs[0].fg, Rgb::new(0xff, 0x00, 0x7f));
        t.set_ansi_palette(None);
        assert_ne!(t.frame().lines[0].runs[0].fg, Rgb::new(0xff, 0x00, 0x7f));
    }

    #[test]
    fn selection_and_scrollback() {
        let mut t = Terminal::new(10, 3);
        for i in 0..20 {
            t.write_vt(format!("line{i}\r\n").as_bytes());
        }
        let sb = t.scrollbar();
        assert!(sb.total > sb.len && sb.at_bottom());
        t.scroll(-5);
        assert!(!t.scrollbar().at_bottom());
        t.scroll_to_bottom();
        assert!(t.select_word(1, 0));
        assert_eq!(t.selection_text().as_deref(), Some("line18"));
        assert!(t.select_line(1));
        assert_eq!(t.selection_text().as_deref(), Some("line19"));
        assert!(t.all_text().contains("line0\nline1\n"));
    }
}
