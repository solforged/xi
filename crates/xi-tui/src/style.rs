//! Pigments, spans and width-bounded lines.
//!
//! Every pigment maps to an ANSI index, so the user's terminal theme decides
//! the hue. Text steps through four tones, bright, ink, muted and dim, so
//! what matters reads first without a hue of its own. The one exception is the raised surface: the deck lifts the
//! terminal's own background a little behind its panel, prompts and
//! composer, when the terminal reports that background. A line never
//! exceeds the width it was built for: anything wider is cut with `…`,
//! because a wrapped line would break the live region's row count.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A background colour as red, green and blue.
pub type Rgb = [u8; 3];

/// A colour role. Each has exactly one meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// What the eye lands on first: a title, the file in a path. ANSI
    /// bright white.
    Bright,
    /// What you read. The terminal's default foreground.
    Ink,
    /// What you read second: tool names and their output. ANSI white.
    Muted,
    /// Apparatus: metadata, connectors, hints, and the model's reasoning.
    /// ANSI bright black.
    Dim,
    /// Hairlines that only group rows, and may vanish. ANSI black.
    Faint,
    /// Labels: section names and list marks. ANSI bright red.
    Rubric,
    /// You and now, anything waiting on you, and warnings. ANSI yellow.
    Brass,
    /// Done, passed, added. ANSI green.
    Verdigris,
    /// Failed, removed. ANSI red.
    Sanguine,
    /// Taint, and nothing else. ANSI magenta.
    Murex,
    /// Literal text quoted in prose: inline code. ANSI cyan.
    Azurite,
}

impl Role {
    fn sgr(self) -> Option<&'static str> {
        match self {
            Self::Bright => Some("97"),
            Self::Ink => None,
            Self::Muted => Some("37"),
            Self::Dim => Some("90"),
            Self::Faint => Some("30"),
            Self::Rubric => Some("91"),
            Self::Brass => Some("33"),
            Self::Verdigris => Some("32"),
            Self::Sanguine => Some("31"),
            Self::Murex => Some("35"),
            Self::Azurite => Some("36"),
        }
    }
}

/// A run of text in one role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Printable text: no control characters.
    pub text: String,
    /// Its pigment.
    pub role: Role,
    /// Set in bold.
    pub bold: bool,
    /// Underlined, for marks that must not change the text's role, such as
    /// a search match on a selected row.
    pub underline: bool,
    /// Set in italics, for the model's reasoning.
    pub italic: bool,
    /// Set on the raised surface, where the deck draws one.
    pub raised: bool,
}

/// One terminal row of spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line(pub Vec<Span>);

impl From<&str> for Line {
    fn from(text: &str) -> Self {
        Self::default().ink(text)
    }
}

impl From<String> for Line {
    fn from(text: String) -> Self {
        Self::default().ink(text)
    }
}

/// Replaces tabs and drops escape sequences and control characters, so the
/// width is what prints. A carriage return followed by more text keeps only
/// what comes after it, the way a progress bar ends up on screen.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut returned = false;
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\r' => returned = true,
            c if c.is_control() && c != '\t' => {}
            c => {
                if std::mem::take(&mut returned) {
                    out.clear();
                }
                if c == '\t' {
                    out.push_str("    ");
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Skips the rest of an escape sequence: CSI to its final byte, OSC and the
/// other string sequences to BEL or ST, anything else one character.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    chars.next_if_eq(&'\\');
                    break;
                }
            }
        }
        _ => {}
    }
}

macro_rules! role_methods {
    ($($name:ident => $role:ident),* $(,)?) => {
        $(
            #[doc = concat!("Appends text in [`Role::", stringify!($role), "`].")]
            #[must_use]
            pub fn $name(self, text: impl AsRef<str>) -> Self {
                self.span(Role::$role, false, text)
            }
        )*
    };
}

impl Line {
    /// Appends a span.
    #[must_use]
    pub fn span(mut self, role: Role, bold: bool, text: impl AsRef<str>) -> Self {
        let text = clean(text.as_ref());
        if !text.is_empty() {
            self.0.push(Span {
                text,
                role,
                bold,
                underline: false,
                italic: false,
                raised: false,
            });
        }
        self
    }

    role_methods!(
        bright => Bright,
        ink => Ink,
        muted => Muted,
        dim => Dim,
        faint => Faint,
        rubric => Rubric,
        brass => Brass,
        verdigris => Verdigris,
        sanguine => Sanguine,
        murex => Murex,
    );

    /// Appends bold text in a role.
    #[must_use]
    pub fn strong(self, role: Role, text: impl AsRef<str>) -> Self {
        self.span(role, true, text)
    }

    /// Appends underlined text in a role.
    #[must_use]
    pub fn underline(mut self, role: Role, text: impl AsRef<str>) -> Self {
        let text = clean(text.as_ref());
        if !text.is_empty() {
            self.0.push(Span {
                text,
                role,
                bold: false,
                underline: true,
                italic: false,
                raised: false,
            });
        }
        self
    }

    /// Appends another line's spans.
    #[must_use]
    pub fn extend(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self
    }

    /// Sets every span in italics.
    #[must_use]
    pub fn italic(mut self) -> Self {
        for span in &mut self.0 {
            span.italic = true;
        }
        self
    }

    /// Sets every span on the raised surface. [`Line::fill`] carries it to
    /// the edge of a column.
    #[must_use]
    pub fn raise(mut self) -> Self {
        for span in &mut self.0 {
            span.raised = true;
        }
        self
    }

    /// Cuts the line to `cols` cells and, when it starts on the raised
    /// surface, pads it with that surface to exactly `cols`.
    #[must_use]
    pub fn fill(self, cols: usize) -> Self {
        let line = self.truncate(cols);
        let gap = cols - line.width();
        if gap == 0 || !line.0.first().is_some_and(|span| span.raised) {
            return line;
        }
        line.extend(Line::default().ink(" ".repeat(gap)).raise())
    }

    /// Display width in terminal cells.
    #[must_use]
    pub fn width(&self) -> usize {
        self.0.iter().map(|s| s.text.width()).sum()
    }

    /// The text without styling.
    #[must_use]
    pub fn plain(&self) -> String {
        self.0.iter().map(|s| s.text.as_str()).collect()
    }

    /// Cuts the line to `cols` cells, ending in `…` when anything was lost.
    #[must_use]
    pub fn truncate(mut self, cols: usize) -> Self {
        if self.width() <= cols {
            return self;
        }
        let budget = cols.saturating_sub(1);
        let mut used = 0;
        let mut out = Vec::new();
        for span in self.0.drain(..) {
            let mut text = String::new();
            for c in span.text.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > budget {
                    break;
                }
                used += w;
                text.push(c);
            }
            let full = text.len() == span.text.len();
            if !text.is_empty() {
                out.push(Span { text, ..span });
            }
            if !full {
                break;
            }
        }
        let mut line = Self(out);
        if cols > 0 {
            let (role, raised) = line
                .0
                .last()
                .map_or((Role::Dim, false), |s| (s.role, s.raised));
            line = line.span(role, false, "…");
            if let Some(last) = line.0.last_mut() {
                last.raised = raised;
            }
        }
        line
    }

    /// Writes the line with SGR codes, or plain when `color` is off. Raised
    /// spans get `surface` as their background when one is given.
    pub fn write_ansi(&self, out: &mut String, color: bool, surface: Option<Rgb>) {
        use std::fmt::Write as _;
        for span in &self.0 {
            let bg = surface.filter(|_| color && span.raised);
            let codes = [
                (color && span.bold).then_some("1"),
                (color && span.underline).then_some("4"),
                (color && span.italic).then_some("3"),
                span.role.sgr().filter(|_| color),
            ];
            if bg.is_none() && codes.iter().all(Option::is_none) {
                out.push_str(&span.text);
                continue;
            }
            out.push_str("\x1b[");
            let mut first = true;
            for code in codes.into_iter().flatten() {
                if !first {
                    out.push(';');
                }
                out.push_str(code);
                first = false;
            }
            if let Some([r, g, b]) = bg {
                let sep = if first { "" } else { ";" };
                let _ = write!(out, "{sep}48;2;{r};{g};{b}");
            }
            out.push('m');
            out.push_str(&span.text);
            out.push_str("\x1b[0m");
        }
    }
}

/// `left`, then `right` flush to column `cols`. When both don't fit, `left`
/// is cut first so the outcome on the right stays readable, and the cut
/// keeps two cells before `right` so its `…` doesn't run into it.
#[must_use]
pub fn lr(left: Line, right: Line, cols: usize) -> Line {
    let (lw, rw) = (left.width(), right.width());
    if lw + 1 + rw <= cols {
        return left.ink(" ".repeat(cols - lw - rw)).extend(right);
    }
    if rw + 12 <= cols {
        let left = left.truncate(cols - rw - 2);
        let gap = cols - left.width() - rw;
        return left.ink(" ".repeat(gap)).extend(right);
    }
    left.truncate(cols)
}

/// A list row: `left`, then `right` flush to column `cols`. Unlike [`lr`],
/// the right side is metadata, so it is cut first and drops once fewer
/// than six cells are left for it; only then is `left` cut.
#[must_use]
pub fn row(left: Line, right: Line, cols: usize) -> Line {
    let (lw, rw) = (left.width(), right.width());
    if lw + 1 + rw <= cols {
        return left.ink(" ".repeat(cols - lw - rw)).extend(right);
    }
    let room = cols.saturating_sub(lw + 2);
    if room >= 6 {
        let right = right.truncate(room);
        let gap = cols - lw - right.width();
        return left.ink(" ".repeat(gap)).extend(right);
    }
    left.truncate(cols)
}

/// A line of `─` in dim, `cols` wide.
#[must_use]
pub fn rule(cols: usize) -> Line {
    Line::default().dim("─".repeat(cols))
}

/// `text` cut to `cols` cells at its last space, ending in `…`, so a cut
/// row doesn't end mid-word. A cut that would keep less than half the row
/// falls back to the last cell that fits.
#[must_use]
pub fn clip_words(text: &str, cols: usize) -> String {
    if text.width() <= cols {
        return text.to_owned();
    }
    if cols == 0 {
        return String::new();
    }
    let budget = cols.saturating_sub(1);
    let (mut used, mut end, mut space) = (0, 0, None);
    for (i, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        if c == ' ' {
            space = Some((i, used));
        }
        used += w;
        end = i + c.len_utf8();
    }
    let cut = space
        .filter(|&(_, at)| at * 2 >= budget)
        .map_or(end, |(i, _)| i);
    format!("{}…", text[..cut].trim_end())
}

/// Word-wraps text to `cols` cells, keeping its own line breaks and each
/// line's indentation. Words wider than a line are split.
#[must_use]
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut rows = Vec::new();
    for para in clean_lines(text) {
        let body = para.trim_start_matches(' ');
        let indent = (para.len() - body.len()).min(cols / 2);
        let mut row = " ".repeat(indent);
        let mut used = indent;
        // Whether the row holds a word yet, so the first one needs no space.
        let mut started = false;
        for word in body.split(' ') {
            let w = word.width();
            if started && used + 1 + w > cols {
                rows.push(std::mem::take(&mut row));
                used = 0;
                started = false;
            }
            if started {
                row.push(' ');
                used += 1;
            }
            started = true;
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > cols {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += cw;
            }
        }
        rows.push(row);
    }
    rows
}

/// Word-wraps a styled line like [`wrap`]; each character keeps its span's
/// style.
#[must_use]
pub fn wrap_line(line: &Line, cols: usize) -> Vec<Line> {
    let styled: Vec<(char, &Span)> = line
        .0
        .iter()
        .flat_map(|span| span.text.chars().map(move |c| (c, span)))
        .collect();
    // A row is the line's characters in order, less the spaces wrapping
    // dropped, so each row character finds its span by skipping ahead.
    let mut at = 0;
    let mut rows = Vec::new();
    for row in wrap(&line.plain(), cols) {
        let mut out: Vec<Span> = Vec::new();
        for c in row.chars() {
            while at < styled.len() && styled[at].0 != c {
                at += 1;
            }
            let Some(&(_, span)) = styled.get(at).or(styled.last()) else {
                continue;
            };
            at += 1;
            match out.last_mut() {
                Some(last)
                    if (
                        last.role,
                        last.bold,
                        last.underline,
                        last.italic,
                        last.raised,
                    ) == (
                        span.role,
                        span.bold,
                        span.underline,
                        span.italic,
                        span.raised,
                    ) =>
                {
                    last.text.push(c);
                }
                _ => out.push(Span {
                    text: c.to_string(),
                    ..*span
                }),
            }
        }
        rows.push(Line(out));
    }
    rows
}

fn clean_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split('\n').map(|l| clean(l.trim_end_matches('\r')))
}

/// How one character of prose is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Plain,
    Strong,
    Code,
}

/// The position of the next run of exactly `n` backticks at or after `from`.
fn backtick_run(chars: &[char], from: usize, n: usize) -> Option<usize> {
    let mut at = from;
    while at < chars.len() {
        if chars[at] == '`' {
            let len = chars[at..].iter().take_while(|&&c| c == '`').count();
            if len == n {
                return Some(at);
            }
            at += len;
        } else {
            at += 1;
        }
    }
    None
}

/// Strips `**strong**` and `` `code` `` markers from one cleaned line,
/// returning its text and how each character is set. A marker without a
/// partner on the same line stays literal.
fn marks(line: &str) -> (String, Vec<Mark>) {
    let chars: Vec<char> = line.chars().collect();
    let mut text = String::with_capacity(line.len());
    let mut set = Vec::with_capacity(chars.len());
    let mut strong = false;
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        if c == '`' {
            let run = chars[at..].iter().take_while(|&&c| c == '`').count();
            if let Some(close) = backtick_run(&chars, at + run, run) {
                let mut body = &chars[at + run..close];
                // One space pads code that starts or ends with a backtick.
                if body.len() > 2
                    && body[0] == ' '
                    && body[body.len() - 1] == ' '
                    && body.iter().any(|&c| c != ' ')
                {
                    body = &body[1..body.len() - 1];
                }
                text.extend(body);
                set.extend(std::iter::repeat_n(Mark::Code, body.len()));
                at = close + run;
            } else {
                let mark = if strong { Mark::Strong } else { Mark::Plain };
                text.extend(&chars[at..at + run]);
                set.extend(std::iter::repeat_n(mark, run));
                at += run;
            }
            continue;
        }
        if c == '*' && chars.get(at + 1) == Some(&'*') {
            let opens = !strong
                && chars.get(at + 2).is_some_and(|c| !c.is_whitespace())
                && (at + 3..chars.len()).any(|j| {
                    chars[j] == '*' && chars[j - 1] == '*' && !chars[j - 2].is_whitespace()
                });
            let closes = strong && at > 0 && !chars[at - 1].is_whitespace();
            if opens || closes {
                strong = opens;
                at += 2;
                continue;
            }
        }
        text.push(c);
        set.push(if strong { Mark::Strong } else { Mark::Plain });
        at += 1;
    }
    (text, set)
}

/// Appends `text` set as `mark` in prose of `role`. Code keeps the dim role
/// of apparatus such as thinking, and is Azurite in ordinary prose.
fn marked(line: Line, mark: Mark, role: Role, text: &str) -> Line {
    match mark {
        Mark::Plain => line.span(role, false, text),
        Mark::Strong => line.span(role, true, text),
        Mark::Code if role == Role::Ink => line.span(Role::Azurite, false, text),
        Mark::Code => line.span(role, false, text),
    }
}

/// One line of prose in `role` with its inline markdown set, unwrapped.
#[must_use]
pub fn inline(text: &str, role: Role) -> Line {
    let (text, set) = marks(&clean(text));
    let mut line = Line::default();
    let mut run = String::new();
    let mut current = Mark::Plain;
    for (c, &mark) in text.chars().zip(&set) {
        if mark != current {
            line = marked(line, current, role, &std::mem::take(&mut run));
            current = mark;
        }
        run.push(c);
    }
    marked(line, current, role, &run)
}

/// Word-wraps one line of prose in `role`, setting its inline markdown.
/// `literal` keeps the markers, as inside a fenced code block.
fn set(text: &str, cols: usize, role: Role, literal: bool) -> Vec<Line> {
    let (text, set) = if literal {
        let n = text.chars().count();
        (text.to_owned(), vec![Mark::Plain; n])
    } else {
        marks(text)
    };
    let chars: Vec<char> = text.chars().collect();
    // A row is the line's characters in order, less the spaces wrapping
    // dropped, so each row character finds its mark by skipping ahead.
    let mut at = 0;
    let mut lines = Vec::new();
    for row in wrap(&text, cols) {
        let mut line = Line::default();
        let mut run = String::new();
        let mut current = Mark::Plain;
        for c in row.chars() {
            while at < chars.len() && chars[at] != c {
                at += 1;
            }
            let mark = set.get(at).copied().unwrap_or(Mark::Plain);
            at += 1;
            if mark != current {
                line = marked(line, current, role, &std::mem::take(&mut run));
                current = mark;
            }
            run.push(c);
        }
        lines.push(marked(line, current, role, &run));
    }
    lines
}

/// `body` wrapped beside `lead`, its later rows hanging under the body's
/// first column.
fn hang(lead: Line, body: &str, cols: usize, role: Role) -> Vec<Line> {
    let width = lead.width();
    set(body, cols.saturating_sub(width).max(1), role, false)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                lead.clone()
            } else {
                Line::default().ink(" ".repeat(width))
            };
            lead.extend(row)
        })
        .collect()
}

/// Every span of `line` in `role`.
fn recolor(mut line: Line, role: Role) -> Line {
    for span in &mut line.0 {
        span.role = role;
    }
    line
}

/// One line of prose outside a fence: a heading in bold, a
/// task with an open `○` or a done `✓`, a list item behind its bullet or
/// number, or a paragraph.
fn block(raw: &str, cols: usize, role: Role) -> Vec<Line> {
    let body = raw.trim_start();
    let indent = Line::default().ink(" ".repeat(raw.len() - body.len()));
    let label = if role == Role::Ink {
        Role::Rubric
    } else {
        role
    };
    let hashes = body.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && body[hashes..].starts_with(' ') {
        let heading = if role == Role::Ink { Role::Brass } else { role };
        return set(body[hashes..].trim(), cols, role, false)
            .into_iter()
            .map(|row| {
                let mut row = recolor(row, heading);
                for span in &mut row.0 {
                    span.bold = true;
                }
                row
            })
            .collect();
    }
    if let Some(item) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|bullet| body.strip_prefix(bullet))
    {
        let task = ["[ ] ", "[x] ", "[X] "].iter().find_map(|box_| {
            item.strip_prefix(box_)
                .map(|rest| (box_.as_bytes()[1] != b' ', rest))
        });
        return match task {
            Some((false, rest)) => hang(indent.dim("○ "), rest, cols, role),
            // The tick keeps its colour; the text recedes.
            Some((true, rest)) => hang(indent.verdigris("✓ "), rest, cols, Role::Dim),
            None => hang(indent.span(label, false, "• "), item, cols, role),
        };
    }
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits)
        && let Some(rest) = body[digits..]
            .strip_prefix(". ")
            .or_else(|| body[digits..].strip_prefix(") "))
    {
        let number = format!("{} ", &body[..=digits]);
        return hang(indent.span(label, false, number), rest, cols, role);
    }
    set(raw, cols, role, false)
}

/// Word-wraps prose like [`wrap`], in `role`, setting `**strong**` in bold,
/// `` `code` `` apart, and markdown's headings, lists and tasks as blocks.
/// Lines inside a fenced code block stay literal, with their fences dim.
/// Quotes and callouts are [`crate::callout::markdown`]'s.
#[must_use]
pub fn prose(text: &str, cols: usize, role: Role) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut fenced = false;
    for line in clean_lines(text) {
        let fence = line.trim_start().starts_with("```");
        if fenced || fence {
            fenced ^= fence;
            let code = if fence { Role::Dim } else { role };
            lines.extend(set(&line, cols, code, true));
        } else {
            lines.extend(block(&line, cols, role));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_width_and_marks_the_cut() {
        let line = Line::default()
            .brass("ξ ")
            .ink("a long message that will not fit");
        let cut = line.clone().truncate(12);
        assert_eq!(cut.plain(), "ξ a long me…");
        assert_eq!(cut.width(), 12);
        assert_eq!(line.clone().truncate(80), line);
    }

    #[test]
    fn escape_sequences_and_control_characters_never_reach_the_terminal() {
        let line = Line::default().ink("a\x1b[2Jb\tc\r");
        assert_eq!(line.plain(), "ab    c");
        let colored = "\x1b[1;31merror\x1b[0m: \x1b]8;;https://x\x07link\x1b]8;;\x1b\\ done";
        assert_eq!(Line::default().ink(colored).plain(), "error: link done");
        assert_eq!(Line::default().ink("10%\r55%\r100%").plain(), "100%");
    }

    #[test]
    fn list_rows_cut_their_metadata_before_their_own_text() {
        let row = |cols| {
            super::row(
                Line::default().ink("› Compaction trigger    80% of window"),
                Line::default().dim("--config extra.toml"),
                cols,
            )
            .plain()
        };
        assert_eq!(
            row(60),
            "› Compaction trigger    80% of window    --config extra.toml"
        );
        assert_eq!(
            row(50),
            "› Compaction trigger    80% of window  --config e…"
        );
        assert_eq!(row(45), "› Compaction trigger    80% of window  --con…");
        assert_eq!(
            row(44),
            "› Compaction trigger    80% of window",
            "drops under six cells"
        );
        assert_eq!(row(20), "› Compaction trigge…");
    }

    #[test]
    fn right_side_survives_a_narrow_row() {
        let row = lr(
            Line::default()
                .verdigris("●")
                .ink(" bash pytest tests/test_export.py -x"),
            Line::default().dim("sandbox · 1.8s"),
            40,
        );
        assert_eq!(row.width(), 40);
        assert!(row.plain().ends_with("sandbox · 1.8s"));
        assert!(row.plain().contains('…'));
    }

    #[test]
    fn color_is_optional() {
        let line = Line::default().strong(Role::Ink, "bash").dim(" 1.8s");
        let (mut colored, mut plain) = (String::new(), String::new());
        line.write_ansi(&mut colored, true, None);
        line.write_ansi(&mut plain, false, Some([1, 2, 3]));
        assert_eq!(colored, "\x1b[1mbash\x1b[0m\x1b[90m 1.8s\x1b[0m");
        assert_eq!(plain, "bash 1.8s");
    }

    #[test]
    fn raised_rows_fill_their_column_on_the_surface_only_when_one_is_known() {
        let line = Line::default().brass("┃").ink(" hi").raise().fill(6);
        assert_eq!(line.plain(), "┃ hi  ");
        assert!(line.0.iter().all(|span| span.raised));
        let (mut lifted, mut flat) = (String::new(), String::new());
        line.write_ansi(&mut lifted, true, Some([30, 24, 22]));
        line.write_ansi(&mut flat, true, None);
        assert_eq!(
            lifted,
            "\x1b[33;48;2;30;24;22m┃\x1b[0m\x1b[48;2;30;24;22m hi\x1b[0m\x1b[48;2;30;24;22m  \x1b[0m"
        );
        assert_eq!(flat, "\x1b[33m┃\x1b[0m hi  ");
        // A cut raised row keeps its ellipsis on the surface; a base row
        // isn't padded.
        let cut = Line::default().ink("abcdef").raise().fill(4);
        assert_eq!(cut.plain(), "abc…");
        assert!(cut.0.iter().all(|span| span.raised));
        assert_eq!(Line::default().ink("ab").fill(6).plain(), "ab");
    }

    #[test]
    fn wrap_breaks_on_words_and_keeps_paragraphs_and_indentation() {
        assert_eq!(
            wrap("archive.py still registers\nunder the old key", 12),
            ["archive.py", "still", "registers", "under the", "old key"]
        );
        assert_eq!(
            wrap("  fn main() {\n\tlet answer = 42;", 12),
            ["  fn main()", "{", "    let", "answer = 42;"]
        );
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn prose_sets_inline_markdown_and_leaves_unpaired_markers() {
        let spans = |line: &Line| {
            line.0
                .iter()
                .map(|s| (s.text.clone(), s.role, s.bold))
                .collect::<Vec<_>>()
        };
        let rows = prose(
            "add `xhigh` to **the reasoning controls** now",
            14,
            Role::Ink,
        );
        assert_eq!(
            rows.iter().map(Line::plain).collect::<Vec<_>>(),
            ["add xhigh to", "the reasoning", "controls now"]
        );
        assert_eq!(
            spans(&rows[0]),
            [
                ("add ".into(), Role::Ink, false),
                ("xhigh".into(), Role::Azurite, false),
                (" to".into(), Role::Ink, false),
            ]
        );
        assert_eq!(spans(&rows[1]), [("the reasoning".into(), Role::Ink, true)]);
        assert_eq!(
            spans(&rows[2]),
            [
                ("controls".into(), Role::Ink, true),
                (" now".into(), Role::Ink, false),
            ]
        );
        assert_eq!(
            inline("a ** b, 2 * 3, ``a ` b`` and `open", Role::Dim).plain(),
            "a ** b, 2 * 3, a ` b and `open"
        );
        assert_eq!(
            inline("**Considering scope**", Role::Dim).0,
            [Span {
                text: "Considering scope".into(),
                role: Role::Dim,
                bold: true,
                underline: false,
                italic: false,
                raised: false,
            }]
        );
        let fenced = prose("```sh\nls `pwd` **x**\n```\n`y`", 40, Role::Ink);
        assert_eq!(fenced[1].plain(), "ls `pwd` **x**");
        assert_eq!(fenced[3].plain(), "y");
    }

    #[test]
    fn prose_sets_tasks_lists_and_headings_as_blocks() {
        let rows = prose(
            "## Next\n- [x] protocol done\n- [ ] tui: wire the key\n- a bullet that wraps past the width\n2. second",
            20,
            Role::Ink,
        );
        assert_eq!(
            rows.iter().map(Line::plain).collect::<Vec<_>>(),
            [
                "Next",
                "✓ protocol done",
                "○ tui: wire the key",
                "• a bullet that",
                "  wraps past the",
                "  width",
                "2. second",
            ]
        );
        assert!(rows[0].0.iter().all(|s| s.role == Role::Brass && s.bold));
        // A finished task keeps its tick and lets its text recede.
        assert_eq!(rows[1].0[0].role, Role::Verdigris);
        assert!(rows[1].0[1..].iter().all(|s| s.role == Role::Dim));
        assert_eq!(rows[2].0[0].role, Role::Dim);
        assert_eq!(rows[2].0[1].role, Role::Ink);
        assert_eq!(rows[3].0[0].role, Role::Rubric);
        // Thinking keeps its own role for the marks.
        let dim = prose("- a\n# b", 20, Role::Dim);
        assert!(dim.iter().flat_map(|l| &l.0).all(|s| s.role == Role::Dim));
    }

    #[test]
    fn tones_and_italic_have_their_own_codes() {
        let line = Line::default()
            .bright("a")
            .muted("b")
            .faint("c")
            .rubric("d")
            .extend(Line::default().dim("e").italic());
        let mut out = String::new();
        line.write_ansi(&mut out, true, None);
        assert_eq!(
            out,
            "\x1b[97ma\x1b[0m\x1b[37mb\x1b[0m\x1b[30mc\x1b[0m\x1b[91md\x1b[0m\x1b[3;90me\x1b[0m"
        );
    }
}
