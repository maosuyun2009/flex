//! A one-line text field's state: the text, a caret, a selection and an input
//! method's composition.
//!
//! Keys edit it (arrows, Home/End, Backspace/Delete, Ctrl-A); text arrives
//! from the platform's input handler, which is how an input method, a pasted
//! character and an ordinary keystroke all reach it the same way.

use std::ops::Range;

use gpui::{HighlightStyle, Hsla, StyledText, UnderlineStyle};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextField {
    text: String,
    /// Where typing goes, as a byte offset on a character boundary.
    caret: usize,
    /// The other end of the selection; equal to `caret` when nothing is selected.
    anchor: usize,
    /// The input method's composition: text in the field that is not final yet.
    marked: Option<Range<usize>>,
}

impl TextField {
    /// A field holding `text`, all of it selected: typing replaces it, an
    /// arrow key keeps it. For renaming something that already has a name.
    pub fn selected(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            caret: text.len(),
            anchor: 0,
            marked: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn selection(&self) -> Range<usize> {
        self.caret.min(self.anchor)..self.caret.max(self.anchor)
    }

    /// Put `text` where the composition or the selection is, caret after it.
    fn replace(&mut self, text: &str) -> Range<usize> {
        let range = self.marked.take().unwrap_or_else(|| self.selection());
        self.text.replace_range(range.clone(), text);
        self.caret = range.start + text.len();
        self.anchor = self.caret;
        range.start..self.caret
    }

    /// Typed, pasted or committed by an input method. An empty commit removes
    /// a composition that was cancelled.
    pub fn insert(&mut self, text: &str) {
        self.replace(text);
    }

    /// An input method's composition so far; replaces the previous one.
    pub fn mark(&mut self, text: &str) {
        let range = self.replace(text);
        self.marked = (!range.is_empty()).then_some(range);
    }

    /// The composition is over and its text stays as it is.
    pub fn unmark(&mut self) {
        self.marked = None;
    }

    pub fn backspace(&mut self) {
        if self.selection().is_empty() {
            self.anchor = self.before(self.caret);
        }
        self.replace("");
    }

    pub fn delete(&mut self) {
        if self.selection().is_empty() {
            self.anchor = self.after(self.caret);
        }
        self.replace("");
    }

    /// Move the caret; `extend` keeps the anchor, which selects.
    fn move_to(&mut self, at: usize, extend: bool) {
        self.caret = at;
        if !extend {
            self.anchor = at;
        }
    }

    pub fn left(&mut self, extend: bool) {
        // Without Shift, a selection collapses to its near edge first.
        let at = match self.selection() {
            selected if !extend && !selected.is_empty() => selected.start,
            _ => self.before(self.caret),
        };
        self.move_to(at, extend);
    }

    pub fn right(&mut self, extend: bool) {
        let at = match self.selection() {
            selected if !extend && !selected.is_empty() => selected.end,
            _ => self.after(self.caret),
        };
        self.move_to(at, extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn end(&mut self, extend: bool) {
        self.move_to(self.text.len(), extend);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    fn before(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn after(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    fn utf16(&self, at: usize) -> usize {
        self.text[..at].encode_utf16().count()
    }

    /// The caret, in the UTF-16 units input methods count in.
    pub fn caret_utf16(&self) -> usize {
        self.utf16(self.caret)
    }

    /// The composition, in UTF-16 units. While this is `Some`, keys belong to
    /// the input method.
    pub fn marked_utf16(&self) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|marked| self.utf16(marked.start)..self.utf16(marked.end))
    }

    /// Where the caret is drawn: the line it is on, and the text before it
    /// on that line, to be measured.
    pub fn caret_line(&self) -> (usize, &str) {
        let before = &self.text[..self.caret];
        let line = before.rfind('\n').map_or(0, |at| at + 1);
        (before.matches('\n').count(), &before[line..])
    }

    /// The field as text to draw: the selection tinted, the composition
    /// underlined, and a block caret on the character typing would go before
    /// (a trailing space stands in at the end, so the caret never shifts text).
    pub fn styled(&self, caret: Hsla, on_caret: Hsla) -> StyledText {
        let shown = format!("{} ", self.text);
        let selected = self.selection();
        let caret_cell = self.caret..self.after(self.caret).max(self.caret + 1).min(shown.len());
        let caret_cell = if selected.is_empty() {
            caret_cell
        } else {
            0..0
        };
        let marked = self.marked.clone().unwrap_or(0..0);
        let mut edges = vec![
            0,
            selected.start,
            selected.end,
            caret_cell.start,
            caret_cell.end,
            marked.start,
            marked.end,
            shown.len(),
        ];
        edges.sort_unstable();
        edges.dedup();
        let within = |range: &Range<usize>, at: usize| range.start <= at && at < range.end;
        let highlights: Vec<_> = edges
            .windows(2)
            .filter_map(|edge| {
                let (from, to) = (edge[0], edge[1]);
                let mut style = HighlightStyle::default();
                if within(&selected, from) {
                    style.background_color = Some(Hsla { a: 0.35, ..caret });
                }
                if within(&caret_cell, from) {
                    style.background_color = Some(caret);
                    style.color = Some(on_caret);
                }
                if within(&marked, from) {
                    style.underline = Some(UnderlineStyle {
                        thickness: gpui::px(1.0),
                        color: None,
                        wavy: false,
                    });
                }
                (style != HighlightStyle::default()).then_some((from..to, style))
            })
            .collect();
        StyledText::new(shown).with_highlights(highlights)
    }
}

/// What a key does to a field. `None`: the key is not an editing key, and if
/// it types something the text will come through the input handler.
pub fn edit(field: &mut TextField, key: &str, shift: bool) -> Option<()> {
    match key {
        "backspace" => field.backspace(),
        "delete" => field.delete(),
        "left" => field.left(shift),
        "right" => field.right(shift),
        "home" => field.home(shift),
        "end" => field.end(shift),
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> TextField {
        let mut field = TextField::default();
        field.insert(text);
        field
    }

    #[test]
    fn typing_goes_where_the_caret_is() {
        let mut field = typed("ac");
        field.left(false);
        field.insert("b");
        assert_eq!(field.text(), "abc");
        field.home(false);
        field.insert(">");
        field.end(false);
        field.insert("!");
        assert_eq!(field.text(), ">abc!");
    }

    #[test]
    fn editing_keys_step_over_whole_characters() {
        let mut field = typed("a中😀");
        field.backspace();
        assert_eq!(field.text(), "a中");
        field.left(false);
        field.delete();
        assert_eq!(field.text(), "a");
        // At either end, the keys do nothing rather than panic.
        field.delete();
        field.home(false);
        field.backspace();
        field.left(false);
        assert_eq!(field.text(), "a");
    }

    #[test]
    fn a_prefilled_name_is_replaced_by_typing_or_kept_by_an_arrow() {
        let mut replaced = TextField::selected("old name");
        replaced.insert("n");
        assert_eq!(replaced.text(), "n");

        let mut kept = TextField::selected("old name");
        kept.right(false);
        kept.insert("!");
        assert_eq!(kept.text(), "old name!");

        let mut cleared = TextField::selected("old name");
        cleared.backspace();
        assert_eq!(cleared.text(), "");
    }

    #[test]
    fn shift_arrows_select_and_typing_replaces_the_selection() {
        let mut field = typed("hello world");
        for _ in 0..5 {
            field.left(true);
        }
        field.insert("there");
        assert_eq!(field.text(), "hello there");
        field.select_all();
        field.left(false);
        field.insert(">");
        assert_eq!(field.text(), ">hello there");
    }

    #[test]
    fn a_composition_is_replaced_as_it_changes_and_by_its_commit() {
        let mut field = typed("名字：");
        field.mark("zhong");
        assert_eq!(field.text(), "名字：zhong");
        assert_eq!(field.marked_utf16(), Some(3..8));
        field.mark("zhongwen");
        assert_eq!(field.text(), "名字：zhongwen");
        // The commit replaces the pinyin; nothing is left marked.
        field.insert("中文");
        assert_eq!(field.text(), "名字：中文");
        assert_eq!(field.marked_utf16(), None);
        assert_eq!(field.caret_utf16(), 5);
    }

    #[test]
    fn a_cancelled_composition_leaves_no_text_behind() {
        let mut field = typed("ab");
        field.left(false);
        field.mark("ni");
        // Windows reports a cancelled composition as an empty one.
        field.mark("");
        assert_eq!((field.text(), field.marked_utf16()), ("ab", None));
        field.mark("ni");
        field.insert("");
        assert_eq!(field.text(), "ab");
        field.insert("X");
        assert_eq!(field.text(), "aXb");
    }

    #[test]
    fn offsets_for_the_input_method_count_utf16_units() {
        let field = typed("a😀中");
        assert_eq!(field.caret_utf16(), 4);
    }

    #[test]
    fn the_caret_is_measured_on_its_own_line() {
        let mut field = typed("{\n  \"a\": 1");
        assert_eq!(field.caret_line(), (1, "  \"a\": 1"));
        field.home(false);
        assert_eq!(field.caret_line(), (0, ""));
    }

    #[test]
    fn only_editing_keys_are_taken_from_the_input_method() {
        let mut field = typed("ab");
        assert_eq!(edit(&mut field, "backspace", false), Some(()));
        assert_eq!(field.text(), "a");
        for typing in ["a", "space", "enter", "escape", "up"] {
            assert_eq!(edit(&mut field, typing, false), None, "{typing}");
        }
        assert_eq!(field.text(), "a");
    }
}
