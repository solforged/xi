//! The desktop deck, composed without terminal I/O.
//!
//! Sessions fold to a mark rail and the inspector folds into the inline footer.
//! The log keeps its live rows against the bottom of the screen.

use crate::layout::SHEET;
use crate::log::Inspect;
use crate::style::{Line, Role, lr, row, wrap};
use crate::transcript::tokens;

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

/// A session in the rail or the full sessions pane.
pub struct SessionRow {
    /// The session's activity.
    pub mark: Mark,
    /// The title, with the workspace already included for other workspaces.
    pub title: String,
    /// The model and age, or waiting count and age.
    pub detail: String,
}

/// An unanswered approval in the sessions pane.
pub struct AskRow {
    /// Tool and action, such as `bash exec`.
    pub head: String,
    /// The command or other requested operation.
    pub body: String,
    /// Session title and age.
    pub origin: String,
}

/// Sessions and approvals across workspaces.
pub struct SessionsPane {
    /// The current workspace, home-relative when possible.
    pub cwd: String,
    /// Sessions in the current workspace.
    pub here: Vec<SessionRow>,
    /// Sessions in other workspaces.
    pub elsewhere: Vec<SessionRow>,
    /// Unanswered approvals.
    pub asks: Vec<AskRow>,
    /// The selected index into `here` followed by `elsewhere`.
    pub selected: Option<usize>,
}

/// Retained output and the live dock.
pub struct LogPane<'a> {
    /// The attached session's title.
    pub title: &'a str,
    /// Clock, model, head and event count.
    pub meta: &'a str,
    /// Retained rows, oldest first.
    pub rows: &'a [Line],
    /// The retained row emphasized for inspection.
    pub selected: Option<usize>,
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

/// Selected output and the session's supporting facts.
pub struct InspectorPane<'a> {
    /// The selected call or edit run.
    pub inspect: Option<&'a Inspect>,
    /// Context occupancy and traffic.
    pub context: ContextGauge,
    /// Changed paths and their added and removed line counts.
    pub changed: Vec<(String, usize, usize)>,
    /// Queued steering messages.
    pub queued: Vec<String>,
    /// Approval mode, sandbox, trust and tool count.
    pub session: String,
    /// Rows scrolled down from the top of the inspector.
    pub scrolled: usize,
}

/// All input for one deck frame.
pub struct Scene<'a> {
    /// Application, workspace and branch.
    pub header_left: Line,
    /// Provider, model and thinking level.
    pub header_right: Line,
    /// Sessions and approvals, when available.
    pub sessions: Option<SessionsPane>,
    /// The attached session's output and composer.
    pub log: LogPane<'a>,
    /// The selected output and supporting facts.
    pub inspector: InspectorPane<'a>,
    /// Context, cache, sandbox and waiting approvals.
    pub status: Line,
    /// The inline footer used when the inspector is folded.
    pub footer: Line,
    /// The pane receiving keyboard input.
    pub focus: Focus,
}

/// The widths of the deck's visible columns, excluding separators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Columns {
    /// The sessions pane or folded mark rail.
    pub rail: Rail,
    /// The log's width.
    pub log: usize,
    /// The inspector's width when visible.
    pub inspector: Option<usize>,
}

/// How the sessions column folds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rail {
    /// A full sessions pane of this width.
    Full(usize),
    /// Three cells containing activity marks.
    Marks,
    /// No sessions column.
    None,
}

impl Rail {
    fn width(self) -> usize {
        match self {
            Self::Full(width) => width,
            Self::Marks => 3,
            Self::None => 0,
        }
    }
}

/// Measures the columns, including one cell per visible separator.
#[must_use]
pub fn columns(cols: usize) -> Columns {
    let rail = match cols {
        150.. => Rail::Full(28),
        100..150 => Rail::Marks,
        _ => Rail::None,
    };
    let remaining = cols - rail.width() - usize::from(rail != Rail::None);
    let (log, inspector) = if cols >= 200 {
        let available = remaining - 1;
        let log = (SHEET + 2).min(available - 40);
        (log, Some(available - log))
    } else {
        (remaining, None)
    };
    Columns {
        rail,
        log,
        inspector,
    }
}

/// Composes exactly `rows` width-bounded lines and an absolute cursor.
#[must_use]
pub fn render(scene: &Scene<'_>, cols: usize, rows: usize) -> (Vec<Line>, Option<(usize, usize)>) {
    if rows == 0 {
        return (Vec::new(), None);
    }
    let columns = columns(cols);
    let sessions_folded = !matches!(columns.rail, Rail::Full(_)) || scene.sessions.is_none();
    let mut hints = Vec::new();
    if sessions_folded {
        hints.push("alt+1 sessions");
    }
    if columns.inspector.is_none() {
        hints.push("alt+3 inspector");
    }
    let right = if hints.is_empty() {
        scene.header_right.clone()
    } else {
        Line::default().dim(hints.join(" · "))
    };
    let mut out = Vec::with_capacity(rows);
    out.push(lr(scene.header_left.clone(), right, cols));
    if rows == 1 {
        return (out, None);
    }
    let height = rows - 2;
    let rail_width = columns.rail.width();
    let log_x = rail_width + usize::from(columns.rail != Rail::None);
    let rail = session_rows(scene, columns.rail, height);
    // One cell of margin after each separator.
    let (log, cursor) = log_rows(&scene.log, columns.log.saturating_sub(1), height);
    let inspector = columns
        .inspector
        .map(|width| inspector_rows(&scene.inspector, width.saturating_sub(1), height));
    let mut rail = rail.into_iter();
    let mut log = log.into_iter();
    let mut inspector = inspector.map(Vec::into_iter);
    for _ in 0..height {
        let mut line = Line::default();
        if columns.rail != Rail::None {
            line = row(rail.next().unwrap_or_default(), Line::default(), rail_width).dim("│");
        }
        let log_line = Line::default()
            .ink(" ")
            .extend(log.next().unwrap_or_default());
        line = line.extend(pad(log_line, columns.log));
        if let Some(inspector) = inspector.as_mut() {
            line = line
                .dim("│")
                .ink(" ")
                .extend(inspector.next().unwrap_or_default());
        }
        out.push(line.truncate(cols));
    }
    out.push(if columns.inspector.is_some() {
        lr(
            scene.status.clone(),
            Line::default().dim("alt+1 sessions · alt+2 log · alt+3 inspector"),
            cols,
        )
    } else {
        scene.footer.clone().truncate(cols)
    });
    let cursor = cursor.map(|(row, col)| (row + 1, log_x + 1 + col));
    (out, cursor)
}

fn pad(line: Line, width: usize) -> Line {
    let line = line.truncate(width);
    let gap = width - line.width();
    line.ink(" ".repeat(gap))
}

fn mark(mark: Mark) -> Line {
    match mark {
        Mark::Current => Line::default().brass("●"),
        Mark::Running => Line::default().brass("┤"),
        Mark::Waiting => Line::default().brass("?"),
        Mark::Idle => Line::default().dim("·"),
    }
}

fn session_rows(scene: &Scene<'_>, rail: Rail, height: usize) -> Vec<Line> {
    let Some(pane) = &scene.sessions else {
        return Vec::new();
    };
    if height == 0 || rail == Rail::None {
        return Vec::new();
    }
    let selected = (scene.focus == Focus::Sessions)
        .then_some(pane.selected)
        .flatten();
    let mut lines = Vec::new();
    if rail == Rail::Marks {
        for (index, session) in pane.here.iter().enumerate() {
            lines.push(session_mark(session, selected == Some(index)));
        }
        if !pane.elsewhere.is_empty() {
            lines.push(Line::default());
            for (index, session) in pane.elsewhere.iter().enumerate() {
                lines.push(session_mark(
                    session,
                    selected == Some(pane.here.len() + index),
                ));
            }
        }
    } else {
        lines.push(Line::default().dim(format!("Sessions · {}", pane.cwd)));
        lines.push(Line::default());
        for (index, session) in pane.here.iter().enumerate() {
            full_session(&mut lines, session, selected == Some(index));
        }
        lines.push(Line::default());
        lines.push(Line::default().dim("Other workspaces"));
        for (index, session) in pane.elsewhere.iter().enumerate() {
            full_session(
                &mut lines,
                session,
                selected == Some(pane.here.len() + index),
            );
        }
        lines.push(Line::default());
        lines.push(Line::default().dim(format!("Waiting · {}", pane.asks.len())));
        for ask in &pane.asks {
            lines.push(Line::default().brass(format!("? {}", ask.head)));
            lines.push(Line::default().ink(format!("  {}", ask.body)));
            lines.push(Line::default().dim(format!("  {}", ask.origin)));
            lines.push(Line::default().dim("  tab answers"));
        }
        lines.truncate(height - 1);
        lines.resize(height - 1, Line::default());
        lines.push(Line::default().dim("/new · /resume · /tree"));
    }
    lines.truncate(height);
    lines
}

fn session_mark(session: &SessionRow, selected: bool) -> Line {
    if selected {
        Line::default().brass("› ").extend(mark(session.mark))
    } else {
        mark(session.mark)
    }
}

fn full_session(lines: &mut Vec<Line>, session: &SessionRow, selected: bool) {
    let activity = if session.mark == Mark::Idle {
        Line::default().ink(" ")
    } else {
        mark(session.mark)
    };
    let title = if selected {
        Line::default()
            .brass("› ")
            .extend(activity)
            .brass(format!(" {}", session.title))
    } else {
        activity.ink(format!(" {}", session.title))
    };
    lines.push(title);
    lines.push(Line::default().dim(format!("  {}", session.detail)));
}

fn log_rows(
    pane: &LogPane<'_>,
    width: usize,
    height: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let mut lines = vec![Line::default(); height];
    if let Some(line) = lines.first_mut() {
        *line = Line::default()
            .strong(Role::Ink, pane.title)
            .truncate(width);
    }
    if let Some(line) = lines.get_mut(1) {
        *line = Line::default().dim(pane.meta).truncate(width);
    }
    let top = height.min(3);
    let live_skip = pane.live.len().saturating_sub(height - top);
    let live = &pane.live[live_skip..];
    let live_top = height - live.len();
    let end = pane.rows.len().saturating_sub(pane.scrolled);
    let start = end.saturating_sub(live_top - top);
    let retained_top = live_top - (end - start);
    for (index, line) in pane.rows[start..end].iter().enumerate() {
        let mut line = line.clone().truncate(width);
        if pane.selected == Some(start + index) {
            for span in &mut line.0 {
                span.bold = true;
            }
        }
        lines[retained_top + index] = line;
    }
    for (index, line) in live.iter().enumerate() {
        lines[live_top + index] = line.clone().truncate(width);
    }
    let cursor = pane.cursor.and_then(|(row, col)| {
        let row = row.checked_sub(live_skip)?;
        (row < live.len() && width > 0).then(|| (live_top + row, col.min(width - 1)))
    });
    (lines, cursor)
}

fn inspector_rows(pane: &InspectorPane<'_>, width: usize, height: usize) -> Vec<Line> {
    let tail = inspector_facts(pane, width);
    let mut lines = Vec::new();
    if let Some(inspect) = pane.inspect {
        let title = inspect
            .title
            .strip_prefix("▸ ")
            .or_else(|| inspect.title.strip_prefix("● "))
            .unwrap_or(&inspect.title);
        lines.push(lr(
            Line::default().strong(Role::Ink, title),
            Line::default().dim(&inspect.meta),
            width,
        ));
        lines.push(Line::default().dim(&inspect.note));
        lines.push(Line::default());
        let budget = height.saturating_sub(tail.len() + 5);
        let count = inspect.text.lines().count();
        let shown = if count > budget {
            budget.saturating_sub(1)
        } else {
            count
        };
        for text in inspect.text.lines().take(shown) {
            let role = if inspect.diff && text.starts_with("- ") {
                Role::Sanguine
            } else if inspect.diff && text.starts_with("+ ") {
                Role::Verdigris
            } else if inspect.diff {
                Role::Dim
            } else {
                Role::Ink
            };
            lines.push(Line::default().span(role, false, text));
        }
        if count > shown && budget > 0 {
            lines.push(Line::default().dim(format!("… {} more lines", count - shown)));
        }
        lines.push(Line::default().dim("enter opens the whole diff · up and down move in the log"));
        lines.push(Line::default());
    }
    lines.extend(tail);
    lines
        .into_iter()
        .skip(pane.scrolled)
        .take(height)
        .map(|line| line.truncate(width))
        .collect()
}

fn inspector_facts(pane: &InspectorPane<'_>, width: usize) -> Vec<Line> {
    let context = pane.context;
    let mut lines = vec![Line::default().strong(Role::Ink, "Context")];
    if context.window > 0 {
        // The footer truncates; the gauge agrees with it.
        let percent = u128::from(context.used) * 100 / u128::from(context.window);
        let filled = usize::try_from((percent * 30 / 100).min(30)).unwrap_or(30);
        lines.push(lr(
            Line::default()
                .ink(format!("ctx {percent}%  "))
                .brass("█".repeat(filled))
                .dim("░".repeat(30 - filled)),
            Line::default().dim(format!(
                "{} of {}",
                tokens(context.used),
                tokens(context.window)
            )),
            width,
        ));
    } else {
        lines.push(Line::default().dim(format!("ctx unknown · {} used", tokens(context.used))));
    }
    let traffic = format!("↑ {} ↓ {}", tokens(context.sent), tokens(context.output));
    lines.push(Line::default().dim(match context.cache_percent {
        Some(percent) => format!("cache {percent}% · {traffic}"),
        None => traffic,
    }));
    lines.push(Line::default());
    let (added, removed) = pane
        .changed
        .iter()
        .fold((0usize, 0usize), |(added, removed), (_, a, r)| {
            (added.saturating_add(*a), removed.saturating_add(*r))
        });
    let files = if pane.changed.len() == 1 {
        "file"
    } else {
        "files"
    };
    lines.push(lr(
        Line::default().strong(
            Role::Ink,
            format!("Changed · {} {files}", pane.changed.len()),
        ),
        counts(added, removed),
        width,
    ));
    for (path, added, removed) in &pane.changed {
        lines.push(lr(
            Line::default().ink(path),
            counts(*added, *removed),
            width,
        ));
    }
    lines.push(Line::default());
    if !pane.queued.is_empty() {
        lines.push(Line::default().strong(Role::Ink, "Queued"));
        for text in &pane.queued {
            for (index, text) in wrap(text, width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                lines.push(
                    Line::default()
                        .brass(if index == 0 { "» " } else { "  " })
                        .ink(text),
                );
            }
        }
        lines.push(Line::default());
    }
    lines.push(Line::default().strong(Role::Ink, "Session"));
    lines.extend(
        wrap(&pane.session, width.max(1))
            .into_iter()
            .map(|text| Line::default().dim(text)),
    );
    lines
}

fn counts(added: usize, removed: usize) -> Line {
    Line::default()
        .verdigris(format!("+{added}"))
        .dim(" ")
        .sanguine(format!("−{removed}"))
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
        fn new(cols: usize) -> Self {
            let width = columns(cols).log.min(SHEET);
            Self {
                retained: vec![
                    Line::default().brass("ξ ").ink("route sandboxed bash through the egress proxy on Linux, and keep direct commands"),
                    Line::default().ink("  unsandboxed with their own process group"),
                    Line::default(),
                    lr(
                        Line::default().dim("  ∴ ").strong(Role::Ink, "Planning the spawn path"),
                        Line::default().dim("14s · 2.1k"),
                        width,
                    ),
                    Line::default().dim("    Spec::command builds the helper invocation before setsid. · ctrl+t"),
                    Line::default(),
                    lr(
                        Line::default().verdigris("● ").ink("read crates/xi-sandbox/src/lib.rs:1-180"),
                        Line::default().dim("180 lines · 0.0s"),
                        width,
                    ),
                    lr(
                        Line::default().dim("▸ ").ink("edit ×5 crates/xi-tools/src/bash.rs"),
                        counts(22, 16).dim(" · 0.1s"),
                        width,
                    ),
                    lr(
                        Line::default().sanguine("✕ ").ink("bash cargo test -p xi-sandbox egress"),
                        Line::default().sanguine("exit 101").dim(" · 6.8s"),
                        width,
                    ),
                    Line::default().dim("  … 12 more lines · enter"),
                    Line::default(),
                ],
                live: vec![
                    lr(
                        Line::default().dim("┤ ").ink("bash cargo test -p xi-sandbox egress"),
                        Line::default().dim("sandbox · 3.1s"),
                        width,
                    ),
                    Line::default().dim("└ working · 5m13s · ↑ 2.3M ↓ 5.5k · esc stop"),
                    Line::default().brass(format!("╭{}╮", "─".repeat(width - 2))),
                    pad(
                        Line::default().brass("│ ").ink("ξ enter steers · alt+enter queues · esc stops"),
                        width - 1,
                    ).brass("│"),
                    Line::default().brass(format!("╰{}╯", "─".repeat(width - 2))),
                ],
                inspect: Inspect {
                    title: "edit ×5 crates/xi-tools/src/bash.rs".into(),
                    meta: "+22 −16".into(),
                    note: "net diff · 5 edits · the row selected in the log".into(),
                    diff: true,
                    text: [
                        "-     let mut command = spec",
                        "-         .command(&ctx.helper, shell, &args);",
                        "+     let launch_error = |error| {",
                        "+         if spec.enabled {",
                        "  …",
                        "      ┄",
                        "-     // Do not set process_group(0) here.",
                        "+     // Sandboxed commands need setsid in the helper.",
                    ].join("\n"),
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
                    .strong(Role::Ink, "XI")
                    .ink("  ~/src/xi  main*"),
                header_right: Line::default().dim("chatgpt/gpt-6.1-sol · think xhigh"),
                sessions: Some(SessionsPane {
                    cwd: "~/src/xi".into(),
                    here: vec![
                        session(
                            Mark::Current,
                            "Route sandboxed bash through the egress proxy",
                            "gpt-6.1-sol · 5m",
                        ),
                        session(Mark::Running, "Port footer fit to deck", "sonnet-5.5 · 2m"),
                        session(Mark::Waiting, "Bump tokio to 1.48", "1 waiting · 12m"),
                        session(Mark::Idle, "Settings frame hints", "yesterday"),
                        session(Mark::Idle, "Overlay stack refactor", "2 days ago"),
                    ],
                    elsewhere: vec![session(
                        Mark::Running,
                        "ledger · Fix export offsets",
                        "gpt-6.1-sol · 1h",
                    )],
                    asks: vec![AskRow {
                        head: "bash exec".into(),
                        body: "cargo update -p tokio".into(),
                        origin: "Bump tokio to 1.48 · 12m".into(),
                    }],
                    selected: Some(1),
                }),
                log: LogPane {
                    title: "Route sandboxed bash through the egress proxy",
                    meta: "○ 14:02 · gpt-6.1-sol · head #a3f9 · 41 events",
                    rows: &self.retained,
                    selected: Some(7),
                    scrolled: 0,
                    live: &self.live,
                    cursor: Some((3, 4)),
                },
                inspector: InspectorPane {
                    inspect: Some(&self.inspect),
                    context: ContextGauge {
                        used: 29_800,
                        window: 272_000,
                        cache_percent: Some(94),
                        sent: 2_300_000,
                        output: 5_500,
                    },
                    changed: vec![
                        ("crates/xi-sandbox/src/lib.rs".into(), 21, 12),
                        ("crates/xi-tools/src/bash.rs".into(), 22, 16),
                    ],
                    queued: vec!["also run clippy on both crates once the tests pass".into()],
                    session: "approvals ask · ◇ sandbox · trusted · 9 tools".into(),
                    scrolled: 0,
                },
                status: Line::default().dim("ctx 11% · cache 94% · ◇ sandbox · ? 1 waiting"),
                footer: Line::default()
                    .ink("~/src/xi  main*")
                    .dim(" · gpt-6.1-sol · think xhigh · ctx 11% · cache 94% · ◇ · ? 1 waiting"),
                focus: Focus::Composer,
            }
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    #[test]
    fn columns_fold_at_their_thresholds_and_account_for_separators() {
        assert_eq!(
            columns(250),
            Columns {
                rail: Rail::Full(28),
                log: 122,
                inspector: Some(98)
            }
        );
        assert_eq!(
            columns(200),
            Columns {
                rail: Rail::Full(28),
                log: 122,
                inspector: Some(48)
            }
        );
        assert_eq!(
            columns(199),
            Columns {
                rail: Rail::Full(28),
                log: 170,
                inspector: None
            }
        );
        assert_eq!(
            columns(150),
            Columns {
                rail: Rail::Full(28),
                log: 121,
                inspector: None
            }
        );
        assert_eq!(
            columns(149),
            Columns {
                rail: Rail::Marks,
                log: 145,
                inspector: None
            }
        );
        assert_eq!(
            columns(124),
            Columns {
                rail: Rail::Marks,
                log: 120,
                inspector: None
            }
        );
        assert_eq!(
            columns(100),
            Columns {
                rail: Rail::Marks,
                log: 96,
                inspector: None
            }
        );
        assert_eq!(
            columns(99),
            Columns {
                rail: Rail::None,
                log: 99,
                inspector: None
            }
        );
        for cols in 0..500 {
            let c = columns(cols);
            assert_eq!(
                c.rail.width()
                    + usize::from(c.rail != Rail::None)
                    + c.log
                    + usize::from(c.inspector.is_some())
                    + c.inspector.unwrap_or(0),
                cols,
            );
        }
    }

    #[test]
    fn deck_renders_each_fold_with_a_bottom_aligned_live_cursor() {
        for cols in [250, 199, 149, 124, 99] {
            let fixture = Fixture::new(cols);
            let scene = fixture.scene();
            let (lines, cursor) = render(&scene, cols, 36);
            assert_eq!(lines.len(), 36);
            assert!(lines.iter().all(|line| line.width() <= cols));
            let columns = columns(cols);
            let log_x = columns.rail.width() + usize::from(columns.rail != Rail::None);
            assert_eq!(cursor, Some((33, log_x + 5)));
            let text = plain(&lines);
            assert!(text[1].contains(scene.log.title));
            assert!(text[2].contains(scene.log.meta));
            assert!(text[33].contains("ξ enter steers"));
            assert!(text[34].contains("╰"));
            assert_eq!(text[0].contains("alt+1 sessions"), cols < 150);
            assert_eq!(text[0].contains("alt+3 inspector"), cols < 200);
            if cols >= 200 {
                for section in [
                    "Other workspaces",
                    "Waiting · 1",
                    "? bash exec",
                    "/new · /resume · /tree",
                    "net diff · 5 edits",
                    "let launch_error",
                    "Context",
                    "ctx 11%",
                    "cache 94%",
                    "Changed · 2 files",
                    "+43",
                    "−28",
                    "Queued",
                    "» also run clippy",
                    "Session",
                    "approvals ask",
                    "enter opens the whole diff",
                ] {
                    assert!(
                        text.iter().any(|line| line.contains(section)),
                        "{section} missing"
                    );
                }
                assert!(text[35].ends_with("alt+1 sessions · alt+2 log · alt+3 inspector"));
            } else {
                assert_eq!(text[35], scene.footer.plain());
            }
            if (100..150).contains(&cols) {
                assert!(text[1].starts_with("●  │"));
                assert!(text[2].starts_with("┤  │"));
                assert!(text[3].starts_with("?  │"));
                assert!(text[4].starts_with("·  │"));
                assert!(text[6].starts_with("   │"));
                assert!(text[7].starts_with("┤  │"));
            }
        }
    }

    #[test]
    fn selected_log_rows_keep_their_roles_and_text_but_become_strong() {
        let fixture = Fixture::new(250);
        let scene = fixture.scene();
        let (rows, _) = log_rows(&scene.log, 122, 34);
        let selected = rows
            .iter()
            .find(|line| line.plain() == fixture.retained[7].plain())
            .unwrap();
        assert!(!selected.plain().starts_with('›'));
        assert_eq!(selected.0.len(), fixture.retained[7].0.len());
        for (strong, original) in selected.0.iter().zip(&fixture.retained[7].0) {
            assert!(strong.bold);
            assert_eq!(strong.role, original.role);
            assert_eq!(strong.text, original.text);
        }
        let (_, cursor) = render(&scene, 250, 36);
        assert_eq!(cursor, Some((33, 34)));
    }

    #[test]
    fn sessions_show_selection_only_while_focused() {
        let fixture = Fixture::new(250);
        let mut scene = fixture.scene();
        assert!(
            !plain(&session_rows(&scene, Rail::Full(28), 34))
                .iter()
                .any(|line| line.starts_with('›'))
        );
        scene.focus = Focus::Sessions;
        assert!(
            plain(&session_rows(&scene, Rail::Full(28), 34))
                .iter()
                .any(|line| line.starts_with("› ┤ Port"))
        );
        let marks = session_rows(&scene, Rail::Marks, 34);
        assert_eq!(marks[1].plain(), "› ┤");
        scene.sessions.as_mut().unwrap().selected = Some(5);
        assert_eq!(session_rows(&scene, Rail::Marks, 34)[6].plain(), "› ┤");
    }

    #[test]
    fn log_scrolling_keeps_live_rows_fixed_and_crops_hidden_cursors() {
        let fixture = Fixture::new(124);
        let mut scene = fixture.scene();
        let (bottom, bottom_cursor) = log_rows(&scene.log, 120, 12);
        assert!(
            plain(&bottom)
                .iter()
                .any(|line| line.contains("12 more lines"))
        );
        scene.log.scrolled = 4;
        let (scrolled, scrolled_cursor) = log_rows(&scene.log, 120, 12);
        assert!(
            !plain(&scrolled)
                .iter()
                .any(|line| line.contains("12 more lines"))
        );
        assert_eq!(bottom_cursor, scrolled_cursor);
        assert_eq!(&bottom[7..], &scrolled[7..]);
        let (short, cursor) = log_rows(&scene.log, 120, 6);
        assert_eq!(short.len(), 6);
        assert_eq!(cursor, Some((4, 4)));
        scene.log.cursor = Some((0, 0));
        assert_eq!(log_rows(&scene.log, 120, 6).1, None);
    }

    #[test]
    fn inspector_preserves_sections_cuts_long_diffs_and_scrolls() {
        let mut fixture = Fixture::new(250);
        fixture.inspect.title.insert_str(0, "▸ ");
        fixture.inspect.text = (0..80)
            .map(|n| format!("+ added {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut scene = fixture.scene();
        let rows = inspector_rows(&scene.inspector, 98, 34);
        let text = plain(&rows);
        assert!(text[0].starts_with("edit ×5"));
        assert!(!text[0].starts_with('▸'));
        assert!(text.iter().any(|line| line.contains("more lines")));
        assert!(text.iter().any(|line| line == "Context"));
        assert!(text.iter().any(|line| line == "Session"));
        assert_eq!(text.len(), 34);
        let added = rows
            .iter()
            .find(|line| line.plain().starts_with("+ added"))
            .unwrap();
        assert_eq!(added.0[0].role, Role::Verdigris);
        scene.inspector.scrolled = 3;
        let scrolled = inspector_rows(&scene.inspector, 98, 34);
        assert_eq!(scrolled, rows[3..]);
        scene.inspector.queued.clear();
        scene.inspector.context.window = 0;
        scene.inspector.context.cache_percent = None;
        scene.inspector.inspect = None;
        scene.inspector.scrolled = 0;
        let text = plain(&inspector_rows(&scene.inspector, 98, 34));
        assert!(!text.iter().any(|line| line == "Queued"));
        assert!(
            !text
                .iter()
                .any(|line| line.contains('░') || line.contains('█'))
        );
        assert!(text.iter().any(|line| line.contains("ctx unknown")));
        assert!(text.iter().any(|line| line == "↑ 2.3M ↓ 5.5k"));
    }

    #[test]
    fn inspector_diff_rows_use_existing_semantic_roles() {
        let fixture = Fixture::new(250);
        let rows = inspector_rows(&fixture.scene().inspector, 98, 34);
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
    fn deck_bounds_hold_on_empty_short_and_zero_sized_screens() {
        let fixture = Fixture::new(250);
        let mut scene = fixture.scene();
        scene.sessions = None;
        for cols in [0, 1, 2, 60, 99, 100, 124, 149, 199, 200, 250] {
            for rows in 0..20 {
                let (lines, cursor) = render(&scene, cols, rows);
                assert_eq!(lines.len(), rows);
                assert!(lines.iter().all(|line| line.width() <= cols));
                if let Some((row, col)) = cursor {
                    assert!(row < rows && col < cols);
                }
            }
        }
        assert!(
            render(&scene, 250, 36).0[0]
                .plain()
                .contains("alt+1 sessions")
        );
    }

    #[test]
    fn deck_plain_text_dumps_at_desktop_and_folded_widths() {
        for cols in [250, 124] {
            let fixture = Fixture::new(cols);
            let (lines, _) = render(&fixture.scene(), cols, 36);
            println!("--- deck {cols}×36 ---");
            for line in lines {
                println!("{}", line.plain().trim_end());
            }
        }
    }
}
