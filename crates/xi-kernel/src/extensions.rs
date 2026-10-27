//! Sandboxed, full-duplex extension peers bound to their owning session.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use xi_protocol::manifest::{CommandSpec, HookPoint, PackageManifest, Registration, ToolSpec};
use xi_protocol::methods::extension::*;
use xi_protocol::rpc::{self, Message, RequestId, RequestMethod};
use xi_protocol::{ExtensionId, SessionId, ToolOutput};

use crate::{Error, Result, config::Config, egress};

type Reply = oneshot::Sender<Result<Value>>;

/// Locks extension bookkeeping even if an earlier holder panicked.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a session services for its extensions while it waits.
pub(crate) enum Incoming {
    /// A request from an extension process.
    Request {
        extension: ExtensionId,
        request: rpc::Request,
        reply: Reply,
    },
    /// A connection through an extension's egress listener.
    Connect(egress::Connect),
}

struct Route {
    peers: HashSet<ExtensionId>,
    sender: mpsc::UnboundedSender<Incoming>,
}

/// A session's extension supervisor. No process serves another session.
pub(crate) struct ExtensionHost {
    session: SessionId,
    peers: Mutex<HashMap<(SessionId, PathBuf, PathBuf), Arc<Peer>>>,
    loading: tokio::sync::Mutex<()>,
    closed: AtomicBool,
    reaping: Mutex<Vec<CancellationToken>>,
    routes: Arc<Mutex<HashMap<SessionId, Route>>>,
}

/// A session's ordered extension catalog.
#[derive(Clone, Default)]
pub struct Extensions(Arc<[Arc<Peer>]>);

pub(crate) struct Scope {
    session: SessionId,
    routes: Arc<Mutex<HashMap<SessionId, Route>>>,
    pub receiver: mpsc::UnboundedReceiver<Incoming>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        lock(&self.routes).remove(&self.session);
    }
}

impl ExtensionHost {
    pub(crate) fn new(session: SessionId) -> Self {
        Self {
            session,
            peers: Mutex::default(),
            loading: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
            reaping: Mutex::default(),
            routes: Arc::default(),
        }
    }

    pub(crate) async fn load(
        &self,
        config: &Config,
        context: &xi_tools::Context,
        log: &Path,
        builtins: &[ToolSpec],
    ) -> Result<Extensions> {
        self.load_inner(config, context, log, builtins, false)
            .await
            .map(|(extensions, _)| extensions)
    }

    /// Starts the catalog without making a failed extension prevent attachment.
    pub(crate) async fn startup(
        &self,
        config: &Config,
        context: &xi_tools::Context,
        log: &Path,
        builtins: &[ToolSpec],
    ) -> Result<(Extensions, bool)> {
        self.load_inner(config, context, log, builtins, true).await
    }

    async fn load_inner(
        &self,
        config: &Config,
        context: &xi_tools::Context,
        log: &Path,
        builtins: &[ToolSpec],
        startup: bool,
    ) -> Result<(Extensions, bool)> {
        let _loading = self.loading.lock().await;
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Extension("session extension host closed".into()));
        }
        let mut selected = Vec::new();
        let mut complete = true;
        let mut tools: HashSet<_> = builtins.iter().map(|s| s.name.clone()).collect();
        let mut names = HashSet::new();
        let mut commands: HashSet<String> = [
            "help",
            "hotkeys",
            "quit",
            "exit",
            "new",
            "resume",
            "tree",
            "model",
            "reasoning",
            "compact",
            "approvals",
            "settings",
            "copy",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for root in &config.extensions {
            let root = match root.canonicalize() {
                Ok(root) => root,
                Err(error) if startup => {
                    eprintln!("xi: extension {}: {error}", root.display());
                    complete = false;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let key = (self.session, config.workspace.clone(), root.clone());
            // A peer stops when it crashes or when a cancelled invocation
            // retires it; the next load starts a fresh process.
            let cached = lock(&self.peers).get(&key).cloned();
            let peer = match cached {
                Some(peer) if !peer.stop.is_cancelled() => peer,
                _ => {
                    let peer = match Peer::start(
                        &root,
                        config,
                        context,
                        log,
                        self.session,
                        self.routes.clone(),
                    )
                    .await
                    {
                        Ok(peer) => peer,
                        Err(error) if startup => {
                            eprintln!("xi: extension {}: {error}", root.display());
                            complete = false;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let mut peers = lock(&self.peers);
                    if self.closed.load(Ordering::Relaxed) {
                        peer.stop.cancel();
                        lock(&self.reaping).push(peer.exited.clone());
                        return Err(Error::Extension("session extension host closed".into()));
                    }
                    peers.insert(key, peer.clone());
                    peer
                }
            };
            if !names.insert(peer.manifest.name.clone()) {
                return Err(Error::Config(format!(
                    "duplicate extension {}",
                    peer.manifest.name
                )));
            }
            for tool in &peer.registration.tools {
                if tool.name.is_empty() || !tools.insert(tool.name.clone()) {
                    return Err(Error::Config(format!(
                        "extension tool name clash: {}",
                        tool.name
                    )));
                }
            }
            for command in &peer.registration.commands {
                if command.name.is_empty()
                    || command.name.contains(char::is_whitespace)
                    || command.name.starts_with('/')
                    || !commands.insert(command.name.clone())
                {
                    return Err(Error::Config(format!(
                        "extension command name clash or invalid name: {}",
                        command.name
                    )));
                }
            }
            selected.push(peer);
        }
        Ok((Extensions(selected.into()), complete))
    }

    pub(crate) fn scope(&self, extensions: &Extensions) -> Scope {
        let (sender, receiver) = mpsc::unbounded_channel();
        let route = Route {
            peers: extensions
                .0
                .iter()
                .map(|p| p.manifest.name.clone())
                .collect(),
            sender,
        };
        lock(&self.routes).insert(self.session, route);
        Scope {
            session: self.session,
            routes: self.routes.clone(),
            receiver,
        }
    }

    /// Evicts the session's peers and immediately stops their process groups.
    pub(crate) fn close(&self) {
        let mut peers = lock(&self.peers);
        self.closed.store(true, Ordering::Relaxed);
        let mut reaping = lock(&self.reaping);
        for (_, peer) in peers.drain() {
            peer.wire.closing.store(true, Ordering::Relaxed);
            peer.stop.cancel();
            reaping.push(peer.exited.clone());
        }
        lock(&self.routes).clear();
    }

    /// Requests shutdown, then terminates each process group and reaps it.
    pub(crate) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _loading = self.loading.lock().await;
        let peers: Vec<_> = lock(&self.peers).drain().map(|(_, p)| p).collect();
        futures::future::join_all(peers.iter().map(|peer| async move {
            peer.wire.closing.store(true, Ordering::Relaxed);
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                peer.request::<Shutdown>(&(), &CancellationToken::new()),
            )
            .await;
            peer.stop.cancel();
            peer.exited.cancelled().await;
        }))
        .await;
        let reaping = std::mem::take(&mut *lock(&self.reaping));
        futures::future::join_all(reaping.iter().map(CancellationToken::cancelled)).await;
        lock(&self.routes).clear();
    }
}

impl Drop for ExtensionHost {
    fn drop(&mut self) {
        self.close();
    }
}

impl Extensions {
    pub(crate) fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.0.iter().flat_map(|peer| &peer.registration.tools)
    }

    pub(crate) fn commands(&self) -> impl Iterator<Item = &CommandSpec> {
        self.0.iter().flat_map(|peer| &peer.registration.commands)
    }

    /// Whether every extension process in the catalog is still running.
    pub(crate) fn running(&self) -> bool {
        self.0.iter().all(|peer| !peer.stop.is_cancelled())
    }

    pub(crate) fn has_tool(&self, name: &str) -> bool {
        self.specs().any(|spec| spec.name == name)
    }

    pub(crate) fn has_hook(&self, point: HookPoint) -> bool {
        self.0
            .iter()
            .any(|peer| peer.registration.hooks.contains(&point))
    }

    pub(crate) async fn invoke(
        &self,
        params: &InvokeParams,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput> {
        let peer = self
            .0
            .iter()
            .find(|peer| {
                peer.registration
                    .tools
                    .iter()
                    .any(|tool| tool.name == params.tool)
            })
            .ok_or_else(|| Error::Extension(format!("unknown extension tool {}", params.tool)))?;
        peer.request::<ToolInvoke>(params, cancel).await
    }

    pub(crate) async fn command(
        &self,
        params: &CommandParams,
        cancel: &CancellationToken,
    ) -> Result<Option<CommandResult>> {
        let Some(peer) = self.0.iter().find(|peer| {
            peer.registration
                .commands
                .iter()
                .any(|command| command.name == params.name)
        }) else {
            return Ok(None);
        };
        peer.request::<CommandRun>(params, cancel).await.map(Some)
    }

    pub(crate) async fn hook(
        &self,
        session: SessionId,
        mut event: HookEvent,
        cancel: &CancellationToken,
    ) -> Result<HookOutcome> {
        let point = match event {
            HookEvent::PreTool { .. } => HookPoint::PreTool,
            HookEvent::PostTool { .. } => HookPoint::PostTool,
            HookEvent::TurnEnd { .. } => HookPoint::TurnEnd,
            HookEvent::Compact { .. } => HookPoint::Compact,
        };
        let mut result = HookOutcome::Continue;
        for peer in self.0.iter() {
            if !peer.registration.hooks.contains(&point) {
                continue;
            }
            // Each peer sees prior rewrites, so the owned request needs the
            // current event rather than one serialization shared by all peers.
            let outcome = peer
                .request::<HookRun>(
                    &HookParams {
                        session,
                        event: event.clone(),
                    },
                    cancel,
                )
                .await?;
            match (&mut event, &outcome) {
                (_, HookOutcome::Continue) => continue,
                (HookEvent::PreTool { .. }, HookOutcome::Deny { .. }) => return Ok(outcome),
                (HookEvent::PreTool { input, .. }, HookOutcome::RewriteInput { input: next }) => {
                    input.clone_from(next)
                }
                (
                    HookEvent::PostTool { output, .. },
                    HookOutcome::ReplaceOutput { output: next },
                ) => {
                    let untrusted = output.untrusted;
                    output.clone_from(next);
                    output.untrusted |= untrusted;
                }
                (HookEvent::Compact { .. }, HookOutcome::ReplaceSummary { .. }) => {}
                _ => {
                    return Err(Error::Extension(format!(
                        "extension {} returned an invalid outcome for {point:?}",
                        peer.manifest.name
                    )));
                }
            }
            result = match &event {
                HookEvent::PostTool { output, .. } => HookOutcome::ReplaceOutput {
                    output: output.clone(),
                },
                _ => outcome,
            };
        }
        Ok(result)
    }

    /// Runs compact hooks in registration order. The last supplied summary
    /// wins; `None` asks the kernel to generate its own summary. The caller
    /// must service the host's session scope while awaiting this method.
    pub async fn compact(
        &self,
        session: SessionId,
        tokens_before: u64,
        previous_summary: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        match self
            .hook(
                session,
                HookEvent::Compact {
                    tokens_before,
                    previous_summary,
                },
                cancel,
            )
            .await?
        {
            HookOutcome::ReplaceSummary { summary } => Ok(Some(summary)),
            _ => Ok(None),
        }
    }
}

struct Peer {
    manifest: PackageManifest,
    registration: Registration,
    wire: Wire,
    stop: CancellationToken,
    exited: CancellationToken,
}

#[derive(Clone)]
struct Wire {
    sender: mpsc::Sender<Message>,
    stop: CancellationToken,
    pending: Arc<Mutex<HashMap<RequestId, Reply>>>,
    next: Arc<AtomicI64>,
    failure: Arc<Mutex<Option<String>>>,
    log: Arc<Mutex<std::fs::File>>,
    closing: Arc<AtomicBool>,
}

impl Wire {
    async fn send(&self, message: Message) -> Result<()> {
        let result = tokio::select! {
            biased;
            () = self.stop.cancelled() => return Err(Error::Extension("extension connection closed".into())),
            result = self.sender.send(message) => result,
        };
        result.map_err(|_| {
            Error::Extension(
                lock(&self.failure)
                    .clone()
                    .unwrap_or_else(|| "extension connection closed".into()),
            )
        })
    }
    fn fail(&self, message: String) {
        let mut failure = lock(&self.failure);
        if failure.is_some() {
            return;
        }
        self.log(&message);
        if !self.closing.load(Ordering::Relaxed) {
            eprintln!("xi: {message}");
        }
        *failure = Some(message.clone());
        for (_, reply) in lock(&self.pending).drain() {
            let _ = reply.send(Err(Error::Extension(message.clone())));
        }
    }
    fn log(&self, message: &str) {
        let _ = writeln!(lock(&self.log), "{message}");
    }
}

impl Peer {
    async fn start(
        root: &Path,
        config: &Config,
        context: &xi_tools::Context,
        log: &Path,
        session: SessionId,
        routes: Arc<Mutex<HashMap<SessionId, Route>>>,
    ) -> Result<Arc<Self>> {
        let path = root.join("xi-extension.toml");
        let manifest: PackageManifest = toml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?;
        let name = manifest.name.to_string();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Config(
                "extension name must contain only letters, digits, '-' or '_'".into(),
            ));
        }
        let egress = if manifest.capabilities.net.is_empty() {
            None
        } else {
            let hosts = egress::host_set(&manifest.capabilities.net).map_err(|error| {
                Error::Config(format!("extension {name}: invalid net pattern: {error}"))
            })?;
            let mut listener = egress::Listener::bind(&name)?;
            let routes = routes.clone();
            let extension = ExtensionId(name.clone());
            listener.start(
                Some(name.clone()),
                Some(hosts),
                Box::new(move |connect| {
                    let routes = lock(&routes);
                    let Some(route) = routes
                        .get(&session)
                        .filter(|route| route.peers.contains(&extension))
                    else {
                        return Some(connect);
                    };
                    route
                        .sender
                        .send(Incoming::Connect(connect))
                        .err()
                        .map(|error| match error.0 {
                            Incoming::Connect(connect) => connect,
                            Incoming::Request { .. } => unreachable!("sent a connection"),
                        })
                }),
            );
            Some(listener)
        };
        let Some((program, args)) = manifest.run.split_first() else {
            return Err(Error::Config(format!(
                "extension {name} has an empty run command"
            )));
        };
        let tmp = log.with_extension(format!("{name}.tmp"));
        std::fs::create_dir_all(&tmp)?;
        let mut spec = xi_sandbox::Spec::new(root, tmp.canonicalize()?);
        spec.writable = vec![spec.tmp.clone()];
        spec.protected.clear();
        spec.readable.push(root.to_owned());
        spec.egress = egress.as_ref().map(|listener| listener.egress().clone());
        // Runtime roots only: never inherit workspace, skills or user sandbox grants.
        for runtime in [
            "~/.local/share/mise/installs",
            "~/.local/bin",
            "/opt/homebrew",
        ] {
            let runtime = config.paths.expand(Path::new(runtime));
            if runtime.exists() {
                spec.readable.push(runtime);
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            spec.readable
                .extend(std::env::split_paths(&path).filter(|p| {
                    p.is_absolute() && p.is_dir() && !config.paths.home.starts_with(p)
                }));
        }
        for (entries, roots) in [
            (&manifest.capabilities.fs_read, &mut spec.readable),
            (&manifest.capabilities.fs_write, &mut spec.writable),
        ] {
            for entry in entries {
                if entry.contains(['*', '?', '[', ']']) {
                    return Err(Error::Config(format!(
                        "extension {name}: filesystem capabilities must be literal paths, not globs: {entry}"
                    )));
                }
                roots.push(
                    root.join(config.paths.expand(Path::new(entry)))
                        .canonicalize()?,
                );
            }
        }
        // Only the manifest may opt in to additional environment variables.
        spec.env.extend(manifest.capabilities.env.iter().cloned());
        let logfile = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log.with_extension(format!("{name}.log")))?;
        let mut command = spec
            .command(&context.helper, program, args)
            .map_err(|e| Error::Config(e.to_string()))?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        drop(command);
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        // Node fstats all standard descriptors at startup. Passing the log
        // file itself would require granting access to its metadata.
        // Keep stderr a pipe and copy it outside the sandbox instead.
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut stderr_log = tokio::fs::File::from_std(logfile.try_clone()?);
        let stderr_copy = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stderr, &mut stderr_log).await;
        });
        // Bound memory when an extension stops reading. Producers wait for
        // the pipe writer rather than accumulating an unbounded queue.
        let (sender, mut outgoing) = mpsc::channel(256);
        let stop = CancellationToken::new();
        let wire = Wire {
            sender,
            stop: stop.clone(),
            pending: Arc::default(),
            next: Arc::new(AtomicI64::new(1)),
            failure: Arc::default(),
            log: Arc::new(Mutex::new(logfile)),
            closing: Arc::default(),
        };
        let writer_wire = wire.clone();
        let writer_stop = stop.clone();
        tokio::spawn(async move {
            while let Some(message) = tokio::select! { biased; () = writer_stop.cancelled() => None, message = outgoing.recv() => message }
            {
                let result = async {
                    let mut bytes = serde_json::to_vec(&message)?;
                    bytes.push(b'\n');
                    stdin.write_all(&bytes).await?;
                    stdin.flush().await?;
                    Ok::<_, Error>(())
                };
                let result = tokio::select! {
                    biased;
                    () = writer_stop.cancelled() => break,
                    result = result => result,
                };
                if let Err(error) = result {
                    writer_wire.fail(format!("extension write failed: {error}"));
                    writer_stop.cancel();
                    break;
                }
            }
        });
        let reader_wire = wire.clone();
        let reader_stop = stop.clone();
        let extension = manifest.name.clone();
        let subagents = manifest.capabilities.subagents;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let requests = Arc::new(Semaphore::new(32));
            let mut tasks = JoinSet::new();
            loop {
                let line = tokio::select! {
                    biased;
                    () = reader_stop.cancelled() => break,
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
                    line = lines.next_line() => line,
                };
                let message = match line {
                    Ok(Some(line)) => {
                        serde_json::from_str::<Message>(&line).map_err(|e| e.to_string())
                    }
                    Ok(None) => Err("stdout closed".into()),
                    Err(error) => Err(error.to_string()),
                };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        reader_wire.fail(format!("extension {extension} stopped: {error}"));
                        reader_stop.cancel();
                        break;
                    }
                };
                match message {
                    Message::Response(response) => {
                        if let Some(id) = &response.id
                            && let Some(reply) = lock(&reader_wire.pending).remove(id)
                        {
                            let result = match response.outcome {
                                rpc::Outcome::Ok(value) => Ok(value),
                                rpc::Outcome::Err(error) => {
                                    Err(Error::Extension(error.to_string()))
                                }
                            };
                            let _ = reply.send(result);
                        }
                    }
                    Message::Notification(notification) => {
                        if let Some(Ok(params)) = notification.params::<Log>() {
                            reader_wire.log(&format!("{:?}: {}", params.level, params.message));
                        }
                    }
                    Message::Request(request) => {
                        let id = request.id.clone();
                        // Reject excess requests without blocking response
                        // routing, which active requests may still need.
                        let Ok(permit) = requests.clone().try_acquire_owned() else {
                            let response = rpc::Response::err(
                                Some(id),
                                rpc::RpcError::new(
                                    -32000,
                                    "too many concurrent extension requests",
                                ),
                            );
                            let _ = reader_wire.send(Message::Response(response)).await;
                            continue;
                        };
                        // Routing before spawning keeps a session's requests,
                        // such as concurrent `agent/run`s, in the order the
                        // extension sent them, so children start in that order.
                        let routed = route(
                            &reader_wire,
                            &routes,
                            session,
                            extension.clone(),
                            subagents,
                            request,
                        );
                        let wire = reader_wire.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            let result = match routed {
                                Ok(reply) => reply.await.unwrap_or_else(|_| {
                                    Err(Error::Extension(
                                        "session extension request cancelled".into(),
                                    ))
                                }),
                                Err(error) => Err(error),
                            };
                            let response = match result {
                                Ok(value) => rpc::Response {
                                    jsonrpc: rpc::Version,
                                    id: Some(id),
                                    outcome: rpc::Outcome::Ok(value),
                                },
                                Err(error) => rpc::Response::err(
                                    Some(id),
                                    rpc::RpcError::new(-32000, error.to_string()),
                                ),
                            };
                            let _ = wire.send(Message::Response(response)).await;
                        });
                    }
                }
            }
        });
        let exited = CancellationToken::new();
        let supervisor_exited = exited.clone();
        let supervisor_stop = stop.clone();
        let supervisor_wire = wire.clone();
        let supervisor_name = name.clone();
        tokio::spawn(async move {
            let mut group = ProcessGroup {
                pid: child.id().expect("spawned child") as i32,
                reaped: false,
            };
            tokio::select! {
                result = child.wait() => supervisor_wire.fail(format!("extension {supervisor_name} exited: {result:?}")),
                () = supervisor_stop.cancelled() => { group.kill(); let _ = child.wait().await; }
            }
            group.reaped = true;
            group.kill();
            group.pid = 0;
            supervisor_stop.cancel();
            supervisor_wire.fail(format!("extension {supervisor_name} connection closed"));
            let _ = stderr_copy.await;
            drop(egress);
            supervisor_exited.cancel();
        });
        let mut peer = Self {
            manifest,
            registration: Registration::default(),
            wire,
            stop,
            exited,
        };
        let handshake = tokio::time::timeout(
            Duration::from_secs(15),
            peer.request::<Initialize>(
                &InitializeParams {
                    protocol_version: xi_protocol::PROTOCOL_VERSION,
                    xi_version: env!("CARGO_PKG_VERSION").into(),
                    workspace: config.workspace.clone(),
                },
                &context.cancel,
            ),
        )
        .await;
        peer.registration = match handshake {
            Ok(Ok(registration)) => registration,
            Ok(Err(error)) => {
                peer.stop.cancel();
                return Err(error);
            }
            Err(_) => {
                peer.stop.cancel();
                return Err(Error::Extension(format!(
                    "extension {name} initialize timed out"
                )));
            }
        };
        Ok(Arc::new(peer))
    }

    async fn request<M: RequestMethod>(
        &self,
        params: &M::Params,
        cancel: &CancellationToken,
    ) -> Result<M::Result> {
        let id = RequestId::Number(self.wire.next.fetch_add(1, Ordering::Relaxed));
        let request = rpc::Request::new::<M>(id.clone(), params)?;
        let (reply, receiver) = oneshot::channel();
        {
            let failure = lock(&self.wire.failure);
            if let Some(error) = &*failure {
                return Err(Error::Extension(error.clone()));
            }
            lock(&self.wire.pending).insert(id.clone(), reply);
        }
        let guard = Pending {
            id: id.clone(),
            wire: &self.wire,
        };
        let value = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = self.wire.sender.try_send(Message::Notification(rpc::Notification { jsonrpc: rpc::Version, method: "$/cancelRequest".into(), params: serde_json::json!({"id": id}) }));
                // A process may ignore cooperative cancellation. Retire it so
                // no cancelled invocation continues changing the workspace.
                self.stop.cancel();
                return Err(Error::Extension("extension invocation cancelled".into()));
            }
            result = async {
                self.wire.send(Message::Request(request)).await?;
                receiver.await.map_err(|_| Error::Extension("extension response channel closed".into()))?
            } => result?,
        };
        drop(guard);
        serde_json::from_value(value).map_err(|error| Error::Extension(error.to_string()))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Pending<'a> {
    id: RequestId,
    wire: &'a Wire,
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        lock(&self.wire.pending).remove(&self.id);
    }
}

struct ProcessGroup {
    pid: i32,
    reaped: bool,
}
impl ProcessGroup {
    fn kill(&self) {
        if self.pid == 0 {
            return;
        }
        use nix::sys::signal::{Signal, kill, killpg};
        let pid = nix::unistd::Pid::from_raw(self.pid);
        let _ = killpg(pid, Signal::SIGKILL);
        if !self.reaped {
            let _ = kill(pid, Signal::SIGKILL);
        }
        let _ = killpg(pid, Signal::SIGKILL);
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Checks an extension request and hands it to its session. The receiver
/// yields the session's answer.
fn route(
    wire: &Wire,
    routes: &Mutex<HashMap<SessionId, Route>>,
    bound_session: SessionId,
    extension: ExtensionId,
    subagents: bool,
    request: rpc::Request,
) -> Result<oneshot::Receiver<Result<Value>>> {
    let (reply, receiver) = oneshot::channel();
    if request.method == "log" {
        let params: LogParams = serde_json::from_value(request.params)
            .map_err(|error| Error::Extension(error.to_string()))?;
        wire.log(&format!("{:?}: {}", params.level, params.message));
        let _ = reply.send(Ok(Value::Null));
        return Ok(receiver);
    }
    if request.method != AppendCustom::METHOD && request.method != AgentRun::METHOD {
        return Err(Error::Extension(format!(
            "unknown extension request {}",
            request.method
        )));
    }
    if request.method == AgentRun::METHOD && !subagents {
        return Err(Error::Extension(format!(
            "extension {extension} requires the subagents capability for agent/run"
        )));
    }
    let session: SessionId = serde_json::from_value(
        request
            .params
            .get("session")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(|error| Error::Extension(error.to_string()))?;
    if session != bound_session {
        return Err(Error::Extension(
            "extension request names a different session".into(),
        ));
    }
    let routes = lock(routes);
    let route = routes
        .get(&session)
        .filter(|route| route.peers.contains(&extension))
        .ok_or_else(|| Error::Extension("session is not serving this extension".into()))?;
    route
        .sender
        .send(Incoming::Request {
            extension,
            request,
            reply,
        })
        .map_err(|_| Error::Extension("session extension scope closed".into()))?;
    Ok(receiver)
}
