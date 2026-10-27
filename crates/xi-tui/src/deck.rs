//! The desktop deck, composed without terminal I/O.
//!
//! The log keeps the sheet at the left: retained rows from the top, the
//! composer against the bottom. A side panel on the raised surface takes
//! every column beside it: the session's title, what waits on you, its
//! context, what changed and what else runs while nothing has the keys; the
//! selected log row while you walk the log; every session while you pick
//! one. Without room for the panel, a header row carries the session's facts
//! and the panel's views replace the log while they have the keys.

use unicode_width::UnicodeWidthChar;

use crate::icons::{Icon, Icons};
use crate::layout::{PANEL_MIN, SHEET};
use crate::log::Inspect;
use crate::style::{Line, Role, Span, lr, wrap};
use crate::transcript::tokens;

/// The widest the overview sets its rows, so counts stay near their names.
const LIVE_MAX: usize = 72;
/// Asks the overview lists before counting the rest.
const ASK_ROWS: usize = 3;
/// Changed files the overview lists before counting the rest.
const CHANGED_ROWS: usize = 8;
/// Sessions the overview lists: running or waiting ones first, then this
/// workspace's latest.
const SESSION_ROWS: usize = 5;
/// Of those, the most that are this workspace's idle sessions.
const RECENT_ROWS: usize = 3;
/// Blank columns on either side of the panel's text.
const PANEL_PAD: usize = 2;

/// The pane receiving keyboard input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    /// The live composer.
    #[default]
    Composer,
    /// The session list.
    Sessions,
    /// The retained log.
    Log,
    /// The inspector.
    Inspector,
}

/// A session's activity mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// The attached session.
    Current,
    /// A session with work in flight.
    Running,
    /// A session waiting for an answer.
    Waiting,
    /// An inactive session.
    Idle,
}

/// A session in the panel.
pub struct SessionRow {
    /// The session's activity.
    pub mark: Mark,
    /// The title, with the workspace already included for other workspaces.
    pub title: String,
    /// The model and age, or waiting count and age.
    pub detail: String,
}

/// An unanswered approval.
pub struct AskRow {
    /// Tool and action, such as `bash exec`.
    pub head: String,
    /// The command or other requested operation.
    pub body: String,
    /// Session title and age.
    pub origin: String,
}

/// Every top-level session the daemon lists.
pub struct SessionsPane {
    /// Sessions in the current workspace.
    pub here: Vec<SessionRow>,
    /// Sessions in other workspaces.
    pub elsewhere: Vec<SessionRow>,
    /// The selected index into `here` followed by `elsewhere`.
    pub selected: usize,
}

/// Retained output and the live dock.
pub struct LogPane<'a> {
    /// Retained rows, oldest first.
    pub rows: &'a [Line],
    /// The retained row marked for inspection.
    pub selected: Option<usize>,
    /// Whether the rows shown move to keep `selected` in view.
    pub reveal: bool,
    /// Rows scrolled back from the newest retained row.
    pub scrolled: usize,
    /// The live region, including the composer.
    pub live: &'a [Line],
    /// How many of `live`'s last rows stay against the bottom: frames, the
    /// composer and its status row. The rest follow the retained rows.
    pub dock: usize,
    /// The cursor within `live`.
    pub cursor: Option<(usize, usize)>,
}

/// Context occupancy and the cache hit rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextGauge {
    /// Tokens in the current context.
    pub used: u64,
    /// Context capacity, or zero when unknown.
    pub window: u64,
    /// The cache percentage, when known.
    pub cache_percent: Option<u64>,
}

/// What the side panel can show.
pub struct Panel<'a> {
    /// The selected call or edit run.
    pub inspect: Option<&'a Inspect>,
    /// Rows scrolled down in the inspected text.
    pub scrolled: usize,
    /// Unanswered approvals across sessions.
    pub asks: Vec<AskRow>,
    /// Changed paths and their added and removed line counts.
    pub changed: Vec<(String, usize, usize)>,
    /// Every session, for the overview's active ones and the full list.
    pub sessions: SessionsPane,
    /// Context occupancy and cache.
    pub context: ContextGauge,
    /// The workspace and branch, at the panel's foot.
    pub place: Line,
    /// Approvals, sandbox, trust, taint and running agents, under `place`.
    pub facts: Line,
    /// Which glyphs mark sessions, asks and tools.
    pub icons: Icons,
}

/// All input for one deck frame.
pub struct Scene<'a> {
    /// Application, workspace and branch, for the header of a folded deck.
    pub header_left: Line,
    /// Model, thinking level, running agents, taint and waiting asks, for
    /// the header of a folded deck.
    pub header_right: Line,
    /// Context and cache, which the folded header adds.
    pub status: Line,
    /// The attached session's title.
    pub title: &'a str,
    /// When it started, its head and event count.
    pub meta: &'a str,
    /// The attached session's output and composer.
    pub log: LogPane<'a>,
    /// The side panel.
    pub panel: Panel<'a>,
    /// The pane receiving keyboard input.
    pub focus: Focus,
    /// Whether raised surfaces draw; without them a rule parts the panel
    /// from the log.
    pub surfaces: bool,
}

/// The widths of the deck's columns, excluding the separator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    /// The log's width: a gutter cell, its rows and one cell of margin.
    pub log: usize,
    /// The side panel's width when it fits.
    pub panel: Option<usize>,
}

/// Measures the columns: the sheet and the panel beside it when at least
/// [`PANEL_MIN`] columns are left, else the log alone.
#[must_use]
pub fn columns(cols: usize) -> Columns {
    let log = SHEET + 2;
    if cols > log + PANEL_MIN {
        Columns {
            log,
            panel: Some(cols - log - 1),
        }
    } else {
        Columns {
            log: cols,
            panel: None,
        }
    }
}

/// Composes exactly `rows` width-bounded lines and an absolute cursor.
#[must_use]
pub fn render(scene: &Scene<'_>, cols: usize, rows: usize) -> (Vec<Line>, Option<(usize, usize)>) {
    if rows == 0 {
        return (Vec::new(), None);
    }
    let columns = columns(cols);
    let Some(panel) = columns.panel else {
        return folded(scene, cols, rows);
    };
    // A blank row above the log, when there is room, matches the panel's.
    let top = usize::from(rows >= 8);
    let (log, cursor) = log_rows(&scene.log, columns.log - 2, rows - top);
    let rule = if scene.surfaces {
        Line::default().ink(" ")
    } else {
        Line::default().dim("│")
    };
    let out = blank(top)
        .into_iter()
        .chain(log)
        .zip(panel_column(scene, panel, rows))
        .map(|(log, side)| {
            pad(log, columns.log)
                .extend(rule.clone())
                .extend(side)
                .truncate(cols)
        })
        .collect();
    (out, cursor.map(|(row, col)| (top + row, col + 1)))
}

/// The deck without a panel: a header row with the session's facts, the
/// title, then the log, or the panel's view while it has the keys.
fn folded(scene: &Scene<'_>, cols: usize, rows: usize) -> (Vec<Line>, Option<(usize, usize)>) {
    let mut right = scene.header_right.clone();
    if !scene.status.0.is_empty() {
        if !right.0.is_empty() {
            right = right.dim(" · ");
        }
        right = right.extend(scene.status.clone());
    }
    let mut out = Vec::with_capacity(rows);
    out.push(lr(scene.header_left.clone(), right, cols));
    let height = rows - 1;
    let inner = cols.saturating_sub(2);
    let gutter = |line: Line| Line::default().ink(" ").extend(line).truncate(cols);
    if matches!(scene.focus, Focus::Sessions | Focus::Inspector) {
        out.extend(panel_rows(scene, inner, height).into_iter().map(gutter));
        return (out, None);
    }
    let title = lr(
        Line::default().strong(Role::Ink, scene.title),
        Line::default().dim(scene.meta),
        inner,
    );
    let head: Vec<Line> = [title, Line::default()]
        .into_iter()
        .take(height.min(2))
        .collect();
    let above = 1 + head.len();
    out.extend(head.into_iter().map(gutter));
    let (log, cursor) = log_rows(&scene.log, inner, rows - above);
    out.extend(log.into_iter().map(|line| line.truncate(cols)));
    (out, cursor.map(|(row, col)| (above + row, col + 1)))
}

fn pad(line: Line, width: usize) -> Line {
    let line = line.truncate(width);
    let gap = width - line.width();
    line.ink(" ".repeat(gap))
}

fn blank(height: usize) -> Vec<Line> {
    vec![Line::default(); height]
}

/// The log's rows, each behind a gutter cell that marks the selected row:
/// retained rows from the top and the streamed live rows after them, which
/// scroll together, and the dock against the bottom. `width` excludes the
/// gutter; the cursor is relative to `width`.
fn log_rows(
    pane: &LogPane<'_>,
    width: usize,
    height: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let mut lines = blank(height);
    let docked = pane.dock.min(pane.live.len());
    let (flow, dock) = pane.live.split_at(pane.live.len() - docked);
    let dock_skip = dock.len().saturating_sub(height);
    let dock = &dock[dock_skip..];
    let room = height - dock.len();
    // Retained rows, then the streamed ones, as one list.
    let len = pane.rows.len();
    let total = len + flow.len();
    let row = |i: usize| pane.rows.get(i).unwrap_or_else(|| &flow[i - len]);
    let mut end = total.saturating_sub(pane.scrolled);
    if pane.reveal
        && let Some(selected) = pane.selected.filter(|&row| row < len)
    {
        if selected >= end {
            end = selected + 1;
        } else if selected < end.saturating_sub(room) {
            end = (selected + room).min(total);
        }
    }
    let start = end.saturating_sub(room);
    for (y, i) in (start..end).enumerate() {
        lines[y] = row(i).clone().fill(width);
    }
    for (i, line) in dock.iter().enumerate() {
        lines[room + i] = line.clone().fill(width);
    }
    let marked = pane
        .selected
        .filter(|row| (start..end).contains(row))
        .map(|row| row - start);
    let lines = lines
        .into_iter()
        .enumerate()
        .map(|(y, line)| {
            let gutter = if marked == Some(y) {
                Line::default().brass("›")
            } else {
                Line::default().ink(" ")
            };
            gutter.extend(line)
        })
        .collect();
    let cursor = pane.cursor.and_then(|(row, col)| {
        let y = match row.checked_sub(flow.len()) {
            Some(in_dock) => room + in_dock.checked_sub(dock_skip)?,
            None => (len + row).checked_sub(start).filter(|_| len + row < end)?,
        };
        (y < height && width > 0).then(|| (y, col.min(width - 1)))
    });
    (lines, cursor)
}

/// The panel `width` wide on the raised surface: a blank row above and
/// below when there is room, and [`PANEL_PAD`] columns either side.
fn panel_column(scene: &Scene<'_>, width: usize, height: usize) -> Vec<Line> {
    let edge = usize::from(height >= 8);
    let inner = width.saturating_sub(2 * PANEL_PAD);
    let margin = " ".repeat(PANEL_PAD);
    let blank = Line::default().ink(&margin).raise().fill(width);
    let body = panel_rows(scene, inner, height - 2 * edge);
    let mut out = Vec::with_capacity(height);
    out.extend(std::iter::repeat_n(blank.clone(), edge));
    out.extend(body.into_iter().map(|line| {
        Line::default()
            .ink(&margin)
            .extend(line.truncate(inner))
            .raise()
            .fill(width)
    }));
    out.extend(std::iter::repeat_n(blank, edge));
    out
}

/// Exactly `height` panel rows: the focused view, then its keys while a
/// pane has them.
fn panel_rows(scene: &Scene<'_>, width: usize, height: usize) -> Vec<Line> {
    let icons = scene.panel.icons;
    let keys = match scene.focus {
        Focus::Composer => {
            return overview(scene, width, height)
                .into_iter()
                .map(|line| line.truncate(width))
                .collect();
        }
        Focus::Log => "↑↓ select · enter prints · pgup pgdn · esc back",
        Focus::Inspector => "↑↓ scroll · alt+2 log · esc back",
        Focus::Sessions => "↑↓ move · enter resumes · esc back",
    };
    let Some(body) = height.checked_sub(1) else {
        return Vec::new();
    };
    let mut lines = match scene.focus {
        Focus::Sessions => sessions_view(&scene.panel.sessions, icons, width, body),
        _ => inspector_view(&scene.panel, width, body),
    };
    lines.push(Line::default().dim(keys));
    lines.into_iter().map(|line| line.truncate(width)).collect()
}

fn mark(mark: Mark, icons: Icons) -> Line {
    match mark {
        Mark::Current => Line::default().brass(icons.get(Icon::Current)),
        Mark::Running => Line::default().brass(icons.get(Icon::Running)),
        Mark::Waiting => Line::default().brass(icons.get(Icon::Paused)),
        Mark::Idle => Line::default().dim(icons.get(Icon::Idle)),
    }
}

/// A section heading in bold.
fn heading(title: impl AsRef<str>) -> Line {
    Line::default().strong(Role::Ink, title)
}

fn counts(added: usize, removed: usize) -> Line {
    Line::default()
        .verdigris(format!("+{added}"))
        .dim(" ")
        .sanguine(format!("−{removed}"))
}

/// `line` cut from the left to `cols` cells behind a dim `…`, keeping its
/// spans' roles, so a path keeps its file name.
fn elide_left(line: Line, cols: usize) -> Line {
    if line.width() <= cols {
        return line;
    }
    if cols == 0 {
        return Line::default();
    }
    let mut used = 1;
    let mut kept = Vec::new();
    for span in line.0.into_iter().rev() {
        let mut text = Vec::new();
        let mut whole = true;
        for c in span.text.chars().rev() {
            let w = c.width().unwrap_or(0);
            if used + w > cols {
                whole = false;
                break;
            }
            used += w;
            text.push(c);
        }
        if !text.is_empty() {
            kept.push(Span {
                text: text.into_iter().rev().collect(),
                ..span
            });
        }
        if !whole {
            break;
        }
    }
    let mut out = Line::default().dim("…");
    out.0.extend(kept.into_iter().rev());
    out
}

/// The session at a glance while the composer has the keys: its title, what
/// waits on you, its context, what changed and what else runs, with where
/// it runs and under what rules against the bottom.
fn overview(scene: &Scene<'_>, width: usize, height: usize) -> Vec<Line> {
    let panel = &scene.panel;
    let icons = panel.icons;
    let w = width.min(LIVE_MAX);
    let mut top: Vec<Line> = Vec::new();
    let title = wrap(scene.title, w.max(1));
    for (i, row) in title.iter().take(2).enumerate() {
        let text = if i == 1 && title.len() > 2 {
            title[1..].join(" ")
        } else {
            row.clone()
        };
        top.push(Line::default().strong(Role::Ink, text).truncate(w));
    }
    if !scene.meta.is_empty() {
        top.push(Line::default().dim(scene.meta).truncate(w));
    }
    let section = |top: &mut Vec<Line>, title: Line, right: Line| {
        if !top.is_empty() {
            top.push(Line::default());
        }
        top.push(if right.0.is_empty() {
            title
        } else {
            lr(title, right, w)
        });
    };
    if !panel.asks.is_empty() {
        section(
            &mut top,
            Line::default().strong(Role::Brass, format!("Waiting · {}", panel.asks.len())),
            Line::default().dim("tab answers"),
        );
        for ask in panel.asks.iter().take(ASK_ROWS) {
            top.push(
                Line::default()
                    .brass(format!("{} {}", icons.get(Icon::Ask), ask.head))
                    .ink(format!("  {}", ask.body))
                    .truncate(w),
            );
            top.push(Line::default().dim(format!("  {}", ask.origin)).truncate(w));
        }
        if panel.asks.len() > ASK_ROWS {
            top.push(Line::default().dim(format!("  … {} more", panel.asks.len() - ASK_ROWS)));
        }
    }
    section(&mut top, heading("Context"), Line::default());
    top.extend(context(panel.context));
    let (added, removed) = panel
        .changed
        .iter()
        .fold((0usize, 0usize), |(added, removed), (_, a, r)| {
            (added.saturating_add(*a), removed.saturating_add(*r))
        });
    let total = if panel.changed.is_empty() {
        Line::default()
    } else {
        counts(added, removed)
    };
    section(&mut top, heading("Changes"), total);
    if panel.changed.is_empty() {
        top.push(Line::default().dim("nothing yet"));
    }
    for (path, added, removed) in panel.changed.iter().take(CHANGED_ROWS) {
        let counts = counts(*added, *removed);
        let room = w.saturating_sub(counts.width() + 2);
        top.push(lr(elide_left(Line::default().ink(path), room), counts, w));
    }
    if panel.changed.len() > CHANGED_ROWS {
        top.push(Line::default().dim(format!(
            "… {} more files",
            panel.changed.len() - CHANGED_ROWS
        )));
    }
    let sessions = &panel.sessions;
    let others: Vec<&SessionRow> = sessions
        .here
        .iter()
        .chain(&sessions.elsewhere)
        .filter(|session| session.mark != Mark::Current)
        .collect();
    if !others.is_empty() {
        let active = others
            .iter()
            .filter(|session| matches!(session.mark, Mark::Running | Mark::Waiting))
            .take(SESSION_ROWS);
        let room = SESSION_ROWS
            .saturating_sub(active.clone().count())
            .min(RECENT_ROWS);
        let recent = sessions
            .here
            .iter()
            .filter(|session| session.mark == Mark::Idle)
            .take(room);
        let shown: Vec<&SessionRow> = active.copied().chain(recent).collect();
        section(&mut top, heading("Sessions"), Line::default().dim("alt+1"));
        for session in &shown {
            top.push(lr(
                mark(session.mark, icons).ink(format!(" {}", session.title)),
                Line::default().dim(&session.detail),
                w,
            ));
        }
        if others.len() > shown.len() {
            top.push(Line::default().dim(format!("{} more", others.len() - shown.len())));
        }
    }
    let foot: Vec<Line> = [
        elide_left(panel.place.clone(), w),
        panel.facts.clone().truncate(w),
        Line::default()
            .brass("ξ ")
            .strong(Role::Ink, "xi")
            .dim(format!(" {}", env!("CARGO_PKG_VERSION")))
            .truncate(w),
    ]
    .into_iter()
    .filter(|line| !line.0.is_empty())
    .collect();
    // The foot keeps its rows; the sections above it are cut first, with
    // a gap between the two.
    let foot = &foot[foot.len().saturating_sub(height)..];
    let room = height.saturating_sub(foot.len() + 1);
    if top.len() > room {
        top.truncate(room.saturating_sub(1));
        if room > 0 {
            top.push(Line::default().dim("…"));
        }
    }
    let mut lines = top;
    lines.resize(height - foot.len(), Line::default());
    lines.extend_from_slice(foot);
    lines
}

/// Context occupancy, then the cache hit rate when known.
fn context(context: ContextGauge) -> Vec<Line> {
    let mut rows = vec![if context.window == 0 {
        Line::default()
            .ink(tokens(context.used))
            .dim(" used · window unknown")
    } else {
        // The footer truncates; the overview agrees with it.
        let percent = u128::from(context.used) * 100 / u128::from(context.window);
        Line::default()
            .ink(tokens(context.used))
            .dim(" of ")
            .ink(tokens(context.window))
            .dim(" · ")
            .ink(format!("{percent}%"))
            .dim(" used")
    }];
    if let Some(percent) = context.cache_percent {
        rows.push(
            Line::default()
                .ink(format!("{percent}%"))
                .dim(" from cache"),
        );
    }
    rows
}

/// An inspected row's headline without its live mark, a built-in tool's
/// name replaced by its icon.
fn headline(inspect: &Inspect, icons: Icons) -> String {
    let title = inspect
        .title
        .strip_prefix("▸ ")
        .or_else(|| inspect.title.strip_prefix("● "))
        .unwrap_or(&inspect.title);
    let (name, rest) = title.split_once(' ').unwrap_or((title, ""));
    match icons.tool(name) {
        Some(glyph) => format!("{glyph} {rest}"),
        None => title.to_owned(),
    }
}

/// The selected row: its headline fixed at the top, its whole text below,
/// wrapped and scrolled.
fn inspector_view(panel: &Panel<'_>, width: usize, height: usize) -> Vec<Line> {
    let Some(inspect) = panel.inspect else {
        let mut lines = vec![Line::default().dim("No tool rows yet.")];
        lines.resize(height, Line::default());
        lines.truncate(height);
        return lines;
    };
    let mut lines = vec![lr(
        Line::default().strong(Role::Ink, headline(inspect, panel.icons)),
        Line::default().dim(&inspect.meta),
        width,
    )];
    if !inspect.note.is_empty() {
        lines.push(Line::default().dim(&inspect.note));
    }
    lines.push(Line::default());
    let body = inspected(inspect, width);
    let room = height.saturating_sub(lines.len());
    let skip = panel.scrolled.min(body.len().saturating_sub(1));
    let rest = body.len() - skip;
    if rest > room && room > 0 {
        lines.extend(body.into_iter().skip(skip).take(room - 1));
        lines.push(Line::default().dim(format!("… {} more lines", rest - (room - 1))));
    } else {
        lines.extend(body.into_iter().skip(skip));
    }
    lines.resize(height, Line::default());
    lines
}

/// Every session, this workspace first, keeping the selection in view.
fn sessions_view(pane: &SessionsPane, icons: Icons, width: usize, height: usize) -> Vec<Line> {
    let mut lines = vec![heading("Sessions"), Line::default()];
    let mut at = None;
    let mut push = |lines: &mut Vec<Line>, index: usize, session: &SessionRow| {
        let selected = index == pane.selected;
        if selected {
            at = Some(lines.len());
        }
        let lead = if selected {
            Line::default().brass("› ")
        } else {
            Line::default().ink("  ")
        };
        let title = if selected {
            Line::default().brass(format!(" {}", session.title))
        } else {
            Line::default().ink(format!(" {}", session.title))
        };
        lines.push(lr(
            lead.extend(mark(session.mark, icons)).extend(title),
            Line::default().dim(&session.detail),
            width,
        ));
    };
    for (index, session) in pane.here.iter().enumerate() {
        push(&mut lines, index, session);
    }
    if !pane.elsewhere.is_empty() {
        lines.push(Line::default());
        lines.push(Line::default().dim("Other workspaces"));
        for (index, session) in pane.elsewhere.iter().enumerate() {
            push(&mut lines, pane.here.len() + index, session);
        }
    }
    if let Some(at) = at
        && at >= height
    {
        lines.drain(..=at - height);
    }
    lines.resize(height, Line::default());
    lines
}

/// The inspected text, wrapped to `width`, with diff rows in their roles.
fn inspected(inspect: &Inspect, width: usize) -> Vec<Line> {
    let mut body = Vec::new();
    for text in inspect.text.lines() {
        let role = if inspect.diff && text.starts_with("- ") {
            Role::Sanguine
        } else if inspect.diff && text.starts_with("+ ") {
            Role::Verdigris
        } else if inspect.diff {
            Role::Dim
        } else {
            Role::Ink
        };
        for part in wrap(text, width.max(1)) {
            body.push(Line::default().span(role, false, part));
        }
    }
    if body.is_empty() {
        body.push(Line::default().dim("no output"));
    }
    body
}

/// How many rows the inspector sets `inspect`'s text in on a `cols`-wide
/// deck, which bounds how far it scrolls.
#[must_use]
pub fn inspector_rows(inspect: &Inspect, cols: usize) -> usize {
    let width = columns(cols).panel.map_or(cols.saturating_sub(2), |panel| {
        panel.saturating_sub(2 * PANEL_PAD)
    });
    inspected(inspect, width).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        retained: Vec<Line>,
        live: Vec<Line>,
        inspect: Inspect,
    }

    impl Fixture {
        fn new() -> Self {
            let width = SHEET;
            Self {
                retained: vec![
                    Line::default()
                        .brass("┃ ")
                        .ink("route sandboxed bash through the egress proxy on Linux")
                        .raise(),
                    Line::default(),
                    lr(
                        Line::default()
                            .verdigris("● ")
                            .ink("read crates/xi-sandbox/src/lib.rs:1-180"),
                        Line::default().dim("180 lines · 0.0s"),
                        width,
                    ),
                    lr(
                        Line::default()
                            .dim("▸ ")
                            .ink("edit ×5 crates/xi-tools/src/bash.rs"),
                        counts(22, 16).dim(" · 0.1s"),
                        width,
                    ),
                    lr(
                        Line::default()
                            .sanguine("✕ ")
                            .ink("bash cargo test -p xi-sandbox egress"),
                        Line::default().sanguine("exit 101").dim(" · 6.8s"),
                        width,
                    ),
                    Line::default().dim("  … 12 more lines · enter"),
                    Line::default(),
                ],
                live: vec![
                    Line::default().ink("streamed prose"),
                    Line::default().brass("┃").raise(),
                    Line::default().brass("┃").ink("  draft").raise(),
                    Line::default()
                        .brass("┃")
                        .ink("  gpt-6.1-sol")
                        .dim(" · xhigh")
                        .raise(),
                    Line::default().brass("┃").raise(),
                    Line::default().dim("enter sends"),
                ],
                inspect: Inspect {
                    title: "▸ edit ×5 crates/xi-tools/src/bash.rs".into(),
                    meta: "+22 −16".into(),
                    note: "net diff · 5 edits".into(),
                    diff: true,
                    text: [
                        "-     let mut command = spec",
                        "+     let launch_error = |error| {",
                        "  …",
                    ]
                    .join("\n"),
                },
            }
        }

        fn scene(&self) -> Scene<'_> {
            let session = |mark, title: &str, detail: &str| SessionRow {
                mark,
                title: title.into(),
                detail: detail.into(),
            };
            Scene {
                header_left: Line::default()
                    .brass("ξ ")
                    .strong(Role::Ink, "xi")
                    .ink("  ~/src/xi  main*"),
                header_right: Line::default().dim("chatgpt/gpt-6.1-sol · think xhigh"),
                status: Line::default().dim("ctx 11% · cache 94%"),
                title: "Route sandboxed bash through the egress proxy",
                meta: "14:02 · a3f9c2e1 · 41 events",
                log: LogPane {
                    rows: &self.retained,
                    selected: None,
                    reveal: false,
                    scrolled: 0,
                    live: &self.live,
                    dock: 5,
                    cursor: Some((2, 7)),
                },
                panel: Panel {
                    inspect: Some(&self.inspect),
                    scrolled: 0,
                    asks: vec![AskRow {
                        head: "bash exec".into(),
                        body: "cargo update -p tokio".into(),
                        origin: "Bump tokio to 1.48 · 12m".into(),
                    }],
                    changed: vec![
                        ("crates/xi-sandbox/src/lib.rs".into(), 21, 12),
                        ("crates/xi-tools/src/bash.rs".into(), 22, 16),
                    ],
                    sessions: SessionsPane {
                        here: vec![
                            session(Mark::Current, "Route sandboxed bash", "gpt-6.1-sol · 5m"),
                            session(Mark::Running, "Port footer fit to deck", "sonnet-5.5 · 2m"),
                            session(Mark::Waiting, "Bump tokio to 1.48", "1 waiting · 12m"),
                            session(Mark::Idle, "Settings frame hints", "yesterday"),
                        ],
                        elsewhere: vec![session(
                            Mark::Idle,
                            "ledger · Fix export offsets",
                            "2 days ago",
                        )],
                        selected: 0,
                    },
                    context: ContextGauge {
                        used: 29_800,
                        window: 272_000,
                        cache_percent: Some(94),
                    },
                    place: Line::default()
                        .dim("~/src/")
                        .ink("xi")
                        .dim(":")
                        .ink("main*"),
                    facts: Line::default().dim("ask · sandboxed · trusted"),
                    icons: Icons::Unicode,
                },
                focus: Focus::Composer,
                surfaces: true,
            }
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    /// The panel's text: what follows the log, the rule and the panel's
    /// margin.
    fn side(text: &[String]) -> Vec<String> {
        text.iter()
            .map(|line| line.chars().skip(123 + PANEL_PAD).collect::<String>())
            .collect()
    }

    #[test]
    fn the_panel_takes_every_column_beside_the_sheet_once_it_fits() {
        assert_eq!(
            columns(250),
            Columns {
                log: 122,
                panel: Some(127)
            }
        );
        assert_eq!(
            columns(159),
            Columns {
                log: 122,
                panel: Some(36)
            }
        );
        assert_eq!(
            columns(158),
            Columns {
                log: 158,
                panel: None
            }
        );
        for cols in 0..400 {
            let c = columns(cols);
            assert_eq!(c.log + c.panel.map_or(0, |panel| panel + 1), cols);
        }
    }

    #[test]
    fn the_overview_titles_the_session_and_keeps_its_foot_at_the_bottom() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        for cols in [176, 250] {
            let (lines, cursor) = render(&scene, cols, 40);
            assert_eq!(lines.len(), 40);
            assert!(lines.iter().all(|line| line.width() == cols));
            // The dock's draft row sits four rows above the status row.
            assert_eq!(cursor, Some((36, 8)));
            let side = side(&plain(&lines));
            assert_eq!(side[0].trim_end(), "");
            assert_eq!(
                side[1].trim_end(),
                "Route sandboxed bash through the egress proxy"
            );
            assert_eq!(side[2].trim_end(), "14:02 · a3f9c2e1 · 41 events");
            assert!(
                side[4].starts_with("Waiting · 1") && side[4].trim_end().ends_with("tab answers")
            );
            assert!(side[5].starts_with("? bash exec  cargo update -p tokio"));
            let at = |name: &str| side.iter().position(|line| line.starts_with(name)).unwrap();
            let context = at("Context");
            assert_eq!(side[context + 1].trim_end(), "29.8k of 272.0k · 10% used");
            assert_eq!(side[context + 2].trim_end(), "94% from cache");
            assert!(side[at("Changes")].trim_end().ends_with("+43 −28"));
            let sessions = at("Sessions");
            assert!(side[sessions].trim_end().ends_with("alt+1"));
            assert!(side[sessions + 1].starts_with("┤ Port footer fit to deck"));
            assert!(side[sessions + 2].starts_with("? Bump tokio to 1.48"));
            // This workspace's idle sessions follow; other workspaces' are
            // counted.
            assert!(side[sessions + 3].starts_with("· Settings frame hints"));
            assert!(side[sessions + 3].trim_end().ends_with("yesterday"));
            assert_eq!(side[sessions + 4].trim_end(), "1 more");
            assert!(!side.iter().any(|line| line.contains("Fix export offsets")));
            // The newest tool row stays in the log and the inspector.
            assert!(!side.iter().any(|line| line.contains("edit ×5")));
            assert_eq!(side[36].trim_end(), "~/src/xi:main*");
            assert_eq!(side[37].trim_end(), "ask · sandboxed · trusted");
            assert!(side[38].starts_with("ξ xi "));
            assert_eq!(side[39].trim_end(), "");
        }
    }

    #[test]
    fn the_panel_and_the_prompt_sit_on_the_raised_surface_and_the_log_does_not() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        let (lines, _) = render(&scene, 176, 40);
        for line in &lines {
            let (mut col, mut raised) = (0, Vec::new());
            for span in &line.0 {
                raised.extend(std::iter::repeat_n(span.raised, span.text.chars().count()));
                col += span.text.chars().count();
            }
            assert_eq!(col, 176);
            assert!(raised[123..].iter().all(|r| *r), "{}", line.plain());
            assert!(!raised[0] && !raised[122]);
        }
        let text = plain(&lines);
        let prompt = text
            .iter()
            .position(|line| line.starts_with(" ┃ route"))
            .unwrap();
        // The prompt's surface runs to the log's edge; tool rows have none.
        assert!(
            lines[prompt]
                .0
                .iter()
                .filter(|s| s.raised)
                .map(|s| s.text.chars().count())
                .sum::<usize>()
                >= 120
        );
        assert!(!lines[prompt + 2].0.iter().take(3).any(|s| s.raised));
        assert_eq!(text[0].chars().nth(122), Some(' '));
        scene.surfaces = false;
        let text = plain(&render(&scene, 176, 40).0);
        assert!(text.iter().all(|line| line.chars().nth(122) == Some('│')));
    }

    #[test]
    fn the_log_reads_from_the_top_with_the_dock_against_the_bottom() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        let (rows, cursor) = log_rows(&scene.log, 120, 20);
        let text = plain(&rows);
        assert!(text[0].starts_with(" ┃ route sandboxed bash"));
        assert!(text[4].starts_with(" ✕ bash cargo test"));
        assert_eq!(text[7].trim_end(), " streamed prose");
        assert_eq!(text[8].trim_end(), "");
        assert_eq!(text[14].trim_end(), "");
        assert_eq!(text[15].trim_end(), " ┃");
        assert_eq!(text[16].trim_end(), " ┃  draft");
        assert_eq!(text[19].trim_end(), " enter sends");
        assert_eq!(cursor, Some((16, 7)));
        // Rows fill the column only on the surface.
        assert_eq!(rows[0].width(), 121);
        assert_eq!(rows[16].width(), 121);
        assert_eq!(rows[7].width(), 15);
    }

    #[test]
    fn short_panels_keep_the_foot_and_cut_the_sections_above_it() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        let text = plain(&overview(&scene, 60, 12));
        assert_eq!(text.len(), 12);
        assert_eq!(text[7], "…");
        assert_eq!(text[8], "");
        assert_eq!(text[9], "~/src/xi:main*");
        assert!(text[11].starts_with("ξ xi"));
        assert_eq!(
            plain(&overview(&scene, 60, 2)),
            [
                "ask · sandboxed · trusted".to_owned(),
                format!("ξ xi {}", env!("CARGO_PKG_VERSION"))
            ]
        );
        assert!(overview(&scene, 60, 0).is_empty());
    }

    #[test]
    fn the_overview_says_when_nothing_changed_and_drops_lone_sessions() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.asks.clear();
        scene.panel.changed.clear();
        scene.panel.sessions.here.truncate(1);
        scene.panel.sessions.elsewhere.clear();
        scene.panel.context = ContextGauge {
            used: 2_400,
            window: 0,
            cache_percent: None,
        };
        let text = plain(&overview(&scene, 60, 20));
        assert_eq!(
            text[..9],
            [
                "Route sandboxed bash through the egress proxy",
                "14:02 · a3f9c2e1 · 41 events",
                "",
                "Context",
                "2.4k used · window unknown",
                "",
                "Changes",
                "nothing yet",
                "",
            ]
        );
        assert!(
            !text
                .iter()
                .any(|line| line.starts_with("Waiting") || line.starts_with("Sessions"))
        );
        // Long titles take two rows and cut the rest.
        scene.title = "a title that runs on well past the width of one panel row and then a second";
        let text = plain(&overview(&scene, 30, 20));
        assert_eq!(text[0], "a title that runs on well past");
        assert!(text[1].ends_with('…') && text[1].chars().count() == 30);
        assert!(text[2].starts_with("14:02"));
    }

    #[test]
    fn changed_paths_keep_their_file_names_and_long_lists_count_the_rest() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.changed = (0..11)
            .map(|n| {
                (
                    format!("crates/xi-kernel/src/deeply/nested/module_{n}.rs"),
                    n,
                    1,
                )
            })
            .collect();
        let text = plain(&overview(&scene, 40, 40));
        let first = text
            .iter()
            .position(|line| line.starts_with("Changes"))
            .unwrap();
        assert!(text[first + 1].starts_with('…'));
        assert!(text[first + 1].contains("module_0.rs"));
        assert!(text[first + 1].ends_with("+0 −1"));
        assert_eq!(text[first + 1 + CHANGED_ROWS], "… 3 more files");
        let path = |text: &str, cols| elide_left(Line::default().ink(text), cols).plain();
        assert_eq!(path("abc", 3), "abc");
        assert_eq!(path("abcdef", 4), "…def");
        assert_eq!(path("abcdef", 0), "");
        // A styled place keeps its roles past the cut.
        let place = Line::default()
            .dim("~/src/")
            .ink("xi")
            .dim(":")
            .ink("main*");
        let cut = elide_left(place, 9);
        assert_eq!(cut.plain(), "…xi:main*");
        let roles: Vec<Role> = cut.0.iter().map(|span| span.role).collect();
        assert_eq!(roles, [Role::Dim, Role::Ink, Role::Dim, Role::Ink]);
    }

    #[test]
    fn nerd_icons_mark_sessions_asks_and_tool_names_but_not_headings() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.icons = Icons::Nerd;
        let text = plain(&overview(&scene, 60, 40));
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f420} bash exec"))
        );
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f46a} Port footer"))
        );
        assert!(text.iter().any(|line| line.starts_with("Sessions")));
        assert!(text.iter().any(|line| line.starts_with("Changes")));
        let mut inspect = fixture.inspect.clone();
        inspect.title = "mcp_lookup docs".into();
        assert_eq!(headline(&inspect, Icons::Nerd), "mcp_lookup docs");
        assert_eq!(
            headline(&fixture.inspect, Icons::Nerd),
            "\u{f448} ×5 crates/xi-tools/src/bash.rs"
        );
        assert_eq!(
            headline(&fixture.inspect, Icons::Unicode),
            "edit ×5 crates/xi-tools/src/bash.rs"
        );
        scene.focus = Focus::Log;
        let text = plain(&render(&scene, 176, 36).0);
        assert!(text[34].trim_end().ends_with("esc back"));
    }

    #[test]
    fn the_selected_row_gets_a_gutter_mark_and_stays_in_view() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.focus = Focus::Log;
        scene.log.selected = Some(3);
        let (rows, _) = log_rows(&scene.log, 120, 20);
        let text = plain(&rows);
        let marked: Vec<_> = text.iter().filter(|line| line.starts_with('›')).collect();
        assert_eq!(marked, [&format!("›{}", fixture.retained[3].plain())]);
        let row = rows
            .iter()
            .find(|line| line.plain().starts_with('›'))
            .unwrap();
        assert_eq!(row.0[0].role, Role::Brass);
        assert_eq!(&row.0[1..], &fixture.retained[3].0[..]);
        // Nine rows leave four above the dock: three retained, one streamed.
        scene.log.selected = Some(0);
        assert!(
            !plain(&log_rows(&scene.log, 120, 9).0)
                .iter()
                .any(|line| line.starts_with('›'))
        );
        scene.log.reveal = true;
        let text = plain(&log_rows(&scene.log, 120, 9).0);
        assert!(text[0].starts_with("›┃ route"));
        scene.log.scrolled = 6;
        scene.log.selected = Some(5);
        let text = plain(&log_rows(&scene.log, 120, 9).0);
        assert!(text[3].starts_with("›  … 12 more lines"));
    }

    #[test]
    fn log_scrolling_keeps_live_rows_fixed_and_crops_hidden_cursors() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        let (bottom, bottom_cursor) = log_rows(&scene.log, 120, 12);
        assert!(
            plain(&bottom)
                .iter()
                .any(|line| line.contains("12 more lines"))
        );
        scene.log.scrolled = 3;
        let (scrolled, scrolled_cursor) = log_rows(&scene.log, 120, 12);
        assert!(
            !plain(&scrolled)
                .iter()
                .any(|line| line.contains("12 more lines"))
        );
        assert_eq!(bottom_cursor, scrolled_cursor);
        // Scrolling moves the streamed rows with the log, not the dock.
        assert_eq!(&bottom[7..], &scrolled[7..]);
        assert!(
            !plain(&scrolled)
                .iter()
                .any(|line| line.contains("streamed"))
        );
        // The dock keeps its last rows on a short log, and the flow goes.
        let (short, cursor) = log_rows(&scene.log, 120, 4);
        assert_eq!(short.len(), 4);
        assert_eq!(cursor, Some((0, 7)));
        assert!(!plain(&short).iter().any(|line| line.contains("streamed")));
        assert_eq!(log_rows(&scene.log, 120, 3).1, None);
        // A cursor in the streamed rows follows them, and scrolls away.
        scene.log.cursor = Some((0, 0));
        assert_eq!(log_rows(&scene.log, 120, 12).1, None);
        scene.log.scrolled = 0;
        assert_eq!(log_rows(&scene.log, 120, 12).1, Some((6, 0)));
    }

    #[test]
    fn inspector_keeps_its_headline_wraps_scrolls_and_counts_the_rest() {
        let mut fixture = Fixture::new();
        fixture.inspect.text = (0..40)
            .map(|n| format!("+ added {n} {}", "x".repeat(n % 3 * 30)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut scene = fixture.scene();
        scene.focus = Focus::Log;
        let rows = inspector_view(&scene.panel, 40, 20);
        let text = plain(&rows);
        assert_eq!(rows.len(), 20);
        assert!(text[0].starts_with("edit ×5 crates/xi-tools"));
        assert!(text[0].ends_with("+22 −16"));
        assert_eq!(text[1], "net diff · 5 edits");
        assert_eq!(text[3].trim_end(), "+ added 0");
        assert!(text[6].starts_with("xxx"), "wrapped: {text:?}");
        assert!(text[19].starts_with("… ") && text[19].ends_with(" more lines"));
        assert!(rows[6].0.iter().all(|span| span.role == Role::Verdigris));
        scene.panel.scrolled = 4;
        let scrolled = plain(&inspector_view(&scene.panel, 40, 20));
        assert_eq!(scrolled[..3], text[..3]);
        assert_eq!(scrolled[3], text[7]);
        scene.panel.scrolled = 10_000;
        let end = plain(&inspector_view(&scene.panel, 40, 20));
        assert_eq!(end[3].trim_end(), "+ added 39");
        assert_eq!(end[4], "");
        fixture.inspect.text.clear();
        let scene = fixture.scene();
        assert_eq!(plain(&inspector_view(&scene.panel, 40, 6))[3], "no output");
        // The inspector's scroll bound measures the panel's own width.
        let wide = Fixture::new();
        assert_eq!(inspector_rows(&wide.inspect, 176), 3);
    }

    #[test]
    fn diff_rows_use_existing_semantic_roles() {
        let fixture = Fixture::new();
        let rows = inspector_view(&fixture.scene().panel, 98, 20);
        for (prefix, role) in [
            ("- ", Role::Sanguine),
            ("+ ", Role::Verdigris),
            ("  …", Role::Dim),
        ] {
            let line = rows
                .iter()
                .find(|line| line.plain().starts_with(prefix))
                .unwrap();
            assert!(line.0.iter().all(|span| span.role == role));
        }
    }

    #[test]
    fn focus_picks_the_panel_view_and_folded_views_replace_the_log() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.focus = Focus::Log;
        let text = plain(&render(&scene, 200, 30).0);
        assert!(text[1].contains("   edit ×5 crates/xi-tools/src/bash.rs"));
        assert!(text[28].trim_end().ends_with("esc back"));
        scene.focus = Focus::Sessions;
        scene.panel.sessions.selected = 4;
        let (lines, cursor) = render(&scene, 200, 30);
        let text = plain(&lines);
        assert!(
            text.iter()
                .any(|line| line.contains("   › · ledger · Fix export offsets"))
        );
        assert!(text.iter().any(|line| line.contains("   Other workspaces")));
        assert_eq!(cursor, Some((26, 8)));
        let (folded, cursor) = render(&scene, 120, 30);
        let text = plain(&folded);
        assert!(text[0].ends_with("think xhigh · ctx 11% · cache 94%"));
        assert_eq!(text[1], " Sessions");
        assert!(
            !text
                .iter()
                .any(|line| line.contains("Route sandboxed bash through"))
        );
        assert_eq!(cursor, None);
        scene.focus = Focus::Composer;
        let (folded, cursor) = render(&scene, 120, 30);
        let text = plain(&folded);
        assert!(text[1].starts_with(" Route sandboxed bash"));
        assert!(text[1].ends_with("41 events"));
        assert!(text[3].starts_with(" ┃ route"));
        assert_eq!(cursor, Some((26, 8)));
    }

    #[test]
    fn the_session_list_scrolls_to_keep_the_selection() {
        let fixture = Fixture::new();
        let mut pane = fixture.scene().panel.sessions;
        pane.here.extend((0..30).map(|n| SessionRow {
            mark: Mark::Idle,
            title: format!("old {n}"),
            detail: "last week".into(),
        }));
        pane.selected = 34;
        let text = plain(&sessions_view(&pane, Icons::Unicode, 50, 10));
        assert_eq!(text.len(), 10);
        assert!(text[9].starts_with("› · ledger"));
        pane.selected = 1;
        let text = plain(&sessions_view(&pane, Icons::Unicode, 50, 10));
        assert!(text[3].starts_with("› ┤ Port footer"));
        assert!(text[3].ends_with("sonnet-5.5 · 2m"));
        // A title too long for the row is cut so its age stays readable.
        pane.here[1].title = "a title far too long to fit beside its age ".repeat(3);
        let text = plain(&sessions_view(&pane, Icons::Unicode, 50, 10));
        assert!(text[3].starts_with("› ┤ a title far") && text[3].contains('…'));
        assert!(text[3].ends_with("sonnet-5.5 · 2m"));
        assert_eq!(text[3].chars().count(), 50);
    }

    #[test]
    fn deck_bounds_hold_on_empty_short_and_zero_sized_screens() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        for surfaces in [true, false] {
            scene.surfaces = surfaces;
            for focus in [
                Focus::Composer,
                Focus::Log,
                Focus::Inspector,
                Focus::Sessions,
            ] {
                scene.focus = focus;
                for cols in [0, 1, 2, 60, 120, 158, 159, 200, 250] {
                    for rows in 0..20 {
                        let (lines, cursor) = render(&scene, cols, rows);
                        assert_eq!(lines.len(), rows);
                        assert!(lines.iter().all(|line| line.width() <= cols));
                        if let Some((row, col)) = cursor {
                            assert!(row < rows && col < cols);
                        }
                    }
                }
            }
        }
    }
}
