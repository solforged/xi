//! The xi agent loop, stable context, layered configuration and client server.
//!
//! Configuration uses `default_model = "provider/model"` for selection and
//! `[[model]]` for registry metadata. Global and explicit files have full
//! authority. Project policy only tightens; project settings and local
//! authority require an exact canonical root in `$XDG_CONFIG_HOME/xi/trust.toml`.
//!
//! Each open session owns one durable tree log and one cached system prompt.
//! Concurrent prompts steer the current turn only at model boundaries, after
//! all tool results have been persisted. Cancellation retains unconsumed
//! steering for the next prompt. Transport closure explicitly rejects it.

pub mod compaction;
pub mod config;
pub mod context;
mod egress;
mod extension_session;
pub mod extensions;
pub mod settings;
mod task;
pub mod title;
pub mod worker;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xi_policy::{Action, ActionKind, Taint};
use xi_protocol::event::{Approver, ModelRef, Remember, StopReason, Usage};
use xi_protocol::methods::client::{Delta, DeltaParams, EventParams, ResetParams};
use xi_protocol::model::{ModelInfo, ModelSelector, Reasoning};
use xi_protocol::{
    Block, Decision, EventId, EventKind, OutputBlock, SessionId, ToolCallId, ToolOutput,
};
use xi_provider::{Provider, Request, Response};
use xi_session::SessionLog;
use xi_tools::{Crossing, Tool};

pub use config::Approvals;
use config::Config;
use context::SystemContext;

/// Kernel and configuration failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid settings or client operation.
    #[error("{0}")]
    Config(String),
    /// A turn is running.
    #[error("session is running")]
    Running,
    /// No ask is pending for that call.
    #[error("no pending approval for that call")]
    NotPending,
    /// A requested approval scope is not valid for this ask.
    #[error("{0}")]
    InvalidRemember(String),
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Session durability or tree failure.
    #[error(transparent)]
    Session(#[from] xi_session::Error),
    /// Invalid policy.
    #[error(transparent)]
    Policy(#[from] xi_policy::Error),
    /// Invalid JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Provider initialization failure.
    #[error(transparent)]
    Provider(#[from] xi_provider::Error),
}

/// A kernel operation result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A persisted event or an ephemeral model increment.
#[derive(Clone, Debug)]
pub enum Notification {
    /// A durable append, including the session root.
    Event(EventParams),
    /// A model stream increment.
    Delta(DeltaParams),
    /// A retry replaces the preceding attempt's partial output.
    Reset(ResetParams),
}

impl Notification {
    /// Serializes the JSON-RPC notification envelope.
    pub fn json(&self) -> serde_json::Value {
        match self {
            Self::Event(params) => {
                serde_json::json!({"jsonrpc":"2.0", "method":"event/appended", "params":params})
            }
            Self::Delta(params) => {
                serde_json::json!({"jsonrpc":"2.0", "method":"stream/delta", "params":params})
            }
            Self::Reset(params) => {
                serde_json::json!({"jsonrpc":"2.0", "method":"stream/reset", "params":params})
            }
        }
    }
}

/// Fanout without lossy broadcast-ring overwrites.
#[derive(Clone, Default)]
pub struct EventBus(
    Arc<Mutex<Vec<mpsc::UnboundedSender<Notification>>>>,
    Arc<extensions::ExtensionHost>,
);

impl EventBus {
    /// Subscribes to future notifications.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Notification> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.0.lock().expect("event bus lock").push(tx);
        rx
    }

    fn send(&self, notification: Notification) {
        self.0
            .lock()
            .expect("event bus lock")
            .retain(|tx| tx.send(notification.clone()).is_ok());
    }

    /// Gracefully stops the process-wide extension host.
    pub async fn shutdown_extensions(&self) {
        self.1.shutdown().await;
    }
}

/// How asks are resolved for a session's clients.
#[derive(Clone, Copy, Debug)]
pub enum ApprovalMode {
    /// Wait for `approval/resolve`.
    Client,
    /// Resolve immediately, recording the headless approver.
    Headless(bool),
}

/// Grants permission to send one model request to an endpoint.
pub trait Leaser: Send + Sync {
    /// Resolves when granted. The returned guard releases on drop.
    fn lease<'a>(&'a self, endpoint: &'a str) -> BoxFuture<'a, Result<Box<dyn Send>>>;
}

/// Turn limits and client approval behavior.
#[derive(Clone)]
pub struct TurnOptions {
    /// Maximum model requests, excluding retries.
    pub max_turns: usize,
    /// Approval source.
    pub approval: ApprovalMode,
    /// Optional daemon scheduler for model requests.
    pub leaser: Option<Arc<dyn Leaser>>,
    /// Name the session with the `tiny` model when `[title] auto` allows.
    pub auto_title: bool,
}

impl Default for TurnOptions {
    fn default() -> Self {
        Self {
            max_turns: 200,
            approval: ApprovalMode::Client,
            leaser: None,
            auto_title: false,
        }
    }
}

/// Injectable model and tools for embedding and deterministic tests.
pub struct Backend {
    /// Provider instance.
    pub provider: Arc<dyn Provider>,
    /// Registry metadata for the selected model.
    pub model: ModelInfo,
    /// Active tools, in schema order.
    pub tools: Vec<Arc<dyn Tool>>,
}

impl Backend {
    fn connect(config: &Config) -> Result<Self> {
        let registry = config.registry();
        let model = registry.model(&config.model).cloned().ok_or_else(|| {
            Error::Config(format!(
                "unknown model {}/{}",
                config.model.provider, config.model.model
            ))
        })?;
        let provider = registry.connect(&config.model)?;
        let tools = toolset(&config.tools)?;
        Ok(Self {
            provider,
            model,
            tools,
        })
    }
}

/// Tools in `names` order: built-ins from `xi-tools`, plus the kernel's own
/// opt-in `task`, which runs only inside a [`Session`].
///
/// # Errors
/// Returns `Config` for an unknown tool name.
pub fn toolset(names: &[impl AsRef<str>]) -> Result<Vec<Arc<dyn Tool>>> {
    let names: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    let builtins: Vec<_> = names.iter().filter(|&&name| name != task::NAME).collect();
    let mut tools = xi_tools::toolset(&builtins).map_err(Error::Config)?;
    if let Some(at) = names.iter().position(|&name| name == task::NAME) {
        tools.insert(at, Arc::new(task::Task));
    }
    Ok(tools)
}

/// Whether `name` may appear in `tools`.
pub(crate) fn is_tool_name(name: &str) -> bool {
    xi_tools::is_builtin(name) || name == task::NAME
}

struct ActiveBackend {
    provider: Arc<dyn Provider>,
    model: ModelInfo,
    specs: Vec<xi_protocol::manifest::ToolSpec>,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl From<Backend> for ActiveBackend {
    fn from(backend: Backend) -> Self {
        let specs: Vec<_> = backend.tools.iter().map(|tool| tool.spec()).collect();
        let tools = specs
            .iter()
            .map(|spec| spec.name.clone())
            .zip(backend.tools)
            .collect();
        Self {
            provider: backend.provider,
            model: backend.model,
            specs,
            tools,
        }
    }
}

/// Result shared by all prompts consumed by one running turn.
#[derive(Clone, Debug)]
pub struct TurnOutcome {
    /// New branch head.
    pub head: EventId,
    /// Whether a failure or request limit ended the turn.
    pub failed: bool,
    /// Whether the client cancelled the turn.
    pub cancelled: bool,
    /// Text from the final assistant response only.
    pub text: String,
}

type Reply = oneshot::Sender<std::result::Result<TurnOutcome, String>>;
struct Queued {
    content: Vec<Block>,
    reply: Reply,
}
struct ApprovalAnswer {
    approved: bool,
    remember: Remember,
    grant: Option<(String, String)>,
}

struct PendingAsk {
    answer: oneshot::Sender<ApprovalAnswer>,
    taint: bool,
    kind: ActionKind,
    subject: String,
    grant: (String, String),
    workspace: PathBuf,
    trusted: bool,
}

#[derive(Default)]
struct Control {
    running: bool,
    generation: u64,
    cancel: CancellationToken,
    queue: VecDeque<Queued>,
    approvals: HashMap<ToolCallId, PendingAsk>,
    /// Titles that arrived while a turn held the log.
    titles: Vec<title::Update>,
}
struct State {
    log: SessionLog,
    head: EventId,
    config: Config,
    initial_reasoning: Reasoning,
    system: SystemContext,
    tools_context: xi_tools::Context,
    backend: Option<ActiveBackend>,
    extensions: Option<extensions::Extensions>,
    grants: HashSet<(String, String)>,
    egress: EgressState,
    titles: title::Titles,
}
/// The session's egress proxy for tool processes. Extensions have their own
/// listeners.
struct EgressState {
    listener: Option<egress::Listener>,
    tx: mpsc::UnboundedSender<egress::Connect>,
    /// Taken by [`Session::service`] while it waits, and put back after.
    rx: Option<mpsc::UnboundedReceiver<egress::Connect>>,
}

impl State {
    /// Every tool offered to the model: built-ins, then extension tools.
    fn tool_specs(&self) -> impl Iterator<Item = &xi_protocol::manifest::ToolSpec> {
        let backend = self.backend.as_ref().expect("connected backend");
        backend.specs.iter().chain(
            self.extensions
                .iter()
                .flat_map(|extensions| extensions.specs()),
        )
    }
}
struct Inner {
    id: SessionId,
    state: tokio::sync::Mutex<State>,
    control: Mutex<Control>,
    commands: Mutex<Vec<xi_protocol::manifest::CommandSpec>>,
    bus: EventBus,
    options: TurnOptions,
    stop: CancellationToken,
    idle: Notify,
}

/// A session handle. Prompts queue while a turn runs; control stays responsive.
#[derive(Clone)]
pub struct Session(Arc<Inner>);

/// A kernel session's durable log snapshot.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    /// Session identity.
    pub session: SessionId,
    /// Session file.
    pub path: PathBuf,
    /// Event the next prompt continues from.
    pub head: EventId,
    /// Every event in file order.
    pub events: Vec<xi_protocol::Event>,
}

impl Session {
    /// Creates a timestamped log and broadcasts its root.
    pub fn create(
        config: Config,
        directory: Option<&Path>,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
    ) -> Result<Self> {
        Self::create_linked(config, directory, bus, options, backend, None)
    }

    fn create_linked(
        config: Config,
        directory: Option<&Path>,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
        parent_session: Option<SessionId>,
    ) -> Result<Self> {
        let directory = directory.map_or_else(|| config.session_dir(), Path::to_owned);
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let id = SessionId::generate();
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| Error::Config(e.to_string()))?
            .replace(':', "-");
        let path = directory.join(format!("{timestamp}_{id}.jsonl"));
        let log = SessionLog::create(
            path,
            EventKind::SessionStarted {
                session: id,
                cwd: config.workspace.clone(),
                model: config.model.clone(),
                xi_version: env!("CARGO_PKG_VERSION").into(),
                parent_session,
            },
        )?;
        let root = log.root().clone();
        let session = Self::from_log(log, config, bus, options, backend)?;
        session.0.bus.send(Notification::Event(EventParams {
            session: id,
            event: root,
        }));
        Ok(session)
    }

    /// Resumes at the latest leaf of an already opened log.
    pub fn from_log(
        log: SessionLog,
        mut config: Config,
        bus: EventBus,
        options: TurnOptions,
        backend: Option<Backend>,
    ) -> Result<Self> {
        let EventKind::SessionStarted {
            session: id,
            cwd,
            model,
            ..
        } = &log.root().kind
        else {
            unreachable!("validated session root")
        };
        if cwd.canonicalize()? != config.workspace {
            return Err(Error::Config(
                "session workspace does not match configuration".into(),
            ));
        }
        let id = *id;
        let head = log.leaves().last().expect("session has a leaf").id;
        config.model = model.clone();
        let initial_reasoning = config.reasoning_override.unwrap_or(config.reasoning);
        config.reasoning = initial_reasoning;
        // Only an explicit invocation override beats saved reasoning. Config
        // defaults fill logs that have no reasoning change on this lineage.
        let restore_reasoning = config.reasoning_override.is_none();
        let mut pending = Vec::<(ToolCallId, bool)>::new();
        let mut asks = Vec::<ToolCallId>::new();
        for event in log.lineage(head).expect("known head") {
            match &event.kind {
                EventKind::ModelChanged { model } => config.model = model.clone(),
                EventKind::ReasoningChanged { reasoning } if restore_reasoning => {
                    config.reasoning = *reasoning;
                }
                EventKind::AssistantMessage { content, .. } => {
                    for block in content {
                        if let Block::ToolUse { id, .. } = block {
                            pending.push((id.clone(), false));
                        }
                    }
                }
                EventKind::ToolDecision { call, decision, .. } => {
                    if let Some((_, unresolved)) = pending.iter_mut().find(|(id, _)| id == call) {
                        *unresolved = *decision == Decision::Ask;
                    } else if *decision == Decision::Ask {
                        // An egress ask: it has no tool result of its own.
                        asks.push(call.clone());
                    }
                }
                EventKind::ApprovalResolved { call, .. } => {
                    if let Some((_, unresolved)) = pending.iter_mut().find(|(id, _)| id == call) {
                        *unresolved = false;
                    }
                    asks.retain(|id| id != call);
                }
                EventKind::ToolResult { call, .. } => pending.retain(|(id, _)| id != call),
                _ => {}
            }
        }
        let grants = lineage_grants(&log, head);
        let system = SystemContext::at(&config, log.root().at)?;
        let spill = log.file_path().with_extension("spill");
        let tmp = log.file_path().with_extension("tmp");
        std::fs::create_dir_all(&spill)?;
        std::fs::create_dir_all(&tmp)?;
        let spill = spill.canonicalize()?;
        let tmp = tmp.canonicalize()?;
        let git = log.file_path().with_extension("git");
        config::write_sandbox_git(&git, &config.paths, config.sandbox.ssh_agent)?;
        let git = git.canonicalize()?;
        let mut sandbox = config.sandbox_spec(&tmp, &spill, &system.skill_roots, &git)?;
        let listener = if sandbox.enabled {
            let listener = egress::Listener::bind("tools")?;
            sandbox.egress = Some(listener.egress().clone());
            Some(listener)
        } else {
            None
        };
        let (egress_tx, egress_rx) = mpsc::unbounded_channel();
        let executable = std::env::current_exe()?;
        let binary_dir = executable.parent().expect("executable directory");
        let binary_dir = if binary_dir.file_name().is_some_and(|name| name == "deps") {
            binary_dir.parent().expect("Cargo target directory")
        } else {
            binary_dir
        };
        let helper = std::env::var_os("XI_SANDBOX_EXEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| binary_dir.join("xi-sandbox-exec"));
        let tools_context = xi_tools::Context {
            workspace: config.workspace.clone(),
            sandbox,
            helper,
            spill_dir: spill,
            output_limit: 32 * 1024,
            cancel: CancellationToken::new(),
        };
        let backend = backend.map(Into::into);
        let titles = title::Titles::from_events(log.events());
        let session = Self(Arc::new(Inner {
            id,
            state: tokio::sync::Mutex::new(State {
                log,
                head,
                config,
                initial_reasoning,
                system,
                tools_context,
                backend,
                extensions: None,
                grants,
                egress: EgressState {
                    listener,
                    tx: egress_tx,
                    rx: Some(egress_rx),
                },
                titles,
            }),
            control: Mutex::new(Control::default()),
            commands: Mutex::new(Vec::new()),
            bus,
            options,
            stop: CancellationToken::new(),
            idle: Notify::new(),
        }));
        {
            let mut state = session.0.state.try_lock().expect("new session state");
            for call in asks {
                session.append(
                    &mut state,
                    EventKind::ApprovalResolved {
                        call,
                        approved: false,
                        by: Approver::Interrupted,
                        remember: Remember::Once,
                    },
                )?;
            }
            for (call, unresolved) in pending {
                if unresolved {
                    session.append(
                        &mut state,
                        EventKind::ApprovalResolved {
                            call: call.clone(),
                            approved: false,
                            by: Approver::Interrupted,
                            remember: Remember::Once,
                        },
                    )?;
                }
                session.append(
                    &mut state,
                    EventKind::ToolResult {
                        call,
                        output: error_output("interrupted"),
                        spill: None,
                    },
                )?;
            }
        }
        Ok(session)
    }

    /// Session identity.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.0.id
    }

    /// Whether a turn currently owns this session.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.0.control.lock().expect("control lock").running
    }

    /// Stops model requests after daemon loss without cancelling the tool batch.
    /// Queued and future prompts are rejected. Repeated calls have no effect.
    pub fn stop_requests(&self) {
        let mut control = self.0.control.lock().expect("control lock");
        self.0.stop.cancel();
        for prompt in control.queue.drain(..) {
            let _ = prompt.reply.send(Err("daemon connection lost".into()));
        }
        self.0.idle.notify_waiters();
    }

    /// Resolves when no turn is running and the prompt queue is empty.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.0.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let control = self.0.control.lock().expect("control lock");
                if !control.running && control.queue.is_empty() {
                    return;
                }
            }
            notified.await;
        }
    }

    /// A snapshot, available when no turn holds the session log.
    pub async fn info(&self) -> SessionSnapshot {
        let state = self.0.state.lock().await;
        SessionSnapshot {
            session: self.id(),
            path: state.log.file_path().to_owned(),
            head: state.head,
            events: state.log.events().to_vec(),
        }
    }

    /// Queues a prompt immediately. Its reply waits for the consuming turn.
    pub fn submit(
        &self,
        content: Vec<Block>,
    ) -> oneshot::Receiver<std::result::Result<TurnOutcome, String>> {
        let (reply, rx) = oneshot::channel();
        let mut control = self.0.control.lock().expect("control lock");
        if self.0.stop.is_cancelled() {
            let _ = reply.send(Err("daemon connection lost".into()));
            return rx;
        }
        control.queue.push_back(Queued { content, reply });
        if !control.running {
            control.running = true;
            control.cancel = CancellationToken::new();
            control.generation += 1;
            let generation = control.generation;
            let cancel = control.cancel.clone();
            let session = self.clone();
            tokio::spawn(async move {
                session.drive(generation, cancel).await;
            });
        }
        rx
    }

    /// Runs or steers a turn, resolving once that turn ends.
    pub async fn prompt(&self, content: Vec<Block>) -> Result<TurnOutcome> {
        self.submit(content)
            .await
            .map_err(|_| Error::Config("turn task stopped".into()))?
            .map_err(Error::Config)
    }

    /// Cancels the running turn without discarding queued steering messages.
    pub fn cancel(&self) {
        self.0.control.lock().expect("control lock").cancel.cancel();
    }

    /// Cancels and rejects queued prompts when the client transport closes.
    pub fn close(&self) {
        let mut control = self.0.control.lock().expect("control lock");
        control.cancel.cancel();
        for prompt in control.queue.drain(..) {
            let _ = prompt.reply.send(Err("client disconnected".into()));
        }
        self.0.idle.notify_waiters();
    }

    /// Resolves a pending ask. Duplicate and unknown answers fail.
    pub fn approve(&self, call: &ToolCallId, approved: bool, remember: Remember) -> Result<()> {
        let mut control = self.0.control.lock().expect("control lock");
        let pending = control.approvals.get(call).ok_or(Error::NotPending)?;
        if !remember.is_once() && (!approved || pending.taint) {
            return Err(Error::InvalidRemember(
                "only an approved, non-taint ask can be remembered".into(),
            ));
        }
        if remember == Remember::Workspace {
            if !pending.trusted {
                return Err(Error::InvalidRemember(
                    "workspace approvals require a trusted workspace".into(),
                ));
            }
            config::append_workspace_allow(&pending.workspace, pending.kind, &pending.subject)?;
        }
        let pending = control.approvals.remove(call).ok_or(Error::NotPending)?;
        pending
            .answer
            .send(ApprovalAnswer {
                approved,
                remember,
                grant: (!remember.is_once()).then_some(pending.grant),
            })
            .map_err(|_| Error::NotPending)
    }

    /// Moves the head only when no turn is running.
    pub async fn checkout(&self, event: EventId) -> Result<()> {
        let mut state = self.0.state.try_lock().map_err(|_| Error::Running)?;
        let control = self.0.control.lock().expect("control lock");
        if control.running {
            return Err(Error::Running);
        }
        let lineage = state
            .log
            .lineage(event)
            .ok_or_else(|| Error::Config("unknown checkout event".into()))?;
        let mut selected = None;
        let mut reasoning = state.initial_reasoning;
        for event in lineage {
            match &event.kind {
                EventKind::SessionStarted { model, .. } | EventKind::ModelChanged { model } => {
                    selected = Some(model.clone())
                }
                EventKind::ReasoningChanged {
                    reasoning: selected,
                } => reasoning = *selected,
                _ => {}
            }
        }
        state.grants = lineage_grants(&state.log, event);
        state.head = event;
        state.config.reasoning = reasoning;
        if let Some(model) = selected
            && model != state.config.model
        {
            state.config.model = model;
            state.backend = None;
        }
        Ok(())
    }

    /// Switches model at an idle prompt and records the selection, returning
    /// the concrete model a role resolved to. Selecting the model already in
    /// use connects it but records nothing.
    pub async fn set_model(&self, model: impl Into<ModelSelector>) -> Result<ModelRef> {
        let mut state = self.0.state.try_lock().map_err(|_| Error::Running)?;
        let control = self.0.control.lock().expect("control lock");
        if control.running {
            return Err(Error::Running);
        }
        let model = state.config.resolve(&model.into());
        let mut next = state.config.clone();
        next.model = model.clone();
        let backend = Backend::connect(&next)?;
        if model != state.config.model {
            self.append(
                &mut state,
                EventKind::ModelChanged {
                    model: model.clone(),
                },
            )?;
        }
        state.config = next;
        state.backend = Some(backend.into());
        Ok(model)
    }

    /// Changes reasoning depth at an idle prompt, recording only changed
    /// levels. A model that reasons must list the level.
    pub async fn set_reasoning(&self, reasoning: Reasoning) -> Result<()> {
        let mut state = self.0.state.try_lock().map_err(|_| Error::Running)?;
        let control = self.0.control.lock().expect("control lock");
        if control.running {
            return Err(Error::Running);
        }
        let registry;
        let model = match &state.backend {
            Some(backend) => Some(&backend.model),
            None => {
                registry = state.config.registry();
                registry.model(&state.config.model)
            }
        };
        if let Some(model) = model
            && model.resolve(reasoning) != reasoning
        {
            let levels: Vec<_> = model.reasoning.iter().map(|level| level.name()).collect();
            return Err(Error::Config(format!(
                "{}/{} does not support {reasoning} reasoning; it accepts {}",
                model.provider,
                model.id,
                levels.join(", ")
            )));
        }
        if reasoning != state.config.reasoning {
            self.append(&mut state, EventKind::ReasoningChanged { reasoning })?;
            state.config.reasoning = reasoning;
        }
        Ok(())
    }

    fn append(&self, state: &mut State, kind: EventKind) -> Result<()> {
        let event = state.log.append(state.head, kind)?.clone();
        state.head = event.id;
        self.0.bus.send(Notification::Event(EventParams {
            session: self.id(),
            event,
        }));
        Ok(())
    }

    fn drain(&self, state: &mut State, replies: &mut Vec<Reply>) -> Result<bool> {
        let queued = {
            let mut control = self.0.control.lock().expect("control lock");
            if control.cancel.is_cancelled() || self.0.stop.is_cancelled() {
                return Ok(false);
            }
            control.queue.drain(..).collect::<Vec<_>>()
        };
        let consumed = !queued.is_empty();
        for prompt in queued {
            replies.push(prompt.reply);
            self.append(
                state,
                EventKind::UserMessage {
                    content: prompt.content,
                },
            )?;
        }
        Ok(consumed)
    }

    async fn drive(&self, generation: u64, cancel: CancellationToken) {
        let mut state = self.0.state.lock().await;
        state.tools_context.cancel = cancel.clone();
        loop {
            let mut replies = Vec::new();
            let result = self.run(&mut state, &cancel, &mut replies).await;
            // A prompt arriving during turn-end hooks starts a new turn.
            // Cancellation retains steering until a later submission.
            let (again, titles) = {
                let mut control = self.0.control.lock().expect("control lock");
                if control.generation != generation {
                    (false, Vec::new())
                } else {
                    control.approvals.clear();
                    let again = result.is_ok()
                        && !cancel.is_cancelled()
                        && !self.0.stop.is_cancelled()
                        && !control.queue.is_empty();
                    control.running = again;
                    (again, std::mem::take(&mut control.titles))
                }
            };
            let mut result = result.map_err(|error| error.to_string());
            // Titles that arrived after the turn's last boundary.
            for update in titles {
                if let Err(error) = self.apply_title(&mut state, update)
                    && result.is_ok()
                {
                    result = Err(error.to_string());
                }
            }
            for reply in replies {
                let _ = reply.send(result.clone());
            }
            self.0.idle.notify_waiters();
            if !again {
                break;
            }
        }
    }

    async fn run(
        &self,
        state: &mut State,
        cancel: &CancellationToken,
        replies: &mut Vec<Reply>,
    ) -> Result<TurnOutcome> {
        // Even an immediate cancel acknowledges the initiating prompt, but not
        // later steering. The first prompt is the current turn's input.
        let first = self
            .0
            .control
            .lock()
            .expect("control lock")
            .queue
            .pop_front();
        if let Some(first) = first {
            replies.push(first.reply);
            self.append(
                state,
                EventKind::UserMessage {
                    content: first.content,
                },
            )?;
        }
        let mut outcome = TurnOutcome {
            head: state.head,
            failed: false,
            cancelled: false,
            text: String::new(),
        };
        if !self.0.stop.is_cancelled() {
            self.prepare_extensions(state).await?;
        }
        let mut requests = 0;
        loop {
            if self.0.stop.is_cancelled() {
                self.append(
                    state,
                    EventKind::TurnFailed {
                        message: "daemon connection lost".into(),
                    },
                )?;
                outcome.failed = true;
                break;
            }
            if cancel.is_cancelled() {
                outcome.cancelled = true;
                break;
            }
            self.drain(state, replies)?;
            self.apply_titles(state)?;
            if requests >= self.0.options.max_turns {
                self.append(
                    state,
                    EventKind::TurnFailed {
                        message: format!(
                            "maximum model requests reached ({})",
                            self.0.options.max_turns
                        ),
                    },
                )?;
                outcome.failed = true;
                break;
            }
            if state.backend.is_none() {
                state.backend = Some(Backend::connect(&state.config)?.into());
            }
            self.start_titles(state);

            requests += 1;
            let response = self.model_response(state, cancel).await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    self.append(
                        state,
                        EventKind::TurnFailed {
                            message: if self.0.stop.is_cancelled() {
                                "daemon connection lost".into()
                            } else {
                                error.to_string()
                            },
                        },
                    )?;
                    outcome.failed = true;
                    break;
                }
            };
            outcome.text = response
                .content
                .iter()
                .filter_map(|b| {
                    if let Block::Text { text } = b {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            let stop = response.stop;
            let calls: Vec<_> = response
                .content
                .iter()
                .filter_map(|b| {
                    if let Block::ToolUse {
                        id, name, input, ..
                    } = b
                    {
                        Some((id.clone(), name.clone(), input.clone()))
                    } else {
                        None
                    }
                })
                .collect();
            self.append(
                state,
                EventKind::AssistantMessage {
                    model: state.config.model.clone(),
                    content: response.content,
                    stop,
                    usage: response.usage,
                },
            )?;
            for (call, name, input) in calls {
                if cancel.is_cancelled() {
                    self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output("cancelled"),
                            spill: None,
                        },
                    )?;
                } else {
                    self.invoke(state, call, &name, input, cancel).await?;
                }
            }
            if stop == StopReason::Cancelled || cancel.is_cancelled() {
                outcome.cancelled = true;
                break;
            }
            if stop != StopReason::ToolUse {
                // A steering message can extend an otherwise completed turn.
                let control = self.0.control.lock().expect("control lock");
                if control.queue.is_empty() {
                    break;
                }
            }
        }
        let extensions = state.extensions.clone().unwrap_or_default();
        if !cancel.is_cancelled()
            && !self.0.stop.is_cancelled()
            && extensions.has_hook(xi_protocol::manifest::HookPoint::TurnEnd)
        {
            let event = xi_protocol::methods::extension::HookEvent::TurnEnd { head: state.head };
            if let Err(error) = self
                .service(
                    state,
                    extensions.hook(self.id(), event, cancel),
                    cancel,
                    None,
                )
                .await
            {
                self.append(
                    state,
                    EventKind::TurnFailed {
                        message: error.to_string(),
                    },
                )?;
                outcome.failed = true;
            }
        }
        if self.0.stop.is_cancelled() && !outcome.failed {
            self.append(
                state,
                EventKind::TurnFailed {
                    message: "daemon connection lost".into(),
                },
            )?;
            outcome.failed = true;
        }
        self.apply_titles(state)?;
        self.finish_titles(state, &outcome);
        outcome.head = state.head;
        Ok(outcome)
    }

    async fn model_response(
        &self,
        state: &mut State,
        cancel: &CancellationToken,
    ) -> Result<Response> {
        let backend = state.backend.as_ref().expect("connected backend");
        if state.config.compaction.enabled
            && compaction::estimate(state)
                >= state
                    .config
                    .compaction
                    .trigger(backend.model.context_window)
        {
            self.compact_at_boundary(state, cancel).await?;
        }
        let mut overflow_compacted = false;
        loop {
            let backend = state.backend.as_ref().expect("connected backend");
            let request = Request {
                model: state.config.model.model.clone(),
                system: state.system.prompt.clone(),
                messages: context::messages(state.log.context(state.head).expect("known head")),
                tools: state.tool_specs().cloned().collect(),
                max_tokens: backend.model.max_output,
                reasoning: backend.model.resolve(state.config.reasoning),
                cache_key: Some(self.id().to_string()),
            };
            let response = self
                .stream(
                    backend.provider.as_ref(),
                    &request,
                    &state.config,
                    &state.config.model,
                    cancel,
                    true,
                )
                .await;
            // Overflow recovery runs even with automatic compaction off: the
            // request has already failed, so summarising costs nothing extra.
            match response {
                Err(Error::Provider(error))
                    if error.kind == xi_provider::ErrorKind::ContextOverflow
                        && !overflow_compacted =>
                {
                    overflow_compacted = true;
                    if self.compact_at_boundary(state, cancel).await?.is_none()
                        && !cancel.is_cancelled()
                    {
                        return Err(error.into());
                    }
                }
                result => return result,
            }
        }
    }

    async fn stream(
        &self,
        provider: &dyn Provider,
        request: &Request,
        config: &Config,
        model: &ModelRef,
        cancel: &CancellationToken,
        publish: bool,
    ) -> Result<Response> {
        let cancelled = |content| Response {
            content,
            stop: StopReason::Cancelled,
            usage: Usage::default(),
        };
        let _lease = if let Some(leaser) = &self.0.options.leaser {
            let registry = config.registry();
            let endpoint = &registry
                .provider(&model.provider)
                .ok_or_else(|| Error::Config(format!("unknown provider {}", model.provider)))?
                .base_url;
            Some(tokio::select! {
                biased;
                () = self.0.stop.cancelled() => return Err(Error::Config("daemon connection lost".into())),
                () = cancel.cancelled() => return Ok(cancelled(Vec::new())),
                guard = leaser.lease(endpoint) => guard?,
            })
        } else {
            None
        };
        for attempt in 0..=4 {
            if self.0.stop.is_cancelled() {
                return Err(Error::Config("daemon connection lost".into()));
            }
            if cancel.is_cancelled() {
                return Ok(cancelled(Vec::new()));
            }
            if attempt > 0 && publish {
                self.0.bus.send(Notification::Reset(ResetParams {
                    session: self.id(),
                    attempt,
                    sequence: 0,
                }));
            }
            let mut partial = Vec::<Block>::new();
            let mut on_delta = |delta: Delta| {
                if !publish {
                    return;
                }
                match &delta {
                    Delta::Text { text } => push_partial(&mut partial, text, false),
                    Delta::Thinking { text } => push_partial(&mut partial, text, true),
                    Delta::ToolInput { .. } => {}
                }
                self.0.bus.send(Notification::Delta(DeltaParams {
                    session: self.id(),
                    delta,
                    attempt,
                    sequence: 0,
                }));
            };
            let result = tokio::select! {
                biased;
                () = self.0.stop.cancelled() => return Err(Error::Config("daemon connection lost".into())),
                () = cancel.cancelled() => None,
                result = provider.stream(request, &mut on_delta) => Some(result),
            };
            let Some(result) = result else {
                return Ok(cancelled(partial));
            };
            match result {
                Ok(response) => return Ok(response),
                Err(error) if error.is_retryable() && attempt < 4 => {
                    let backoff = Duration::from_secs(1 << attempt);
                    let delay = error
                        .retry_after
                        .unwrap_or(backoff)
                        .max(backoff)
                        .min(Duration::from_secs(60));
                    tokio::select! {
                        biased;
                        () = self.0.stop.cancelled() => return Err(Error::Config("daemon connection lost".into())),
                        () = cancel.cancelled() => return Ok(cancelled(partial)),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        unreachable!("last attempt returns")
    }

    /// Runs a `task` call's children concurrently while serving egress asks,
    /// and joins their answers in task order.
    async fn run_tasks(
        &self,
        state: &mut State,
        input: serde_json::Value,
        cancel: &CancellationToken,
        current: extension_session::Current<'_>,
    ) -> xi_tools::Ran {
        let output = match task::parse(input) {
            Err(error) => error_output(error),
            Ok(children) => {
                let runs: Vec<_> = children
                    .into_iter()
                    .map(|child| {
                        let params = xi_protocol::methods::extension::AgentRunParams {
                            session: self.id(),
                            prompt: child.prompt,
                            tools: child.tools,
                            model: child.model,
                        };
                        self.run_child(state, params, cancel)
                    })
                    .collect();
                let all = async { Ok(futures::future::join_all(runs).await) };
                match self.service(state, all, cancel, Some(current)).await {
                    Ok(results) => task::output(results),
                    Err(error) => error_output(error.to_string()),
                }
            }
        };
        xi_tools::Ran {
            output,
            spill: None,
        }
    }

    async fn invoke(
        &self,
        state: &mut State,
        call: ToolCallId,
        name: &str,
        mut input: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<()> {
        use xi_protocol::methods::extension::{HookEvent, HookOutcome, InvokeParams};
        let extensions = state.extensions.clone().unwrap_or_default();
        let current = extension_session::Current {
            call: &call,
            tool: name,
        };
        if extensions.has_hook(xi_protocol::manifest::HookPoint::PreTool) {
            let event = HookEvent::PreTool {
                call: call.clone(),
                tool: name.into(),
                input: input.clone(),
            };
            match self
                .service(
                    state,
                    extensions.hook(self.id(), event, cancel),
                    cancel,
                    Some(current),
                )
                .await
            {
                Ok(HookOutcome::RewriteInput { input: rewritten }) => input = rewritten,
                Ok(HookOutcome::Deny { reason }) => {
                    self.append(
                        state,
                        EventKind::ToolDecision {
                            call: call.clone(),
                            action: format!("tool {name}"),
                            decision: Decision::Deny,
                            rule: Some("extension pre_tool".into()),
                            taint: false,
                            during: None,
                            extension: None,
                        },
                    )?;
                    return self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output(reason),
                            spill: None,
                        },
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    return self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output(error.to_string()),
                            spill: None,
                        },
                    );
                }
            }
        }
        let tool = state
            .backend
            .as_ref()
            .expect("connected backend")
            .tools
            .get(name)
            .cloned();
        if tool.is_none() && !extensions.has_tool(name) {
            return self.append(
                state,
                EventKind::ToolResult {
                    call,
                    output: error_output(format!("unknown tool {name}")),
                    spill: None,
                },
            );
        };
        let crossings = match tool.as_ref().map_or_else(
            || Ok(Vec::new()),
            |tool| tool.crossings(&input, &state.tools_context),
        ) {
            Ok(crossings) => crossings,
            Err(error) => {
                return self.append(
                    state,
                    EventKind::ToolResult {
                        call,
                        output: error_output(error),
                        spill: None,
                    },
                );
            }
        };
        let taint = if state.log.is_tainted(state.head) == Some(true) {
            Taint::Tainted
        } else {
            Taint::Clean
        };
        let mut verdict = state.config.policy.evaluate(Action::Tool { name }, taint);
        let mut action = format!("tool {name}");
        let mut deciding = None;
        for crossing in &crossings {
            let candidate = state.config.policy.evaluate(
                match crossing {
                    Crossing::Read(path) => Action::Read { path },
                    Crossing::Write(path) => Action::Write { path },
                    Crossing::Exec(command) => Action::Exec { command },
                    Crossing::Net(host) => Action::Net { host },
                },
                taint,
            );
            if candidate.decision > verdict.decision {
                verdict = candidate;
                deciding = Some(crossing);
                action = match crossing {
                    Crossing::Read(p) => format!("read {}", p.display()),
                    Crossing::Write(p) => format!("write {}", p.display()),
                    Crossing::Exec(c) => format!("exec {c}"),
                    Crossing::Net(h) => format!("net {h}"),
                };
            }
        }
        let taint_ask = verdict.taint_downgraded;
        let mut decision = verdict.decision;
        let mut rule = verdict.rule.map(|rule| rule.to_string());
        if decision == Decision::Ask
            && !taint_ask
            && !(taint == Taint::Tainted && matches!(deciding, Some(Crossing::Net(_))))
            && state
                .grants
                .iter()
                .any(|(tool, granted)| tool == name && granted == &action)
        {
            decision = Decision::Allow;
            rule = Some("session".into());
        }
        // Register the waiter before announcing the decision: fast clients may
        // answer as soon as they see the event.
        let waiter = if decision == Decision::Ask
            && matches!(self.0.options.approval, ApprovalMode::Client)
        {
            let (tx, rx) = oneshot::channel();
            self.0
                .control
                .lock()
                .expect("control lock")
                .approvals
                .insert(
                    call.clone(),
                    PendingAsk {
                        answer: tx,
                        taint: taint_ask,
                        kind: match deciding {
                            Some(Crossing::Read(_)) => ActionKind::Read,
                            Some(Crossing::Write(_)) => ActionKind::Write,
                            Some(Crossing::Exec(_)) => ActionKind::Exec,
                            Some(Crossing::Net(_)) => ActionKind::Net,
                            None => ActionKind::Tool,
                        },
                        subject: match deciding {
                            Some(Crossing::Read(path) | Crossing::Write(path)) => {
                                path.to_string_lossy().into_owned()
                            }
                            Some(Crossing::Exec(subject) | Crossing::Net(subject)) => {
                                subject.clone()
                            }
                            None => name.into(),
                        },
                        grant: (name.into(), action.clone()),
                        workspace: state.config.workspace.clone(),
                        trusted: state.config.trusted,
                    },
                );
            Some(rx)
        } else {
            None
        };
        self.append(
            state,
            EventKind::ToolDecision {
                call: call.clone(),
                action,
                decision,
                rule: rule.clone(),
                taint: taint_ask,
                during: None,
                extension: None,
            },
        )?;
        let approved = match decision {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask => {
                let (approved, by, remember, grant) = match self.0.options.approval {
                    ApprovalMode::Headless(approved) => {
                        (approved, Approver::Headless, Remember::Once, None)
                    }
                    ApprovalMode::Client => {
                        let timeout = async {
                            match state.config.daemon.approval_timeout {
                                Some(seconds) => {
                                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                                }
                                None => std::future::pending::<()>().await,
                            }
                        };
                        let result = tokio::select! {
                            biased;
                            () = cancel.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                            () = self.0.stop.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                            result = waiter.expect("client waiter") => match result {
                                Ok(answer) => (answer.approved, Approver::User, answer.remember, answer.grant),
                                Err(_) => (false, Approver::Cancelled, Remember::Once, None),
                            },
                            () = timeout => (false, Approver::Timeout, Remember::Once, None),
                        };
                        self.0
                            .control
                            .lock()
                            .expect("control lock")
                            .approvals
                            .remove(&call);
                        result
                    }
                };
                let cancelled = matches!(by, Approver::Cancelled);
                self.append(
                    state,
                    EventKind::ApprovalResolved {
                        call: call.clone(),
                        approved,
                        by,
                        remember,
                    },
                )?;
                if let Some(grant) = grant {
                    state.grants.insert(grant);
                }
                if cancelled {
                    return self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output("cancelled"),
                            spill: None,
                        },
                    );
                }
                approved
            }
        };
        if !approved {
            return self.append(
                state,
                EventKind::ToolResult {
                    call,
                    output: error_output(format!(
                        "denied by policy ({})",
                        rule.as_deref().unwrap_or("no matching rule")
                    )),
                    spill: None,
                },
            );
        }
        let post_input = extensions
            .has_hook(xi_protocol::manifest::HookPoint::PostTool)
            .then(|| input.clone());
        let mut result = if name == task::NAME && tool.is_some() {
            self.run_tasks(state, input, cancel, current).await
        } else if let Some(tool) = tool {
            let context = state.tools_context.clone();
            let run = async {
                Ok(tokio::select! {
                    biased;
                    () = cancel.cancelled() => xi_tools::Ran { output: error_output("cancelled"), spill: None },
                    result = tool.run(&call, input, &context) => result,
                })
            };
            self.service(state, run, cancel, Some(current))
                .await
                .unwrap_or_else(|error| xi_tools::Ran {
                    output: error_output(error.to_string()),
                    spill: None,
                })
        } else {
            let params = InvokeParams {
                session: self.id(),
                call: call.clone(),
                tool: name.into(),
                input,
            };
            let output = self
                .service(
                    state,
                    extensions.invoke(&params, cancel),
                    cancel,
                    Some(current),
                )
                .await
                .unwrap_or_else(|error| error_output(error.to_string()));
            xi_tools::Ran {
                output,
                spill: None,
            }
        };
        if !cancel.is_cancelled()
            && let Some(input) = post_input
        {
            let event = HookEvent::PostTool {
                call: call.clone(),
                tool: name.into(),
                input,
                output: result.output.clone(),
            };
            match self
                .service(
                    state,
                    extensions.hook(self.id(), event, cancel),
                    cancel,
                    Some(current),
                )
                .await
            {
                Ok(HookOutcome::ReplaceOutput { output }) => {
                    result.output = output;
                    result.spill = None;
                }
                Ok(_) => {}
                Err(error) => {
                    let untrusted = result.output.untrusted;
                    result.output = error_output(error.to_string());
                    result.output.untrusted = untrusted;
                    result.spill = None;
                }
            }
        }
        if result.spill.is_none()
            && result
                .output
                .content
                .iter()
                .all(|block| matches!(block, OutputBlock::Text { .. }))
            && result
                .output
                .content
                .iter()
                .map(|block| match block {
                    OutputBlock::Text { text } => text.len().saturating_add(1),
                    _ => 0,
                })
                .sum::<usize>()
                > state.tools_context.output_limit
        {
            let text = result
                .output
                .content
                .iter()
                .filter_map(|block| match block {
                    OutputBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.len() > state.tools_context.output_limit {
                let (text, spill) = xi_tools::truncate(&call, text, &state.tools_context);
                result.output.content = vec![OutputBlock::Text { text }];
                result.spill = spill;
            }
        }
        let untrusted = result.output.untrusted;
        self.append(
            state,
            EventKind::ToolResult {
                call: call.clone(),
                output: result.output,
                spill: result.spill,
            },
        )?;
        if untrusted {
            self.append(
                state,
                EventKind::Tainted {
                    call: Some(call),
                    reason: format!("untrusted output from {name}"),
                },
            )?;
        }
        Ok(())
    }
}

fn lineage_grants(log: &SessionLog, head: EventId) -> HashSet<(String, String)> {
    let mut tools: HashMap<&ToolCallId, &String> = HashMap::new();
    let mut actions = HashMap::new();
    let mut grants = HashSet::new();
    for event in log.lineage(head).expect("known head") {
        match &event.kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    if let Block::ToolUse { id, name, .. } = block {
                        tools.insert(id, name);
                    }
                }
            }
            EventKind::ToolDecision {
                call,
                action,
                during,
                extension,
                ..
            } => {
                actions.insert(call, action);
                // An egress decision is keyed by the tool or extension that
                // connected, like the grant it may create.
                if let Some(name) = during
                    .as_ref()
                    .and_then(|during| tools.get(during).copied())
                    .or(extension.as_ref())
                {
                    tools.insert(call, name);
                }
            }
            EventKind::ApprovalResolved {
                call,
                approved: true,
                remember: Remember::Session | Remember::Workspace,
                ..
            } => {
                if let (Some(tool), Some(action)) = (tools.get(call), actions.get(call)) {
                    grants.insert(((*tool).clone(), (*action).clone()));
                }
            }
            _ => {}
        }
    }
    grants
}

fn push_partial(blocks: &mut Vec<Block>, text: &str, thinking: bool) {
    match blocks.last_mut() {
        Some(Block::Thinking { text: current, .. }) if thinking => current.push_str(text),
        Some(Block::Text { text: current }) if !thinking => current.push_str(text),
        _ if thinking => blocks.push(Block::Thinking {
            text: text.into(),
            signature: None,
        }),
        _ => blocks.push(Block::Text { text: text.into() }),
    }
}

fn error_output(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![OutputBlock::Text {
            text: message.into(),
        }],
        is_error: true,
        untrusted: false,
    }
}
