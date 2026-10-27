//! Session handles, snapshots and actor-owned turn state.

pub(crate) mod actor;
pub(crate) mod approvals;
#[path = "../compaction.rs"]
pub mod compaction;
#[path = "../extension_session.rs"]
mod extension_session;
mod invoke;
mod restore;
mod snapshot;
#[path = "../task.rs"]
mod task;
#[path = "../title.rs"]
pub mod title;
mod turn;
#[path = "../web_search.rs"]
mod web_search;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use xi_policy::{Action, ActionKind, Taint};
use xi_protocol::BoxFuture;
use xi_protocol::event::{Approver, ModelRef, Remember, StopReason, Usage};
use xi_protocol::methods::client::{Delta, DeltaParams, EventParams, ResetParams, SessionDetails};
use xi_protocol::model::{ModelInfo, ModelSelector, Reasoning};
use xi_protocol::{
    Block, Decision, EventId, EventKind, OutputBlock, SessionId, ToolCallId, ToolOutput,
};
use xi_provider::{Provider, Request, Response};
use xi_session::SessionLog;
use xi_tools::{Crossing, Tool};

use crate::{config, context, egress, extensions, sandbox};
use actor::{Command, IdleCommand};
use approvals::{ActionKey, ApprovalAnswer, PendingAsk};
use config::Config;
use context::SystemContext;

/// Kernel and configuration failures.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Error {
    /// Invalid user configuration.
    #[error("{0}")]
    Config(String),
    /// A selected model is absent from the registry.
    #[error("unknown model {provider}/{model}")]
    Model {
        /// Provider identifier.
        provider: String,
        /// Model identifier.
        model: String,
    },
    /// An extension violated its protocol or stopped unexpectedly.
    #[error("{0}")]
    Extension(String),
    /// A requested event does not exist in this session.
    #[error("unknown checkout event {0}")]
    UnknownEvent(EventId),
    /// The session no longer accepts requests.
    #[error("session stopped")]
    Stopped,
    /// The model requested an unavailable tool.
    #[error("unknown tool {0}")]
    Tool(String),
    /// A session operation could not finish.
    #[error("{0}")]
    Operation(String),
    /// A turn is running.
    #[error("session is running")]
    Running,
    /// No ask is pending for that call.
    #[error("no pending approval for that call")]
    NotPending,
    /// A requested approval scope is not valid for this ask.
    #[error("{0}")]
    InvalidRemember(String),
    /// Filesystem failure, shared by prompts consumed in one turn.
    #[error(transparent)]
    Io(Arc<std::io::Error>),
    /// Session durability or tree failure.
    #[error(transparent)]
    Session(Arc<xi_session::Error>),
    /// Invalid policy.
    #[error(transparent)]
    Policy(Arc<xi_policy::Error>),
    /// Invalid JSON.
    #[error(transparent)]
    Json(Arc<serde_json::Error>),
    /// Provider initialization failure.
    #[error(transparent)]
    Provider(Arc<xi_provider::Error>),
}

macro_rules! shared_error {
    ($($source:ty => $variant:ident),* $(,)?) => {$ (
        impl From<$source> for Error {
            fn from(error: $source) -> Self { Self::$variant(Arc::new(error)) }
        }
    )*};
}
shared_error!(std::io::Error => Io, xi_session::Error => Session, xi_policy::Error => Policy, serde_json::Error => Json, xi_provider::Error => Provider);

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
    /// The protocol marker's method name, without a hand-built envelope.
    #[must_use]
    pub fn method(&self) -> &'static str {
        use xi_protocol::methods::client::{EventAppended, StreamDelta, StreamReset};
        use xi_protocol::rpc::NotificationMethod;
        match self {
            Self::Event(_) => EventAppended::METHOD,
            Self::Delta(_) => StreamDelta::METHOD,
            Self::Reset(_) => StreamReset::METHOD,
        }
    }
}

/// Fanout without lossy broadcast-ring overwrites.
#[derive(Clone, Default)]
pub struct EventBus {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<Notification>>>>,
}

impl EventBus {
    /// Subscribes to future notifications.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Notification> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        rx
    }

    fn send(&self, notification: Notification) {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|tx| tx.send(notification.clone()).is_ok());
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
    pub(crate) fn connect(config: &Config) -> Result<Self> {
        let registry = config.registry();
        let model = registry
            .model(&config.model)
            .cloned()
            .ok_or_else(|| Error::Model {
                provider: config.model.provider.clone(),
                model: config.model.model.clone(),
            })?;
        let provider = registry.connect(&config.model)?;
        let tools = toolset(config)?;
        Ok(Self {
            provider,
            model,
            tools,
        })
    }
}

/// The tools `config.tools` names, in order: built-ins from `xi-tools`, plus
/// the kernel's own opt-in `task` and `web_search`, which run only inside a
/// [`Session`].
///
/// # Errors
/// Returns `Tool` for an unknown tool name.
pub fn toolset(config: &Config) -> Result<Vec<Arc<dyn Tool>>> {
    let names: Vec<&str> = config.tools.iter().map(String::as_str).collect();
    let builtins: Vec<_> = names
        .iter()
        .filter(|name| invoke::KernelKind::named(name).is_none())
        .collect();
    let mut tools = xi_tools::toolset(&builtins).map_err(|_| {
        Error::Tool(
            builtins
                .iter()
                .find(|name| !xi_tools::is_builtin(name))
                .map_or("", |name| **name)
                .into(),
        )
    })?;
    for (at, name) in names.iter().enumerate() {
        if let Some(kind) = invoke::KernelKind::named(name) {
            tools.insert(at, kind.catalog(config));
        }
    }
    Ok(tools)
}

/// Whether `name` may appear in `tools`.
pub(crate) fn is_tool_name(name: &str) -> bool {
    xi_tools::is_builtin(name) || invoke::KernelKind::named(name).is_some()
}

struct ActiveBackend {
    provider: Arc<dyn Provider>,
    model: ModelInfo,
    specs: Vec<xi_protocol::manifest::ToolSpec>,
    tools: HashMap<String, Arc<dyn Tool>>,
    kernel_tools: HashMap<String, Arc<dyn invoke::KernelTool>>,
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
            kernel_tools: invoke::kernel_tools(&tools),
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

type Reply = oneshot::Sender<Result<TurnOutcome>>;
struct Queued {
    content: Vec<Block>,
    reply: Reply,
}

pub(crate) struct State {
    log: SessionLog,
    head: EventId,
    config: Config,
    initial_reasoning: Reasoning,
    system: SystemContext,
    tools_context: xi_tools::Context,
    backend: Option<ActiveBackend>,
    extensions: Option<extensions::Extensions>,
    grants: HashMap<String, HashSet<ActionKey>>,
    egress: EgressState,
    titles: title::Titles,
}
/// The session's egress proxy for tool processes. Extensions have their own
/// listeners.
struct EgressState {
    listener: Option<egress::Listener>,
    tx: mpsc::UnboundedSender<egress::Connect>,
    rx: mpsc::UnboundedReceiver<egress::Connect>,
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
    tx: mpsc::UnboundedSender<Command>,
    snapshot: watch::Sender<snapshot::Snapshot>,
    parent_snapshot: Option<watch::Sender<snapshot::Snapshot>>,
    running: watch::Sender<bool>,
    commands: watch::Sender<Vec<xi_protocol::manifest::CommandSpec>>,
    config: watch::Sender<Config>,
    skills: Vec<context::Skill>,
    bus: EventBus,
    extensions: extensions::ExtensionHost,
    options: TurnOptions,
    stop: CancellationToken,
    sequence: AtomicU64,
    child_sequence: AtomicU64,
    child_ready: watch::Sender<u64>,
}

/// A cheap session handle. One actor owns control and idle turn state.
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
    /// Current session metadata.
    pub details: SessionDetails,
    /// Text and thinking received since the last durable assistant message.
    pub partial: Vec<Block>,
    /// Last published stream sequence.
    pub stream_sequence: u64,
    /// Unresolved approvals for attachment.
    pub pending_approvals: Vec<xi_protocol::methods::client::PendingApproval>,
    /// Direct children active in the current tool batch.
    pub children: Vec<xi_protocol::methods::client::ChildEvents>,
    /// Whether the selected lineage contains untrusted output.
    pub tainted: bool,
}

impl Session {
    /// Session identity.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.0.id
    }
    /// Whether a turn or idle operation currently owns this session.
    #[must_use]
    pub fn is_running(&self) -> bool {
        *self.0.running.borrow()
    }
    /// Stops future model requests without interrupting the current tool batch.
    pub fn stop_requests(&self) {
        self.0.stop.cancel();
        let _ = self.0.tx.send(Command::Stop);
    }
    /// Resolves when no operation or queued prompt remains.
    pub async fn wait_idle(&self) {
        let (tx, rx) = oneshot::channel();
        if self.0.tx.send(Command::WaitIdle(tx)).is_ok() {
            let _ = rx.await;
        }
    }
    /// Materializes a snapshot without waiting for a running turn.
    pub async fn info(&self) -> SessionSnapshot {
        self.snapshot()
    }
    pub(crate) fn snapshot(&self) -> SessionSnapshot {
        self.0.snapshot.borrow().materialize()
    }
    pub(crate) fn details(&self) -> SessionDetails {
        self.0.snapshot.borrow().details.clone()
    }
    pub(crate) fn config(&self) -> Config {
        self.0.config.borrow().clone()
    }
    /// Extension and skill commands available for completion.
    #[must_use]
    pub fn commands(&self) -> Vec<xi_protocol::manifest::CommandSpec> {
        let mut commands = self.0.commands.borrow().clone();
        commands.extend(self.0.skills.iter().map(|skill| {
            xi_protocol::manifest::CommandSpec {
                name: format!("skill:{}", skill.name),
                description: skill
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            }
        }));
        commands
    }
    /// Queues input and returns the typed result of its consuming turn.
    pub fn submit(&self, content: Vec<Block>) -> oneshot::Receiver<Result<TurnOutcome>> {
        let (reply, rx) = oneshot::channel();
        match context::expand_skill(&self.0.skills, content) {
            Ok(content) if !self.0.stop.is_cancelled() => {
                self.0.running.send_replace(true);
                if let Err(error) = self
                    .0
                    .tx
                    .send(Command::Submit(Queued { content, reply }, self.clone()))
                    && let Command::Submit(prompt, _) = error.0
                {
                    self.0.running.send_replace(false);
                    let _ = prompt.reply.send(Err(Error::Stopped));
                }
            }
            Ok(_) => {
                let _ = reply.send(Err(Error::Stopped));
            }
            Err(error) => {
                let _ = reply.send(Err(Error::Config(error)));
            }
        }
        rx
    }
    /// Runs or steers a turn, preserving its typed failure.
    pub async fn prompt(&self, content: Vec<Block>) -> Result<TurnOutcome> {
        self.submit(content).await.map_err(|_| Error::Stopped)?
    }
    /// Cancels a turn while retaining unconsumed steering.
    pub fn cancel(&self) {
        let _ = self.0.tx.send(Command::Cancel);
    }
    /// Cancels and rejects input when the transport closes.
    pub fn close(&self) {
        self.0.stop.cancel();
        self.0.extensions.close();
        let _ = self.0.tx.send(Command::Close);
    }
    /// Stops and reaps the session's extension processes.
    pub async fn shutdown_extensions(&self) {
        self.0.extensions.shutdown().await;
    }
    /// Resolves a pending ask, persisting workspace rules off the actor.
    pub async fn approve(
        &self,
        call: &ToolCallId,
        approved: bool,
        remember: Remember,
    ) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.0
            .tx
            .send(Command::Approve {
                call: call.clone(),
                approved,
                remember,
                reply,
            })
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }
    /// Moves to an existing event while idle.
    pub async fn checkout(&self, event: EventId) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.idle_operation(IdleCommand::Checkout(event, tx), rx)
            .await
    }
    /// Connects and selects a concrete model while idle.
    pub async fn set_model(&self, model: impl Into<ModelSelector>) -> Result<ModelRef> {
        let (tx, rx) = oneshot::channel();
        self.idle_operation(IdleCommand::Model(model.into(), tx), rx)
            .await
    }
    /// Selects a supported reasoning depth while idle.
    pub async fn set_reasoning(&self, reasoning: Reasoning) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.idle_operation(IdleCommand::Reasoning(reasoning, tx), rx)
            .await
    }
    /// Changes the live worker's tool sandbox while idle, recording an audit
    /// event without changing saved defaults or extension sandboxes.
    /// Reopening uses configuration rather than this override.
    pub async fn set_sandbox(&self, enabled: bool) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.idle_operation(IdleCommand::Sandbox(enabled, tx), rx)
            .await
    }
    pub(crate) async fn startup_extensions(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.idle_operation(IdleCommand::Startup(tx), rx).await
    }
    async fn idle_operation<T>(
        &self,
        command: IdleCommand,
        rx: oneshot::Receiver<Result<T>>,
    ) -> Result<T> {
        self.0
            .tx
            .send(Command::Idle(command))
            .map_err(|_| Error::Stopped)?;
        rx.await.map_err(|_| Error::Stopped)?
    }
    pub(crate) fn append(&self, state: &mut State, kind: EventKind) -> Result<()> {
        let event = state.log.append(state.head, kind)?.clone();
        state.head = event.id;
        self.0
            .snapshot
            .send_modify(|snapshot| snapshot.append(&event, state));
        self.publish_event(EventParams {
            session: self.id(),
            event,
        });
        Ok(())
    }
    fn publish_event(&self, params: EventParams) {
        if let Some(parent) = &self.0.parent_snapshot {
            parent.send_modify(|snapshot| snapshot.child(&params));
        }
        self.0.bus.send(Notification::Event(params));
    }
    async fn queued(&self) -> Vec<Queued> {
        let (reply, rx) = oneshot::channel();
        let _ = self.0.tx.send(Command::Drain(reply));
        rx.await.unwrap_or_default()
    }
    async fn has_queue(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        let _ = self.0.tx.send(Command::HasQueue(reply));
        rx.await.unwrap_or(false)
    }
    pub(crate) async fn title_updates(&self) -> Vec<title::Update> {
        let (reply, rx) = oneshot::channel();
        let _ = self.0.tx.send(Command::Titles(reply));
        rx.await.unwrap_or_default()
    }
    pub(crate) fn register_ask(&self, call: ToolCallId, pending: PendingAsk) {
        let _ = self.0.tx.send(Command::Register(call, pending));
    }
    pub(crate) fn remove_ask(&self, call: ToolCallId) {
        let _ = self.0.tx.send(Command::Remove(call));
    }
    pub(crate) fn publish_stream(&self, notification: Notification) {
        self.0
            .snapshot
            .send_modify(|snapshot| snapshot.stream(&notification));
        self.0.bus.send(notification);
    }
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

pub(crate) fn error_output(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: vec![OutputBlock::Text {
            text: message.into(),
        }],
        is_error: true,
        untrusted: false,
    }
}
