//! The desktop deck, composed without terminal I/O.
//!
//! The log keeps the sheet at the left: retained rows from the top, the
//! composer against the bottom. A side panel on the raised surface takes
//! at most [`PANEL_MAX`] columns against the right edge, and the log takes
//! every column left of it: the session's title, what waits on you, its
//! context, what changed and its running agents while nothing has the keys; the
//! selected log row while you walk the log; every session while you pick
//! one. Without room for the panel, a header row carries the session's facts
//! and the panel's views replace the log while they have the keys.

use unicode_width::UnicodeWidthChar;

use crate::icons::{Icon, Icons};
use crate::layout::{PANEL_MAX, PANEL_MIN, SHEET};
use crate::log::Inspect;
use crate::style::{Line, Role, Span, lr, wrap};
use crate::transcript::{counted, tokens};

/// The widest the overview sets its rows, so counts stay near their names.
const LIVE_MAX: usize = 72;
/// Asks the overview lists before counting the rest.
const ASK_ROWS: usize = 3;
/// Rows the overview gives changed files and their directories before
/// counting the rest.
const CHANGED_ROWS: usize = 10;
/// Agents the overview lists before counting the rest.
const AGENT_ROWS: usize = 4;
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

/// A subagent of the call in flight.
pub struct AgentRow {
    /// A spinner while it works, `●` once done, `✕` once failed.
    pub mark: Line,
    /// What it was asked, from its first prompt.
    pub title: String,
    /// Its open call, `thinking`, its answer's first line or its failure.
    pub doing: Line,
    /// Its time so far.
    pub detail: String,
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
    /// Subagents of the call in flight.
    pub agents: Vec<AgentRow>,
    /// Every session, for the sessions view.
    pub sessions: SessionsPane,
    /// Context occupancy and cache.
    pub context: ContextGauge,
    /// The workspace and branch, on the shared baseline under the log.
    pub place: Line,
    /// The turn's spinner, time and tokens while one runs, else empty; the
    /// top row of the panel's foot.
    pub activity: Line,
    /// The model and reasoning level, under the activity.
    pub model: Line,
    /// Approvals, sandbox, trust, taint and running agents, at the panel's foot.
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
    /// When it started.
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

/// Measures the columns. Once at least [`PANEL_MIN`] columns remain beside
/// the sheet, a side panel sits against the right edge, no wider than
/// [`PANEL_MAX`], and the log takes the rest. Otherwise the log takes the
/// terminal.
#[must_use]
pub fn columns(cols: usize) -> Columns {
    let sheet = SHEET + 2;
    let room = cols.saturating_sub(sheet + 1);
    if room >= PANEL_MIN {
        let panel = room.min(PANEL_MAX);
        Columns {
            log: cols - panel - 1,
            panel: Some(panel),
        }
    } else {
        Columns {
            log: cols,
            panel: None,
        }
    }
}

/// Rows available to the log and its dock, excluding deck chrome.
pub(crate) fn log_height(cols: usize, rows: usize) -> usize {
    if columns(cols).panel.is_some() {
        let body = rows.saturating_sub(1);
        body.saturating_sub(usize::from(body >= 8))
    } else {
        rows.saturating_sub(3)
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
    // Both panes end above one unraised baseline: the workspace under the
    // log and the sessions key under the panel.
    let body = rows - 1;
    // A blank row above the log, when there is room, matches the panel's.
    let top = usize::from(body >= 8);
    let (log, cursor) = log_rows(&scene.log, columns.log - 2, log_height(cols, rows));
    let rule = if scene.surfaces {
        Line::default().ink(" ")
    } else {
        Line::default().dim("│")
    };
    let mut out: Vec<Line> = blank(top)
        .into_iter()
        .chain(log)
        .zip(panel_column(scene, panel, body))
        .map(|(log, side)| {
            // The panel is the last columns; pad only if a row came up short.
            pad(
                pad(log, columns.log).extend(rule.clone()).extend(side),
                cols,
            )
        })
        .collect();
    // The workspace starts in the log's first column, as its rows do.
    let place = Line::default().ink(" ").extend(elide_left(
        scene.panel.place.clone(),
        columns.log.saturating_sub(2),
    ));
    let keys = Line::default().muted("alt+1").dim(" sessions");
    let side = Line::default().ink(" ".repeat(PANEL_PAD)).extend(keys);
    out.push(pad(pad(place, columns.log).ink(" ").extend(side), cols));
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
        out.extend(
            panel_rows(scene, inner, height, false)
                .into_iter()
                .map(gutter),
        );
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
    let (log, cursor) = log_rows(&scene.log, inner, log_height(cols, rows));
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
    let body = panel_rows(scene, inner, height - 2 * edge, true);
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
/// pane has them. Beside the log, the panel's foot stays at the bottom
/// under every view.
fn panel_rows(scene: &Scene<'_>, width: usize, height: usize, beside: bool) -> Vec<Line> {
    let icons = scene.panel.icons;
    let keys: &[&str] = match scene.focus {
        Focus::Composer => {
            return overview(scene, width, height)
                .into_iter()
                .map(|line| line.truncate(width))
                .collect();
        }
        Focus::Log => &["↑↓ select", "enter prints", "pgup pgdn", "esc back"],
        Focus::Inspector => &["↑↓ scroll", "alt+2 log", "esc back"],
        Focus::Sessions => &["↑↓ move", "enter resumes", "esc back"],
    };
    let keys = hint_rows(keys, width);
    let foot = if beside {
        foot(&scene.panel, width.min(LIVE_MAX))
    } else {
        Vec::new()
    };
    let gap = usize::from(!foot.is_empty());
    let body = height.saturating_sub(keys.len() + gap + foot.len());
    let mut lines = match scene.focus {
        Focus::Sessions => sessions_view(&scene.panel.sessions, icons, width, body),
        _ => inspector_view(&scene.panel, width, body),
    };
    lines.extend(keys);
    if gap > 0 {
        lines.push(Line::default());
    }
    lines.extend(foot);
    lines.drain(..lines.len().saturating_sub(height));
    lines.into_iter().map(|line| line.truncate(width)).collect()
}

/// Key hints joined by ` · `, wrapping between hints so a narrow panel
/// keeps every one.
fn hint_rows(hints: &[&str], width: usize) -> Vec<Line> {
    use unicode_width::UnicodeWidthStr;
    let mut rows: Vec<String> = Vec::new();
    for hint in hints {
        match rows.last_mut() {
            Some(row) if row.width() + 3 + hint.width() <= width => {
                row.push_str(" · ");
                row.push_str(hint);
            }
            _ => rows.push((*hint).to_owned()),
        }
    }
    rows.into_iter()
        .map(|row| Line::default().dim(row))
        .collect()
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
/// waits on you, its context, what changed and its running agents, with
/// the turn's activity, the model and the rules it runs under against the
/// bottom.
fn overview(scene: &Scene<'_>, width: usize, height: usize) -> Vec<Line> {
    let panel = &scene.panel;
    let icons = panel.icons;
    let w = width.min(LIVE_MAX);
    let mut top: Vec<Line> = Vec::new();
    let mut title = wrap(scene.title, w.max(1));
    // A title on two rows splits at the narrowest width that still fits
    // it there, so the second row isn't one stranded word.
    if title.len() == 2
        && let Some(even) = (w / 2..w)
            .map(|cols| wrap(scene.title, cols))
            .find(|rows| rows.len() == 2)
    {
        title = even;
    }
    for (i, row) in title.iter().take(2).enumerate() {
        let text = if i == 1 && title.len() > 2 {
            title[1..].join(" ")
        } else {
            row.clone()
        };
        top.push(Line::default().strong(Role::Bright, text).truncate(w));
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
            Line::default().brass(format!("waiting · {}", panel.asks.len())),
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
    let percent = (panel.context.window > 0)
        .then(|| u128::from(panel.context.used) * 100 / u128::from(panel.context.window));
    section(
        &mut top,
        label("context"),
        percent.map_or_else(Line::default, |p| Line::default().muted(format!("{p}%"))),
    );
    top.extend(context(panel.context, w));
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
    section(&mut top, label("changes"), total);
    if panel.changed.is_empty() {
        top.push(Line::default().dim("nothing yet"));
    }
    top.extend(changes(&panel.changed, w));
    if !panel.agents.is_empty() {
        section(
            &mut top,
            label("agents"),
            Line::default().dim(counted(panel.agents.len(), "agent", "agents")),
        );
        for agent in panel.agents.iter().take(AGENT_ROWS) {
            top.push(lr(
                agent.mark.clone().ink(" ").ink(&agent.title),
                Line::default().dim(&agent.detail),
                w,
            ));
            top.push(
                Line::default()
                    .ink("  ")
                    .extend(agent.doing.clone())
                    .truncate(w),
            );
        }
        if panel.agents.len() > AGENT_ROWS {
            top.push(Line::default().dim(format!("… {} more", panel.agents.len() - AGENT_ROWS)));
        }
    }
    let foot = foot(panel, w);
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

/// The panel's foot: the turn's activity while one runs, the model, then
/// the rules, which wrap between items so unusual ones stay whole.
fn foot(panel: &Panel<'_>, width: usize) -> Vec<Line> {
    [panel.activity.clone(), panel.model.clone()]
        .into_iter()
        .chain(wrap_items(&panel.facts, width))
        .map(|line| line.truncate(width))
        .filter(|line| !line.plain().trim().is_empty())
        .collect()
}

/// `line` broken into rows at its ` · ` separators, each as wide as fits
/// `width`.
fn wrap_items(line: &Line, width: usize) -> Vec<Line> {
    let mut items = vec![Line::default()];
    for span in &line.0 {
        if span.text == " · " {
            items.push(Line::default());
        } else if let Some(item) = items.last_mut() {
            item.0.push(span.clone());
        }
    }
    let mut rows: Vec<Line> = Vec::new();
    for item in items.into_iter().filter(|item| !item.0.is_empty()) {
        match rows.last_mut() {
            Some(row) if row.width() + 3 + item.width() <= width => {
                *row = std::mem::take(row).dim(" · ").extend(item);
            }
            _ => rows.push(item),
        }
    }
    rows
}

/// A changed path's file, or the path itself, with its added and removed
/// line counts.
type Changed = (String, usize, usize);

/// A section's name, in muted bold so it reads apart from its rows and
/// from failures.
fn label(name: &str) -> Line {
    Line::default().strong(Role::Muted, name)
}

/// Changed files under their directories, in the order each directory was
/// first touched, without the directories every path shares.
fn changes(changed: &[(String, usize, usize)], w: usize) -> Vec<Line> {
    let split = |path: &str| -> (String, String) {
        match path.rsplit_once('/') {
            Some((dir, file)) => (dir.to_owned(), file.to_owned()),
            None => (String::new(), path.to_owned()),
        }
    };
    let dirs: Vec<String> = changed.iter().map(|(path, ..)| split(path).0).collect();
    // Whole directories every changed path starts with, such as `crates/`.
    let shared = dirs.first().map_or(0, |first| {
        let mut shared = 0;
        for (at, _) in first.match_indices('/') {
            let prefix = &first[..=at];
            if dirs.iter().all(|dir| format!("{dir}/").starts_with(prefix)) {
                shared = at + 1;
            }
        }
        // Only directories that differ are worth cutting down to; one
        // directory, or a path in the shared one itself, keeps them whole.
        if dirs.iter().any(|dir| dir.len() <= shared) || dirs.iter().all(|dir| dir == first) {
            0
        } else {
            shared
        }
    });
    // Each directory with its files and their counts.
    let mut groups: Vec<(String, Vec<Changed>)> = Vec::new();
    for (path, added, removed) in changed {
        let (dir, file) = split(path);
        let dir = dir.get(shared..).unwrap_or(&dir).to_owned();
        match groups.iter_mut().find(|(d, _)| *d == dir) {
            Some((_, files)) => files.push((file, *added, *removed)),
            None => groups.push((dir, vec![(file, *added, *removed)])),
        }
    }
    let mut lines = Vec::new();
    let mut shown = 0;
    'groups: for (dir, files) in &groups {
        if !dir.is_empty() {
            if lines.len() + 2 > CHANGED_ROWS {
                break;
            }
            lines.push(elide_left(Line::default().dim(dir), w));
        }
        let indent = if dir.is_empty() { "" } else { "  " };
        for (file, added, removed) in files {
            if lines.len() + 1 > CHANGED_ROWS {
                break 'groups;
            }
            let counts = counts(*added, *removed);
            let room = w.saturating_sub(counts.width() + 2 + indent.len());
            lines.push(lr(
                Line::default()
                    .ink(indent)
                    .extend(elide_left(Line::default().ink(file), room)),
                counts,
                w,
            ));
            shown += 1;
        }
    }
    if changed.len() > shown {
        let hidden = changed.len() - shown;
        let noun = if hidden == 1 { "file" } else { "files" };
        lines.push(Line::default().dim(format!("… {hidden} more {noun}")));
    }
    lines
}

/// Context occupancy as a gauge, then the numbers and the cache hit rate.
fn context(context: ContextGauge, w: usize) -> Vec<Line> {
    if context.window == 0 {
        let mut line = Line::default()
            .muted(tokens(context.used))
            .dim(" used · window unknown");
        if let Some(percent) = context.cache_percent {
            line = line.dim(" · ").muted(format!("{percent}%")).dim(" cached");
        }
        return vec![line];
    }
    // The footer truncates; the overview agrees with it. The percentage
    // sits on the section's row, so the bar spans the panel's text column.
    let bar = w;
    let filled = usize::try_from(
        (u128::from(context.used) * bar as u128).div_ceil(u128::from(context.window)),
    )
    .unwrap_or(bar)
    .min(bar);
    let gauge = Line::default()
        .brass("━".repeat(filled))
        .faint("━".repeat(bar - filled));
    let mut numbers = Line::default()
        .muted(tokens(context.used))
        .dim(" of ")
        .muted(tokens(context.window));
    if let Some(percent) = context.cache_percent {
        numbers = numbers
            .dim(" · ")
            .muted(format!("{percent}%"))
            .dim(" cached");
    }
    vec![gauge, numbers]
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
    let head = Line::default().strong(Role::Ink, headline(inspect, panel.icons));
    let meta = Line::default().dim(&inspect.meta);
    // A headline too long to share its row puts the meta under it.
    let mut lines = if meta.0.is_empty() || head.width() + 2 + meta.width() <= width {
        vec![lr(head, meta, width)]
    } else {
        vec![head.truncate(width), meta]
    };
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
        let left = lead.extend(mark(session.mark, icons)).extend(title);
        let detail = Line::default().dim(&session.detail);
        if left.width() + 2 + detail.width() <= width {
            lines.push(lr(left, detail, width));
        } else {
            // A title too long to share its row puts the detail under it.
            lines.push(left.truncate(width));
            lines.push(Line::default().ink("    ").extend(detail).truncate(width));
        }
        if selected {
            at = Some(lines.len() - 1);
        }
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
                        Line::default().dim("180 lines"),
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
                meta: "started 14:02",
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
                    agents: vec![AgentRow {
                        mark: Line::default().brass("┤"),
                        title: "Audit the proxy's allow list".into(),
                        doing: Line::default()
                            .muted("read")
                            .dim(" crates/xi-sandbox/src/net.rs"),
                        detail: "12.0s".into(),
                    }],
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
                        .dim("  ")
                        .ink("main*"),
                    activity: Line::default(),
                    model: Line::default().dim("chatgpt/").ink("gpt-6").dim(" · high"),
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
    fn side(text: &[String], cols: usize) -> Vec<String> {
        let skip = columns(cols).log + 1 + PANEL_PAD;
        text.iter()
            .map(|line| line.chars().skip(skip).collect::<String>())
            .collect()
    }

    #[test]
    fn the_panel_stays_at_its_max_against_the_right_edge() {
        assert_eq!(
            columns(250),
            Columns {
                log: 250 - 1 - PANEL_MAX,
                panel: Some(PANEL_MAX)
            }
        );
        assert_eq!(
            columns(SHEET + 2 + 1 + PANEL_MAX),
            Columns {
                log: SHEET + 2,
                panel: Some(PANEL_MAX)
            }
        );
        // Between the minimum and the maximum the panel still takes the room
        // and the log stays on the sheet.
        assert_eq!(
            columns(163),
            Columns {
                log: 122,
                panel: Some(40)
            }
        );
        assert_eq!(
            columns(159),
            Columns {
                log: 122,
                panel: Some(PANEL_MIN)
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
            if let Some(panel) = c.panel {
                assert!(c.log >= SHEET + 2);
                assert!((PANEL_MIN..=PANEL_MAX).contains(&panel), "{cols}");
                assert_eq!(panel, (cols - (SHEET + 2) - 1).min(PANEL_MAX));
                assert_eq!(c.log, cols - panel - 1);
            } else {
                assert_eq!(c.log, cols);
            }
        }
    }

    #[test]
    fn a_wide_terminal_keeps_the_panel_against_the_right_edge() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        let cols = 250;
        let (lines, _) = render(&scene, cols, 40);
        let measured = columns(cols);
        let start = measured.log + 1;
        assert_eq!(measured.panel, Some(PANEL_MAX));
        assert_eq!(start + PANEL_MAX, cols);
        assert!(lines.iter().all(|line| line.width() == cols));
        for line in &lines[..lines.len() - 1] {
            let mut raised = Vec::new();
            for span in &line.0 {
                raised.extend(std::iter::repeat_n(span.raised, span.text.chars().count()));
            }
            assert_eq!(raised.len(), cols, "{}", line.plain());
            assert!(raised[start..].iter().all(|r| *r), "{}", line.plain());
            assert!(!raised[measured.log]);
        }
        // A raised prompt fills the log, so the composer meets the panel.
        let prompt = lines
            .iter()
            .find(|line| line.plain().contains("route sandboxed"))
            .unwrap();
        let raised = prompt
            .0
            .iter()
            .filter(|span| span.raised)
            .map(|span| span.text.chars().count())
            .sum::<usize>();
        assert!(raised >= measured.log - 1, "{}", prompt.plain());
    }

    #[test]
    fn the_overview_titles_the_session_and_keeps_its_foot_at_the_bottom() {
        let fixture = Fixture::new();
        let scene = fixture.scene();
        for cols in [176, 250] {
            let (lines, cursor) = render(&scene, cols, 40);
            assert_eq!(lines.len(), 40);
            assert!(lines.iter().all(|line| line.width() == cols));
            // The dock stays above the shared footer.
            assert_eq!(cursor, Some((35, 8)));
            let side = side(&plain(&lines), cols);
            assert_eq!(side[0].trim_end(), "");
            // The title splits evenly over two rows in the narrow panel.
            assert_eq!(side[1].trim_end(), "Route sandboxed bash");
            assert_eq!(side[2].trim_end(), "through the egress proxy");
            assert_eq!(side[3].trim_end(), "started 14:02");
            assert!(
                side[5].starts_with("waiting · 1") && side[5].trim_end().ends_with("tab answers")
            );
            assert!(side[6].starts_with("? bash exec  cargo update -p tokio"));
            let at = |name: &str| side.iter().position(|line| line.starts_with(name)).unwrap();
            let context = at("context");
            // The percentage sits on the section's row, flush right where
            // "tab answers" ends, and the gauge spans the same width.
            assert!(side[context].trim_end().ends_with(" 10%"));
            let gauge = side[context + 1].trim_end();
            assert!(gauge.chars().all(|c| c == '━'), "{gauge}");
            assert_eq!(gauge.chars().count(), side[5].trim_end().chars().count());
            assert_eq!(
                side[context].trim_end().chars().count(),
                gauge.chars().count()
            );
            assert_eq!(side[context + 2].trim_end(), "29.8k of 272k · 94% cached");
            let changes = at("changes");
            assert!(side[changes].trim_end().ends_with("+43 −28"));
            // Paths group under their directories, less the `crates/` they
            // share.
            assert_eq!(side[changes + 1].trim_end(), "xi-sandbox/src");
            assert!(side[changes + 2].starts_with("  lib.rs"));
            assert!(side[changes + 2].trim_end().ends_with("+21 −12"));
            assert_eq!(side[changes + 3].trim_end(), "xi-tools/src");
            let agents = at("agents");
            assert!(side[agents].trim_end().ends_with("1 agent"));
            assert!(side[agents + 1].starts_with("┤ Audit the proxy's allow list"));
            assert!(side[agents + 1].trim_end().ends_with("12.0s"));
            assert_eq!(
                side[agents + 2].trim_end(),
                "  read crates/xi-sandbox/src/net.rs"
            );
            // Other sessions wait for alt+1.
            assert!(!side.iter().any(|line| line.contains("Port footer")));
            // The newest tool row stays in the log and the inspector.
            assert!(!side.iter().any(|line| line.contains("edit ×5")));
            // The model and the rules close the panel; the sessions key
            // shares the log's baseline.
            assert_eq!(side[36].trim_end(), "chatgpt/gpt-6 · high");
            assert_eq!(side[37].trim_end(), "ask · sandboxed · trusted");
            assert_eq!(side[38].trim_end(), "");
            assert_eq!(side[39].trim_end(), "alt+1 sessions");
            assert!(lines[39].plain().starts_with(" ~/src/xi  main*"));
            assert!(lines[39].0.iter().all(|span| !span.raised));
            assert_eq!(side.iter().filter(|l| l.contains("trusted")).count(), 1);
            assert!(!side.iter().any(|line| line.contains("~/src/xi")));
        }
    }

    #[test]
    fn the_panel_and_the_prompt_sit_on_the_raised_surface_and_the_log_does_not() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        let (lines, _) = render(&scene, 176, 40);
        let log = columns(176).log;
        for line in &lines[..lines.len() - 1] {
            let (mut col, mut raised) = (0, Vec::new());
            for span in &line.0 {
                raised.extend(std::iter::repeat_n(span.raised, span.text.chars().count()));
                col += span.text.chars().count();
            }
            assert_eq!(col, 176);
            assert!(raised[log + 1..].iter().all(|r| *r), "{}", line.plain());
            assert!(!raised[0] && !raised[log]);
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
        assert_eq!(text[0].chars().nth(log), Some(' '));
        scene.surfaces = false;
        let text = plain(&render(&scene, 176, 40).0);
        assert!(
            text[..text.len() - 1]
                .iter()
                .all(|line| line.chars().nth(log) == Some('│'))
        );
        assert_eq!(text.last().unwrap().chars().nth(log), Some(' '));
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
        assert_eq!(text[8], "…");
        assert_eq!(text[9], "");
        assert_eq!(text[10], "chatgpt/gpt-6 · high");
        assert_eq!(text[11], "ask · sandboxed · trusted");
        let mut running = fixture.scene();
        running.panel.activity = Line::default().brass("⠋").dim(" working · 4s");
        let text = plain(&overview(&running, 60, 12));
        assert_eq!(
            text[9..],
            [
                "⠋ working · 4s",
                "chatgpt/gpt-6 · high",
                "ask · sandboxed · trusted"
            ]
        );
        assert_eq!(
            plain(&overview(&scene, 60, 1)),
            ["ask · sandboxed · trusted"]
        );
        assert!(overview(&scene, 60, 0).is_empty());
    }

    #[test]
    fn the_overview_says_when_nothing_changed_and_drops_idle_agents() {
        let fixture = Fixture::new();
        let mut scene = fixture.scene();
        scene.panel.asks.clear();
        scene.panel.changed.clear();
        scene.panel.agents.clear();
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
                "started 14:02",
                "",
                "context",
                "2.4k used · window unknown",
                "",
                "changes",
                "nothing yet",
                "",
            ]
        );
        assert!(
            !text
                .iter()
                .any(|line| line.starts_with("waiting") || line.starts_with("agents"))
        );
        // Long titles take two rows and cut the rest.
        scene.title = "a title that runs on well past the width of one panel row and then a second";
        let text = plain(&overview(&scene, 30, 20));
        assert_eq!(text[0], "a title that runs on well past");
        assert!(text[1].ends_with('…') && text[1].chars().count() == 30);
        assert_eq!(text[2], "started 14:02");
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
            .position(|line| line.starts_with("changes"))
            .unwrap();
        // One directory keeps its whole path.
        assert_eq!(text[first + 1], "crates/xi-kernel/src/deeply/nested");
        assert!(text[first + 2].starts_with("  module_0.rs"));
        assert!(text[first + 2].ends_with("+0 −1"));
        assert_eq!(text[first + 1 + CHANGED_ROWS], "… 2 more files");
        scene.panel.changed.truncate(2);
        scene.panel.changed.push(("README.md".into(), 1, 0));
        let text = plain(&overview(&scene, 60, 40));
        assert!(
            text.iter().any(|line| line.starts_with("README.md")),
            "a file at the top keeps no directory row: {text:?}"
        );
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
        assert!(text.iter().any(|line| line.starts_with("changes")));
        assert!(text.iter().any(|line| line.starts_with("agents")));
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
        // The keys wrap above the panel's foot, which stays at the bottom.
        assert!(text[30].trim_end().ends_with("esc back"));
        assert!(text[33].trim_end().ends_with("ask · sandboxed · trusted"));
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
        // Too long to share a row, the headline puts its meta under it.
        assert_eq!(text[0], "edit ×5 crates/xi-tools/src/bash.rs");
        assert_eq!(text[1], "+22 −16");
        assert_eq!(text[2], "net diff · 5 edits");
        assert_eq!(text[4].trim_end(), "+ added 0");
        assert!(text[7].starts_with("xxx"), "wrapped: {text:?}");
        assert!(text[19].starts_with("… ") && text[19].ends_with(" more lines"));
        assert!(rows[7].0.iter().all(|span| span.role == Role::Verdigris));
        scene.panel.scrolled = 4;
        let scrolled = plain(&inspector_view(&scene.panel, 40, 20));
        assert_eq!(scrolled[..4], text[..4]);
        assert_eq!(scrolled[4], text[8]);
        scene.panel.scrolled = 10_000;
        let end = plain(&inspector_view(&scene.panel, 40, 20));
        assert_eq!(end[4].trim_end(), "+ added 39");
        assert_eq!(end[5], "");
        let wide = plain(&inspector_view(&scene.panel, 60, 20));
        assert!(wide[0].starts_with("edit ×5") && wide[0].ends_with("+22 −16"));
        fixture.inspect.text.clear();
        let scene = fixture.scene();
        assert_eq!(plain(&inspector_view(&scene.panel, 40, 6))[4], "no output");
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
        assert!(text[24].trim_end().ends_with("esc back"));
        assert!(text[27].trim_end().ends_with("ask · sandboxed · trusted"));
        scene.focus = Focus::Sessions;
        scene.panel.sessions.selected = 4;
        let (lines, cursor) = render(&scene, 200, 30);
        let text = plain(&lines);
        assert!(
            text.iter()
                .any(|line| line.contains("   › · ledger · Fix export offsets"))
        );
        assert!(text.iter().any(|line| line.contains("   Other workspaces")));
        assert_eq!(cursor, Some((25, 8)));
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
        assert!(text[1].ends_with("started 14:02"));
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
        // A title too long to share its row puts the detail under it.
        pane.here[1].title = "a title far too long to fit beside its age ".repeat(3);
        let text = plain(&sessions_view(&pane, Icons::Unicode, 50, 10));
        assert!(text[3].starts_with("› ┤ a title far") && text[3].contains('…'));
        assert_eq!(text[3].chars().count(), 50);
        assert!(text[4].starts_with("    ") && text[4].ends_with("sonnet-5.5 · 2m"));
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
