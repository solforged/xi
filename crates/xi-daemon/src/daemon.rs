//! Client routing, worker supervision and the daemon's session index.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use xi_rpc::{Incoming, Peer, PeerError};
use xi_kernel::config::{DaemonSettings, Paths, sessions_root};
use xi_protocol::event::EventKind;
use xi_protocol::methods::{client::*, worker::*};
use xi_protocol::rpc::{RequestId, RpcError};
use xi_protocol::{Block, Decision, Event, EventId, SessionId, Timestamp, ToolCallId};

use crate::scheduler::Scheduler;
use crate::spawn::{Spawner, WorkerProcess};

/// A cloneable handle to a daemon's connection router.
#[derive(Clone)]
pub struct Daemon {
    commands: mpsc::UnboundedSender<Command>,
    finished: watch::Receiver<bool>,
}

/// Configuration and worker factory for one daemon instance.
pub struct DaemonOptions {
    /// Global daemon lifetime, notification and endpoint limit settings.
    pub settings: DaemonSettings,
    /// User-owned configuration and persistence directories.
    pub paths: Paths,
    /// Factory for isolated session workers.
    pub spawner: Arc<dyn Spawner>,
    /// Whether the last client disconnect shuts the daemon down immediately.
    pub private: bool,
}

impl Daemon {
    /// Starts the router and its idle-lifetime timer.
    pub fn new(options: DaemonOptions) -> Daemon {
        let (commands, incoming) = mpsc::unbounded_channel();
        let (finished, ended) = watch::channel(false);
        let actor = Router {
            scheduler: Scheduler::new(&options.settings.limit),
            options,
            commands: commands.clone(),
            clients: HashMap::new(),
            workers: HashMap::new(),
            starts: HashMap::new(),
            inbox: HashMap::new(),
            next_id: 1,
            started: Instant::now(),
            idle_since: Instant::now(),
            forwards: 0,
            shutdown: None,
            exe_changed: false,
            config_edits: Arc::new(Mutex::new(())),
        };
        tokio::spawn(actor.run(incoming, finished));
        Daemon {
            commands,
            finished: ended,
        }
    }

    /// Serves one client until its pipe closes or the daemon shuts down.
    /// Disconnecting detaches without cancellation, except for the last private client.
    pub async fn serve<R, W>(&self, reader: R, writer: W)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (peer, mut incoming) = Peer::spawn(reader, writer);
        let (reply, receiver) = oneshot::channel();
        if self
            .commands
            .send(Command::Connect {
                peer: peer.clone(),
                reply,
            })
            .is_err()
        {
            peer.close();
            return;
        }
        let Ok(client) = receiver.await else {
            peer.close();
            return;
        };
        loop {
            tokio::select! {
                _ = self.finished() => break,
                _ = peer.closed() => break,
                call = incoming.recv() => match call {
                    Some(Incoming::Request { id, method, params }) => {
                        if self.commands.send(Command::Request { client, id, method, params }).is_err() {
                            break;
                        }
                    }
                    Some(Incoming::Notification { .. }) => {}
                    None => break,
                },
            }
        }
        let _ = self.commands.send(Command::Disconnect(client));
        peer.close();
    }

    /// Resolves when explicit shutdown, idle expiry or executable change ends the daemon.
    pub async fn finished(&self) {
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|finished| *finished).await;
    }

    pub(crate) fn executable_changed(&self) {
        let _ = self.commands.send(Command::ExecutableChanged);
    }
}

type Reply = (Peer, RequestId);

struct Connection {
    peer: Peer,
    initialized: bool,
    attached: HashSet<SessionId>,
    watches_running: bool,
}

struct Start {
    client: u64,
    request: RequestId,
    params: WorkerStartParams,
    path: Option<PathBuf>,
}

struct Worker {
    peer: Peer,
    kill: Option<Box<dyn FnOnce() + Send>>,
    info: Option<SessionDetails>,
    start_reply: Option<(u64, RequestId)>,
    path: Option<PathBuf>,
    buffered: Vec<(String, Value)>,
    tools: HashMap<ToolCallId, String>,
    approval: ApprovalPolicy,
    compactions: usize,
    checking_idle: bool,
    idle_since: Instant,
    activity: u64,
    closing: bool,
    close_deadline: Option<Instant>,
    close_replies: Vec<(u64, RequestId)>,
}

impl Worker {
    fn reset_idle(&mut self) {
        self.idle_since = Instant::now();
        self.activity = self.activity.wrapping_add(1);
    }

    fn begin_close(&mut self, key: u64, commands: &mpsc::UnboundedSender<Command>) {
        self.closing = true;
        let deadline = Instant::now() + Duration::from_secs(5);
        self.close_deadline = Some(deadline);
        let commands = commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let _ = commands.send(Command::CloseExpired {
                worker: key,
                deadline,
            });
        });
    }
}

enum Command {
    Connect {
        peer: Peer,
        reply: oneshot::Sender<u64>,
    },
    Disconnect(u64),
    Request {
        client: u64,
        id: RequestId,
        method: String,
        params: Value,
    },
    Spawned {
        worker: u64,
        process: std::io::Result<WorkerProcess>,
    },
    Started {
        worker: u64,
        result: Result<Box<AttachResult>, RpcError>,
    },
    Forwarded {
        client: Option<u64>,
        id: Option<RequestId>,
        worker: u64,
        method: String,
        checkout: Option<EventId>,
        result: Result<Value, RpcError>,
    },
    WorkerCall {
        worker: u64,
        call: Incoming,
    },
    Exited {
        worker: u64,
        status: Option<i32>,
    },
    IdleChecked {
        worker: u64,
        activity: u64,
        result: Result<bool, RpcError>,
    },
    CloseExpired {
        worker: u64,
        deadline: Instant,
    },
    ExecutableChanged,
}

enum WorkerNotification {
    Event(EventParams),
    Delta(DeltaParams),
    Reset(ResetParams),
}

impl WorkerNotification {
    fn session(&self) -> SessionId {
        match self {
            Self::Event(params) => params.session,
            Self::Delta(params) => params.session,
            Self::Reset(params) => params.session,
        }
    }

    fn send(&self, peer: &Peer) {
        let _ = match self {
            Self::Event(params) => peer.notify::<EventAppended>(params),
            Self::Delta(params) => peer.notify::<StreamDelta>(params),
            Self::Reset(params) => peer.notify::<StreamReset>(params),
        };
    }
}

struct Router {
    options: DaemonOptions,
    commands: mpsc::UnboundedSender<Command>,
    clients: HashMap<u64, Connection>,
    workers: HashMap<u64, Worker>,
    starts: HashMap<u64, Start>,
    inbox: HashMap<(SessionId, ToolCallId), (Timestamp, PendingApproval)>,
    scheduler: Scheduler<Reply>,
    next_id: u64,
    started: Instant,
    idle_since: Instant,
    forwards: usize,
    shutdown: Option<ShutdownWhen>,
    exe_changed: bool,
    config_edits: Arc<Mutex<()>>,
}

impl Router {
    async fn run(
        mut self,
        mut incoming: mpsc::UnboundedReceiver<Command>,
        finished: watch::Sender<bool>,
    ) {
        let idle = Duration::from_secs(self.options.settings.idle_exit);
        let period = (idle / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
        let mut timer = tokio::time::interval(period);
        loop {
            let was_busy =
                !self.workers.is_empty() || !self.starts.is_empty() || !self.clients.is_empty();
            tokio::select! {
                command = incoming.recv() => match command {
                    Some(command) => self.handle(command),
                    None => break,
                },
                _ = timer.tick() => self.tick(idle),
            }
            let empty = self.workers.is_empty() && self.starts.is_empty();
            if empty
                && self.forwards == 0
                && (self.shutdown.is_some() || (self.exe_changed && self.clients.is_empty()))
            {
                break;
            }
            if was_busy || !empty || !self.clients.is_empty() {
                self.idle_since = Instant::now();
            }
        }
        for worker in self.workers.values_mut() {
            if let Some(kill) = worker.kill.take() {
                kill();
            }
            worker.peer.close();
        }
        for client in self.clients.values() {
            client.peer.close();
        }
        finished.send_replace(true);
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Connect { peer, reply } => {
                let id = self.next_id;
                self.next_id += 1;
                self.clients.insert(
                    id,
                    Connection {
                        peer,
                        initialized: false,
                        attached: HashSet::new(),
                        watches_running: false,
                    },
                );
                let _ = reply.send(id);
            }
            Command::Disconnect(client) => {
                if let Some(connection) = self.clients.remove(&client) {
                    for session in connection.attached {
                        if !self.attached(session)
                            && let Some(worker) = self
                                .worker_id(session)
                                .and_then(|id| self.workers.get_mut(&id))
                        {
                            worker.reset_idle();
                        }
                    }
                }
                if self.options.private && self.clients.is_empty() {
                    self.begin_shutdown(ShutdownWhen::Now);
                }
            }
            Command::Request {
                client,
                id,
                method,
                params,
            } => {
                if let Err(error) = self.request(client, id.clone(), &method, params) {
                    self.respond(client, id, Err(error));
                }
            }
            Command::Spawned { worker, process } => self.spawned(worker, process),
            Command::Started { worker, result } => self.worker_started(worker, result.map(|a| *a)),
            Command::Forwarded {
                client,
                id,
                worker,
                method,
                checkout,
                result,
            } => {
                self.forwarded(client, id, worker, &method, checkout, result);
            }
            Command::WorkerCall { worker, call } => self.worker_call(worker, call),
            Command::IdleChecked {
                worker,
                activity,
                result,
            } => self.idle_checked(worker, activity, result),
            Command::Exited { worker, status } => self.worker_exited(worker, status),
            Command::CloseExpired { worker, deadline } => {
                if let Some(worker) = self.workers.get_mut(&worker)
                    && worker.close_deadline == Some(deadline)
                {
                    worker.close_deadline = None;
                    if let Some(kill) = worker.kill.take() {
                        kill();
                    }
                    worker.peer.close();
                }
            }
            Command::ExecutableChanged => self.exe_changed = true,
        }
    }

    fn request(
        &mut self,
        client: u64,
        id: RequestId,
        method: &str,
        params: Value,
    ) -> Result<(), RpcError> {
        let Some(connection) = self.clients.get(&client) else {
            return Ok(());
        };
        if method == "initialize" {
            let params: InitializeParams = decode(params)?;
            if params.protocol_version != xi_protocol::PROTOCOL_VERSION {
                return Err(RpcError::with_data(
                    RpcError::VERSION_MISMATCH,
                    "protocol version mismatch",
                    json!({
                        "daemon_protocol_version": xi_protocol::PROTOCOL_VERSION,
                    }),
                ));
            }
            if let Some(connection) = self.clients.get_mut(&client) {
                connection.initialized = true;
            }
            self.respond(
                client,
                id,
                encode(InitializeResult {
                    protocol_version: xi_protocol::PROTOCOL_VERSION,
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    capabilities: Capabilities {
                        private: self.options.private,
                    },
                }),
            );
            return Ok(());
        }
        if !connection.initialized {
            return Err(RpcError::new(
                RpcError::NOT_INITIALIZED,
                "initialize required",
            ));
        }
        match method {
            "session/create" => {
                self.start(client, id, WorkerStartParams::Create(decode(params)?))?
            }
            "session/open" => self.start(client, id, WorkerStartParams::Open(decode(params)?))?,
            "session/attach" => {
                let attach: AttachParams = decode_ref(&params)?;
                let worker = self.require_worker(attach.session)?;
                if let Some(connection) = self.clients.get_mut(&client) {
                    connection.attached.insert(attach.session);
                }
                self.forward(Some(client), Some(id), worker, method, params);
            }
            "session/detach" => {
                let session: SessionRef = decode(params)?;
                if let Some(connection) = self.clients.get_mut(&client) {
                    connection.attached.remove(&session.session);
                }
                if !self.attached(session.session)
                    && let Some(worker) = self
                        .worker_id(session.session)
                        .and_then(|id| self.workers.get_mut(&id))
                {
                    worker.reset_idle();
                }
                self.respond(client, id, Ok(Value::Null));
            }
            "session/list" => {
                let filters: ListParams = decode(params)?;
                if filters.running == Some(true)
                    && let Some(connection) = self.clients.get_mut(&client)
                {
                    connection.watches_running = true;
                }
                let mut live = self.summaries();
                let running: Vec<_> = self
                    .workers
                    .values()
                    .filter_map(|worker| {
                        let info = worker.info.as_ref()?;
                        (!worker.closing)
                            .then(|| (info.session, worker_running(&worker.peer, info)))
                    })
                    .collect();
                let root = sessions_root(&self.options.paths);
                let Some(peer) = self
                    .clients
                    .get(&client)
                    .map(|connection| connection.peer.clone())
                else {
                    return Ok(());
                };
                tokio::spawn(async move {
                    let result = async {
                        let running = futures::future::try_join_all(running.into_iter().map(
                            |(session, reply)| async move {
                                Ok::<_, RpcError>((session, reply?.await?))
                            },
                        ))
                        .await?
                        .into_iter()
                        .collect::<HashMap<_, _>>();
                        for summary in &mut live {
                            if let Some(running) = running.get(&summary.session) {
                                summary.running = *running;
                            }
                        }
                        tokio::task::spawn_blocking(move || list_sessions(&root, live, filters))
                            .await
                            .map_err(internal)?
                            .and_then(encode)
                    }
                    .await;
                    let _ = peer.respond(id, result);
                });
            }
            "config/settings" => {
                let params: SettingsParams = decode(params)?;
                settings_paths(&params.cwd, &params.configs)?;
                let paths = self.options.paths.clone();
                let Some(peer) = self
                    .clients
                    .get(&client)
                    .map(|connection| connection.peer.clone())
                else {
                    return Ok(());
                };
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        xi_kernel::settings::report(&paths, &params.cwd, &params.configs)
                            .map_err(internal)
                    })
                    .await
                    .map_err(internal)
                    .and_then(|result| result)
                    .and_then(encode);
                    let _ = peer.respond(id, result);
                });
            }
            "config/set" => {
                let params: SetSettingParams = decode(params)?;
                settings_paths(&params.cwd, &params.configs)?;
                let paths = self.options.paths.clone();
                let edits = self.config_edits.clone();
                let Some(peer) = self
                    .clients
                    .get(&client)
                    .map(|connection| connection.peer.clone())
                else {
                    return Ok(());
                };
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let _guard = edits.lock().map_err(internal)?;
                        xi_kernel::settings::set_global(&paths, &params.key, &params.value)
                            .map_err(|error| match error {
                                xi_kernel::Error::Config(message) => {
                                    RpcError::new(RpcError::INVALID_PARAMS, message)
                                }
                                error => internal(error),
                            })?;
                        xi_kernel::settings::report(&paths, &params.cwd, &params.configs)
                            .map_err(internal)
                    })
                    .await
                    .map_err(internal)
                    .and_then(|result| result)
                    .and_then(encode);
                    let _ = peer.respond(id, result);
                });
            }
            "approval/list" => {
                decode::<()>(params)?;
                let mut approvals: Vec<_> = self.inbox.values().cloned().collect();
                approvals.sort_by(|(at, a), (bt, b)| {
                    (at, a.session, &a.call).cmp(&(bt, b.session, &b.call))
                });
                self.respond(
                    client,
                    id,
                    encode(
                        approvals
                            .into_iter()
                            .map(|(_, approval)| approval)
                            .collect::<Vec<_>>(),
                    ),
                );
            }
            "daemon/status" => {
                decode::<()>(params)?;
                self.respond(
                    client,
                    id,
                    encode(StatusResult {
                        version: env!("CARGO_PKG_VERSION").into(),
                        pid: std::process::id(),
                        uptime_secs: self.started.elapsed().as_secs(),
                        workers: self.workers.len() + self.starts.len(),
                        clients: self.clients.len(),
                        leases: self.scheduler.status(),
                    }),
                );
            }
            "daemon/shutdown" => {
                let shutdown: ShutdownParams = decode(params)?;
                self.respond(client, id, Ok(Value::Null));
                self.begin_shutdown(shutdown.when);
            }
            "session/close"
            | "session/prompt"
            | "session/cancel"
            | "session/checkout"
            | "session/compact"
            | "session/set_model"
            | "session/set_reasoning"
            | "session/models"
            | "session/commands"
            | "session/command"
            | "approval/resolve" => {
                let session: SessionRef = decode_ref(&params)?;
                let worker = self.require_worker(session.session)?;
                self.forward(Some(client), Some(id), worker, method, params);
            }
            _ => return Err(RpcError::new(RpcError::METHOD_NOT_FOUND, "unknown method")),
        }
        Ok(())
    }

    fn start(
        &mut self,
        client: u64,
        request: RequestId,
        params: WorkerStartParams,
    ) -> Result<(), RpcError> {
        let (name, path) = match &params {
            WorkerStartParams::Create(create) => ("cwd", &create.cwd),
            WorkerStartParams::Open(open) => ("log path", &open.path),
        };
        if !path.is_absolute() {
            return Err(RpcError::new(
                RpcError::INVALID_PARAMS,
                format!("session {name} must be absolute: {}", path.display()),
            ));
        }
        if self.shutdown.is_some() {
            return Err(RpcError::new(
                RpcError::WORKER_EXITED,
                "daemon is shutting down",
            ));
        }
        let path = match &params {
            WorkerStartParams::Open(open) => {
                let path = open.path.canonicalize().map_err(internal)?;
                if let Some(worker) = self
                    .workers
                    .values()
                    .find(|worker| worker.path.as_ref() == Some(&path))
                {
                    let data = worker
                        .info
                        .as_ref()
                        .map_or(Value::Null, |info| json!({ "session": info.session }));
                    return Err(RpcError::with_data(
                        RpcError::SESSION_HELD,
                        "session is already open",
                        data,
                    ));
                }
                if self
                    .starts
                    .values()
                    .any(|start| start.path.as_ref() == Some(&path))
                {
                    return Err(RpcError::with_data(
                        RpcError::SESSION_HELD,
                        "session is opening",
                        Value::Null,
                    ));
                }
                Some(path)
            }
            WorkerStartParams::Create(_) => None,
        };
        let worker = self.next_id;
        self.next_id += 1;
        self.starts.insert(
            worker,
            Start {
                client,
                request,
                params,
                path,
            },
        );
        let spawner = self.options.spawner.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let process = spawner.spawn().await;
            let _ = commands.send(Command::Spawned { worker, process });
        });
        Ok(())
    }

    fn spawned(&mut self, key: u64, process: std::io::Result<WorkerProcess>) {
        let Some(start) = self.starts.remove(&key) else {
            return;
        };
        let process = match process {
            Ok(process) => process,
            Err(error) => {
                self.respond(start.client, start.request, Err(internal(error)));
                return;
            }
        };
        let WorkerProcess {
            reader,
            writer,
            mut exited,
            kill,
        } = process;
        let (peer, mut incoming) = Peer::spawn(reader, writer);
        self.workers.insert(
            key,
            Worker {
                peer: peer.clone(),
                kill: Some(kill),
                info: None,
                start_reply: Some((start.client, start.request)),
                path: start.path,
                buffered: Vec::new(),
                tools: HashMap::new(),
                approval: match &start.params {
                    WorkerStartParams::Create(params) => params.options.approval,
                    WorkerStartParams::Open(params) => params.options.approval,
                },
                compactions: 0,
                checking_idle: false,
                idle_since: Instant::now(),
                activity: 0,
                closing: false,
                close_deadline: None,
                close_replies: Vec::new(),
            },
        );
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let mut status = None;
            let mut did_exit = false;
            loop {
                tokio::select! {
                    result = &mut exited, if !did_exit => { status = result; did_exit = true; }
                    call = incoming.recv() => match call {
                        Some(call) => { let _ = commands.send(Command::WorkerCall { worker: key, call }); }
                        None => break,
                    },
                }
            }
            if !did_exit {
                status = exited.await;
            }
            let _ = commands.send(Command::Exited {
                worker: key,
                status,
            });
        });
        if self.shutdown == Some(ShutdownWhen::Now) {
            if let Some(worker) = self.workers.get_mut(&key) {
                if let Some(kill) = worker.kill.take() {
                    kill();
                }
                worker.peer.close();
            }
            return;
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = peer
                .request::<WorkerStart>(&start.params)
                .await
                .map(Box::new)
                .map_err(peer_error);
            let _ = commands.send(Command::Started {
                worker: key,
                result,
            });
        });
    }

    fn worker_started(&mut self, key: u64, result: Result<AttachResult, RpcError>) {
        let Some(worker) = self.workers.get_mut(&key) else {
            return;
        };
        let Some((client, request)) = worker.start_reply.take() else {
            return;
        };
        match result {
            Ok(mut result) => {
                worker.path = result
                    .info
                    .path
                    .canonicalize()
                    .ok()
                    .or_else(|| Some(result.info.path.clone()));
                worker.info = Some(result.info.clone());
                for event in &result.events {
                    remember_tools(&mut worker.tools, event);
                }
                let buffered = std::mem::take(&mut worker.buffered);
                if let Some(connection) = self.clients.get_mut(&client) {
                    connection.attached.insert(result.info.session);
                }
                if worker.approval == ApprovalPolicy::Client {
                    for pending in &result.pending_approvals {
                        let at = result.events.iter().find_map(|event| {
                            matches!(&event.kind, EventKind::ToolDecision { call, .. } if call == &pending.call)
                                .then_some(event.at)
                        }).unwrap_or(Timestamp(0));
                        self.inbox.insert(
                            (pending.session, pending.call.clone()),
                            (at, pending.clone()),
                        );
                    }
                } else {
                    result.pending_approvals.clear();
                }
                self.respond(client, request, encode(result));
                for (method, params) in buffered {
                    self.notification(key, &method, params);
                }
            }
            Err(mut error) => {
                if error.code == RpcError::SESSION_HELD {
                    error.data = Some(Value::Null);
                }
                if let Some(kill) = worker.kill.take() {
                    kill();
                }
                worker.peer.close();
                self.respond(client, request, Err(error));
            }
        }
    }

    fn forward(
        &mut self,
        client: Option<u64>,
        id: Option<RequestId>,
        key: u64,
        method: &str,
        params: Value,
    ) {
        let Some(worker) = self.workers.get_mut(&key) else {
            return;
        };
        if method == "session/compact" {
            worker.compactions += 1;
        }
        if matches!(method, "session/prompt" | "session/compact") {
            worker.reset_idle();
        }
        if method == "session/close" {
            worker.begin_close(key, &self.commands);
        }
        self.forwards += 1;
        let commands = self.commands.clone();
        let checkout = if method == "session/checkout" {
            params
                .get("event")
                .and_then(|value| decode_ref::<EventId>(value).ok())
        } else {
            None
        };
        let reply = worker.peer.start_request_value(method, params);
        let method = method.to_owned();
        tokio::spawn(async move {
            let result = match reply {
                Ok(reply) => reply.await.map_err(peer_error),
                Err(error) => Err(peer_error(error)),
            };
            let _ = commands.send(Command::Forwarded {
                client,
                id,
                worker: key,
                method,
                checkout,
                result,
            });
        });
    }

    fn forwarded(
        &mut self,
        client: Option<u64>,
        id: Option<RequestId>,
        key: u64,
        method: &str,
        checkout: Option<EventId>,
        result: Result<Value, RpcError>,
    ) {
        self.forwards = self.forwards.saturating_sub(1);
        let mut wait_exit = false;
        if let Some(worker) = self.workers.get_mut(&key) {
            if method == "session/compact" {
                worker.compactions = worker.compactions.saturating_sub(1);
            }
            if matches!(method, "session/prompt" | "session/compact") {
                worker.reset_idle();
            }
            if result.is_ok() {
                if method == "session/attach" {
                    if let Some(info) = result
                        .as_ref()
                        .ok()
                        .and_then(|value| value.get("info"))
                        .and_then(|value| decode_ref::<SessionDetails>(value).ok())
                    {
                        worker.info = Some(info);
                    }
                } else if method == "session/checkout" {
                    if let (Some(info), Some(head)) = (&mut worker.info, checkout) {
                        info.head = head;
                    }
                } else if matches!(method, "session/prompt" | "session/compact")
                    && let Some(head) = result
                        .as_ref()
                        .ok()
                        .and_then(|value| value.get("head"))
                        .and_then(|head| decode_ref::<EventId>(head).ok())
                    && let Some(info) = &mut worker.info
                {
                    info.head = head;
                }
            }
            if method == "session/close" {
                if result.is_ok() {
                    worker.begin_close(key, &self.commands);
                } else if worker.kill.is_some() && self.shutdown != Some(ShutdownWhen::Now) {
                    worker.closing = false;
                    worker.close_deadline = None;
                }
                if result.is_ok() {
                    if let (Some(client), Some(id)) = (client, id.as_ref()) {
                        worker.close_replies.push((client, id.clone()));
                    }
                    wait_exit = true;
                } else {
                    worker.reset_idle();
                }
            }
        }
        if !wait_exit && let (Some(client), Some(id)) = (client, id) {
            self.respond(client, id, result);
        }
    }

    fn worker_call(&mut self, key: u64, call: Incoming) {
        match call {
            Incoming::Request { id, method, params } => {
                let Some(peer) = self.workers.get(&key).map(|worker| worker.peer.clone()) else {
                    return;
                };
                if method != "model/lease" {
                    let _ = peer.respond(
                        id,
                        Err(RpcError::new(
                            RpcError::METHOD_NOT_FOUND,
                            "unknown worker method",
                        )),
                    );
                    return;
                }
                match decode::<LeaseParams>(params) {
                    Ok(params) => grant(self.scheduler.acquire(key, params.endpoint, (peer, id))),
                    Err(error) => {
                        let _ = peer.respond(id, Err(error));
                    }
                }
            }
            Incoming::Notification { method, params } => {
                if method == "model/release" {
                    if let Ok(params) = decode::<LeaseParams>(params) {
                        grant(self.scheduler.release(key, &params.endpoint));
                    }
                } else if matches!(
                    method.as_str(),
                    "event/appended" | "stream/delta" | "stream/reset"
                ) {
                    let Some(worker) = self.workers.get_mut(&key) else {
                        return;
                    };
                    if worker.info.is_none() {
                        worker.buffered.push((method, params));
                    } else {
                        self.notification(key, &method, params);
                    }
                }
            }
        }
    }

    fn notification(&mut self, key: u64, method: &str, params: Value) {
        let Some(session) = self
            .workers
            .get(&key)
            .and_then(|worker| worker.info.as_ref())
            .map(|info| info.session)
        else {
            return;
        };
        let notification = match method {
            "event/appended" => decode(params).map(WorkerNotification::Event),
            "stream/delta" => decode(params).map(WorkerNotification::Delta),
            "stream/reset" => decode(params).map(WorkerNotification::Reset),
            _ => return,
        };
        let Ok(notification) = notification else {
            return;
        };
        if notification.session() != session {
            // A worker forwards other sessions' events only for its own
            // session's children; clients attached to the parent draw them.
            if matches!(notification, WorkerNotification::Event(_)) {
                for connection in self.clients.values() {
                    if connection.initialized && connection.attached.contains(&session) {
                        notification.send(&connection.peer);
                    }
                }
            }
            return;
        }
        let mut pending = None;
        if let WorkerNotification::Event(event) = &notification
            && let Some(worker) = self.workers.get_mut(&key)
        {
            remember_tools(&mut worker.tools, &event.event);
            if let Some(info) = &mut worker.info {
                info.head = event.event.id;
                match &event.event.kind {
                    EventKind::ModelChanged { model } => info.model = model.clone(),
                    EventKind::ReasoningChanged { reasoning } => info.reasoning = *reasoning,
                    _ => {}
                }
            }
            match &event.event.kind {
                EventKind::ToolDecision {
                    call,
                    action,
                    decision: Decision::Ask,
                    rule,
                    taint,
                } if worker.approval == ApprovalPolicy::Client => {
                    let approval = PendingApproval {
                        session,
                        call: call.clone(),
                        tool: worker.tools.get(call).cloned().unwrap_or_default(),
                        action: action.clone(),
                        rule: rule.clone(),
                        taint: *taint,
                        trusted: worker.info.as_ref().is_some_and(|info| info.trusted),
                    };
                    if self
                        .inbox
                        .insert((session, call.clone()), (event.event.at, approval.clone()))
                        .is_none()
                    {
                        pending = Some(approval);
                    }
                }
                EventKind::ApprovalResolved { call, .. } | EventKind::ToolResult { call, .. } => {
                    self.inbox.remove(&(session, call.clone()));
                    worker.tools.remove(call);
                }
                _ => {}
            }
        }
        for connection in self.clients.values() {
            if connection.initialized && connection.attached.contains(&session) {
                notification.send(&connection.peer);
            }
        }
        if let Some(pending) = pending {
            for connection in self
                .clients
                .values()
                .filter(|connection| connection.initialized)
            {
                let _ = connection.peer.notify::<ApprovalPending>(&pending);
            }
            if !self.attached(session)
                && let Some(argv) = &self.options.settings.notify
            {
                crate::notify::approval(argv, &pending);
            }
        }
    }

    fn worker_exited(&mut self, key: u64, status: Option<i32>) {
        let Some(mut worker) = self.workers.remove(&key) else {
            return;
        };
        worker.peer.close();
        grant(self.scheduler.remove_worker(key));
        if let Some((client, id)) = worker.start_reply.take() {
            self.respond(client, id, Err(worker_exited()));
        }
        for (client, id) in worker.close_replies {
            self.respond(client, id, Ok(Value::Null));
        }
        if let Some(info) = worker.info {
            self.inbox
                .retain(|(session, _), _| *session != info.session);
            let exited = ExitedParams {
                session: info.session,
                status,
                message: None,
            };
            for connection in self.clients.values_mut() {
                let attached = connection.attached.remove(&info.session);
                if connection.initialized && (attached || connection.watches_running) {
                    let _ = connection.peer.notify::<SessionExited>(&exited);
                }
            }
        }
    }

    fn begin_shutdown(&mut self, when: ShutdownWhen) {
        if self.shutdown == Some(ShutdownWhen::Now) {
            return;
        }
        self.shutdown = Some(when);
        if when == ShutdownWhen::Now {
            let workers: Vec<_> = self
                .workers
                .iter_mut()
                .filter_map(|(key, worker)| {
                    worker
                        .info
                        .as_ref()
                        .map(|info| (info.session, worker.peer.clone()))
                        .map(|(session, peer)| {
                            worker.begin_close(*key, &self.commands);
                            (*key, session, peer)
                        })
                })
                .collect();
            for (key, session, peer) in workers {
                self.forwards += 1;
                let commands = self.commands.clone();
                tokio::spawn(async move {
                    let params = json!({ "session": session });
                    let _ = peer.request_value("session/cancel", params.clone()).await;
                    let result = loop {
                        let result = peer
                            .request_value("session/close", params.clone())
                            .await
                            .map_err(peer_error);
                        if matches!(&result, Err(error) if error.code == RpcError::SESSION_RUNNING)
                        {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        } else {
                            break result;
                        }
                    };
                    let _ = commands.send(Command::Forwarded {
                        client: None,
                        id: None,
                        worker: key,
                        method: "session/close".into(),
                        checkout: None,
                        result,
                    });
                });
            }
            for worker in self
                .workers
                .values_mut()
                .filter(|worker| worker.info.is_none())
            {
                if let Some(kill) = worker.kill.take() {
                    kill();
                }
                worker.peer.close();
            }
        }
    }

    fn tick(&mut self, idle: Duration) {
        if self.shutdown != Some(ShutdownWhen::Now) {
            let expired: Vec<_> = self
                .workers
                .iter()
                .filter_map(|(key, worker)| {
                    let info = worker.info.as_ref()?;
                    (worker.compactions == 0
                        && !worker.checking_idle
                        && !worker.closing
                        && !self.attached(info.session)
                        && worker.idle_since.elapsed() >= idle)
                        .then_some(*key)
                })
                .collect();
            for key in expired {
                let Some(worker) = self.workers.get_mut(&key) else {
                    continue;
                };
                let Some(info) = worker.info.as_ref() else {
                    continue;
                };
                worker.checking_idle = true;
                let activity = worker.activity;
                let reply = worker_running(&worker.peer, info);
                let commands = self.commands.clone();
                tokio::spawn(async move {
                    let result = match reply {
                        Ok(reply) => reply.await,
                        Err(error) => Err(error),
                    };
                    let _ = commands.send(Command::IdleChecked {
                        worker: key,
                        activity,
                        result,
                    });
                });
            }
            if self.workers.is_empty()
                && self.starts.is_empty()
                && self.clients.is_empty()
                && self.idle_since.elapsed() >= idle
            {
                self.begin_shutdown(ShutdownWhen::Idle);
            }
        }
    }

    fn idle_checked(&mut self, key: u64, activity: u64, result: Result<bool, RpcError>) {
        let Some(session) = self
            .workers
            .get(&key)
            .and_then(|worker| worker.info.as_ref())
            .map(|info| info.session)
        else {
            return;
        };
        let attached = self.attached(session);
        let Some(worker) = self.workers.get_mut(&key) else {
            return;
        };
        worker.checking_idle = false;
        if worker.closing || worker.compactions > 0 || worker.activity != activity || attached {
            return;
        }
        if matches!(result, Ok(false)) {
            // Cancelled steering can retain an unanswered prompt while no turn runs.
            // Closing an idle worker rejects that input instead of keeping it alive.
            self.forward(
                None,
                None,
                key,
                "session/close",
                json!({ "session": session }),
            );
        } else {
            worker.reset_idle();
        }
    }

    fn attached(&self, session: SessionId) -> bool {
        self.clients
            .values()
            .any(|client| client.attached.contains(&session))
    }

    fn worker_id(&self, session: SessionId) -> Option<u64> {
        self.workers.iter().find_map(|(id, worker)| {
            (!worker.closing && worker.info.as_ref().map(|info| info.session) == Some(session))
                .then_some(*id)
        })
    }

    fn require_worker(&self, session: SessionId) -> Result<u64, RpcError> {
        self.worker_id(session)
            .ok_or_else(|| RpcError::new(RpcError::UNKNOWN_SESSION, "unknown session"))
    }

    fn respond(&self, client: u64, id: RequestId, result: Result<Value, RpcError>) {
        if let Some(connection) = self.clients.get(&client) {
            let _ = connection.peer.respond(id, result);
        }
    }

    fn summaries(&self) -> Vec<SessionSummary> {
        self.workers
            .values()
            .filter_map(|worker| {
                let info = worker.info.as_ref()?;
                let metadata = log_metadata(&info.path);
                Some(SessionSummary {
                    session: info.session,
                    path: info.path.clone(),
                    cwd: info.cwd.clone(),
                    model: info.model.clone(),
                    head: (!worker.closing).then_some(info.head),
                    running: false,
                    pending_approvals: if worker.closing {
                        0
                    } else {
                        self.inbox
                            .keys()
                            .filter(|(session, _)| *session == info.session)
                            .count()
                    },
                    parent_session: info.parent_session,
                    started: metadata.root.as_ref().map(|root| root.at),
                    updated: metadata.updated,
                    title: metadata.title,
                })
            })
            .collect()
    }
}

fn worker_running(
    peer: &Peer,
    info: &SessionDetails,
) -> Result<impl Future<Output = Result<bool, RpcError>> + Send + 'static + use<>, RpcError> {
    let reply = peer
        .start_request_value(
            "session/attach",
            json!({ "session": info.session, "since": info.head }),
        )
        .map_err(peer_error)?;
    Ok(async move {
        let snapshot = reply.await.map_err(peer_error)?;
        snapshot
            .get("running")
            .and_then(Value::as_bool)
            .ok_or_else(|| internal("worker snapshot has no running flag"))
    })
}

fn remember_tools(tools: &mut HashMap<ToolCallId, String>, event: &Event) {
    if let EventKind::AssistantMessage { content, .. } = &event.kind {
        for block in content {
            if let Block::ToolUse { id, name, .. } = block {
                tools.insert(id.clone(), name.clone());
            }
        }
    }
}

fn list_sessions(
    root: &Path,
    mut live: Vec<SessionSummary>,
    filters: ListParams,
) -> Result<Vec<SessionSummary>, RpcError> {
    let live_ids: HashSet<_> = live.iter().map(|session| session.session).collect();
    let workspaces = match std::fs::read_dir(root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(internal(error)),
    };
    if let Some(workspaces) = workspaces {
        for workspace in workspaces {
            let workspace = workspace.map_err(internal)?;
            if !workspace.file_type().map_err(internal)?.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(workspace.path()).map_err(internal)? {
                let file = file.map_err(internal)?;
                let path = file.path();
                if path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
                {
                    continue;
                }
                let metadata = log_metadata(&path);
                let Some(event) = metadata.root else {
                    continue;
                };
                if let EventKind::SessionStarted {
                    session,
                    cwd,
                    model,
                    parent_session,
                    ..
                } = event.kind
                    && !live_ids.contains(&session)
                {
                    live.push(SessionSummary {
                        session,
                        path,
                        cwd,
                        model,
                        head: None,
                        running: false,
                        pending_approvals: 0,
                        parent_session,
                        started: Some(event.at),
                        updated: metadata.updated,
                        title: metadata.title,
                    });
                }
            }
        }
    }
    live.retain(|session| {
        filters.cwd.as_ref().is_none_or(|cwd| *cwd == session.cwd)
            && filters
                .running
                .is_none_or(|running| running == session.running)
    });
    live.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| a.session.cmp(&b.session))
    });
    Ok(live)
}

#[derive(Default)]
struct LogMetadata {
    root: Option<Event>,
    updated: Option<Timestamp>,
    title: Option<String>,
}

fn log_metadata(path: &Path) -> LogMetadata {
    let mut metadata = LogMetadata::default();
    let Ok(file) = std::fs::File::open(path) else {
        return metadata;
    };
    metadata.updated = file
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .map(Timestamp);
    let mut reader = BufReader::new(file.take(64 * 1024));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
        return metadata;
    }
    let Ok(root) = serde_json::from_str::<Event>(&line) else {
        return metadata;
    };
    if !matches!(root.kind, EventKind::SessionStarted { .. }) {
        return metadata;
    }
    metadata.root = Some(root);
    loop {
        line.clear();
        if reader.read_line(&mut line).is_err() || !line.ends_with('\n') {
            break;
        }
        let Ok(event) = serde_json::from_str::<Event>(&line) else {
            break;
        };
        if let EventKind::UserMessage { content } = event.kind {
            let text: String = content
                .into_iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text),
                    _ => None,
                })
                .collect();
            if let Some(first) = text.lines().find(|line| !line.trim().is_empty()) {
                let first = first.trim();
                let mut chars = first.chars();
                let mut title: String = chars.by_ref().take(80).collect();
                if chars.next().is_some() {
                    title.push('…');
                }
                metadata.title = Some(title);
            }
            break;
        }
    }
    metadata
}

fn grant(replies: Vec<Reply>) {
    for (peer, id) in replies {
        let _ = peer.respond(id, Ok(Value::Null));
    }
}

fn settings_paths(cwd: &Path, configs: &[PathBuf]) -> Result<(), RpcError> {
    if !cwd.is_absolute() {
        return Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            format!("settings cwd must be absolute: {}", cwd.display()),
        ));
    }
    if let Some(path) = configs.iter().find(|path| !path.is_absolute()) {
        return Err(RpcError::new(
            RpcError::INVALID_PARAMS,
            format!("settings config path must be absolute: {}", path.display()),
        ));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value)
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn decode_ref<T: DeserializeOwned>(value: &Value) -> Result<T, RpcError> {
    serde::Deserialize::deserialize(value)
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

fn encode(value: impl Serialize) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(internal)
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcError::INTERNAL_ERROR, error.to_string())
}

fn worker_exited() -> RpcError {
    RpcError::new(RpcError::WORKER_EXITED, "worker exited")
}

fn peer_error(error: PeerError) -> RpcError {
    match error {
        PeerError::Rpc(error) => error,
        PeerError::Closed => worker_exited(),
        PeerError::Json(error) => internal(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(session: SessionId) -> Event {
        Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp(11),
            kind: EventKind::SessionStarted {
                session,
                cwd: "/work".into(),
                model: xi_protocol::event::ModelRef {
                    provider: "test".into(),
                    model: "test".into(),
                },
                xi_version: "test".into(),
                parent_session: None,
            },
        }
    }

    #[test]
    fn metadata_scans_are_bounded_and_malformed_lines_are_not_fatal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.jsonl");
        let root = root(SessionId::generate());
        let first = format!("{}\n", serde_json::to_string(&root).unwrap());
        let prompt = Event {
            id: EventId::generate(),
            parent: Some(root.id),
            at: Timestamp(12),
            kind: EventKind::UserMessage {
                content: vec![Block::Text {
                    text: "title".into(),
                }],
            },
        };
        let last = format!("{}\n", serde_json::to_string(&prompt).unwrap());
        let padding = Event {
            id: EventId::generate(),
            parent: Some(root.id),
            at: Timestamp(12),
            kind: EventKind::Custom {
                extension: "test".into(),
                kind: "large".into(),
                data: json!("x".repeat(64 * 1024)),
            },
        };
        std::fs::write(
            &path,
            format!(
                "{first}{}\n{last}",
                serde_json::to_string(&padding).unwrap()
            ),
        )
        .unwrap();
        let metadata = log_metadata(&path);
        assert_eq!(
            metadata.root.as_ref().map(|event| event.at),
            Some(Timestamp(11))
        );
        assert!(metadata.updated.is_some());
        assert_eq!(metadata.title, None);
        std::fs::write(&path, format!("{first}not json\n{last}")).unwrap();
        let metadata = log_metadata(&path);
        assert_eq!(
            metadata.root.as_ref().map(|event| event.at),
            Some(Timestamp(11))
        );
        assert_eq!(metadata.title, None);
        std::fs::write(&path, "not json\n").unwrap();
        assert!(log_metadata(&path).root.is_none());
        let missing = log_metadata(&directory.path().join("missing"));
        assert!(missing.root.is_none());
        assert!(missing.updated.is_none());
        assert!(missing.title.is_none());
    }

    #[test]
    fn saved_sessions_with_equal_mtimes_are_sorted_by_id() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut sessions = [SessionId::generate(), SessionId::generate()];
        sessions.sort();
        for (index, session) in sessions.iter().rev().enumerate() {
            let path = workspace.join(format!("{index}.jsonl"));
            std::fs::write(
                &path,
                format!("{}\n", serde_json::to_string(&root(*session)).unwrap()),
            )
            .unwrap();
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(100))
                .unwrap();
        }
        let listed = list_sessions(directory.path(), Vec::new(), ListParams::default()).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|summary| summary.session)
                .collect::<Vec<_>>(),
            sessions
        );
        assert!(
            listed
                .iter()
                .all(|summary| summary.updated == Some(Timestamp(100_000)))
        );
    }
}
