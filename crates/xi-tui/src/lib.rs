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
pub mod callout;
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
pub mod surface;
pub mod transcript;
pub mod tree;
mod view;

use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyEventKind, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use crate::callout::Kind;
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
use crate::style::Line;
use crate::transcript::{Transcript, duration, tokens};

/// A failure running the terminal client.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A terminal or local file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A daemon request failed.
    #[error(transparent)]
    Client(#[from] ClientError),
}

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
struct Raw {
    mouse: bool,
}

impl Raw {
    fn enter(mouse: bool) -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut raw = Self { mouse: false };
        // Modified enter is indistinguishable from enter in legacy input.
        // Terminals without the keyboard protocol ignore this request.
        crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableBracketedPaste
        )?;
        raw.set_mouse(mouse)?;
        Ok(raw)
    }

    fn set_mouse(&mut self, enabled: bool) -> std::io::Result<()> {
        if enabled != self.mouse {
            self.mouse = enabled;
            if enabled {
                crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
            } else {
                crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;
            }
        }
        Ok(())
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.set_mouse(false);
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
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

/// The client routes between transport and its focused components.
struct App {
    launch: Launch,
    git: git::Status,
    transcript: Transcript,
    overlays: Overlays,
    keymap: Keymap,
    display: Display,
    turn: Turn,
    composer: Composer,
    approvals: ApprovalBook,
    quit: bool,
}

/// Terminal geometry, retained output and display preferences.
struct Display {
    cols: usize,
    rows: usize,
    frame: usize,
    controls: Vec<String>,
    clear_scrollback: bool,
    layout: Layout,
    icons: icons::Icons,
    surfaces: surface::Surfaces,
    deck: Option<Deck>,
    /// The session's latest title, from its log.
    title: Option<String>,
    /// The first prompt line, used by the deck until a title arrives.
    prompt: Option<String>,
    title_written: bool,
    commit: Vec<Line>,
}

/// A running turn and its event and reply bookkeeping.
#[derive(Default)]
struct Turn {
    pending: FuturesUnordered<Reply>,
    started: Option<Instant>,
    /// Sent during a turn, shown until the worker takes them in.
    steering: Vec<Vec<Block>>,
    /// Held until the turn ends, then sent in order.
    queued: Vec<Vec<Block>>,
    failed: bool,
    exited: bool,
    /// Every event in the snapshot or a subsequent notification.
    seen: HashSet<EventId>,
    /// Calls already resolved in any recorded branch.
    resolved: HashSet<ToolCallId>,
    refresh: bool,
}

/// The draft, its history and the completion sources it uses.
struct Composer {
    editor: Editor,
    /// Images attached to this draft, not prompt history.
    images: Vec<Block>,
    history: History,
    completion: Option<Menu>,
    files: Option<FileIndex>,
    file_cwd: Option<PathBuf>,
    commands: Vec<CommandSpec>,
    external_requested: Option<ExternalRequest>,
}

/// Cross-session approval bookkeeping.
#[derive(Default)]
struct ApprovalBook {
    approval_workspaces: HashMap<SessionId, PathBuf>,
    /// Answered asks can remain listed until their resolution event arrives.
    answered: HashSet<(SessionId, ToolCallId)>,
    inbox_refresh: bool,
}

/// Runs the client until the user quits.
///
/// # Errors
///
/// Terminal I/O failures and failures creating a session.
pub async fn run(launch: Launch) -> Result<(), Error> {
    let (cols, rows) = crossterm::terminal::size()?;
    let mut app = App::new(launch, usize::from(cols), usize::from(rows));
    app.initialize().await;
    let mut terminal = Terminal::enter(cols, &app.display)?;
    while !app.quit {
        app.drain_requests(&mut terminal).await?;
        app.refresh().await;
        if terminal.prepare(&mut app.display)? {
            app.draw(&mut terminal.screen)?;
        }
        app.select(&mut terminal).await?;
    }
    app.leave_deck();
    terminal.finish(&mut app.display)?;
    app.release().await?;
    Ok(())
}

/// The terminal resources suspended together for an external editor.
struct Terminal {
    screen: Screen<std::io::Stdout>,
    keys: Option<EventStream>,
    raw: Option<Raw>,
    settle: Settle,
    on_deck: bool,
    tick: tokio::time::Interval,
    git_tick: tokio::time::Interval,
    _title: TitleStack,
}

impl Terminal {
    fn enter(cols: u16, display: &Display) -> std::io::Result<Self> {
        let title = TitleStack::push()?;
        let raw = Raw::enter(false)?;
        let color = std::env::var_os("NO_COLOR").is_none();
        let mut screen = Screen::new(std::io::stdout(), color, usize::from(cols));
        // Query before the key reader can consume the terminal's answer.
        if color && display.surfaces == surface::Surfaces::Auto {
            screen.set_surface(surface::query());
        }
        Ok(Self {
            screen,
            keys: Some(EventStream::new()),
            raw: Some(raw),
            settle: Settle::new(cols),
            on_deck: false,
            tick: tokio::time::interval(Duration::from_millis(125)),
            git_tick: tokio::time::interval(Duration::from_secs(1)),
            _title: title,
        })
    }

    fn edit(
        &mut self,
        composer: &mut Composer,
        display: &mut Display,
        request: ExternalRequest,
    ) -> std::io::Result<ExternalEdit> {
        self.screen.finish()?;
        drop(self.keys.take());
        drop(self.raw.take());
        let settings = matches!(&request, ExternalRequest::Settings(_));
        let result = composer.edit_external(request);
        self.raw = Some(Raw::enter(self.on_deck)?);
        // The editor may have set its own title.
        display.title_written = false;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::MoveToColumn(0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown)
        )?;
        self.keys = Some(EventStream::new());
        Ok(ExternalEdit { settings, result })
    }

    fn sync_deck(&mut self, deck: bool) -> std::io::Result<()> {
        if deck != self.on_deck {
            if deck {
                self.screen.finish()?;
                self.screen.enter_deck()?;
            } else {
                self.screen.leave_deck()?;
            }
            self.raw
                .as_mut()
                .expect("terminal input resumed")
                .set_mouse(deck)?;
            self.on_deck = deck;
        }
        Ok(())
    }

    fn prepare(&mut self, display: &mut Display) -> std::io::Result<bool> {
        self.sync_deck(display.deck.is_some())?;
        Ok(display.settle(&mut self.screen, &mut self.settle))
    }

    fn finish(&mut self, display: &mut Display) -> std::io::Result<()> {
        if let Ok((cols, _)) = crossterm::terminal::size() {
            self.screen.resize(usize::from(cols));
        }
        // Leaving the deck prints only the log rows scrollback still lacks.
        self.sync_deck(false)?;
        self.screen.draw(
            &std::mem::take(&mut display.commit),
            &[],
            None,
            display.rows,
        )?;
        self.screen.finish()?;
        drop(self.raw.take());
        Ok(())
    }
}

struct ExternalEdit {
    settings: bool,
    result: std::io::Result<()>,
}

impl Display {
    fn new(config: &TuiConfig, cols: usize, rows: usize) -> Self {
        Self {
            cols: cols.max(20),
            rows: rows.max(4),
            frame: 0,
            controls: Vec::new(),
            clear_scrollback: false,
            layout: config.layout,
            icons: config.icons,
            surfaces: config.surfaces,
            deck: config.layout.deck_at(cols, rows).then(Deck::default),
            title: None,
            prompt: None,
            title_written: false,
            commit: Vec::new(),
        }
    }

    fn settle<W: std::io::Write>(&mut self, screen: &mut Screen<W>, settle: &mut Settle) -> bool {
        // Resize events only wake the loop. Queued events may already be stale.
        // Inline frames wait for a stable width to avoid rewinding too few rows.
        // The deck redraws whole, so it need not wait.
        let Ok((cols, rows)) = crossterm::terminal::size() else {
            return true;
        };
        self.cols = usize::from(cols).max(20);
        self.rows = usize::from(rows).max(4);
        if settle.ready(cols, Instant::now()) || self.deck.is_some() {
            screen.resize(usize::from(cols));
            true
        } else {
            false
        }
    }

    /// The terminal inline, or the log column on the deck.
    fn measure(&self) -> Measure {
        if self.deck.is_some() {
            self.deck_measure()
        } else {
            Measure::new(self.cols, self.rows)
        }
    }

    fn controls<W: std::io::Write>(&mut self, screen: &mut Screen<W>) -> std::io::Result<()> {
        if std::mem::take(&mut self.clear_scrollback) {
            screen.clear();
        }
        for control in self.controls.drain(..) {
            screen.control(&control)?;
        }
        if !self.title_written {
            screen.control(&title_control(self.title.as_deref()))?;
            self.title_written = true;
        }
        Ok(())
    }
}

impl Turn {
    fn running(&self) -> bool {
        self.started.is_some()
    }

    fn exit(&mut self) {
        self.exited = true;
        self.started = None;
        self.steering.clear();
        self.queued.clear();
    }

    fn elapsed(&mut self) -> u64 {
        self.started.take().map_or(0, |started| {
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }
}

impl Composer {
    fn new(history: History) -> Self {
        let mut editor = Editor::default();
        editor.set_history(history.workspace());
        Self {
            editor,
            images: Vec::new(),
            history,
            completion: None,
            files: None,
            file_cwd: None,
            commands: builtin_commands(),
            external_requested: None,
        }
    }

    async fn load_commands(
        &mut self,
        client: &Client,
        session: SessionId,
    ) -> Result<(), ClientError> {
        self.commands = builtin_commands();
        for command in client.commands(session).await? {
            if !self.commands.iter().any(|known| known.name == command.name) {
                self.commands.push(command);
            }
        }
        Ok(())
    }

    async fn index_files(&mut self, cwd: &std::path::Path) -> Result<(), tokio::task::JoinError> {
        if self.file_cwd.as_deref() != Some(cwd) {
            let root = cwd.to_owned();
            self.files =
                Some(tokio::task::spawn_blocking(move || FileIndex::build(&root, 20_000)).await?);
            self.file_cwd = Some(cwd.to_owned());
        }
        Ok(())
    }

    fn remember(&mut self, text: &str) -> std::io::Result<()> {
        let saved = self.history.push(text);
        self.editor.set_history(self.history.workspace());
        saved
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

    fn edit_external(&mut self, request: ExternalRequest) -> std::io::Result<()> {
        match request {
            ExternalRequest::Composer => {
                let edited = external::edit(self.editor.text());
                if let Ok(Some(text)) = &edited {
                    self.editor.set_text(text);
                }
                self.update_completion();
                edited.map(|_| ())
            }
            ExternalRequest::Settings(path) => external::edit_file(&path),
        }
    }

    fn paste(&mut self, text: &str) {
        self.editor.insert(text);
        self.update_completion();
    }
}

impl ApprovalBook {
    fn close(&mut self) {
        self.inbox_refresh = false;
    }
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

/// The first non-blank line of the first user message in `events`, cut to
/// 80 characters, as `session/list` titles a session before it is titled.
fn first_prompt(events: &[Event]) -> Option<String> {
    events.iter().find_map(|event| match &event.kind {
        xi_protocol::EventKind::UserMessage { content } => prompt_line(content),
        _ => None,
    })
}

/// The first non-blank typed line of `content`, trimmed and cut to 80
/// characters. Injected skill instructions are skipped.
fn prompt_line(content: &[Block]) -> Option<String> {
    let text = crate::transcript::blocks_text(xi_protocol::skill::typed(content));
    let first = text.lines().find(|line| !line.trim().is_empty())?;
    let mut chars = first.trim().chars();
    let mut line: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        line.push('…');
    }
    Some(line)
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
        let mut app = Self {
            launch,
            git: git::Status::default(),
            transcript,
            composer: Composer::new(history),
            overlays: Overlays::default(),
            display: Display::new(&config, cols, rows),
            turn: Turn::default(),
            approvals: ApprovalBook::default(),
            keymap: config.keymap,
            quit: false,
        };
        app.attach();
        for warning in warnings {
            app.notice(Kind::Warn, format!("tui.toml: {warning}"));
        }
        if let Some(error) = history_error {
            app.notice(Kind::Warn, format!("prompt history: {error}"));
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

    async fn drain_requests(&mut self, terminal: &mut Terminal) -> Result<(), Error> {
        while let Some(request) = self.composer.external_requested.take() {
            let edited = terminal.edit(&mut self.composer, &mut self.display, request)?;
            if edited.settings {
                self.refresh_settings().await;
                if let Err(error) = edited.result {
                    self.settings_note(Kind::Error, format!("external editor: {error}"));
                }
            } else if let Err(error) = edited.result {
                self.notice(Kind::Error, format!("external editor: {error}"));
            }
        }
        Ok(())
    }

    async fn refresh(&mut self) {
        if self.turn.refresh {
            self.refresh_details().await;
        }
        if self.approvals.inbox_refresh {
            self.refresh_inbox().await;
        }
        self.git.refresh(&self.launch.attached.info.cwd).await;
        self.refresh_deck().await;
    }

    async fn select(&mut self, terminal: &mut Terminal) -> Result<(), Error> {
        let settled = terminal
            .settle
            .deadline()
            .map(tokio::time::Instant::from_std);
        let keys = terminal.keys.as_mut().expect("terminal input resumed");
        tokio::select! {
            // The worker sends a turn's last events before its reply.
            biased;
            notification = self.launch.notifications.recv() => match notification {
                Some(notification) => {
                    self.notify(&notification);
                    while let Ok(notification) = self.launch.notifications.try_recv() {
                        self.notify(&notification);
                    }
                }
                None => {
                    self.notice(Kind::Error, "daemon connection closed");
                    self.quit = true;
                }
            },
            Some(reply) = self.turn.pending.next(), if !self.turn.pending.is_empty() => self.replied(reply),
            event = keys.next() => match event {
                Some(Ok(TermEvent::Key(key))) if key.kind != KeyEventKind::Release => self.key(key).await?,
                Some(Ok(TermEvent::Paste(text))) if self.overlays.is_empty() => self.composer.paste(&text),
                Some(Ok(TermEvent::Mouse(mouse))) if self.overlays.is_empty() => self.display.deck_mouse(mouse),
                // Query the current size before drawing, not a queued resize.
                Some(Ok(TermEvent::Resize(..))) => {}
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => self.quit = true,
            },
            () = tokio::time::sleep_until(settled.unwrap_or_else(tokio::time::Instant::now)), if settled.is_some() => {}
            _ = terminal.git_tick.tick() => {},
            _ = terminal.tick.tick(), if self.turn.running() => {
                self.display.frame += 1;
                if self.turn.pending.is_empty() && self.display.frame.is_multiple_of(8) {
                    self.refresh_details().await;
                }
            },
        }
        Ok(())
    }

    async fn initialize_composer(&mut self) {
        let session = self.session_id();
        if let Err(error) = self
            .composer
            .load_commands(&self.launch.client, session)
            .await
        {
            self.notice(Kind::Error, format!("command completion: {error}"));
        }
        if let Err(error) = self
            .composer
            .index_files(&self.launch.attached.info.cwd)
            .await
        {
            self.notice(Kind::Error, format!("file completion: {error}"));
        }
        self.composer.update_completion();
    }

    /// Closes every overlay and returns the keys to the composer.
    fn close_overlay(&mut self) {
        self.overlays.clear();
        self.approvals.close();
        self.composer.completion = None;
    }

    fn idle_only(&mut self, action: &str) -> bool {
        if !self.turn.running() {
            return true;
        }
        self.notice(
            Kind::Warn,
            Line::default()
                .ink(format!("{action} waits for the turn to end"))
                .dim(" · esc stops it"),
        );
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
        self.turn.refresh = false;
        if self.turn.exited {
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
                self.turn.refresh = false;
                if !attached.running && self.turn.pending.is_empty() && self.turn.started.is_some()
                {
                    self.finish_turn(None);
                }
            }
            Err(error) => self.notice(Kind::Error, error.to_string()),
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
                let cwd = self
                    .approvals
                    .approval_workspaces
                    .get(&pending.session)
                    .cloned();
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
                    self.approvals.inbox_refresh = true;
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
            && self.turn.resolved.contains(&pending.call)
        {
            return;
        }
        if let ClientNotification::Event(p) = notification {
            if !self.turn.seen.insert(p.event.id) {
                return;
            }
            self.launch.attached.info.head = p.event.id;
            self.launch.attached.events.push(p.event.clone());
            match &p.event.kind {
                xi_protocol::EventKind::ApprovalResolved { call, .. }
                | xi_protocol::EventKind::ToolResult { call, .. } => {
                    self.turn.resolved.insert(call.clone());
                }
                _ => {}
            }
        }
        let asks = self.transcript.asks().len();
        let lines = self
            .transcript
            .apply_live(notification, self.display.measure());
        self.commit_lines(lines);
        match notification {
            ClientNotification::Event(p) => match &p.event.kind {
                xi_protocol::EventKind::UserMessage { content } => {
                    if self.display.prompt.is_none() {
                        self.display.prompt = prompt_line(content);
                    }
                    if self.turn.started.is_none() {
                        self.turn.started = Some(Instant::now());
                        self.transcript.begin_turn();
                    }
                    self.launch.attached.running = true;
                    let content = xi_protocol::skill::typed(content);
                    if let Some(i) = self.turn.steering.iter().position(|s| s == content) {
                        self.turn.steering.remove(i);
                    }
                }
                xi_protocol::EventKind::ModelChanged { model } => {
                    self.launch.attached.info.model = model.clone();
                    self.launch.attached.info.model_info = None;
                    self.turn.refresh = true;
                }
                xi_protocol::EventKind::ReasoningChanged { reasoning } => {
                    self.launch.attached.info.reasoning = *reasoning;
                }
                xi_protocol::EventKind::SandboxChanged { enabled } => {
                    self.launch.attached.info.sandbox_enabled = *enabled;
                }
                // The deck lists titles from `session/list`; fetch it now.
                xi_protocol::EventKind::SessionTitled { title, .. } => {
                    self.display.title = Some(title.clone());
                    self.display.title_written = false;
                    if let Some(deck) = self.display.deck.as_mut() {
                        deck.refreshed = None;
                    }
                }
                _ => {}
            },
            ClientNotification::Exited(_) => {
                self.turn.exit();
                self.launch.attached.running = false;
            }
            _ => {}
        }
        // A new ask takes the keys only when nothing else has them;
        // otherwise it waits on the status line for `tab`.
        let now = self.transcript.asks().len();
        if now > asks
            && self.composer.editor.is_empty()
            && self.composer.images.is_empty()
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
                if !self.turn.seen.contains(&outcome.head) {
                    self.launch.attached.info.head = outcome.head;
                }
                Some(outcome)
            }
            Ok(Err(error)) => {
                self.notice(Kind::Error, error.to_string());
                self.turn.failed = true;
                None
            }
            Err(_) => None,
        };
        if self.turn.pending.is_empty() {
            self.finish_turn(outcome);
        }
    }

    fn finish_turn(&mut self, outcome: Option<PromptResult>) {
        let lines = self.transcript.flush(self.display.measure());
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
        let elapsed = self.turn.elapsed();
        let failed = std::mem::take(&mut self.turn.failed);
        let (word, kind) = match &outcome {
            Some(o) if o.cancelled => ("stopped", Kind::Warn),
            Some(o) if o.failed => ("failed", Kind::Error),
            None if failed => ("failed", Kind::Error),
            _ => ("done", Kind::Done),
        };
        let turn = self.transcript.turn;
        let mut parts = vec![duration(elapsed)];
        if turn.sent() > 0 {
            parts.push(format!(
                "↑ {}  ↓ {}",
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
        self.notice(
            kind,
            Line::default()
                .ink(word)
                .dim(format!("  {}", parts.join("  "))),
        );
        self.turn.steering.clear();
        if !self.turn.exited && !self.turn.queued.is_empty() {
            let next = self.turn.queued.remove(0);
            self.send(next);
        }
    }

    fn send(&mut self, content: Vec<Block>) {
        if self.turn.exited {
            self.notice(
                Kind::Warn,
                Line::default()
                    .ink("session exited")
                    .dim(" · /new starts another"),
            );
            return;
        }
        if self.turn.started.is_none() {
            self.turn.started = Some(Instant::now());
            self.transcript.begin_turn();
        } else {
            self.turn.steering.push(content.clone());
        }
        self.transcript.sent(&content);
        self.launch.attached.running = true;
        let client = self.launch.client.clone();
        let session = self.session_id();
        let (send, reply) = oneshot::channel();
        tokio::spawn(async move {
            let _ = send.send(client.prompt(session, content).await);
        });
        self.turn.pending.push(reply);
    }

    /// Commits rows to the client's log: scrollback inline, the retained
    /// log on the deck.
    fn commit_lines(&mut self, lines: Vec<Line>) {
        let anchors = self.transcript.take_inspects();
        match self.display.deck.as_mut() {
            Some(deck) => deck.log.extend(lines, anchors),
            None => self.display.commit.extend(lines),
        }
    }

    fn note(&mut self, lines: Vec<Line>) {
        let lines = self.transcript.note(lines);
        self.commit_lines(lines);
    }

    /// Commits a marked notice, such as a setting change or an error.
    fn notice(&mut self, kind: Kind, text: impl Into<Line>) {
        let lines = self.transcript.notice(kind, text, self.display.measure());
        self.commit_lines(lines);
    }

    async fn cancel(&mut self) {
        if let Err(error) = self.launch.client.cancel(self.session_id()).await {
            self.notice(Kind::Error, error.to_string());
        }
    }

    async fn release(&self) -> Result<(), ClientError> {
        if self.turn.exited {
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
        match tokio::task::spawn_blocking(clipboard::image).await {
            Ok(Ok(image)) => {
                self.composer.images.push(image);
                self.composer.completion = None;
            }
            Ok(Err(error)) => self.notice(Kind::Warn, format!("paste image: {error}")),
            Err(error) => self.notice(Kind::Warn, format!("paste image: {error}")),
        }
    }

    async fn submit(&mut self, queue: bool) -> Result<(), Error> {
        let text = self.composer.editor.text().trim().to_owned();
        self.composer.editor.clear();
        self.composer.completion = None;
        if text.starts_with('/')
            && !text.contains('\n')
            && xi_protocol::skill::invocation(&text).is_none()
        {
            // Commands leave attachments in the draft unless they switch sessions.
            return self.command(&text).await;
        }
        if text.is_empty() && self.composer.images.is_empty() {
            return Ok(());
        }
        if self.turn.exited {
            self.notice(
                Kind::Warn,
                Line::default()
                    .ink("session exited")
                    .dim(" · /new starts another"),
            );
            self.composer.editor.set_text(&text);
            self.composer.update_completion();
            return Ok(());
        }
        if !text.is_empty()
            && let Err(error) = self.composer.remember(&text)
        {
            self.notice(Kind::Warn, format!("prompt history: {error}"));
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(Block::Text { text });
        }
        content.append(&mut self.composer.images);
        if queue && self.turn.running() {
            self.turn.queued.push(content);
        } else {
            self.send(content);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod error_tests {
    use super::Error;
    use xi_client::client::ClientError;

    #[test]
    fn errors_preserve_the_terminal_or_client_failure() {
        let io = Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(matches!(io, Error::Io(error) if error.kind() == std::io::ErrorKind::BrokenPipe));
        let client = Error::from(ClientError::Closed);
        assert!(matches!(client, Error::Client(ClientError::Closed)));
    }
}
