//! The session tree: branch points, compactions and taint, drawn from the
//! log.
//!
//! Each prompt is a node, and so is each compaction. The events a turn
//! appended between them fold into one summary node. A chain without branches
//! stays at one indent; only a branch point draws connectors, so a long linear
//! session reads as a column.

use std::collections::HashMap;

use xi_protocol::{Block, Event, EventId, EventKind, Timestamp};

use crate::style::{Line, Role, lr};
use crate::transcript::tokens;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Root,
    You,
    Turn,
    Compaction,
}

/// One row of the tree.
#[derive(Clone, Debug)]
pub struct Node {
    kind: Kind,
    /// Connectors drawn before the node.
    prefix: String,
    text: String,
    at: Timestamp,
    /// The event a checkout moves the head to.
    pub target: EventId,
    /// A prompt node's text, which goes back in the composer on checkout.
    pub prompt: Option<String>,
    /// Whether the branch has read untrusted content by this node.
    tainted: bool,
    /// Whether this node is where the branch read it.
    taints: bool,
    head: bool,
}

/// The tree overlay's state.
pub struct Tree {
    nodes: Vec<Node>,
    /// Index of the selected node.
    pub selected: usize,
    branches: usize,
}

/// `now`, `4m`, `2h`, `3d`.
fn ago(now: Timestamp, at: Timestamp) -> String {
    let s = now.0.saturating_sub(at.0) / 1000;
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// What a run of turn events did, in one line.
#[derive(Default)]
struct Summary {
    calls: usize,
    edits: usize,
    failed: usize,
    reply: String,
    notes: Vec<String>,
}

impl Summary {
    fn add(&mut self, kind: &EventKind) {
        match kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    match block {
                        Block::ToolUse { name, .. } => {
                            self.calls += 1;
                            if matches!(name.as_str(), "edit" | "write" | "hashline") {
                                self.edits += 1;
                            }
                        }
                        Block::Text { text } if !text.trim().is_empty() => {
                            self.reply = first_line(text);
                        }
                        _ => {}
                    }
                }
            }
            EventKind::ToolResult { output, .. } if output.is_error => self.failed += 1,
            EventKind::ModelChanged { model } => {
                self.notes
                    .push(format!("model → {}/{}", model.provider, model.model));
            }
            EventKind::TurnFailed { .. } => self.notes.push("turn failed".into()),
            _ => {}
        }
    }

    fn text(self) -> String {
        let mut parts = self.notes;
        if self.calls > 0 {
            parts.push(plural(self.calls, "call"));
        }
        if self.edits > 0 {
            parts.push(plural(self.edits, "edit"));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if !self.reply.is_empty() {
            parts.push(self.reply);
        }
        if parts.is_empty() {
            "no reply".into()
        } else {
            parts.join(" · ")
        }
    }
}

fn prompt_text(content: &[Block]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tree {
    /// Builds the tree of `events`, in file order, with `head` selected.
    #[must_use]
    pub fn new(events: &[Event], head: EventId) -> Self {
        let mut children: HashMap<EventId, Vec<usize>> = HashMap::new();
        let mut root = None;
        for (i, event) in events.iter().enumerate() {
            match event.parent {
                Some(parent) => children.entry(parent).or_default().push(i),
                None => root = Some(i),
            }
        }
        let kids = |i: usize| children.get(&events[i].id).map_or(&[][..], Vec::as_slice);
        let mut tree = Self {
            nodes: Vec::new(),
            selected: 0,
            branches: events
                .iter()
                .filter(|e| !children.contains_key(&e.id))
                .count(),
        };
        // (first event of the node, its connector prefix, the prefix under it, tainted)
        let mut stack = root
            .map(|i| (i, String::new(), String::new(), false))
            .into_iter()
            .collect::<Vec<_>>();
        while let Some((start, prefix, under, tainted)) = stack.pop() {
            let event = &events[start];
            let (kind, mut end) = match &event.kind {
                EventKind::SessionStarted { .. } => (Kind::Root, start),
                EventKind::UserMessage { .. } => (Kind::You, start),
                EventKind::Compaction { .. } => (Kind::Compaction, start),
                _ => (Kind::Turn, start),
            };
            let mut summary = Summary::default();
            let mut taints = matches!(event.kind, EventKind::Tainted { .. });
            if kind == Kind::Turn {
                summary.add(&event.kind);
                // Fold the chain until it branches, reaches the head or a
                // prompt or compaction starts the next node.
                while events[end].id != head
                    && let [next] = kids(end)
                    && !matches!(
                        events[*next].kind,
                        EventKind::UserMessage { .. } | EventKind::Compaction { .. }
                    )
                {
                    end = *next;
                    summary.add(&events[end].kind);
                    taints |= matches!(events[end].kind, EventKind::Tainted { .. });
                }
            }
            let (text, prompt, target) = match &event.kind {
                EventKind::SessionStarted { model, .. } => (
                    format!("session started · {}/{}", model.provider, model.model),
                    None,
                    event.id,
                ),
                EventKind::UserMessage { content } => {
                    let prompt = prompt_text(content);
                    // A prompt checks out its parent, so resending it, edited
                    // or not, starts a sibling branch.
                    (
                        first_line(&prompt),
                        Some(prompt),
                        event.parent.unwrap_or(event.id),
                    )
                }
                EventKind::Compaction { tokens_before, .. } => (
                    format!("compacted · {} tokens before", tokens(*tokens_before)),
                    None,
                    event.id,
                ),
                _ => (summary.text(), None, events[end].id),
            };
            let tainted = tainted || taints;
            tree.nodes.push(Node {
                kind,
                prefix,
                text,
                at: events[end].at,
                target,
                prompt,
                tainted,
                taints,
                head: events[end].id == head,
            });
            if tree.nodes.last().is_some_and(|n| n.head) {
                tree.selected = tree.nodes.len() - 1;
            }
            let next = kids(end);
            // Pushed in reverse so the first child is drawn first.
            if let [only] = next {
                stack.push((*only, under.clone(), under, tainted));
            } else {
                for (n, &child) in next.iter().enumerate().rev() {
                    let last = n + 1 == next.len();
                    let (joint, guide) = if last {
                        ("└─ ", "   ")
                    } else {
                        ("├─ ", "│  ")
                    };
                    stack.push((
                        child,
                        format!("{under}{joint}"),
                        format!("{under}{guide}"),
                        tainted,
                    ));
                }
            }
        }
        tree
    }

    /// The selected node.
    #[must_use]
    pub fn current(&self) -> Option<&Node> {
        self.nodes.get(self.selected)
    }

    /// Moves the selection by `by` rows, clamped to the tree.
    pub fn step(&mut self, by: isize) {
        let last = self.nodes.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    /// Selects the first or the last node.
    pub fn jump(&mut self, end: bool) {
        self.selected = if end {
            self.nodes.len().saturating_sub(1)
        } else {
            0
        };
    }

    fn row(&self, i: usize, now: Timestamp, cols: usize) -> Line {
        let node = &self.nodes[i];
        let cursor = if i == self.selected {
            Line::default().brass("› ")
        } else {
            Line::default().ink("  ")
        };
        let mut left = cursor.dim(&node.prefix);
        left = match node.kind {
            Kind::Root => left.dim("○ ").dim(&node.text),
            Kind::You => left.brass("ξ ").ink(&node.text),
            Kind::Turn => left.dim("▸ ").dim(&node.text),
            Kind::Compaction => left.strong(Role::Ink, "≡ ").dim(&node.text),
        };
        let mut right = Line::default();
        if node.taints {
            right = right.murex("◆ tainted ");
        } else if node.tainted {
            right = right.murex("◆ ");
        }
        right = if node.head {
            right.brass("● head")
        } else {
            right.dim(ago(now, node.at))
        };
        lr(left, right, cols)
    }

    /// The overlay, `cols` wide and at most `rows` tall, keeping the
    /// selection in view.
    #[must_use]
    pub fn render(&self, cols: usize, rows: usize, now: Timestamp) -> Vec<Line> {
        let inner = cols.saturating_sub(4);
        let Some(node) = self.current() else {
            return Vec::new();
        };
        let mut foot = Vec::new();
        if node.tainted {
            foot.push(
                Line::default()
                    .murex("◆")
                    .dim(" this branch read untrusted content, so egress on it asks."),
            );
        }
        foot.push(Line::default().dim(if node.prompt.is_some() {
            "enter puts this prompt back in the composer; sending it starts a new branch."
        } else {
            "enter checks out this point; your next prompt continues from it."
        }));
        let visible = rows
            .saturating_sub(foot.len() + 3)
            .clamp(1, self.nodes.len().max(1));
        let top = self
            .selected
            .saturating_sub(visible / 2)
            .min(self.nodes.len() - visible);
        let mut body: Vec<Line> = (top..top + visible)
            .map(|i| self.row(i, now, inner))
            .collect();
        body.push(Line::default());
        body.extend(foot);
        let b = |s: &str| Line::default().span(Role::Brass, false, s);
        let branches = match self.branches {
            1 => "1 branch".to_owned(),
            n => format!("{n} branches"),
        };
        let title = Line::default()
            .strong(Role::Ink, "session tree")
            .dim(format!(" · {branches}"));
        let right = Line::default().dim("↑↓ move · enter checkout · esc");
        let fill = cols.saturating_sub(title.width() + right.width() + 8);
        let mut lines = vec![
            b("╭─ ")
                .extend(title)
                .ink(" ")
                .extend(b(&"─".repeat(fill)))
                .ink(" ")
                .extend(right)
                .extend(b(" ─╮"))
                .truncate(cols),
        ];
        for row in body {
            let row = row.truncate(inner);
            let pad = inner - row.width();
            lines.push(b("│ ").extend(row).ink(" ".repeat(pad)).extend(b(" │")));
        }
        lines.push(b(&format!("╰{}╯", "─".repeat(cols.saturating_sub(2)))));
        lines
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use xi_protocol::event::{ModelRef, StopReason, Usage};
    use xi_protocol::{OutputBlock, SessionId, ToolOutput};

    use super::*;

    struct Log(Vec<Event>);

    impl Log {
        fn add(&mut self, parent: Option<EventId>, kind: EventKind) -> EventId {
            let id = EventId::generate();
            self.0.push(Event {
                id,
                parent,
                at: Timestamp(1_000),
                kind,
            });
            id
        }
    }

    fn model() -> ModelRef {
        ModelRef {
            provider: "anthropic".into(),
            model: "m".into(),
        }
    }

    fn user(text: &str) -> EventKind {
        EventKind::UserMessage {
            content: vec![Block::Text { text: text.into() }],
        }
    }

    fn reply(text: &str, tools: &[&str]) -> EventKind {
        let mut content = vec![Block::Text { text: text.into() }];
        for (i, name) in tools.iter().enumerate() {
            content.push(Block::ToolUse {
                id: format!("{name}-{i}").as_str().into(),
                name: (*name).into(),
                input: json!({}),
            });
        }
        EventKind::AssistantMessage {
            model: model(),
            content,
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn result(is_error: bool) -> EventKind {
        EventKind::ToolResult {
            call: "x".into(),
            output: ToolOutput {
                content: vec![OutputBlock::Text { text: "ok".into() }],
                is_error,
                untrusted: false,
            },
            spill: None,
        }
    }

    #[test]
    fn chains_fold_branches_draw_connectors_and_taint_stays_on_its_branch() {
        let mut log = Log(Vec::new());
        let root = log.add(
            None,
            EventKind::SessionStarted {
                session: SessionId::generate(),
                cwd: "/w".into(),
                model: model(),
                xi_version: "0".into(),
                parent_session: None,
            },
        );
        let u1 = log.add(
            Some(root),
            user("the export tests fail\nsince the refactor"),
        );
        let a1 = log.add(Some(u1), reply("Looking.", &["bash", "edit"]));
        let r1 = log.add(Some(a1), result(false));
        let r2 = log.add(Some(r1), result(true));
        let a2 = log.add(Some(r2), reply("Tests fail on archive.", &[]));
        let u2 = log.add(Some(a2), user("try a shim"));
        let a3 = log.add(Some(u2), reply("", &["fetch"]));
        let t = log.add(
            Some(a3),
            EventKind::Tainted {
                call: None,
                reason: "web".into(),
            },
        );
        let r3 = log.add(Some(t), result(false));
        log.add(Some(r3), reply("shim done", &[]));
        let c = log.add(
            Some(a2),
            EventKind::Compaction {
                summary: "s".into(),
                tokens_before: 1_200,
                kept: None,
                usage: None,
            },
        );
        let u3 = log.add(Some(c), user("ok, now deleted.py"));
        let head = log.add(Some(u3), reply("done", &[]));

        let tree = Tree::new(&log.0, head);
        let rows: Vec<String> = tree
            .render(80, 40, Timestamp(1_000))
            .iter()
            .map(|l| {
                let p = l.plain();
                p.trim_start_matches("│ ")
                    .trim_end_matches(" │")
                    .trim_end()
                    .to_owned()
            })
            .collect();
        assert!(rows[0].contains("session tree · 2 branches"), "{rows:?}");
        let starts = [
            "  ○ session started · anthropic/m",
            "  ξ the export tests fail",
            "  ▸ 2 calls · 1 edit · 1 failed · Tests fail on archive.",
            "  ├─ ξ try a shim",
            "  │  ▸ 1 call · shim done",
            "  └─ ≡ compacted · 1.2k tokens before",
            "     ξ ok, now deleted.py",
            "›    ▸ done",
        ];
        for (row, start) in rows[1..].iter().zip(starts) {
            assert!(row.starts_with(start), "{row:?} should start {start:?}");
        }
        assert!(rows[4].ends_with("now"), "{rows:?}");
        assert!(rows[5].ends_with("◆ tainted now"), "{rows:?}");
        assert!(rows[6].ends_with("now"), "{rows:?}");
        assert!(rows[8].ends_with("● head"), "{rows:?}");
        assert_eq!(rows[9], "");
        assert!(rows[10].starts_with("enter checks out"), "{rows:?}");

        let mut tree = tree;
        tree.step(-4);
        let node = tree.current().unwrap();
        assert_eq!(node.target, a2, "a prompt checks out its parent");
        assert_eq!(node.prompt.as_deref(), Some("try a shim"));
        tree.step(1);
        assert_eq!(tree.current().unwrap().target, log.0[10].id);
        tree.step(-100);
        assert_eq!(tree.current().unwrap().target, root);
    }
}
