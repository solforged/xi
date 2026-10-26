//! Workspace and cross-workspace session selection.

use xi_protocol::methods::client::SessionSummary;
use xi_protocol::{SessionId, Timestamp};

use crate::style::{Line, Role};

/// A filterable list of top-level sessions, newest activity first.
pub struct Sessions {
    sessions: Vec<SessionSummary>,
    current: SessionId,
    /// Whether the list includes every workspace.
    pub all: bool,
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

impl Sessions {
    /// Opens the picker on the current session, when it is listed.
    #[must_use]
    pub fn new(sessions: Vec<SessionSummary>, current: SessionId) -> Self {
        let mut picker = Self {
            sessions: Vec::new(),
            current,
            all: false,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        picker.replace(sessions, false);
        picker.selected = picker
            .matches
            .iter()
            .position(|index| picker.sessions[*index].session == current)
            .unwrap_or(0);
        picker
    }

    /// Replaces the fetched rows, keeping the filter and selected session.
    pub fn replace(&mut self, mut sessions: Vec<SessionSummary>, all: bool) {
        let selected = self.selected().map(|entry| entry.session);
        sessions.retain(|entry| entry.parent_session.is_none());
        sessions.sort_by(|a, b| {
            b.updated
                .cmp(&a.updated)
                .then_with(|| a.session.cmp(&b.session))
        });
        self.sessions = sessions;
        self.all = all;
        self.filter();
        if let Some(selected) = selected {
            self.selected = self
                .matches
                .iter()
                .position(|index| self.sessions[*index].session == selected)
                .unwrap_or(0);
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let title = entry.title.as_deref().unwrap_or("");
                (title.to_lowercase().contains(&query)
                    || entry.cwd.to_string_lossy().to_lowercase().contains(&query))
                .then_some(index)
            })
            .collect();
        self.selected = 0;
    }

    /// Adds one filter character.
    pub fn input(&mut self, c: char) {
        self.query.push(c);
        self.filter();
    }

    /// Removes the newest filter character.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.filter();
    }

    /// Selects the previous matching session.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Selects the next matching session.
    pub fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
    }

    /// The selected session, if the filter matches any.
    #[must_use]
    pub fn selected(&self) -> Option<&SessionSummary> {
        self.matches
            .get(self.selected)
            .map(|index| &self.sessions[*index])
    }

    /// Draws the query and visible rows, keeping the selection in view.
    #[must_use]
    pub fn render(&self, cols: usize, rows: usize, now: Timestamp) -> Vec<Line> {
        if rows == 0 {
            return Vec::new();
        }
        let scope = if self.all {
            "all workspaces"
        } else {
            "this workspace"
        };
        let query = if self.query.is_empty() {
            "type to filter"
        } else {
            self.query.as_str()
        };
        let mut lines = vec![
            Line::default()
                .brass("sessions")
                .dim(format!(
                    " · {scope} · {query} · tab toggles · enter selects · esc closes"
                ))
                .truncate(cols),
        ];
        if rows == 1 {
            return lines;
        }
        if self.matches.is_empty() {
            lines.push(Line::default().dim("  no matching sessions").truncate(cols));
            return lines;
        }
        let capacity = rows - 1;
        let start = self.selected.saturating_sub(capacity.saturating_sub(1) / 2);
        for (position, index) in self.matches.iter().enumerate().skip(start).take(capacity) {
            let entry = &self.sessions[*index];
            let cursor = if position == self.selected {
                "›"
            } else {
                " "
            };
            let current = if entry.session == self.current {
                "●"
            } else {
                " "
            };
            let title = entry
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("(no prompt yet)");
            let mut label = format!(
                "{cursor} {current} {title} · {} · {}",
                entry.model.model,
                age(entry.updated, now)
            );
            if self.all {
                label.push_str(&format!(" · {}", crate::view::home(&entry.cwd)))
            }
            if entry.running {
                label.push_str(" · running")
            }
            if entry.pending_approvals > 0 {
                label.push_str(&format!(" · ? {}", entry.pending_approvals))
            }
            lines.push(
                Line::default()
                    .span(
                        if position == self.selected {
                            Role::Brass
                        } else {
                            Role::Ink
                        },
                        false,
                        label,
                    )
                    .truncate(cols),
            );
        }
        lines
    }
}

fn age(updated: Option<Timestamp>, now: Timestamp) -> String {
    let Some(updated) = updated else {
        return "?".into();
    };
    let minutes = now.0.saturating_sub(updated.0) / 60_000;
    match minutes {
        0 => "now".into(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}
