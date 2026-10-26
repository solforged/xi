//! Child sessions a running call started, one row each.
//!
//! A subagent is a child session that runs inside its parent's worker. Its
//! events arrive alongside the parent's, tagged with the child's session id.
//! While the call that started them runs, each child gets a row in the live
//! region: its number in start order, what it is doing now, and its calls and
//! time so far. When the call's result lands, the rows commit under the
//! call's headline with each child's answer in place of what it was doing.

use std::path::Path;

use xi_protocol::event::StopReason;
use xi_protocol::{Block, Event, EventKind, SessionId, Timestamp, ToolCallId};

use crate::style::{Line, Role, inline, lr};
use crate::transcript::{SPINNER, Totals, counted, duration, headline_arg};

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
                        .filter_map(|b| match b {
                            Block::Text { text } => text.lines().find(|l| !l.trim().is_empty()),
                            _ => None,
                        })
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned()
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
                self.state = State::Failed(message.lines().next().unwrap_or("").to_owned());
                self.ended = Some(event.at);
            }
            _ => {}
        }
    }

    /// `├─ ● 2  what` on the left, `3 calls · 12.0s` on the right.
    fn row(
        &self,
        number: usize,
        width: usize,
        last: bool,
        live: Option<usize>,
        cols: usize,
    ) -> Line {
        let elbow = Line::default().dim(if last { "  └─ " } else { "  ├─ " });
        let mark = match (&self.state, live) {
            (State::Running, Some(frame)) => Line::default().brass(SPINNER[frame % SPINNER.len()]),
            (State::Running, None) => Line::default().dim("·"),
            (State::Done(_), _) => Line::default().verdigris("●"),
            (State::Failed(_), _) => Line::default().sanguine("✕"),
        };
        let mut left = elbow
            .extend(mark)
            .ink(" ")
            .strong(Role::Ink, format!("{number:<width$}"))
            .ink("  ");
        left = match &self.state {
            State::Running => match (self.open.first(), live) {
                (Some((_, tool, arg)), Some(_)) => {
                    let line = left.dim(tool);
                    if arg.is_empty() {
                        line
                    } else {
                        line.ink(" ").ink(arg)
                    }
                }
                (None, Some(_)) => left.dim("thinking"),
                (_, None) => left.dim("no answer"),
            },
            State::Done(text) if text.is_empty() => left.dim("done"),
            State::Done(text) => left.extend(inline(text, Role::Ink)),
            State::Failed(message) => left.sanguine("failed").dim(" · ").ink(message),
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
                self.0[i].row(i + 1, width, last, Some(frame), cols)
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

    /// Every child with its answer, for the call's committed block.
    #[must_use]
    pub fn committed(&self, cols: usize) -> Vec<Line> {
        let width = self.0.len().to_string().len();
        self.0
            .iter()
            .enumerate()
            .map(|(i, c)| c.row(i + 1, width, i + 1 == self.0.len(), None, cols))
            .collect()
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
        assert_eq!(children.committed(100).len(), 10);
    }
}
