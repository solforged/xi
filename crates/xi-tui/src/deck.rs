//! The desktop deck, composed without terminal I/O.
//!
//! The log keeps the sheet at the left with its live rows against the bottom
//! of the screen. A side panel takes every column beside it: what waits on
//! you and what changed while nothing has the keys, the selected log row
//! while you walk the log, every session while you pick one. Without room
//! for the panel, its views replace the log while they have the keys.

use unicode_width::UnicodeWidthChar;

use crate::icons::{Icon, Icons};
use crate::layout::{PANEL_MIN, SHEET};
use crate::log::Inspect;
use crate::style::{Line, Role, lr, row, wrap};
use crate::transcript::tokens;

/// The widest the live view sets its rows, so counts stay near their names.
const LIVE_MAX: usize = 72;
/// Asks the live view lists before counting the rest.
const ASK_ROWS: usize = 3;
/// Changed files the live view lists before counting the rest.
const CHANGED_ROWS: usize = 8;
/// Running or waiting sessions the live view lists.
const SESSION_ROWS: usize = 5;

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
    /// The attached session's title.
    pub title: &'a str,
    /// When it started, its head and event count.
    pub meta: &'a str,
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
    /// The cursor within `live`.
    pub cursor: Option<(usize, usize)>,
}

/// Context occupancy and cumulative token traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextGauge {
    /// Tokens in the current context.
    pub used: u64,
    /// Context capacity, or zero when unknown.
    pub window: u64,
    /// The cache percentage, when known.
    pub cache_percent: Option<u64>,
    /// Tokens sent to the model.
    pub sent: u64,
    /// Tokens produced by the model.
    pub output: u64,
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
    /// Every session, for the live view's active ones and the full list.
    pub sessions: SessionsPane,
    /// Context occupancy and traffic.
    pub context: ContextGauge,
    /// Approval mode, sandbox, trust and tool count.
    pub facts: String,
    /// Which glyphs label the panel.
    pub icons: Icons,
}

/// All input for one deck frame.
pub struct Scene<'a> {
    /// Application, workspace and branch.
    pub header_left: Line,
    /// Model, thinking level, running agents, taint and waiting asks.
    pub header_right: Line,
    /// Context, cache and sandbox, which the header carries when the panel
    /// is folded.
    pub status: Line,
    /// The attached session's output and composer.
    pub log: LogPane<'a>,
    /// The side panel.
    pub panel: Panel<'a>,
    /// The pane receiving keyboard input.
    pub focus: Focus,
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
    let mut right = scene.header_right.clone();
    if columns.panel.is_none() && !scene.status.0.is_empty() {
        if !right.0.is_empty() {
            right = right.dim(" · ");
        }
        right = right.extend(scene.status.clone());
    }
    let mut out = Vec::with_capacity(rows);
    out.push(lr(scene.header_left.clone(), right, cols));
    let height = rows - 1;
    let Some(panel) = columns.panel else {
        let inner = cols.saturating_sub(2);
        if matches!(scene.focus, Focus::Sessions | Focus::Inspector) {
            out.extend(
                panel_rows(scene, inner, height)
                    .into_iter()
                    .map(|line| Line::default().ink(" ").extend(line).truncate(cols)),
            );
            return (out, None);
        }
        let (log, cursor) = log_rows(&scene.log, inner, height);
        out.extend(log.into_iter().map(|line| line.truncate(cols)));
        return (out, cursor.map(|(row, col)| (row + 1, col + 1)));
    };
    let (log, cursor) = log_rows(&scene.log, columns.log - 2, height);
    let side = panel_rows(scene, panel.saturating_sub(2), height);
    for (log, side) in log.into_iter().zip(side) {
        out.push(
            pad(log, columns.log)
                .dim("│")
                .ink(" ")
                .extend(side)
                .truncate(cols),
        );
    }
    (out, cursor.map(|(row, col)| (row + 1, col + 1)))
}

fn pad(line: Line, width: usize) -> Line {
    let line = line.truncate(width);
    let gap = width - line.width();
    line.ink(" ".repeat(gap))
}

fn blank(height: usize) -> Vec<Line> {
    vec![Line::default(); height]
}

/// The log's rows, each behind a gutter cell that marks the selected row.
/// `width` excludes the gutter; the cursor is relative to `width`.
fn log_rows(
    pane: &LogPane<'_>,
    width: usize,
    height: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let mut lines = blank(height);
    if let Some(line) = lines.first_mut() {
        *line = lr(
            Line::default().strong(Role::Ink, pane.title),
            Line::default().dim(pane.meta),
            width,
        );
    }
    let top = height.min(2);
    let live_skip = pane.live.len().saturating_sub(height - top);
    let live = &pane.live[live_skip..];
    let live_top = height - live.len();
    let room = live_top - top;
    let len = pane.rows.len();
    let mut end = len.saturating_sub(pane.scrolled);
    if pane.reveal
        && let Some(selected) = pane.selected.filter(|&row| row < len)
    {
        if selected >= end {
            end = selected + 1;
        } else if selected < end.saturating_sub(room) {
            end = (selected + room).min(len);
        }
    }
    let start = end.saturating_sub(room);
    let retained_top = live_top - (end - start);
    for (index, line) in pane.rows[start..end].iter().enumerate() {
        lines[retained_top + index] = line.clone().truncate(width);
    }
    for (index, line) in live.iter().enumerate() {
        lines[live_top + index] = line.clone().truncate(width);
    }
    let marked = pane
        .selected
        .filter(|row| (start..end).contains(row))
        .map(|row| retained_top + row - start);
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
        let row = row.checked_sub(live_skip)?;
        (row < live.len() && width > 0).then(|| (live_top + row, col.min(width - 1)))
    });
    (lines, cursor)
}

/// Exactly `height` panel rows: the focused view, then its keys.
fn panel_rows(scene: &Scene<'_>, width: usize, height: usize) -> Vec<Line> {
    let Some(body) = height.checked_sub(1) else {
        return Vec::new();
    };
    let icons = scene.panel.icons;
    let keys = match (scene.focus, icons) {
        (Focus::Composer, Icons::Nerd) => Line::default()
            .extend(icons.label(Icon::Sessions, Line::default().dim("alt+1")))
            .dim("  ")
            .extend(icons.label(Icon::Log, Line::default().dim("alt+2")))
            .dim("  ")
            .extend(icons.label(Icon::Inspector, Line::default().dim("alt+3"))),
        (Focus::Composer, Icons::Unicode) => {
            Line::default().dim("alt+1 sessions · alt+2 log · alt+3 inspector")
        }
        (Focus::Log, _) => Line::default().dim("↑↓ select · enter prints · pgup pgdn · esc back"),
        (Focus::Inspector, _) => Line::default().dim("↑↓ scroll · alt+2 log · esc back"),
        (Focus::Sessions, _) => Line::default().dim("↑↓ move · enter resumes · esc back"),
    };
    let mut lines = match scene.focus {
        Focus::Composer => live_view(&scene.panel, width, body),
        Focus::Log | Focus::Inspector => inspector_view(&scene.panel, width, body),
        Focus::Sessions => sessions_view(&scene.panel.sessions, icons, width, body),
    };
    lines.push(keys);
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

/// A section heading: its icon, then its title in bold.
fn heading(icons: Icons, icon: Icon, title: impl AsRef<str>) -> Line {
    let glyph = icons.get(icon);
    let line = Line::default().strong(Role::Ink, title);
    if icons == Icons::Nerd {
        Line::default().dim(glyph).ink(" ").extend(line)
    } else {
        line
    }
}

fn counts(added: usize, removed: usize) -> Line {
    Line::default()
        .verdigris(format!("+{added}"))
        .dim(" ")
        .sanguine(format!("−{removed}"))
}

/// `text` cut from the left to `cols` cells, so a path keeps its file name.
fn elide_left(text: &str, cols: usize) -> String {
    let width: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if width <= cols {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > cols {
            break;
        }
        used += w;
        kept.push(c);
    }
    let mut out = String::from(if cols > 0 { "…" } else { "" });
    out.extend(kept.into_iter().rev());
    out
}

/// What waits on you and what changed, with the context gauge and the
/// session's facts against the bottom.
fn live_view(panel: &Panel<'_>, width: usize, height: usize) -> Vec<Line> {
    let icons = panel.icons;
    let w = width.min(LIVE_MAX);
    let mut top: Vec<Line> = Vec::new();
    let section = |top: &mut Vec<Line>, title: Line, right: Line| {
        if !top.is_empty() {
            top.push(Line::default());
        }
        top.push(lr(title, right, w));
    };
    if !panel.asks.is_empty() {
        let mut title = heading(
            icons,
            Icon::Waiting,
            format!("Waiting · {}", panel.asks.len()),
        );
        if let Some(bell) = title.0.first_mut().filter(|_| icons == Icons::Nerd) {
            bell.role = Role::Brass;
        }
        section(&mut top, title, Line::default().dim("tab answers"));
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
    if !panel.changed.is_empty() {
        let (added, removed) = panel
            .changed
            .iter()
            .fold((0usize, 0usize), |(added, removed), (_, a, r)| {
                (added.saturating_add(*a), removed.saturating_add(*r))
            });
        let files = if panel.changed.len() == 1 {
            "file"
        } else {
            "files"
        };
        section(
            &mut top,
            heading(
                icons,
                Icon::Changes,
                format!("Changes · {} {files}", panel.changed.len()),
            ),
            counts(added, removed),
        );
        for (path, added, removed) in panel.changed.iter().take(CHANGED_ROWS) {
            let counts = counts(*added, *removed);
            let room = w.saturating_sub(counts.width() + 2);
            top.push(lr(Line::default().ink(elide_left(path, room)), counts, w));
        }
        if panel.changed.len() > CHANGED_ROWS {
            top.push(Line::default().dim(format!(
                "… {} more files",
                panel.changed.len() - CHANGED_ROWS
            )));
        }
    }
    let sessions = &panel.sessions;
    let others: Vec<&SessionRow> = sessions
        .here
        .iter()
        .chain(&sessions.elsewhere)
        .filter(|session| session.mark != Mark::Current)
        .collect();
    if !others.is_empty() {
        let active: Vec<&&SessionRow> = others
            .iter()
            .filter(|session| matches!(session.mark, Mark::Running | Mark::Waiting))
            .collect();
        let idle = others.len() - active.len();
        let mut right = Vec::new();
        if !active.is_empty() {
            right.push(format!("{} active", active.len()));
        }
        if idle > 0 {
            right.push(format!("{idle} idle"));
        }
        section(
            &mut top,
            heading(icons, Icon::Sessions, "Sessions"),
            Line::default().dim(right.join(" · ")),
        );
        for session in active.iter().take(SESSION_ROWS) {
            top.push(row(
                mark(session.mark, icons).ink(format!(" {}", session.title)),
                Line::default().dim(&session.detail),
                w,
            ));
        }
        if active.len() > SESSION_ROWS {
            top.push(Line::default().dim(format!("… {} more active", active.len() - SESSION_ROWS)));
        }
    }
    if top.is_empty() && panel.inspect.is_none() {
        top.push(
            Line::default()
                .dim("Nothing waits on you and nothing has changed yet.")
                .truncate(w),
        );
    }
    let bottom = gauge(panel, w);
    let room = height.saturating_sub(bottom.len() + 1);
    if top.len() > room {
        top.truncate(room.saturating_sub(1));
        if room > 0 {
            top.push(Line::default().dim("…"));
        }
    }
    let mut lines = top;
    // The newest tool row fills what the sections leave, at the panel's
    // whole width: its output past the log's few rows, or its whole diff.
    let gap = usize::from(!lines.is_empty());
    let spare = room.saturating_sub(lines.len() + gap);
    if let Some(inspect) = panel.inspect
        && spare >= 3
    {
        if gap > 0 {
            lines.push(Line::default());
        }
        lines.push(lr(
            Line::default().strong(Role::Ink, headline(inspect, icons)),
            Line::default().dim(&inspect.meta),
            width,
        ));
        let body = inspected(inspect, width);
        let shown = spare - 1;
        if body.len() > shown {
            lines.extend(body.iter().take(shown - 1).cloned());
            lines.push(
                Line::default().dim(format!("… {} more lines · alt+3", body.len() - (shown - 1))),
            );
        } else {
            lines.extend(body);
        }
    }
    lines.resize(height.saturating_sub(bottom.len()), Line::default());
    lines.extend(bottom);
    lines.truncate(height);
    lines.into_iter().map(|line| line.truncate(width)).collect()
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

/// Context occupancy, traffic and the session's facts.
fn gauge(panel: &Panel<'_>, w: usize) -> Vec<Line> {
    let icons = panel.icons;
    let context = panel.context;
    let mut lines = Vec::new();
    if context.window > 0 {
        // The footer truncates; the gauge agrees with it.
        let percent = u128::from(context.used) * 100 / u128::from(context.window);
        let label = icons.label(Icon::Context, Line::default().ink(format!("{percent}%")));
        let of = format!("{} of {}", tokens(context.used), tokens(context.window));
        let bar = w.saturating_sub(label.width() + 2 + 2 + of.chars().count());
        let filled = usize::try_from((percent * bar as u128 / 100).min(bar as u128)).unwrap_or(bar);
        lines.push(
            label
                .ink("  ")
                .brass("━".repeat(filled))
                .dim("━".repeat(bar - filled))
                .dim(format!("  {of}")),
        );
    } else {
        lines.push(
            icons
                .label(Icon::Context, Line::default().ink("unknown"))
                .dim(format!(" · {} used", tokens(context.used))),
        );
    }
    let mut traffic = Line::default();
    if let Some(percent) = context.cache_percent {
        traffic = icons
            .label(Icon::Cache, Line::default().ink(format!("{percent}%")))
            .dim(" · ");
    }
    lines.push(
        traffic
            .extend(icons.label(Icon::Sent, Line::default().ink(tokens(context.sent))))
            .ink(" ")
            .extend(icons.label(Icon::Received, Line::default().ink(tokens(context.output)))),
    );
    lines.extend(
        wrap(&panel.facts, w.max(1))
            .into_iter()
            .map(|text| Line::default().dim(text)),
    );
    lines
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
    let mut lines = vec![heading(icons, Icon::Sessions, "Sessions"), Line::default()];
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
        lines.push(row(
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
    let width = columns(cols).panel.unwrap_or(cols).saturating_sub(2);
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
                        .brass("ξ ")
                        .ink("route sandboxed bash through the egress proxy on Linux"),
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
                    Line::default().dim("└ working · 5m13s · esc stop"),
                    Line::default().brass(format!("╭{}╮", "─".repeat(width - 2))),
                    pad(
                        Line::default()
                            .brass("│ ")
                            .ink("ξ enter steers · alt+enter queues · esc stops"),
                        width - 1,
                    )
                    .brass("│"),
                    Line::default().brass(format!("╰{}╯", "─".repeat(width - 2))),
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
                status: Line::default().dim("ctx 11% · cache 94% · ◇ sandbox"),
                log: LogPane {
                    title: "Route sandboxed bash through the egress proxy",
                    meta: "14:02 · head #a3f9 · 41 events",
                    rows: &self.retained,
                    selected: None,
                    reveal: false,
                    scrolled: 0,
                    live: &self.live,
                    cursor: Some((2, 4)),
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
                        sent: 2_300_000,
                        output: 5_500,
                    },
                    facts: "approvals ask · ◇ sandbox · trusted · 9 tools".into(),
                    icons: Icons::Unicode,
                },
                focus: Focus::Composer,
            }
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
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
    fn live_view_lists_what_waits_changed_and_runs_above_the_gauge() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        for cols in [176, 250] {
            let (lines, cursor) = render(&scene, cols, 36);
            assert_eq!(lines.len(), 36);
            assert!(lines.iter().all(|line| line.width() <= cols));
            assert_eq!(cursor, Some((34, 5)));
            let text = plain(&lines);
            assert!(text[0].ends_with("think xhigh"));
            assert!(text[1].starts_with(" Route sandboxed bash"));
            let side: Vec<String> = text
                .iter()
                .skip(1)
                .map(|line| line.chars().skip(124).collect::<String>())
                .collect();
            assert!(side[0].starts_with("Waiting · 1") && side[0].ends_with("tab answers"));
            assert!(side[1].starts_with("? bash exec  cargo update -p tokio"));
            assert!(
                side.iter()
                    .any(|line| line.starts_with("Changes · 2 files") && line.ends_with("+43 −28"))
            );
            assert!(
                side.iter()
                    .any(|line| line.starts_with("┤ Port footer fit to deck"))
            );
            assert!(
                side.iter()
                    .any(|line| line.starts_with("? Bump tokio to 1.48"))
            );
            assert!(
                !side
                    .iter()
                    .any(|line| line.contains("Settings frame hints"))
            );
            assert!(side.iter().any(
                |line| line.starts_with("Sessions") && line.ends_with("2 active · 2 idle")
            ));
            let latest = side
                .iter()
                .position(|line| line.starts_with("edit ×5 crates/xi-tools/src/bash.rs"))
                .expect("the newest row fills the spare rows");
            assert!(side[latest].ends_with("+22 −16"));
            assert_eq!(side[latest + 1].trim_end(), "-     let mut command = spec");
            assert!(side[31].starts_with("ctx 10%  ━"));
            assert!(side[31].ends_with("29.8k of 272.0k"));
            assert!(side[31].chars().count() <= LIVE_MAX);
            assert_eq!(side[32], "cache 94% · ↑ 2.3M ↓ 5.5k");
            assert_eq!(side[33], "approvals ask · ◇ sandbox · trusted · 9 tools");
            assert_eq!(side[34], "alt+1 sessions · alt+2 log · alt+3 inspector");
        }
    }

    #[test]
    fn live_view_hides_empty_sections_and_says_so_when_all_are() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.asks.clear();
        scene.panel.sessions.here.truncate(1);
        scene.panel.sessions.elsewhere.clear();
        let text = plain(&live_view(&scene.panel, 60, 20));
        assert_eq!(
            text[0],
            "Changes · 2 files                                    +43 −28"
        );
        assert!(
            !text
                .iter()
                .any(|line| line.starts_with("Waiting") || line.starts_with("Sessions"))
        );
        scene.panel.changed.clear();
        let text = plain(&live_view(&scene.panel, 60, 20));
        assert!(text[0].starts_with("edit ×5 crates/xi-tools/src/bash.rs"));
        scene.panel.inspect = None;
        let text = plain(&live_view(&scene.panel, 60, 20));
        assert_eq!(text[0], "Nothing waits on you and nothing has changed yet.");
        scene.panel.sessions.here[0].mark = Mark::Idle;
        scene.panel.sessions.here.push(SessionRow {
            mark: Mark::Current,
            title: "this".into(),
            detail: "now".into(),
        });
        let text = plain(&live_view(&scene.panel, 60, 20));
        assert!(text[0].starts_with("Sessions") && text[0].ends_with("1 idle"));
        assert_eq!(text[1], "");
    }

    #[test]
    fn nerd_icons_replace_labels_and_tool_names_but_keep_values_and_widths() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.icons = Icons::Nerd;
        let text = plain(&live_view(&scene.panel, 60, 30));
        // Bell, diff and discussion octicons head the sections.
        assert_eq!(text[0].chars().next(), Some('\u{f49a}'));
        assert!(text[0].starts_with("\u{f49a} Waiting · 1"));
        assert!(text[1].starts_with("\u{f420} bash exec"));
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f440} Changes · 2 files"))
        );
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f442} Sessions"))
        );
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f46a} Port footer"))
        );
        // The edit run's name becomes the pencil; other words stay.
        assert!(
            text.iter()
                .any(|line| line.starts_with("\u{f448} ×5 crates/xi-tools"))
        );
        assert!(text[27].starts_with("\u{f463} 10%  ━"));
        assert_eq!(text[28], "\u{f49b} 94% · \u{f431} 2.3M \u{f433} 5.5k");
        assert!(
            text.iter()
                .all(|line| !line.contains("ctx") && !line.contains("cache"))
        );
        let mut inspect = fixture.inspect.clone();
        inspect.title = "mcp_lookup docs".into();
        assert_eq!(headline(&inspect, Icons::Nerd), "mcp_lookup docs");
        assert_eq!(
            headline(&fixture.inspect, Icons::Unicode),
            "edit ×5 crates/xi-tools/src/bash.rs"
        );
        let (lines, _) = render(&scene, 176, 36);
        assert!(lines.iter().all(|line| line.width() <= 176));
        assert!(plain(&lines)[35].ends_with("\u{f442} alt+1  \u{f451} alt+2  \u{f441} alt+3"));
    }

    #[test]
    fn the_newest_row_takes_only_spare_rows_and_counts_what_it_cuts() {
        let mut fixture = Fixture::new();
        fixture.inspect.text = (0..30)
            .map(|n| format!("+ row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let scene = fixture.scene();
        // Waiting (3), Changes (3) and Sessions (3) with their gaps take 11
        // of the 20 body rows above the gauge's gap and three rows.
        let text = plain(&live_view(&scene.panel, 60, 20));
        assert!(text[12].starts_with("edit ×5"));
        assert_eq!(text[13], "+ row 0");
        assert_eq!(text[15], "… 28 more lines · alt+3");
        assert_eq!(text[16], "");
        assert!(text[17].starts_with("ctx 10%"));
        let text = plain(&live_view(&scene.panel, 60, 17));
        assert!(!text.iter().any(|line| line.starts_with("edit ×5")));
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
        let text = plain(&live_view(&scene.panel, 40, 34));
        let first = text
            .iter()
            .position(|line| line.starts_with("Changes · 11 files"))
            .unwrap();
        assert!(text[first + 1].starts_with('…'));
        assert!(text[first + 1].contains("module_0.rs"));
        assert!(text[first + 1].ends_with("+0 −1"));
        assert_eq!(text[first + 1 + CHANGED_ROWS], "… 3 more files");
        assert_eq!(elide_left("abc", 3), "abc");
        assert_eq!(elide_left("abcdef", 4), "…def");
        assert_eq!(elide_left("abcdef", 0), "");
    }

    #[test]
    fn short_panels_keep_the_gauge_and_cut_the_sections_above_it() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        let text = plain(&live_view(&scene.panel, 60, 9));
        assert_eq!(text.len(), 9);
        assert_eq!(text[4], "…");
        assert_eq!(text[5], "");
        assert!(text[6].starts_with("ctx 10%"));
        assert_eq!(text[8], "approvals ask · ◇ sandbox · trusted · 9 tools");
        assert_eq!(live_view(&scene.panel, 60, 2).len(), 2);
        assert!(live_view(&scene.panel, 60, 0).is_empty());
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
        // Eight rows leave room for two retained rows above the live ones.
        scene.log.selected = Some(0);
        assert!(
            !plain(&log_rows(&scene.log, 120, 8).0)
                .iter()
                .any(|line| line.starts_with('›'))
        );
        scene.log.reveal = true;
        let text = plain(&log_rows(&scene.log, 120, 8).0);
        assert!(text[2].starts_with("›ξ route"));
        scene.log.scrolled = 6;
        scene.log.selected = Some(5);
        let text = plain(&log_rows(&scene.log, 120, 8).0);
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
        assert_eq!(&bottom[8..], &scrolled[8..]);
        let (short, cursor) = log_rows(&scene.log, 120, 4);
        assert_eq!(short.len(), 4);
        assert_eq!(cursor, Some((2, 4)));
        scene.log.cursor = Some((0, 0));
        assert_eq!(log_rows(&scene.log, 120, 3).1, None);
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
        assert!(text[1].contains("│ edit ×5 crates/xi-tools/src/bash.rs"));
        assert!(text[29].ends_with("esc back"));
        scene.focus = Focus::Sessions;
        scene.panel.sessions.selected = 4;
        let (lines, cursor) = render(&scene, 200, 30);
        let text = plain(&lines);
        assert!(
            text.iter()
                .any(|line| line.contains("│ › · ledger · Fix export offsets"))
        );
        assert!(text.iter().any(|line| line.contains("│ Other workspaces")));
        assert_eq!(cursor, Some((28, 5)));
        let (folded, cursor) = render(&scene, 120, 30);
        let text = plain(&folded);
        assert!(text[0].ends_with("think xhigh · ctx 11% · cache 94% · ◇ sandbox"));
        assert_eq!(text[1], " Sessions");
        assert!(
            !text
                .iter()
                .any(|line| line.contains("Route sandboxed bash through"))
        );
        assert_eq!(cursor, None);
        scene.focus = Focus::Composer;
        let (folded, cursor) = render(&scene, 120, 30);
        assert!(plain(&folded)[1].starts_with(" Route sandboxed bash"));
        assert_eq!(cursor, Some((28, 5)));
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
    }

    #[test]
    fn deck_bounds_hold_on_empty_short_and_zero_sized_screens() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
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
