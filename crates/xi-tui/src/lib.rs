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

pub mod complete;
pub mod editor;
pub mod external;
pub mod history;
pub mod picker;
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
use xi_protocol::manifest::CommandSpec;
use xi_protocol::methods::client::{AttachResult, CreateParams, EventParams, PromptResult};
use xi_protocol::model::Reasoning;
use xi_protocol::rpc::RpcError;
use xi_protocol::{Block, Event, EventId, SessionId, Timestamp, ToolCallId};

use crate::complete::{FileIndex, Menu};
use crate::editor::Editor;
use crate::history::{History, Search};
use crate::picker::Picker;
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
    /// Shared prompt history, or in-memory history when absent.
    pub history: Option<PathBuf>,
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
        let raw = Self;
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        Ok(raw)
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy)]
enum Action {
    Send,
    Queue,
    Newline,
    Escape,
    Interrupt,
    DeleteOrQuit,
    Up,
    Down,
    Complete,
    Search,
    External,
    Models,
    Reasoning,
    Expand,
    Home,
    End,
    Left,
    Right,
    KillBefore,
    KillAfter,
    KillWord,
    Backspace,
    Delete,
}

// Both help and composer dispatch use these effective bindings.
const KEYS: &[(KeyCode, KeyModifiers, Action, &str, &str)] = &[
    (
        KeyCode::Enter,
        KeyModifiers::NONE,
        Action::Send,
        "enter",
        "send; during a turn, steer; accept a completion",
    ),
    (
        KeyCode::Enter,
        KeyModifiers::ALT,
        Action::Queue,
        "alt+enter",
        "queue a follow-up for after the turn",
    ),
    (
        KeyCode::Enter,
        KeyModifiers::SHIFT,
        Action::Newline,
        "shift+enter",
        "new line",
    ),
    (
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
        Action::Newline,
        "ctrl+j",
        "new line",
    ),
    (
        KeyCode::Esc,
        KeyModifiers::NONE,
        Action::Escape,
        "esc",
        "close a menu or overlay; otherwise stop the turn",
    ),
    (
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        Action::Interrupt,
        "ctrl+c",
        "close overlay, stop turn, clear composer, or quit",
    ),
    (
        KeyCode::Char('d'),
        KeyModifiers::CONTROL,
        Action::DeleteOrQuit,
        "ctrl+d",
        "quit when empty and idle; otherwise delete forward",
    ),
    (
        KeyCode::Up,
        KeyModifiers::NONE,
        Action::Up,
        "up",
        "previous completion or workspace prompt",
    ),
    (
        KeyCode::Down,
        KeyModifiers::NONE,
        Action::Down,
        "down",
        "next completion or workspace prompt",
    ),
    (
        KeyCode::Tab,
        KeyModifiers::NONE,
        Action::Complete,
        "tab",
        "accept a completion; otherwise answer a waiting approval",
    ),
    (
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
        Action::Search,
        "ctrl+r",
        "search prompt history; repeat for older matches",
    ),
    (
        KeyCode::Char('g'),
        KeyModifiers::CONTROL,
        Action::External,
        "ctrl+g",
        "edit the prompt in the external editor",
    ),
    (
        KeyCode::Char('m'),
        KeyModifiers::ALT,
        Action::Models,
        "alt+m",
        "open the model picker",
    ),
    (
        KeyCode::BackTab,
        KeyModifiers::NONE,
        Action::Reasoning,
        "shift+tab",
        "cycle reasoning: off, low, medium, high",
    ),
    (
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
        Action::Expand,
        "ctrl+o",
        "expand the newest collapsed result; repeat for older",
    ),
    (
        KeyCode::Char('a'),
        KeyModifiers::CONTROL,
        Action::Home,
        "ctrl+a",
        "line start",
    ),
    (
        KeyCode::Char('e'),
        KeyModifiers::CONTROL,
        Action::End,
        "ctrl+e",
        "line end",
    ),
    (
        KeyCode::Char('b'),
        KeyModifiers::CONTROL,
        Action::Left,
        "ctrl+b",
        "previous character",
    ),
    (
        KeyCode::Char('f'),
        KeyModifiers::CONTROL,
        Action::Right,
        "ctrl+f",
        "next character",
    ),
    (
        KeyCode::Left,
        KeyModifiers::NONE,
        Action::Left,
        "left",
        "previous character",
    ),
    (
        KeyCode::Right,
        KeyModifiers::NONE,
        Action::Right,
        "right",
        "next character",
    ),
    (
        KeyCode::Home,
        KeyModifiers::NONE,
        Action::Home,
        "home",
        "line start",
    ),
    (
        KeyCode::End,
        KeyModifiers::NONE,
        Action::End,
        "end",
        "line end",
    ),
    (
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
        Action::KillBefore,
        "ctrl+u",
        "delete to line start",
    ),
    (
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
        Action::KillAfter,
        "ctrl+k",
        "delete to line end",
    ),
    (
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
        Action::KillWord,
        "ctrl+w",
        "delete previous word",
    ),
    (
        KeyCode::Backspace,
        KeyModifiers::ALT,
        Action::KillWord,
        "alt+backspace",
        "delete previous word",
    ),
    (
        KeyCode::Backspace,
        KeyModifiers::NONE,
        Action::Backspace,
        "backspace",
        "delete previous character",
    ),
    (
        KeyCode::Delete,
        KeyModifiers::NONE,
        Action::Delete,
        "delete",
        "delete next character",
    ),
];

fn action(key: KeyEvent) -> Option<Action> {
    let modifiers = if key.code == KeyCode::BackTab {
        key.modifiers - KeyModifiers::SHIFT
    } else {
        key.modifiers
    };
    KEYS.iter()
        .find(|(code, mods, ..)| *code == key.code && *mods == modifiers)
        .map(|(_, _, action, ..)| *action)
}

#[derive(Clone, Copy)]
enum Builtin {
    Help,
    Quit,
    New,
    Tree,
    Model,
    Reasoning,
    Compact,
}

const COMMANDS: &[(&str, &str, Builtin)] = &[
    ("help", "show keys and commands", Builtin::Help),
    ("hotkeys", "show keys and commands", Builtin::Help),
    ("quit", "leave; a running turn keeps running", Builtin::Quit),
    ("exit", "leave; a running turn keeps running", Builtin::Quit),
    ("new", "start a session in this workspace", Builtin::New),
    (
        "tree",
        "browse branches and check out an earlier point",
        Builtin::Tree,
    ),
    (
        "model",
        "switch PROVIDER/MODEL; without an argument, open the picker",
        Builtin::Model,
    ),
    (
        "reasoning",
        "set off, low, medium, high; without an argument, show it",
        Builtin::Reasoning,
    ),
    ("compact", "summarize older context now", Builtin::Compact),
];

fn builtin_commands() -> Vec<CommandSpec> {
    COMMANDS
        .iter()
        .map(|(name, description, _)| CommandSpec {
            name: (*name).into(),
            description: (*description).into(),
        })
        .collect()
}

fn reasoning_name(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Off => "off",
        Reasoning::Low => "low",
        Reasoning::Medium => "medium",
        Reasoning::High => "high",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Composer,
    Approval,
    Tree,
    Models,
    Search,
}

type Reply = oneshot::Receiver<Result<PromptResult, ClientError>>;

struct App {
    launch: Launch,
    transcript: Transcript,
    editor: Editor,
    focus: Focus,
    /// The session tree overlay, while it is open.
    tree: Option<Tree>,
    picker: Option<Picker>,
    search: Option<Search>,
    history: History,
    commands: Vec<CommandSpec>,
    completion: Option<Menu>,
    files: Option<FileIndex>,
    file_cwd: Option<PathBuf>,
    external_requested: bool,
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
    app.initialize_composer().await;
    let mut raw = Some(Raw::enter()?);
    let mut screen = Screen::new(std::io::stdout(), color, usize::from(cols));
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(125));
    while !app.quit {
        if app.external_requested {
            app.external_requested = false;
            screen.finish()?;
            drop(keys);
            drop(raw.take());
            let edited = external::edit(app.editor.text());
            raw = Some(Raw::enter()?);
            crossterm::execute!(
                std::io::stdout(),
                crossterm::cursor::MoveToColumn(0),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
            )?;
            keys = EventStream::new();
            match edited {
                Ok(Some(text)) => app.editor.set_text(&text),
                Ok(None) => {}
                Err(error) => app.note(vec![
                    Line::default().dim(format!("  external editor: {error}")),
                ]),
            }
            app.update_completion();
        }
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
                Some(Ok(TermEvent::Paste(text))) if app.focus == Focus::Composer => {
                    app.editor.insert(&text);
                    app.update_completion();
                }
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
        let (history, history_error) =
            History::load(launch.history.clone(), &launch.attached.info.cwd);
        let mut editor = Editor::default();
        editor.set_history(history.workspace());
        let mut app = Self {
            launch,
            transcript: Transcript::default(),
            editor,
            focus: Focus::Composer,
            tree: None,
            picker: None,
            search: None,
            history,
            commands: builtin_commands(),
            completion: None,
            files: None,
            file_cwd: None,
            external_requested: false,
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
        if let Some(error) = history_error {
            app.note(vec![
                Line::default().dim(format!("  prompt history: {error}")),
            ]);
        }
        app
    }

    async fn initialize_composer(&mut self) {
        self.commands = builtin_commands();
        match self.launch.client.commands(self.session_id()).await {
            Ok(commands) => {
                for command in commands {
                    if !self.commands.iter().any(|known| known.name == command.name) {
                        self.commands.push(command);
                    }
                }
            }
            Err(error) => self.note(vec![
                Line::default().dim(format!("  command completion: {error}")),
            ]),
        }
        let cwd = &self.launch.attached.info.cwd;
        if self.file_cwd.as_ref() != Some(cwd) {
            let root = cwd.clone();
            match tokio::task::spawn_blocking(move || FileIndex::build(&root, 20_000)).await {
                Ok(files) => {
                    self.files = Some(files);
                    self.file_cwd = Some(self.launch.attached.info.cwd.clone());
                }
                Err(error) => self.note(vec![
                    Line::default().dim(format!("  file completion: {error}")),
                ]),
            }
        }
        self.update_completion();
    }

    fn update_completion(&mut self) {
        self.completion =
            complete::trigger(self.editor.text(), self.editor.cursor()).and_then(|trigger| {
                match trigger.kind {
                    complete::Kind::Command => Menu::commands(&trigger, &self.commands),
                    complete::Kind::File => self
                        .files
                        .as_ref()
                        .and_then(|files| Menu::files(&trigger, files)),
                }
            });
    }

    fn accept_completion(&mut self) {
        if let Some(menu) = self.completion.take()
            && let Some(item) = menu.selected()
        {
            self.editor.replace(menu.range(), &item.insert);
        }
        self.update_completion();
    }

    fn close_overlay(&mut self) {
        if let Some(search) = self.search.take() {
            self.editor.set_text(search.draft());
        }
        self.tree = None;
        self.picker = None;
        self.focus = Focus::Composer;
        self.completion = None;
    }

    async fn open_models(&mut self) {
        match self.launch.client.models(self.session_id()).await {
            Ok(models) => {
                self.launch.attached.info.model = models.current.clone();
                self.launch.attached.info.reasoning = models.reasoning;
                self.launch.attached.info.model_info = models
                    .models
                    .iter()
                    .find(|entry| entry.model == models.current)
                    .map(|entry| entry.info.clone());
                self.picker = Some(Picker::new(models));
                self.completion = None;
                self.focus = Focus::Models;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    fn idle_only(&mut self, action: &str) -> bool {
        if !self.running() {
            return true;
        }
        self.note(vec![Line::default().dim(format!(
            "  {action} waits for the turn to end · esc stops it"
        ))]);
        false
    }

    async fn set_model(&mut self, model: ModelRef) -> bool {
        if !self.idle_only("model selection") {
            return false;
        }
        match self
            .launch
            .client
            .set_model(self.session_id(), model.clone())
            .await
        {
            Ok(()) => {
                self.launch.attached.info.model = model;
                self.launch.attached.info.model_info = None;
                self.refresh = true;
                true
            }
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                false
            }
        }
    }

    async fn set_reasoning(&mut self, reasoning: Reasoning) {
        if !self.idle_only("reasoning") {
            return;
        }
        match self
            .launch
            .client
            .set_reasoning(self.session_id(), reasoning)
            .await
        {
            Ok(()) => self.launch.attached.info.reasoning = reasoning,
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
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
        let sequence = match notification {
            ClientNotification::Delta(params) => params.sequence,
            ClientNotification::Reset(params) => params.sequence,
            _ => 0,
        };
        if sequence != 0 {
            if sequence <= self.launch.attached.stream_sequence {
                return;
            }
            self.launch.attached.stream_sequence = sequence;
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
                xi_protocol::EventKind::ReasoningChanged { reasoning } => {
                    self.launch.attached.info.reasoning = *reasoning;
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
        if now > asks && self.editor.is_empty() && self.focus == Focus::Composer {
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
        if self.focus == Focus::Approval {
            self.focus = Focus::Composer;
        }
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
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let name = name.trim_start_matches('/');
        let rest = rest.trim();
        let builtin = COMMANDS
            .iter()
            .find(|(command, ..)| *command == name)
            .map(|(_, _, kind)| *kind);
        match builtin {
            Some(Builtin::Help) => {
                let mut lines = vec![Line::default().strong(Role::Ink, "  keys")];
                for (_, _, _, key, description) in KEYS {
                    lines.push(Line::default().ink(format!("  {key:<22}")).dim(description));
                }
                lines.push(Line::default().strong(Role::Ink, "  commands"));
                for (command, description, _) in COMMANDS {
                    lines.push(
                        Line::default()
                            .ink(format!("  /{command:<21}"))
                            .dim(description),
                    );
                }
                lines.push(
                    Line::default()
                        .ink("  /name args            ")
                        .dim("run an extension command"),
                );
                self.note(lines);
            }
            Some(Builtin::Quit) => self.quit = true,
            Some(Builtin::New) => {
                if !self.idle_only("/new") {
                    return Ok(());
                }
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
                self.close_overlay();
                self.steering.clear();
                self.queued.clear();
                self.note(vec![style::rule(self.cols)]);
                self.attach();
                self.initialize_composer().await;
            }
            Some(Builtin::Tree) => {
                if !self.idle_only("/tree") {
                    return Ok(());
                }
                self.tree = Some(Tree::new(
                    &self.launch.attached.events,
                    self.launch.attached.info.head,
                ));
                self.completion = None;
                self.focus = Focus::Tree;
            }
            Some(Builtin::Model) if rest.is_empty() => self.open_models().await,
            Some(Builtin::Model) => match rest.split_once('/') {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                    self.set_model(ModelRef {
                        provider: provider.into(),
                        model: model.into(),
                    })
                    .await;
                }
                _ => self.note(vec![Line::default().dim("  /model expects PROVIDER/MODEL")]),
            },
            Some(Builtin::Reasoning) if rest.is_empty() => {
                self.note(vec![Line::default().dim(format!(
                    "  reasoning {} · /reasoning off|low|medium|high",
                    reasoning_name(self.launch.attached.info.reasoning)
                ))]);
            }
            Some(Builtin::Reasoning) => {
                let reasoning = match rest {
                    "off" => Some(Reasoning::Off),
                    "low" => Some(Reasoning::Low),
                    "medium" => Some(Reasoning::Medium),
                    "high" => Some(Reasoning::High),
                    _ => None,
                };
                if let Some(reasoning) = reasoning {
                    self.set_reasoning(reasoning).await;
                } else {
                    self.note(vec![
                        Line::default().dim("  /reasoning expects off, low, medium, or high"),
                    ]);
                }
            }
            Some(Builtin::Compact) => {
                if !self.idle_only("/compact") {
                    return Ok(());
                }
                match self.launch.client.compact(self.session_id()).await {
                    Ok(result) => self.note(vec![Line::default().dim(format!(
                        "  compacted · {} tokens before",
                        tokens(result.tokens_before)
                    ))]),
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
                }
            }
            None => {
                match self
                    .launch
                    .client
                    .command(self.session_id(), name, rest)
                    .await
                {
                    Ok(Some(result)) => {
                        if let Some(message) = result.message {
                            self.note(
                                message
                                    .lines()
                                    .map(|line| Line::default().dim(format!("  {line}")))
                                    .collect(),
                            );
                        }
                    }
                    Ok(None) => self.note(vec![
                        Line::default().dim(format!("  unknown command /{name} · /help")),
                    ]),
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
                }
            }
        }
        Ok(())
    }

    async fn submit(&mut self, queue: bool) -> Result<(), Error> {
        let text = self.editor.text().trim().to_owned();
        self.editor.clear();
        self.completion = None;
        if text.is_empty() {
            return Ok(());
        }
        if text.starts_with('/') && !text.contains('\n') {
            return self.command(&text).await;
        }
        if self.exited {
            self.note(vec![
                Line::default().dim("  session exited · /new starts another"),
            ]);
            self.editor.set_text(&text);
            self.update_completion();
            return Ok(());
        }
        if let Err(error) = self.history.push(&text) {
            self.note(vec![
                Line::default().dim(format!("  prompt history: {error}")),
            ]);
        }
        self.editor.set_history(self.history.workspace());
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
        self.update_completion();
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

    async fn model_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Enter {
            let entry = self.picker.as_ref().and_then(Picker::selected).cloned();
            if let Some(entry) = entry {
                if !self.idle_only("model selection") {
                    return;
                }
                if !entry.available {
                    let hint = if entry.model.provider == "chatgpt" {
                        " · xi login chatgpt"
                    } else {
                        ""
                    };
                    self.note(vec![Line::default().dim(format!("  no credentials{hint}"))]);
                } else if self.set_model(entry.model).await {
                    self.launch.attached.info.model_info = Some(entry.info);
                    self.close_overlay();
                }
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.close_overlay();
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up => picker.up(),
                KeyCode::Down => picker.down(),
                KeyCode::Backspace => picker.backspace(),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    picker.input(c)
                }
                _ => {}
            }
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => {
                let selected = self
                    .search
                    .as_ref()
                    .and_then(|search| search.current(&self.history))
                    .map(str::to_owned);
                if let Some(selected) = selected {
                    self.editor.set_text(&selected);
                    self.search = None;
                    self.focus = Focus::Composer;
                    self.update_completion();
                }
            }
            _ => {
                if let Some(search) = self.search.as_mut() {
                    match key.code {
                        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            search.older()
                        }
                        KeyCode::Backspace => search.backspace(),
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            search.input(c)
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn key(&mut self, key: KeyEvent) -> Result<(), Error> {
        let action = action(key);
        if matches!(action, Some(Action::Interrupt)) {
            if self.focus != Focus::Composer || self.completion.is_some() {
                self.close_overlay();
            } else if self.running() {
                self.cancel().await;
            } else if !self.editor.is_empty() {
                self.editor.clear();
                self.update_completion();
            } else {
                self.quit = true;
            }
            return Ok(());
        }
        match self.focus {
            Focus::Tree => {
                self.tree_key(key).await;
                return Ok(());
            }
            Focus::Models => {
                self.model_key(key).await;
                return Ok(());
            }
            Focus::Search => {
                self.search_key(key);
                return Ok(());
            }
            Focus::Approval => {
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
            Focus::Composer => {}
        }
        if let Some(menu) = self.completion.as_mut() {
            match action {
                Some(Action::Up) => {
                    menu.up();
                    return Ok(());
                }
                Some(Action::Down) => {
                    menu.down();
                    return Ok(());
                }
                // A token that already equals the selection is complete, so
                // enter sends it instead of accepting the same text again.
                Some(Action::Send)
                    if menu.selected().is_some_and(|item| {
                        self.editor.text().get(menu.range()) == Some(item.insert.trim_end())
                    }) =>
                {
                    self.completion = None;
                }
                Some(Action::Send | Action::Complete) => {
                    self.accept_completion();
                    return Ok(());
                }
                Some(Action::Escape) => {
                    self.completion = None;
                    return Ok(());
                }
                _ => {}
            }
        }
        match action {
            Some(Action::Send) => {
                self.submit(false).await?;
                return Ok(());
            }
            Some(Action::Queue) => {
                self.submit(true).await?;
                return Ok(());
            }
            Some(Action::Newline) => self.editor.insert("\n"),
            Some(Action::Escape) => {
                if self.running() {
                    self.cancel().await;
                }
                return Ok(());
            }
            Some(Action::Complete) => {
                if !self.transcript.asks().is_empty() {
                    self.focus = Focus::Approval;
                    self.choice = 0;
                }
                return Ok(());
            }
            Some(Action::Search) => {
                self.search = Some(Search::new(self.editor.text().to_owned()));
                self.completion = None;
                self.focus = Focus::Search;
                return Ok(());
            }
            Some(Action::External) => {
                self.external_requested = true;
                return Ok(());
            }
            Some(Action::Models) => {
                self.open_models().await;
                return Ok(());
            }
            Some(Action::Reasoning) => {
                let next = match self.launch.attached.info.reasoning {
                    Reasoning::Off => Reasoning::Low,
                    Reasoning::Low => Reasoning::Medium,
                    Reasoning::Medium => Reasoning::High,
                    Reasoning::High => Reasoning::Off,
                };
                self.set_reasoning(next).await;
                return Ok(());
            }
            Some(Action::Expand) => {
                let lines = self.transcript.expand(self.cols).unwrap_or_else(|| {
                    let why = if self.transcript.collapsed() {
                        "  that was the oldest cut-short result · ctrl+o starts again from the newest"
                    } else {
                        "  nothing cut short yet · results longer than three lines show whole here"
                    };
                    self.transcript.note(vec![Line::default().dim(why)])
                });
                self.commit.extend(lines);
                return Ok(());
            }
            Some(Action::DeleteOrQuit) => {
                if self.editor.is_empty() && !self.running() {
                    self.quit = true;
                    return Ok(());
                }
                self.editor.delete();
            }
            Some(Action::Home) => self.editor.home(),
            Some(Action::End) => self.editor.end(),
            Some(Action::Left) => self.editor.left(),
            Some(Action::Right) => self.editor.right(),
            Some(Action::KillBefore) => self.editor.kill_before(),
            Some(Action::KillAfter) => self.editor.kill_after(),
            Some(Action::KillWord) => self.editor.kill_word(),
            Some(Action::Backspace) => self.editor.backspace(),
            Some(Action::Delete) => self.editor.delete(),
            Some(Action::Up) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_back();
                }
            }
            Some(Action::Down) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_forward();
                }
            }
            None => {
                if let KeyCode::Char(c) = key.code
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    let mut buf = [0; 4];
                    self.editor.insert(c.encode_utf8(&mut buf));
                } else {
                    return Ok(());
                }
            }
            Some(Action::Interrupt) => return Ok(()),
        }
        self.update_completion();
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
        let reasoning = if info
            .model_info
            .as_ref()
            .is_some_and(|model| !model.reasoning)
        {
            "no think"
        } else {
            match info.reasoning {
                Reasoning::Off => "think off",
                Reasoning::Low => "think low",
                Reasoning::Medium => "think med",
                Reasoning::High => "think high",
            }
        };
        segs.push(Segment::new("reasoning", d(reasoning.into())));
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
        let overlay_rows = self
            .rows
            .saturating_sub(self.editor.render(cols, Role::Dim, "").0.len() + 3);
        if self.focus == Focus::Models
            && let Some(picker) = &self.picker
        {
            lines.extend(picker.render(cols, overlay_rows));
        }
        if self.focus == Focus::Search
            && let Some(search) = &self.search
        {
            lines.extend(
                search
                    .render(&self.history, cols)
                    .into_iter()
                    .take(overlay_rows),
            );
        }
        if self.focus == Focus::Composer
            && let Some(menu) = &self.completion
        {
            lines.extend(menu.render(cols, overlay_rows.min(8)));
        }
        let border = if self.focus != Focus::Composer {
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
    use xi_protocol::methods::client::{
        Delta, DeltaParams, PendingApproval, ResetParams, SessionDetails,
    };

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
                reasoning: Reasoning::Medium,
                model_info: Some(model_info()),
                parent_session: None,
            },
            events: vec![root],
            partial: Vec::new(),
            stream_sequence: 0,
            running,
            pending_approvals: Vec::new(),
        }
    }

    fn model_info() -> xi_protocol::model::ModelInfo {
        xi_protocol::model::ModelInfo {
            provider: "demo".into(),
            id: "script".into(),
            context_window: 256_000,
            max_output: 4_096,
            reasoning: true,
            pricing: None,
        }
    }

    async fn launch(attached: AttachResult) -> (Launch, mpsc::UnboundedReceiver<(String, Value)>) {
        let (connection, server) = tokio::io::duplex(16 * 1024);
        let (reader, writer) = tokio::io::split(server);
        let (peer, mut incoming) = Peer::spawn(reader, writer);
        let (requests, observed) = mpsc::unbounded_channel();
        let response = attached.clone();
        tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                if let Incoming::Request { id, method, params } = message {
                    let _ = requests.send((method.clone(), params.clone()));
                    let result = match method.as_str() {
                        "initialize" => Ok(json!({
                            "protocol_version": xi_protocol::PROTOCOL_VERSION,
                            "daemon_version": "test",
                            "capabilities": { "private": true },
                        })),
                        "session/attach" | "session/create" => {
                            Ok(serde_json::to_value(&response).unwrap())
                        }
                        "session/models" => Ok(json!({
                            "current": response.info.model,
                            "reasoning": response.info.reasoning,
                            "models": [
                                { "model": response.info.model, "info": model_info(), "available": true },
                                {
                                    "model": { "provider": "demo", "model": "second" },
                                    "info": (xi_protocol::model::ModelInfo { id: "second".into(), ..model_info() }),
                                    "available": true,
                                },
                            ],
                        })),
                        "session/commands" => {
                            Ok(json!([{ "name": "known", "description": "an extension command" }]))
                        }
                        "session/command" if params["name"] == "known" => {
                            Ok(json!({ "message": "extension result" }))
                        }
                        "session/compact" => {
                            Ok(json!({ "head": response.info.head, "tokens_before": 42_000 }))
                        }
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
                history: None,
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
    async fn attach_watermark_deduplicates_queued_deltas_and_resets() {
        let mut attached = snapshot(true);
        attached.partial = vec![Block::Text {
            text: "hello".into(),
        }];
        attached.stream_sequence = 7;
        let (launch, _) = launch(attached).await;
        let mut app = App::new(launch, 80, 24);
        let session = app.session_id();
        app.notify(&ClientNotification::Reset(ResetParams {
            session,
            attempt: 1,
            sequence: 5,
        }));
        for (sequence, text) in [(6, "hel"), (7, "lo"), (8, " world"), (8, " world")] {
            app.notify(&ClientNotification::Delta(DeltaParams {
                session,
                attempt: 1,
                sequence,
                delta: Delta::Text { text: text.into() },
            }));
        }
        let live: Vec<_> = app.transcript.live(80, 0).iter().map(Line::plain).collect();
        assert_eq!(
            live.iter()
                .filter(|line| line.contains("hello world"))
                .count(),
            1,
        );
        assert!(live.iter().all(|line| !line.contains("hello world world")));
        app.notify(&ClientNotification::Reset(ResetParams {
            session,
            attempt: 2,
            sequence: 9,
        }));
        assert!(app.transcript.live(80, 0).is_empty());
        app.notify(&ClientNotification::Delta(DeltaParams {
            session,
            attempt: 2,
            sequence: 10,
            delta: Delta::Text {
                text: "next".into(),
            },
        }));
        assert!(
            app.transcript
                .live(80, 0)
                .iter()
                .any(|line| line.plain().contains("next"))
        );
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
            while let Ok((method, _)) = requests.try_recv() {
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

    fn request(requests: &mut mpsc::UnboundedReceiver<(String, Value)>, wanted: &str) -> Value {
        while let Ok((method, params)) = requests.try_recv() {
            if method == wanted {
                return params;
            }
        }
        panic!("missing request {wanted}");
    }

    #[tokio::test]
    async fn shift_tab_sets_reasoning_and_updates_the_footer() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 160, 24);
        assert!(app.footer(160).plain().contains("think med"));
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await
            .unwrap();
        let params = request(&mut requests, "session/set_reasoning");
        assert_eq!(params["session"], json!(app.session_id()));
        assert_eq!(params["reasoning"], "high");
        assert_eq!(app.launch.attached.info.reasoning, Reasoning::High);
        assert!(app.footer(160).plain().contains("think high"));
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.footer(160).plain().contains("think off"));
        app.launch
            .attached
            .info
            .model_info
            .as_mut()
            .unwrap()
            .reasoning = false;
        assert!(app.footer(160).plain().contains("no think"));
    }

    #[tokio::test]
    async fn model_command_opens_the_picker_and_enter_selects_a_model() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 120, 24);
        app.command("/model").await.unwrap();
        assert!(app.focus == Focus::Models);
        assert_eq!(
            request(&mut requests, "session/models")["session"],
            json!(app.session_id())
        );
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "session/set_model")["model"],
            json!({
                "provider": "demo", "model": "second",
            })
        );
        assert_eq!(app.launch.attached.info.model.model, "second");
        assert!(app.focus == Focus::Composer);
    }

    #[tokio::test]
    async fn unknown_commands_are_sent_to_extensions_and_point_to_help() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        app.command("/missing raw arguments").await.unwrap();
        let params = request(&mut requests, "session/command");
        assert_eq!(params["name"], "missing");
        assert_eq!(params["args"], "raw arguments");
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("unknown command /missing · /help"))
        );
        app.command("/known argument").await.unwrap();
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("extension result"))
        );
    }

    #[tokio::test]
    async fn accepting_command_completion_replaces_the_composer_without_submitting() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        for c in "/rea".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert!(app.completion.is_some());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.text(), "/reasoning ");
        assert!(app.completion.is_none());
        assert!(app.pending.is_empty());
        assert!(!app.running());
        while let Ok((method, _)) = requests.try_recv() {
            assert_eq!(method, "initialize");
        }
    }

    #[tokio::test]
    async fn enter_on_a_fully_typed_command_runs_it() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        app.commit.clear();
        for c in "/help".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert!(app.completion.is_some());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.editor.is_empty());
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("show keys and commands"))
        );
    }

    #[tokio::test]
    async fn submitted_prompts_are_persisted_but_commands_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("prompts.jsonl");
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.history = Some(path.clone());
        let mut app = App::new(launch, 80, 24);
        app.editor.set_text("persist this prompt");
        app.submit(false).await.unwrap();
        let reply = app.pending.next().await.unwrap();
        app.replied(reply);
        app.editor.set_text("/reasoning");
        app.submit(false).await.unwrap();
        let lines = std::fs::read_to_string(path).unwrap();
        let entries: Vec<Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["text"], "persist this prompt");
        assert_eq!(entries[0]["cwd"], json!(app.launch.attached.info.cwd));
        app.editor.history_back();
        assert_eq!(app.editor.text(), "persist this prompt");
    }

    #[tokio::test]
    async fn history_search_walks_older_matches_and_restores_the_draft() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        app.history.push("first matching prompt").unwrap();
        app.history.push("second matching prompt").unwrap();
        app.editor.set_text("unfinished draft");
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(
            app.search.as_ref().unwrap().current(&app.history),
            Some("first matching prompt")
        );
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.text(), "unfinished draft");
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.text(), "second matching prompt");
        assert!(app.focus == Focus::Composer);
    }

    #[tokio::test]
    async fn model_reasoning_and_compaction_changes_are_refused_while_running() {
        let (launch, mut requests) = launch(snapshot(true)).await;
        let mut app = App::new(launch, 80, 24);
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await
            .unwrap();
        app.command("/model demo/second").await.unwrap();
        app.command("/compact").await.unwrap();
        app.command("/new").await.unwrap();
        assert_eq!(app.launch.attached.info.reasoning, Reasoning::Medium);
        assert_eq!(app.launch.attached.info.model.model, "script");
        while let Ok((method, _)) = requests.try_recv() {
            assert_eq!(method, "initialize");
        }
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("waits for the turn to end"))
        );
    }

    #[tokio::test]
    async fn reasoning_and_model_events_from_other_clients_update_metadata() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 160, 24);
        for kind in [
            EventKind::ReasoningChanged {
                reasoning: Reasoning::Low,
            },
            EventKind::ModelChanged {
                model: ModelRef {
                    provider: "demo".into(),
                    model: "other".into(),
                },
            },
        ] {
            app.notify(&ClientNotification::Event(EventParams {
                session: app.session_id(),
                event: Event {
                    id: EventId::generate(),
                    parent: Some(app.launch.attached.info.head),
                    at: Timestamp(1),
                    kind,
                },
            }));
        }
        assert_eq!(app.launch.attached.info.reasoning, Reasoning::Low);
        assert_eq!(app.launch.attached.info.model.model, "other");
        assert!(app.refresh);
        assert!(app.footer(160).plain().contains("think low"));
    }

    #[tokio::test]
    async fn help_uses_dispatch_bindings_and_only_implemented_commands() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 80, 24);
        app.commit.clear();
        app.command("/hotkeys").await.unwrap();
        let help = app
            .commit
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        for (code, modifiers, _, label, _) in KEYS {
            assert!(action(KeyEvent::new(*code, *modifiers)).is_some());
            assert!(help.contains(label));
        }
        for later in ["/resume", "/approvals", "/settings", "/copy", "ctrl+t"] {
            assert!(!help.contains(later));
            assert!(
                !app.commands
                    .iter()
                    .any(|command| format!("/{}", command.name) == later)
            );
        }
    }

    #[tokio::test]
    async fn session_completion_loads_extension_commands_and_workspace_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("source.rs"), "content").unwrap();
        let mut attached = snapshot(false);
        attached.info.cwd = temp.path().to_path_buf();
        let (launch, mut requests) = launch(attached).await;
        let mut app = App::new(launch, 80, 24);
        app.initialize_composer().await;
        request(&mut requests, "session/commands");
        app.editor.set_text("/kno");
        app.update_completion();
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.text(), "/known ");
        app.editor.set_text("read @sour");
        app.update_completion();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(app.editor.text(), "read @source.rs ");
    }
}
