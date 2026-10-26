//! Main-screen rendering: print what's committed once, redraw only the live
//! region.
//!
//! The live region is the last few rows of the screen. Each frame moves the
//! cursor back to its first row, clears to the end of the screen, prints any
//! newly committed lines (which the terminal then owns as scrollback) and
//! draws the live region again below them. Frames are wrapped in synchronized
//! output (`CSI ?2026`) so terminals that support it never show a half-drawn
//! frame.

use std::io::{self, Write};

use crate::style::Line;

/// The bottom of the main screen, as last drawn.
pub struct Screen<W: Write> {
    out: W,
    color: bool,
    /// Rows the live region occupies.
    height: usize,
    /// Row of the cursor within the live region.
    cursor_row: usize,
}

impl<W: Write> Screen<W> {
    /// A screen with nothing drawn yet.
    pub fn new(out: W, color: bool) -> Self {
        Self {
            out,
            color,
            height: 0,
            cursor_row: 0,
        }
    }

    fn rewind(&self, buf: &mut String) {
        if self.height > 0 {
            if self.cursor_row > 0 {
                buf.push_str(&format!("\x1b[{}A", self.cursor_row));
            }
            buf.push_str("\r\x1b[J");
        }
    }

    /// Commits `committed` to scrollback and redraws `live` below it. `live`
    /// keeps its last `rows` lines when the screen is shorter. `cursor` is a
    /// row and column within `live`; without one the cursor is hidden.
    pub fn draw(
        &mut self,
        committed: &[Line],
        live: &[Line],
        cursor: Option<(usize, usize)>,
        rows: usize,
    ) -> io::Result<()> {
        let mut buf = String::from("\x1b[?2026h\x1b[?25l");
        self.rewind(&mut buf);
        for line in committed {
            line.write_ansi(&mut buf, self.color);
            buf.push_str("\r\n");
        }
        let skip = live.len().saturating_sub(rows.max(1));
        let live = &live[skip..];
        for (i, line) in live.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            line.write_ansi(&mut buf, self.color);
        }
        let last = live.len().saturating_sub(1);
        let target = cursor.and_then(|(r, c)| r.checked_sub(skip).map(|r| (r.min(last), c)));
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
                self.cursor_row = row;
            }
            None => self.cursor_row = last,
        }
        self.height = live.len();
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
        self.height = 0;
        self.cursor_row = 0;
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
    fn committed_lines_print_once_above_a_redrawn_region() {
        let mut s = Screen::new(Vec::new(), false);
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
    fn a_short_screen_keeps_the_bottom_of_the_region() {
        let mut s = Screen::new(Vec::new(), false);
        let l = |t: &str| Line::default().ink(t);
        s.draw(&[], &[l("a"), l("b"), l("c")], Some((2, 0)), 2)
            .unwrap();
        assert!(text(&s).contains("b\r\nc\r\x1b[?25h"), "{:?}", text(&s));
    }
}
