//! Pending approvals and their available answer scopes.

use std::path::PathBuf;

use xi_protocol::event::Remember;
use xi_protocol::methods::client::PendingApproval;
use xi_protocol::{SessionId, ToolCallId};

use crate::style::{Line, Role};

/// An ask with the workspace information needed by the inbox.
#[derive(Clone)]
pub struct Approval {
    /// The daemon's pending ask.
    pub pending: PendingApproval,
    /// The owning workspace, when known.
    pub cwd: Option<PathBuf>,
}

/// A visible answer, including its stable numeric shortcut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Allow only this call.
    Once,
    /// Allow the same tool and action on this branch.
    Session,
    /// Also save an allow rule in the trusted workspace.
    Workspace,
    /// Reject this call.
    Deny,
}

impl Choice {
    /// Whether this answer approves the call.
    #[must_use]
    pub fn approved(self) -> bool {
        self != Self::Deny
    }

    /// The lifetime sent to the worker.
    #[must_use]
    pub fn remember(self) -> Remember {
        match self {
            Self::Once | Self::Deny => Remember::Once,
            Self::Session => Remember::Session,
            Self::Workspace => Remember::Workspace,
        }
    }

    fn number(self) -> char {
        match self {
            Self::Once => '1',
            Self::Session => '2',
            Self::Workspace => '3',
            Self::Deny => '4',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Once => "Allow once",
            Self::Session => "Allow for this session",
            Self::Workspace => "Always allow in this workspace",
            Self::Deny => "Deny",
        }
    }
}

/// One pending ask at a time, with answer selection and inbox mode.
pub struct Approvals {
    /// Whether the overlay includes other sessions.
    pub inbox: bool,
    entries: Vec<Approval>,
    choice: usize,
}

impl Approvals {
    /// Opens on the first supplied ask.
    #[must_use]
    pub fn new(entries: Vec<Approval>, inbox: bool) -> Self {
        Self {
            inbox,
            entries,
            choice: 0,
        }
    }

    /// The ask currently being shown.
    #[must_use]
    pub fn current(&self) -> Option<&Approval> {
        self.entries.first()
    }

    /// Replaces the inbox while preserving the selection for the same call.
    pub fn replace(&mut self, entries: Vec<Approval>) {
        let same = self
            .current()
            .zip(entries.first())
            .is_some_and(|(old, new)| {
                old.pending.session == new.pending.session && old.pending.call == new.pending.call
            });
        self.entries = entries;
        if !same {
            self.choice = 0;
        }
        self.choice = self.choice.min(self.options().len().saturating_sub(1));
    }

    /// Refreshes current-session asks without losing other inbox entries.
    pub fn replace_current(&mut self, mut entries: Vec<Approval>, current: SessionId) {
        if self.inbox {
            entries.extend(
                self.entries
                    .iter()
                    .filter(|entry| entry.pending.session != current)
                    .cloned(),
            );
        }
        self.replace(entries);
    }

    /// Adds or updates an ask delivered by a daemon notification.
    pub fn upsert(&mut self, entry: Approval, current: SessionId) {
        if let Some(existing) = self.entries.iter_mut().find(|old| {
            old.pending.session == entry.pending.session && old.pending.call == entry.pending.call
        }) {
            *existing = entry;
        } else if entry.pending.session == current {
            let at = self
                .entries
                .iter()
                .take_while(|old| old.pending.session == current)
                .count();
            self.entries.insert(at, entry);
        } else {
            self.entries.push(entry);
        }
    }

    /// Drops an ask whose answer has been accepted or observed in the log.
    pub fn remove(&mut self, session: SessionId, call: &ToolCallId) {
        self.entries
            .retain(|entry| entry.pending.session != session || entry.pending.call != *call);
        self.choice = 0;
    }

    /// The available scopes, in numeric shortcut order.
    #[must_use]
    pub fn options(&self) -> &'static [Choice] {
        let Some(entry) = self.current() else {
            return &[];
        };
        if entry.pending.taint {
            &[Choice::Once, Choice::Deny]
        } else if entry.pending.trusted {
            &[
                Choice::Once,
                Choice::Session,
                Choice::Workspace,
                Choice::Deny,
            ]
        } else {
            &[Choice::Once, Choice::Session, Choice::Deny]
        }
    }

    /// Moves through visible choices, wrapping at either end.
    pub fn step(&mut self, down: bool) {
        let count = self.options().len();
        if count > 0 {
            self.choice = if down {
                (self.choice + 1) % count
            } else {
                (self.choice + count - 1) % count
            };
        }
    }

    /// The selected visible answer.
    #[must_use]
    pub fn selected(&self) -> Option<Choice> {
        self.options().get(self.choice).copied()
    }

    /// Looks up a numeric or yes/no shortcut without renumbering hidden scopes.
    #[must_use]
    pub fn shortcut(&self, key: char) -> Option<Choice> {
        let key = match key {
            'y' => '1',
            'n' => '4',
            other => other,
        };
        self.options()
            .iter()
            .copied()
            .find(|choice| choice.number() == key)
    }

    /// Draws the ask, its policy context and the available answers.
    #[must_use]
    pub fn render(&self, cols: usize, current: SessionId) -> Vec<Line> {
        let Some(entry) = self.current() else {
            return Vec::new();
        };
        let ask = &entry.pending;
        let border = if ask.taint { Role::Murex } else { Role::Brass };
        let b = |s: String| Line::default().span(border, false, s);
        let inner = cols.saturating_sub(4);
        let mut rows = vec![Line::default().strong(Role::Ink, &ask.action)];
        if ask.session != current {
            let id = ask.session.to_string();
            let cwd = entry
                .cwd
                .as_deref()
                .map(crate::view::home)
                .unwrap_or_else(|| "(workspace unavailable)".into());
            rows.push(Line::default().dim(format!("session {} · {cwd}", &id[..8])));
        }
        rows.push(Line::default().dim("tool  ").ink(&ask.tool));
        if let Some(rule) = &ask.rule {
            rows.push(Line::default().dim("rule  ").ink(rule));
        }
        if ask.taint {
            rows.push(Line::default().dim("tainted branch: only once or deny"));
        }
        rows.push(Line::default());
        for (index, choice) in self.options().iter().copied().enumerate() {
            let prefix = if index == self.choice { "›" } else { " " };
            rows.push(Line::default().span(
                if index == self.choice {
                    Role::Brass
                } else {
                    Role::Ink
                },
                index == self.choice,
                format!("{prefix} {} {}", choice.number(), choice.label()),
            ));
        }
        if self.entries.len() > 1 {
            rows.push(Line::default().dim(format!(
                "{} more waiting after this one",
                self.entries.len() - 1
            )));
        }
        let title = if self.inbox {
            "approvals inbox"
        } else {
            "approval"
        };
        let top = b(format!("╭─ ? {title} · esc later "));
        let fill = cols.saturating_sub(top.width() + 1);
        let mut lines = vec![
            top.extend(b("─".repeat(fill)))
                .extend(b("╮".into()))
                .truncate(cols),
        ];
        for row in rows {
            let row = row.truncate(inner);
            let pad = inner.saturating_sub(row.width());
            lines.push(
                b("│ ".into())
                    .extend(row)
                    .ink(" ".repeat(pad))
                    .extend(b(" │".into())),
            );
        }
        lines.push(b(format!("╰{}╯", "─".repeat(cols.saturating_sub(2)))));
        lines
    }
}
