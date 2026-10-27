//! Pending approvals and their available answer scopes.

use std::path::PathBuf;

use xi_protocol::event::Remember;
use xi_protocol::methods::client::PendingApproval;
use xi_protocol::{SessionId, ToolCallId};

use crate::frame::{Body, Frame, Hint, Note, Tone};
use crate::keys::{Action, Keymap};
use crate::style::{Line, Role, lr};

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

    /// What choosing it does, for the frame's foot note.
    fn consequence(self) -> &'static str {
        match self {
            Self::Once => "Allowed once; the next matching call asks again.",
            Self::Session => {
                "The same tool and action run without asking for the rest of this branch."
            }
            Self::Workspace => {
                "Also saves an allow rule for this exact action, which new sessions in this workspace read."
            }
            Self::Deny => "The call is refused and the model sees why.",
        }
    }
}

/// Pending asks shown one at a time, with answer selection and inbox mode.
pub struct Approvals {
    /// Whether the overlay includes other sessions.
    pub inbox: bool,
    entries: Vec<Approval>,
    /// The ask being shown.
    at: usize,
    choice: usize,
}

impl Approvals {
    /// Opens on the first supplied ask.
    #[must_use]
    pub fn new(entries: Vec<Approval>, inbox: bool) -> Self {
        Self {
            inbox,
            entries,
            at: 0,
            choice: 0,
        }
    }

    /// The ask currently being shown.
    #[must_use]
    pub fn current(&self) -> Option<&Approval> {
        self.entries.get(self.at)
    }

    /// Replaces the asks, staying on the same call when it is still waiting.
    pub fn replace(&mut self, entries: Vec<Approval>) {
        let shown = self
            .current()
            .map(|old| (old.pending.session, old.pending.call.clone()));
        let same = shown.and_then(|(session, call)| {
            entries
                .iter()
                .position(|new| new.pending.session == session && new.pending.call == call)
        });
        self.entries = entries;
        match same {
            Some(at) => self.at = at,
            None => {
                self.at = self.at.min(self.entries.len().saturating_sub(1));
                self.choice = 0;
            }
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
        let mut entries = self.entries.clone();
        if let Some(existing) = entries.iter_mut().find(|old| {
            old.pending.session == entry.pending.session && old.pending.call == entry.pending.call
        }) {
            *existing = entry;
        } else if entry.pending.session == current {
            let at = entries
                .iter()
                .take_while(|old| old.pending.session == current)
                .count();
            entries.insert(at, entry);
        } else {
            entries.push(entry);
        }
        self.replace(entries);
    }

    /// Drops an ask whose answer has been accepted or observed in the log.
    pub fn remove(&mut self, session: SessionId, call: &ToolCallId) {
        self.entries
            .retain(|entry| entry.pending.session != session || entry.pending.call != *call);
        self.at = self.at.min(self.entries.len().saturating_sub(1));
        self.choice = 0;
    }

    /// Shows the next waiting ask, wrapping after the last.
    pub fn next(&mut self) {
        if !self.entries.is_empty() {
            self.at = (self.at + 1) % self.entries.len();
            self.choice = 0;
        }
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

    /// The border, title, count and keys. Murex when the ask is on a
    /// tainted branch.
    #[must_use]
    pub fn frame(&self, keys: &Keymap) -> Frame {
        let ask = self.current().map(|entry| &entry.pending);
        let taint = ask.is_some_and(|ask| ask.taint);
        let mut title = Line::default();
        if taint {
            title = title.murex("◆ ");
        }
        title = title.brass("?").ink(if self.inbox {
            " approvals inbox"
        } else {
            " approval"
        });
        if let Some(ask) = ask {
            title = title.dim(format!(" · {}", ask.tool));
        }
        let info = if taint {
            Some(Line::default().murex("tainted branch"))
        } else {
            (self.entries.len() > 1).then(|| {
                Line::default().dim(format!("{} of {} waiting", self.at + 1, self.entries.len()))
            })
        };
        let numbers: Vec<char> = self.options().iter().map(|c| c.number()).collect();
        let mut hints = if taint {
            vec![Hint::new("1 allow", 0), Hint::new("4 deny", 0)]
        } else {
            let keys = match numbers.as_slice() {
                [first, .., last] if *last as u32 - *first as u32 + 1 == numbers.len() as u32 => {
                    format!("{first}–{last}")
                }
                _ => numbers
                    .iter()
                    .map(char::to_string)
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            vec![Hint::new(format!("{keys} choose"), 0).short(keys)]
        };
        hints.extend(Hint::moves(keys, 3));
        if self.entries.len() > 1 {
            hints.extend(
                Hint::key(keys, Action::Complete, "next ask", 2).map(|hint| {
                    hint.short(format!(
                        "{} next",
                        keys.key(Action::Complete).unwrap_or_default()
                    ))
                }),
            );
        }
        hints.extend(Hint::key(keys, Action::Escape, "later", 0));
        Frame {
            tone: if taint { Tone::Taint } else { Tone::Focus },
            title,
            info,
            hints,
        }
    }

    /// The ask's action, its policy context and the available answers,
    /// `inner` wide.
    #[must_use]
    pub fn body(&self, inner: usize, current: SessionId) -> Body {
        let Some(entry) = self.current() else {
            return Body::default();
        };
        let ask = &entry.pending;
        let mut facts = vec![
            ("tool".to_owned(), Line::default().ink(&ask.tool)),
            (
                "rule".to_owned(),
                Line::default().ink(ask.rule.as_deref().unwrap_or("none matched")),
            ),
        ];
        if ask.session != current {
            let id = ask.session.to_string();
            let cwd = entry
                .cwd
                .as_deref()
                .map(crate::view::home)
                .unwrap_or_else(|| "(workspace unavailable)".into());
            facts.push(("session".into(), Line::default().ink(&id[..8])));
            facts.push(("workspace".into(), Line::default().ink(cwd)));
        } else {
            facts.push((
                "workspace".into(),
                Line::default().ink(if ask.trusted { "trusted" } else { "untrusted" }),
            ));
        }
        let list = self
            .options()
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let selected = index == self.choice;
                let left = Line::default().span(
                    if selected { Role::Brass } else { Role::Ink },
                    false,
                    format!(
                        "{} {} {}",
                        if selected { "›" } else { " " },
                        choice.number(),
                        choice.label()
                    ),
                );
                if *choice == Choice::Workspace {
                    lr(
                        left,
                        Line::default().dim("writes .xi/config.local.toml"),
                        inner,
                    )
                } else {
                    left
                }
            })
            .collect();
        let mut foot = Vec::new();
        if ask.taint {
            foot.push(Note::marked(
                Line::default().murex("◆"),
                "This branch read untrusted content, so session and workspace grants are not offered.",
            ));
        } else if let Some(choice) = self.selected() {
            foot.push(Note::new(choice.consequence()));
        }
        Body {
            head: vec![Line::default().strong(Role::Ink, &ask.action)],
            facts,
            list,
            selected: Some(self.choice),
            foot,
            cursor: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(session: SessionId, call: &str) -> Approval {
        Approval {
            pending: PendingApproval {
                session,
                call: call.into(),
                tool: "bash".into(),
                action: format!("exec {call}"),
                rule: None,
                taint: false,
                trusted: true,
            },
            cwd: None,
        }
    }

    fn shown(approvals: &Approvals) -> String {
        approvals.current().unwrap().pending.call.to_string()
    }

    #[test]
    fn tab_walks_asks_and_refreshes_keep_the_shown_one() {
        let (here, there) = (SessionId::generate(), SessionId::generate());
        let mut approvals = Approvals::new(vec![ask(here, "a"), ask(there, "b")], true);
        approvals.step(true);
        approvals.next();
        assert_eq!(shown(&approvals), "b");
        assert_eq!(
            approvals.selected(),
            Some(Choice::Once),
            "a new ask starts on once"
        );
        approvals.step(true);
        // A new ask of this session goes first, and the shown ask stays.
        approvals.upsert(ask(here, "c"), here);
        assert_eq!(shown(&approvals), "b");
        assert_eq!(approvals.selected(), Some(Choice::Session));
        approvals.next();
        assert_eq!(shown(&approvals), "a", "wraps after the last");
        approvals.remove(here, &"a".into());
        approvals.remove(there, &"b".into());
        assert_eq!(shown(&approvals), "c");
        approvals.remove(here, &"c".into());
        assert!(approvals.current().is_none());
    }
}
