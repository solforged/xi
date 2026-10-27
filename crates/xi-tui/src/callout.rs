//! Notices and callouts: text set apart from prose so it reads as what it
//! is.
//!
//! A notice is one marked paragraph, such as `✓ done · 14.3s` or
//! `✕ connection refused`. The mark sits in the gutter and the text wraps
//! under itself. A block is an optional headline over rows behind a bar:
//! a thinking block, a `>` quote, or a `> [!NOTE]` callout. Both wrap
//! rather than truncate, because they are meant to be read. Anything that
//! speaks into the transcript, such as a message from another session,
//! builds on these two shapes.

use crate::style::{Line, Role, lr, prose, wrap_line};

/// What a notice or callout says. It picks the mark and its pigment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Information: a setting changed, a hint, a note. Dim `i`.
    Info,
    /// Finished well. Verdigris `✓`.
    Done,
    /// Needs your attention, or held an action back. Brass `!`.
    Warn,
    /// Failed. Sanguine `✕`.
    Error,
}

impl Kind {
    /// The pigment of the mark and the bar.
    #[must_use]
    pub fn role(self) -> Role {
        match self {
            Self::Info => Role::Dim,
            Self::Done => Role::Verdigris,
            Self::Warn => Role::Brass,
            Self::Error => Role::Sanguine,
        }
    }

    /// The one-cell mark.
    #[must_use]
    pub fn mark(self) -> Line {
        let glyph = match self {
            Self::Info => "i",
            Self::Done => "✓",
            Self::Warn => "!",
            Self::Error => "✕",
        };
        Line::default().span(self.role(), false, glyph)
    }

    /// The kind of a GitHub or Obsidian callout name such as `NOTE`.
    fn of_callout(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "success" | "check" | "done" => Self::Done,
            "important" | "warning" | "caution" | "attention" => Self::Warn,
            "danger" | "error" | "failure" | "fail" | "missing" | "bug" => Self::Error,
            _ => Self::Info,
        }
    }
}

/// `mark` in the gutter, then `text` wrapped to `cols` with each further
/// row under the text.
#[must_use]
pub fn notice(mark: Line, text: Line, cols: usize) -> Vec<Line> {
    let lead = mark.width() + 1;
    wrap_line(&text, cols.saturating_sub(lead).max(1))
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let start = if i == 0 {
                mark.clone().ink(" ")
            } else {
                Line::default().ink(" ".repeat(lead))
            };
            start.extend(row).truncate(cols)
        })
        .collect()
}

/// Rows behind a bar, under an optional headline.
#[derive(Clone, Debug)]
pub struct Block {
    /// The headline's left: a mark and a title, such as `∴ Planning`.
    /// Empty for a plain quote, which has no headline.
    pub head: Line,
    /// The headline's right, such as `4.8s · ↓ 120`.
    pub meta: Line,
    /// The bar's pigment.
    pub bar: Role,
    /// Rows at most [`Block::inner`] wide.
    pub rows: Vec<Line>,
}

impl Block {
    /// The width rows get inside a block `cols` wide.
    #[must_use]
    pub fn inner(cols: usize) -> usize {
        cols.saturating_sub(2).max(1)
    }

    /// The headline with its meta flush right at `cols`, then each row
    /// behind the bar.
    #[must_use]
    pub fn lines(self, cols: usize) -> Vec<Line> {
        let mut lines = Vec::with_capacity(self.rows.len() + 1);
        if self.head.width() > 0 {
            lines.push(if self.meta.width() > 0 {
                lr(self.head, self.meta, cols)
            } else {
                self.head.truncate(cols)
            });
        }
        for row in self.rows {
            lines.push(if row.width() == 0 {
                Line::default().span(self.bar, false, "│")
            } else {
                Line::default()
                    .span(self.bar, false, "│ ")
                    .extend(row)
                    .truncate(cols)
            });
        }
        lines
    }
}

/// Prose like [`prose`], with `>` quotes and GitHub or Obsidian callouts
/// such as `> [!WARNING] Title` set as notes behind a `▍` bar. A note
/// outside another sits on the raised surface `cols` wide, its bar running
/// through a blank row above and below. Quotes nest; lines inside a fenced
/// code block stay literal.
#[must_use]
pub fn markdown(text: &str, cols: usize, role: Role) -> Vec<Line> {
    blocks(text, cols, role, true)
}

/// [`markdown`], with its quotes raised and padded when they are `outer`.
fn blocks(text: &str, cols: usize, role: Role, outer: bool) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    let mut quoted: Vec<&str> = Vec::new();
    let mut fenced = false;
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        let inner = if fenced { None } else { unquote(raw) };
        if let Some(inner) = inner {
            if !plain.is_empty() {
                lines.extend(prose(&plain.join("\n"), cols, role));
                plain.clear();
            }
            quoted.push(inner);
        } else {
            if !quoted.is_empty() {
                lines.extend(note(&quoted, cols, role, outer));
                quoted.clear();
            }
            fenced ^= raw.trim_start().starts_with("```");
            plain.push(raw);
        }
    }
    if !plain.is_empty() {
        lines.extend(prose(&plain.join("\n"), cols, role));
    }
    if !quoted.is_empty() {
        lines.extend(note(&quoted, cols, role, outer));
    }
    lines
}

/// The text of a `>` line, less the marker and one space after it.
fn unquote(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// A note's label, such as `warning`, its pigment, and a title when it
/// names one.
struct Label {
    name: String,
    bar: Role,
    title: Option<String>,
}

impl Label {
    fn new(name: &str, title: &str) -> Self {
        let bar = match Kind::of_callout(name) {
            Kind::Info => Role::Rubric,
            kind => kind.role(),
        };
        Self {
            name: name.to_ascii_lowercase(),
            bar,
            title: (!title.is_empty()).then(|| title.to_owned()),
        }
    }
}

/// The label of a callout's first line, such as `[!NOTE]` or
/// `[!tip]- Custom title`.
fn callout(first: &str) -> Option<Label> {
    let (name, after) = first.trim().strip_prefix("[!")?.split_once(']')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(Label::new(
        name,
        after.trim_start_matches(['+', '-']).trim(),
    ))
}

/// The label of a first line that opens with a bold callout name, such as
/// `**Note:** text`, and the text after it.
fn lead(first: &str) -> Option<(Label, &str)> {
    let (word, after) = first.trim_start().strip_prefix("**")?.split_once("**")?;
    let word = word.trim_end_matches(':');
    let known = matches!(
        word.to_ascii_lowercase().as_str(),
        "note" | "tip" | "important" | "warning" | "caution" | "danger"
    );
    known.then(|| {
        let after = after.strip_prefix(':').unwrap_or(after).trim_start();
        (Label::new(word, ""), after)
    })
}

/// A run of quoted lines as a note: a bar down its left in the label's
/// pigment, rubric for a plain quote or a note, and the label beside its
/// first row with every further row hanging under the text. A title is the
/// first row, in bold.
fn note(inner: &[&str], cols: usize, role: Role, outer: bool) -> Vec<Line> {
    let mut body: Vec<&str> = inner.to_vec();
    let label = match body.first().and_then(|first| callout(first)) {
        Some(label) => {
            body.remove(0);
            Some(label)
        }
        None => match body.first().and_then(|first| lead(first)) {
            Some((label, after)) => {
                body[0] = after;
                Some(label)
            }
            None => None,
        },
    };
    let bar = label.as_ref().map_or(Role::Rubric, |label| label.bar);
    let lead = label.as_ref().map_or_else(Line::default, |label| {
        Line::default().span(bar, false, format!("{}  ", label.name))
    });
    let width = Block::inner(cols).saturating_sub(lead.width()).max(1);
    let mut rows = Vec::new();
    if let Some(title) = label.as_ref().and_then(|label| label.title.as_deref()) {
        rows.extend(
            crate::style::wrap(title, width)
                .into_iter()
                .map(|row| Line::default().strong(Role::Ink, row)),
        );
    }
    let text = body.join("\n");
    if !text.trim().is_empty() {
        rows.extend(blocks(text.trim_matches('\n'), width, role, false));
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    let mark = || Line::default().span(bar, false, "▍ ");
    let indent = Line::default().ink(" ".repeat(lead.width()));
    let mut lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 { lead.clone() } else { indent.clone() };
            mark().extend(lead).extend(row).truncate(cols)
        })
        .collect();
    if outer {
        lines.insert(0, mark());
        lines.push(mark());
        lines = lines
            .into_iter()
            .map(|line| line.raise().fill(cols))
            .collect();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    /// The rows without the surface's trailing fill.
    fn trimmed(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.plain().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn callouts_set_their_label_beside_the_text_on_a_bar_in_their_kind() {
        let rows = markdown(
            "Before.\n> [!WARNING] Drops data\n> Run it on a **copy** first.\n\nAfter.",
            24,
            Role::Ink,
        );
        assert_eq!(
            trimmed(&rows),
            [
                "Before.",
                "▍",
                "▍ warning  Drops data",
                "▍          Run it on a",
                "▍          copy first.",
                "▍",
                "",
                "After."
            ]
        );
        // The note and a bar row either side sit on the surface, filled to
        // the width; the prose around it does not.
        assert!(
            rows[1..6]
                .iter()
                .all(|row| { row.width() == 24 && row.0.iter().all(|span| span.raised) })
        );
        assert!(!rows[0].0[0].raised && rows[6].0.is_empty());
        assert!(rows[1..6].iter().all(|row| row.0[0].role == Role::Brass));
        assert!(
            rows[2]
                .0
                .iter()
                .any(|s| s.text == "warning  " && s.role == Role::Brass)
        );
        assert!(rows[2].0.iter().any(|s| s.text == "Drops data" && s.bold));
        assert!(rows[4].0.iter().any(|s| s.text == "copy" && s.bold));
        // A name alone leaves the label; a fold marker is dropped.
        assert_eq!(
            trimmed(&markdown("> [!tip]-\n> Use ctrl+o.", 40, Role::Ink)),
            ["▍", "▍ tip  Use ctrl+o.", "▍"]
        );
        assert_eq!(
            trimmed(&markdown("> [!CAUTION]", 40, Role::Ink)),
            ["▍", "▍ caution", "▍"]
        );
        // A bold name opening a quote is a label too, and a note is rubric.
        let rows = markdown("> **Note:** keep it short", 40, Role::Ink);
        assert_eq!(
            plain(&rows[1..2]),
            [format!("{:<40}", "▍ note  keep it short")]
        );
        assert_eq!(rows[1].0[0].role, Role::Rubric);
    }

    #[test]
    fn quotes_nest_behind_rubric_bars_and_fences_stay_literal() {
        let rows = markdown(
            "```\n> literal\n```\n> outer\n>\n> > inner\n> [!NOTE] not first",
            40,
            Role::Ink,
        );
        assert_eq!(
            trimmed(&rows),
            [
                "```",
                "> literal",
                "```",
                "▍",
                "▍ outer",
                "▍",
                "▍ ▍ inner",
                "▍ [!NOTE] not first",
                "▍"
            ]
        );
        assert!(rows[3..].iter().all(|row| row.0[0].role == Role::Rubric));
        // Only the outer quote takes the bar rows around it.
        assert!(rows[6].0[1].role == Role::Rubric && rows[6].0[1].text == "▍ ");
    }

    #[test]
    fn notices_wrap_under_their_text_and_fit_their_width() {
        let rows = notice(
            Kind::Error.mark(),
            Line::default().ink("connection refused by the daemon socket"),
            20,
        );
        assert_eq!(
            plain(&rows),
            ["✕ connection refused", "  by the daemon", "  socket"]
        );
        assert_eq!(rows[0].0[0].role, Role::Sanguine);
        assert!(rows.iter().all(|row| row.width() <= 20));
    }
}
