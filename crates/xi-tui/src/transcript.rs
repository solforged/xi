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
use xi_protocol::event::{Decision, ModelRef, StopReason, Usage};
use xi_protocol::methods::client::Delta;
use xi_protocol::model::Pricing;
use xi_protocol::{Block, EventKind, OutputBlock, Timestamp, ToolCallId, ToolOutput};

use crate::style::{Line, Role, lr, wrap};

/// Terminal spinner frames, one every 125 ms.
pub const SPINNER: [&str; 8] = ["┤", "┘", "┴", "└", "├", "┌", "┬", "┐"];

/// Result lines shown under a call before the rest is counted.
const RESULT_LINES: usize = 3;
/// Streaming rows kept in the live region.
const STREAM_ROWS: usize = 6;

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
    /// Whether the branch was tainted when it asked.
    pub tainted: bool,
}

struct Call {
    id: ToolCallId,
    name: String,
    arg: String,
    since: Option<Instant>,
    decided_at: Option<Timestamp>,
    decision: Option<Decision>,
    rule: Option<String>,
    approved: Option<bool>,
    result: Option<(ToolOutput, Option<std::path::PathBuf>, Timestamp)>,
}

/// Text the transcript showed cut short, kept whole for ctrl+o.
struct Detail {
    /// The headline: a call's mark, tool and argument, or the compaction.
    head: Line,
    /// What the expansion shows, such as `full output`.
    what: &'static str,
    text: String,
    error: bool,
    spill: Option<std::path::PathBuf>,
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
    fn add(&mut self, usage: Usage) {
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
fn headline_arg(name: &str, input: &Value, workspace: Option<&std::path::Path>) -> String {
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
        "read" | "write" | "edit" | "hashline" => {
            field("path").map(|p| relative(p, workspace).to_owned())
        }
        "search" => field("pattern").map(|p| match field("path").map(|d| relative(d, workspace)) {
            Some(".") | None => p.to_owned(),
            Some(path) => format!("{p} in {path}"),
        }),
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

    /// The block the call commits, and its whole result when the block cut
    /// lines from it.
    fn committed(&self, cols: usize) -> (Vec<Line>, Option<Detail>) {
        let Some((output, spill, at)) = &self.result else {
            return (Vec::new(), None);
        };
        let text = text_of(output);
        let count = text.lines().count();
        let mut meta = Vec::new();
        match (self.decision, self.approved) {
            (Some(Decision::Ask), Some(true)) => meta.push("allowed by you".to_owned()),
            (Some(Decision::Ask), Some(false)) => meta.push("denied by you".to_owned()),
            (Some(Decision::Deny), _) => meta.push("denied".to_owned()),
            _ => {}
        }
        // Built-in rules allow ordinary work; only rules someone wrote are news.
        if let Some(rule) = self.rule.as_ref().filter(|r| !r.starts_with("builtin#")) {
            meta.push(rule.clone());
        }
        let quiet = !output.is_error && matches!(self.name.as_str(), "read" | "search");
        if quiet {
            let noun = if self.name == "read" { "lines" } else { "hits" };
            meta.push(format!("{count} {noun}"));
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
        let mut lines = vec![lr(
            self.head(self.mark()),
            Line::default().span(role, false, meta.join(" · ")),
            cols,
        )];
        if quiet {
            return (lines, None);
        }
        let shown: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(RESULT_LINES)
            .collect();
        for (i, row) in shown.iter().enumerate() {
            let lead = if i == 0 {
                Line::default().dim("  └ ")
            } else {
                Line::default().ink("    ")
            };
            let body = if output.is_error && i == 0 {
                Line::default().sanguine(row)
            } else {
                Line::default().ink(row)
            };
            lines.push(lead.extend(body).truncate(cols));
        }
        let hidden = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
            .saturating_sub(shown.len());
        let mut more = Vec::new();
        if hidden > 0 {
            more.push(format!("… {hidden} more lines · ctrl+o"));
        }
        if let Some(path) = spill {
            more.push(format!("full output in {}", path.display()));
        }
        if !more.is_empty() {
            let more = format!("    {}", more.join(" · "));
            lines.push(Line::default().dim(more).truncate(cols));
        }
        let detail = (hidden > 0).then(|| Detail {
            head: self.head(self.mark()),
            what: "full output",
            text,
            error: output.is_error,
            spill: spill.clone(),
        });
        (lines, detail)
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

    /// Starts counting a new turn.
    pub fn begin_turn(&mut self) {
        self.turn = Totals::default();
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

    /// Applies a notification and returns the lines it commits.
    pub fn apply(&mut self, notification: &ClientNotification, cols: usize) -> Vec<Line> {
        match notification {
            ClientNotification::Reset(_) => {
                self.stream.clear();
                self.thinking = false;
                Vec::new()
            }
            ClientNotification::Delta(params) => {
                match &params.delta {
                    Delta::Text { text } => self.stream.push_str(text),
                    Delta::Thinking { .. } => self.thinking = true,
                    Delta::ToolInput { .. } => {}
                }
                Vec::new()
            }
            ClientNotification::Event(params) => {
                self.event(&params.event.kind, params.event.at, cols)
            }
            ClientNotification::ApprovalPending(pending) => {
                if !self.asks.iter().any(|ask| ask.call == pending.call) {
                    self.asks.push(Ask {
                        call: pending.call.clone(),
                        tool: pending.tool.clone(),
                        action: pending.action.clone(),
                        rule: pending.rule.clone(),
                        tainted: self.tainted,
                    });
                }
                Vec::new()
            }
            ClientNotification::Exited(params) => {
                self.end_turn();
                self.calls.clear();
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
        for block in blocks {
            match block {
                Block::Text { text } => self.stream.push_str(text),
                Block::Thinking { .. } | Block::RedactedThinking { .. } => self.thinking = true,
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
            EventKind::ReasoningChanged { reasoning } => {
                self.push(
                    &mut out,
                    [Line::default().dim(format!(
                        "  reasoning → {}",
                        crate::reasoning_name(*reasoning)
                    ))],
                );
            }
            EventKind::UserMessage { content } => {
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
                self.session.add(*usage);
                self.turn.add(*usage);
                let context = usage.input + usage.cache_read + usage.cache_write + usage.output;
                // A cancelled request reports nothing; keep the last real size.
                if context > 0 {
                    self.context = context;
                }
                for block in content {
                    match block {
                        Block::Text { text } if !text.trim().is_empty() => {
                            self.gap(&mut out);
                            let rows = wrap(text.trim(), cols.saturating_sub(2));
                            let lines: Vec<_> = rows
                                .iter()
                                .map(|r| Line::default().ink("  ").ink(r))
                                .collect();
                            self.push(&mut out, lines);
                            self.blank = false;
                            self.gap(&mut out);
                        }
                        Block::ToolUse { id, name, input } => self.calls.push(Call {
                            id: id.clone(),
                            name: name.clone(),
                            arg: headline_arg(name, input, self.workspace.as_deref()),
                            since: Some(Instant::now()),
                            decided_at: None,
                            decision: None,
                            rule: None,
                            approved: None,
                            result: None,
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
            } => {
                let tainted = self.tainted;
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
                            tainted,
                        };
                        if let Some(pending) = self.asks.iter_mut().find(|a| a.call == *call) {
                            *pending = ask;
                        } else {
                            self.asks.push(ask);
                        }
                    }
                }
            }
            EventKind::ApprovalResolved { call, approved, .. } => {
                self.asks.retain(|a| &a.call != call);
                if let Some(c) = self.call(call) {
                    c.approved = Some(*approved);
                    c.since = Some(Instant::now());
                }
            }
            EventKind::ToolResult {
                call,
                output,
                spill,
            } => {
                self.asks.retain(|a| &a.call != call);
                if let Some(c) = self.call(call) {
                    c.result = Some((output.clone(), spill.clone(), at));
                }
                let done = self.calls.iter().take_while(|c| c.result.is_some()).count();
                let mut lines = Vec::new();
                for call in self.calls.drain(..done) {
                    let (block, detail) = call.committed(cols);
                    lines.extend(block);
                    if let Some(detail) = detail {
                        self.details.push(detail);
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
                            .dim(format!("  … {hidden} more lines of summary · ctrl+o"))
                            .truncate(cols),
                    );
                    self.details.push(Detail {
                        head: Line::default().strong(Role::Ink, "≡ compacted"),
                        what: "full summary",
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
        let mut lines = vec![lr(
            detail.head.clone(),
            Line::default().dim(format!("{} · {count} lines", detail.what)),
            cols,
        )];
        for (i, row) in wrap(&detail.text, cols.saturating_sub(4))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 {
                Line::default().dim("  └ ")
            } else {
                Line::default().ink("    ")
            };
            let body = if detail.error && i == 0 {
                Line::default().sanguine(row)
            } else {
                Line::default().ink(row)
            };
            lines.push(lead.extend(body));
        }
        if let Some(path) = &detail.spill {
            lines.push(
                Line::default()
                    .dim(format!("    full output in {}", path.display()))
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
        self.asks.clear();
    }

    /// Calls still in flight, then the newest streamed rows.
    #[must_use]
    pub fn live(&self, cols: usize, frame: usize) -> Vec<Line> {
        let mut lines = Vec::new();
        if !self.stream.trim().is_empty() {
            let rows = wrap(self.stream.trim(), cols.saturating_sub(2));
            let skip = rows.len().saturating_sub(STREAM_ROWS);
            for row in &rows[skip..] {
                lines.push(Line::default().ink("  ").ink(row));
            }
            lines.push(Line::default());
        }
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
                let meta = match call.rule.as_ref().filter(|r| !r.starts_with("builtin#")) {
                    Some(rule) => format!("{rule} · {}", duration(elapsed)),
                    None => duration(elapsed),
                };
                (
                    Line::default().brass(SPINNER[frame % SPINNER.len()]),
                    Line::default().dim(meta),
                )
            };
            lines.push(lr(call.head(mark), meta, cols));
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

fn blocks_text(content: &[Block]) -> String {
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
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
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
            t.apply(&ev(result("b", "x\ny", false), 1100), 60)
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
        assert_eq!(rows[4], "    … 1 more lines · ctrl+o");
        assert!(rows[5].starts_with("● read policies/archive.py"));
        assert!(rows[5].ends_with("2 lines · 0.1s"));
        assert!(t.live(60, 0).is_empty());
        assert_eq!(t.context, 15_280);
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
        assert!(t.asks()[0].tainted);
        assert!(plain(&t.live(60, 0))[0].ends_with("waiting on you"));
        t.apply(
            &ev(
                EventKind::ApprovalResolved {
                    call: "a".into(),
                    approved: false,
                    by: xi_protocol::event::Approver::User,
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
        assert!(newest[1].starts_with("✕ bash make test"), "{newest:?}");
        assert!(newest[1].ends_with("full output · 4 lines"), "{newest:?}");
        assert_eq!(newest[2..], ["  └ FAIL", "    a", "    b", "    c"]);
        let older = plain(&t.expand(60).unwrap());
        assert!(older[1].starts_with("● bash make"), "{older:?}");
        assert_eq!(older[2..], ["  └ 1", "    2", "    3", "    ", "    4"]);
        assert!(t.expand(60).is_none());
        assert!(plain(&t.expand(60).unwrap())[1].starts_with("✕ bash make test"));
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
        assert!(full[0].ends_with("full summary · 12 lines"), "{full:?}");
        assert_eq!(full[1], "  └ ## User goal and constraints");
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
        });
        transcript.apply(&pending, 60);
        transcript.apply(&pending, 60);
        assert_eq!(transcript.asks().len(), 1);
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
