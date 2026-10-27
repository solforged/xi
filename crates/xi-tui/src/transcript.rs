//! Daemon events in, terminal lines out.
//!
//! [`Transcript::apply`] returns the lines a notification commits to
//! scrollback. Everything not yet committed (streaming text and calls still
//! waiting for their result) is drawn by [`Transcript::live`]. A block
//! commits when its last event lands, and calls commit in the order the model
//! made them, so scrollback reads in call order.

use std::time::Instant;

use serde_json::Value;
use xi_client::client::ClientNotification;
use xi_protocol::event::{Decision, ModelRef, Remember, StopReason, Usage};
use xi_protocol::methods::client::{Delta, EventParams};
use xi_protocol::model::Pricing;
use xi_protocol::{Block, EventKind, OutputBlock, SessionId, Timestamp, ToolCallId, ToolOutput};

use crate::children::Children;
use crate::style::{Line, Role, inline, lr, prose, wrap};

/// Terminal spinner frames, one every 125 ms.
pub const SPINNER: [&str; 8] = ["┤", "┘", "┴", "└", "├", "┌", "┬", "┐"];

/// Result lines shown under a call before the rest is counted.
pub(crate) const RESULT_LINES: usize = 3;
/// Streaming rows kept in the live region.
const STREAM_ROWS: usize = 6;
/// Diff rows shown before the rest is kept for ctrl+o.
const DIFF_ROWS: usize = 8;

/// How visible model reasoning is displayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Thinking {
    /// Show all reasoning text.
    Full,
    /// Show the first committed line or newest live line.
    #[default]
    First,
    /// Hide reasoning text without hiding the activity indicator.
    Hidden,
}

impl Thinking {
    /// Cycles full, first line, hidden, then full again.
    #[must_use]
    pub fn next(self) -> Thinking {
        match self {
            Self::Full => Self::First,
            Self::First => Self::Hidden,
            Self::Hidden => Self::Full,
        }
    }

    /// The name shown in display-setting notes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::First => "first line",
            Self::Hidden => "hidden",
        }
    }

    fn rows(self, text: &str, cols: usize, live: bool) -> Vec<Line> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        match self {
            Self::Hidden => Vec::new(),
            Self::Full => {
                let rows = prose(text, cols.saturating_sub(4), Role::Dim);
                let skip = if live {
                    rows.len().saturating_sub(STREAM_ROWS)
                } else {
                    0
                };
                rows.into_iter()
                    .skip(skip)
                    .enumerate()
                    .map(|(i, row)| {
                        let lead = if i == 0 { "  ∴ " } else { "    " };
                        Line::default().dim(lead).extend(row).truncate(cols)
                    })
                    .collect()
            }
            Self::First => {
                let mut rows = text.lines();
                let row = if live {
                    rows.rfind(|line| !line.trim().is_empty())
                } else {
                    rows.find(|line| !line.trim().is_empty())
                };
                let Some(row) = row else { return Vec::new() };
                let more = !live && rows.next().is_some();
                let line = Line::default().dim("  ∴ ").extend(inline(row, Role::Dim));
                vec![if more {
                    line.truncate(cols.saturating_sub(9))
                        .dim(" · ctrl+t")
                        .truncate(cols)
                } else {
                    line.truncate(cols)
                }]
            }
        }
    }
}

/// An ask waiting on you.
#[derive(Clone, Debug)]
pub struct Ask {
    /// The call asked about.
    pub call: ToolCallId,
    /// Tool name.
    pub tool: String,
    /// The boundary crossing, as the kernel describes it.
    pub action: String,
    /// The rule that matched, as `layer#index`.
    pub rule: Option<String>,
    /// Whether taint caused this ask.
    pub taint: bool,
    /// Workspace trust supplied by a pending-approval notification.
    pub trusted: Option<bool>,
}

struct Call {
    id: ToolCallId,
    name: String,
    arg: String,
    input: Value,
    since: Option<Instant>,
    decided_at: Option<Timestamp>,
    decision: Option<Decision>,
    rule: Option<String>,
    approved: Option<bool>,
    remember: Remember,
    result: Option<(ToolOutput, Option<std::path::PathBuf>, Timestamp)>,
    /// Children the call started, once its result has landed.
    children: Children,
}

/// Text the transcript showed cut short, kept whole for ctrl+o.
pub(crate) struct Detail {
    pub(crate) title: String,
    pub(crate) diff: bool,
    pub(crate) text: String,
    pub(crate) error: bool,
    pub(crate) spill: Option<std::path::PathBuf>,
}

/// Token totals.
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    /// Uncached input.
    pub input: u64,
    /// Output.
    pub output: u64,
    /// Cache reads.
    pub cache_read: u64,
    /// Cache writes.
    pub cache_write: u64,
}

impl Totals {
    pub(crate) fn add(&mut self, usage: Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
    }

    /// Every input token, cached or not.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Cost in US dollars, when the model's prices are known.
    #[must_use]
    pub fn cost(&self, pricing: Option<Pricing>) -> Option<f64> {
        let p = pricing?;
        #[allow(clippy::cast_precision_loss)]
        let m = |n: u64, price: f64| n as f64 * price / 1e6;
        Some(
            m(self.input, p.input)
                + m(self.output, p.output)
                + m(self.cache_read, p.cache_read)
                + m(self.cache_write, p.cache_write),
        )
    }
}

/// What the client knows about the session so far.
#[derive(Default)]
pub struct Transcript {
    calls: Vec<Call>,
    stream: String,
    thinking: bool,
    asks: Vec<Ask>,
    stream_thinking: String,
    redacted_thinking: bool,
    thinking_display: Thinking,
    last_text: Option<String>,
    /// Whether the head's branch has read untrusted content.
    pub tainted: bool,
    /// Usage across the session.
    pub session: Totals,
    /// Usage in the running turn.
    pub turn: Totals,
    /// Context size of the latest request.
    pub context: u64,
    /// The selected model, once known.
    pub model: Option<ModelRef>,
    /// Workspace root, from the session's first event.
    workspace: Option<std::path::PathBuf>,
    blank: bool,
    /// Collapsed results, oldest first.
    details: Vec<Detail>,
    /// Collapsed results already expanded, counted back from the newest.
    expanded: usize,
    /// Children started since the last tool result.
    children: Children,
    /// Content sent by this client, until its live event echo arrives.
    sent: Vec<Vec<Block>>,
}

/// `14.2k`, `1.2M`, `950`.
#[must_use]
pub fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", f / 1e3),
        _ => format!("{:.1}M", f / 1e6),
    }
}

/// `4.2s`, `1m12s`.
#[must_use]
pub fn duration(ms: u64) -> String {
    if ms < 60_000 {
        #[allow(clippy::cast_precision_loss)]
        let s = ms as f64 / 1e3;
        format!("{s:.1}s")
    } else {
        format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000)
    }
}

/// `path` relative to the workspace when it lies inside it; `.` for the
/// workspace itself.
fn relative<'a>(path: &'a str, workspace: Option<&std::path::Path>) -> &'a str {
    let Some(rest) = workspace
        .and_then(|w| w.to_str())
        .and_then(|w| path.strip_prefix(w))
    else {
        return path;
    };
    match rest.strip_prefix('/') {
        Some(rest) if !rest.is_empty() => rest,
        None if rest.is_empty() => ".",
        _ => path,
    }
}

/// The argument that matters for a call's headline. Paths inside the
/// workspace show relative to it, and a command's leading `cd` into the
/// workspace is dropped, since every command already starts there.
pub(crate) fn headline_arg(
    name: &str,
    input: &Value,
    workspace: Option<&std::path::Path>,
) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str);
    let first = |s: &str| s.lines().next().unwrap_or("").to_owned();
    match name {
        "bash" => field("command").map(|c| {
            let c = c.trim_start();
            let rest = c.strip_prefix("cd ").and_then(|rest| {
                let (dir, rest) = rest.split_once("&&")?;
                (relative(dir.trim().trim_matches(['"', '\'']), workspace) == ".")
                    .then(|| rest.trim_start())
            });
            first(rest.unwrap_or(c))
        }),
        "read" => field("path").map(|p| {
            let path = relative(p, workspace);
            let number = |k: &str| input.get(k).and_then(Value::as_u64);
            match (number("offset"), number("limit")) {
                (None, None) => path.to_owned(),
                (Some(from), None) => format!("{path}:{from}-"),
                (from, Some(limit)) => {
                    let from = from.unwrap_or(1);
                    format!(
                        "{path}:{from}-{}",
                        from.saturating_add(limit.saturating_sub(1))
                    )
                }
            }
        }),
        "write" | "edit" | "hashline" => field("path").map(|p| relative(p, workspace).to_owned()),
        "search" => field("pattern").map(|p| match field("path").map(|d| relative(d, workspace)) {
            Some(".") | None => p.to_owned(),
            Some(path) => format!("{p} in {path}"),
        }),
        // The subagents package's tool; its children show in their own rows.
        "task" => input
            .get("tasks")
            .and_then(Value::as_array)
            .map(|tasks| counted(tasks.len(), "task", "tasks")),
        _ => field("url").map(str::to_owned),
    }
    .unwrap_or_else(|| {
        if input.as_object().is_some_and(serde_json::Map::is_empty) {
            String::new()
        } else {
            input.to_string()
        }
    })
}

fn text_of(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .map(|b| match b {
            OutputBlock::Text { text } => text.as_str(),
            OutputBlock::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `n` and the noun for that many.
pub(crate) fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The hint for lines cut from a block.
pub(crate) fn more_lines(hidden: usize) -> String {
    format!("… {} · ctrl+o", counted(hidden, "more line", "more lines"))
}

/// Event timestamps are milliseconds since the epoch. No local-time library
/// is linked by the client, so slips use UTC.
pub(crate) fn clock(at: Timestamp) -> String {
    let minutes = at.0 / 60_000;
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

/// A short, open-sided rule that still fits a narrow terminal.
fn detail_rule(title: &str, cols: usize) -> Line {
    let width = cols.min(48);
    let title = Line::default()
        .dim(title)
        .truncate(width.saturating_sub(6))
        .plain();
    Line::default()
        .dim(format!("── {title} ──"))
        .truncate(width)
}

/// Only a numeric status on the last nonblank line is a bash exit footer.
fn exit_status(lines: &[&str]) -> Option<(usize, u32)> {
    let index = lines.iter().rposition(|line| !line.trim().is_empty())?;
    let code = lines[index]
        .trim()
        .strip_prefix("[exit ")?
        .strip_suffix(']')?;
    if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((index, code.parse().ok()?))
}

/// A read's size: the numbered lines of a file, or a directory's entries.
fn read_size(output: &ToolOutput) -> String {
    if output
        .content
        .iter()
        .any(|b| matches!(b, OutputBlock::Image { .. }))
    {
        return "image".to_owned();
    }
    let text = text_of(output);
    // File lines read `12|text` or `12:ab|text`; status notes and directory
    // entries don't.
    let numbered = |line: &str| {
        line.split_once('|').is_some_and(|(number, _)| {
            let number = number.split_once(':').map_or(number, |(n, _)| n);
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        })
    };
    match text.lines().next() {
        Some(first) if !numbered(first) => counted(text.lines().count(), "entry", "entries"),
        _ => counted(
            text.lines().filter(|l| numbered(l)).count(),
            "line",
            "lines",
        ),
    }
}

/// A numeric exit footer is the alarm; without one, the first nonblank line
/// states the failure and the rest is context.
fn alarm(lines: &[&str]) -> std::ops::Range<usize> {
    let index = exit_status(lines)
        .map(|(index, _)| index)
        .unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| !line.trim().is_empty())
                .unwrap_or(0)
        });
    index..index + 1
}

struct Diff<'a> {
    rows: Vec<(Role, &'a str)>,
    added: usize,
    removed: Option<usize>,
}

fn diff_line(role: Role, text: &str, cols: usize) -> Line {
    let lead = match role {
        Role::Sanguine => "  - ",
        Role::Verdigris => "  + ",
        _ => "    ",
    };
    Line::default()
        .span(role, false, lead)
        .span(role, false, text)
        .truncate(cols)
}

impl Call {
    fn mark(&self) -> Line {
        let Some((output, ..)) = &self.result else {
            return Line::default();
        };
        if !output.is_error {
            Line::default().verdigris("●")
        } else if text_of(output).starts_with("Stale anchors") {
            // The anchor did its job: nothing was written, and the model retries.
            Line::default().brass("↺")
        } else {
            Line::default().sanguine("✕")
        }
    }

    fn head(&self, mark: Line) -> Line {
        let line = mark.ink(" ").strong(Role::Ink, &self.name);
        if self.arg.is_empty() {
            line
        } else {
            line.ink(" ").ink(&self.arg)
        }
    }

    fn detail_title(&self) -> String {
        if self.arg.is_empty() {
            self.name.clone()
        } else {
            format!("{} · {}", self.name, self.arg)
        }
    }

    fn diff(&self) -> Option<Diff<'_>> {
        match self.name.as_str() {
            "edit" => {
                let old: Vec<_> = self.input.get("old")?.as_str()?.lines().collect();
                let new: Vec<_> = self.input.get("new")?.as_str()?.lines().collect();
                let common_start = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
                let common_end = old[common_start..]
                    .iter()
                    .rev()
                    .zip(new[common_start..].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                let old_end = old.len() - common_end;
                let new_end = new.len() - common_end;
                let mut rows = Vec::new();
                if common_start > 0 {
                    rows.push((Role::Dim, old[common_start - 1]));
                }
                rows.extend(
                    old[common_start..old_end]
                        .iter()
                        .map(|line| (Role::Sanguine, *line)),
                );
                rows.extend(
                    new[common_start..new_end]
                        .iter()
                        .map(|line| (Role::Verdigris, *line)),
                );
                if common_end > 0 {
                    rows.push((Role::Dim, old[old_end]));
                }
                Some(Diff {
                    rows,
                    added: new_end - common_start,
                    removed: Some(old_end - common_start),
                })
            }
            "hashline" => {
                let mut rows = Vec::new();
                for edit in self.input.get("edits")?.as_array()? {
                    for line in edit.get("lines")?.as_array()? {
                        rows.push((Role::Verdigris, line.as_str()?));
                    }
                }
                Some(Diff {
                    added: rows.len(),
                    removed: None,
                    rows,
                })
            }
            _ => None,
        }
    }

    /// The block the call commits, and its whole result when the block cut
    /// lines from it.
    fn committed(&self, cols: usize) -> (Vec<Line>, Vec<Detail>) {
        let Some((output, spill, at)) = &self.result else {
            return (Vec::new(), Vec::new());
        };
        let diff = if output.is_error { None } else { self.diff() };
        let text = if diff.is_some() {
            String::new()
        } else {
            text_of(output)
        };
        let count = text.lines().count();
        let rows: Vec<&str> = text.lines().collect();
        let exit = if self.name == "bash" && output.is_error {
            exit_status(&rows)
        } else {
            None
        };
        let mut meta = Vec::new();
        if let Some(diff) = &diff {
            meta.push(match diff.removed {
                Some(removed) => format!("+{} −{removed}", diff.added),
                None => format!("+{}", diff.added),
            });
        }
        match (self.decision, self.approved) {
            (Some(Decision::Ask), Some(true)) => meta.push(
                match self.remember {
                    Remember::Once => "allowed by you",
                    Remember::Session => "allowed for this session",
                    Remember::Workspace => "always allowed here",
                }
                .to_owned(),
            ),
            (Some(Decision::Ask), Some(false)) => meta.push("denied by you".to_owned()),
            (Some(Decision::Deny), _) => meta.push("denied".to_owned()),
            _ => {}
        }
        // Built-in rules allow ordinary work; only rules someone wrote are news.
        if let Some(rule) = self.rule.as_ref().filter(|r| !r.starts_with("builtin#")) {
            meta.push(if rule == "session" {
                "allowed for this session".to_owned()
            } else {
                rule.clone()
            });
        }
        let quiet = !output.is_error && matches!(self.name.as_str(), "read" | "search");
        if quiet {
            meta.push(if self.name == "read" {
                read_size(output)
            } else {
                counted(count, "hit", "hits")
            });
        }
        if let Some(from) = self.decided_at {
            meta.push(duration(at.0.saturating_sub(from.0)));
        }
        let role = if self.mark().plain() == "↺" {
            Role::Brass
        } else if matches!(self.decision, Some(Decision::Deny)) || self.approved == Some(false) {
            Role::Sanguine
        } else {
            Role::Dim
        };
        let mut right = Line::default();
        if let Some((_, code)) = exit {
            right = right.sanguine(format!("exit {code}"));
            if !meta.is_empty() {
                right = right.dim(" · ");
            }
        }
        right = right.span(role, false, meta.join(" · "));
        let mut lines = vec![lr(self.head(self.mark()), right, cols)];
        if !self.children.is_empty() {
            let (slips, mut details) = self.children.committed(cols);
            lines.extend(slips);
            if !text.trim().is_empty() {
                details.insert(
                    0,
                    Detail {
                        title: self.detail_title(),
                        diff: false,
                        text,
                        error: output.is_error,
                        spill: spill.clone(),
                    },
                );
            }
            return (lines, details);
        }
        if let Some(diff) = diff {
            lines.extend(
                diff.rows
                    .iter()
                    .take(DIFF_ROWS)
                    .map(|(role, row)| diff_line(*role, row, cols)),
            );
            let hidden = diff.rows.len().saturating_sub(DIFF_ROWS);
            if hidden > 0 {
                lines.push(
                    Line::default()
                        .dim(format!("    {}", more_lines(hidden)))
                        .truncate(cols),
                );
                let mut text = String::new();
                for (role, row) in diff.rows {
                    text.push_str(match role {
                        Role::Sanguine => "- ",
                        Role::Verdigris => "+ ",
                        _ => "  ",
                    });
                    text.push_str(row);
                    text.push('\n');
                }
                return (
                    lines,
                    vec![Detail {
                        title: self.detail_title(),
                        diff: true,
                        text,
                        error: false,
                        spill: spill.clone(),
                    }],
                );
            }
            return (lines, Vec::new());
        }
        if quiet {
            return (lines, Vec::new());
        }
        let red = if output.is_error && exit.is_none() {
            alarm(&rows)
        } else {
            0..0
        };
        let rows: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(index, line)| {
                !line.trim().is_empty() && exit.is_none_or(|(at, _)| at != *index)
            })
            .map(|(index, line)| (red.contains(&index), *line))
            .collect();
        let shown = &rows[..rows.len().min(RESULT_LINES)];
        for (i, (red, row)) in shown.iter().enumerate() {
            let lead = if i == 0 {
                Line::default().dim("  └ ")
            } else {
                Line::default().ink("    ")
            };
            let body = if *red {
                Line::default().sanguine(row)
            } else {
                Line::default().ink(row)
            };
            lines.push(lead.extend(body).truncate(cols));
        }
        let hidden = rows.len() - shown.len();
        let mut more = Vec::new();
        if hidden > 0 {
            more.push(more_lines(hidden));
        }
        if let Some(path) = spill {
            more.push(format!("full output in {}", path.display()));
        }
        if !more.is_empty() {
            let more = format!("    {}", more.join(" · "));
            lines.push(Line::default().dim(more).truncate(cols));
        }
        let details = if hidden > 0 || exit.is_some() {
            vec![Detail {
                title: self.detail_title(),
                diff: false,
                text,
                error: output.is_error,
                spill: spill.clone(),
            }]
        } else {
            Vec::new()
        };
        (lines, details)
    }
}

impl Transcript {
    /// Asks waiting on you, oldest first.
    #[must_use]
    pub fn asks(&self) -> &[Ask] {
        &self.asks
    }

    /// Whether the model is streaming or calls are in flight.
    #[must_use]
    pub fn thinking(&self) -> bool {
        self.thinking && self.stream.is_empty()
    }

    /// Sets the thinking display for future committed output and the live region.
    pub fn set_thinking(&mut self, thinking: Thinking) {
        self.thinking_display = thinking;
    }

    /// The current thinking display.
    #[must_use]
    pub fn thinking_display(&self) -> Thinking {
        self.thinking_display
    }

    /// Trimmed text of the newest applied assistant message with non-blank text.
    #[must_use]
    pub fn last_text(&self) -> Option<&str> {
        self.last_text.as_deref()
    }

    /// Starts counting a new turn.
    pub fn begin_turn(&mut self) {
        self.turn = Totals::default();
    }

    /// Takes an event from another session. Returns whether that session is
    /// a child of `parent`, the session this transcript draws.
    pub fn child(&mut self, parent: SessionId, params: &EventParams) -> bool {
        self.children.event(
            parent,
            params.session,
            &params.event,
            self.workspace.as_deref(),
        )
    }

    /// Children of the call in flight.
    #[must_use]
    pub fn children(&self) -> &Children {
        &self.children
    }

    fn gap(&mut self, out: &mut Vec<Line>) {
        if !self.blank {
            out.push(Line::default());
            self.blank = true;
        }
    }

    fn push(&mut self, out: &mut Vec<Line>, lines: impl IntoIterator<Item = Line>) {
        for line in lines {
            self.blank = line.0.is_empty();
            out.push(line);
        }
    }

    /// Lines printed outside the event stream, such as the done line.
    pub fn note(&mut self, lines: Vec<Line>) -> Vec<Line> {
        let mut out = Vec::new();
        self.gap(&mut out);
        self.push(&mut out, lines);
        out
    }

    /// Remembers a prompt or steering message until the worker echoes it.
    pub fn sent(&mut self, content: &[Block]) {
        self.sent.push(content.to_vec());
    }

    /// Applies a live notification, distinguishing our own prompt echoes
    /// from messages sent by another client. Snapshot replay uses `apply`.
    pub fn apply_live(&mut self, notification: &ClientNotification, cols: usize) -> Vec<Line> {
        if let ClientNotification::Event(params) = notification
            && let EventKind::UserMessage { content } = &params.event.kind
        {
            if let Some(index) = self.sent.iter().position(|sent| sent == content) {
                self.sent.remove(index);
            } else {
                self.children.clear();
                let mut lines = vec![
                    Line::default()
                        .brass("» ")
                        .dim(format!("from another client · {}", clock(params.event.at)))
                        .truncate(cols),
                ];
                lines.extend(
                    wrap(&blocks_text(content), cols.saturating_sub(2))
                        .into_iter()
                        .map(|row| Line::default().ink("  ").ink(row).truncate(cols)),
                );
                return self.note(lines);
            }
        }
        self.apply(notification, cols)
    }

    /// Applies a notification and returns the lines it commits.
    pub fn apply(&mut self, notification: &ClientNotification, cols: usize) -> Vec<Line> {
        match notification {
            ClientNotification::Reset(_) => {
                self.stream.clear();
                self.thinking = false;
                self.stream_thinking.clear();
                self.redacted_thinking = false;
                Vec::new()
            }
            ClientNotification::Delta(params) => {
                match &params.delta {
                    Delta::Text { text } => self.stream.push_str(text),
                    Delta::Thinking { text } => {
                        self.thinking = true;
                        self.stream_thinking.push_str(text);
                    }
                    Delta::ToolInput { .. } => {}
                }
                Vec::new()
            }
            ClientNotification::Event(params) => {
                self.event(&params.event.kind, params.event.at, cols)
            }
            ClientNotification::ApprovalPending(pending) => {
                let ask = Ask {
                    call: pending.call.clone(),
                    tool: pending.tool.clone(),
                    action: pending.action.clone(),
                    rule: pending.rule.clone(),
                    taint: pending.taint,
                    trusted: Some(pending.trusted),
                };
                if let Some(existing) = self.asks.iter_mut().find(|a| a.call == pending.call) {
                    *existing = ask;
                } else {
                    self.asks.push(ask);
                }
                Vec::new()
            }
            ClientNotification::Exited(params) => {
                self.end_turn();
                self.calls.clear();
                self.children.clear();
                let mut message = "  session exited".to_owned();
                if let Some(status) = params.status {
                    message.push_str(&format!(" · status {status}"));
                }
                if let Some(detail) = &params.message {
                    message.push_str(&format!(" · {detail}"));
                }
                self.note(vec![Line::default().dim(message).truncate(cols)])
            }
        }
    }

    /// Restores in-flight assistant blocks from an attach snapshot.
    pub fn partial(&mut self, blocks: &[Block]) {
        self.stream.clear();
        self.thinking = false;
        self.stream_thinking.clear();
        self.redacted_thinking = false;
        for block in blocks {
            match block {
                Block::Text { text } => self.stream.push_str(text),
                Block::Thinking { text, .. } => {
                    self.thinking = true;
                    if !self.stream_thinking.is_empty() {
                        self.stream_thinking.push('\n');
                    }
                    self.stream_thinking.push_str(text);
                }
                Block::RedactedThinking { .. } => {
                    self.thinking = true;
                    self.redacted_thinking = true;
                }
                _ => {}
            }
        }
    }

    fn event(&mut self, kind: &EventKind, at: Timestamp, cols: usize) -> Vec<Line> {
        let mut out = Vec::new();
        match kind {
            EventKind::SessionStarted { model, cwd, .. } => {
                self.model = Some(model.clone());
                self.workspace = Some(cwd.clone());
            }
            EventKind::ModelChanged { model } => {
                self.model = Some(model.clone());
                self.push(
                    &mut out,
                    [
                        Line::default()
                            .dim(format!("  model → {}/{}", model.provider, model.model)),
                    ],
                );
            }
            // Reasoning changes are reflected in the footer, not the transcript.
            EventKind::ReasoningChanged { .. } => {}
            EventKind::UserMessage { content } => {
                self.children.clear();
                self.gap(&mut out);
                let text = blocks_text(content);
                let rows = wrap(&text, cols.saturating_sub(2));
                let lines = rows.iter().enumerate().map(|(i, row)| {
                    let lead = if i == 0 {
                        Line::default().brass("ξ ")
                    } else {
                        Line::default().ink("  ")
                    };
                    lead.ink(row).truncate(cols)
                });
                self.push(&mut out, lines.collect::<Vec<_>>());
                self.blank = false;
                self.gap(&mut out);
            }
            EventKind::AssistantMessage {
                content,
                usage,
                stop,
                ..
            } => {
                self.stream.clear();
                self.thinking = false;
                self.stream_thinking.clear();
                self.redacted_thinking = false;
                let text = content
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    self.last_text = Some(text.trim().to_owned());
                }
                self.session.add(*usage);
                self.turn.add(*usage);
                let context = usage.input + usage.cache_read + usage.cache_write + usage.output;
                // A cancelled request reports nothing; keep the last real size.
                if context > 0 {
                    self.context = context;
                }
                for block in content {
                    let lines = match block {
                        Block::Thinking { text, .. } => {
                            self.thinking_display.rows(text, cols, false)
                        }
                        Block::RedactedThinking { .. }
                            if self.thinking_display != Thinking::Hidden =>
                        {
                            vec![Line::default().dim("  ∴ redacted thinking").truncate(cols)]
                        }
                        _ => Vec::new(),
                    };
                    if !lines.is_empty() {
                        self.gap(&mut out);
                        self.push(&mut out, lines);
                        // Thinking stands apart from the calls it led to.
                        self.gap(&mut out);
                    }
                }
                for block in content {
                    match block {
                        Block::Text { text } if !text.trim().is_empty() => {
                            self.gap(&mut out);
                            let lines: Vec<_> =
                                prose(text.trim(), cols.saturating_sub(2), Role::Ink)
                                    .into_iter()
                                    .map(|row| Line::default().ink("  ").extend(row))
                                    .collect();
                            self.push(&mut out, lines);
                            self.blank = false;
                            self.gap(&mut out);
                        }
                        Block::ToolUse {
                            id, name, input, ..
                        } => self.calls.push(Call {
                            id: id.clone(),
                            name: name.clone(),
                            arg: headline_arg(name, input, self.workspace.as_deref()),
                            input: input.clone(),
                            since: Some(Instant::now()),
                            decided_at: None,
                            decision: None,
                            rule: None,
                            approved: None,
                            remember: Remember::Once,
                            result: None,
                            children: Children::default(),
                        }),
                        _ => {}
                    }
                }
                let note = match stop {
                    StopReason::MaxTokens => Some("stopped at the output token limit"),
                    StopReason::Refusal => Some("the model declined to answer"),
                    _ => None,
                };
                if let Some(note) = note {
                    self.push(&mut out, [Line::default().dim(format!("  {note}"))]);
                }
            }
            EventKind::ToolDecision {
                call,
                action,
                decision,
                rule,
                taint,
            } => {
                if let Some(c) = self.call(call) {
                    c.decision = Some(*decision);
                    c.rule.clone_from(rule);
                    c.decided_at = Some(at);
                    c.since = Some(Instant::now());
                    if *decision == Decision::Ask {
                        let ask = Ask {
                            call: call.clone(),
                            tool: c.name.clone(),
                            action: action.clone(),
                            rule: rule.clone(),
                            taint: *taint,
                            trusted: None,
                        };
                        if let Some(pending) = self.asks.iter_mut().find(|a| a.call == *call) {
                            *pending = ask;
                        } else {
                            self.asks.push(ask);
                        }
                    }
                }
            }
            EventKind::ApprovalResolved {
                call,
                approved,
                remember,
                ..
            } => {
                self.asks.retain(|a| &a.call != call);
                if let Some(c) = self.call(call) {
                    c.approved = Some(*approved);
                    c.remember = *remember;
                    c.since = Some(Instant::now());
                }
            }
            EventKind::ToolResult {
                call,
                output,
                spill,
            } => {
                self.asks.retain(|a| &a.call != call);
                if let Some(c) = self.calls.iter_mut().find(|c| &c.id == call) {
                    c.result = Some((output.clone(), spill.clone(), at));
                    // Calls run one at a time, so the children started
                    // since the last result are this call's.
                    c.children = std::mem::take(&mut self.children);
                }
                let done = self.calls.iter().take_while(|c| c.result.is_some()).count();
                let mut lines = Vec::new();
                for call in self.calls.drain(..done) {
                    let (block, details) = call.committed(cols);
                    lines.extend(block);
                    if !details.is_empty() {
                        self.details.extend(details);
                        self.expanded = 0;
                    }
                }
                self.push(&mut out, lines);
            }
            EventKind::Tainted { reason, .. } => {
                self.tainted = true;
                self.push(
                    &mut out,
                    [Line::default()
                        .murex("◆ tainted")
                        .dim(format!(" · {reason}. Egress on this branch now asks."))
                        .truncate(cols)],
                );
            }
            EventKind::Compaction {
                summary,
                tokens_before,
                usage,
                ..
            } => {
                self.children.clear();
                if let Some(usage) = usage {
                    self.session.add(*usage);
                    self.turn.add(*usage);
                }
                self.gap(&mut out);
                let head = Line::default()
                    .dim("─── ")
                    .strong(Role::Ink, "≡ compacted")
                    .dim(format!(" {} tokens before ", tokens(*tokens_before)));
                let fill = cols.saturating_sub(head.width());
                let mut lines = vec![head.dim("─".repeat(fill)).truncate(cols)];
                let (rows, hidden) = digest(summary);
                for (label, text) in rows {
                    let lead = if label.is_empty() {
                        "  ".to_owned()
                    } else {
                        format!("  {label:<9} ")
                    };
                    lines.push(Line::default().dim(lead).ink(text).truncate(cols));
                }
                if hidden > 0 {
                    lines.push(
                        Line::default()
                            .dim(format!(
                                "  … {} of summary · ctrl+o",
                                counted(hidden, "more line", "more lines")
                            ))
                            .truncate(cols),
                    );
                    self.details.push(Detail {
                        title: "≡ compacted · summary".to_owned(),
                        diff: false,
                        text: summary.trim().to_owned(),
                        error: false,
                        spill: None,
                    });
                    self.expanded = 0;
                }
                lines.push(crate::style::rule(cols));
                self.push(&mut out, lines);
                self.gap(&mut out);
            }
            EventKind::TurnFailed { message } => {
                self.push(
                    &mut out,
                    [Line::default().sanguine("✕ ").ink(message).truncate(cols)],
                );
            }
            EventKind::Custom { .. } => {}
        }
        out
    }

    fn call(&mut self, id: &ToolCallId) -> Option<&mut Call> {
        self.calls.iter_mut().find(|c| &c.id == id)
    }

    /// The whole result of the newest collapsed call not yet expanded, as
    /// lines to commit. Each call steps one result further back; past the
    /// oldest it returns `None` and starts again from the newest.
    pub fn expand(&mut self, cols: usize) -> Option<Vec<Line>> {
        let Some(at) = self.details.len().checked_sub(self.expanded + 1) else {
            self.expanded = 0;
            return None;
        };
        self.expanded += 1;
        let detail = &self.details[at];
        let count = detail.text.lines().count();
        let mut lines = vec![detail_rule(&detail.title, cols)];
        // Screen only rewinds live rows. Committed rows can soft-wrap safely,
        // keeping the original output lines intact when copied.
        let rows: Vec<&str> = detail.text.lines().collect();
        let red = if detail.error { alarm(&rows) } else { 0..0 };
        for (index, row) in rows.iter().enumerate() {
            let role = if detail.diff && row.starts_with("- ") || red.contains(&index) {
                Role::Sanguine
            } else if detail.diff && row.starts_with("+ ") {
                Role::Verdigris
            } else if detail.diff {
                Role::Dim
            } else {
                Role::Ink
            };
            lines.push(Line::default().span(role, false, row));
        }
        lines.push(detail_rule(
            &format!("end · {}", counted(count, "line", "lines")),
            cols,
        ));
        if let Some(path) = &detail.spill {
            lines.push(
                Line::default()
                    .dim(format!("full output in {}", path.display()))
                    .truncate(cols),
            );
        }
        Some(self.note(lines))
    }

    /// Whether any result was cut short.
    #[must_use]
    pub fn collapsed(&self) -> bool {
        !self.details.is_empty()
    }

    /// Moves collapsed results over from `other`, whose scrollback they are.
    pub fn keep_details(&mut self, other: &mut Self) {
        self.details = std::mem::take(&mut other.details);
        self.expanded = 0;
    }

    /// Clears what a cancelled or failed turn left in flight.
    pub fn end_turn(&mut self) {
        self.stream.clear();
        self.thinking = false;
        self.stream_thinking.clear();
        self.redacted_thinking = false;
        self.asks.clear();
    }

    /// Calls still in flight, then the newest streamed rows.
    #[must_use]
    pub fn live(&self, cols: usize, frame: usize) -> Vec<Line> {
        self.live_rows(cols, frame, false)
    }

    /// [`Self::live`] while a frame holds the keys: streamed text shrinks
    /// to one `┤ streaming · 34 lines` row and thinking to its first line,
    /// so the frame keeps its room. Calls, asks and children stay.
    #[must_use]
    pub fn live_under_frame(&self, cols: usize, frame: usize) -> Vec<Line> {
        self.live_rows(cols, frame, true)
    }

    fn live_rows(&self, cols: usize, frame: usize, collapse: bool) -> Vec<Line> {
        let mut lines = Vec::new();
        if !self.stream.trim().is_empty() {
            let rows = prose(self.stream.trim(), cols.saturating_sub(2), Role::Ink);
            if collapse {
                let noun = if rows.len() == 1 { "line" } else { "lines" };
                lines.push(
                    Line::default()
                        .brass(SPINNER[frame % SPINNER.len()])
                        .dim(format!(" streaming · {} {noun}", rows.len()))
                        .truncate(cols),
                );
            } else {
                let skip = rows.len().saturating_sub(STREAM_ROWS);
                for row in rows.into_iter().skip(skip) {
                    lines.push(Line::default().ink("  ").extend(row));
                }
                lines.push(Line::default());
            }
        }
        if self.thinking() {
            let display = match self.thinking_display {
                Thinking::Full if collapse => Thinking::First,
                display => display,
            };
            lines.extend(display.rows(&self.stream_thinking, cols, true));
            if self.redacted_thinking && display != Thinking::Hidden {
                lines.push(Line::default().dim("  ∴ redacted thinking").truncate(cols));
            }
        }
        let mut roster = (!self.children.is_empty()).then_some(&self.children);
        for call in &self.calls {
            let waiting = call.decision == Some(Decision::Ask) && call.approved.is_none();
            let (mark, meta) = if waiting {
                (
                    Line::default().brass("?"),
                    Line::default().brass("waiting on you"),
                )
            } else if call.result.is_some() {
                (call.mark(), Line::default().dim("done"))
            } else {
                let elapsed = call.since.map_or(0, |s| {
                    u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
                });
                let mut meta = match call.rule.as_ref().filter(|r| !r.starts_with("builtin#")) {
                    Some(rule) if rule == "session" => {
                        format!("allowed for this session · {}", duration(elapsed))
                    }
                    Some(rule) => format!("{rule} · {}", duration(elapsed)),
                    None => duration(elapsed),
                };
                if let Some(children) = roster {
                    meta = format!("↑ {} · {meta}", tokens(children.sent()));
                }
                (
                    Line::default().brass(SPINNER[frame % SPINNER.len()]),
                    Line::default().dim(meta),
                )
            };
            lines.push(lr(call.head(mark), meta, cols));
            if call.result.is_none()
                && !waiting
                && let Some(children) = roster.take()
            {
                lines.extend(children.live(cols, frame));
            }
        }
        lines
    }
}

/// Splits a summary line into a heading and any text after it on the same
/// line. Models write the kernel's headings as `## Decisions`, `**Decisions**`
/// or `- **Decisions**: text`.
fn heading(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix('#') {
        return Some((rest.trim_start_matches('#').trim(), ""));
    }
    let item = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line);
    let (name, rest) = item.strip_prefix("**")?.split_once("**")?;
    Some((
        name.trim_end_matches(':').trim(),
        rest.trim_start_matches(':').trim(),
    ))
}

/// Rows that stand for a compaction summary: the first entry under each of
/// the kernel's headings that says where the work stands, with a short
/// label, and how many lines the digest leaves out. A summary without those
/// headings, such as one an extension wrote, shows its first lines.
fn digest(summary: &str) -> (Vec<(&'static str, String)>, usize) {
    const LABELS: [(&str, &str); 6] = [
        ("user goal", "goal"),
        ("decision", "decided"),
        ("files", "touched"),
        ("current state", "state"),
        ("open", "open"),
        ("next", "next"),
    ];
    const ROWS: usize = 5;
    let lines: Vec<&str> = summary
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut rows = Vec::new();
    let mut label = None;
    for line in &lines {
        // A heading is a `#` line or a bold lead naming one of the kernel's
        // sections; bold words inside an entry are not.
        let found = heading(line).and_then(|(name, rest)| {
            let lower = name.to_lowercase();
            let known = LABELS
                .iter()
                .find(|(prefix, _)| lower.starts_with(prefix))
                .map(|(_, l)| *l);
            (line.starts_with('#') || known.is_some()).then_some((known, rest))
        });
        let text = match found {
            Some((known, rest)) => {
                label = known;
                if rest.is_empty() {
                    continue;
                }
                rest
            }
            None => line
                .strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .unwrap_or(line),
        };
        if let Some(l) = label.take()
            && rows.len() < ROWS
        {
            rows.push((l, text.to_owned()));
        }
    }
    if rows.is_empty() {
        rows = lines
            .iter()
            .take(3)
            .map(|l| ("", (*l).to_owned()))
            .collect();
    }
    let hidden = lines.len().saturating_sub(rows.len());
    (rows, hidden)
}

pub(crate) fn blocks_text(content: &[Block]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            Block::Image { .. } => Some("[image]"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use xi_client::client::ClientNotification;
    use xi_protocol::methods::client::EventParams;
    use xi_protocol::{Event, EventId, SessionId};

    use super::*;

    fn ev(kind: EventKind, at: u64) -> ClientNotification {
        ClientNotification::Event(EventParams {
            session: SessionId::generate(),
            event: Event {
                id: EventId::generate(),
                parent: None,
                at: Timestamp(at),
                kind,
            },
        })
    }

    fn assistant(content: Vec<Block>) -> EventKind {
        EventKind::AssistantMessage {
            model: ModelRef {
                provider: "anthropic".into(),
                model: "m".into(),
            },
            content,
            stop: StopReason::ToolUse,
            usage: Usage {
                input: 1200,
                output: 80,
                cache_read: 14_000,
                cache_write: 0,
            },
        }
    }

    fn use_(id: &str, name: &str, input: Value) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
            extra: Default::default(),
        }
    }

    fn result(id: &str, text: &str, is_error: bool) -> EventKind {
        EventKind::ToolResult {
            call: id.into(),
            output: ToolOutput {
                content: vec![OutputBlock::Text { text: text.into() }],
                is_error,
                untrusted: false,
            },
            spill: None,
        }
    }

    fn decide(id: &str, decision: Decision, rule: Option<&str>) -> EventKind {
        EventKind::ToolDecision {
            call: id.into(),
            action: "exec pytest".into(),
            decision,
            rule: rule.map(Into::into),
            taint: false,
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    #[test]
    fn reasoning_changes_do_not_add_transcript_lines() {
        let mut t = Transcript::default();
        let rows = t.apply(
            &ev(
                EventKind::ReasoningChanged {
                    reasoning: xi_protocol::model::Reasoning::XHigh,
                },
                0,
            ),
            80,
        );
        assert!(rows.is_empty());
        assert!(t.live(80, 0).is_empty());
    }

    #[test]
    fn calls_commit_in_call_order_when_their_result_lands() {
        let mut t = Transcript::default();
        let msg = assistant(vec![
            Block::Text {
                text: "I'll run the tests first.".into(),
            },
            use_("a", "bash", json!({"command": "pytest -x\necho"})),
            use_("b", "read", json!({"path": "policies/archive.py"})),
        ]);
        let prose = t.apply(&ev(msg, 0), 60);
        assert_eq!(plain(&prose), ["", "  I'll run the tests first.", ""]);
        t.apply(&ev(decide("a", Decision::Allow, Some("user#1")), 1000), 60);
        t.apply(&ev(decide("b", Decision::Allow, None), 1000), 60);
        assert!(
            t.apply(
                &ev(
                    result("b", "1|x\n2|y\n[More lines. Continue with read.]", false),
                    1100
                ),
                60
            )
            .is_empty()
        );
        let live = plain(&t.live(60, 0));
        assert!(live[0].starts_with("┤ bash pytest -x"), "{live:?}");
        let rows = t.apply(
            &ev(result("a", "FAILED test_x\nassert\nmore\nmore", true), 2800),
            60,
        );
        let rows = plain(&rows);
        assert!(rows[0].starts_with("✕ bash pytest -x"), "{rows:?}");
        assert!(rows[0].ends_with("user#1 · 1.8s"), "{rows:?}");
        assert_eq!(rows[1], "  └ FAILED test_x");
        assert_eq!(rows[4], "    … 1 more line · ctrl+o");
        assert!(rows[5].starts_with("● read policies/archive.py"));
        assert!(rows[5].ends_with("2 lines · 0.1s"));
        assert!(t.live(60, 0).is_empty());
        assert_eq!(t.context, 15_280);
    }

    #[test]
    fn failures_redden_the_error_not_the_output_above_it() {
        let mut t = Transcript::default();
        let calls = assistant(vec![
            use_("a", "bash", json!({"command": "pwd; find .."})),
            use_("b", "read", json!({"path": "gone"})),
            use_(
                "c",
                "read",
                json!({"path": "src", "offset": 40, "limit": 10}),
            ),
        ]);
        t.apply(&ev(calls, 0), 60);
        let mut rows = t.apply(
            &ev(result("a", "/w\nfind: ..: denied\n[exit 1]", true), 0),
            60,
        );
        rows.extend(t.apply(
            &ev(result("b", "Cannot read gone\nNo such file", true), 0),
            60,
        ));
        rows.extend(t.apply(&ev(result("c", "lib.rs\nmod/", false), 0), 60));
        let red = |line: &Line| line.0.iter().any(|s| s.role == Role::Sanguine);
        let reds: Vec<bool> = rows.iter().map(red).collect();
        assert_eq!(plain(&rows[1..3]), ["  └ /w", "    find: ..: denied"]);
        assert!(rows[0].plain().ends_with("exit 1"));
        assert_eq!(reds[1..3], [false, false]);
        assert_eq!(reds[4..6], [true, false], "{:?}", plain(&rows));
        assert!(plain(&rows)[6].starts_with("● read src:40-49"));
        assert!(plain(&rows)[6].ends_with("2 entries"));
    }

    #[test]
    fn asks_wait_on_you_until_resolved_anywhere() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "bash", json!({"command": "curl x"}))]),
                0,
            ),
            60,
        );
        t.apply(
            &ev(
                EventKind::Tainted {
                    call: None,
                    reason: "untrusted output from web".into(),
                },
                0,
            ),
            60,
        );
        t.apply(&ev(decide("a", Decision::Ask, Some("user#2")), 0), 60);
        assert_eq!(t.asks().len(), 1);
        assert!(!t.asks()[0].taint);
        assert_eq!(t.asks()[0].trusted, None);
        assert!(plain(&t.live(60, 0))[0].ends_with("waiting on you"));
        t.apply(
            &ev(
                EventKind::ApprovalResolved {
                    call: "a".into(),
                    approved: false,
                    by: xi_protocol::event::Approver::User,
                    remember: Remember::Once,
                },
                0,
            ),
            60,
        );
        assert!(t.asks().is_empty());
        let rows = plain(&t.apply(&ev(result("a", "denied by policy (user#2)", true), 0), 60));
        assert!(
            rows[0].ends_with("denied by you · user#2 · 0.0s"),
            "{rows:?}"
        );
    }

    #[test]
    fn stale_anchors_are_a_retry_not_a_failure() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "edit", json!({"path": "deleted.py"}))]),
                0,
            ),
            60,
        );
        let rows = t.apply(
            &ev(result("a", "Stale anchors. No changes written.", true), 0),
            60,
        );
        assert!(rows[0].plain().starts_with("↺ edit deleted.py"));
    }

    #[test]
    fn ctrl_o_expands_cut_results_newest_first_then_starts_over() {
        let mut t = Transcript::default();
        assert!(t.expand(60).is_none());
        t.apply(
            &ev(
                assistant(vec![
                    use_("a", "bash", json!({"command": "make"})),
                    use_("b", "bash", json!({"command": "ls"})),
                    use_("c", "bash", json!({"command": "make test"})),
                ]),
                0,
            ),
            60,
        );
        t.apply(&ev(result("a", "1\n2\n3\n\n4", false), 0), 60);
        t.apply(&ev(result("b", "short", false), 0), 60);
        t.apply(
            &ev(result("c", "\x1b[31mFAIL\x1b[0m\na\nb\nc", true), 0),
            60,
        );
        assert!(t.collapsed());
        let newest = plain(&t.expand(60).unwrap());
        assert_eq!(newest[1], "── bash · make test ──");
        assert_eq!(newest[2..], ["FAIL", "a", "b", "c", "── end · 4 lines ──"]);
        let older = plain(&t.expand(60).unwrap());
        assert_eq!(older[1], "── bash · make ──");
        assert_eq!(older[2..], ["1", "2", "3", "", "4", "── end · 5 lines ──"]);
        assert!(t.expand(60).is_none());
        assert_eq!(plain(&t.expand(60).unwrap())[1], "── bash · make test ──");
    }

    #[test]
    fn headlines_drop_the_workspace_but_keep_paths_outside_it() {
        let w = Some(std::path::Path::new("/tmp/ranges"));
        let arg = |name, input| headline_arg(name, &input, w);
        assert_eq!(
            arg("read", json!({"path": "/tmp/ranges/src/lib.rs"})),
            "src/lib.rs"
        );
        assert_eq!(
            arg("read", json!({"path": "/tmp/ranges-old/x"})),
            "/tmp/ranges-old/x"
        );
        assert_eq!(arg("read", json!({"path": "/etc/hosts"})), "/etc/hosts");
        assert_eq!(
            arg(
                "read",
                json!({"path": "/tmp/ranges/a", "offset": 120, "limit": 46})
            ),
            "a:120-165"
        );
        assert_eq!(arg("read", json!({"path": "a", "offset": 120})), "a:120-");
        assert_eq!(arg("read", json!({"path": "a", "limit": 40})), "a:1-40");
        assert_eq!(
            arg(
                "bash",
                json!({"command": "cd /tmp/ranges && cargo test\necho"})
            ),
            "cargo test"
        );
        assert_eq!(
            arg("bash", json!({"command": "cd /tmp && ls"})),
            "cd /tmp && ls"
        );
        assert_eq!(
            arg("search", json!({"pattern": "fn ", "path": "/tmp/ranges"})),
            "fn "
        );
        assert_eq!(
            arg(
                "search",
                json!({"pattern": "fn ", "path": "/tmp/ranges/src"})
            ),
            "fn  in src"
        );
    }

    #[test]
    fn compaction_commits_a_digest_and_keeps_the_summary_for_ctrl_o() {
        let summary = "## User goal and constraints\n- Fix covered_length.\n- Keep tests.\n\n## Decisions\n- Skip empty ranges.\n\n## Commands run and results\n- cargo test: ok\n\n## Open questions\n- None.\n";
        let mut t = Transcript::default();
        let rows = plain(&t.apply(
            &ev(
                EventKind::Compaction {
                    summary: summary.into(),
                    tokens_before: 30_700,
                    kept: None,
                    usage: None,
                },
                0,
            ),
            60,
        ));
        assert!(
            rows[1].starts_with("─── ≡ compacted 30.7k tokens before ─"),
            "{rows:?}"
        );
        assert_eq!(
            rows[2..6],
            [
                "  goal      Fix covered_length.",
                "  decided   Skip empty ranges.",
                "  open      None.",
                "  … 6 more lines of summary · ctrl+o",
            ]
        );
        let full = plain(&t.expand(60).unwrap());
        // The divider already ended in a blank row, so no gap comes first.
        assert_eq!(full[0], "── ≡ compacted · summary ──");
        assert_eq!(full[1], "## User goal and constraints");
        assert_eq!(full.last().unwrap(), "── end · 12 lines ──");
    }

    #[test]
    fn digest_reads_bold_lead_headings_and_ignores_bold_inside_entries() {
        let summary = "- **User goal and constraints**: Fix it.\n\n- **Decisions**:\n  - **Kept** the API.\n\n- **Files read or changed, with paths and key facts**:\n  - `src/lib.rs`: edited.\n\n- **Commands run and results**:\n  - `cargo test`: ok\n\n- **Current state**: All tests pass.\n";
        let (rows, hidden) = digest(summary);
        assert_eq!(
            rows,
            [
                ("goal", "Fix it.".to_owned()),
                ("decided", "**Kept** the API.".to_owned()),
                ("touched", "`src/lib.rs`: edited.".to_owned()),
                ("state", "All tests pass.".to_owned()),
            ]
        );
        assert_eq!(hidden, 4);
        let (rows, _) = digest("An extension's own summary.\nSecond line.");
        assert_eq!(rows[0], ("", "An extension's own summary.".to_owned()));
    }

    #[test]
    fn thinking_modes_commit_before_text_and_preserve_display_selection() {
        for mode in [Thinking::Full, Thinking::First, Thinking::Hidden] {
            let mut t = Transcript::default();
            t.set_thinking(mode);
            assert_eq!(t.thinking_display(), mode);
            let rows = t.apply(
                &ev(
                    assistant(vec![
                        Block::Text {
                            text: "answer".into(),
                        },
                        Block::Thinking {
                            text: "first thought\nsecond thought".into(),
                            signature: None,
                        },
                    ]),
                    0,
                ),
                60,
            );
            let text = plain(&rows);
            let answer = text.iter().position(|line| line == "  answer").unwrap();
            match mode {
                Thinking::Full => {
                    let first = text
                        .iter()
                        .position(|line| line == "  ∴ first thought")
                        .unwrap();
                    assert!(first < answer);
                    assert_eq!(text[first + 1], "    second thought");
                    assert!(rows[first].0.iter().all(|span| span.role == Role::Dim));
                    assert!(rows[first + 1].0.iter().all(|span| span.role == Role::Dim));
                }
                Thinking::First => {
                    let first = text
                        .iter()
                        .position(|line| line == "  ∴ first thought · ctrl+t")
                        .unwrap();
                    assert!(first < answer);
                    assert!(text.iter().all(|line| !line.contains("second thought")));
                }
                Thinking::Hidden => assert!(text.iter().all(|line| !line.contains("thought"))),
            }
        }
        assert_eq!(Thinking::Full.next(), Thinking::First);
        assert_eq!(Thinking::First.next(), Thinking::Hidden);
        assert_eq!(Thinking::Hidden.next(), Thinking::Full);
        assert_eq!(Thinking::Full.name(), "full");
        assert_eq!(Thinking::First.name(), "first line");
        assert_eq!(Thinking::Hidden.name(), "hidden");
    }

    #[test]
    fn committed_thinking_wraps_or_truncates_and_redacted_respects_modes() {
        let text = "abcdefghijklmnopqrstuvwxyz\nsecond";
        let mut t = Transcript::default();
        t.set_thinking(Thinking::Full);
        let rows = t.apply(
            &ev(
                assistant(vec![Block::Thinking {
                    text: text.into(),
                    signature: None,
                }]),
                0,
            ),
            16,
        );
        assert!(rows.iter().all(|line| line.width() <= 16));
        assert!(rows.len() > 3);
        t.set_thinking(Thinking::First);
        let rows = t.apply(
            &ev(
                assistant(vec![Block::Thinking {
                    text: format!("\n{text}"),
                    signature: None,
                }]),
                0,
            ),
            24,
        );
        assert!(rows.iter().all(|line| line.width() <= 24));
        assert!(plain(&rows).iter().any(|line| line.ends_with(" · ctrl+t")));
        assert!(plain(&rows).iter().all(|line| !line.contains("second")));
        for mode in [Thinking::Full, Thinking::First, Thinking::Hidden] {
            t.set_thinking(mode);
            let rows = t.apply(
                &ev(
                    assistant(vec![Block::RedactedThinking {
                        data: "opaque".into(),
                    }]),
                    0,
                ),
                60,
            );
            assert_eq!(
                plain(&rows)
                    .iter()
                    .any(|line| line == "  ∴ redacted thinking"),
                mode != Thinking::Hidden,
            );
        }
    }

    #[test]
    fn under_a_frame_stream_counts_lines_and_full_thinking_shows_one() {
        use xi_protocol::methods::client::DeltaParams;

        let mut t = Transcript::default();
        t.set_thinking(Thinking::Full);
        let session = SessionId::generate();
        let delta = |delta| {
            ClientNotification::Delta(DeltaParams {
                session,
                attempt: 0,
                sequence: 0,
                delta,
            })
        };
        t.apply(
            &delta(Delta::Thinking {
                text: "one\ntwo\nthree\n".into(),
            }),
            60,
        );
        assert!(t.live(60, 0).len() > 1);
        assert_eq!(plain(&t.live_under_frame(60, 0)), ["  ∴ three"]);
        let text: String = (1..=12).map(|n| format!("line {n}\n\n")).collect();
        t.apply(&delta(Delta::Text { text }), 60);
        assert_eq!(t.live(60, 0).len(), STREAM_ROWS + 1);
        assert_eq!(
            plain(&t.live_under_frame(60, 0)),
            ["┤ streaming · 23 lines"]
        );
    }

    #[test]
    fn live_thinking_keeps_deltas_and_switches_modes_without_losing_text() {
        use xi_protocol::methods::client::{DeltaParams, ResetParams};

        let mut t = Transcript::default();
        let session = SessionId::generate();
        for text in ["one\ntwo\nthree\nfour\n", "five\nsix\nseven\neight\n\n"] {
            t.apply(
                &ClientNotification::Delta(DeltaParams {
                    session,
                    attempt: 0,
                    sequence: 0,
                    delta: Delta::Thinking { text: text.into() },
                }),
                60,
            );
        }
        assert!(t.thinking());
        assert_eq!(plain(&t.live(60, 0)), ["  ∴ eight"]);
        t.set_thinking(Thinking::Full);
        let full = t.live(60, 0);
        assert_eq!(full.len(), STREAM_ROWS);
        assert!(
            full.iter()
                .all(|line| line.0.iter().all(|span| span.role == Role::Dim))
        );
        assert!(
            plain(&full)
                .iter()
                .all(|line| !line.contains("one") && !line.contains("two"))
        );
        assert!(plain(&full).iter().any(|line| line.contains("eight")));
        t.set_thinking(Thinking::Hidden);
        assert!(t.live(60, 0).is_empty());
        assert!(t.thinking());
        t.set_thinking(Thinking::First);
        assert_eq!(plain(&t.live(60, 0)), ["  ∴ eight"]);
        t.apply(
            &ClientNotification::Delta(DeltaParams {
                session,
                attempt: 0,
                sequence: 0,
                delta: Delta::Text {
                    text: "answer starts".into(),
                },
            }),
            60,
        );
        assert!(!t.thinking());
        assert!(
            plain(&t.live(60, 0))
                .iter()
                .all(|line| !line.contains("eight"))
        );
        t.apply(
            &ClientNotification::Reset(ResetParams {
                session,
                attempt: 1,
                sequence: 0,
            }),
            60,
        );
        assert!(t.live(60, 0).is_empty());
        t.partial(&[Block::Thinking {
            text: "restored thought".into(),
            signature: None,
        }]);
        assert_eq!(plain(&t.live(60, 0)), ["  ∴ restored thought"]);
        t.apply(&ev(assistant(Vec::new()), 0), 60);
        assert!(t.live(60, 0).is_empty());
        assert!(!t.thinking());
    }

    #[test]
    fn successful_edit_shows_changed_rows_counts_and_one_context_line_each_side() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_(
                    "edit",
                    "edit",
                    json!({
                        "path": "source.rs",
                        "old": "skip before\nbefore\nold one\nold two\nafter\nskip after\n",
                        "new": "skip before\nbefore\nnew one\nafter\nskip after\n",
                    }),
                )]),
                0,
            ),
            100,
        );
        t.apply(&ev(decide("edit", Decision::Allow, Some("user#1")), 0), 100);
        let rows = t.apply(&ev(result("edit", "replaced text", false), 100), 100);
        let text = plain(&rows);
        assert!(text[0].starts_with("● edit source.rs"));
        assert!(text[0].ends_with("+1 −2 · user#1 · 0.1s"), "{text:?}");
        assert_eq!(
            text[1..],
            [
                "    before",
                "  - old one",
                "  - old two",
                "  + new one",
                "    after"
            ]
        );
        assert!(rows[1].0.iter().all(|span| span.role == Role::Dim));
        assert!(rows[2].0.iter().all(|span| span.role == Role::Sanguine));
        assert!(rows[4].0.iter().all(|span| span.role == Role::Verdigris));
        assert!(!t.collapsed());
    }

    #[test]
    fn edit_diff_caps_eight_rows_and_expands_the_whole_diff() {
        let mut t = Transcript::default();
        let old = (0..6)
            .map(|n| format!("old {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..6)
            .map(|n| format!("new {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        t.apply(
            &ev(
                assistant(vec![use_(
                    "edit",
                    "edit",
                    json!({
                        "path": "source.rs", "old": old, "new": new,
                    }),
                )]),
                0,
            ),
            80,
        );
        let rows = plain(&t.apply(&ev(result("edit", "replaced", false), 0), 80));
        assert_eq!(rows.len(), 10);
        assert!(rows[0].ends_with("+6 −6"));
        assert_eq!(rows[8], "  + new 1");
        assert_eq!(rows[9], "    … 4 more lines · ctrl+o");
        assert!(t.details[0].diff);
        assert!(t.details[0].text.starts_with("- old 0\n"));
        let full = t.expand(80).unwrap();
        let text = plain(&full);
        assert!(text.iter().any(|line| line == "── end · 12 lines ──"));
        assert!(text.iter().any(|line| line == "+ new 5"));
        assert_eq!(text.iter().filter(|line| line.starts_with("- ")).count(), 6);
        assert_eq!(text.iter().filter(|line| line.starts_with("+ ")).count(), 6);
        assert!(
            full.iter()
                .filter(|line| line.plain().starts_with("- "))
                .all(|line| line.0.iter().all(|span| span.role == Role::Sanguine))
        );
        assert!(
            full.iter()
                .filter(|line| line.plain().starts_with("+ "))
                .all(|line| line.0.iter().all(|span| span.role == Role::Verdigris))
        );
    }

    #[test]
    fn diff_rows_truncate_and_failed_edits_keep_error_output() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_(
                    "edit",
                    "edit",
                    json!({
                        "path": "x", "old": "a long removed line", "new": "a long added line",
                    }),
                )]),
                0,
            ),
            12,
        );
        let rows = t.apply(&ev(result("edit", "replaced", false), 0), 12);
        assert!(rows.iter().all(|line| line.width() <= 12));
        assert!(rows[1].plain().ends_with('…'));
        t.apply(
            &ev(
                assistant(vec![use_(
                    "error",
                    "edit",
                    json!({
                        "path": "x", "old": "old", "new": "new",
                    }),
                )]),
                0,
            ),
            80,
        );
        let rows = plain(&t.apply(&ev(result("error", "old text did not match", true), 0), 80));
        assert!(rows[0].starts_with("✕ edit x"));
        assert!(!rows[0].contains("+1"));
        assert_eq!(rows[1], "  └ old text did not match");
    }

    #[test]
    fn hashline_renders_each_edits_lines_as_additions() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_(
                    "hash",
                    "hashline",
                    json!({
                        "path": "source.rs",
                        "edits": [
                            { "start": "1:aa", "lines": ["one", "two"] },
                            { "after": "2:bb", "lines": ["three"] },
                        ],
                    }),
                )]),
                0,
            ),
            80,
        );
        let rows = t.apply(&ev(result("hash", "updated", false), 0), 80);
        let text = plain(&rows);
        assert!(text[0].ends_with("+3"));
        assert_eq!(text[1..], ["  + one", "  + two", "  + three"]);
        assert!(
            rows[1..]
                .iter()
                .all(|line| line.0.iter().all(|span| span.role == Role::Verdigris))
        );
    }

    #[test]
    fn resolved_ask_meta_names_each_scope_and_denial() {
        for (approved, remember, expected) in [
            (true, Remember::Once, "allowed by you"),
            (true, Remember::Session, "allowed for this session"),
            (true, Remember::Workspace, "always allowed here"),
            (false, Remember::Once, "denied by you"),
        ] {
            let mut t = Transcript::default();
            t.apply(
                &ev(
                    assistant(vec![use_("a", "bash", json!({"command": "test"}))]),
                    0,
                ),
                100,
            );
            t.apply(&ev(decide("a", Decision::Ask, None), 0), 100);
            t.apply(
                &ev(
                    EventKind::ApprovalResolved {
                        call: "a".into(),
                        approved,
                        by: xi_protocol::event::Approver::User,
                        remember,
                    },
                    0,
                ),
                100,
            );
            let rows = plain(&t.apply(&ev(result("a", "done", !approved), 0), 100));
            assert!(rows[0].contains(expected), "{rows:?}");
        }
    }

    #[test]
    fn session_rule_is_described_in_live_and_committed_meta() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "bash", json!({"command": "test"}))]),
                0,
            ),
            100,
        );
        t.apply(&ev(decide("a", Decision::Allow, Some("session")), 0), 100);
        assert!(plain(&t.live(100, 0))[0].contains("allowed for this session"));
        let rows = plain(&t.apply(&ev(result("a", "done", false), 0), 100));
        assert!(rows[0].ends_with("allowed for this session · 0.0s"));
    }

    #[test]
    fn asks_take_taint_from_decision_and_trust_from_notifications() {
        use xi_protocol::methods::client::PendingApproval;

        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "bash", json!({"command": "test"}))]),
                0,
            ),
            80,
        );
        t.apply(
            &ev(
                EventKind::ToolDecision {
                    call: "a".into(),
                    action: "exec test".into(),
                    decision: Decision::Ask,
                    rule: None,
                    taint: true,
                },
                0,
            ),
            80,
        );
        assert!(t.asks()[0].taint);
        assert_eq!(t.asks()[0].trusted, None);
        let pending = ClientNotification::ApprovalPending(PendingApproval {
            session: SessionId::generate(),
            call: "a".into(),
            tool: "bash".into(),
            action: "exec test".into(),
            rule: None,
            taint: true,
            trusted: false,
        });
        t.apply(&pending, 80);
        assert_eq!(t.asks().len(), 1);
        assert_eq!(t.asks()[0].trusted, Some(false));
    }

    #[test]
    fn last_text_is_the_latest_nonblank_assistant_text_not_stream_or_tool_output() {
        use xi_protocol::methods::client::DeltaParams;

        let mut t = Transcript::default();
        assert_eq!(t.last_text(), None);
        t.apply(
            &ev(
                assistant(vec![Block::Text {
                    text: "  first response  ".into(),
                }]),
                0,
            ),
            80,
        );
        assert_eq!(t.last_text(), Some("first response"));
        t.apply(
            &ev(
                assistant(vec![
                    Block::Text { text: " \n".into() },
                    Block::Thinking {
                        text: "not an answer".into(),
                        signature: None,
                    },
                    use_("a", "bash", json!({"command": "test"})),
                ]),
                0,
            ),
            80,
        );
        t.apply(&ev(result("a", "tool output", false), 0), 80);
        t.apply(
            &ClientNotification::Delta(DeltaParams {
                session: SessionId::generate(),
                attempt: 0,
                sequence: 0,
                delta: Delta::Text {
                    text: "unfinished".into(),
                },
            }),
            80,
        );
        assert_eq!(t.last_text(), Some("first response"));
        t.apply(
            &ev(
                assistant(vec![
                    Block::Text {
                        text: "\nnew".into(),
                    },
                    Block::Text {
                        text: "response\n ".into(),
                    },
                ]),
                0,
            ),
            80,
        );
        assert_eq!(t.last_text(), Some("new\nresponse"));
    }

    #[test]
    fn exit_status_requires_a_numeric_trailing_footer() {
        assert_eq!(exit_status(&["normal output"]), None);
        assert_eq!(exit_status(&["[exit 0]"]), Some((0, 0)));
        assert_eq!(
            exit_status(&["output", "[exit 127]", "", "  "]),
            Some((1, 127))
        );
        for text in [
            "[exit failed]",
            "[exit ]",
            "[exit +1]",
            "[exit 1.0]",
            "[exit 1] extra",
            "[exit 4294967296]",
        ] {
            assert_eq!(exit_status(&[text]), None, "{text}");
        }
        assert_eq!(exit_status(&["[exit 1]", "more output"]), None);
    }

    #[test]
    fn exit_headline_removes_only_the_status_from_preview_and_hidden_count() {
        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "bash", json!({"command": "git log"}))]),
                0,
            ),
            96,
        );
        t.apply(&ev(decide("a", Decision::Allow, None), 1_000), 96);
        let output = "one\ntwo\nthree\nfour\nfive\n[exit 1]\n";
        let rows = t.apply(&ev(result("a", output, true), 2_800), 96);
        let text = plain(&rows);
        assert!(text[0].starts_with("✕ bash git log"));
        assert!(text[0].ends_with("exit 1 · 1.8s"));
        assert_eq!(
            text[1..],
            [
                "  └ one",
                "    two",
                "    three",
                "    … 2 more lines · ctrl+o"
            ]
        );
        assert!(
            rows[0]
                .0
                .iter()
                .any(|span| span.text == "exit 1" && span.role == Role::Sanguine)
        );
        assert!(
            rows[0]
                .0
                .iter()
                .any(|span| span.text == "1.8s" && span.role == Role::Dim)
        );
        let full = t.expand(96).unwrap();
        let text = plain(&full);
        assert_eq!(text[1], "── bash · git log ──");
        assert_eq!(
            text[2..8],
            ["one", "two", "three", "four", "five", "[exit 1]"]
        );
        assert_eq!(text[8], "── end · 6 lines ──");
        assert!(full[7].0.iter().all(|span| span.role == Role::Sanguine));

        let mut t = Transcript::default();
        t.apply(
            &ev(
                assistant(vec![use_("a", "bash", json!({"command": "false"}))]),
                0,
            ),
            96,
        );
        let rows = plain(&t.apply(&ev(result("a", "[exit 1]", true), 0), 96));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ends_with("exit 1"));
        assert!(
            t.expand(96).is_some(),
            "the removed footer remains reachable"
        );
    }

    #[test]
    fn successful_status_does_not_turn_the_headline_into_a_failure_and_malformed_status_is_output()
    {
        for (output, error) in [("ok\n[exit 0]", false), ("failed\n[exit unknown]", true)] {
            let mut t = Transcript::default();
            t.apply(
                &ev(
                    assistant(vec![use_("a", "bash", json!({"command": "cmd"}))]),
                    0,
                ),
                80,
            );
            let rows = t.apply(&ev(result("a", output, error), 0), 80);
            assert!(
                rows[0]
                    .plain()
                    .starts_with(if error { "✕ bash" } else { "● bash" })
            );
            assert!(!rows[0].plain().contains("exit"));
            assert_eq!(
                rows[2].plain(),
                if error {
                    "    [exit unknown]"
                } else {
                    "    [exit 0]"
                }
            );
            assert_eq!(
                rows[1].0.last().unwrap().role,
                if error { Role::Sanguine } else { Role::Ink }
            );
            assert_eq!(rows[2].0.last().unwrap().role, Role::Ink);
        }
    }

    #[test]
    fn expansion_rules_fit_small_widths_and_output_keeps_its_line_breaks() {
        let output = "a very long original output line that must remain one line when copied\nsecond\nthird\nfourth";
        assert_eq!(detail_rule("bash · anything", 0).width(), 0);
        for cols in 1..=80 {
            let mut t = Transcript::default();
            t.apply(
                &ev(
                    assistant(vec![use_(
                        "a",
                        "bash",
                        json!({"command": "git log --oneline -3; git status --short | wc -l 界"}),
                    )]),
                    0,
                ),
                cols,
            );
            t.apply(&ev(result("a", output, false), 0), cols);
            let lines = t.expand(cols).unwrap();
            assert!(lines[1].width() <= cols.min(48), "cols {cols}");
            assert!(lines.last().unwrap().width() <= cols.min(48), "cols {cols}");
            assert!(lines[1].0.iter().all(|span| span.role == Role::Dim));
            assert_eq!(lines[2].plain(), output.lines().next().unwrap());
            assert_eq!(lines[3].plain(), "second");
        }
    }

    #[test]
    fn live_user_messages_match_each_own_echo_including_steering_and_keep_replay_plain() {
        let mut t = Transcript::default();
        let content = |text: &str| vec![Block::Text { text: text.into() }];
        let first = content("first prompt");
        let steering = content("steer this turn");
        t.sent(&first);
        t.sent(&steering);
        for own in [first, steering] {
            let lines = plain(&t.apply_live(&ev(EventKind::UserMessage { content: own }, 0), 80));
            assert!(lines.iter().any(|line| line.starts_with("ξ ")), "{lines:?}");
            assert!(lines.iter().all(|line| !line.contains("another client")));
        }
        assert!(t.sent.is_empty());
        let peer = ev(
            EventKind::UserMessage {
                content: content("peer note wraps into several rows"),
            },
            (24 * 60 + 14 * 60 + 22) * 60_000,
        );
        let lines = t.apply_live(&peer, 36);
        let text = plain(&lines);
        let header = text.iter().position(|line| line.starts_with("» ")).unwrap();
        assert_eq!(text[header], "» from another client · 14:22");
        assert!(
            lines[header]
                .0
                .iter()
                .any(|span| span.text == "» " && span.role == Role::Brass)
        );
        assert!(text[header + 1..].iter().all(|line| line.starts_with("  ")));
        assert!(lines.iter().all(|line| line.width() <= 36));
        let replay = plain(&Transcript::default().apply(&peer, 36));
        assert!(replay.iter().any(|line| line.starts_with("ξ peer note")));
        let fresh = plain(&Transcript::default().apply_live(&peer, 36));
        assert!(
            fresh
                .iter()
                .any(|line| line.starts_with("» from another client"))
        );
    }

    #[test]
    fn each_child_answer_and_the_parent_result_are_reachable_through_ctrl_o() {
        let mut t = Transcript::default();
        let parent = SessionId::generate();
        t.apply(
            &ev(
                assistant(vec![use_("task", "task", json!({"tasks": [{}, {}]}))]),
                0,
            ),
            96,
        );
        for number in 1..=2 {
            let session = SessionId::generate();
            let mut child_event = |kind| {
                assert!(t.child(
                    parent,
                    &EventParams {
                        session,
                        event: Event {
                            id: EventId::generate(),
                            parent: None,
                            at: Timestamp(60_000),
                            kind
                        },
                    }
                ));
            };
            child_event(EventKind::SessionStarted {
                session,
                cwd: "/w".into(),
                model: ModelRef {
                    provider: "p".into(),
                    model: "m".into(),
                },
                xi_version: "test".into(),
                parent_session: Some(parent),
            });
            child_event(EventKind::UserMessage {
                content: vec![Block::Text {
                    text: format!("check {number}"),
                }],
            });
            child_event(EventKind::AssistantMessage {
                model: ModelRef {
                    provider: "p".into(),
                    model: "m".into(),
                },
                content: vec![Block::Text {
                    text: format!("answer {number}\ntwo\nthree\nfour"),
                }],
                stop: StopReason::EndTurn,
                usage: Default::default(),
            });
        }
        let rows = plain(&t.apply(&ev(result("task", "parent result", false), 60_000), 96));
        assert!(
            rows.iter()
                .any(|line| line == "» from child 1 · check 1 · 00:01")
        );
        assert!(
            rows.iter()
                .any(|line| line == "» from child 2 · check 2 · 00:01")
        );
        assert_eq!(
            rows.iter()
                .filter(|line| line.as_str() == "  … 1 more line · ctrl+o")
                .count(),
            2
        );
        for number in [2, 1] {
            let full = plain(&t.expand(96).unwrap());
            assert_eq!(
                full[1],
                format!("── from child {number} · check {number} ──")
            );
            assert_eq!(
                full[2..],
                [
                    format!("answer {number}"),
                    "two".into(),
                    "three".into(),
                    "four".into(),
                    "── end · 4 lines ──".into()
                ]
            );
        }
        let full = plain(&t.expand(96).unwrap());
        assert!(full.iter().any(|line| line == "parent result"));
        assert!(t.expand(96).is_none());
    }

    #[test]
    fn numbers_read_like_apparatus() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(14_200), "14.2k");
        assert_eq!(tokens(1_250_000), "1.2M");
        assert_eq!(duration(4200), "4.2s");
        assert_eq!(duration(72_000), "1m12s");
    }
}

#[cfg(test)]
mod client_tests {
    use xi_client::client::ClientNotification;
    use xi_protocol::methods::client::{
        Delta, DeltaParams, ExitedParams, PendingApproval, ResetParams,
    };
    use xi_protocol::{Block, SessionId};

    use super::Transcript;

    #[test]
    fn reset_drops_an_earlier_attempt_and_restored_partial() {
        let session = SessionId::generate();
        let mut transcript = Transcript::default();
        transcript.partial(&[
            Block::Thinking {
                text: "reasoning".into(),
                signature: None,
            },
            Block::Text {
                text: "old attempt".into(),
            },
        ]);
        assert!(
            transcript
                .live(60, 0)
                .iter()
                .any(|line| line.plain().contains("old attempt"))
        );
        transcript.apply(
            &ClientNotification::Reset(ResetParams {
                session,
                attempt: 1,
                sequence: 0,
            }),
            60,
        );
        assert!(transcript.live(60, 0).is_empty());
        assert!(!transcript.thinking());
        transcript.apply(
            &ClientNotification::Delta(DeltaParams {
                session,
                attempt: 1,
                sequence: 0,
                delta: Delta::Text {
                    text: "new attempt".into(),
                },
            }),
            60,
        );
        let live: Vec<_> = transcript
            .live(60, 0)
            .iter()
            .map(crate::style::Line::plain)
            .collect();
        assert!(live.iter().any(|line| line.contains("new attempt")));
        assert!(live.iter().all(|line| !line.contains("old attempt")));
    }

    #[test]
    fn pending_approvals_deduplicate_and_exit_clears_live_state() {
        let session = SessionId::generate();
        let mut transcript = Transcript::default();
        let pending = ClientNotification::ApprovalPending(PendingApproval {
            session,
            call: "approval".into(),
            tool: "bash".into(),
            action: "run command".into(),
            rule: None,
            taint: true,
            trusted: true,
        });
        transcript.apply(&pending, 60);
        transcript.apply(&pending, 60);
        assert_eq!(transcript.asks().len(), 1);
        assert!(transcript.asks()[0].taint);
        assert_eq!(transcript.asks()[0].trusted, Some(true));
        transcript.partial(&[Block::Text {
            text: "unfinished".into(),
        }]);
        let lines = transcript.apply(
            &ClientNotification::Exited(ExitedParams {
                session,
                status: Some(1),
                message: Some("worker stopped".into()),
            }),
            60,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.plain().contains("session exited · status 1"))
        );
        assert!(transcript.asks().is_empty());
        assert!(transcript.live(60, 0).is_empty());
    }
}
