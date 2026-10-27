//! Pigments, spans and width-bounded lines.
//!
//! Every pigment maps to an ANSI index, so the user's terminal theme decides
//! the hue. A line never exceeds the width it was built for: anything wider is
//! cut with `…`, because a wrapped line would break the live region's row
//! count.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A colour role. Each has exactly one meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// What you read. The terminal's default foreground.
    Ink,
    /// Apparatus: metadata, connectors, hints. ANSI bright black.
    Dim,
    /// You and now, and anything waiting on you. ANSI yellow.
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
            Self::Ink => None,
            Self::Dim => Some("90"),
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
}

/// One terminal row of spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line(pub Vec<Span>);

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
            });
        }
        self
    }

    role_methods!(ink => Ink, dim => Dim, brass => Brass, verdigris => Verdigris, sanguine => Sanguine, murex => Murex);

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
            let role = line.0.last().map_or(Role::Dim, |s| s.role);
            line = line.span(role, false, "…");
        }
        line
    }

    /// Writes the line with SGR codes, or plain when `color` is off.
    pub fn write_ansi(&self, out: &mut String, color: bool) {
        for span in &self.0 {
            let mut codes = Vec::new();
            if color {
                if span.bold {
                    codes.push("1");
                }
                if span.underline {
                    codes.push("4");
                }
                codes.extend(span.role.sgr());
            }
            if codes.is_empty() {
                out.push_str(&span.text);
            } else {
                out.push_str("\x1b[");
                out.push_str(&codes.join(";"));
                out.push('m');
                out.push_str(&span.text);
                out.push_str("\x1b[0m");
            }
        }
    }
}

/// `left`, then `right` flush to column `cols`. When both don't fit, `left`
/// is cut first so the outcome on the right stays readable.
#[must_use]
pub fn lr(left: Line, right: Line, cols: usize) -> Line {
    let (lw, rw) = (left.width(), right.width());
    if lw + 1 + rw <= cols {
        return left.ink(" ".repeat(cols - lw - rw)).extend(right);
    }
    if rw + 12 <= cols {
        let left = left.truncate(cols - rw - 1);
        let gap = cols - left.width() - rw;
        return left.ink(" ".repeat(gap)).extend(right);
    }
    left.truncate(cols)
}

/// A line of `─` in dim, `cols` wide.
#[must_use]
pub fn rule(cols: usize) -> Line {
    Line::default().dim("─".repeat(cols))
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

/// Word-wraps prose like [`wrap`], in `role`, setting `**strong**` in bold
/// and `` `code` `` apart. Lines inside a fenced code block stay literal.
#[must_use]
pub fn prose(text: &str, cols: usize, role: Role) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut fenced = false;
    for raw in clean_lines(text) {
        let fence = raw.trim_start().starts_with("```");
        let (text, set) = if fenced || fence {
            let n = raw.chars().count();
            (raw, vec![Mark::Plain; n])
        } else {
            marks(&raw)
        };
        fenced ^= fence;
        let chars: Vec<char> = text.chars().collect();
        // A row is the line's characters in order, less the spaces wrapping
        // dropped, so each row character finds its mark by skipping ahead.
        let mut at = 0;
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
        line.write_ansi(&mut colored, true);
        line.write_ansi(&mut plain, false);
        assert_eq!(colored, "\x1b[1mbash\x1b[0m\x1b[90m 1.8s\x1b[0m");
        assert_eq!(plain, "bash 1.8s");
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
            }]
        );
        let fenced = prose("```sh\nls `pwd` **x**\n```\n`y`", 40, Role::Ink);
        assert_eq!(fenced[1].plain(), "ls `pwd` **x**");
        assert_eq!(fenced[3].plain(), "y");
    }
}
