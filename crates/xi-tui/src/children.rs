//! Child sessions a running call started, one row each.
//!
//! A subagent is a child session that runs inside its parent's worker. Its
//! events arrive alongside the parent's, tagged with the child's session id.
//! While the call that started them runs, each child gets a row in the live
//! region: its number in start order, what it is doing now, and its calls and
//! time so far. When the call's result lands, each finished child commits an
//! open-sided slip with its answer and keeps the whole answer for ctrl+o.

use std::path::Path;

use xi_protocol::event::StopReason;
use xi_protocol::{Block, Event, EventKind, SessionId, Timestamp, ToolCallId};

use crate::callout::markdown;
use crate::deck::AgentRow;
use crate::layout::Measure;
use crate::style::{Line, Role, inline, lr};
use crate::transcript::{
    Detail, RESULT_LINES, SPINNER, Totals, blocks_text, clock, counted, duration, headline_arg,
    more_lines,
};

/// Rows the live region gives children before the rest collapse to a count.
const LIVE_ROWS: usize = 8;

enum State {
    Running,
    Done(String),
    Failed(String),
}

struct Child {
    session: SessionId,
    started: Timestamp,
    ended: Option<Timestamp>,
    label: Option<String>,
    calls: usize,
    /// Calls without a result yet, oldest first: (id, tool, argument).
    open: Vec<(ToolCallId, String, String)>,
    usage: Totals,
    state: State,
}

impl Child {
    fn running(&self) -> bool {
        matches!(self.state, State::Running)
    }

    fn event(&mut self, event: &Event, workspace: Option<&Path>) {
        match &event.kind {
            EventKind::UserMessage { content } if self.label.is_none() => {
                let text = blocks_text(xi_protocol::skill::typed(content));
                self.label = Some(
                    Line::default()
                        .ink(
                            text.lines()
                                .find(|line| !line.trim().is_empty())
                                .unwrap_or("")
                                .trim(),
                        )
                        .truncate(32)
                        .plain(),
                );
            }
            EventKind::AssistantMessage {
                content,
                stop,
                usage,
                ..
            } => {
                self.usage.add(*usage);
                for block in content {
                    if let Block::ToolUse {
                        id, name, input, ..
                    } = block
                    {
                        self.calls += 1;
                        self.open.push((
                            id.clone(),
                            name.clone(),
                            headline_arg(name, input, workspace),
                        ));
                    }
                }
                let text = || {
                    content
                        .iter()
                        .filter_map(|block| match block {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.state = match stop {
                    StopReason::ToolUse => State::Running,
                    StopReason::EndTurn => State::Done(text()),
                    StopReason::MaxTokens => State::Failed("output token limit".into()),
                    StopReason::Refusal => State::Failed("refused".into()),
                    StopReason::Cancelled => State::Failed("cancelled".into()),
                };
                if !self.running() {
                    self.ended = Some(event.at);
                }
            }
            EventKind::ToolResult { call, .. } => self.open.retain(|(id, ..)| id != call),
            EventKind::TurnFailed { message } => {
                self.state = State::Failed(message.clone());
                self.ended = Some(event.at);
            }
            _ => {}
        }
    }

    /// `├─ ● 2  what` on the left, `3 calls · 12.0s` on the right.
    fn row(&self, number: usize, width: usize, last: bool, frame: usize, cols: usize) -> Line {
        let elbow = Line::default().dim(if last { "  └─ " } else { "  ├─ " });
        let mark = match &self.state {
            State::Running => Line::default().brass(SPINNER[frame % SPINNER.len()]),
            State::Done(_) => Line::default().verdigris("●"),
            State::Failed(_) => Line::default().sanguine("✕"),
        };
        let mut left = elbow
            .extend(mark)
            .ink(" ")
            .strong(Role::Ink, format!("{number:<width$}"))
            .ink("  ");
        left = match &self.state {
            State::Running => match self.open.first() {
                Some((_, tool, arg)) => {
                    let line = left.dim(tool);
                    if arg.is_empty() {
                        line
                    } else {
                        line.ink(" ").ink(arg)
                    }
                }
                None => left.dim("thinking"),
            },
            State::Done(text) if text.trim().is_empty() => left.dim("done"),
            State::Done(text) => left.extend(inline(
                text.lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim(),
                Role::Ink,
            )),
            State::Failed(message) => left
                .sanguine("failed")
                .dim(" · ")
                .ink(message.lines().next().unwrap_or("")),
        };
        let now = Timestamp::now();
        let elapsed = self.ended.unwrap_or(now).0.saturating_sub(self.started.0);
        let right = Line::default().dim(format!(
            "{} · {}",
            counted(self.calls, "call", "calls"),
            duration(elapsed)
        ));
        lr(left, right, cols)
    }

    fn slip(&self, number: usize, m: Measure) -> Option<(Vec<Line>, Detail)> {
        let cols = m.out;
        let (text, error) = match &self.state {
            State::Running => return None,
            State::Done(text) => (text.as_str(), false),
            State::Failed(reason) => (reason.as_str(), true),
        };
        let mut title = format!("from child {number}");
        if let Some(label) = self.label.as_ref().filter(|label| !label.is_empty()) {
            title.push_str(" · ");
            title.push_str(label);
        }
        let head = if error {
            Line::default().sanguine("✕ ")
        } else {
            Line::default().dim("» ")
        };
        let mut lines = vec![
            head.dim(format!(
                "{title} · {}",
                clock(self.ended.unwrap_or(self.started))
            ))
            .truncate(cols),
        ];
        let rows = markdown(
            text.trim(),
            m.prose,
            if error { Role::Sanguine } else { Role::Ink },
        );
        let hidden = rows.len().saturating_sub(RESULT_LINES);
        for row in rows.into_iter().take(RESULT_LINES) {
            lines.push(Line::default().ink("  ").extend(row).truncate(cols));
        }
        if hidden > 0 {
            lines.push(
                Line::default()
                    .dim(format!("  {}", more_lines(hidden)))
                    .truncate(cols),
            );
        }
        Some((
            lines,
            Detail {
                title,
                diff: false,
                text: text.to_owned(),
                thinking: false,
                error,
                spill: None,
            },
        ))
    }
}

/// The children of the call in flight, in start order.
#[derive(Default)]
pub struct Children(Vec<Child>);

impl Children {
    /// Takes an event from another session. Returns false unless that
    /// session is, or with this event becomes, a child of `parent`.
    pub fn event(
        &mut self,
        parent: SessionId,
        session: SessionId,
        event: &Event,
        workspace: Option<&Path>,
    ) -> bool {
        if let Some(child) = self.0.iter_mut().find(|c| c.session == session) {
            child.event(event, workspace);
            return true;
        }
        match &event.kind {
            EventKind::SessionStarted {
                parent_session: Some(p),
                ..
            } if *p == parent => {
                self.0.push(Child {
                    session,
                    started: event.at,
                    label: None,
                    ended: None,
                    calls: 0,
                    open: Vec::new(),
                    usage: Totals::default(),
                    state: State::Running,
                });
                true
            }
            _ => false,
        }
    }

    /// Whether no child has started since the last result.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Children still working.
    #[must_use]
    pub fn running(&self) -> usize {
        self.0.iter().filter(|c| c.running()).count()
    }

    /// Every input token the children sent.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.0.iter().map(|c| c.usage.sent()).sum()
    }

    /// One row per child under the running call. Past `LIVE_ROWS`, failed
    /// and running children keep their rows and the rest become a count.
    #[must_use]
    pub fn live(&self, cols: usize, frame: usize) -> Vec<Line> {
        let width = self.0.len().to_string().len();
        let mut shown: Vec<usize> = (0..self.0.len()).collect();
        let mut hidden = 0;
        if shown.len() > LIVE_ROWS {
            let keep = LIVE_ROWS - 1;
            let rank = |c: &Child| match c.state {
                State::Failed(_) => 0,
                State::Running => 1,
                State::Done(_) => 2,
            };
            let mut ranked = shown.clone();
            ranked.sort_by_key(|&i| rank(&self.0[i]));
            ranked.truncate(keep);
            ranked.sort_unstable();
            hidden = shown.len() - ranked.len();
            shown = ranked;
        }
        let mut rows: Vec<Line> = shown
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                let last = hidden == 0 && n + 1 == shown.len();
                self.0[i].row(i + 1, width, last, frame, cols)
            })
            .collect();
        if hidden > 0 {
            let shown_running = shown.iter().filter(|&&i| self.0[i].running()).count();
            let mut more = format!("… {hidden} more");
            if let Some(running) = self.running().checked_sub(shown_running).filter(|&n| n > 0) {
                more.push_str(&format!(" · {running} running"));
            }
            rows.push(Line::default().dim("  └─ ").dim(more).truncate(cols));
        }
        rows
    }

    /// Each child's session, in the order [`Self::agents`] lists them.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionId> {
        self.0.iter().map(|child| child.session).collect()
    }

    /// Each child as the deck's panel lists it.
    #[must_use]
    pub fn agents(&self, frame: usize) -> Vec<AgentRow> {
        let now = Timestamp::now();
        self.0
            .iter()
            .enumerate()
            .map(|(index, child)| {
                let mark = match &child.state {
                    State::Running => Line::default().brass(SPINNER[frame % SPINNER.len()]),
                    State::Done(_) => Line::default().verdigris("●"),
                    State::Failed(_) => Line::default().sanguine("✕"),
                };
                let doing = match &child.state {
                    State::Running => match child.open.first() {
                        Some((_, tool, arg)) if arg.is_empty() => Line::default().muted(tool),
                        Some((_, tool, arg)) => Line::default().muted(tool).dim(" ").dim(arg),
                        None => Line::default().dim("thinking"),
                    },
                    State::Done(text) => inline(
                        text.lines()
                            .find(|line| !line.trim().is_empty())
                            .unwrap_or("done")
                            .trim(),
                        Role::Dim,
                    ),
                    State::Failed(message) => {
                        Line::default().sanguine(message.lines().next().unwrap_or("failed"))
                    }
                };
                let elapsed = child.ended.unwrap_or(now).0.saturating_sub(child.started.0);
                AgentRow {
                    mark,
                    title: child
                        .label
                        .clone()
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| format!("agent {}", index + 1)),
                    doing,
                    detail: duration(elapsed),
                }
            })
            .collect()
    }

    /// Open-sided slips and complete answers for the call's finished children.
    pub(crate) fn committed(&self, m: Measure) -> (Vec<Line>, Vec<Detail>) {
        let mut lines = Vec::new();
        let mut details = Vec::new();
        for (index, child) in self.0.iter().enumerate() {
            if let Some((slip, detail)) = child.slip(index + 1, m) {
                lines.extend(slip);
                details.push(detail);
            }
        }
        (lines, details)
    }

    /// Forgets every child.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use xi_protocol::EventId;
    use xi_protocol::event::ModelRef;

    use super::*;

    fn event(kind: EventKind) -> Event {
        Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp::now(),
            kind,
        }
    }

    #[test]
    fn past_eight_children_failed_and_running_keep_rows_and_the_rest_count() {
        let parent = SessionId::generate();
        let mut children = Children::default();
        let ids: Vec<_> = (0..10).map(|_| SessionId::generate()).collect();
        for &id in &ids {
            let root = event(EventKind::SessionStarted {
                session: id,
                cwd: "/w".into(),
                model: ModelRef {
                    provider: "p".into(),
                    model: "m".into(),
                },
                xi_version: "test".into(),
                parent_session: Some(parent),
            });
            assert!(children.event(parent, id, &root, None));
        }
        // The last child fails; the other nine keep running.
        let failed = event(EventKind::TurnFailed {
            message: "boom".into(),
        });
        assert!(children.event(parent, ids[9], &failed, None));
        let rows: Vec<String> = children.live(100, 0).iter().map(Line::plain).collect();
        assert_eq!(rows.len(), LIVE_ROWS, "{rows:#?}");
        assert!(rows[0].starts_with("  ├─ ┤ 1 "), "{rows:#?}");
        assert!(rows[5].starts_with("  ├─ ┤ 6 "), "{rows:#?}");
        assert!(rows[6].starts_with("  ├─ ✕ 10"), "{rows:#?}");
        assert_eq!(rows[7], "  └─ … 3 more · 3 running");
        assert_eq!(children.running(), 9);
        let (slips, details) = children.committed(Measure::new(100, 36));
        assert_eq!(slips.len(), 2);
        assert_eq!(details.len(), 1);
    }

    fn child() -> Child {
        Child {
            session: SessionId::generate(),
            started: Timestamp(0),
            ended: None,
            label: None,
            calls: 0,
            open: Vec::new(),
            usage: Totals::default(),
            state: State::Running,
        }
    }

    #[test]
    fn finished_child_slip_keeps_the_full_answer_and_first_prompt_label() {
        let mut child = child();
        for prompt in ["check keys", "later steering"] {
            child.event(
                &event(EventKind::UserMessage {
                    content: vec![Block::Text {
                        text: prompt.into(),
                    }],
                }),
                None,
            );
        }
        let mut answer = event(EventKind::AssistantMessage {
            model: ModelRef {
                provider: "p".into(),
                model: "m".into(),
            },
            content: vec![
                Block::Text {
                    text: "one\ntwo\nthree\nfour".into(),
                },
                Block::Text {
                    text: "five".into(),
                },
            ],
            stop: StopReason::EndTurn,
            usage: Default::default(),
        });
        answer.at = Timestamp((14 * 60 + 22) * 60_000);
        child.event(&answer, None);
        let live = child.row(1, 1, true, 0, 96).plain();
        assert!(live.starts_with("  └─ ● 1  one"), "{live}");
        let (lines, detail) = child.slip(1, Measure::new(96, 36)).unwrap();
        let text: Vec<_> = lines.iter().map(Line::plain).collect();
        assert_eq!(
            text,
            [
                "» from child 1 · check keys · 14:22",
                "  one",
                "  two",
                "  three",
                "  … 2 more lines · ctrl+o",
            ]
        );
        assert!(lines[0].0.iter().all(|span| span.role == Role::Dim));
        assert_eq!(detail.title, "from child 1 · check keys");
        assert_eq!(detail.text, "one\ntwo\nthree\nfour\nfive");
        assert!(!detail.error);
    }

    #[test]
    fn child_slips_wrap_and_count_display_rows_without_boxes() {
        let child = Child {
            state: State::Done("abcdefghij".into()),
            ended: Some(Timestamp(60_000)),
            ..child()
        };
        let (lines, detail) = child.slip(2, Measure::new(4, 36)).unwrap();
        let text: Vec<_> = lines.iter().map(Line::plain).collect();
        assert_eq!(text[1..4], ["  ab", "  cd", "  ef"]);
        assert!(lines.iter().all(|line| line.width() <= 4));
        assert_eq!(detail.text, "abcdefghij");
        assert_eq!(detail.title, "from child 2");
        let (lines, _) = child.slip(2, Measure::new(28, 36)).unwrap();
        assert_eq!(lines[0].plain(), "» from child 2 · 00:01");
        assert_eq!(lines[1].plain(), "  abcdefghij");
    }

    #[test]
    fn failed_children_keep_a_sanguine_mark_and_reason_and_running_children_do_not_commit() {
        let mut child = child();
        assert!(child.slip(1, Measure::new(80, 36)).is_none());
        child.event(
            &event(EventKind::TurnFailed {
                message: "first reason\nsecond reason".into(),
            }),
            None,
        );
        let (lines, detail) = child.slip(1, Measure::new(80, 36)).unwrap();
        assert!(lines[0].plain().starts_with("✕ from child 1 · "));
        assert_eq!(lines[0].0[0].role, Role::Sanguine);
        assert_eq!(lines[1].plain(), "  first reason");
        assert_eq!(lines[2].plain(), "  second reason");
        assert!(
            lines[1..]
                .iter()
                .all(|line| line.0.last().unwrap().role == Role::Sanguine)
        );
        assert!(detail.error);
        assert_eq!(detail.text, "first reason\nsecond reason");
    }
}
