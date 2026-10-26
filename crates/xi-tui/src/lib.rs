//! The xi terminal client.
//!
//! Committed events print once into the terminal's own scrollback, where the
//! terminal's search, selection and copy work on them. Only the live region
//! at the bottom redraws: calls in flight, streaming text, an approval, the
//! activity line, the composer and the status line. The log draws the line
//! between the two, because a block commits when its last event lands and
//! the log is append-only, so scrollback never needs rewriting.
//!
//! The client attaches to a daemon session over the typed JSON-RPC connection.

pub mod editor;
pub mod screen;
pub mod status;
pub mod style;
pub mod transcript;
pub mod tree;

use std::collections::{HashMap, HashSet};
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
use xi_client::client::{Client, ClientError, ClientNotification};
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{AttachResult, CreateParams, EventParams, PromptResult};
use xi_protocol::rpc::RpcError;
use xi_protocol::{Block, Event, EventId, SessionId, Timestamp, ToolCallId};

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
    /// The initialized daemon connection.
    pub client: Client,
    /// Notifications from the daemon, subscribed before attaching.
    pub notifications: mpsc::UnboundedReceiver<ClientNotification>,
    /// Creation parameters reused by `/new`.
    pub template: CreateParams,
    /// The initial session snapshot.
    pub attached: AttachResult,
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

type Reply = oneshot::Receiver<Result<PromptResult, ClientError>>;

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
    /// Sent during a turn, shown until the worker takes them in.
    steering: Vec<String>,
    /// Held until the turn ends, then sent in order.
    queued: Vec<String>,
    frame: usize,
    cols: usize,
    rows: usize,
    commit: Vec<Line>,
    quit: bool,
    /// Whether a prompt of the running turn was rejected.
    failed: bool,
    /// Every event seen in the attach snapshot or a subsequent notification.
    seen: HashSet<EventId>,
    /// Calls already resolved in any recorded branch.
    resolved: HashSet<ToolCallId>,
    /// Whether fresh session metadata is needed after a model change.
    refresh: bool,
    /// Whether the worker has exited.
    exited: bool,
}

/// Runs the client until the user quits.
///
/// # Errors
///
/// Terminal I/O failures and failures creating a session.
pub async fn run(launch: Launch) -> Result<(), Error> {
    let (cols, rows) = crossterm::terminal::size()?;
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut app = App::new(launch, usize::from(cols), usize::from(rows));
    let raw = Raw::enter()?;
    let mut screen = Screen::new(std::io::stdout(), color, usize::from(cols));
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(125));
    while !app.quit {
        if app.refresh {
            app.refresh_details().await;
        }
        // The terminal may have reflowed since the last resize event, so the
        // rewind counts rows at the width it has now.
        if let Ok((c, _)) = crossterm::terminal::size() {
            screen.resize(usize::from(c));
        }
        app.draw(&mut screen)?;
        tokio::select! {
            // The worker sends a turn's last events before its reply.
            biased;
            notification = app.launch.notifications.recv() => match notification {
                Some(notification) => {
                    app.notify(&notification);
                    while let Ok(n) = app.launch.notifications.try_recv() {
                        app.notify(&n);
                    }
                }
                None => {
                    app.note(vec![Line::default().dim("  daemon connection closed")]);
                    break;
                }
            },
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
            _ = tick.tick(), if app.started.is_some() => {
                app.frame += 1;
                if app.pending.is_empty() && app.frame.is_multiple_of(8) {
                    app.refresh_details().await;
                }
            },
        }
    }
    screen.draw(&std::mem::take(&mut app.commit), &[], None, app.rows)?;
    screen.finish()?;
    drop(raw);
    app.release().await?;
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
    fn new(launch: Launch, cols: usize, rows: usize) -> Self {
        let mut app = Self {
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
            frame: 0,
            cols: cols.max(20),
            rows: rows.max(4),
            commit: Vec::new(),
            quit: false,
            failed: false,
            seen: HashSet::new(),
            resolved: HashSet::new(),
            refresh: false,
            exited: false,
        };
        app.attach();
        app
    }

    fn session_id(&self) -> SessionId {
        self.launch.attached.info.session
    }

    fn mine(&self, notification: &ClientNotification) -> bool {
        let session = match notification {
            ClientNotification::Event(p) => p.session,
            ClientNotification::Delta(p) => p.session,
            ClientNotification::Reset(p) => p.session,
            ClientNotification::Exited(p) => p.session,
            ClientNotification::ApprovalPending(p) => p.session,
        };
        session == self.session_id()
    }

    /// Prints the header and replays the head's branch, for resumed sessions.
    fn attach(&mut self) {
        let info = &self.launch.attached.info;
        let mut header = vec![
            Line::default()
                .brass("ξ ")
                .strong(Role::Ink, "xi")
                .dim(format!(
                    " {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    home(&info.cwd)
                ))
                .truncate(self.cols),
        ];
        if !info.trusted {
            header.push(
                Line::default()
                    .ink("  ")
                    .brass("untrusted")
                    .dim(" · repo config held back · xi trust")
                    .truncate(self.cols),
            );
        }
        self.commit.extend(header);
        self.seen.clear();
        self.launch
            .attached
            .events
            .retain(|event| self.seen.insert(event.id));
        self.resolved = self
            .launch
            .attached
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                xi_protocol::EventKind::ApprovalResolved { call, .. }
                | xi_protocol::EventKind::ToolResult { call, .. } => Some(call.clone()),
                _ => None,
            })
            .collect();
        let events = lineage(&self.launch.attached.events, info.head);
        let replayed = events.len().saturating_sub(1);
        for event in events {
            let n = ClientNotification::Event(EventParams {
                session: info.session,
                event,
            });
            let lines = self.transcript.apply(&n, self.cols);
            self.commit.extend(lines);
        }
        if self.launch.attached.running {
            self.started = Some(Instant::now());
        } else {
            self.transcript.end_turn();
        }
        self.transcript.partial(&self.launch.attached.partial);
        for pending in &self.launch.attached.pending_approvals {
            self.transcript.apply(
                &ClientNotification::ApprovalPending(pending.clone()),
                self.cols,
            );
        }
        if !self.transcript.asks().is_empty() {
            self.focus = Focus::Approval;
        }
        if replayed > 0 {
            let note = self.transcript.note(vec![Line::default().dim(format!(
                "  resumed · {replayed} events · {}",
                home(&info.path)
            ))]);
            self.commit.extend(note);
        }
    }

    async fn refresh_details(&mut self) {
        self.refresh = false;
        if self.exited {
            return;
        }
        let since = self.launch.attached.events.last().map(|event| event.id);
        match self.launch.client.attach(self.session_id(), since).await {
            Ok(attached) => {
                for event in attached.events {
                    self.notify(&ClientNotification::Event(EventParams {
                        session: attached.info.session,
                        event,
                    }));
                }
                self.launch.attached.info = attached.info;
                self.launch.attached.running = attached.running;
                self.refresh = false;
                if !attached.running && self.pending.is_empty() && self.started.is_some() {
                    self.finish_turn(None);
                }
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    fn notify(&mut self, notification: &ClientNotification) {
        if !self.mine(notification) {
            return;
        }
        if let ClientNotification::ApprovalPending(pending) = notification
            && self.resolved.contains(&pending.call)
        {
            return;
        }
        if let ClientNotification::Event(p) = notification {
            if !self.seen.insert(p.event.id) {
                return;
            }
            self.launch.attached.info.head = p.event.id;
            self.launch.attached.events.push(p.event.clone());
            match &p.event.kind {
                xi_protocol::EventKind::ApprovalResolved { call, .. }
                | xi_protocol::EventKind::ToolResult { call, .. } => {
                    self.resolved.insert(call.clone());
                }
                _ => {}
            }
        }
        let asks = self.transcript.asks().len();
        let lines = self.transcript.apply(notification, self.cols);
        self.commit.extend(lines);
        match notification {
            ClientNotification::Event(p) => match &p.event.kind {
                xi_protocol::EventKind::UserMessage { content } => {
                    if self.started.is_none() {
                        self.started = Some(Instant::now());
                        self.transcript.begin_turn();
                    }
                    self.launch.attached.running = true;
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
                xi_protocol::EventKind::ModelChanged { model } => {
                    self.launch.attached.info.model = model.clone();
                    self.launch.attached.info.model_info = None;
                    self.refresh = true;
                }
                _ => {}
            },
            ClientNotification::Exited(_) => {
                self.exited = true;
                self.launch.attached.running = false;
                self.started = None;
                self.steering.clear();
                self.queued.clear();
            }
            _ => {}
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

    fn replied(
        &mut self,
        reply: Result<Result<PromptResult, ClientError>, oneshot::error::RecvError>,
    ) {
        let outcome = match reply {
            Ok(Ok(outcome)) => {
                // Later events may already have advanced the head beyond this turn.
                if !self.seen.contains(&outcome.head) {
                    self.launch.attached.info.head = outcome.head;
                }
                Some(outcome)
            }
            Ok(Err(error)) => {
                let lines = self
                    .transcript
                    .note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                self.commit.extend(lines);
                self.failed = true;
                None
            }
            Err(_) => None,
        };
        if self.pending.is_empty() {
            self.finish_turn(outcome);
        }
    }

    fn finish_turn(&mut self, outcome: Option<PromptResult>) {
        self.launch.attached.running = false;
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
            if let Some(cost) = turn.cost(
                self.launch
                    .attached
                    .info
                    .model_info
                    .as_ref()
                    .and_then(|m| m.pricing),
            ) {
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
        if !self.exited && !self.queued.is_empty() {
            let next = self.queued.remove(0);
            self.send(next);
        }
    }

    fn send(&mut self, text: String) {
        if self.exited {
            self.note(vec![
                Line::default().dim("  session exited · /new starts another"),
            ]);
            return;
        }
        if self.started.is_none() {
            self.started = Some(Instant::now());
            self.transcript.begin_turn();
        } else {
            self.steering.push(text.clone());
        }
        self.launch.attached.running = true;
        let client = self.launch.client.clone();
        let session = self.session_id();
        let (send, reply) = oneshot::channel();
        tokio::spawn(async move {
            let _ = send.send(client.prompt(session, vec![Block::Text { text }]).await);
        });
        self.pending.push(reply);
    }

    fn running(&self) -> bool {
        self.started.is_some()
    }

    fn note(&mut self, lines: Vec<Line>) {
        let lines = self.transcript.note(lines);
        self.commit.extend(lines);
    }

    async fn cancel(&mut self) {
        if let Err(error) = self.launch.client.cancel(self.session_id()).await {
            self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
        }
    }

    async fn release(&self) -> Result<(), ClientError> {
        if self.exited {
            return Ok(());
        }
        let session = self.session_id();
        let running = match self
            .launch
            .client
            .attach(
                session,
                self.launch.attached.events.last().map(|event| event.id),
            )
            .await
        {
            Ok(attached) => attached.running,
            Err(ClientError::Closed) => return Ok(()),
            Err(ClientError::Rpc {
                code: RpcError::UNKNOWN_SESSION | RpcError::WORKER_EXITED,
                ..
            }) => return Ok(()),
            Err(error) => return Err(error),
        };
        self.launch.client.detach(session).await?;
        if !running {
            match self.launch.client.close(session).await {
                Ok(()) => {}
                // Another client may have started a turn or closed the worker.
                Err(ClientError::Rpc {
                    code:
                        RpcError::SESSION_RUNNING | RpcError::UNKNOWN_SESSION | RpcError::WORKER_EXITED,
                    ..
                }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn command(&mut self, line: &str) -> Result<(), Error> {
        let (name, rest) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "/quit" | "/exit" => self.quit = true,
            "/new" if self.running() => self.note(vec![
                Line::default().dim("  /new waits for the turn to end · esc stops it"),
            ]),
            "/new" => {
                let attached = self
                    .launch
                    .client
                    .create(self.launch.template.clone())
                    .await?;
                self.release().await?;
                self.launch.attached = attached;
                self.exited = false;
                self.refresh = false;
                self.failed = false;
                self.started = None;
                self.pending = FuturesUnordered::new();
                self.transcript = Transcript::default();
                self.note(vec![style::rule(self.cols)]);
                self.attach();
            }
            "/tree" if self.running() => self.note(vec![
                Line::default().dim("  /tree waits for the turn to end · esc stops it"),
            ]),
            "/tree" => {
                self.tree = Some(Tree::new(
                    &self.launch.attached.events,
                    self.launch.attached.info.head,
                ));
                self.focus = Focus::Tree;
            }
            "/model" if rest.trim().is_empty() => {
                let selected = &self.launch.attached.info.model;
                let model = format!("{}/{}", selected.provider, selected.model);
                self.note(vec![
                    Line::default()
                        .dim(format!("  model {model} · /model PROVIDER/MODEL switches")),
                ]);
            }
            "/model" => {
                let result = match rest.trim().split_once('/') {
                    Some((provider, model)) if !provider.is_empty() && !model.is_empty() => self
                        .launch
                        .client
                        .set_model(
                            self.session_id(),
                            ModelRef {
                                provider: provider.into(),
                                model: model.into(),
                            },
                        )
                        .await
                        .map_err(|e| e.to_string()),
                    _ => Err(format!(
                        "invalid model {:?}; expected PROVIDER/MODEL",
                        rest.trim()
                    )),
                };
                if let Err(message) = result {
                    self.note(vec![Line::default().sanguine("✕ ").ink(message)]);
                } else {
                    self.refresh = true;
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

    async fn answer(&mut self, approved: bool) {
        if let Some(ask) = self.transcript.asks().first() {
            let result = self
                .launch
                .client
                .resolve(self.session_id(), ask.call.clone(), approved)
                .await;
            match result {
                Ok(()) => {}
                // A client elsewhere may have answered first; the log has the answer.
                Err(ClientError::Rpc {
                    code: RpcError::APPROVAL_NOT_PENDING,
                    ..
                }) => {}
                Err(error) => {
                    self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                }
            }
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
        if let Err(error) = self
            .launch
            .client
            .checkout(self.session_id(), node.target)
            .await
        {
            self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            return;
        }
        self.launch.attached.info.head = node.target;
        let mut transcript = Transcript::default();
        for event in lineage(&self.launch.attached.events, node.target) {
            let n = ClientNotification::Event(EventParams {
                session: self.session_id(),
                event,
            });
            transcript.apply(&n, self.cols);
        }
        transcript.end_turn();
        transcript.session = self.transcript.session;
        transcript.keep_details(&mut self.transcript);
        self.transcript = transcript;
        self.refresh_details().await;
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
                self.cancel().await;
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
                KeyCode::Char('1' | 'y') => self.answer(true).await,
                KeyCode::Char('2' | 'n') => self.answer(false).await,
                KeyCode::Up | KeyCode::Down => self.choice = 1 - self.choice,
                KeyCode::Enter => self.answer(self.choice == 0).await,
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
            KeyCode::Esc if running => self.cancel().await,
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
        let info = &self.launch.attached.info;
        let workspace = home(&info.cwd);
        let base = info
            .cwd
            .file_name()
            .map_or_else(|| workspace.clone(), |n| n.to_string_lossy().into_owned());
        let mut segs = vec![
            Segment::new("workspace", Line::default().ink(workspace))
                .short(Line::default().ink(base))
                .left(),
        ];
        let model = &info.model;
        segs.push(
            Segment::new(
                "model",
                Line::default().ink(format!("{}/{}", model.provider, model.model)),
            )
            .short(Line::default().ink(&model.model)),
        );
        if let Some(model) = &info.model_info {
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
        if let Some(cost) = session.cost(info.model_info.as_ref().and_then(|m| m.pricing)) {
            segs.push(Segment::new(
                "cost",
                Line::default().ink(format!("${cost:.2}")),
            ));
        }
        if !info.trusted {
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use xi_client::peer::{Incoming, Peer};
    use xi_protocol::EventKind;
    use xi_protocol::methods::client::{PendingApproval, SessionDetails};

    use super::*;

    fn snapshot(running: bool) -> AttachResult {
        let session = SessionId::generate();
        let model = ModelRef {
            provider: "demo".into(),
            model: "script".into(),
        };
        let root = Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp(0),
            kind: EventKind::SessionStarted {
                session,
                cwd: "/tmp/xi-tui-demo".into(),
                model: model.clone(),
                xi_version: "test".into(),
                parent_session: None,
            },
        };
        AttachResult {
            info: SessionDetails {
                session,
                path: "/tmp/xi-tui-demo/session.jsonl".into(),
                head: root.id,
                cwd: "/tmp/xi-tui-demo".into(),
                trusted: true,
                model,
                model_info: None,
                parent_session: None,
            },
            events: vec![root],
            partial: Vec::new(),
            running,
            pending_approvals: Vec::new(),
        }
    }

    async fn launch(attached: AttachResult) -> (Launch, mpsc::UnboundedReceiver<String>) {
        let (connection, server) = tokio::io::duplex(16 * 1024);
        let (reader, writer) = tokio::io::split(server);
        let (peer, mut incoming) = Peer::spawn(reader, writer);
        let (requests, observed) = mpsc::unbounded_channel();
        let response = attached.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Request { id, method, .. } = message {
                    let _ = requests.send(method.clone());
                    let result = match method.as_str() {
                        "initialize" => Ok(json!({
                            "protocol_version": xi_protocol::PROTOCOL_VERSION,
                            "daemon_version": "test",
                            "capabilities": { "private": true },
                        })),
                        "session/attach" => Ok(serde_json::to_value(&response).unwrap()),
                        "session/prompt" => Ok(serde_json::to_value(PromptResult {
                            head: response.info.head,
                            failed: false,
                            cancelled: false,
                            text: "finished".into(),
                        })
                        .unwrap()),
                        "approval/resolve" => Err(RpcError::new(
                            RpcError::APPROVAL_NOT_PENDING,
                            "already answered",
                        )),
                        _ => Ok(Value::Null),
                    };
                    let _ = peer.respond(id, result);
                }
            }
        });
        let (reader, writer) = tokio::io::split(connection);
        let (client, notifications) = Client::over(reader, writer, "test-tui").await.unwrap();
        let template = CreateParams {
            cwd: attached.info.cwd.clone(),
            model: Some(attached.info.model.clone()),
            options: Default::default(),
        };
        (
            Launch {
                client,
                notifications,
                template,
                attached,
            },
            observed,
        )
    }

    #[tokio::test]
    async fn attach_keeps_partial_and_deduplicates_events_after_interleaved_appends() {
        let mut attached = snapshot(true);
        attached.partial = vec![Block::Text {
            text: "in flight".into(),
        }];
        attached.events.push(attached.events[0].clone());
        let root = attached.events[0].clone();
        let (launch, _) = launch(attached).await;
        let mut app = App::new(launch, 80, 24);
        assert_eq!(app.launch.attached.events.len(), 1);
        assert!(
            app.transcript
                .live(80, 0)
                .iter()
                .any(|line| line.plain().contains("in flight"))
        );
        let event = Event {
            id: EventId::generate(),
            parent: Some(root.id),
            at: Timestamp(1),
            kind: EventKind::UserMessage {
                content: vec![Block::Text {
                    text: "hello".into(),
                }],
            },
        };
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event: event.clone(),
        }));
        let committed = app.commit.len();
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event,
        }));
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event: root,
        }));
        assert_eq!(app.launch.attached.events.len(), 2);
        assert_eq!(app.commit.len(), committed);
    }

    #[tokio::test]
    async fn prompt_runs_off_the_ui_and_finishes_from_the_daemon_outcome() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        app.commit.clear();
        app.send("hello".into());
        assert!(app.running());
        assert_eq!(app.pending.len(), 1);
        let reply = app.pending.next().await.unwrap();
        app.replied(reply);
        assert!(!app.running());
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("done ·"))
        );
    }

    #[tokio::test]
    async fn approvals_resolved_elsewhere_are_not_reported_as_errors_or_revived() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        let pending = PendingApproval {
            session: app.session_id(),
            call: "approval".into(),
            tool: "bash".into(),
            action: "run command".into(),
            rule: None,
        };
        app.notify(&ClientNotification::ApprovalPending(pending.clone()));
        app.commit.clear();
        app.answer(true).await;
        assert!(app.commit.is_empty());
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event: Event {
                id: EventId::generate(),
                parent: Some(app.launch.attached.info.head),
                at: Timestamp(1),
                kind: EventKind::ApprovalResolved {
                    call: pending.call.clone(),
                    approved: true,
                    by: xi_protocol::event::Approver::User,
                },
            },
        }));
        app.notify(&ClientNotification::ApprovalPending(pending));
        assert!(app.transcript.asks().is_empty());
    }

    #[tokio::test]
    async fn leaving_detaches_and_only_closes_an_idle_worker() {
        for running in [false, true] {
            let (launch, mut requests) = launch(snapshot(running)).await;
            let app = App::new(launch, 80, 24);
            app.release().await.unwrap();
            let mut methods = Vec::new();
            while let Ok(method) = requests.try_recv() {
                methods.push(method);
            }
            assert!(methods.iter().any(|method| method == "session/detach"));
            assert_eq!(
                methods.iter().any(|method| method == "session/close"),
                !running
            );
            assert!(methods.iter().all(|method| method != "session/cancel"));
        }
    }
}
