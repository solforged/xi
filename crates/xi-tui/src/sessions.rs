//! Workspace and cross-workspace session selection.

use xi_protocol::methods::client::SessionSummary;
use xi_protocol::{SessionId, Timestamp};

use crossterm::event::KeyEvent;

use crate::frame::{Body, Cursor, Frame, Hint, Tone};
use crate::keys::{Action, Keymap};
use crate::overlay::{Command, Ctx, ListKey, Outcome, Overlay, list_key};
use crate::style::{Line, row};
use crate::transcript::SPINNER;

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

    /// Focuses the most recently active session other than the current one.
    pub(crate) fn select_previous(&mut self) {
        self.selected = self
            .matches
            .iter()
            .position(|index| self.sessions[*index].session != self.current)
            .unwrap_or(0);
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

    /// The border, the scope and position, and the keys.
    #[must_use]
    pub fn frame(&self, keys: &Keymap) -> Frame {
        let scope = if self.all {
            "all workspaces"
        } else {
            "this workspace"
        };
        let info = if self.matches.is_empty() {
            scope.to_owned()
        } else {
            format!("{scope} · {} of {}", self.selected + 1, self.matches.len())
        };
        let mut hints: Vec<Hint> = Hint::key(keys, Action::Send, "opens", 0)
            .into_iter()
            .collect();
        if let Some(tab) = keys.key(Action::Complete) {
            let (long, short) = if self.all {
                ("this workspace", "this")
            } else {
                ("all workspaces", "all")
            };
            hints.push(Hint::new(format!("{tab} {long}"), 2).short(format!("{tab} {short}")));
        }
        hints.extend(Hint::key(keys, Action::Escape, "closes", 0));
        Frame {
            tone: Tone::Focus,
            title: Line::default().ink("sessions"),
            info: Some(Line::default().dim(info)),
            hints,
        }
    }

    /// The filter row and one row per session, `inner` wide: `●` the one
    /// you're in, a spinner for a running one, `?` for one waiting on you.
    #[must_use]
    pub fn body(&self, inner: usize, now: Timestamp) -> Body {
        let head = vec![Line::default().dim("filter ").ink(&self.query)];
        let cursor = Some(Cursor::Head(
            0,
            unicode_width::UnicodeWidthStr::width(format!("filter {}", self.query).as_str()),
        ));
        if self.matches.is_empty() {
            return Body {
                head,
                list: vec![Line::default().dim("no matching sessions")],
                cursor,
                ..Body::default()
            };
        }
        // Ages pad to one width, so the models before them line up too.
        let ages: Vec<String> = self
            .matches
            .iter()
            .map(|index| age(self.sessions[*index].updated, now))
            .collect();
        let age_width = ages.iter().map(String::len).max().unwrap_or(0);
        let list = self
            .matches
            .iter()
            .enumerate()
            .map(|(position, index)| {
                let entry = &self.sessions[*index];
                let here = position == self.selected;
                let mark = if entry.session == self.current {
                    Line::default().brass("●")
                } else if entry.pending_approvals > 0 {
                    Line::default().brass("?")
                } else if entry.running {
                    Line::default().brass(SPINNER[0])
                } else {
                    Line::default().ink(" ")
                };
                let title = entry
                    .title
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or("(no prompt yet)");
                let left = if here {
                    Line::default()
                        .brass("› ")
                        .extend(mark)
                        .brass(format!(" {title}"))
                } else {
                    Line::default()
                        .ink("  ")
                        .extend(mark)
                        .ink(format!(" {title}"))
                };
                let mut right = Line::default();
                if entry.pending_approvals > 0 {
                    right = right
                        .brass(format!("? {} waiting", entry.pending_approvals))
                        .dim(" · ");
                } else if entry.running {
                    right = right.dim("running · ");
                }
                if self.all {
                    right = right.dim(format!("{} · ", crate::view::home(&entry.cwd)));
                }
                right = right.dim(format!(
                    "{} · {:>age_width$}",
                    entry.model.model, ages[position]
                ));
                row(left, right, inner)
            })
            .collect();
        Body {
            head,
            list,
            selected: Some(self.selected),
            cursor,
            ..Body::default()
        }
    }
}

impl Overlay for Sessions {
    fn name(&self) -> &'static str {
        "sessions"
    }

    fn frame(&self, ctx: &Ctx) -> Frame {
        Self::frame(self, ctx.keys)
    }

    fn body(&self, inner: usize, ctx: &Ctx) -> Body {
        Self::body(self, inner, ctx.now)
    }

    fn key(&mut self, key: KeyEvent, action: Option<Action>, _: &Ctx) -> Outcome {
        match list_key(key, action) {
            ListKey::Choose => {
                if let Some(session) = self.selected() {
                    return Outcome::Send(Box::new(Command::Resume(session.clone())));
                }
            }
            ListKey::Tab => return Outcome::Send(Box::new(Command::Sessions { all: !self.all })),
            ListKey::Close => return Outcome::Close,
            ListKey::Up => self.up(),
            ListKey::Down => self.down(),
            ListKey::Backspace => self.backspace(),
            ListKey::Type(c) => self.input(c),
            _ => {}
        }
        Outcome::Stay
    }
}

pub(crate) fn age(updated: Option<Timestamp>, now: Timestamp) -> String {
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
