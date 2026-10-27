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

pub mod approvals;
pub mod children;
pub mod clipboard;
mod commands;
pub mod complete;
pub mod config;
pub mod deck;
pub mod editor;
pub mod external;
pub mod frame;
mod git;
pub mod history;
pub mod icons;
mod input;
pub mod keys;
pub mod layout;
pub mod log;
mod overlay;
mod overlays;
mod panes;
pub mod picker;
pub mod roles;
pub mod screen;
pub mod sessions;
pub mod settings;
pub mod status;
pub mod style;
pub mod transcript;
pub mod tree;
mod view;

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyEventKind,
};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::{mpsc, oneshot};
use xi_client::client::{Client, ClientError, ClientNotification};
use xi_protocol::manifest::CommandSpec;
use xi_protocol::methods::client::{AttachResult, CreateParams, EventParams, PromptResult};
use xi_protocol::rpc::RpcError;
use xi_protocol::{Block, Event, EventId, SessionId, ToolCallId};

use crate::approvals::{Approval, Approvals};
use crate::commands::builtin_commands;
use crate::complete::{FileIndex, Menu};
use crate::config::TuiConfig;
use crate::editor::Editor;
use crate::history::History;
use crate::keys::Keymap;
use crate::layout::{Layout, Measure};
use crate::overlay::Overlays;
use crate::panes::Deck;
use crate::screen::{Screen, Settle};
use crate::style::{Line, Role};
use crate::transcript::{Transcript, duration, tokens};

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
    /// Open the session picker on startup, focused on the latest other session.
    pub pick_session: bool,
    /// Shared prompt history, or in-memory history when absent.
    pub history: Option<PathBuf>,
    /// Client key and display settings, or defaults when absent.
    pub tui_config: Option<PathBuf>,
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

/// Saves the terminal's title and restores it however the client exits.
/// Terminals without the xterm title stack keep the last `ξ` title.
struct TitleStack;

impl TitleStack {
    fn push() -> std::io::Result<Self> {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b[22;0t")?;
        out.flush()?;
        Ok(Self)
    }
}

impl Drop for TitleStack {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[23;0t");
        let _ = out.flush();
    }
}

type Reply = oneshot::Receiver<Result<PromptResult, ClientError>>;

enum ExternalRequest {
    Composer,
    Settings(PathBuf),
}

struct App {
    launch: Launch,
    git: git::Status,
    transcript: Transcript,
    editor: Editor,
    /// Images attached to the current composer draft, not prompt history.
    images: Vec<Block>,
    /// Open overlays; only the top one draws and gets keys. Empty while
    /// the composer has them.
    overlays: Overlays,
    approval_workspaces: HashMap<SessionId, PathBuf>,
    /// Other sessions' asks this client answered. The daemon drops an ask
    /// from `approval/list` only when its resolution event arrives, which
    /// can be after the resolve reply.
    answered: HashSet<(SessionId, ToolCallId)>,
    inbox_refresh: bool,
    keymap: Keymap,
    /// The saved choice of terminal layout.
    layout: Layout,
    /// Which glyphs the deck draws.
    icons: icons::Icons,
    /// The deck while the client owns the alternate screen.
    deck: Option<Deck>,
    /// Terminal controls written before the next frame.
    controls: Vec<String>,
    /// The session's latest title, from its log.
    title: Option<String>,
    /// Whether the terminal shows `title`; frames write it only when not.
    title_written: bool,
    history: History,
    commands: Vec<CommandSpec>,
    completion: Option<Menu>,
    files: Option<FileIndex>,
    file_cwd: Option<PathBuf>,
    external_requested: Option<ExternalRequest>,
    /// Replies of prompts sent to the running turn.
    pending: FuturesUnordered<Reply>,
    started: Option<Instant>,
    /// Sent during a turn, shown until the worker takes them in.
    steering: Vec<Vec<Block>>,
    /// Held until the turn ends, then sent in order.
    queued: Vec<Vec<Block>>,
    frame: usize,
    cols: usize,
    rows: usize,
    commit: Vec<Line>,
    /// Clears the terminal before drawing the fresh session.
    clear_scrollback: bool,
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
    app.initialize().await;
    let _title_stack = TitleStack::push()?;
    let mut raw = Some(Raw::enter()?);
    let mut screen = Screen::new(std::io::stdout(), color, usize::from(cols));
    let mut settle = Settle::new(cols);
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(125));
    let mut git_tick = tokio::time::interval(Duration::from_secs(1));
    let mut on_deck = false;
    while !app.quit {
        if let Some(request) = app.external_requested.take() {
            let settings_file = matches!(&request, ExternalRequest::Settings(_));
            screen.finish()?;
            drop(keys);
            drop(raw.take());
            let edited = match request {
                ExternalRequest::Composer => external::edit(app.editor.text()),
                ExternalRequest::Settings(path) => external::edit_file(&path).map(|()| None),
            };
            raw = Some(Raw::enter()?);
            // The editor may have set its own title.
            app.title_written = false;
            crossterm::execute!(
                std::io::stdout(),
                crossterm::cursor::MoveToColumn(0),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
            )?;
            keys = EventStream::new();
            if settings_file {
                app.refresh_settings().await;
                if let Err(error) = edited {
                    app.settings_note(format!("external editor: {error}"));
                }
            } else {
                match edited {
                    Ok(Some(text)) => app.editor.set_text(&text),
                    Ok(None) => {}
                    Err(error) => app.note(vec![
                        Line::default().dim(format!("  external editor: {error}")),
                    ]),
                }
                app.update_completion();
            }
        }
        if app.refresh {
            app.refresh_details().await;
        }
        if app.inbox_refresh {
            app.refresh_inbox().await;
        }
        app.git.refresh(&app.launch.attached.info.cwd).await;
        app.refresh_deck().await;
        // The deck owns the alternate screen; entering and leaving it are
        // the frame's first controls.
        let deck = app.deck.is_some();
        if deck != on_deck {
            if deck {
                screen.finish()?;
                screen.enter_deck()?;
            } else {
                screen.leave_deck()?;
            }
            on_deck = deck;
        }
        // Use one current size for layout and cursor tracking. Queued resize
        // events can describe an older size while the window is being dragged.
        // Frames wait until the width settles: a frame read after a further
        // resize would rewind short and leave a copy of the live region. The
        // deck redraws whole, so it never waits.
        if let Ok((c, r)) = crossterm::terminal::size() {
            app.cols = usize::from(c).max(20);
            app.rows = usize::from(r).max(4);
            if settle.ready(c, Instant::now()) || deck {
                screen.resize(usize::from(c));
                app.draw(&mut screen)?;
            }
        } else {
            app.draw(&mut screen)?;
        }
        let settled = settle.deadline().map(tokio::time::Instant::from_std);
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
                Some(Ok(TermEvent::Paste(text))) if app.overlays.is_empty() => {
                    app.editor.insert(&text);
                    app.update_completion();
                }
                // Resize events only wake the loop; the next frame queries
                // the current size rather than rendering a queued stale one.
                Some(Ok(TermEvent::Resize(..))) => {}
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => break,
            },
            () = tokio::time::sleep_until(settled.unwrap_or_else(tokio::time::Instant::now)), if settled.is_some() => {}
            _ = git_tick.tick() => {},
            _ = tick.tick(), if app.started.is_some() => {
                app.frame += 1;
                if app.pending.is_empty() && app.frame.is_multiple_of(8) {
                    app.refresh_details().await;
                }
            },
        }
    }
    if let Ok((c, _)) = crossterm::terminal::size() {
        screen.resize(usize::from(c));
    }
    // Quitting from the deck leaves its log in scrollback, as /layout
    // inline would.
    app.leave_deck();
    if on_deck {
        screen.leave_deck()?;
    }
    screen.draw(&std::mem::take(&mut app.commit), &[], None, app.rows)?;
    screen.finish()?;
    drop(raw);
    app.release().await?;
    Ok(())
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

/// The latest title in `events`, in file order and whatever the branch, as
/// `session/list` reports it.
fn latest_title(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.kind {
        xi_protocol::EventKind::SessionTitled { title, .. } => Some(title.clone()),
        _ => None,
    })
}

/// The OSC 0 sequence that titles the terminal `ξ: title`, or `ξ` before
/// the session has a title. Control characters are dropped so a title
/// cannot end the sequence early.
fn title_control(title: Option<&str>) -> String {
    let mut control = String::from("\x1b]0;ξ");
    if let Some(title) = title {
        control.push_str(": ");
        control.extend(title.chars().filter(|c| !c.is_control()));
    }
    control.push('\x07');
    control
}

impl App {
    fn new(mut launch: Launch, cols: usize, rows: usize) -> Self {
        launch.template.cwd = launch.attached.info.cwd.clone();
        let (config, warnings) = TuiConfig::load(launch.tui_config.as_deref());
        let mut transcript = Transcript::default();
        transcript.set_thinking(config.thinking);
        let (history, history_error) =
            History::load(launch.history.clone(), &launch.attached.info.cwd);
        let mut editor = Editor::default();
        editor.set_history(history.workspace());
        let mut app = Self {
            launch,
            git: git::Status::default(),
            transcript,
            editor,
            images: Vec::new(),
            overlays: Overlays::default(),
            approval_workspaces: HashMap::new(),
            answered: HashSet::new(),
            inbox_refresh: false,
            keymap: config.keymap,
            layout: config.layout,
            icons: config.icons,
            deck: config.layout.deck_at(cols, rows).then(Deck::default),
            controls: Vec::new(),
            title: None,
            title_written: false,
            history,
            commands: builtin_commands(),
            completion: None,
            files: None,
            file_cwd: None,
            external_requested: None,
            pending: FuturesUnordered::new(),
            started: None,
            steering: Vec::new(),
            queued: Vec::new(),
            frame: 0,
            cols: cols.max(20),
            rows: rows.max(4),
            commit: Vec::new(),
            clear_scrollback: false,
            quit: false,
            failed: false,
            seen: HashSet::new(),
            resolved: HashSet::new(),
            refresh: false,
            exited: false,
        };
        app.attach();
        for warning in warnings {
            app.note(vec![Line::default().dim(format!("  tui.toml: {warning}"))]);
        }
        if let Some(error) = history_error {
            app.note(vec![
                Line::default().dim(format!("  prompt history: {error}")),
            ]);
        }
        app
    }

    async fn initialize(&mut self) {
        self.initialize_composer().await;
        if self.launch.pick_session {
            self.open_sessions().await;
            if let Some(picker) = self.overlays.get_mut::<sessions::Sessions>() {
                picker.select_previous();
            }
        }
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

    /// Closes every overlay and returns the keys to the composer.
    fn close_overlay(&mut self) {
        self.overlays.clear();
        self.inbox_refresh = false;
        self.completion = None;
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
        if let ClientNotification::Event(params) = notification
            && params.session != self.session_id()
            && self.transcript.child(self.session_id(), params)
        {
            return;
        }
        if !self.mine(notification) {
            if let ClientNotification::ApprovalPending(pending) = notification {
                let cwd = self.approval_workspaces.get(&pending.session).cloned();
                let current = self.session_id();
                if let Some(approvals) = self.overlays.get_mut::<Approvals>()
                    && approvals.inbox
                {
                    approvals.upsert(
                        Approval {
                            pending: pending.clone(),
                            cwd,
                        },
                        current,
                    );
                    self.inbox_refresh = true;
                }
            }
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
        let lines = self.transcript.apply_live(notification, self.measure());
        self.commit_lines(lines);
        match notification {
            ClientNotification::Event(p) => match &p.event.kind {
                xi_protocol::EventKind::UserMessage { content } => {
                    if self.started.is_none() {
                        self.started = Some(Instant::now());
                        self.transcript.begin_turn();
                    }
                    self.launch.attached.running = true;
                    if let Some(i) = self.steering.iter().position(|s| s == content) {
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
                // The deck lists titles from `session/list`; fetch it now.
                xi_protocol::EventKind::SessionTitled { title, .. } => {
                    self.title = Some(title.clone());
                    self.title_written = false;
                    if let Some(deck) = self.deck.as_mut() {
                        deck.refreshed = None;
                    }
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
        // A new ask takes the keys only when nothing else has them;
        // otherwise it waits on the status line for `tab`.
        let now = self.transcript.asks().len();
        if now > asks
            && self.editor.is_empty()
            && self.images.is_empty()
            && self.overlays.is_empty()
        {
            self.open_own_approvals();
        }
        if self.overlays.get::<Approvals>().is_some() {
            let entries = self.own_approvals();
            let session = self.session_id();
            if let Some(approvals) = self.overlays.get_mut::<Approvals>() {
                approvals.replace_current(entries, session);
            }
            self.close_empty_approvals();
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
                self.commit_lines(lines);
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
        let lines = self.transcript.flush(self.measure());
        self.commit_lines(lines);
        self.launch.attached.running = false;
        self.transcript.end_turn();
        if let Some(approvals) = self.overlays.get_mut::<Approvals>() {
            if approvals.inbox {
                approvals.replace_current(Vec::new(), self.launch.attached.info.session);
                self.close_empty_approvals();
            } else {
                self.overlays.close::<Approvals>();
            }
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
        self.commit_lines(lines);
        self.steering.clear();
        if !self.exited && !self.queued.is_empty() {
            let next = self.queued.remove(0);
            self.send(next);
        }
    }

    fn send(&mut self, content: Vec<Block>) {
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
            self.steering.push(content.clone());
        }
        self.transcript.sent(&content);
        self.launch.attached.running = true;
        let client = self.launch.client.clone();
        let session = self.session_id();
        let (send, reply) = oneshot::channel();
        tokio::spawn(async move {
            let _ = send.send(client.prompt(session, content).await);
        });
        self.pending.push(reply);
    }

    fn running(&self) -> bool {
        self.started.is_some()
    }

    /// The measure of the pane drawn into: the terminal inline, the log
    /// column on the deck.
    fn measure(&self) -> Measure {
        if self.deck.is_some() {
            self.deck_measure()
        } else {
            Measure::new(self.cols, self.rows)
        }
    }

    /// Commits rows to the client's log: scrollback inline, the retained
    /// log on the deck.
    fn commit_lines(&mut self, lines: Vec<Line>) {
        let anchors = self.transcript.take_inspects();
        match self.deck.as_mut() {
            Some(deck) => deck.log.extend(lines, anchors),
            None => self.commit.extend(lines),
        }
    }

    fn note(&mut self, lines: Vec<Line>) {
        let lines = self.transcript.note(lines);
        self.commit_lines(lines);
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

    async fn paste_image(&mut self) {
        let image = tokio::task::spawn_blocking(clipboard::image)
            .await
            .unwrap_or_else(|error| Err(error.into()));
        match image {
            Ok(image) => {
                self.images.push(image);
                self.completion = None;
            }
            Err(error) => self.note(vec![Line::default().dim(format!("  paste image: {error}"))]),
        }
    }

    async fn submit(&mut self, queue: bool) -> Result<(), Error> {
        let text = self.editor.text().trim().to_owned();
        self.editor.clear();
        self.completion = None;
        if text.starts_with('/') && !text.contains('\n') {
            // Commands leave attachments in the draft unless they switch sessions.
            return self.command(&text).await;
        }
        if text.is_empty() && self.images.is_empty() {
            return Ok(());
        }
        if self.exited {
            self.note(vec![
                Line::default().dim("  session exited · /new starts another"),
            ]);
            self.editor.set_text(&text);
            self.update_completion();
            return Ok(());
        }
        if !text.is_empty() {
            if let Err(error) = self.history.push(&text) {
                self.note(vec![
                    Line::default().dim(format!("  prompt history: {error}")),
                ]);
            }
            self.editor.set_history(self.history.workspace());
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Block::Text { text });
        }
        content.append(&mut self.images);
        if queue && self.running() {
            self.queued.push(content);
        } else {
            self.send(content);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
