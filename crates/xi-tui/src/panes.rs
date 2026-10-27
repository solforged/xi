//! The deck as the client runs it: its retained log, the pane with the
//! keys, the sessions and asks it lists, and the scene drawn each frame.
//!
//! [`crate::deck`] composes the screen from a [`Scene`]; this module fills
//! the scene from the client's state and carries out the deck's keys.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent};
use xi_client::client::ClientNotification;
use xi_protocol::Timestamp;
use xi_protocol::methods::client::{EventParams, ListParams, SessionSummary, SettingsReport};

use crate::approvals::Approval;
use crate::deck::{
    self, AskRow, ContextGauge, Focus, LogPane, Mark, Panel, Scene, SessionRow, SessionsPane,
};
use crate::keys::Action;
use crate::layout::Measure;
use crate::log::{Inspect, Log};
use crate::screen::Screen;
use crate::sessions::age;
use crate::style::{Line, Role};
use crate::transcript::{Transcript, clock};
use crate::view::home;
use crate::{App, lineage};

/// How long the sessions and asks lists stay fresh.
const REFRESH: Duration = Duration::from_secs(5);

/// Footer segments the deck's header carries beside the title.
const HEADER: [&str; 5] = ["model", "reasoning", "children", "taint", "approvals"];

/// Footer segments the header adds when the side panel, which shows them,
/// is folded.
const STATUS: [&str; 4] = ["context", "cache", "sandbox", "sandbox-off"];

/// The deck's state while the client owns the screen.
#[derive(Default)]
pub(crate) struct Deck {
    /// Committed rows and the rows the inspector can open.
    pub log: Log,
    /// Leading log rows already in scrollback, which leaving the deck must
    /// not print again.
    pub printed: usize,
    /// The pane with the keys.
    pub focus: Focus,
    /// Every top-level session the daemon lists, newest first.
    pub sessions: Vec<SessionSummary>,
    /// Asks waiting in other sessions.
    pub asks: Vec<Approval>,
    /// Effective settings, for the inspector's session line.
    pub settings: Option<SettingsReport>,
    /// When the lists were last fetched.
    pub refreshed: Option<Instant>,
    /// Selected session in the sessions pane, over this workspace then the
    /// others.
    pub session: usize,
    /// Rows scrolled up in the inspector.
    pub inspector: usize,
}

impl App {
    /// Enters the deck. The log starts with the head's branch replayed, as
    /// scrollback already shows it, and committed rows go to it from here on.
    pub(crate) fn enter_deck(&mut self) {
        if self.deck.is_some() {
            return;
        }
        self.deck = Some(Deck::default());
        self.completion = None;
        let (rows, anchors) = self.replay();
        if let Some(deck) = self.deck.as_mut() {
            deck.printed = rows.len();
            deck.log.extend(rows, anchors);
        }
    }

    /// The head's branch as the log commits it at the current measure, with
    /// its inspector anchors. Rows the live transcript still holds stay out.
    fn replay(&self) -> (Vec<Line>, Vec<(usize, Inspect)>) {
        let m = self.measure();
        let session = self.session_id();
        let mut transcript = Transcript::default();
        transcript.set_thinking(self.transcript.thinking_display());
        let mut rows = Vec::new();
        let mut anchors = Vec::new();
        let mut take = |transcript: &mut Transcript, lines: Vec<Line>| {
            let base = rows.len();
            anchors.extend(
                transcript
                    .take_inspects()
                    .into_iter()
                    .map(|(at, inspect)| (base + at, inspect)),
            );
            rows.extend(lines);
        };
        let info = &self.launch.attached.info;
        for event in lineage(&self.launch.attached.events, info.head) {
            let n = ClientNotification::Event(EventParams { session, event });
            let lines = transcript.apply(&n, m);
            take(&mut transcript, lines);
        }
        if !self.running() {
            let lines = transcript.flush(m);
            take(&mut transcript, lines);
        }
        (rows, anchors)
    }

    /// Leaves the deck, printing the log rows scrollback lacks ahead of
    /// anything committed since.
    pub(crate) fn leave_deck(&mut self) {
        if let Some(mut deck) = self.deck.take() {
            let mut rows = deck.log.take();
            rows.drain(..deck.printed.min(rows.len()));
            self.commit.splice(0..0, rows);
        }
    }

    /// Starts the log over for a newly attached session.
    pub(crate) fn reset_deck_log(&mut self) {
        if let Some(deck) = self.deck.as_mut() {
            deck.log = Log::default();
            deck.printed = 0;
            deck.inspector = 0;
            deck.refreshed = None;
        }
    }

    /// Refreshes the sessions, asks and settings the deck lists once they
    /// go stale.
    pub(crate) async fn refresh_deck(&mut self) {
        let stale = self
            .deck
            .as_ref()
            .is_some_and(|deck| deck.refreshed.is_none_or(|at| at.elapsed() >= REFRESH));
        if !stale {
            return;
        }
        let sessions = self.launch.client.list(ListParams::default()).await;
        let pending = self.launch.client.approvals().await;
        let settings = if self.deck.as_ref().is_some_and(|d| d.settings.is_none()) {
            self.launch
                .client
                .settings(self.settings_params())
                .await
                .ok()
        } else {
            None
        };
        // This session's own asks are read live when the scene is built.
        let current = self.session_id();
        let mut asks = Vec::new();
        if let Ok(pending) = pending {
            asks.extend(
                pending
                    .into_iter()
                    .filter(|ask| {
                        ask.session != current
                            && !self.answered.contains(&(ask.session, ask.call.clone()))
                    })
                    .map(|pending| {
                        let cwd = self.approval_workspaces.get(&pending.session).cloned();
                        Approval { pending, cwd }
                    }),
            );
        }
        let Some(deck) = self.deck.as_mut() else {
            return;
        };
        if let Ok(mut sessions) = sessions {
            sessions.retain(|entry| entry.parent_session.is_none());
            sessions.sort_by(|a, b| {
                b.updated
                    .cmp(&a.updated)
                    .then_with(|| a.session.cmp(&b.session))
            });
            deck.sessions = sessions;
        }
        deck.asks = asks;
        if settings.is_some() {
            deck.settings = settings;
        }
        deck.refreshed = Some(Instant::now());
    }

    /// Whether the deck took `key`: its focus keys, and the keys of the
    /// pane with the focus.
    pub(crate) async fn deck_key(&mut self, key: KeyEvent, action: Option<Action>) -> bool {
        let page = self.rows / 2;
        let cols = self.cols;
        let Some(deck) = self.deck.as_mut() else {
            return false;
        };
        let focus = match action {
            Some(Action::FocusSessions) => Some(Focus::Sessions),
            Some(Action::FocusLog) => Some(Focus::Log),
            Some(Action::FocusInspector) => Some(Focus::Inspector),
            _ => None,
        };
        if let Some(focus) = focus {
            deck.focus = if deck.focus == focus {
                Focus::Composer
            } else {
                focus
            };
            match deck.focus {
                Focus::Composer => {
                    deck.log.select_newest();
                    deck.inspector = 0;
                }
                Focus::Log => {}
                Focus::Sessions | Focus::Inspector => deck.log.scroll_end(),
            }
            return true;
        }
        match (deck.focus, key.code) {
            (Focus::Composer, _) => return false,
            (_, KeyCode::Esc) => {
                // Back at the composer the panel follows the newest row.
                deck.focus = Focus::Composer;
                deck.log.select_newest();
                deck.inspector = 0;
            }
            (Focus::Sessions, KeyCode::Up) => deck.session = deck.session.saturating_sub(1),
            (Focus::Sessions, KeyCode::Down) => {
                let last = deck.sessions.len().saturating_sub(1);
                deck.session = (deck.session + 1).min(last);
            }
            (Focus::Sessions, KeyCode::Enter) => {
                let at = deck.session;
                if let Some(session) = self.deck_sessions().into_iter().nth(at)
                    && let Err(error) = self.resume(session).await
                {
                    self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                }
            }
            (Focus::Log, KeyCode::Up) => {
                deck.log.up();
                deck.inspector = 0;
            }
            (Focus::Log, KeyCode::Down) => {
                deck.log.down();
                deck.inspector = 0;
            }
            (Focus::Log, KeyCode::PageUp) => deck.log.scroll_up(page),
            (Focus::Log, KeyCode::PageDown) => deck.log.scroll_down(page),
            (Focus::Log, KeyCode::Enter) => {
                if let Some(inspect) = deck.log.selected().cloned() {
                    let lines = open(&inspect, self.measure());
                    self.note(lines);
                }
            }
            (Focus::Inspector, KeyCode::Up) => deck.inspector = deck.inspector.saturating_sub(1),
            (Focus::Inspector, KeyCode::Down) => {
                let last = deck
                    .log
                    .selected()
                    .map_or(0, |inspect| deck::inspector_rows(inspect, cols))
                    .saturating_sub(1);
                deck.inspector = (deck.inspector + 1).min(last);
            }
            _ => return false,
        }
        true
    }

    /// The sessions pane's rows in order: this workspace, then the others.
    fn deck_sessions(&self) -> Vec<SessionSummary> {
        let Some(deck) = &self.deck else {
            return Vec::new();
        };
        let cwd = &self.launch.attached.info.cwd;
        let (here, elsewhere): (Vec<_>, Vec<_>) = deck
            .sessions
            .iter()
            .cloned()
            .partition(|entry| &entry.cwd == cwd);
        here.into_iter().chain(elsewhere).collect()
    }

    /// Draws the deck.
    pub(crate) fn draw_deck<W: std::io::Write>(
        &mut self,
        screen: &mut Screen<W>,
    ) -> std::io::Result<()> {
        let (live, cursor) = self.live();
        let meta = self.log_meta();
        let scene = self.scene(&live, cursor, &meta);
        let (rows, cursor) = deck::render(&scene, self.cols, self.rows);
        screen.draw_deck(&rows, cursor)
    }

    /// The log's meta row: when the session started, its head and event
    /// count.
    fn log_meta(&self) -> String {
        let info = &self.launch.attached.info;
        let started = self
            .deck
            .as_ref()
            .and_then(|deck| deck.sessions.iter().find(|e| e.session == info.session))
            .and_then(|entry| entry.started);
        let mut meta = String::new();
        if let Some(started) = started {
            meta.push_str(&clock(started));
            meta.push_str(" · ");
        }
        let head = info.head.to_string();
        meta.push_str(&format!(
            "head #{} · {} events",
            &head[..head.len().min(8)],
            self.launch.attached.events.len()
        ));
        meta
    }

    /// The scene the deck draws from.
    fn scene<'a>(
        &'a self,
        live: &'a [Line],
        cursor: Option<(usize, usize)>,
        meta: &'a str,
    ) -> Scene<'a> {
        let deck = self.deck.as_ref().expect("scene needs the deck");
        let info = &self.launch.attached.info;
        let now = Timestamp::now();
        let cwd = &info.cwd;
        let mut header_left = Line::default()
            .brass("ξ ")
            .strong(Role::Ink, "xi")
            .ink(format!("  {}", home(cwd)));
        if let Some(branch) = self.git.label(cwd) {
            header_left = header_left.dim(format!("  {branch}"));
        }
        let header_right = self.joined(|name| HEADER.contains(&name));
        let mark = |entry: &SessionSummary| {
            if entry.session == info.session {
                Mark::Current
            } else if entry.pending_approvals > 0 {
                Mark::Waiting
            } else if entry.running {
                Mark::Running
            } else {
                Mark::Idle
            }
        };
        let row = |entry: &SessionSummary, prefix: Option<String>| {
            let title = entry
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("(no prompt yet)");
            let detail = if entry.pending_approvals > 0 {
                format!(
                    "{} waiting · {}",
                    entry.pending_approvals,
                    age(entry.updated, now)
                )
            } else if entry.running || entry.session == info.session {
                format!("{} · {}", entry.model.model, age(entry.updated, now))
            } else {
                age(entry.updated, now)
            };
            SessionRow {
                mark: mark(entry),
                title: match prefix {
                    Some(prefix) => format!("{prefix} · {title}"),
                    None => title.to_owned(),
                },
                detail,
            }
        };
        let here: Vec<SessionRow> = deck
            .sessions
            .iter()
            .filter(|entry| &entry.cwd == cwd)
            .map(|entry| row(entry, None))
            .collect();
        let elsewhere: Vec<SessionRow> = deck
            .sessions
            .iter()
            .filter(|entry| &entry.cwd != cwd)
            .map(|entry| {
                let base = entry
                    .cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                row(entry, base)
            })
            .collect();
        let own = self.own_approvals();
        let asks = own
            .iter()
            .chain(&deck.asks)
            .map(|ask| {
                let owner = deck
                    .sessions
                    .iter()
                    .find(|entry| entry.session == ask.pending.session);
                let origin = match owner {
                    _ if ask.pending.session == info.session => "this session".to_owned(),
                    Some(entry) => format!(
                        "{} · {}",
                        entry.title.as_deref().unwrap_or("(no prompt yet)"),
                        age(entry.updated, now)
                    ),
                    None => ask
                        .cwd
                        .as_deref()
                        .map_or_else(|| "another session".to_owned(), home),
                };
                AskRow {
                    head: ask.pending.tool.clone(),
                    body: ask.pending.action.clone(),
                    origin,
                }
            })
            .collect();
        let sessions = SessionsPane {
            here,
            elsewhere,
            selected: deck.session,
        };
        let current = deck
            .sessions
            .iter()
            .find(|entry| entry.session == info.session);
        let title = current
            .and_then(|entry| entry.title.as_deref())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("(no prompt yet)");
        let log = LogPane {
            title,
            meta,
            rows: deck.log.rows(),
            selected: matches!(deck.focus, Focus::Log | Focus::Inspector)
                .then(|| deck.log.selected_row())
                .flatten(),
            reveal: deck.log.reveal(),
            scrolled: deck.log.scrolled(),
            live,
            cursor,
        };
        let session = self.transcript.session;
        let window = info
            .model_info
            .as_ref()
            .map_or(0, |model| model.context_window);
        let context = ContextGauge {
            used: self.transcript.context,
            window,
            cache_percent: (session.sent() > 0).then(|| session.cache_read * 100 / session.sent()),
            sent: session.sent(),
            output: session.output,
        };
        let changed = self
            .transcript
            .changed()
            .iter()
            .map(|file| (file.path.clone(), file.added, file.removed))
            .collect();
        let mut facts = Vec::new();
        if let Some(report) = &deck.settings {
            let value = |key: &str| {
                report
                    .settings
                    .iter()
                    .find(|setting| setting.key == key)
                    .map(|setting| setting.value.clone())
            };
            if let Some(mode) = value("approvals").and_then(|v| v.as_str().map(str::to_owned)) {
                facts.push(format!("approvals {mode}"));
            }
            if let Some(count) = value("tools").and_then(|v| v.as_array().map(Vec::len)) {
                facts.push(format!("{count} tools"));
            }
        }
        facts.insert(
            facts.len().min(1),
            if info.sandbox_enabled {
                "◇ sandbox".to_owned()
            } else {
                "sandbox off".to_owned()
            },
        );
        facts.insert(
            facts.len().min(2),
            if info.trusted {
                "trusted".to_owned()
            } else {
                "untrusted".to_owned()
            },
        );
        let panel = Panel {
            inspect: deck.log.selected(),
            scrolled: deck.inspector,
            asks,
            changed,
            sessions,
            context,
            facts: facts.join(" · "),
        };
        Scene {
            header_left,
            header_right,
            status: self.joined(|name| STATUS.contains(&name)),
            log,
            panel,
            focus: deck.focus,
        }
    }

    /// The measure of the log column on the deck.
    pub(crate) fn deck_measure(&self) -> Measure {
        let columns = deck::columns(self.cols);
        // A gutter cell and a cell of margin beside the log's rows; the
        // header, the title and the gap under it are the deck's.
        Measure::new(
            columns.log.saturating_sub(2),
            self.rows.saturating_sub(3).max(4),
        )
    }

    /// The footer segments `keep` names, in full form, joined by ` · `.
    fn joined(&self, keep: impl Fn(&str) -> bool) -> Line {
        let mut line = Line::default();
        for segment in self.segments() {
            if !keep(segment.name) {
                continue;
            }
            if !line.0.is_empty() {
                line = line.dim(" · ");
            }
            line = line.extend(segment.value);
        }
        line
    }
}

/// The whole text behind an inspected row, as lines to commit.
fn open(inspect: &Inspect, m: Measure) -> Vec<Line> {
    let title = Line::default()
        .dim(format!("── {} ──", inspect.title))
        .truncate(m.out);
    let mut lines = vec![title];
    if inspect.text.is_empty() {
        lines.push(Line::default().dim("  no output").truncate(m.out));
        return lines;
    }
    for row in inspect.text.lines() {
        let role = if inspect.diff && row.starts_with("- ") {
            Role::Sanguine
        } else if inspect.diff && row.starts_with("+ ") {
            Role::Verdigris
        } else if inspect.diff {
            Role::Dim
        } else {
            Role::Ink
        };
        lines.push(Line::default().span(role, false, row));
    }
    lines.push(
        Line::default()
            .dim(format!("── end · {} ──", inspect.meta))
            .truncate(m.out),
    );
    lines
}
