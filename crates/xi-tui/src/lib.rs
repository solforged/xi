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
pub mod clipboard;
pub mod complete;
pub mod config;
pub mod editor;
pub mod external;
pub mod history;
pub mod keys;
pub mod picker;
pub mod screen;
pub mod sessions;
pub mod settings;
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
use xi_protocol::methods::client::{
    AttachResult, CreateParams, EventParams, ListParams, OpenParams, PendingApproval, PromptResult,
    SessionSummary, SetSettingParams, SettingsParams,
};
use xi_protocol::model::Reasoning;
use xi_protocol::rpc::RpcError;
use xi_protocol::{Block, Event, EventId, SessionId, Timestamp, ToolCallId};

use crate::approvals::{Approval, Approvals, Choice};
use crate::complete::{FileIndex, Menu};
use crate::config::TuiConfig;
use crate::editor::Editor;
use crate::history::{History, Search};
use crate::keys::{Action, Keymap};
use crate::picker::Picker;
use crate::screen::Screen;
use crate::sessions::Sessions;
use crate::settings::Settings;
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

#[derive(Clone, Copy)]
enum Builtin {
    Help,
    Quit,
    New,
    Resume,
    Approvals,
    Settings,
    Copy,
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
        "resume",
        "pick a session to attach or open",
        Builtin::Resume,
    ),
    (
        "approvals",
        "answer pending approvals in every session",
        Builtin::Approvals,
    ),
    ("settings", "show and change settings", Builtin::Settings),
    (
        "copy",
        "copy the last response to the clipboard",
        Builtin::Copy,
    ),
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

/// The footer's form of a level.
fn short_reasoning(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Minimal => "min",
        Reasoning::Medium => "med",
        level => level.name(),
    }
}

/// The levels a model lists, lowest first, joined by `separator`.
fn reasoning_levels(model: &xi_protocol::model::ModelInfo, separator: &str) -> String {
    let mut levels = model.reasoning.clone();
    levels.sort_unstable();
    levels.dedup();
    levels
        .iter()
        .map(|level| level.name())
        .collect::<Vec<_>>()
        .join(separator)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Composer,
    Approval,
    Tree,
    Models,
    Search,
    Sessions,
    Settings,
}

type Reply = oneshot::Receiver<Result<PromptResult, ClientError>>;

enum ExternalRequest {
    Composer,
    Settings(PathBuf),
}

struct App {
    launch: Launch,
    transcript: Transcript,
    editor: Editor,
    focus: Focus,
    /// The session tree overlay, while it is open.
    tree: Option<Tree>,
    picker: Option<Picker>,
    search: Option<Search>,
    sessions: Option<Sessions>,
    approvals: Option<Approvals>,
    settings: Option<Settings>,
    approval_workspaces: HashMap<SessionId, PathBuf>,
    /// Other sessions' asks this client answered. The daemon drops an ask
    /// from `approval/list` only when its resolution event arrives, which
    /// can be after the resolve reply.
    answered: HashSet<(SessionId, ToolCallId)>,
    inbox_refresh: bool,
    keymap: Keymap,
    /// Terminal controls written before the next frame.
    controls: Vec<String>,
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
            transcript,
            editor,
            focus: Focus::Composer,
            tree: None,
            picker: None,
            search: None,
            sessions: None,
            approvals: None,
            settings: None,
            approval_workspaces: HashMap::new(),
            answered: HashSet::new(),
            inbox_refresh: false,
            keymap: config.keymap,
            controls: Vec::new(),
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
        self.sessions = None;
        self.approvals = None;
        self.settings = None;
        self.inbox_refresh = false;
        self.focus = Focus::Composer;
        self.completion = None;
    }

    fn own_approvals(&self) -> Vec<Approval> {
        let info = &self.launch.attached.info;
        self.transcript
            .asks()
            .iter()
            .filter(|ask| !self.resolved.contains(&ask.call))
            .map(|ask| Approval {
                pending: PendingApproval {
                    session: info.session,
                    call: ask.call.clone(),
                    tool: ask.tool.clone(),
                    action: ask.action.clone(),
                    rule: ask.rule.clone(),
                    taint: ask.taint,
                    trusted: ask.trusted.unwrap_or(info.trusted),
                },
                cwd: Some(info.cwd.clone()),
            })
            .collect()
    }

    fn open_own_approvals(&mut self) {
        let entries = self.own_approvals();
        if entries.is_empty() {
            return;
        }
        self.approvals = Some(Approvals::new(entries, false));
        self.completion = None;
        self.focus = Focus::Approval;
    }

    fn close_empty_approvals(&mut self) {
        if self
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.current().is_none())
        {
            self.approvals = None;
            self.focus = Focus::Composer;
        }
    }

    async fn refresh_inbox(&mut self) {
        self.inbox_refresh = false;
        let pending = match self.launch.client.approvals().await {
            Ok(pending) => pending,
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        };
        let current = self.session_id();
        if pending.iter().any(|ask| {
            ask.session != current && !self.approval_workspaces.contains_key(&ask.session)
        }) {
            match self.launch.client.list(ListParams::default()).await {
                Ok(sessions) => {
                    self.approval_workspaces = sessions
                        .into_iter()
                        .map(|session| (session.session, session.cwd))
                        .collect();
                }
                Err(error) => self.note(vec![
                    Line::default().dim(format!("  approval workspaces: {error}")),
                ]),
            }
        }
        let mut entries = self.own_approvals();
        entries.extend(
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
        if let Some(approvals) = self.approvals.as_mut() {
            approvals.replace(entries);
        } else {
            self.approvals = Some(Approvals::new(entries, true));
        }
        self.completion = None;
        self.focus = Focus::Approval;
        self.close_empty_approvals();
    }

    async fn open_sessions(&mut self) {
        if !self.idle_only("/resume") {
            return;
        }
        match self
            .launch
            .client
            .list(ListParams {
                cwd: Some(self.launch.attached.info.cwd.clone()),
                running: None,
            })
            .await
        {
            Ok(sessions) => {
                self.sessions = Some(Sessions::new(sessions, self.session_id()));
                self.completion = None;
                self.focus = Focus::Sessions;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    async fn switch_session(&mut self, attached: AttachResult) -> Result<(), Error> {
        self.release().await?;
        let thinking = self.transcript.thinking_display();
        let changed_workspace = attached.info.cwd != self.launch.attached.info.cwd;
        self.launch.template.cwd = attached.info.cwd.clone();
        self.launch.attached = attached;
        self.exited = false;
        self.refresh = false;
        self.failed = false;
        self.started = None;
        self.pending = FuturesUnordered::new();
        self.transcript = Transcript::default();
        self.transcript.set_thinking(thinking);
        self.close_overlay();
        self.inbox_refresh = false;
        self.steering.clear();
        self.queued.clear();
        self.editor = Editor::default();
        let error = if changed_workspace {
            let (history, error) =
                History::load(self.launch.history.clone(), &self.launch.attached.info.cwd);
            self.history = history;
            error
        } else {
            None
        };
        self.editor.set_history(self.history.workspace());
        self.note(vec![style::rule(self.cols)]);
        self.attach();
        if let Some(error) = error {
            self.note(vec![
                Line::default().dim(format!("  prompt history: {error}")),
            ]);
        }
        self.initialize_composer().await;
        Ok(())
    }

    async fn resume(&mut self, session: SessionSummary) -> Result<(), Error> {
        if session.session == self.session_id() {
            self.close_overlay();
            return Ok(());
        }
        if !self.idle_only("/resume") {
            return Ok(());
        }
        let result = if session.head.is_some() {
            self.launch.client.attach(session.session, None).await
        } else {
            self.launch
                .client
                .open(OpenParams {
                    path: session.path,
                    options: self.launch.template.options.clone(),
                })
                .await
        };
        let attached = match result {
            Ok(attached) => attached,
            Err(ClientError::Rpc {
                code: RpcError::SESSION_HELD,
                data: Some(data),
                ..
            }) if data.get("session").is_some() => {
                let held: SessionId = serde_json::from_value(data["session"].clone())?;
                self.launch.client.attach(held, None).await?
            }
            Err(error) => return Err(error.into()),
        };
        self.switch_session(attached).await
    }

    async fn sessions_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => {
                if let Some(session) = self.sessions.as_ref().and_then(Sessions::selected).cloned()
                    && let Err(error) = self.resume(session).await
                {
                    self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                }
            }
            KeyCode::Tab => {
                let all = self.sessions.as_ref().is_some_and(|picker| !picker.all);
                let cwd = (!all).then(|| self.launch.attached.info.cwd.clone());
                match self
                    .launch
                    .client
                    .list(ListParams { cwd, running: None })
                    .await
                {
                    Ok(sessions) => {
                        if let Some(picker) = self.sessions.as_mut() {
                            picker.replace(sessions, all);
                        }
                    }
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
                }
            }
            _ => {
                if let Some(picker) = self.sessions.as_mut() {
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
        }
    }

    fn settings_params(&self) -> SettingsParams {
        SettingsParams {
            cwd: self.launch.template.cwd.clone(),
            configs: self.launch.template.options.configs.clone(),
        }
    }

    async fn open_settings(&mut self) {
        match self.launch.client.settings(self.settings_params()).await {
            Ok(report) => {
                self.close_overlay();
                self.settings = Some(Settings::new(report));
                self.focus = Focus::Settings;
            }
            Err(error) => {
                self.close_overlay();
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            }
        }
    }

    fn settings_note(&mut self, note: impl Into<String>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.note(note);
        }
    }

    async fn refresh_settings(&mut self) {
        match self.launch.client.settings(self.settings_params()).await {
            Ok(report) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.replace(report);
                }
            }
            Err(error) => self.settings_note(error.to_string()),
        }
    }

    async fn settings_key(&mut self, key: KeyEvent) {
        let info = &self.launch.attached.info;
        let Some(settings) = self.settings.as_mut() else {
            self.focus = Focus::Composer;
            return;
        };
        match settings.key(key, &info.model, info.reasoning) {
            settings::Action::None => {}
            settings::Action::Close => self.close_overlay(),
            settings::Action::Models => self.open_models().await,
            settings::Action::Reasoning => self.cycle_reasoning().await,
            settings::Action::Thinking => {
                let thinking = self.transcript.thinking_display().next();
                self.transcript.set_thinking(thinking);
                let saved = self
                    .launch
                    .tui_config
                    .as_deref()
                    .map(|path| config::save_thinking(path, thinking));
                match saved {
                    Some(Ok(())) => self.settings_note("saved · thinking display"),
                    Some(Err(error)) => self.settings_note(error.to_string()),
                    None => self.settings_note("not saved: no config path"),
                }
            }
            settings::Action::Set { key, value } => {
                let params = self.settings_params();
                match self
                    .launch
                    .client
                    .set_setting(SetSettingParams {
                        cwd: params.cwd,
                        configs: params.configs,
                        key: key.into(),
                        value,
                    })
                    .await
                {
                    Ok(report) => {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.saved(report);
                        }
                    }
                    Err(error) => self.settings_note(error.to_string()),
                }
            }
            settings::Action::EditFile => {
                if let Some(settings) = &self.settings {
                    self.external_requested = Some(ExternalRequest::Settings(
                        settings.report.files.global.clone(),
                    ));
                }
            }
        }
    }

    fn close_models(&mut self) {
        if self.settings.is_some() {
            self.picker = None;
            self.focus = Focus::Settings;
        } else {
            self.close_overlay();
        }
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

    /// Steps to the model's next accepted level. Does nothing until the
    /// next attach supplies a switched model's levels.
    async fn cycle_reasoning(&mut self) {
        let info = &self.launch.attached.info;
        if let Some(next) = info
            .model_info
            .as_ref()
            .and_then(|model| model.next_reasoning(info.reasoning))
        {
            self.set_reasoning(next).await;
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
        if replayed > 0 {
            let note = self.transcript.note(vec![Line::default().dim(format!(
                "  resumed · {replayed} events · {}",
                home(&info.path)
            ))]);
            self.commit.extend(note);
        }
        if !self.transcript.asks().is_empty() {
            self.open_own_approvals();
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
            if let ClientNotification::ApprovalPending(pending) = notification {
                let cwd = self.approval_workspaces.get(&pending.session).cloned();
                let current = self.session_id();
                if let Some(approvals) = self.approvals.as_mut()
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
            self.open_own_approvals();
        }
        if self.focus == Focus::Approval {
            let entries = self.own_approvals();
            let session = self.session_id();
            if let Some(approvals) = self.approvals.as_mut() {
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
        if self.focus == Focus::Approval
            && !self
                .approvals
                .as_ref()
                .is_some_and(|approvals| approvals.inbox)
        {
            self.close_overlay();
        }
        if self.focus == Focus::Approval {
            let session = self.session_id();
            if let Some(approvals) = self.approvals.as_mut() {
                approvals.replace_current(Vec::new(), session);
            }
            self.close_empty_approvals();
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
                for (key, description) in self.keymap.help() {
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
                self.switch_session(attached).await?;
            }
            Some(Builtin::Resume) => self.open_sessions().await,
            Some(Builtin::Approvals) => {
                self.approvals = None;
                self.refresh_inbox().await;
                if self.approvals.is_none() {
                    self.note(vec![Line::default().dim("  no pending approvals")]);
                }
            }
            Some(Builtin::Settings) => self.open_settings().await,
            Some(Builtin::Copy) => {
                if let Some(text) = self.transcript.last_text() {
                    let lines = text.lines().count();
                    self.controls.push(clipboard::osc52(text));
                    self.note(vec![Line::default().dim(format!("  copied {lines} lines"))]);
                } else {
                    self.note(vec![Line::default().dim("  nothing to copy yet")]);
                }
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
                let info = &self.launch.attached.info;
                let line = match &info.model_info {
                    Some(model) if model.reasoning.is_empty() => {
                        format!("  {} does not reason", model.id)
                    }
                    Some(model) => format!(
                        "  reasoning {} · /reasoning {}",
                        model.resolve(info.reasoning),
                        reasoning_levels(model, "|")
                    ),
                    None => format!("  reasoning {}", info.reasoning),
                };
                self.note(vec![Line::default().dim(line)]);
            }
            Some(Builtin::Reasoning) => match rest.parse::<Reasoning>() {
                Ok(reasoning) => self.set_reasoning(reasoning).await,
                Err(error) => {
                    let names = match &self.launch.attached.info.model_info {
                        Some(model) if !model.reasoning.is_empty() => reasoning_levels(model, ", "),
                        _ => Reasoning::ALL.map(Reasoning::name).join(", "),
                    };
                    self.note(vec![Line::default().dim(format!("  {error}; use {names}"))]);
                }
            },
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

    async fn answer(&mut self, choice: Choice) {
        let Some(ask) = self
            .approvals
            .as_ref()
            .and_then(Approvals::current)
            .cloned()
        else {
            return;
        };
        let pending = ask.pending;
        match self
            .launch
            .client
            .resolve(
                pending.session,
                pending.call.clone(),
                choice.approved(),
                choice.remember(),
            )
            .await
        {
            Ok(())
            | Err(ClientError::Rpc {
                code: RpcError::APPROVAL_NOT_PENDING,
                ..
            }) => {}
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        }
        if pending.session == self.session_id() {
            self.resolved.insert(pending.call.clone());
        } else {
            self.answered
                .insert((pending.session, pending.call.clone()));
        }
        let inbox = self
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.inbox);
        if let Some(approvals) = self.approvals.as_mut() {
            approvals.remove(pending.session, &pending.call);
        }
        if inbox {
            self.refresh_inbox().await;
        } else {
            self.close_empty_approvals();
        }
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
        transcript.set_thinking(self.transcript.thinking_display());
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
                    self.close_models();
                }
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.close_models();
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
        let action = self.keymap.action(key);
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
            Focus::Sessions => {
                self.sessions_key(key).await;
                return Ok(());
            }
            Focus::Settings => {
                self.settings_key(key).await;
                return Ok(());
            }
            Focus::Approval => {
                let choice = match key.code {
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.approvals
                            .as_ref()
                            .and_then(|approvals| approvals.shortcut(c))
                    }
                    KeyCode::Enter => self.approvals.as_ref().and_then(Approvals::selected),
                    KeyCode::Up | KeyCode::Down => {
                        if let Some(approvals) = self.approvals.as_mut() {
                            approvals.step(key.code == KeyCode::Down);
                        }
                        None
                    }
                    KeyCode::Esc | KeyCode::Tab => {
                        self.close_overlay();
                        None
                    }
                    _ => None,
                };
                if let Some(choice) = choice {
                    self.answer(choice).await;
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
                self.open_own_approvals();
                return Ok(());
            }
            Some(Action::Search) => {
                self.search = Some(Search::new(self.editor.text().to_owned()));
                self.completion = None;
                self.focus = Focus::Search;
                return Ok(());
            }
            Some(Action::External) => {
                self.external_requested = Some(ExternalRequest::Composer);
                return Ok(());
            }
            Some(Action::Models) => {
                self.open_models().await;
                return Ok(());
            }
            Some(Action::Reasoning) => {
                self.cycle_reasoning().await;
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
            Some(Action::Thinking) => {
                let thinking = self.transcript.thinking_display().next();
                self.transcript.set_thinking(thinking);
                self.note(vec![
                    Line::default().dim(format!("  thinking display → {}", thinking.name())),
                ]);
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
        let reasoning = match &info.model_info {
            Some(model) if model.reasoning.is_empty() => "no think".to_owned(),
            Some(model) => format!("think {}", short_reasoning(model.resolve(info.reasoning))),
            None => format!("think {}", short_reasoning(info.reasoning)),
        };
        segs.push(Segment::new("reasoning", d(reasoning)));
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
        if self.focus == Focus::Approval
            && let Some(approvals) = &self.approvals
        {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(approvals.render(cols, self.session_id()));
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
        if self.focus == Focus::Sessions
            && let Some(sessions) = &self.sessions
        {
            lines.extend(sessions.render(cols, overlay_rows, Timestamp::now()));
        }
        let mut overlay_cursor = None;
        if self.focus == Focus::Settings
            && let Some(settings) = &self.settings
        {
            let info = &self.launch.attached.info;
            let (overlay, cursor) = settings.render(
                cols,
                overlay_rows.saturating_sub(lines.len()),
                &info.model,
                info.reasoning,
                self.transcript.thinking_display(),
                self.launch.tui_config.as_deref(),
            );
            overlay_cursor = cursor.map(|(row, col)| (lines.len() + row, col));
            lines.extend(overlay);
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
        let cursor =
            overlay_cursor.or_else(|| (self.focus == Focus::Composer).then_some((top + row, col)));
        (lines, cursor)
    }

    fn draw<W: std::io::Write>(&mut self, screen: &mut Screen<W>) -> std::io::Result<()> {
        for control in self.controls.drain(..) {
            screen.control(&control)?;
        }
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
    use xi_protocol::event::{Remember, StopReason, Usage};
    use xi_protocol::methods::client::{
        ConfigFiles, Delta, DeltaParams, PendingApproval, ResetParams, SessionDetails, Setting,
        SettingLayer, SettingsReport,
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
            reasoning: Reasoning::ALL.to_vec(),
            pricing: None,
        }
    }

    fn settings_report() -> SettingsReport {
        let settings = [
            ("default_model", json!("default/model")),
            ("reasoning", json!("low")),
            ("approvals", json!("ask")),
            (
                "tools",
                json!(["read", "edit", "write", "bash", "search", "hashline"]),
            ),
            ("compaction.enabled", json!(true)),
            ("compaction.trigger_tokens", Value::Null),
            ("compaction.keep_recent_tokens", json!(16_000)),
            ("extensions", json!(["/tmp/extensions"])),
            ("sandbox.readable", json!(["/tmp/notes"])),
            ("sandbox.writable", json!(["/tmp/output"])),
            ("sandbox.env", json!(["LANG"])),
            ("sandbox.env_command", json!(["sh", "-c", "printenv"])),
            ("daemon.idle_exit", json!(900)),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (key, value))| Setting {
            key: key.into(),
            value,
            source: SettingLayer::Default,
            editable: index < 7,
            held_back: None,
        })
        .collect();
        SettingsReport {
            files: ConfigFiles {
                global: "/tmp/xi-test-config/config.toml".into(),
                explicit: Vec::new(),
                project: "/tmp/xi-tui-demo/.xi/config.toml".into(),
                local: "/tmp/xi-tui-demo/.xi/config.local.toml".into(),
            },
            trusted: true,
            settings,
            warnings: Vec::new(),
        }
    }

    #[derive(Default)]
    struct FakeData {
        sessions: Vec<AttachResult>,
        summaries: Vec<SessionSummary>,
        approvals: Vec<PendingApproval>,
        held: bool,
        reject_resolve: bool,
        settings: Option<SettingsReport>,
        reject_settings: bool,
        reject_setting: bool,
        /// Keep answered asks in `approval/list`, as the daemon does until
        /// the resolution event arrives.
        lagging_inbox: bool,
    }

    async fn launch(attached: AttachResult) -> (Launch, mpsc::UnboundedReceiver<(String, Value)>) {
        launch_with(attached, FakeData::default()).await
    }

    async fn launch_with(
        attached: AttachResult,
        mut data: FakeData,
    ) -> (Launch, mpsc::UnboundedReceiver<(String, Value)>) {
        let (connection, server) = tokio::io::duplex(16 * 1024);
        let (reader, writer) = tokio::io::split(server);
        let (peer, mut incoming) = Peer::spawn(reader, writer);
        let (requests, observed) = mpsc::unbounded_channel();
        let response = attached.clone();
        data.sessions.push(response.clone());
        data.approvals.extend(response.pending_approvals.clone());
        let mut settings = data.settings.take().unwrap_or_else(settings_report);
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
                        "session/create" => Ok(serde_json::to_value(&response).unwrap()),
                        "session/attach" => {
                            let session: SessionId =
                                serde_json::from_value(params["session"].clone()).unwrap();
                            let found = data
                                .sessions
                                .iter()
                                .find(|attached| attached.info.session == session)
                                .unwrap_or(&response);
                            Ok(serde_json::to_value(found).unwrap())
                        }
                        "session/open" => {
                            let found = data
                                .sessions
                                .iter()
                                .find(|attached| json!(attached.info.path) == params["path"])
                                .unwrap_or(&response);
                            if data.held {
                                Err(RpcError::with_data(
                                    RpcError::SESSION_HELD,
                                    "session held",
                                    json!({ "session": found.info.session }),
                                ))
                            } else {
                                Ok(serde_json::to_value(found).unwrap())
                            }
                        }
                        "session/list" => Ok(serde_json::to_value(
                            data.summaries
                                .iter()
                                .filter(|row| {
                                    params.get("cwd").is_none_or(|cwd| *cwd == json!(row.cwd))
                                })
                                .collect::<Vec<_>>(),
                        )
                        .unwrap()),
                        "approval/list" => Ok(serde_json::to_value(&data.approvals).unwrap()),
                        "config/settings" if data.reject_settings => Err(RpcError::new(
                            RpcError::INVALID_PARAMS,
                            "settings unavailable",
                        )),
                        "config/settings" => Ok(serde_json::to_value(&settings).unwrap()),
                        "config/set" if data.reject_setting => {
                            Err(RpcError::new(RpcError::INVALID_PARAMS, "setting rejected"))
                        }
                        "config/set" => {
                            let row = settings
                                .settings
                                .iter_mut()
                                .find(|row| json!(row.key) == params["key"])
                                .unwrap();
                            row.value = params["value"].clone();
                            row.source = SettingLayer::Global;
                            Ok(serde_json::to_value(&settings).unwrap())
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
                        "approval/resolve" if data.reject_resolve => {
                            Err(RpcError::new(RpcError::INVALID_PARAMS, "scope rejected"))
                        }
                        "approval/resolve" => {
                            let before = data.approvals.len();
                            let kept: Vec<_> = data
                                .approvals
                                .iter()
                                .filter(|ask| {
                                    json!(ask.session) != params["session"]
                                        || json!(ask.call) != params["call"]
                                })
                                .cloned()
                                .collect();
                            let found = kept.len() < before;
                            if !data.lagging_inbox {
                                data.approvals = kept;
                            }
                            if found {
                                Ok(Value::Null)
                            } else {
                                Err(RpcError::new(
                                    RpcError::APPROVAL_NOT_PENDING,
                                    "already answered",
                                ))
                            }
                        }
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
                tui_config: None,
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
            taint: false,
            trusted: true,
        };
        app.notify(&ClientNotification::ApprovalPending(pending.clone()));
        app.commit.clear();
        app.answer(Choice::Once).await;
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
                    remember: Remember::Once,
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
    async fn shift_tab_cycles_the_models_levels_and_updates_the_footer() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 160, 24);
        assert!(app.footer(160).plain().contains("think med"));
        let model = app.launch.attached.info.model_info.as_mut().unwrap();
        model.reasoning = vec![Reasoning::Low, Reasoning::High, Reasoning::XHigh];
        // Medium isn't listed, so the model runs, and shows, low.
        assert!(app.footer(160).plain().contains("think low"));
        for expected in [Reasoning::High, Reasoning::XHigh, Reasoning::Low] {
            app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
                .await
                .unwrap();
            let params = request(&mut requests, "session/set_reasoning");
            assert_eq!(params["session"], json!(app.session_id()));
            assert_eq!(params["reasoning"], expected.name());
            assert_eq!(app.launch.attached.info.reasoning, expected);
            assert!(
                app.footer(160)
                    .plain()
                    .contains(&format!("think {}", expected.name()))
            );
        }
        app.launch
            .attached
            .info
            .model_info
            .as_mut()
            .unwrap()
            .reasoning
            .clear();
        assert!(app.footer(160).plain().contains("no think"));
        app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn reasoning_command_accepts_xhigh_without_a_transcript_note() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 160, 24);
        app.commit.clear();
        app.command("/reasoning xhigh").await.unwrap();
        assert_eq!(
            request(&mut requests, "session/set_reasoning")["reasoning"],
            "xhigh"
        );
        assert_eq!(app.launch.attached.info.reasoning, Reasoning::XHigh);
        assert!(app.footer(160).plain().contains("think xhigh"));
        assert!(app.commit.is_empty());
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
        app.command("/resume").await.unwrap();
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
        assert!(
            app.commit
                .iter()
                .all(|line| !line.plain().contains("reasoning →"))
        );
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
        for action in Action::ALL {
            for binding in app.keymap.bindings(*action) {
                assert_eq!(
                    app.keymap
                        .action(KeyEvent::new(binding.code, binding.modifiers)),
                    Some(*action)
                );
                assert!(help.contains(&binding.label()));
            }
        }
        for implemented in ["/resume", "/approvals", "/copy", "/settings", "ctrl+t"] {
            assert!(help.contains(implemented));
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

    fn pending(session: SessionId) -> PendingApproval {
        PendingApproval {
            session,
            call: "pending".into(),
            tool: "bash".into(),
            action: "exec git push".into(),
            rule: Some("project#1".into()),
            taint: false,
            trusted: true,
        }
    }

    fn summary(attached: &AttachResult, live: bool, title: &str, updated: u64) -> SessionSummary {
        SessionSummary {
            session: attached.info.session,
            path: attached.info.path.clone(),
            cwd: attached.info.cwd.clone(),
            model: attached.info.model.clone(),
            head: live.then_some(attached.info.head),
            running: attached.running,
            pending_approvals: attached.pending_approvals.len(),
            parent_session: None,
            started: Some(Timestamp(0)),
            updated: Some(Timestamp(updated)),
            title: Some(title.into()),
        }
    }

    fn append(app: &mut App, kind: EventKind) {
        app.notify(&ClientNotification::Event(EventParams {
            session: app.session_id(),
            event: Event {
                id: EventId::generate(),
                parent: Some(app.launch.attached.info.head),
                at: Timestamp::now(),
                kind,
            },
        }));
    }

    fn live_text(app: &App) -> String {
        app.live()
            .0
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn approval_option_two_remembers_the_session_scope() {
        let mut attached = snapshot(true);
        let ask = pending(attached.info.session);
        attached.pending_approvals.push(ask.clone());
        let (launch, mut requests) = launch(attached).await;
        let mut app = App::new(launch, 120, 32);
        let overlay = live_text(&app);
        for label in [
            "1 Allow once",
            "2 Allow for this session",
            "3 Always allow in this workspace",
            "4 Deny",
        ] {
            assert!(overlay.contains(label), "{overlay}");
        }
        app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "approval/resolve");
        assert_eq!(params["session"], json!(ask.session));
        assert_eq!(params["call"], json!(ask.call));
        assert_eq!(params["approved"], true);
        assert_eq!(params["remember"], "session");
        assert!(app.focus == Focus::Composer);
    }

    #[tokio::test]
    async fn taint_approval_hides_persistent_scopes_without_renumbering_deny() {
        let mut attached = snapshot(true);
        let mut ask = pending(attached.info.session);
        ask.taint = true;
        attached.pending_approvals.push(ask);
        let (launch, mut requests) = launch(attached).await;
        let mut app = App::new(launch, 120, 32);
        let overlay = live_text(&app);
        assert!(overlay.contains("1 Allow once"));
        assert!(overlay.contains("4 Deny"));
        assert!(overlay.contains("tainted branch: only once or deny"));
        assert!(!overlay.contains("2 Allow for this session"));
        assert!(!overlay.contains("3 Always allow"));
        for c in ['2', '3'] {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        while let Ok((method, _)) = requests.try_recv() {
            assert_ne!(method, "approval/resolve");
        }
        assert!(app.focus == Focus::Approval);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "approval/resolve");
        assert_eq!(params["approved"], false);
        assert!(params.get("remember").is_none());
    }

    #[tokio::test]
    async fn untrusted_approval_hides_workspace_scope_and_uses_session_details_for_events() {
        let mut attached = snapshot(true);
        attached.info.trusted = false;
        let (launch, mut requests) = launch(attached).await;
        let mut app = App::new(launch, 120, 32);
        append(
            &mut app,
            EventKind::AssistantMessage {
                model: ModelRef {
                    provider: "demo".into(),
                    model: "script".into(),
                },
                content: vec![Block::ToolUse {
                    id: "event-ask".into(),
                    name: "bash".into(),
                    input: json!({ "command": "git push" }),
                    extra: Default::default(),
                }],
                stop: StopReason::ToolUse,
                usage: Usage::default(),
            },
        );
        append(
            &mut app,
            EventKind::ToolDecision {
                call: "event-ask".into(),
                action: "exec git push".into(),
                decision: xi_protocol::event::Decision::Ask,
                rule: None,
                taint: false,
            },
        );
        assert!(app.focus == Focus::Approval);
        assert!(live_text(&app).contains("2 Allow for this session"));
        assert!(!live_text(&app).contains("3 Always allow"));
        app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE))
            .await
            .unwrap();
        while let Ok((method, _)) = requests.try_recv() {
            assert_ne!(method, "approval/resolve");
        }
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.focus == Focus::Composer);
        assert_eq!(app.transcript.asks().len(), 1);
    }

    #[tokio::test]
    async fn workspace_approval_sends_workspace_scope_and_errors_keep_the_overlay() {
        for reject in [false, true] {
            let mut attached = snapshot(true);
            attached
                .pending_approvals
                .push(pending(attached.info.session));
            let (launch, mut requests) = launch_with(
                attached,
                FakeData {
                    reject_resolve: reject,
                    ..FakeData::default()
                },
            )
            .await;
            let mut app = App::new(launch, 120, 32);
            app.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE))
                .await
                .unwrap();
            let params = request(&mut requests, "approval/resolve");
            assert_eq!(params["remember"], "workspace");
            assert_eq!(app.focus == Focus::Approval, reject);
            if reject {
                assert!(
                    app.commit
                        .iter()
                        .any(|line| line.plain().contains("scope rejected"))
                );
            }
        }
    }

    #[tokio::test]
    async fn approvals_inbox_shows_and_resolves_another_sessions_ask() {
        let attached = snapshot(false);
        let mut other = snapshot(true);
        other.info.cwd = "/tmp/other-workspace".into();
        let ask = pending(other.info.session);
        let data = FakeData {
            summaries: vec![summary(&other, true, "other prompt", 100)],
            approvals: vec![ask.clone()],
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(attached, data).await;
        let mut app = App::new(launch, 160, 32);
        app.command("/approvals").await.unwrap();
        request(&mut requests, "approval/list");
        assert!(app.focus == Focus::Approval);
        let overlay = live_text(&app);
        assert!(overlay.contains(&format!("session {}", &other.info.session.to_string()[..8])));
        assert!(overlay.contains("/tmp/other-workspace"));
        assert!(!app.footer(160).plain().contains("waiting"));
        app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "approval/resolve");
        assert_eq!(params["session"], json!(ask.session));
        assert_eq!(params["call"], json!(ask.call));
        assert_eq!(params["remember"], "session");
        request(&mut requests, "approval/list");
        assert!(app.focus == Focus::Composer);
    }

    #[tokio::test]
    async fn an_answered_ask_still_listed_by_the_daemon_does_not_reopen_the_inbox() {
        let attached = snapshot(false);
        let other = snapshot(true);
        let ask = pending(other.info.session);
        let data = FakeData {
            summaries: vec![summary(&other, true, "other prompt", 100)],
            approvals: vec![ask.clone()],
            lagging_inbox: true,
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(attached, data).await;
        let mut app = App::new(launch, 160, 32);
        app.command("/approvals").await.unwrap();
        request(&mut requests, "approval/list");
        app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))
            .await
            .unwrap();
        request(&mut requests, "approval/resolve");
        request(&mut requests, "approval/list");
        assert!(app.focus == Focus::Composer);
        assert!(app.approvals.is_none());
    }

    #[tokio::test]
    async fn approvals_inbox_keeps_current_asks_before_the_daemons_oldest_first_order() {
        let mut attached = snapshot(true);
        let own = pending(attached.info.session);
        attached.pending_approvals.push(own.clone());
        let mut older = pending(SessionId::generate());
        older.call = "older".into();
        let mut newer = pending(SessionId::generate());
        newer.call = "newer".into();
        let data = FakeData {
            approvals: vec![older.clone(), newer.clone()],
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(attached, data).await;
        let mut app = App::new(launch, 120, 32);
        app.command("/approvals").await.unwrap();
        for expected in [own, older, newer] {
            assert_eq!(
                app.approvals
                    .as_ref()
                    .unwrap()
                    .current()
                    .unwrap()
                    .pending
                    .session,
                expected.session
            );
            app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE))
                .await
                .unwrap();
            assert_eq!(
                request(&mut requests, "approval/resolve")["session"],
                json!(expected.session)
            );
        }
        assert!(app.focus == Focus::Composer);
    }

    #[tokio::test]
    async fn resume_lists_the_workspace_toggles_all_and_opens_saved_or_attaches_live_rows() {
        for live in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut attached = snapshot(false);
            attached.info.cwd = temp.path().into();
            let mut target = snapshot(live);
            target.info.cwd = temp.path().into();
            target.info.path = temp.path().join("target.jsonl");
            if live {
                target.pending_approvals.push(pending(target.info.session));
                target.partial = vec![Block::Text {
                    text: "target in flight".into(),
                }];
            }
            let row = summary(&target, live, "target prompt", 200);
            let data = FakeData {
                sessions: vec![target.clone()],
                summaries: vec![row],
                ..FakeData::default()
            };
            let (launch, mut requests) = launch_with(attached, data).await;
            let mut app = App::new(launch, 160, 32);
            app.command("/resume").await.unwrap();
            assert!(app.focus == Focus::Sessions);
            let params = request(&mut requests, "session/list");
            assert_eq!(params["cwd"], json!(temp.path()));
            assert!(params.get("running").is_none());
            app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
                .await
                .unwrap();
            let params = request(&mut requests, "session/list");
            assert!(params.get("cwd").is_none());
            assert!(live_text(&app).contains("all workspaces"));
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            if live {
                let params = request(&mut requests, "session/attach");
                assert_eq!(params["session"], json!(target.info.session));
                assert!(params.get("since").is_none());
                assert!(app.running());
                assert!(app.focus == Focus::Approval);
                assert_eq!(app.transcript.asks().len(), 1);
                assert!(live_text(&app).contains("target in flight"));
            } else {
                let params = request(&mut requests, "session/open");
                assert_eq!(params["path"], json!(target.info.path));
                assert!(!app.running());
                assert!(app.focus == Focus::Composer);
            }
            assert_eq!(app.session_id(), target.info.session);
            assert_eq!(app.launch.template.cwd, target.info.cwd);
            assert!(app.editor.is_empty());
        }
    }

    #[tokio::test]
    async fn saved_session_held_by_a_worker_attaches_and_updates_the_new_session_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let mut target = snapshot(true);
        target.info.cwd = other.clone();
        target.info.path = other.join("saved.jsonl");
        let data = FakeData {
            sessions: vec![target.clone()],
            summaries: vec![summary(&target, false, "held prompt", 200)],
            held: true,
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(snapshot(false), data).await;
        let mut app = App::new(launch, 120, 32);
        app.command("/resume").await.unwrap();
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        request(&mut requests, "session/open");
        assert_eq!(
            request(&mut requests, "session/attach")["session"],
            json!(target.info.session)
        );
        assert_eq!(app.launch.template.cwd, other);
        assert_eq!(app.file_cwd.as_ref(), Some(&target.info.cwd));
        assert!(app.running());
    }

    #[tokio::test]
    async fn resume_hides_children_filters_titles_and_workspaces_and_closes_on_current() {
        let attached = snapshot(false);
        let mut older = snapshot(false);
        older.info.cwd = "/tmp/OTHER-workspace".into();
        let child = snapshot(false);
        let mut child_row = summary(&child, true, "hidden child", 300);
        child_row.parent_session = Some(attached.info.session);
        let data = FakeData {
            summaries: vec![
                summary(&attached, true, "Current prompt", 200),
                summary(&older, false, "Older prompt", 100),
                child_row,
            ],
            ..FakeData::default()
        };
        let (launch, mut requests) = launch_with(attached, data).await;
        let mut app = App::new(launch, 160, 32);
        app.command("/resume").await.unwrap();
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(!live_text(&app).contains("hidden child"));
        assert!(live_text(&app).contains("● Current prompt"));
        for c in "other-workspace".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        assert_eq!(
            app.sessions.as_ref().unwrap().selected().unwrap().session,
            older.info.session
        );
        assert!(!live_text(&app).contains("Current prompt"));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        app.command("/resume").await.unwrap();
        while requests.try_recv().is_ok() {}
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.focus == Focus::Composer);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn custom_keymap_changes_dispatch_help_and_initial_thinking() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.toml");
        std::fs::write(
            &path,
            "[keys]\nsearch = \"ctrl+y\"\n[display]\nthinking = \"full\"\n",
        )
        .unwrap();
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.tui_config = Some(path);
        let mut app = App::new(launch, 120, 32);
        assert_eq!(
            app.transcript.thinking_display(),
            transcript::Thinking::Full
        );
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.focus == Focus::Composer);
        app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.focus == Focus::Search);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        app.commit.clear();
        app.command("/help").await.unwrap();
        let help = app
            .commit
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(help.contains("ctrl+y"));
        assert!(!help.contains("ctrl+r"));
    }

    #[tokio::test]
    async fn config_warnings_are_dim_notes_and_ctrl_t_cycles_thinking_display() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.toml");
        std::fs::write(&path, "[keys]\nunknown_action = \"ctrl+y\"\n").unwrap();
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.tui_config = Some(path);
        let mut app = App::new(launch, 120, 32);
        assert_eq!(
            app.commit
                .iter()
                .filter(|line| line.plain().contains("tui.toml:"))
                .count(),
            1
        );
        assert_eq!(
            app.transcript.thinking_display(),
            transcript::Thinking::First
        );
        for expected in [
            transcript::Thinking::Hidden,
            transcript::Thinking::Full,
            transcript::Thinking::First,
        ] {
            app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .await
                .unwrap();
            assert_eq!(app.transcript.thinking_display(), expected);
            assert!(app.commit.iter().any(|line| {
                line.plain()
                    .contains(&format!("thinking display → {}", expected.name()))
            }));
        }
    }

    #[tokio::test]
    async fn copy_queues_osc52_until_the_next_draw_on_the_frame_writer() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 120, 32);
        app.command("/copy").await.unwrap();
        assert!(app.controls.is_empty());
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("nothing to copy yet"))
        );
        append(
            &mut app,
            EventKind::AssistantMessage {
                model: ModelRef {
                    provider: "demo".into(),
                    model: "script".into(),
                },
                content: vec![Block::Text {
                    text: "  hello\nworld  ".into(),
                }],
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            },
        );
        app.command("/copy").await.unwrap();
        let sequence = "\x1b]52;c;aGVsbG8Kd29ybGQ=\x07";
        assert_eq!(app.controls, vec![sequence]);
        assert!(
            app.commit
                .iter()
                .any(|line| line.plain().contains("copied 2 lines"))
        );
        let mut output = Vec::new();
        {
            let mut screen = Screen::new(&mut output, false, 120);
            app.draw(&mut screen).unwrap();
        }
        assert!(output.starts_with(sequence.as_bytes()));
        assert!(app.controls.is_empty());
        assert!(String::from_utf8(output).unwrap().contains("\x1b[?2026h"));
    }

    async fn settings_down(app: &mut App, count: usize) {
        for _ in 0..count {
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn settings_opens_with_report_rows_and_session_model_and_reasoning() {
        let mut report = settings_report();
        report.trusted = false;
        report.settings[2].source = SettingLayer::Explicit {
            path: "/tmp/extra.toml".into(),
        };
        report.settings[2].held_back = Some(SettingLayer::Local);
        let (mut launch, mut requests) = launch_with(
            snapshot(false),
            FakeData {
                settings: Some(report),
                ..FakeData::default()
            },
        )
        .await;
        launch.template.options.configs = vec!["/tmp/extra.toml".into()];
        let mut app = App::new(launch, 240, 40);
        app.command("/settings").await.unwrap();
        assert!(app.focus == Focus::Settings);
        let params = request(&mut requests, "config/settings");
        assert_eq!(params["cwd"], "/tmp/xi-tui-demo");
        assert_eq!(params["configs"], json!(["/tmp/extra.toml"]));
        let text = live_text(&app);
        for expected in [
            "settings",
            "saved to /tmp/xi-test-config/config.toml",
            "Model",
            "demo/script",
            "Reasoning",
            "medium",
            "session",
            "Thinking display",
            "Approvals",
            "--config extra.toml",
            "held back (untrusted)",
            "Tool read",
            "Tool hashline",
            "Automatic compaction",
            "80% of window",
            "Kept tail",
            "Sandbox readable",
            "/tmp/notes",
            "Sandbox writable",
            "/tmp/output",
            "Sandbox env",
            "LANG",
            "Sandbox env command",
            "sh, -c, printenv",
            "Extensions",
            "/tmp/extensions",
            "Trust",
            "untrusted · xi trust",
            "Daemon idle exit",
            "900 seconds",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("default/model"));
        assert!(app.commands.iter().any(|command| {
            command.name == "settings" && command.description == "show and change settings"
        }));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        app.commit.clear();
        app.command("/help").await.unwrap();
        assert!(app.commit.iter().any(|line| {
            line.plain().contains("/settings") && line.plain().contains("show and change settings")
        }));
    }

    #[tokio::test]
    async fn settings_compaction_toggle_sends_set_and_replaces_the_report() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        request(&mut requests, "config/settings");
        settings_down(&mut app, 10).await;
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["cwd"], "/tmp/xi-tui-demo");
        assert_eq!(params["key"], "compaction.enabled");
        assert_eq!(params["value"], false);
        let row = app
            .settings
            .as_ref()
            .unwrap()
            .report
            .settings
            .iter()
            .find(|row| row.key == "compaction.enabled")
            .unwrap();
        assert_eq!(row.value, false);
        assert_eq!(row.source, SettingLayer::Global);
        assert!(live_text(&app).contains("saved · new sessions use it"));
    }

    #[tokio::test]
    async fn settings_saves_session_model_and_reasoning_and_returns_from_picker() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "default_model");
        assert_eq!(params["value"], "demo/script");
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.focus == Focus::Models);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.focus == Focus::Settings);
        assert_eq!(app.launch.attached.info.model.model, "second");
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(request(&mut requests, "config/set")["value"], "demo/second");
        settings_down(&mut app, 1).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "session/set_reasoning")["reasoning"],
            "high"
        );
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "reasoning");
        assert_eq!(params["value"], "high");
        assert_eq!(app.launch.attached.info.reasoning, Reasoning::High);
    }

    #[tokio::test]
    async fn settings_tool_toggle_sends_full_array_preserving_unknown_names_and_order() {
        let mut report = settings_report();
        report.settings[3].value = json!(["read", "custom", "edit"]);
        let (launch, mut requests) = launch_with(
            snapshot(false),
            FakeData {
                settings: Some(report),
                ..FakeData::default()
            },
        )
        .await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        settings_down(&mut app, 4).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "tools");
        assert_eq!(params["value"], json!(["custom", "edit"]));
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            request(&mut requests, "config/set")["value"],
            json!(["custom", "edit", "read"])
        );
    }

    #[tokio::test]
    async fn settings_refuses_to_remove_the_last_tool() {
        let mut report = settings_report();
        report.settings[3].value = json!(["read"]);
        let (launch, mut requests) = launch_with(
            snapshot(false),
            FakeData {
                settings: Some(report),
                ..FakeData::default()
            },
        )
        .await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        request(&mut requests, "config/settings");
        settings_down(&mut app, 4).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(live_text(&app).contains("cannot remove the last tool"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn settings_cycles_approvals_and_edits_nullable_and_positive_token_counts() {
        let (launch, mut requests) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        settings_down(&mut app, 3).await;
        for approval in ["reads", "auto", "ask"] {
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .await
                .unwrap();
            let params = request(&mut requests, "config/set");
            assert_eq!(params["key"], "approvals");
            assert_eq!(params["value"], approval);
        }
        settings_down(&mut app, 8).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(app.live().1.is_some());
        for c in "24000".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "compaction.trigger_tokens");
        assert_eq!(params["value"], 24_000);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        for _ in 0..5 {
            app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(request(&mut requests, "config/set")["value"].is_null());
        assert!(live_text(&app).contains("80% of window"));
        settings_down(&mut app, 1).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        for _ in 0..5 {
            app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(live_text(&app).contains("enter a positive token count"));
        assert!(requests.try_recv().is_err());
        for c in "8000".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        let params = request(&mut requests, "config/set");
        assert_eq!(params["key"], "compaction.keep_recent_tokens");
        assert_eq!(params["value"], 8_000);
    }

    #[tokio::test]
    async fn settings_thinking_toggle_persists_but_live_toggle_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tui.toml");
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.tui_config = Some(path.clone());
        let mut app = App::new(launch, 180, 40);
        app.command("/settings").await.unwrap();
        settings_down(&mut app, 2).await;
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            app.transcript.thinking_display(),
            transcript::Thinking::Hidden
        );
        assert_eq!(
            TuiConfig::load(Some(&path)).0.thinking,
            transcript::Thinking::Hidden
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(
            app.transcript.thinking_display(),
            transcript::Thinking::Full
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    }

    #[tokio::test]
    async fn settings_handles_missing_client_path_external_file_and_ctrl_c() {
        let (launch, _) = launch(snapshot(false)).await;
        let mut app = App::new(launch, 180, 40);
        app.editor.set_text("keep draft");
        app.command("/settings").await.unwrap();
        settings_down(&mut app, 2).await;
        app.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .await
            .unwrap();
        assert_eq!(
            app.transcript.thinking_display(),
            transcript::Thinking::Hidden
        );
        assert!(live_text(&app).contains("not saved: no config path"));
        app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            matches!(&app.external_requested, Some(ExternalRequest::Settings(path))
            if path == Path::new("/tmp/xi-test-config/config.toml"))
        );
        app.external_requested = None;
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(app.focus == Focus::Composer);
        assert!(!app.quit);
        assert!(app.settings.is_none());
        assert_eq!(app.editor.text(), "keep draft");
    }

    #[tokio::test]
    async fn settings_errors_leave_the_composer_or_show_the_rpc_message_in_the_view() {
        for reject_open in [true, false] {
            let (launch, mut requests) = launch_with(
                snapshot(false),
                FakeData {
                    reject_settings: reject_open,
                    reject_setting: !reject_open,
                    ..FakeData::default()
                },
            )
            .await;
            let mut app = App::new(launch, 180, 40);
            app.command("/settings").await.unwrap();
            if reject_open {
                assert!(app.focus == Focus::Composer);
                assert!(app.settings.is_none());
                assert!(
                    app.commit
                        .iter()
                        .any(|line| line.plain().contains("settings unavailable"))
                );
            } else {
                app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
                    .await
                    .unwrap();
                request(&mut requests, "config/set");
                assert!(app.focus == Focus::Settings);
                assert!(live_text(&app).contains("setting rejected"));
                assert!(!live_text(&app).contains("saved · new sessions use it"));
            }
        }
    }

    #[tokio::test]
    async fn startup_resume_uses_the_attached_workspace_for_new_sessions() {
        let (mut launch, _) = launch(snapshot(false)).await;
        launch.template.cwd = "/tmp/original-workspace".into();
        let app = App::new(launch, 120, 32);
        assert_eq!(app.launch.template.cwd, app.launch.attached.info.cwd);
    }
}
