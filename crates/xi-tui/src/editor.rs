//! The composer: a small multi-line editor that stays live through a turn.

use unicode_width::UnicodeWidthChar;

use crate::style::{Line, Role};

/// The composer's visible text rows and border form.
#[derive(Clone, Copy, Debug)]
pub struct Form {
    /// Text rows shown before the composer scrolls.
    pub rows: usize,
    /// A rule and prompt without side borders.
    pub bare: bool,
}

/// Text being composed, with a cursor and history.
#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset of the cursor, always on a char boundary.
    cursor: usize,
    history: Vec<String>,
    /// Position while browsing history, and the draft it replaced.
    browsing: Option<(usize, String)>,
}

impl Editor {
    /// The composed text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's byte offset in the composed text.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the draft and leaves history browsing, placing the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.cursor = self.text.len();
        self.browsing = None;
    }

    /// Replaces a byte range, placing the cursor after the inserted text.
    ///
    /// Panics if the range is out of bounds or not on UTF-8 character boundaries.
    pub fn replace(&mut self, range: std::ops::Range<usize>, with: &str) {
        let start = range.start;
        self.text.replace_range(range, with);
        self.cursor = start + with.len();
        self.browsing = None;
    }

    /// Replaces the prompt history, oldest first, and leaves history browsing.
    pub fn set_history(&mut self, entries: Vec<String>) {
        self.history = entries;
        self.browsing = None;
    }

    /// Whether nothing has been typed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Inserts text at the cursor.
    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
        self.browsing = None;
    }

    fn prev(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// Deletes the character before the cursor.
    pub fn backspace(&mut self) {
        let at = self.prev();
        self.text.replace_range(at..self.cursor, "");
        self.cursor = at;
    }

    /// Deletes the character under the cursor.
    pub fn delete(&mut self) {
        let end = self.next();
        self.text.replace_range(self.cursor..end, "");
    }

    /// Moves one character left.
    pub fn left(&mut self) {
        self.cursor = self.prev();
    }

    /// Moves one character right.
    pub fn right(&mut self) {
        self.cursor = self.next();
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    /// Moves to the start of the line.
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// Moves to the end of the line.
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Deletes from the start of the line to the cursor.
    pub fn kill_before(&mut self) {
        let start = self.line_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Deletes from the cursor to the end of the line.
    pub fn kill_after(&mut self) {
        let end = self.line_end();
        self.text.replace_range(self.cursor..end, "");
    }

    /// Deletes the word before the cursor.
    pub fn kill_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(' ');
        let start = trimmed.rfind([' ', '\n']).map_or(0, |i| i + 1);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Takes the text out, recording it in history.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.browsing = None;
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Clears the text without recording it.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.browsing = None;
    }

    /// Replaces the text with an earlier entry. Returns false at the oldest.
    pub fn history_back(&mut self) -> bool {
        let at = match &self.browsing {
            Some((0, _)) => return false,
            Some((i, _)) => i - 1,
            None if self.history.is_empty() => return false,
            None => self.history.len() - 1,
        };
        let draft = match self.browsing.take() {
            Some((_, draft)) => draft,
            None => self.text.clone(),
        };
        self.text.clone_from(&self.history[at]);
        self.cursor = self.text.len();
        self.browsing = Some((at, draft));
        true
    }

    /// Moves toward the draft. Returns false when not browsing.
    pub fn history_forward(&mut self) -> bool {
        let Some((at, draft)) = self.browsing.take() else {
            return false;
        };
        if at + 1 < self.history.len() {
            self.text.clone_from(&self.history[at + 1]);
            self.browsing = Some((at + 1, draft));
        } else {
            self.text = draft;
        }
        self.cursor = self.text.len();
        true
    }

    /// Rows of text wrapped to `cols`, and the cursor's row and column.
    fn layout(&self, cols: usize) -> (Vec<String>, (usize, usize)) {
        let cols = cols.max(2);
        let mut rows = vec![String::new()];
        let mut used = 0;
        let mut cursor = (0, 0);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = (rows.len() - 1, used);
            }
            if c == '\n' {
                rows.push(String::new());
                used = 0;
                continue;
            }
            let c = if c == '\t' { ' ' } else { c };
            if c.is_control() {
                continue;
            }
            let w = c.width().unwrap_or(0);
            if used + w > cols {
                rows.push(String::new());
                used = 0;
                if i == self.cursor {
                    cursor = (rows.len() - 1, 0);
                }
            }
            rows.last_mut().expect("a row").push(c);
            used += w;
        }
        if self.cursor == self.text.len() {
            if used >= cols {
                rows.push(String::new());
                used = 0;
            }
            cursor = (rows.len() - 1, used);
        }
        (rows, cursor)
    }

    /// Draws the composer `cols` wide and returns the cursor's row and
    /// column within it.
    #[must_use]
    pub fn render(
        &self,
        cols: usize,
        border: Role,
        hint: &str,
        form: Form,
    ) -> (Vec<Line>, (usize, usize)) {
        let inner = cols.saturating_sub(if form.bare { 2 } else { 6 });
        let (rows, (crow, ccol)) = self.layout(inner);
        let visible = form.rows.max(1);
        let top = crow
            .saturating_sub(visible - 1)
            .min(rows.len().saturating_sub(visible));
        let b = |s: &str| Line::default().span(border, false, s);
        let mut lines = vec![if form.bare {
            b(&"─".repeat(cols))
        } else {
            b(&format!("╭{}╮", "─".repeat(cols.saturating_sub(2))))
        }];
        for (i, row) in rows.iter().enumerate().skip(top).take(visible) {
            let lead = if i == 0 {
                Line::default().brass("ξ ")
            } else {
                Line::default().ink("  ")
            };
            let body = if self.text.is_empty() {
                Line::default().dim(hint).truncate(inner)
            } else {
                Line::default().ink(row)
            };
            let line = if form.bare {
                lead.extend(body)
            } else {
                let pad = inner.saturating_sub(body.width());
                b("│ ")
                    .extend(lead)
                    .extend(body)
                    .ink(" ".repeat(pad))
                    .extend(b(" │"))
            };
            lines.push(line.truncate(cols));
        }
        if !form.bare {
            lines.push(b(&format!("╰{}╯", "─".repeat(cols.saturating_sub(2)))));
        }
        (
            lines,
            (1 + crow - top, if form.bare { 2 } else { 4 } + ccol),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_text_and_history_preserves_byte_cursor_boundaries() {
        let mut e = Editor::default();
        e.set_history(vec!["old".into(), "new".into()]);
        e.set_text("draft ξ");
        assert_eq!(e.cursor(), "draft ξ".len());
        assert!(e.history_back());
        e.set_text("look @ξ.rs tail");
        assert!(!e.history_forward());
        e.replace(5..11, "@other.rs ");
        assert_eq!(e.text(), "look @other.rs  tail");
        assert_eq!(e.cursor(), "look @other.rs ".len());
        assert!(e.text().is_char_boundary(e.cursor()));
        assert!(e.history_back());
        e.set_history(vec!["replacement".into()]);
        assert!(!e.history_forward());
        assert!(e.history_back());
        assert_eq!(e.text(), "replacement");
    }

    #[test]
    fn editing_moves_by_characters_not_bytes() {
        let mut e = Editor::default();
        e.insert("ξ fix it");
        e.home();
        e.right();
        e.delete();
        assert_eq!(e.text(), "ξfix it");
        e.end();
        e.kill_word();
        assert_eq!(e.text(), "ξfix ");
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "ξfi");
    }

    #[test]
    fn history_returns_to_the_draft() {
        let mut e = Editor::default();
        e.insert("first");
        e.take();
        e.insert("second");
        e.take();
        e.insert("draft");
        assert!(e.history_back());
        assert_eq!(e.text(), "second");
        assert!(e.history_back());
        assert!(!e.history_back());
        assert_eq!(e.text(), "first");
        assert!(e.history_forward());
        assert!(e.history_forward());
        assert_eq!(e.text(), "draft");
        assert!(!e.history_forward());
    }

    #[test]
    fn the_box_is_exactly_as_wide_as_asked_and_tracks_the_cursor() {
        let mut e = Editor::default();
        e.insert("one\ntwo three four");
        let form = Form {
            rows: 8,
            bare: false,
        };
        let (lines, cursor) = e.render(16, Role::Brass, "", form);
        assert!(lines.iter().all(|l| l.width() == 16), "{lines:?}");
        assert_eq!(lines[1].plain(), "│ ξ one        │");
        assert_eq!(lines[2].plain(), "│   two three  │");
        assert_eq!(lines[3].plain(), "│   four       │");
        assert_eq!(cursor, (3, 8));
        let (empty, at) = Editor::default().render(30, Role::Brass, "enter sends", form);
        assert_eq!(empty[1].plain(), "│ ξ enter sends              │");
        assert_eq!(at, (1, 4));
    }

    #[test]
    fn bare_composer_scrolls_to_the_cursor_without_side_borders() {
        let mut editor = Editor::default();
        editor.set_text("one\ntwo\nthree\nfour");
        let form = Form {
            rows: 3,
            bare: true,
        };
        let (lines, cursor) = editor.render(16, Role::Murex, "", form);
        assert_eq!(
            lines.iter().map(Line::plain).collect::<Vec<_>>(),
            [
                "─".repeat(16),
                "  two".into(),
                "  three".into(),
                "  four".into()
            ]
        );
        assert_eq!(cursor, (3, 6));
        assert_eq!(lines[0].0[0].role, Role::Murex);
        let (empty, cursor) = Editor::default().render(60, Role::Brass, "enter sends", form);
        assert_eq!(empty[1].plain(), "ξ enter sends");
        assert_eq!(empty[1].0.last().unwrap().role, Role::Dim);
        assert_eq!(cursor, (1, 2));
    }
}
