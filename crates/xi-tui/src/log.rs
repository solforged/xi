//! The deck's retained log.
//!
//! Inline, committed rows go to the terminal's scrollback and are forgotten.
//! On the deck xi owns the screen, so the log keeps every committed row,
//! which rows the inspector can open, and which of those is selected.

use crate::style::Line;

/// What the inspector shows for a row of the log: a call's headline, its
/// outcome and its whole output or diff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inspect {
    /// The headline, as plain text: `edit ×5 crates/xi-tools/src/bash.rs`.
    pub title: String,
    /// The outcome, as plain text: `+22 −16`, `exit 101 · 6.8s`, `9 hits`.
    pub meta: String,
    /// A note under the title: `net diff · 3 hunks`, `12 lines`.
    pub note: String,
    /// Whether `text` is a diff whose rows start with `- `, `+ ` or two
    /// spaces.
    pub diff: bool,
    /// The whole text, or empty when the call produced none.
    pub text: String,
}

/// Committed rows kept for the deck, oldest first, with the rows the
/// inspector can open.
#[derive(Default)]
pub struct Log {
    rows: Vec<Line>,
    /// Row index and what the inspector shows for it, in row order.
    anchors: Vec<(usize, Inspect)>,
    /// Index into `anchors` of the selected row; the newest when `None`.
    selected: Option<usize>,
    /// Rows scrolled up from the bottom.
    scrolled: usize,
    /// Whether the rows shown follow the selection, after it moved.
    reveal: bool,
}

impl Log {
    /// Appends committed rows, with `anchors` as offsets into `rows`.
    pub fn extend(&mut self, rows: Vec<Line>, anchors: Vec<(usize, Inspect)>) {
        let base = self.rows.len();
        self.anchors
            .extend(anchors.into_iter().map(|(at, i)| (base + at, i)));
        self.rows.extend(rows);
    }

    /// Every row, oldest first.
    #[must_use]
    pub fn rows(&self) -> &[Line] {
        &self.rows
    }

    /// Takes every row, for printing to scrollback when leaving the deck.
    pub fn take(&mut self) -> Vec<Line> {
        self.anchors.clear();
        self.selected = None;
        self.scrolled = 0;
        self.reveal = false;
        std::mem::take(&mut self.rows)
    }

    /// Row index of the selected anchor.
    #[must_use]
    pub fn selected_row(&self) -> Option<usize> {
        self.selected_at().map(|at| self.anchors[at].0)
    }

    /// What the inspector shows for the selected row.
    #[must_use]
    pub fn selected(&self) -> Option<&Inspect> {
        self.selected_at().map(|at| &self.anchors[at].1)
    }

    fn selected_at(&self) -> Option<usize> {
        let last = self.anchors.len().checked_sub(1)?;
        Some(self.selected.map_or(last, |at| at.min(last)))
    }

    /// Selects the row above and brings it into view.
    pub fn up(&mut self) {
        if let Some(at) = self.selected_at() {
            self.selected = Some(at.saturating_sub(1));
            self.follow();
        }
    }

    /// Selects the row below and brings it into view; at the newest the
    /// selection follows new rows.
    pub fn down(&mut self) {
        if let Some(at) = self.selected_at() {
            let last = self.anchors.len() - 1;
            self.selected = (at + 1 < last).then_some(at + 1);
            self.follow();
        }
    }

    fn follow(&mut self) {
        self.scrolled = 0;
        self.reveal = true;
    }

    /// Whether the rows shown follow the selection.
    #[must_use]
    pub fn reveal(&self) -> bool {
        self.reveal
    }

    /// Rows scrolled up from the bottom.
    #[must_use]
    pub fn scrolled(&self) -> usize {
        self.scrolled
    }

    /// Scrolls `by` rows up, keeping at least one row visible, and lets
    /// the selection leave the rows shown.
    pub fn scroll_up(&mut self, by: usize) {
        self.scrolled = (self.scrolled + by).min(self.rows.len().saturating_sub(1));
        self.reveal = false;
    }

    /// Scrolls `by` rows back down.
    pub fn scroll_down(&mut self, by: usize) {
        self.scrolled = self.scrolled.saturating_sub(by);
        self.reveal = false;
    }

    /// Follows the newest rows again.
    pub fn scroll_end(&mut self) {
        self.scrolled = 0;
        self.reveal = false;
    }

    /// Selects the newest row again and follows new rows.
    pub fn select_newest(&mut self) {
        self.selected = None;
        self.scroll_end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(title: &str) -> Inspect {
        Inspect {
            title: title.into(),
            ..Inspect::default()
        }
    }

    #[test]
    fn selection_follows_the_newest_until_moved_and_offsets_by_prior_rows() {
        let mut log = Log::default();
        assert!(log.selected().is_none());
        log.extend(
            vec![Line::default(), Line::default()],
            vec![(1, inspect("a"))],
        );
        log.extend(
            vec![Line::default(), Line::default(), Line::default()],
            vec![(0, inspect("b")), (2, inspect("c"))],
        );
        assert_eq!(log.selected_row(), Some(4));
        assert_eq!(log.selected().map(|i| i.title.as_str()), Some("c"));
        log.up();
        assert_eq!(log.selected_row(), Some(2));
        log.up();
        log.up();
        assert_eq!(log.selected_row(), Some(1));
        log.down();
        log.down();
        assert_eq!(log.selected_row(), Some(4));
        log.extend(vec![Line::default()], vec![(0, inspect("d"))]);
        assert_eq!(log.selected().map(|i| i.title.as_str()), Some("d"));
        log.up();
        log.extend(vec![Line::default()], vec![(0, inspect("e"))]);
        assert_eq!(log.selected().map(|i| i.title.as_str()), Some("c"));
        let rows = log.take();
        assert_eq!(rows.len(), 7);
        assert!(log.selected().is_none());
        assert!(log.rows().is_empty());
    }

    #[test]
    fn scrolling_stays_within_the_rows() {
        let mut log = Log::default();
        log.extend(vec![Line::default(); 5], Vec::new());
        log.scroll_up(10);
        assert_eq!(log.scrolled(), 4);
        log.scroll_down(1);
        assert_eq!(log.scrolled(), 3);
        log.scroll_end();
        assert_eq!(log.scrolled(), 0);
    }
}
