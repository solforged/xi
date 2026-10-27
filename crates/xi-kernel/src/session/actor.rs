//! The session's single control owner. Slow operations own State in a child
//! task, leaving cancellation, approvals and steering responsive.

use super::*;
use std::sync::Weak;
use tokio::task::JoinSet;
use xi_protocol::methods::{client::CompactResult, extension::CommandResult};

pub(super) enum Command {
    Submit(Queued, Session),
    Cancel,
    Close,
    Stop,
    WaitIdle(oneshot::Sender<()>),
    Drain(oneshot::Sender<Vec<Queued>>),
    HasQueue(oneshot::Sender<bool>),
    Titles(oneshot::Sender<Vec<title::Update>>),
    Register(ToolCallId, PendingAsk),
    Remove(ToolCallId),
    Approve {
        call: ToolCallId,
        approved: bool,
        remember: Remember,
        reply: oneshot::Sender<Result<()>>,
    },
    ApprovalWritten {
        call: ToolCallId,
        approved: bool,
        remember: Remember,
        result: Result<()>,
        reply: oneshot::Sender<Result<()>>,
    },
    Idle(IdleCommand),
}

pub(crate) enum IdleCommand {
    Checkout(EventId, oneshot::Sender<Result<()>>),
    Model(ModelSelector, oneshot::Sender<Result<ModelRef>>),
    Reasoning(Reasoning, oneshot::Sender<Result<()>>),
    Sandbox(bool, oneshot::Sender<Result<()>>),
    Startup(oneshot::Sender<Result<()>>),
    Compact(oneshot::Sender<Result<CompactResult>>),
    ExtensionCommand(
        String,
        String,
        oneshot::Sender<Result<Option<CommandResult>>>,
    ),
    Title(title::Update, oneshot::Sender<Result<()>>),
}

enum Completion {
    Unit(oneshot::Sender<Result<()>>, Result<()>),
    Model(oneshot::Sender<Result<ModelRef>>, Result<ModelRef>),
    Compact(
        oneshot::Sender<Result<CompactResult>>,
        Result<CompactResult>,
    ),
    Extension(
        oneshot::Sender<Result<Option<CommandResult>>>,
        Result<Option<CommandResult>>,
    ),
    None,
}

impl Completion {
    fn send(self) {
        match self {
            Self::Unit(reply, result) => {
                let _ = reply.send(result);
            }
            Self::Model(reply, result) => {
                let _ = reply.send(result);
            }
            Self::Compact(reply, result) => {
                let _ = reply.send(result);
            }
            Self::Extension(reply, result) => {
                let _ = reply.send(result);
            }
            Self::None => {}
        }
    }
}

impl IdleCommand {
    fn reject(self, error: Error) {
        match self {
            Self::Checkout(_, reply)
            | Self::Reasoning(_, reply)
            | Self::Sandbox(_, reply)
            | Self::Startup(reply)
            | Self::Title(_, reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Model(_, reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Compact(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::ExtensionCommand(_, _, reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn run(
        self,
        session: &Session,
        state: &mut State,
        cancel: &CancellationToken,
    ) -> Completion {
        match self {
            Self::Checkout(event, reply) => {
                Completion::Unit(reply, session.checkout_state(state, event))
            }
            Self::Model(selector, reply) => {
                let result = async {
                    let mut config = state.config.clone();
                    let model = config.resolve(&selector);
                    config.model = model.clone();
                    let connect = config.clone();
                    let backend = tokio::task::spawn_blocking(move || Backend::connect(&connect))
                        .await
                        .map_err(|_| Error::Stopped)??;
                    if model != state.config.model {
                        session.append(
                            state,
                            EventKind::ModelChanged {
                                model: model.clone(),
                            },
                        )?;
                    }
                    state.config = config;
                    state.backend = Some(backend.into());
                    session.refresh(state);
                    Ok(model)
                }
                .await;
                Completion::Model(reply, result)
            }
            Self::Reasoning(reasoning, reply) => {
                let result = (|| {
                    let registry = state.config.registry();
                    let model = state
                        .backend
                        .as_ref()
                        .map(|backend| &backend.model)
                        .or_else(|| registry.model(&state.config.model));
                    if let Some(model) = model
                        && model.resolve(reasoning) != reasoning
                    {
                        let levels = model
                            .reasoning
                            .iter()
                            .map(|level| level.name())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(Error::Config(format!(
                            "{}/{} does not support {reasoning} reasoning; it accepts {levels}",
                            model.provider, model.id
                        )));
                    }
                    if reasoning != state.config.reasoning {
                        session.append(state, EventKind::ReasoningChanged { reasoning })?;
                        state.config.reasoning = reasoning;
                    }
                    session.refresh(state);
                    Ok(())
                })();
                Completion::Unit(reply, result)
            }
            Self::Sandbox(enabled, reply) => {
                let result = (|| {
                    if enabled == state.config.sandbox.enabled {
                        return Ok(());
                    }
                    let mut sandbox = state.tools_context.sandbox.clone();
                    sandbox.enabled = enabled;
                    let listener = if enabled {
                        sandbox::validate_sandbox(&sandbox)?;
                        let listener = egress::Listener::bind("tools")?;
                        sandbox.egress = Some(listener.egress().clone());
                        Some(listener)
                    } else {
                        sandbox.egress = None;
                        None
                    };
                    session.append(state, EventKind::SandboxChanged { enabled })?;
                    state.tools_context.sandbox = sandbox;
                    state.config.sandbox.enabled = enabled;
                    state.egress.listener = listener;
                    session.refresh(state);
                    Ok(())
                })();
                Completion::Unit(reply, result)
            }
            Self::Startup(reply) => {
                if !state.config.extensions.is_empty() {
                    let specs = match &state.backend {
                        Some(backend) => Ok(backend.specs.clone()),
                        None => toolset(&state.config)
                            .map(|tools| tools.iter().map(|tool| tool.spec()).collect()),
                    };
                    let loaded = match specs {
                        Ok(specs) => {
                            session
                                .0
                                .extensions
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
                            session
                                .0
                                .commands
                                .send_replace(extensions.commands().cloned().collect());
                            if complete {
                                state.extensions = Some(extensions);
                            }
                        }
                        Err(error) => eprintln!("xi: extension startup: {error}"),
                    }
                }
                Completion::Unit(reply, Ok(()))
            }
            Self::Compact(mut reply) => {
                tokio::select! {
                    biased;
                    () = reply.closed() => Completion::None,
                    result = session.compact_state(state, cancel) => Completion::Compact(reply, result),
                }
            }
            Self::ExtensionCommand(name, args, mut reply) => {
                tokio::select! {
                    biased;
                    () = reply.closed() => Completion::None,
                    result = session.command_state(state, &name, &args, cancel) => Completion::Extension(reply, result),
                }
            }
            Self::Title(update, reply) => {
                Completion::Unit(reply, session.apply_title(state, update))
            }
        }
    }
}

struct Completed {
    session: Session,
    state: State,
    replies: Vec<Reply>,
    result: Option<Result<TurnOutcome>>,
    completion: Completion,
}

#[derive(Default)]
struct Control {
    queue: VecDeque<Queued>,
    approvals: HashMap<ToolCallId, PendingAsk>,
    titles: Vec<title::Update>,
    cancel: CancellationToken,
    idle: Vec<oneshot::Sender<()>>,
    closed: bool,
    paused: bool,
}

pub(super) async fn run(
    inner: Weak<Inner>,
    mut state: Option<State>,
    mut rx: mpsc::UnboundedReceiver<Command>,
) {
    let mut control = Control::default();
    let mut jobs = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            command = rx.recv() => {
                let Some(command) = command else {
                    for prompt in control.queue.drain(..) { let _ = prompt.reply.send(Err(Error::Stopped)); }
                    break;
                };
                let Some(inner) = inner.upgrade() else { break; };
                let session = Session(inner);
                match command {
                    Command::Submit(prompt, _owner) => {
                        if session.0.stop.is_cancelled() { let _ = prompt.reply.send(Err(Error::Stopped)); }
                        else {
                            control.paused = false;
                            control.queue.push_back(prompt);
                            if let Some(state) = state.take() { start_turn(&session, state, &mut control, &mut jobs); }
                        }
                    }
                    Command::Cancel => { control.cancel.cancel(); control.paused = true; }
                    Command::Close | Command::Stop => {
                        if matches!(command, Command::Close) { control.cancel.cancel(); control.closed = true; }
                        for prompt in control.queue.drain(..) { let _ = prompt.reply.send(Err(Error::Stopped)); }
                    }
                    Command::WaitIdle(reply) => control.idle.push(reply),
                    Command::Drain(reply) => {
                        let queued = if control.cancel.is_cancelled() || session.0.stop.is_cancelled() { Vec::new() } else { control.queue.drain(..).collect() };
                        let _ = reply.send(queued);
                    }
                    Command::HasQueue(reply) => { let _ = reply.send(!control.queue.is_empty()); }
                    Command::Titles(reply) => { let _ = reply.send(std::mem::take(&mut control.titles)); }
                    Command::Register(call, pending) => { control.approvals.insert(call, pending); }
                    Command::Remove(call) => { control.approvals.remove(&call); }
                    Command::Approve { call, approved, remember, reply } => {
                        match control.approvals.get_mut(&call).ok_or(Error::NotPending).and_then(|pending| pending.validate(approved, remember).map(|()| pending)) {
                            Err(error) => { let _ = reply.send(Err(error)); }
                            Ok(pending) if remember == Remember::Workspace => {
                                pending.resolving = true;
                                let workspace = pending.workspace.clone(); let action = pending.grant.1.clone(); let tx = session.0.tx.clone();
                                tokio::spawn(async move {
                                    let result = tokio::task::spawn_blocking(move || config::append_workspace_allow(&workspace, action.kind(), &action.subject())).await.map_err(|_| Error::Stopped).and_then(|result| result);
                                    let _ = tx.send(Command::ApprovalWritten { call, approved, remember, result, reply });
                                });
                            }
                            Ok(_) => resolve(&mut control, call, approved, remember, reply),
                        }
                    }
                    Command::ApprovalWritten { call, approved, remember, result, reply } => {
                        match result {
                            Ok(()) => resolve(&mut control, call, approved, remember, reply),
                            Err(error) => {
                                if let Some(pending) = control.approvals.get_mut(&call) { pending.resolving = false; }
                                let _ = reply.send(Err(error));
                            }
                        }
                    }
                    Command::Idle(IdleCommand::Title(update, reply)) if state.is_none() => { control.titles.push(update); let _ = reply.send(Ok(())); }
                    Command::Idle(command) => {
                        if let Some(state) = state.take() { start_operation(&session, state, command, &mut control, &mut jobs); }
                        else { command.reject(Error::Running); }
                    }
                }
            }
            Some(completed) = jobs.join_next(), if !jobs.is_empty() => {
                match completed {
                    Ok(done) => {
                        let session = done.session.clone();
                        control.approvals.clear();
                        if !control.titles.is_empty() {
                            let updates = std::mem::take(&mut control.titles);
                            start_flush(done, updates, &mut jobs);
                        } else {
                            let again = !control.paused && done.result.as_ref().is_none_or(Result::is_ok)
                                && !session.0.stop.is_cancelled() && !control.queue.is_empty();
                            if again { start_turn(&session, done.state, &mut control, &mut jobs); }
                            else { state = Some(done.state); session.0.running.send_replace(false); }
                            done.completion.send();
                            if let Some(result) = done.result { for reply in done.replies { let _ = reply.send(result.clone()); } }
                        }
                    }
                    Err(_) => {
                        for prompt in control.queue.drain(..) { let _ = prompt.reply.send(Err(Error::Stopped)); }
                        break;
                    }
                }
            }
        }
        if control.closed && jobs.is_empty() {
            drop(state.take());
            if let Some(inner) = inner.upgrade() {
                inner.running.send_replace(false);
            }
            break;
        }
        if state.is_some() && control.queue.is_empty() {
            for reply in control.idle.drain(..) {
                let _ = reply.send(());
            }
        }
    }
}

fn resolve(
    control: &mut Control,
    call: ToolCallId,
    approved: bool,
    remember: Remember,
    reply: oneshot::Sender<Result<()>>,
) {
    let result = control
        .approvals
        .remove(&call)
        .ok_or(Error::NotPending)
        .and_then(|pending| {
            pending
                .answer
                .send(ApprovalAnswer {
                    approved,
                    remember,
                    grant: (!remember.is_once()).then_some(pending.grant),
                })
                .map_err(|_| Error::NotPending)
        });
    let _ = reply.send(result);
}

fn start_turn(
    session: &Session,
    mut state: State,
    control: &mut Control,
    jobs: &mut JoinSet<Completed>,
) {
    let Some(first) = control.queue.pop_front() else {
        return;
    };
    control.cancel = CancellationToken::new();
    let cancel = control.cancel.clone();
    state.tools_context.cancel = cancel.clone();
    session.0.running.send_replace(true);
    let session = session.clone();
    jobs.spawn(async move {
        let mut replies = Vec::new();
        let result = session.run(&mut state, &cancel, &mut replies, first).await;
        Completed {
            session,
            state,
            replies,
            result: Some(result),
            completion: Completion::None,
        }
    });
}

fn start_operation(
    session: &Session,
    mut state: State,
    command: IdleCommand,
    control: &mut Control,
    jobs: &mut JoinSet<Completed>,
) {
    control.cancel = CancellationToken::new();
    let cancel = control.cancel.clone();
    session.0.running.send_replace(true);
    let session = session.clone();
    jobs.spawn(async move {
        let completion = command.run(&session, &mut state, &cancel).await;
        Completed {
            session,
            state,
            replies: Vec::new(),
            result: None,
            completion,
        }
    });
}

fn start_flush(mut done: Completed, updates: Vec<title::Update>, jobs: &mut JoinSet<Completed>) {
    jobs.spawn(async move {
        for update in updates {
            if let Err(error) = done.session.apply_title(&mut done.state, update) {
                if let Some(result) = &mut done.result {
                    if result.is_ok() {
                        *result = Err(error);
                    }
                } else {
                    done.completion.fail(error);
                }
            }
        }
        done
    });
}

impl Completion {
    fn fail(&mut self, error: Error) {
        match self {
            Self::Unit(_, result) => {
                if result.is_ok() {
                    *result = Err(error);
                }
            }
            Self::Model(_, result) => {
                if result.is_ok() {
                    *result = Err(error);
                }
            }
            Self::Compact(_, result) => {
                if result.is_ok() {
                    *result = Err(error);
                }
            }
            Self::Extension(_, result) => {
                if result.is_ok() {
                    *result = Err(error);
                }
            }
            Self::None => eprintln!("xi: session title: {error}"),
        }
    }
}

impl Session {
    pub(super) fn refresh(&self, state: &State) {
        self.0
            .snapshot
            .send_modify(|snapshot| snapshot.refresh(state));
        self.0.config.send_replace(state.config.clone());
    }
}
