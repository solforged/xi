//! Main-screen and alternate-screen rendering.
//!
//! The live region is the last few rows of the screen. Each frame moves the
//! cursor back to its first row, clears to the end of the screen, prints any
//! newly committed lines (which the terminal then owns as scrollback) and
//! draws the live region again below them. Frames are wrapped in synchronized
//! output (`CSI ?2026`) so terminals that support it never show a half-drawn
//! frame.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::style::Line;

/// How long the width must hold still before a frame is drawn.
const SETTLE: Duration = Duration::from_millis(150);

/// Holds frames back while the terminal width changes.
///
/// A frame's rewind counts the rows the terminal reflowed at the width read
/// before drawing. While a window is dragged, the terminal can resize again
/// before it reads the frame, so the rewind stops short and leaves a copy of
/// the live region behind. Drawing only once the width settles avoids that.
pub struct Settle {
    width: u16,
    until: Option<Instant>,
}

impl Settle {
    /// Settled at `width`.
    pub fn new(width: u16) -> Self {
        Self { width, until: None }
    }

    /// Records the width read `now` and returns whether a frame may draw.
    pub fn ready(&mut self, width: u16, now: Instant) -> bool {
        if width != self.width {
            self.width = width;
            self.until = Some(now + SETTLE);
        }
        if self.until.is_some_and(|until| until <= now) {
            self.until = None;
        }
        self.until.is_none()
    }

    /// When a held-back frame may draw, if one is waiting.
    pub fn deadline(&self) -> Option<Instant> {
        self.until
    }
}

/// The bottom of the main screen, as last drawn.
pub struct Screen<W: Write> {
    out: W,
    color: bool,
    /// Terminal width now.
    cols: usize,
    /// Width of each live row as last drawn, for counting the rows it takes
    /// once a resize reflows it.
    widths: Vec<usize>,
    /// Row and column of the cursor within the live region.
    cursor: (usize, usize),
    /// Erases the display and scrollback at the start of the next frame.
    clear: bool,
}

impl<W: Write> Screen<W> {
    /// A screen `cols` wide with nothing drawn yet.
    pub fn new(out: W, color: bool, cols: usize) -> Self {
        Self {
            out,
            color,
            cols: cols.max(1),
            widths: Vec::new(),
            cursor: (0, 0),
            clear: false,
        }
    }

    /// Records a new terminal width. Terminals reflow the rows already on
    /// screen, so a row drawn wider than `cols` now takes several.
    pub fn resize(&mut self, cols: usize) {
        self.cols = cols.max(1);
    }

    /// Clears the display and native scrollback in the next synchronized frame.
    pub fn clear(&mut self) {
        self.clear = true;
    }

    /// Writes a terminal control sequence before the next frame flushes it.
    ///
    /// # Errors
    /// Returns the writer's I/O error.
    pub fn control(&mut self, sequence: &str) -> io::Result<()> {
        self.out.write_all(sequence.as_bytes())
    }

    /// Enters the deck's alternate screen and forgets the inline live region.
    ///
    /// # Errors
    /// Returns the writer's I/O error.
    pub fn enter_deck(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J")?;
        self.widths.clear();
        self.cursor = (0, 0);
        self.out.flush()
    }

    /// Restores the main screen and forgets the deck's cursor.
    ///
    /// # Errors
    /// Returns the writer's I/O error.
    pub fn leave_deck(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[?1049l")?;
        self.widths.clear();
        self.cursor = (0, 0);
        self.out.flush()
    }

    /// Redraws the deck from the screen's origin without scrolling its last row.
    /// `cursor` is an absolute zero-based screen position.
    ///
    /// # Errors
    /// Returns the writer's I/O error.
    pub fn draw_deck(&mut self, rows: &[Line], cursor: Option<(usize, usize)>) -> io::Result<()> {
        let mut buf = String::from("\x1b[?2026h\x1b[?25l\x1b[H");
        for (i, line) in rows.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            let width = if line.width() > self.cols {
                let line = line.clone().truncate(self.cols);
                line.write_ansi(&mut buf, self.color);
                line.width()
            } else {
                line.write_ansi(&mut buf, self.color);
                line.width()
            };
            // A full row leaves the cursor waiting to wrap on the last
            // cell, where erasing to the end of the line would blank it.
            if width < self.cols {
                buf.push_str("\x1b[K");
            }
        }
        if let Some((row, col)) = cursor {
            buf.push_str(&format!("\x1b[{};{}H\x1b[?25h", row + 1, col + 1));
        }
        buf.push_str("\x1b[?2026l");
        self.out.write_all(buf.as_bytes())?;
        self.out.flush()
    }

    fn rewind(&self, buf: &mut String) {
        if self.widths.is_empty() {
            return;
        }
        let (row, col) = self.cursor;
        let up: usize = self.widths[..row]
            .iter()
            .map(|w| w.div_ceil(self.cols).max(1))
            .sum::<usize>()
            + col / self.cols;
        if up > 0 {
            buf.push_str(&format!("\x1b[{up}A"));
        }
        buf.push_str("\r\x1b[J");
    }

    /// Commits `committed` to scrollback and redraws `live` below it. `live`
    /// keeps its last `rows` lines when the screen is shorter. `cursor` is a
    /// row and column within `live`; without one the cursor is hidden. Live
    /// lines and the cursor are clipped to the current terminal width.
    pub fn draw(
        &mut self,
        committed: &[Line],
        live: &[Line],
        cursor: Option<(usize, usize)>,
        rows: usize,
    ) -> io::Result<()> {
        let mut buf = String::from("\x1b[?2026h\x1b[?25l");
        if std::mem::take(&mut self.clear) {
            buf.push_str("\x1b[H\x1b[2J\x1b[3J");
        } else {
            self.rewind(&mut buf);
        }
        for line in committed {
            line.write_ansi(&mut buf, self.color);
            buf.push_str("\r\n");
        }
        let skip = live.len().saturating_sub(rows.max(1));
        let live: Vec<_> = live[skip..]
            .iter()
            .cloned()
            .map(|line| line.truncate(self.cols))
            .collect();
        for (i, line) in live.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            line.write_ansi(&mut buf, self.color);
        }
        let last = live.len().saturating_sub(1);
        let target = cursor.and_then(|(r, c)| {
            r.checked_sub(skip)
                .map(|r| (r.min(last), c.min(self.cols - 1)))
        });
        match target {
            Some((row, col)) => {
                if last > row {
                    buf.push_str(&format!("\x1b[{}A", last - row));
                }
                buf.push('\r');
                if col > 0 {
                    buf.push_str(&format!("\x1b[{col}C"));
                }
                buf.push_str("\x1b[?25h");
                self.cursor = (row, col);
            }
            // Short rows leave the cursor after the text; a full row leaves
            // it on the final cell with autowrap pending.
            None => {
                let width = live.last().map_or(0, Line::width);
                self.cursor = (last, width.min(self.cols - 1));
            }
        }
        self.widths = live.iter().map(Line::width).collect();
        buf.push_str("\x1b[?2026l");
        self.out.write_all(buf.as_bytes())?;
        self.out.flush()
    }

    /// Erases the live region and leaves the cursor where it began, so the
    /// shell prompt lands right under the last committed line.
    pub fn finish(&mut self) -> io::Result<()> {
        let mut buf = String::new();
        self.rewind(&mut buf);
        buf.push_str("\x1b[?25h");
        self.widths.clear();
        self.cursor = (0, 0);
        self.out.write_all(buf.as_bytes())?;
        self.out.flush()
    }

    /// The writer, for tests.
    #[cfg(test)]
    fn output(&self) -> &W {
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(screen: &Screen<Vec<u8>>) -> String {
        String::from_utf8(screen.output().clone()).unwrap()
    }

    #[test]
    fn deck_switches_forget_inline_widths_and_cursor() {
        let mut s = Screen::new(Vec::new(), false, 80);
        s.draw(&[], &[Line::default().ink("ξ draft")], Some((0, 2)), 24)
            .unwrap();
        assert!(!s.widths.is_empty());
        s.enter_deck().unwrap();
        assert!(text(&s).ends_with("\x1b[?1049h\x1b[H\x1b[2J"));
        assert!(s.widths.is_empty());
        assert_eq!(s.cursor, (0, 0));
        s.draw_deck(&[Line::default().ink("deck")], Some((0, 2)))
            .unwrap();
        s.leave_deck().unwrap();
        assert!(text(&s).ends_with("\x1b[?1049l"));
        assert!(s.widths.is_empty());
        assert_eq!(s.cursor, (0, 0));
        s.draw(&[], &[Line::default().ink("inline")], None, 24)
            .unwrap();
        assert!(text(&s).ends_with("\x1b[?2026h\x1b[?25linline\x1b[?2026l"));
    }

    #[test]
    fn deck_draws_from_home_erases_rows_and_places_an_absolute_cursor() {
        let mut s = Screen::new(Vec::new(), false, 8);
        s.draw_deck(
            &[
                Line::default().ink("first"),
                Line::default().ink("界界界界界"),
                Line::default(),
            ],
            Some((1, 3)),
        )
        .unwrap();
        assert_eq!(
            text(&s),
            "\x1b[?2026h\x1b[?25l\x1b[Hfirst\x1b[K\r\n界界界…\x1b[K\r\n\x1b[K\x1b[2;4H\x1b[?25h\x1b[?2026l"
        );
    }

    #[test]
    fn deck_full_rows_skip_the_erase_that_would_blank_their_last_cell() {
        let mut s = Screen::new(Vec::new(), false, 4);
        s.draw_deck(&[Line::default().ink("full")], None).unwrap();
        assert_eq!(text(&s), "\x1b[?2026h\x1b[?25l\x1b[Hfull\x1b[?2026l");
        s.draw_deck(&[], None).unwrap();
        assert!(text(&s).ends_with("\x1b[?2026h\x1b[?25l\x1b[H\x1b[?2026l"));
    }

    #[test]
    fn committed_lines_print_once_above_a_redrawn_region() {
        let mut s = Screen::new(Vec::new(), false, 80);
        let l = |t: &str| Line::default().ink(t);
        s.draw(&[], &[l("working"), l("ξ "), l("footer")], Some((1, 2)), 24)
            .unwrap();
        s.draw(&[l("● bash")], &[l("ξ "), l("footer")], Some((0, 2)), 24)
            .unwrap();
        let out = text(&s);
        let frames: Vec<&str> = out.split("\x1b[?2026h").skip(1).collect();
        assert_eq!(
            frames[0],
            "\x1b[?25lworking\r\nξ \r\nfooter\x1b[1A\r\x1b[2C\x1b[?25h\x1b[?2026l"
        );
        // Back up to the region's first row, clear, commit, redraw.
        assert_eq!(
            frames[1],
            "\x1b[?25l\x1b[1A\r\x1b[J● bash\r\nξ \r\nfooter\x1b[1A\r\x1b[2C\x1b[?25h\x1b[?2026l"
        );
        s.finish().unwrap();
        assert!(text(&s).ends_with("\r\x1b[J\x1b[?25h"));
    }

    #[test]
    fn clear_erases_scrollback_and_redraws_from_home_without_a_stale_rewind() {
        let mut s = Screen::new(Vec::new(), false, 10);
        let l = |t: &str| Line::default().ink(t);
        s.draw(
            &[l("old output")],
            &[l("working"), l("ξ draft"), l("footer")],
            Some((1, 7)),
            24,
        )
        .unwrap();
        s.resize(80);
        s.clear();
        s.draw(&[l("fresh")], &[l("ξ "), l("footer")], Some((0, 2)), 24)
            .unwrap();
        s.draw(&[], &[l("ξ "), l("footer")], Some((0, 2)), 24)
            .unwrap();
        let out = text(&s);
        let frames: Vec<_> = out.split("\x1b[?2026h").skip(1).collect();
        assert_eq!(
            frames[1],
            "\x1b[?25l\x1b[H\x1b[2J\x1b[3Jfresh\r\nξ \r\nfooter\x1b[1A\r\x1b[2C\x1b[?25h\x1b[?2026l"
        );
        assert_eq!(
            frames[2],
            "\x1b[?25l\r\x1b[Jξ \r\nfooter\x1b[1A\r\x1b[2C\x1b[?25h\x1b[?2026l"
        );
    }

    #[test]
    fn a_short_screen_keeps_the_bottom_of_the_region() {
        let mut s = Screen::new(Vec::new(), false, 80);
        let l = |t: &str| Line::default().ink(t);
        s.draw(&[], &[l("a"), l("b"), l("c")], Some((2, 0)), 2)
            .unwrap();
        assert!(text(&s).contains("b\r\nc\r\x1b[?25h"), "{:?}", text(&s));
    }

    #[test]
    fn stale_live_widths_are_clipped_without_wrapping_or_clipping_commits() {
        let mut s = Screen::new(Vec::new(), false, 8);
        let l = |t: &str| Line::default().ink(t);
        let live = [l("╭──────────╮"), l("│ ξ draft  │"), l("╰──────────╯")];
        s.draw(&[], &live, Some((1, 10)), 24).unwrap();
        s.draw(
            &[l("committed output stays unwrapped")],
            &[l("ready")],
            None,
            24,
        )
        .unwrap();
        let out = text(&s);
        let frames: Vec<_> = out.split("\x1b[?2026h").skip(1).collect();
        assert_eq!(
            frames[0],
            "\x1b[?25l╭──────…\r\n│ ξ dra…\r\n╰──────…\x1b[1A\r\x1b[7C\x1b[?25h\x1b[?2026l"
        );
        assert_eq!(
            frames[1],
            "\x1b[?25l\x1b[1A\r\x1b[Jcommitted output stays unwrapped\r\nready\x1b[?2026l"
        );
    }

    #[test]
    fn frames_wait_until_the_width_holds_still() {
        let t = Instant::now();
        let mut s = Settle::new(80);
        assert!(s.ready(80, t));
        assert_eq!(s.deadline(), None);
        assert!(!s.ready(70, t));
        assert!(!s.ready(60, t + SETTLE / 2));
        // Each change restarts the wait.
        assert!(!s.ready(60, t + SETTLE));
        assert_eq!(s.deadline(), Some(t + SETTLE / 2 + SETTLE));
        assert!(s.ready(60, t + SETTLE / 2 + SETTLE));
        assert_eq!(s.deadline(), None);
        assert!(s.ready(60, t + SETTLE * 3));
    }

    #[test]
    fn narrowing_tracks_the_hidden_cursor_after_a_short_last_row() {
        let mut s = Screen::new(Vec::new(), false, 10);
        let l = |t: &str| Line::default().ink(t);
        s.draw(&[], &[l("top"), l("footer")], None, 24).unwrap();
        assert_eq!(s.cursor, (1, 6));
        s.resize(3);
        s.finish().unwrap();
        assert!(text(&s).ends_with("\x1b[3A\r\x1b[J\x1b[?25h"));
    }

    #[test]
    fn narrowing_rewinds_over_the_rows_the_terminal_reflowed() {
        let mut s = Screen::new(Vec::new(), false, 10);
        let l = |t: &str| Line::default().ink(t);
        let live = [
            l("streaming!"),
            l("╭────────╮"),
            l("│ ξ ab   │"),
            l("footer"),
        ];
        s.draw(&[], &live, Some((2, 6)), 24).unwrap();
        s.resize(4);
        s.draw(&[], &live[1..], None, 24).unwrap();
        // Two ten-cell rows now take three rows each; the cursor's own row
        // wrapped once before column 6.
        assert!(
            text(&s).contains("\x1b[?2026h\x1b[?25l\x1b[7A\r\x1b[J╭"),
            "{:?}",
            text(&s)
        );
        s.resize(40);
        s.draw(&[], &live[1..], None, 24).unwrap();
        // The cursor rested at the end of `footer`, two rows below the top.
        assert!(
            text(&s).ends_with("\x1b[2A\r\x1b[J╭────────╮\r\n│ ξ ab   │\r\nfooter\x1b[?2026l"),
            "{:?}",
            text(&s)
        );
    }
}
