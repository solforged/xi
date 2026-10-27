//! Frames: the one box every overlay draws in.
//!
//! A frame lives only in the live region, so it redraws at the new width on
//! every resize and never reaches scrollback. It has three slots: the title
//! at top left, info such as a position or count at top right, and key hints
//! in the bottom border, fitted by priority like the status line. Between
//! the borders sit head rows that never scroll, a row of facts, a list that
//! scrolls around its selection, and a foot note that wraps.
//!
//! Drawing is a pure function from a size to lines, so every frame is
//! testable as text.

use crate::callout::{Kind, notice};
use crate::keys::{Action, Keymap};
use crate::status::{Form, shrink};
use crate::style::{Line, Role};

/// Columns below which the side borders drop. The top and bottom rules
/// stay, so the frame still reads as one.
const SIDES: usize = 48;

/// Rows the list keeps before the foot note and facts give way.
const MIN_LIST: usize = 3;

/// The border's colour and what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Brass: the frame holds the keys.
    Focus,
    /// Murex: the frame acts on a tainted branch.
    Taint,
    /// Dim: the frame is shown without the keys.
    Idle,
}

impl Tone {
    fn role(self) -> Role {
        match self {
            Self::Focus => Role::Brass,
            Self::Taint => Role::Murex,
            Self::Idle => Role::Dim,
        }
    }
}

/// One key hint in the bottom border.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hint {
    /// Full form, such as `enter selects`.
    pub text: String,
    /// Shorter form used before the hint drops, such as `enter`.
    pub short: Option<String>,
    /// 0 never drops; the highest number shortens and drops first.
    pub priority: u8,
}

impl Hint {
    /// A hint without a short form.
    #[must_use]
    pub fn new(text: impl Into<String>, priority: u8) -> Self {
        Self {
            text: text.into(),
            short: None,
            priority,
        }
    }

    /// Adds a shorter form.
    #[must_use]
    pub fn short(mut self, short: impl Into<String>) -> Self {
        self.short = Some(short.into());
        self
    }

    /// `KEY verb` for an action's first binding, shortening to the key
    /// alone. `None` when nothing is bound to it.
    #[must_use]
    pub fn key(keys: &Keymap, action: Action, verb: &str, priority: u8) -> Option<Self> {
        keys.key(action)
            .map(|key| Self::new(format!("{key} {verb}"), priority).short(key))
    }

    /// `↑↓ move` from the bindings of up and down.
    #[must_use]
    pub fn moves(keys: &Keymap, priority: u8) -> Option<Self> {
        let (up, down) = (keys.key(Action::Up)?, keys.key(Action::Down)?);
        let keys = if up.chars().count() == 1 && down.chars().count() == 1 {
            format!("{up}{down}")
        } else {
            format!("{up}/{down}")
        };
        Some(Self::new(format!("{keys} move"), priority))
    }
}

/// The frame's border and slots.
#[derive(Clone, Debug)]
pub struct Frame {
    /// Border colour.
    pub tone: Tone,
    /// Top left, breadcrumb included. Cut last.
    pub title: Line,
    /// Top right. Drops before the title shortens.
    pub info: Option<Line>,
    /// Bottom border, fitted by priority.
    pub hints: Vec<Hint>,
}

/// A note under the list: a mark in the gutter, then text that wraps under
/// itself instead of truncating, because it explains a consequence or
/// reports a result.
#[derive(Clone, Debug)]
pub struct Note {
    /// A kind's mark, or another such as murex `◆`.
    pub mark: Line,
    /// The note's text.
    pub text: String,
}

impl Note {
    /// A note marked by its kind.
    #[must_use]
    pub fn new(kind: Kind, text: impl Into<String>) -> Self {
        Self::marked(kind.mark(), text)
    }

    /// A note after another mark.
    #[must_use]
    pub fn marked(mark: Line, text: impl Into<String>) -> Self {
        Self {
            mark,
            text: text.into(),
        }
    }
}

/// Where the cursor sits in a body, by row and column within that row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    /// In a head row, such as a filter.
    Head(usize, usize),
    /// In a list row, such as an inline number field.
    List(usize, usize),
}

/// What an overlay puts inside its frame. Rows are drawn at most the inner
/// width; [`inner`] gives it for a terminal width.
#[derive(Clone, Debug, Default)]
pub struct Body {
    /// Pinned above the list: a filter or the subject of an ask.
    pub head: Vec<Line>,
    /// Dim labels and ink values in one row that wraps when narrow.
    pub facts: Vec<(String, Line)>,
    /// The rows that scroll.
    pub list: Vec<Line>,
    /// The list row kept in view.
    pub selected: Option<usize>,
    /// Notes under the list.
    pub foot: Vec<Note>,
    /// The cursor, when the overlay has an input.
    pub cursor: Option<Cursor>,
}

/// The width rows get inside a frame `cols` wide.
#[must_use]
pub fn inner(cols: usize) -> usize {
    if cols >= SIDES {
        cols.saturating_sub(4)
    } else {
        cols.saturating_sub(2)
    }
}

/// Hints joined by dim ` · `, shortened and dropped by priority until they
/// fit `cols`, then cut with `…` if even the keepers don't.
#[must_use]
pub fn hints(hints: &[Hint], cols: usize) -> Line {
    let items: Vec<_> = hints
        .iter()
        .map(|h| (h.priority, h.short.is_some()))
        .collect();
    let join = |forms: &[Form]| {
        let mut line = Line::default();
        let shown = hints
            .iter()
            .zip(forms)
            .filter(|(_, form)| **form != Form::Dropped);
        for (i, (hint, form)) in shown.enumerate() {
            if i > 0 {
                line = line.dim(" · ");
            }
            let text = match (&hint.short, form) {
                (Some(short), Form::Short) => short,
                _ => &hint.text,
            };
            line = line.ink(text);
        }
        line
    };
    let (forms, _) = shrink(&items, |forms| join(forms).width() <= cols);
    join(&forms).truncate(cols)
}

fn facts(facts: &[(String, Line)], inner: usize) -> Vec<Line> {
    let mut rows = Vec::new();
    let mut row = Line::default();
    for (label, value) in facts {
        let item = Line::default().dim(label).ink(" ").extend(value.clone());
        if row.width() > 0 && row.width() + 3 + item.width() > inner {
            rows.push(std::mem::take(&mut row));
        }
        if row.width() > 0 {
            row = row.ink("   ");
        }
        row = row.extend(item);
    }
    if row.width() > 0 {
        rows.push(row);
    }
    rows
}

fn foot(notes: &[Note], inner: usize) -> Vec<Line> {
    notes
        .iter()
        .flat_map(|note| notice(note.mark.clone(), Line::default().ink(&note.text), inner))
        .collect()
}

/// Rows the body groups take, with one blank row between groups.
fn size(groups: &[usize]) -> usize {
    let present = groups.iter().filter(|n| **n > 0).count();
    groups.iter().sum::<usize>() + present.saturating_sub(1)
}

/// Draws a frame `cols` wide and at most `max_rows` tall, and returns its
/// lines and the cursor as a row and column within them.
///
/// The frame is as tall as its content. When rows run short the list
/// shrinks to three, then the foot note goes, then the facts, then the list
/// shrinks to one. `▲` and `▼` take the right border where rows are hidden.
#[must_use]
pub fn draw(
    frame: &Frame,
    body: Body,
    cols: usize,
    max_rows: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    if max_rows < 2 || cols < 8 {
        return (Vec::new(), None);
    }
    let sides = cols >= SIDES;
    let inner = inner(cols);
    let role = frame.tone.role();
    let b = |s: &str| Line::default().span(role, false, s);
    let (left, right) = if sides {
        ("╭─", "─╮")
    } else {
        ("──", "──")
    };

    let mut title = frame.title.clone();
    let info = frame
        .info
        .clone()
        .filter(|info| title.width() + info.width() + 9 <= cols);
    if title.width() + 7 > cols {
        title = title.truncate(cols - 7);
    }
    let info_width = info.as_ref().map_or(0, |info| info.width() + 2);
    let fill = cols.saturating_sub(6 + title.width() + info_width).max(1);
    let mut top = b(left)
        .ink(" ")
        .extend(title)
        .ink(" ")
        .extend(b(&"─".repeat(fill)));
    if let Some(info) = info {
        top = top.ink(" ").extend(info).ink(" ");
    }
    let top = top.extend(b(right)).truncate(cols);

    let head = body.head;
    let mut facts = facts(&body.facts, inner);
    let mut foot = foot(&body.foot, inner);
    let list = body.list;
    let cap = max_rows - 2;
    let mut n = list.len();
    let fits = |facts: &[Line], foot: &[Line], n: usize| {
        size(&[head.len() + facts.len(), n, foot.len()]) <= cap
    };
    while !fits(&facts, &foot, n) && n > MIN_LIST.min(list.len()) {
        n -= 1;
    }
    if !fits(&facts, &foot, n) {
        foot.clear();
    }
    if !fits(&facts, &foot, n) {
        facts.clear();
    }
    while !fits(&facts, &foot, n) && n > 1 {
        n -= 1;
    }
    let selected = body.selected.unwrap_or(0);
    let start = selected
        .saturating_sub(n.saturating_sub(1) / 2)
        .min(list.len() - n);
    let below = list.len() - start - n;

    let mut rows: Vec<(Line, Option<&str>)> = Vec::new();
    let pinned = head.len() + facts.len();
    rows.extend(head.into_iter().chain(facts).map(|row| (row, None)));
    if pinned > 0 && n > 0 {
        rows.push((Line::default(), None));
    }
    let list_top = rows.len();
    for (i, row) in list.into_iter().skip(start).take(n).enumerate() {
        let mark = if i == 0 && start > 0 {
            Some("▲")
        } else if i + 1 == n && below > 0 {
            Some("▼")
        } else {
            None
        };
        rows.push((row, mark));
    }
    if (pinned > 0 || n > 0) && !foot.is_empty() {
        rows.push((Line::default(), None));
    }
    rows.extend(foot.into_iter().map(|row| (row, None)));
    rows.truncate(cap);

    let mut lines = vec![top];
    for (row, mark) in rows {
        let row = row.truncate(inner);
        let pad = " ".repeat(inner - row.width());
        lines.push(if sides {
            b("│ ")
                .extend(row)
                .ink(pad)
                .ink(" ")
                .extend(b(mark.unwrap_or("│")))
        } else {
            let end = mark.map_or_else(|| Line::default().ink(" "), b);
            Line::default().ink(" ").extend(row).ink(pad).extend(end)
        });
    }
    let fitted = hints(&frame.hints, cols - 6);
    lines.push(if fitted.width() == 0 {
        let (l, r) = if sides {
            ("╰", "╯")
        } else {
            ("─", "─")
        };
        b(&format!("{l}{}{r}", "─".repeat(cols - 2)))
    } else {
        let (l, r) = if sides {
            ("╰─", "─╯")
        } else {
            ("──", "──")
        };
        let fill = cols - 6 - fitted.width();
        b(l).ink(" ")
            .extend(fitted)
            .ink(" ")
            .extend(b(&"─".repeat(fill)))
            .extend(b(r))
    });

    let offset = if sides { 2 } else { 1 };
    let cursor = body.cursor.and_then(|cursor| {
        let row = match cursor {
            Cursor::Head(row, _) if row < pinned => 1 + row,
            Cursor::List(row, _) if (start..start + n).contains(&row) => 1 + list_top + row - start,
            _ => return None,
        };
        let (Cursor::Head(_, col) | Cursor::List(_, col)) = cursor;
        (row < lines.len() - 1).then_some((row, (offset + col).min(cols - 1)))
    });
    (lines, cursor)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::keys::Binding;
    use crate::style::lr;

    fn approval(cols: usize) -> (Frame, Body) {
        let frame = Frame {
            tone: Tone::Focus,
            title: Line::default().brass("?").ink(" approval").dim(" · bash"),
            info: Some(Line::default().dim("1 of 2 waiting")),
            hints: vec![
                Hint::new("1–4 choose", 0).short("1–4"),
                Hint::new("↑↓ move", 3),
                Hint::new("tab next ask", 2).short("tab next"),
                Hint::new("esc later", 0),
            ],
        };
        let body = Body {
            head: vec![
                Line::default().strong(Role::Ink, "exec cargo publish --dry-run --allow-dirty"),
            ],
            facts: vec![
                ("tool".into(), Line::default().ink("bash")),
                ("rule".into(), Line::default().ink("none matched")),
                ("workspace".into(), Line::default().ink("trusted")),
            ],
            list: vec![
                Line::default().brass("› 1 Allow once"),
                Line::default().ink("  2 Allow for this session"),
                lr(
                    Line::default().ink("  3 Always allow in this workspace"),
                    Line::default().dim("writes .xi/config.local.toml"),
                    inner(cols),
                ),
                Line::default().ink("  4 Deny"),
            ],
            selected: Some(0),
            foot: vec![Note::new(
                Kind::Info,
                "Allowed once, the next publish asks again. The answer lands in the log, so any other client sees it.",
            )],
            cursor: None,
        };
        (frame, body)
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    /// Snapshots match the design study's reference `frame()` at the same
    /// sizes, so the Rust frame keeps the drawn contract.
    #[test]
    fn approval_snapshots_at_40_60_92_and_140_columns() {
        let draw_at = |cols: usize| {
            let (frame, body) = approval(cols);
            let (lines, cursor) = draw(&frame, body, cols, 99);
            assert!(cursor.is_none());
            assert!(lines.iter().all(|l| l.width() == cols), "width {cols}");
            plain(&lines)
        };
        assert_eq!(
            draw_at(40),
            [
                "── ? approval · bash ─ 1 of 2 waiting ──",
                " exec cargo publish --dry-run --allow-… ",
                " tool bash   rule none matched          ",
                " workspace trusted                      ",
                "                                        ",
                " › 1 Allow once                         ",
                "   2 Allow for this session             ",
                "   3 Always allow in this workspace     ",
                "   4 Deny                               ",
                "                                        ",
                " ◦ Allowed once, the next publish asks  ",
                "   again. The answer lands in the log,  ",
                "   so any other client sees it.         ",
                "── 1–4 choose · tab next · esc later ───",
            ]
        );
        assert_eq!(
            draw_at(60),
            [
                "╭─ ? approval · bash ───────────────────── 1 of 2 waiting ─╮",
                "│ exec cargo publish --dry-run --allow-dirty               │",
                "│ tool bash   rule none matched   workspace trusted        │",
                "│                                                          │",
                "│ › 1 Allow once                                           │",
                "│   2 Allow for this session                               │",
                "│   3 Always allow in this …  writes .xi/config.local.toml │",
                "│   4 Deny                                                 │",
                "│                                                          │",
                "│ ◦ Allowed once, the next publish asks again. The answer  │",
                "│   lands in the log, so any other client sees it.         │",
                "╰─ 1–4 choose · ↑↓ move · tab next ask · esc later ────────╯",
            ]
        );
        assert_eq!(
            draw_at(92),
            [
                "╭─ ? approval · bash ───────────────────────────────────────────────────── 1 of 2 waiting ─╮",
                "│ exec cargo publish --dry-run --allow-dirty                                               │",
                "│ tool bash   rule none matched   workspace trusted                                        │",
                "│                                                                                          │",
                "│ › 1 Allow once                                                                           │",
                "│   2 Allow for this session                                                               │",
                "│   3 Always allow in this workspace                          writes .xi/config.local.toml │",
                "│   4 Deny                                                                                 │",
                "│                                                                                          │",
                "│ ◦ Allowed once, the next publish asks again. The answer lands in the log, so any other   │",
                "│   client sees it.                                                                        │",
                "╰─ 1–4 choose · ↑↓ move · tab next ask · esc later ────────────────────────────────────────╯",
            ]
        );
        let wide = draw_at(140);
        assert_eq!(wide.len(), 11);
        assert_eq!(
            wide[0],
            format!("╭─ ? approval · bash {} 1 of 2 waiting ─╮", "─".repeat(101))
        );
        assert_eq!(
            wide[9],
            format!(
                "│ ◦ Allowed once, the next publish asks again. The answer lands in the log, so any other client sees it.{} │",
                " ".repeat(34)
            )
        );
        assert_eq!(
            wide[10],
            format!(
                "╰─ 1–4 choose · ↑↓ move · tab next ask · esc later {}─╯",
                "─".repeat(87)
            )
        );
    }

    #[test]
    fn short_frames_drop_the_foot_then_facts_and_mark_hidden_rows() {
        let (frame, body) = approval(60);
        let (lines, _) = draw(&frame, body, 60, 7);
        assert_eq!(
            plain(&lines),
            [
                "╭─ ? approval · bash ───────────────────── 1 of 2 waiting ─╮",
                "│ exec cargo publish --dry-run --allow-dirty               │",
                "│                                                          │",
                "│ › 1 Allow once                                           │",
                "│   2 Allow for this session                               │",
                "│   3 Always allow in this …  writes .xi/config.local.toml ▼",
                "╰─ 1–4 choose · ↑↓ move · tab next ask · esc later ────────╯",
            ]
        );
        for rows in 2..20 {
            let (frame, body) = approval(60);
            let (lines, _) = draw(&frame, body, 60, rows);
            assert!(lines.len() <= rows, "{rows} rows");
            assert!(
                rows < 5 || lines.iter().any(|l| l.plain().contains("› 1 Allow once")),
                "{rows} rows keep the selection"
            );
        }
    }

    fn models(cols: usize, cursor: Option<Cursor>) -> (Vec<String>, Option<(usize, usize)>) {
        let frame = Frame {
            tone: Tone::Focus,
            title: Line::default().ink("models"),
            info: Some(Line::default().dim("12 models")),
            hints: vec![
                Hint::new("enter selects", 0).short("enter"),
                Hint::new("type to filter", 2).short("type"),
                Hint::new("esc closes", 0).short("esc"),
            ],
        };
        let list = (0..12)
            .map(|i| Line::default().ink(format!("{}model-{i}", if i == 6 { "› " } else { "  " })))
            .collect();
        let body = Body {
            head: vec![Line::default().dim("filter ")],
            list,
            selected: Some(6),
            cursor,
            ..Body::default()
        };
        let (lines, cursor) = draw(&frame, body, cols, 9);
        (plain(&lines), cursor)
    }

    #[test]
    fn lists_scroll_around_the_selection_with_marks_in_place_of_the_border() {
        assert_eq!(
            models(40, None).0,
            [
                "── models ───────────────── 12 models ──",
                " filter                                 ",
                "                                        ",
                "   model-4                             ▲",
                "   model-5                              ",
                " › model-6                              ",
                "   model-7                              ",
                "   model-8                             ▼",
                "── enter selects · type · esc closes ───",
            ]
        );
        assert_eq!(
            models(60, None).0,
            [
                "╭─ models ───────────────────────────────────── 12 models ─╮",
                "│ filter                                                   │",
                "│                                                          │",
                "│   model-4                                                ▲",
                "│   model-5                                                │",
                "│ › model-6                                                │",
                "│   model-7                                                │",
                "│   model-8                                                ▼",
                "╰─ enter selects · type to filter · esc closes ────────────╯",
            ]
        );
    }

    #[test]
    fn the_cursor_maps_from_head_or_visible_list_rows_into_the_frame() {
        assert_eq!(models(60, Some(Cursor::Head(0, 7))).1, Some((1, 9)));
        assert_eq!(models(40, Some(Cursor::Head(0, 7))).1, Some((1, 8)));
        assert_eq!(models(60, Some(Cursor::List(6, 3))).1, Some((5, 5)));
        assert_eq!(models(60, Some(Cursor::List(0, 3))).1, None, "scrolled out");
    }

    #[test]
    fn the_title_outlasts_info_and_choosing_and_closing_outlast_other_hints() {
        let frame = Frame {
            tone: Tone::Taint,
            title: Line::default().ink("a rather long settings title"),
            info: Some(Line::default().dim("~/.config/xi/config.toml")),
            hints: vec![
                Hint::new("enter changes", 0).short("enter"),
                Hint::new("ctrl+s saves default", 2).short("ctrl+s"),
                Hint::new("e edits file", 3),
                Hint::new("esc closes", 0).short("esc"),
            ],
        };
        for cols in 8..=140 {
            let (lines, _) = draw(&frame, Body::default(), cols, 10);
            assert!(lines.iter().all(|l| l.width() == cols), "width {cols}");
            let top = lines[0].plain();
            let bottom = lines[1].plain();
            if top.contains("config.toml") {
                assert!(top.contains("a rather long settings title"), "{top}");
            }
            if cols >= 30 {
                assert!(
                    bottom.contains("enter") && bottom.contains("esc"),
                    "{bottom}"
                );
            }
            if bottom.contains("e edits file") {
                assert!(bottom.contains("ctrl+s saves default"), "{bottom}");
            }
        }
        assert!(draw(&frame, Body::default(), 7, 10).0.is_empty());
        assert!(draw(&frame, Body::default(), 60, 1).0.is_empty());
        let (lines, _) = draw(&frame, Body::default(), 60, 2);
        assert_eq!(lines[0].0[0].role, Role::Murex);
    }

    #[test]
    fn hints_read_their_keys_from_the_keymap() {
        let mut keys = Keymap::default();
        assert_eq!(
            Hint::key(&keys, Action::Send, "selects", 0),
            Some(Hint::new("enter selects", 0).short("enter"))
        );
        assert_eq!(Hint::moves(&keys, 3), Some(Hint::new("↑↓ move", 3)));
        keys.bind(Action::Send, vec![Binding::parse("ctrl+y").unwrap()]);
        keys.bind(
            Action::Up,
            vec![Binding {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
            }],
        );
        assert_eq!(
            Hint::key(&keys, Action::Send, "selects", 0).unwrap().text,
            "ctrl+y selects"
        );
        assert_eq!(Hint::moves(&keys, 3).unwrap().text, "ctrl+p/↓ move");
        keys.bind(Action::Escape, Vec::new());
        assert_eq!(Hint::key(&keys, Action::Escape, "closes", 0), None);
    }

    #[test]
    fn seven_rows_keep_the_title_five_body_rows_and_hints() {
        let frame = Frame {
            tone: Tone::Focus,
            title: Line::default().ink("settings"),
            info: None,
            hints: vec![Hint::new("enter changes", 0), Hint::new("esc closes", 0)],
        };
        let body = Body {
            list: (0..10)
                .map(|i| Line::default().ink(format!("row {i}")))
                .collect(),
            selected: Some(4),
            ..Body::default()
        };
        let (lines, _) = draw(&frame, body, 120, 7);
        assert_eq!(lines.len(), 7);
        assert!(lines[0].plain().contains("settings"));
        assert!(lines[1..6].iter().all(|line| line.plain().contains("row ")));
        assert!(lines[6].plain().contains("enter changes"));
        assert!(lines[6].plain().contains("esc closes"));
    }
}
