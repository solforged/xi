//! The worker side of the daemon pipe: one session per worker process.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{
    ApprovalPolicy, AttachParams, AttachResult, CheckoutParams, ChildEvents, Delta, EventAppended,
    EventParams, ModelEntry, ModelsResult, PendingApproval, PromptParams, PromptResult,
    RenameParams, ResolveParams, SessionCommandParams, SessionDetails, SessionOptions, SessionRef,
    SetModelParams, SetReasoningParams, StreamDelta, StreamReset,
};
use xi_protocol::methods::worker::{LeaseParams, ModelLease, ModelRelease, WorkerStartParams};
use xi_protocol::model::{ModelRole, Reasoning};
use xi_protocol::rpc::{RequestId, RpcError};
use xi_protocol::{Block, Decision, Event, EventId, EventKind, SessionId, ToolCallId};
use xi_provider::{Auth, LoginService, Registry};
use xi_rpc::{Incoming, Peer};
use xi_session::SessionLog;

use crate::config::{Config, Paths};
use crate::{
    ApprovalMode, Backend, Error, EventBus, Leaser, Notification, Result, Session, TurnOptions,
};

/// User paths and an optional injected backend for one worker.
pub struct WorkerSetup {
    /// User-owned configuration and storage locations.
    pub paths: Paths,
    /// Test/demo injection; `None` connects from session configuration.
    pub backend: Option<Backend>,
}

type RpcResult<T> = std::result::Result<T, RpcError>;

struct PeerLeaser(Peer);

impl Leaser for PeerLeaser {
    fn lease<'a>(&'a self, endpoint: &'a str) -> BoxFuture<'a, Result<Box<dyn Send>>> {
        Box::pin(async move {
            let params = LeaseParams {
                endpoint: endpoint.to_owned(),
            };
            let peer = self.0.clone();
            let (reply, granted) = oneshot::channel();
            // A cancelled waiter must still release a lease granted later.
            tokio::spawn(async move {
                let result = peer
                    .request::<ModelLease>(&params)
                    .await
                    .map_err(|error| Error::Config(error.to_string()))
                    .map(|()| Box::new(LeaseGuard { peer, params }) as Box<dyn Send>);
                let _ = reply.send(result);
            });
            granted
                .await
                .map_err(|_| Error::Config("lease task stopped".into()))?
        })
    }
}

struct LeaseGuard {
    peer: Peer,
    params: LeaseParams,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = self.peer.notify::<ModelRelease>(&self.params);
    }
}

struct Worker {
    session: Session,
    mirror: Mirror,
}

struct Mirror {
    info: SessionDetails,
    registry: Registry,
    credentials: PathBuf,
    /// Each role's model; config does not change while the worker runs.
    roles: BTreeMap<ModelRole, ModelRef>,
    initial_reasoning: Reasoning,
    events: Vec<Event>,
    index: HashMap<EventId, usize>,
    partial: Vec<Block>,
    stream_sequence: u64,
    tools: HashMap<ToolCallId, String>,
    pending: Vec<PendingApproval>,
    /// Children started since the last tool result, kept for attach.
    children: Vec<ChildEvents>,
}

impl Mirror {
    fn event(&mut self, event: &Event) {
        if self.index.contains_key(&event.id) {
            return;
        }
        self.info.head = event.id;
        match &event.kind {
            EventKind::AssistantMessage { content, .. } => {
                self.partial.clear();
                for block in content {
                    if let Block::ToolUse { id, name, .. } = block {
                        self.tools.insert(id.clone(), name.clone());
                    }
                }
            }
            EventKind::UserMessage { .. } => {
                self.partial.clear();
                self.children.clear();
            }
            EventKind::TurnFailed { .. } => {
                self.partial.clear();
            }
            EventKind::Compaction { .. } => self.children.clear(),
            EventKind::ModelChanged { model } => {
                self.info.model = model.clone();
                self.info.model_info = self.registry.model(model).cloned();
            }
            EventKind::ReasoningChanged { reasoning } => {
                self.info.reasoning = *reasoning;
            }
            EventKind::ToolDecision {
                call,
                action,
                decision: Decision::Ask,
                rule,
                taint,
                during,
                extension,
            } => {
                if !self.pending.iter().any(|pending| pending.call == *call) {
                    self.pending.push(PendingApproval {
                        session: self.info.session,
                        call: call.clone(),
                        tool: self
                            .tools
                            .get(during.as_ref().unwrap_or(call))
                            .cloned()
                            .or_else(|| extension.clone())
                            .unwrap_or_default(),
                        action: action.clone(),
                        rule: rule.clone(),
                        taint: *taint,
                        trusted: self.info.trusted,
                    });
                }
            }
            EventKind::ApprovalResolved { call, .. } => {
                self.pending.retain(|pending| pending.call != *call);
            }
            EventKind::ToolResult { call, .. } => {
                self.pending.retain(|pending| pending.call != *call);
                self.tools.remove(call);
                // The call these children ran under has its result.
                self.children.clear();
            }
            _ => {}
        }
        self.index.insert(event.id, self.events.len());
        self.events.push(event.clone());
    }

    /// Records an event from another session on this worker's bus. Returns
    /// whether it belongs to a child of this session.
    fn child(&mut self, params: &EventParams) -> bool {
        if let Some(child) = self
            .children
            .iter_mut()
            .find(|c| c.session == params.session)
        {
            child.events.push(params.event.clone());
            return true;
        }
        match &params.event.kind {
            EventKind::SessionStarted {
                parent_session: Some(parent),
                ..
            } if *parent == self.info.session => {
                self.children.push(ChildEvents {
                    session: params.session,
                    events: vec![params.event.clone()],
                });
                true
            }
            _ => false,
        }
    }

    fn checkout(&mut self, head: EventId) {
        self.info.head = head;
        self.info.reasoning = self.initial_reasoning;
        let mut model_found = false;
        let mut reasoning_found = false;
        let mut next = Some(head);
        while let Some(id) = next {
            let Some(event) = self.index.get(&id).map(|index| &self.events[*index]) else {
                break;
            };
            match &event.kind {
                EventKind::SessionStarted { model, .. } | EventKind::ModelChanged { model }
                    if !model_found =>
                {
                    self.info.model = model.clone();
                    self.info.model_info = self.registry.model(model).cloned();
                    model_found = true;
                }
                EventKind::ReasoningChanged { reasoning } if !reasoning_found => {
                    self.info.reasoning = *reasoning;
                    reasoning_found = true;
                }
                _ => {}
            }
            if model_found && reasoning_found {
                break;
            }
            next = event.parent;
        }
    }

    fn models(&self) -> ModelsResult {
        let mut models: Vec<_> = self
            .registry
            .models()
            .map(|info| ModelEntry {
                model: ModelRef {
                    provider: info.provider.clone(),
                    model: info.id.clone(),
                },
                available: self
                    .registry
                    .provider(&info.provider)
                    .is_some_and(|provider| match &provider.auth {
                        Auth::None => true,
                        Auth::Env(name) => {
                            std::env::var_os(name).is_some_and(|value| !value.is_empty())
                        }
                        Auth::File(path) => path.exists(),
                        Auth::Login(LoginService::Chatgpt) => {
                            xi_provider::chatgpt_file(&self.credentials).exists()
                        }
                        Auth::Login(LoginService::Anthropic) => {
                            xi_provider::anthropic_file(&self.credentials).exists()
                        }
                    }),
                info: info.clone(),
            })
            .collect();
        models.sort_by(|left, right| {
            (&left.model.provider, &left.model.model)
                .cmp(&(&right.model.provider, &right.model.model))
        });
        ModelsResult {
            current: self.info.model.clone(),
            reasoning: self.info.reasoning,
            models,
            roles: self.roles.clone(),
        }
    }

    fn attach(&self, since: Option<EventId>, running: bool) -> RpcResult<Value> {
        let start =
            match since {
                Some(id) => self.index.get(&id).map(|index| index + 1).ok_or_else(|| {
                    RpcError::new(RpcError::INVALID_PARAMS, "unknown replay event")
                })?,
                None => 0,
            };
        encode(AttachResult {
            info: self.info.clone(),
            events: self.events[start..].to_vec(),
            partial: self.partial.clone(),
            stream_sequence: self.stream_sequence,
            running,
            pending_approvals: self.pending.clone(),
            children: self.children.clone(),
        })
    }
}

enum Completion {
    Value(Value),
    Checkout(EventId),
}

/// Serves one session over a daemon pipe until the session closes or the pipe ends.
pub async fn run<R, W>(reader: R, writer: W, mut setup: WorkerSetup) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (peer, mut incoming) = Peer::spawn(reader, writer);
    let bus = EventBus::default();
    let mut notifications = bus.subscribe();
    let mut worker: Option<Worker> = None;
    let mut requests: JoinSet<(RequestId, RpcResult<Completion>)> = JoinSet::new();
    let graceful = loop {
        tokio::select! {
            _ = peer.closed() => break false,
            notification = notifications.recv() => {
                if let Some(notification) = notification {
                    mirror(&mut worker, &peer, notification);
                }
            }
            call = incoming.recv() => {
                let Some(call) = call else {
                    break false;
                };
                let Incoming::Request { id, method, params } = call else {
                    continue;
                };
                drain(&mut notifications, &mut worker, &peer);
                let Some(active) = worker.as_mut() else {
                    if method != "worker/start" {
                        let _ = peer.respond(id, Err(RpcError::new(
                            RpcError::NOT_INITIALIZED, "worker/start is required",
                        )));
                        continue;
                    }
                    let result = match decode(params) {
                        Ok(params) => start(params, &mut setup, &peer, &bus).await,
                        Err(error) => Err(error),
                    };
                    let result = match result {
                        Ok(active) => {
                            worker = Some(active);
                            drain(&mut notifications, &mut worker, &peer);
                            worker.as_ref().expect("worker was just started")
                                .mirror.attach(None, false)
                        }
                        Err(error) => Err(error),
                    };
                    let _ = peer.respond(id, result);
                    continue;
                };
                match method.as_str() {
                    "session/models" | "session/commands" => {
                        let result = decode::<SessionRef>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            if method == "session/models" {
                                encode(active.mirror.models())
                            } else {
                                encode(active.session.commands())
                            }
                        });
                        let _ = peer.respond(id, result);
                    }
                    "session/attach" => {
                        let result = decode::<AttachParams>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            active.mirror.attach(params.since, active.session.is_running())
                        });
                        let _ = peer.respond(id, result);
                    }
                    "session/close" => {
                        let result = decode::<SessionRef>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            if active.session.is_running() {
                                return Err(kernel_error(Error::Running));
                            }
                            Ok(Value::Null)
                        });
                        let close = result.is_ok();
                        let _ = peer.respond(id, result);
                        if close {
                            break true;
                        }
                    }
                    "session/prompt" => {
                        let prompt = decode::<PromptParams>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            Ok(active.session.submit(params.content))
                        });
                        match prompt {
                            Ok(prompt) => {
                                requests.spawn(async move {
                                    let result = async {
                                        let outcome = prompt.await
                                            .map_err(|_| kernel_error(Error::Config(
                                                "turn task stopped".into(),
                                            )))?
                                            .map_err(|error| kernel_error(Error::Config(error)))?;
                                        Ok(Completion::Value(encode(PromptResult {
                                            head: outcome.head,
                                            failed: outcome.failed,
                                            cancelled: outcome.cancelled,
                                            text: outcome.text,
                                        })?))
                                    }.await;
                                    (id, result)
                                });
                            }
                            Err(error) => {
                                let _ = peer.respond(id, Err(error));
                            }
                        }
                    }
                    _ => {
                        let session = active.session.clone();
                        requests.spawn(async move {
                            (id, dispatch(session, &method, params).await)
                        });
                    }
                }
            }
            completed = requests.join_next(), if !requests.is_empty() => {
                if let Some(Ok((id, result))) = completed {
                    drain(&mut notifications, &mut worker, &peer);
                    let result = result.map(|completion| match completion {
                        Completion::Value(value) => value,
                        Completion::Checkout(head) => {
                            if let Some(active) = worker.as_mut() {
                                active.mirror.checkout(head);
                            }
                            Value::Null
                        }
                    });
                    let _ = peer.respond(id, result);
                }
            }
        }
    };
    if let Some(active) = worker.as_ref() {
        if graceful {
            active.session.close();
        } else {
            active.session.stop_requests();
        }
        active.session.wait_idle().await;
    }
    // Requests still in flight, such as steering prompts rejected by close,
    // get their replies before the pipe closes.
    while let Some(completed) = requests.join_next().await {
        if let Ok((id, result)) = completed {
            drain(&mut notifications, &mut worker, &peer);
            let result = result.map(|completion| match completion {
                Completion::Value(value) => value,
                Completion::Checkout(_) => Value::Null,
            });
            let _ = peer.respond(id, result);
        }
    }
    bus.shutdown_extensions().await;
    peer.close();
    peer.closed().await;
    Ok(())
}

async fn start(
    params: WorkerStartParams,
    setup: &mut WorkerSetup,
    peer: &Peer,
    bus: &EventBus,
) -> RpcResult<Worker> {
    let (options, cwd, log, model) = match params {
        WorkerStartParams::Create(params) => {
            validate_absolute(&params.cwd, "cwd")?;
            (params.options, params.cwd, None, params.model)
        }
        WorkerStartParams::Open(params) => {
            validate_absolute(&params.path, "session log path")?;
            validate_options(&params.options)?;
            let log = SessionLog::open(params.path).map_err(|error| kernel_error(error.into()))?;
            let EventKind::SessionStarted { cwd, .. } = &log.root().kind else {
                unreachable!("validated session root")
            };
            (params.options, cwd.clone(), Some(log), None)
        }
    };
    validate_options(&options)?;
    let mut config = session_config(&setup.paths, &cwd, &options).map_err(kernel_error)?;
    if let Some(model) = model {
        config.model = config.resolve(&model);
    }
    let turn = TurnOptions {
        max_turns: options.max_turns.unwrap_or(200),
        approval: match options.approval {
            ApprovalPolicy::Client => ApprovalMode::Client,
            ApprovalPolicy::Allow => ApprovalMode::Headless(true),
            ApprovalPolicy::Deny => ApprovalMode::Headless(false),
        },
        leaser: Some(Arc::new(PeerLeaser(peer.clone()))),
        auto_title: options.auto_title,
    };
    let session = match log {
        Some(log) => Session::from_log(log, config, bus.clone(), turn, setup.backend.take()),
        None => Session::create(
            config,
            options.session_dir.as_deref(),
            bus.clone(),
            turn,
            setup.backend.take(),
        ),
    }
    .map_err(kernel_error)?;
    let mut state = session.0.state.lock().await;
    let mut commands = Vec::new();
    if !state.config.extensions.is_empty() {
        // Registration needs tool schemas, not a model connection or credential.
        let specs = match &state.backend {
            Some(backend) => Ok(std::borrow::Cow::Borrowed(backend.specs.as_slice())),
            None => crate::toolset(&state.config).map(|tools| {
                std::borrow::Cow::Owned(tools.iter().map(|tool| tool.spec()).collect())
            }),
        };
        let loaded = match specs {
            Ok(specs) => {
                bus.1
                    .startup(
                        &state.config,
                        &state.tools_context,
                        state.log.file_path(),
                        &specs,
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        match loaded {
            Ok((extensions, complete)) => {
                commands = extensions.commands().cloned().collect();
                // Incomplete startup catalogs retry through the normal use path.
                if complete {
                    state.extensions = Some(extensions);
                }
            }
            Err(error) => eprintln!("xi: extension startup: {error}"),
        }
    }
    *session.0.commands.lock().expect("command catalog") = commands;
    let registry = state.config.registry();
    let parent_session = match &state.log.root().kind {
        EventKind::SessionStarted { parent_session, .. } => *parent_session,
        _ => unreachable!("validated session root"),
    };
    let info = SessionDetails {
        session: session.id(),
        path: state.log.file_path().to_owned(),
        head: state.head,
        cwd: state.config.workspace.clone(),
        trusted: state.config.trusted,
        model: state.config.model.clone(),
        reasoning: state.config.reasoning,
        sandbox_enabled: state.config.sandbox.enabled,
        model_info: registry.model(&state.config.model).cloned(),
        parent_session,
    };
    let mut mirror = Mirror {
        info,
        registry,
        credentials: state.config.paths.credentials(),
        roles: state.config.resolved_roles(),
        initial_reasoning: state.initial_reasoning,
        events: Vec::with_capacity(state.log.events().len()),
        index: HashMap::new(),
        partial: Vec::new(),
        stream_sequence: 0,
        tools: HashMap::new(),
        pending: Vec::new(),
        children: Vec::new(),
    };
    for event in state.log.events() {
        mirror.event(event);
    }
    mirror.checkout(state.head);
    // Replaying the log must not overwrite an explicit open-time override.
    mirror.info.reasoning = state.config.reasoning;
    drop(state);
    Ok(Worker { session, mirror })
}

fn validate_options(options: &SessionOptions) -> RpcResult<()> {
    for path in options
        .configs
        .iter()
        .chain(options.session_dir.iter())
        .chain(options.extensions.iter())
    {
        validate_absolute(path, "session option path")?;
    }
    Ok(())
}

fn validate_absolute(path: &Path, name: &str) -> RpcResult<()> {
    if !path.is_absolute() {
        return Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            format!("{name} must be absolute: {}", path.display()),
        ));
    }
    Ok(())
}

/// Validates invocation configuration without starting a daemon or worker.
///
/// For a saved session, reads its root to find the workspace without locking
/// or modifying the log.
pub fn validate_config(paths: &Paths, params: &WorkerStartParams) -> Result<()> {
    match params {
        WorkerStartParams::Create(params) => {
            session_config(paths, &params.cwd, &params.options)?;
        }
        WorkerStartParams::Open(params) => {
            let mut line = String::new();
            std::io::BufReader::new(std::fs::File::open(&params.path)?).read_line(&mut line)?;
            let root: Event = serde_json::from_str(&line)
                .map_err(|error| Error::Config(format!("{}: {error}", params.path.display())))?;
            let EventKind::SessionStarted { cwd, .. } = root.kind else {
                return Err(Error::Config(format!(
                    "{}: session root is not session_started",
                    params.path.display(),
                )));
            };
            session_config(paths, &cwd, &params.options)?;
        }
    }
    Ok(())
}

fn session_config(paths: &Paths, cwd: &Path, options: &SessionOptions) -> Result<Config> {
    let mut config = Config::load(paths.clone(), cwd, &options.configs)?;
    if let Some(reasoning) = options.reasoning {
        config.reasoning = reasoning;
        config.reasoning_override = Some(reasoning);
    }
    if let Some(tools) = &options.tools {
        config.tools = tools.clone();
    }
    config
        .extensions
        .extend(options.extensions.iter().map(|path| paths.expand(path)));
    Ok(config)
}

async fn dispatch(session: Session, method: &str, params: Value) -> RpcResult<Completion> {
    let value = match method {
        "session/cancel" => {
            let params: SessionRef = decode(params)?;
            check_session(&session, params.session)?;
            session.cancel();
            Value::Null
        }
        "session/checkout" => {
            let params: CheckoutParams = decode(params)?;
            check_session(&session, params.session)?;
            session.checkout(params.event).await.map_err(kernel_error)?;
            return Ok(Completion::Checkout(params.event));
        }
        "session/compact" => {
            let params: SessionRef = decode(params)?;
            check_session(&session, params.session)?;
            encode(session.compact().await.map_err(kernel_error)?)?
        }
        "session/set_model" => {
            let params: SetModelParams = decode(params)?;
            check_session(&session, params.session)?;
            encode(
                session
                    .set_model(params.model)
                    .await
                    .map_err(kernel_error)?,
            )?
        }
        "session/set_reasoning" => {
            let params: SetReasoningParams = decode(params)?;
            check_session(&session, params.session)?;
            session
                .set_reasoning(params.reasoning)
                .await
                .map_err(kernel_error)?;
            Value::Null
        }
        "session/rename" => {
            let params: RenameParams = decode(params)?;
            check_session(&session, params.session)?;
            session.rename(&params.title).await.map_err(kernel_error)?;
            Value::Null
        }
        "session/command" => {
            let params: SessionCommandParams = decode(params)?;
            check_session(&session, params.session)?;
            encode(
                session
                    .command(&params.name, &params.args)
                    .await
                    .map_err(kernel_error)?,
            )?
        }
        "approval/resolve" => {
            let params: ResolveParams = decode(params)?;
            check_session(&session, params.session)?;
            session
                .approve(&params.call, params.approved, params.remember)
                .map_err(kernel_error)?;
            Value::Null
        }
        _ => {
            return Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
    };
    Ok(Completion::Value(value))
}

fn mirror(worker: &mut Option<Worker>, peer: &Peer, notification: Notification) {
    let Some(active) = worker.as_mut() else {
        return;
    };
    match notification {
        Notification::Event(params) if params.session == active.session.id() => {
            active.mirror.event(&params.event);
            let _ = peer.notify::<EventAppended>(&params);
        }
        Notification::Delta(mut params) if params.session == active.session.id() => {
            active.mirror.stream_sequence += 1;
            params.sequence = active.mirror.stream_sequence;
            match &params.delta {
                Delta::Text { text } => {
                    crate::push_partial(&mut active.mirror.partial, text, false)
                }
                Delta::Thinking { text } => {
                    crate::push_partial(&mut active.mirror.partial, text, true)
                }
                Delta::ToolInput { .. } => {}
            }
            let _ = peer.notify::<StreamDelta>(&params);
        }
        Notification::Reset(mut params) if params.session == active.session.id() => {
            active.mirror.stream_sequence += 1;
            params.sequence = active.mirror.stream_sequence;
            active.mirror.partial.clear();
            let _ = peer.notify::<StreamReset>(&params);
        }
        // Only the session's children share its bus. Their stream deltas
        // stay in the worker; clients draw children from events.
        Notification::Event(params) if active.mirror.child(&params) => {
            let _ = peer.notify::<EventAppended>(&params);
        }
        _ => {}
    }
}

fn drain(
    notifications: &mut mpsc::UnboundedReceiver<Notification>,
    worker: &mut Option<Worker>,
    peer: &Peer,
) {
    while let Ok(notification) = notifications.try_recv() {
        mirror(worker, peer, notification);
    }
}

fn check_session(session: &Session, id: SessionId) -> RpcResult<()> {
    if session.id() != id {
        return Err(RpcError::new(RpcError::UNKNOWN_SESSION, "unknown session"));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(params: Value) -> RpcResult<T> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn encode<T: Serialize>(value: T) -> RpcResult<Value> {
    serde_json::to_value(value)
        .map_err(|error| RpcError::new(RpcError::INTERNAL_ERROR, error.to_string()))
}

fn kernel_error(error: Error) -> RpcError {
    let code = match &error {
        Error::Running => RpcError::SESSION_RUNNING,
        Error::NotPending => RpcError::APPROVAL_NOT_PENDING,
        Error::InvalidRemember(_) => RpcError::INVALID_PARAMS,
        Error::Session(xi_session::Error::Locked(_)) => RpcError::SESSION_HELD,
        _ => RpcError::INTERNAL_ERROR,
    };
    RpcError::new(code, error.to_string())
}
