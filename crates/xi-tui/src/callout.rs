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

/// Prose like [`prose`], with `>` quotes behind a dim bar and GitHub or
/// Obsidian callouts such as `> [!WARNING] Title` under a marked headline.
/// Quotes nest; lines inside a fenced code block stay literal.
#[must_use]
pub fn markdown(text: &str, cols: usize, role: Role) -> Vec<Line> {
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
                lines.extend(quote(&quoted, cols, role));
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
        lines.extend(quote(&quoted, cols, role));
    }
    lines
}

/// The text of a `>` line, less the marker and one space after it.
fn unquote(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The kind and title of a callout's first line, such as
/// `[!NOTE]` or `[!tip]- Custom title`.
fn callout(first: &str) -> Option<(Kind, String)> {
    let (name, after) = first.trim().strip_prefix("[!")?.split_once(']')?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let title = after.trim_start_matches(['+', '-']).trim();
    let title = if title.is_empty() {
        let mut chars = name.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
        })
    } else {
        title.to_owned()
    };
    Some((Kind::of_callout(name), title))
}

fn quote(inner: &[&str], cols: usize, role: Role) -> Vec<Line> {
    let (head, bar, body) = match inner.first().and_then(|first| callout(first)) {
        Some((kind, title)) => (
            kind.mark().ink(" ").strong(Role::Ink, title),
            kind.role(),
            &inner[1..],
        ),
        None => (Line::default(), Role::Dim, inner),
    };
    let rows = if body.is_empty() {
        Vec::new()
    } else {
        markdown(&body.join("\n"), Block::inner(cols), role)
    };
    Block {
        head,
        meta: Line::default(),
        bar,
        rows,
    }
    .lines(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    #[test]
    fn callouts_take_a_marked_headline_and_a_bar_in_their_kind() {
        let rows = markdown(
            "Before.\n> [!WARNING] Drops data\n> Run it on a **copy** first.\n\nAfter.",
            24,
            Role::Ink,
        );
        assert_eq!(
            plain(&rows),
            [
                "Before.",
                "! Drops data",
                "│ Run it on a copy",
                "│ first.",
                "",
                "After."
            ]
        );
        assert_eq!(rows[1].0[0].role, Role::Brass);
        assert!(rows[1].0.iter().any(|s| s.text == "Drops data" && s.bold));
        assert_eq!(rows[2].0[0].role, Role::Brass);
        assert!(rows[2].0.iter().any(|s| s.text == "copy" && s.bold));
        // A name alone becomes the title; a fold marker is dropped.
        assert_eq!(
            plain(&markdown("> [!tip]-\n> Use ctrl+o.", 40, Role::Ink)),
            ["i Tip", "│ Use ctrl+o."]
        );
        assert_eq!(
            plain(&markdown("> [!CAUTION]", 40, Role::Ink)),
            ["! Caution"]
        );
    }

    #[test]
    fn quotes_nest_behind_dim_bars_and_fences_stay_literal() {
        let rows = markdown(
            "```\n> literal\n```\n> outer\n>\n> > inner\n> [!NOTE] not first",
            40,
            Role::Ink,
        );
        assert_eq!(
            plain(&rows),
            [
                "```",
                "> literal",
                "```",
                "│ outer",
                "│",
                "│ │ inner",
                "│ [!NOTE] not first"
            ]
        );
        assert!(rows[3..].iter().all(|row| row.0[0].role == Role::Dim));
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
