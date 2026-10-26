//! The xi terminal client.
//!
//! Committed events print once into the terminal's own scrollback, where the
//! terminal's search, selection and copy work on them. Only the live region
//! at the bottom redraws: calls in flight, streaming text, an approval, the
//! activity line, the composer and the status line. The log draws the line
//! between the two, because a block commits when its last event lands and
//! the log is append-only, so scrollback never needs rewriting.
//!
//! The client runs the kernel in process and reads the same notifications a
//! JSON-RPC client receives.

pub mod editor;
pub mod screen;
pub mod status;
pub mod style;
pub mod transcript;
pub mod tree;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot};
use xi_kernel::config::{Config, parse_model};
use xi_kernel::{EventBus, Notification, Session, TurnOptions, TurnOutcome};
use xi_protocol::{Block, Event, EventId, SessionId, Timestamp};
use xi_provider::ModelInfo;

use crate::editor::Editor;
use crate::screen::Screen;
use crate::status::{Segment, fit};
use crate::style::{Line, Role, lr};
use crate::transcript::{SPINNER, Transcript, duration, tokens};
use crate::tree::Tree;

/// A client error.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// What the client needs to run and replace sessions.
pub struct Launch {
    /// The session to attach to.
    pub session: Session,
    /// Notifications from `bus`, subscribed before `session` was created.
    pub notifications: mpsc::UnboundedReceiver<Notification>,
    /// The bus new sessions publish to.
    pub bus: EventBus,
    /// Configuration for new sessions.
    pub config: Config,
    /// Session directory override for new sessions.
    pub session_dir: Option<PathBuf>,
    /// Turn limits for new sessions.
    pub turn_options: TurnOptions,
}

/// Whether the client can run here: both stdin and stdout are terminals.
#[must_use]
pub fn available() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Restores the terminal however the client exits.
struct Raw;

impl Raw {
    fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Composer,
    Approval,
    Tree,
}

type Reply = oneshot::Receiver<Result<TurnOutcome, String>>;

struct App {
    launch: Launch,
    transcript: Transcript,
    editor: Editor,
    focus: Focus,
    /// The session tree overlay, while it is open.
    tree: Option<Tree>,
    /// Option selected in the approval: 0 allow, 1 deny.
    choice: usize,
    /// Replies of prompts sent to the running turn.
    pending: FuturesUnordered<Reply>,
    started: Option<Instant>,
    /// Sent during a turn, shown until the kernel takes them in.
    steering: Vec<String>,
    /// Held until the turn ends, then sent in order.
    queued: Vec<String>,
    model: Option<ModelInfo>,
    frame: usize,
    cols: usize,
    rows: usize,
    commit: Vec<Line>,
    quit: bool,
    /// Whether a prompt of the running turn was rejected.
    failed: bool,
}

/// Runs the client until the user quits.
///
/// # Errors
///
/// Terminal I/O failures and failures creating a session.
pub async fn run(launch: Launch) -> Result<(), Error> {
    let (cols, rows) = crossterm::terminal::size()?;
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut app = App {
        launch,
        transcript: Transcript::default(),
        editor: Editor::default(),
        focus: Focus::Composer,
        tree: None,
        choice: 0,
        pending: FuturesUnordered::new(),
        started: None,
        steering: Vec::new(),
        queued: Vec::new(),
        model: None,
        frame: 0,
        cols: usize::from(cols).max(20),
        rows: usize::from(rows).max(4),
        commit: Vec::new(),
        quit: false,
        failed: false,
    };
    app.attach().await;
    let raw = Raw::enter()?;
    let mut screen = Screen::new(std::io::stdout(), color);
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(125));
    while !app.quit {
        app.draw(&mut screen)?;
        tokio::select! {
            // Events first: the kernel publishes a turn's last events before
            // it replies, and the done line must land after them.
            biased;
            Some(notification) = app.launch.notifications.recv() => {
                app.notify(&notification);
                while let Ok(n) = app.launch.notifications.try_recv() {
                    app.notify(&n);
                }
            }
            Some(reply) = app.pending.next(), if !app.pending.is_empty() => app.replied(reply),
            event = keys.next() => match event {
                Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => app.key(key).await?,
                Some(Ok(TermEvent::Paste(text))) if app.focus == Focus::Composer => app.editor.insert(&text),
                Some(Ok(TermEvent::Resize(c, r))) => {
                    app.cols = usize::from(c).max(20);
                    app.rows = usize::from(r).max(4);
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            _ = tick.tick(), if app.started.is_some() => app.frame += 1,
        }
    }
    screen.draw(&std::mem::take(&mut app.commit), &[], None, app.rows)?;
    screen.finish()?;
    drop(raw);
    app.launch.session.cancel();
    Ok(())
}

fn home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// The head's lineage, root first.
fn lineage(events: &[Event], head: EventId) -> Vec<Event> {
    let by_id: HashMap<EventId, &Event> = events.iter().map(|e| (e.id, e)).collect();
    let mut out = Vec::new();
    let mut at = Some(head);
    while let Some(id) = at {
        let Some(event) = by_id.get(&id) else { break };
        out.push((*event).clone());
        at = event.parent;
    }
    out.reverse();
    out
}

impl App {
    fn session_id(&self) -> SessionId {
        self.launch.session.id()
    }

    fn mine(&self, notification: &Notification) -> bool {
        let session = match notification {
            Notification::Event(p) => p.session,
            Notification::Delta(p) => p.session,
        };
        session == self.session_id()
    }

    /// Prints the header and replays the head's branch, for resumed sessions.
    async fn attach(&mut self) {
        let info = self.launch.session.info().await;
        let config = &self.launch.config;
        let mut header = vec![
            Line::default()
                .brass("ξ ")
                .strong(Role::Ink, "xi")
                .dim(format!(
                    " {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    home(&config.workspace)
                ))
                .truncate(self.cols),
        ];
        if !config.trusted {
            header.push(
                Line::default()
                    .ink("  ")
                    .brass("untrusted")
                    .dim(" · repo config held back · xi trust")
                    .truncate(self.cols),
            );
        }
        self.commit.extend(header);
        let events = lineage(&info.events, info.head);
        let replayed = events.len().saturating_sub(1);
        for event in events {
            let n = Notification::Event(xi_protocol::methods::client::EventParams {
                session: info.session,
                event,
            });
            let lines = self.transcript.apply(&n, self.cols);
            self.commit.extend(lines);
        }
        self.transcript.end_turn();
        if replayed > 0 {
            let note = self.transcript.note(vec![Line::default().dim(format!(
                "  resumed · {replayed} events · {}",
                home(&info.path)
            ))]);
            self.commit.extend(note);
        }
        self.refresh_model();
    }

    fn refresh_model(&mut self) {
        let model = self
            .transcript
            .model
            .clone()
            .unwrap_or_else(|| self.launch.config.model.clone());
        self.model = self.launch.config.registry().model(&model).cloned();
    }

    fn notify(&mut self, notification: &Notification) {
        if !self.mine(notification) {
            return;
        }
        let asks = self.transcript.asks().len();
        let lines = self.transcript.apply(notification, self.cols);
        self.commit.extend(lines);
        if let Notification::Event(p) = notification {
            match &p.event.kind {
                xi_protocol::EventKind::UserMessage { content } => {
                    let text: String = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if let Some(i) = self.steering.iter().position(|s| *s == text) {
                        self.steering.remove(i);
                    }
                }
                xi_protocol::EventKind::ModelChanged { .. } => self.refresh_model(),
                _ => {}
            }
        }
        let now = self.transcript.asks().len();
        if now > asks && self.editor.is_empty() {
            self.focus = Focus::Approval;
            self.choice = 0;
        }
        if now == 0 && self.focus == Focus::Approval {
            self.focus = Focus::Composer;
        }
    }

    fn replied(&mut self, reply: Result<Result<TurnOutcome, String>, oneshot::error::RecvError>) {
        let outcome = match reply {
            Ok(Ok(outcome)) => Some(outcome),
            Ok(Err(message)) => {
                let lines = self
                    .transcript
                    .note(vec![Line::default().sanguine("✕ ").ink(message)]);
                self.commit.extend(lines);
                self.failed = true;
                None
            }
            Err(_) => None,
        };
        if !self.pending.is_empty() {
            return;
        }
        self.transcript.end_turn();
        self.focus = Focus::Composer;
        let elapsed = self.started.take().map_or(0, |s| {
            u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let failed = std::mem::take(&mut self.failed);
        let (word, role) = match &outcome {
            Some(o) if o.cancelled => ("stopped", Role::Brass),
            Some(o) if o.failed => ("failed", Role::Sanguine),
            None if failed => ("failed", Role::Sanguine),
            _ => ("done", Role::Dim),
        };
        let turn = self.transcript.turn;
        let mut parts = vec![duration(elapsed)];
        if turn.sent() > 0 {
            parts.push(format!(
                "↑ {} ↓ {}",
                tokens(turn.sent()),
                tokens(turn.output)
            ));
            if let Some(cost) = turn.cost(self.model.as_ref().and_then(|m| m.pricing)) {
                parts.push(format!("${cost:.2}"));
            }
        }
        let line = Line::default()
            .ink("  ")
            .span(role, false, word)
            .dim(format!(" · {}", parts.join(" · ")));
        let lines = self.transcript.note(vec![line]);
        self.commit.extend(lines);
        self.steering.clear();
        if !self.queued.is_empty() {
            let next = self.queued.remove(0);
            self.send(next);
        }
    }

    fn send(&mut self, text: String) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
            self.transcript.begin_turn();
        } else {
            self.steering.push(text.clone());
        }
        let reply = self.launch.session.submit(vec![Block::Text { text }]);
        self.pending.push(reply);
    }

    fn running(&self) -> bool {
        self.started.is_some()
    }

    fn note(&mut self, lines: Vec<Line>) {
        let lines = self.transcript.note(lines);
        self.commit.extend(lines);
    }

    async fn command(&mut self, line: &str) -> Result<(), Error> {
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "/quit" | "/exit" => self.quit = true,
            "/new" if self.running() => self.note(vec![
                Line::default().dim("  /new waits for the turn to end · esc stops it"),
            ]),
            "/new" => {
                let session = Session::create(
                    self.launch.config.clone(),
                    self.launch.session_dir.as_deref(),
                    self.launch.bus.clone(),
                    self.launch.turn_options,
                    None,
                )?;
                self.launch.session = session;
                self.transcript = Transcript::default();
                self.note(vec![style::rule(self.cols)]);
                self.attach().await;
            }
            "/tree" if self.running() => self.note(vec![
                Line::default().dim("  /tree waits for the turn to end · esc stops it"),
            ]),
            "/tree" => {
                let info = self.launch.session.info().await;
                self.tree = Some(Tree::new(&info.events, info.head));
                self.focus = Focus::Tree;
            }
            "/model" if rest.trim().is_empty() => {
                let model = self.model.as_ref().map_or_else(
                    || "unknown".to_owned(),
                    |m| format!("{}/{}", m.provider, m.id),
                );
                self.note(vec![
                    Line::default()
                        .dim(format!("  model {model} · /model PROVIDER/MODEL switches")),
                ]);
            }
            "/model" => {
                let result = match parse_model(rest.trim()) {
                    Ok(model) => self
                        .launch
                        .session
                        .set_model(model)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                if let Err(message) = result {
                    self.note(vec![Line::default().sanguine("✕ ").ink(message)]);
                }
            }
            _ => {
                let rows = [
                    ("/new", "start a new session"),
                    (
                        "/tree",
                        "browse branches; check out a point to branch from it",
                    ),
                    ("/model PROVIDER/MODEL", "switch models"),
                    ("/quit", "leave; ctrl+d on an empty line does the same"),
                    ("enter", "send; during a turn, steer it"),
                    ("alt+enter", "queue a follow-up for after the turn"),
                    ("ctrl+j", "new line"),
                    ("esc", "stop the turn"),
                    ("tab", "answer a waiting approval"),
                    (
                        "ctrl+o",
                        "show the newest cut-short result whole; again for older",
                    ),
                ];
                let mut lines = Vec::new();
                if name != "/help" {
                    lines.push(Line::default().dim(format!("  unknown command {name}")));
                }
                for (k, v) in rows {
                    lines.push(Line::default().ink(format!("  {k:<22}")).dim(v));
                }
                self.note(lines);
            }
        }
        Ok(())
    }

    async fn submit(&mut self, queue: bool) -> Result<(), Error> {
        let text = self.editor.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed.starts_with('/') && !trimmed.contains('\n') {
            return self.command(trimmed).await;
        }
        let text = trimmed.to_owned();
        if queue && self.running() {
            self.queued.push(text);
        } else {
            self.send(text);
        }
        Ok(())
    }

    fn answer(&mut self, approved: bool) {
        if let Some(ask) = self.transcript.asks().first() {
            // A client elsewhere may have answered first; the log has the answer.
            let _ = self.launch.session.approve(&ask.call, approved);
        }
        self.focus = Focus::Composer;
    }

    fn close_tree(&mut self) {
        self.tree = None;
        self.focus = Focus::Composer;
    }

    /// Checks out the selected tree node and rebuilds what the client knows
    /// from the new head's branch. Scrollback stays as it was.
    async fn checkout(&mut self) {
        let Some(node) = self.tree.as_ref().and_then(Tree::current).cloned() else {
            return;
        };
        self.close_tree();
        if let Err(error) = self.launch.session.checkout(node.target).await {
            self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            return;
        }
        let info = self.launch.session.info().await;
        let mut transcript = Transcript::default();
        for event in lineage(&info.events, info.head) {
            let n = Notification::Event(xi_protocol::methods::client::EventParams {
                session: info.session,
                event,
            });
            transcript.apply(&n, self.cols);
        }
        transcript.end_turn();
        transcript.session = self.transcript.session;
        transcript.keep_details(&mut self.transcript);
        self.transcript = transcript;
        self.refresh_model();
        let what = match &node.prompt {
            Some(prompt) => {
                if self.editor.is_empty() {
                    self.editor.insert(prompt);
                }
                "before this prompt · edit it or send it as is to start a new branch"
            }
            None => "your next prompt continues from here",
        };
        let mut line = Line::default()
            .brass("  ↳ ")
            .ink("checked out · ")
            .dim(what);
        if self.transcript.tainted {
            line = line.dim(" · ").murex("◆ tainted");
        }
        self.note(vec![line.truncate(self.cols)]);
    }

    async fn tree_key(&mut self, key: KeyEvent) {
        let Some(tree) = self.tree.as_mut() else {
            self.focus = Focus::Composer;
            return;
        };
        let page = isize::try_from(self.rows / 2).unwrap_or(1).max(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => tree.step(-1),
            KeyCode::Down | KeyCode::Char('j') => tree.step(1),
            KeyCode::PageUp => tree.step(-page),
            KeyCode::PageDown => tree.step(page),
            KeyCode::Home => tree.jump(false),
            KeyCode::End => tree.jump(true),
            KeyCode::Enter => self.checkout().await,
            KeyCode::Esc | KeyCode::Char('q') => self.close_tree(),
            _ => {}
        }
    }

    async fn key(&mut self, key: KeyEvent) -> Result<(), Error> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.focus == Focus::Tree {
                self.close_tree();
            } else if self.running() {
                self.launch.session.cancel();
            } else if !self.editor.is_empty() {
                self.editor.clear();
            } else {
                self.quit = true;
            }
            return Ok(());
        }
        if self.focus == Focus::Tree {
            self.tree_key(key).await;
            return Ok(());
        }
        if self.focus == Focus::Approval {
            match key.code {
                KeyCode::Char('1' | 'y') => self.answer(true),
                KeyCode::Char('2' | 'n') => self.answer(false),
                KeyCode::Up | KeyCode::Down => self.choice = 1 - self.choice,
                KeyCode::Enter => self.answer(self.choice == 0),
                KeyCode::Esc | KeyCode::Tab => self.focus = Focus::Composer,
                _ => {}
            }
            return Ok(());
        }
        let running = self.running();
        let e = &mut self.editor;
        match key.code {
            KeyCode::Enter if alt => self.submit(true).await?,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => e.insert("\n"),
            KeyCode::Enter => self.submit(false).await?,
            KeyCode::Char('j') if ctrl => e.insert("\n"),
            KeyCode::Char('o') if ctrl => {
                let lines = self.transcript.expand(self.cols).unwrap_or_else(|| {
                    let why = if self.transcript.collapsed() {
                        "  that was the oldest cut-short result · ctrl+o starts again from the newest"
                    } else {
                        "  nothing cut short yet · results longer than three lines show whole here"
                    };
                    self.transcript.note(vec![Line::default().dim(why)])
                });
                self.commit.extend(lines);
            }
            KeyCode::Tab if !self.transcript.asks().is_empty() => {
                self.focus = Focus::Approval;
                self.choice = 0;
            }
            KeyCode::Esc if running => self.launch.session.cancel(),
            KeyCode::Char('d') if ctrl => {
                if e.is_empty() && !running {
                    self.quit = true;
                } else {
                    e.delete();
                }
            }
            KeyCode::Char('a') if ctrl => e.home(),
            KeyCode::Char('e') if ctrl => e.end(),
            KeyCode::Char('u') if ctrl => e.kill_before(),
            KeyCode::Char('k') if ctrl => e.kill_after(),
            KeyCode::Char('w') if ctrl => e.kill_word(),
            KeyCode::Backspace if alt => e.kill_word(),
            KeyCode::Char('b') if ctrl => e.left(),
            KeyCode::Char('f') if ctrl => e.right(),
            KeyCode::Char(c) if !ctrl => {
                let mut buf = [0; 4];
                e.insert(c.encode_utf8(&mut buf));
            }
            KeyCode::Backspace => e.backspace(),
            KeyCode::Delete => e.delete(),
            KeyCode::Left => e.left(),
            KeyCode::Right => e.right(),
            KeyCode::Home => e.home(),
            KeyCode::End => e.end(),
            KeyCode::Up if !e.text().contains('\n') => {
                e.history_back();
            }
            KeyCode::Down if !e.text().contains('\n') => {
                e.history_forward();
            }
            _ => {}
        }
        Ok(())
    }

    fn approval_box(&self, cols: usize) -> Vec<Line> {
        let Some(ask) = self.transcript.asks().first() else {
            return Vec::new();
        };
        let border = if ask.tainted {
            Role::Murex
        } else {
            Role::Brass
        };
        let inner = cols.saturating_sub(4);
        let mut why = Line::default().dim("rule  ");
        why = match &ask.rule {
            Some(rule) => why.strong(Role::Ink, rule),
            None => why.ink("none matched"),
        };
        if ask.tainted {
            why = why
                .dim(" · ")
                .murex("◆")
                .dim(" this branch read untrusted content");
        }
        let option = |i: usize, label: &str| {
            if self.choice == i {
                Line::default()
                    .brass(format!("› {} ", i + 1))
                    .strong(Role::Ink, label)
            } else {
                Line::default().dim(format!("  {} ", i + 1)).ink(label)
            }
        };
        let mut rows = vec![
            Line::default().strong(Role::Ink, &ask.action),
            Line::default(),
            why,
            Line::default(),
            option(0, "Allow once"),
            option(1, "Deny"),
        ];
        let more = self.transcript.asks().len() - 1;
        if more > 0 {
            rows.push(Line::default());
            rows.push(Line::default().dim(format!("{more} more waiting after this one")));
        }
        let b = |s: &str| Line::default().span(border, false, s);
        let title = Line::default()
            .brass("? ")
            .ink("approval")
            .dim(format!(" · {}", ask.tool));
        let right = Line::default().dim("1–2 · esc later");
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
        for row in rows {
            let row = row.truncate(inner);
            let pad = inner - row.width();
            lines.push(b("│ ").extend(row).ink(" ".repeat(pad)).extend(b(" │")));
        }
        lines.push(b(&format!("╰{}╯", "─".repeat(cols.saturating_sub(2)))));
        lines
    }

    fn footer(&self, cols: usize) -> Line {
        let d = |s: String| Line::default().dim(s);
        let config = &self.launch.config;
        let workspace = home(&config.workspace);
        let base = config
            .workspace
            .file_name()
            .map_or_else(|| workspace.clone(), |n| n.to_string_lossy().into_owned());
        let mut segs = vec![
            Segment::new("workspace", Line::default().ink(workspace))
                .short(Line::default().ink(base))
                .left(),
        ];
        if let Some(model) = &self.model {
            segs.push(
                Segment::new(
                    "model",
                    Line::default().ink(format!("{}/{}", model.provider, model.id)),
                )
                .short(Line::default().ink(&model.id)),
            );
            let window = model.context_window.max(1);
            segs.push(Segment::new(
                "context",
                d(format!("ctx {}%", self.transcript.context * 100 / window)),
            ));
        }
        let session = self.transcript.session;
        if session.sent() > 0 {
            segs.push(Segment::new(
                "cache",
                d(format!(
                    "cache {}%",
                    session.cache_read * 100 / session.sent()
                )),
            ));
        }
        if let Some(cost) = session.cost(self.model.as_ref().and_then(|m| m.pricing)) {
            segs.push(Segment::new(
                "cost",
                Line::default().ink(format!("${cost:.2}")),
            ));
        }
        if !config.trusted {
            segs.push(Segment::new("trust", Line::default().brass("untrusted")));
        }
        if self.transcript.tainted {
            segs.push(
                Segment::new("taint", Line::default().murex("◆ tainted"))
                    .short(Line::default().murex("◆")),
            );
        }
        let asks = self.transcript.asks().len();
        if asks > 0 {
            segs.push(
                Segment::new(
                    "approvals",
                    Line::default().brass(format!("? {asks} waiting · tab")),
                )
                .short(Line::default().brass(format!("?{asks}"))),
            );
        }
        fit(segs, cols).line
    }

    fn live(&self) -> (Vec<Line>, Option<(usize, usize)>) {
        let cols = self.cols;
        let mut lines = self.transcript.live(cols, self.frame);
        if self.focus == Focus::Approval {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(self.approval_box(cols));
        }
        if self.focus == Focus::Tree
            && let Some(tree) = &self.tree
        {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let composer = self.editor.render(cols, Role::Dim, "").0.len();
            let rows = self.rows.saturating_sub(composer + 3);
            lines.extend(tree.render(cols, rows, Timestamp::now()));
        }
        if self.running() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let elapsed = self.started.map_or(0, |s| s.elapsed().as_secs());
            let turn = self.transcript.turn;
            let what = if self.transcript.thinking() {
                "thinking"
            } else {
                "working"
            };
            let mut line = Line::default()
                .brass(SPINNER[self.frame % SPINNER.len()])
                .dim(format!(" {what} · {elapsed}s"));
            if turn.sent() > 0 {
                line = line.dim(format!(
                    " · ↑ {} ↓ {}",
                    tokens(turn.sent()),
                    tokens(turn.output)
                ));
            }
            lines.push(line.dim(" · esc stop").truncate(cols));
        }
        for text in &self.steering {
            lines.push(lr(
                Line::default()
                    .ink("  ")
                    .brass("» steer ")
                    .ink(text.replace('\n', " ")),
                Line::default().dim("at the next request"),
                cols,
            ));
        }
        for text in &self.queued {
            lines.push(lr(
                Line::default()
                    .ink("  ")
                    .dim("» queued ")
                    .ink(text.replace('\n', " ")),
                Line::default().dim("after this turn"),
                cols,
            ));
        }
        let border = if matches!(self.focus, Focus::Approval | Focus::Tree) {
            Role::Dim
        } else if self.transcript.tainted {
            Role::Murex
        } else {
            Role::Brass
        };
        let hint = if self.running() {
            "enter steers · alt+enter queues · esc stops"
        } else {
            "enter sends · ctrl+j new line · /help"
        };
        let (composer, (row, col)) = self.editor.render(cols, border, hint);
        let top = lines.len();
        lines.extend(composer);
        lines.push(self.footer(cols));
        let cursor = (self.focus == Focus::Composer).then_some((top + row, col));
        (lines, cursor)
    }

    fn draw<W: std::io::Write>(&mut self, screen: &mut Screen<W>) -> std::io::Result<()> {
        let (live, cursor) = self.live();
        let commit = std::mem::take(&mut self.commit);
        screen.draw(&commit, &live, cursor, self.rows.saturating_sub(1))
    }
}
