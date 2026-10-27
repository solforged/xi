//! The worker side of the daemon pipe: one session per worker process.
//!
//! Attach responses come from the kernel's published snapshot. The worker
//! forwards typed event and stream notifications without replaying the log or
//! changing stream sequence numbers. Session construction runs on a blocking
//! thread before async extension startup.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use xi_protocol::BoxFuture;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{
    ApprovalPolicy, ApprovalResolve, AttachResult, EventAppended, ModelEntry, ModelsResult,
    PromptResult, SessionAttach, SessionCancel, SessionCheckout, SessionClose, SessionCommand,
    SessionCommands, SessionCompact, SessionModels, SessionOptions, SessionPrompt, SessionRename,
    SessionSetModel, SessionSetReasoning, SessionSetSandbox, StreamDelta, StreamReset,
};
use xi_protocol::methods::worker::{
    LeaseParams, ModelLease, ModelRelease, WorkerStart, WorkerStartParams,
};
use xi_protocol::model::ModelRole;
use xi_protocol::rpc::{RequestId, RequestMethod, RpcError};
use xi_protocol::{Event, EventId, EventKind, SessionId};
use xi_provider::{Auth, LoginService, Registry};
use xi_rpc::{Incoming, Peer, decode_request, encode_result};
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
                    .map_err(|error| Error::Operation(error.to_string()))
                    .map(|()| Box::new(LeaseGuard { peer, params }) as Box<dyn Send>);
                let _ = reply.send(result);
            });
            granted.await.map_err(|_| Error::Stopped)?
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
    registry: Registry,
    credentials: PathBuf,
    /// Each role's model; the role configuration is fixed for this worker.
    roles: BTreeMap<ModelRole, ModelRef>,
}

impl Worker {
    fn models(&self) -> ModelsResult {
        let details = self.session.details();
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
                            xi_auth::chatgpt_file(&self.credentials).exists()
                        }
                        Auth::Login(LoginService::Anthropic) => {
                            xi_auth::anthropic_file(&self.credentials).exists()
                        }
                        Auth::Login(LoginService::Grok) => {
                            xi_auth::grok_file(&self.credentials).exists()
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
            current: details.model,
            reasoning: details.reasoning,
            models,
            roles: self.roles.clone(),
        }
    }

    fn attach(&self, since: Option<EventId>) -> RpcResult<AttachResult> {
        let mut snapshot = self.session.snapshot();
        if let Some(id) = since {
            let index = snapshot
                .events
                .iter()
                .position(|event| event.id == id)
                .ok_or_else(|| kernel_error(Error::UnknownEvent(id)))?;
            drop(snapshot.events.drain(..=index));
        }
        Ok(AttachResult {
            info: snapshot.details,
            events: snapshot.events,
            partial: snapshot.partial,
            stream_sequence: snapshot.stream_sequence,
            running: self.session.is_running(),
            pending_approvals: snapshot.pending_approvals,
            children: snapshot.children,
        })
    }
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
    let mut requests: JoinSet<(RequestId, RpcResult<Value>)> = JoinSet::new();
    let graceful = loop {
        tokio::select! {
            _ = peer.closed() => break false,
            notification = notifications.recv() => {
                if let Some(notification) = notification {
                    forward(&worker, &peer, notification);
                }
            }
            call = incoming.recv() => {
                let Some(call) = call else {
                    break false;
                };
                let Incoming::Request { id, method, params } = call else {
                    continue;
                };
                drain(&mut notifications, &worker, &peer);
                let Some(active) = worker.as_ref() else {
                    if method != WorkerStart::METHOD {
                        let _ = peer.respond(id, Err(RpcError::new(
                            RpcError::NOT_INITIALIZED, "worker/start is required",
                        )));
                        continue;
                    }
                    let result = match decode_request::<WorkerStart>(params) {
                        Ok(params) => start(params, &mut setup, &peer, &bus).await,
                        Err(error) => Err(error),
                    };
                    let result = match result {
                        Ok(active) => {
                            worker = Some(active);
                            drain(&mut notifications, &worker, &peer);
                            worker.as_ref().expect("worker was just started")
                                .attach(None).and_then(encode_result::<WorkerStart>)
                        }
                        Err(error) => Err(error),
                    };
                    let _ = peer.respond(id, result);
                    continue;
                };
                match method.as_str() {
                    SessionModels::METHOD => {
                        let result = decode_request::<SessionModels>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            encode_result::<SessionModels>(active.models())
                        });
                        let _ = peer.respond(id, result);
                    }
                    SessionCommands::METHOD => {
                        let result = decode_request::<SessionCommands>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            encode_result::<SessionCommands>(active.session.commands())
                        });
                        let _ = peer.respond(id, result);
                    }
                    SessionAttach::METHOD => {
                        let result = decode_request::<SessionAttach>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            active.attach(params.since)
                                .and_then(encode_result::<SessionAttach>)
                        });
                        let _ = peer.respond(id, result);
                    }
                    SessionClose::METHOD => {
                        let result = decode_request::<SessionClose>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            if active.session.is_running() {
                                return Err(kernel_error(Error::Running));
                            }
                            encode_result::<SessionClose>(())
                        });
                        let close = result.is_ok();
                        let _ = peer.respond(id, result);
                        if close {
                            break true;
                        }
                    }
                    SessionPrompt::METHOD => {
                        let prompt = decode_request::<SessionPrompt>(params).and_then(|params| {
                            check_session(&active.session, params.session)?;
                            Ok(active.session.submit(params.content))
                        });
                        match prompt {
                            Ok(prompt) => {
                                requests.spawn(async move {
                                    let result = async {
                                        let outcome = prompt.await
                                            .map_err(|_| kernel_error(Error::Stopped))?
                                            .map_err(kernel_error)?;
                                        encode_result::<SessionPrompt>(PromptResult {
                                            head: outcome.head,
                                            failed: outcome.failed,
                                            cancelled: outcome.cancelled,
                                            text: outcome.text,
                                        })
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
                    drain(&mut notifications, &worker, &peer);
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
            drain(&mut notifications, &worker, &peer);
            let _ = peer.respond(id, result);
        }
    }
    if let Some(active) = worker.as_ref() {
        active.session.shutdown_extensions().await;
        active.session.close();
        active.session.wait_idle().await;
    }
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
    let paths = setup.paths.clone();
    let backend = setup.backend.take();
    let peer = peer.clone();
    let bus = bus.clone();
    // Loading the log and constructing the session can probe the host and
    // access the filesystem, so neither runs on an async executor thread.
    let session = tokio::task::spawn_blocking(move || {
        let (options, cwd, log, model) = match params {
            WorkerStartParams::Create(params) => {
                validate_absolute(&params.cwd, "cwd")?;
                (params.options, params.cwd, None, params.model)
            }
            WorkerStartParams::Open(params) => {
                validate_absolute(&params.path, "session log path")?;
                validate_options(&params.options)?;
                let log =
                    SessionLog::open(params.path).map_err(|error| kernel_error(error.into()))?;
                let EventKind::SessionStarted { cwd, .. } = &log.root().kind else {
                    unreachable!("validated session root")
                };
                (params.options, cwd.clone(), Some(log), None)
            }
        };
        validate_options(&options)?;
        let mut config = session_config(&paths, &cwd, &options).map_err(kernel_error)?;
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
            leaser: Some(Arc::new(PeerLeaser(peer))),
            auto_title: options.auto_title,
        };
        match log {
            Some(log) => Session::from_log(log, config, bus, turn, backend),
            None => Session::create(config, options.session_dir.as_deref(), bus, turn, backend),
        }
        .map_err(kernel_error)
    })
    .await
    .map_err(|error| RpcError::new(RpcError::INTERNAL_ERROR, error.to_string()))??;
    session.startup_extensions().await.map_err(kernel_error)?;
    let config = session.config();
    Ok(Worker {
        session,
        registry: config.registry(),
        credentials: config.paths.credentials(),
        roles: config.resolved_roles(),
    })
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

async fn dispatch(session: Session, method: &str, params: Value) -> RpcResult<Value> {
    let value = match method {
        SessionCancel::METHOD => {
            let params = decode_request::<SessionCancel>(params)?;
            check_session(&session, params.session)?;
            session.cancel();
            encode_result::<SessionCancel>(())?
        }
        SessionCheckout::METHOD => {
            let params = decode_request::<SessionCheckout>(params)?;
            check_session(&session, params.session)?;
            session.checkout(params.event).await.map_err(kernel_error)?;
            encode_result::<SessionCheckout>(())?
        }
        SessionCompact::METHOD => {
            let params = decode_request::<SessionCompact>(params)?;
            check_session(&session, params.session)?;
            encode_result::<SessionCompact>(session.compact().await.map_err(kernel_error)?)?
        }
        SessionSetModel::METHOD => {
            let params = decode_request::<SessionSetModel>(params)?;
            check_session(&session, params.session)?;
            encode_result::<SessionSetModel>(
                session
                    .set_model(params.model)
                    .await
                    .map_err(kernel_error)?,
            )?
        }
        SessionSetReasoning::METHOD => {
            let params = decode_request::<SessionSetReasoning>(params)?;
            check_session(&session, params.session)?;
            session
                .set_reasoning(params.reasoning)
                .await
                .map_err(kernel_error)?;
            encode_result::<SessionSetReasoning>(())?
        }
        SessionSetSandbox::METHOD => {
            let params = decode_request::<SessionSetSandbox>(params)?;
            check_session(&session, params.session)?;
            session
                .set_sandbox(params.enabled)
                .await
                .map_err(kernel_error)?;
            encode_result::<SessionSetSandbox>(())?
        }
        SessionRename::METHOD => {
            let params = decode_request::<SessionRename>(params)?;
            check_session(&session, params.session)?;
            session.rename(&params.title).await.map_err(kernel_error)?;
            encode_result::<SessionRename>(())?
        }
        SessionCommand::METHOD => {
            let params = decode_request::<SessionCommand>(params)?;
            check_session(&session, params.session)?;
            encode_result::<SessionCommand>(
                session
                    .command(&params.name, &params.args)
                    .await
                    .map_err(kernel_error)?,
            )?
        }
        ApprovalResolve::METHOD => {
            let params = decode_request::<ApprovalResolve>(params)?;
            check_session(&session, params.session)?;
            session
                .approve(&params.call, params.approved, params.remember)
                .await
                .map_err(kernel_error)?;
            encode_result::<ApprovalResolve>(())?
        }
        _ => {
            return Err(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
    };
    Ok(value)
}

fn forward(worker: &Option<Worker>, peer: &Peer, notification: Notification) {
    let Some(active) = worker.as_ref() else {
        return;
    };
    match notification {
        // The worker-local bus contains this session and its children. Child
        // stream output stays local; clients draw children from durable events.
        Notification::Event(params) => {
            let _ = peer.notify::<EventAppended>(&params);
        }
        Notification::Delta(params) if params.session == active.session.id() => {
            let _ = peer.notify::<StreamDelta>(&params);
        }
        Notification::Reset(params) if params.session == active.session.id() => {
            let _ = peer.notify::<StreamReset>(&params);
        }
        _ => {}
    }
}

fn drain(
    notifications: &mut mpsc::UnboundedReceiver<Notification>,
    worker: &Option<Worker>,
    peer: &Peer,
) {
    while let Ok(notification) = notifications.try_recv() {
        forward(worker, peer, notification);
    }
}

fn check_session(session: &Session, id: SessionId) -> RpcResult<()> {
    if session.id() != id {
        return Err(RpcError::new(RpcError::UNKNOWN_SESSION, "unknown session"));
    }
    Ok(())
}

fn kernel_error(error: Error) -> RpcError {
    let code = match &error {
        Error::Running => RpcError::SESSION_RUNNING,
        Error::NotPending => RpcError::APPROVAL_NOT_PENDING,
        Error::Config(_)
        | Error::Model { .. }
        | Error::UnknownEvent(_)
        | Error::InvalidRemember(_) => RpcError::INVALID_PARAMS,
        Error::Session(error) if matches!(error.as_ref(), xi_session::Error::Locked(_)) => {
            RpcError::SESSION_HELD
        }
        Error::Extension(_) | Error::Stopped | Error::Tool(_) => RpcError::INTERNAL_ERROR,
        _ => RpcError::INTERNAL_ERROR,
    };
    RpcError::new(code, error.to_string())
}
