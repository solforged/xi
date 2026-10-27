//! Turn-side servicing of extension callbacks and egress decisions while the
//! actor remains available for approvals, cancellation and steering.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xi_policy::{Action, Taint};
use xi_protocol::BoxFuture;
use xi_protocol::event::{Approver, Remember, Usage};
use xi_protocol::methods::extension::{
    AgentRunParams, AgentRunResult, AppendCustomParams, CommandParams, CommandResult,
};
use xi_protocol::{Block, Decision, EventKind, ToolCallId};

use super::approvals::ActionKey;
use crate::egress::{Connect, Verdict};
use crate::extensions::Incoming;
use crate::{ApprovalMode, Backend, Error, PendingAsk, Result, Session, State, TurnOptions};

/// What the current wait is running, so an egress decision can name it.
#[derive(Clone, Copy)]
pub(crate) struct Current<'a> {
    /// The tool call whose process may connect.
    pub call: &'a ToolCallId,
    /// Its tool name, the key of a session grant.
    pub tool: &'a str,
}

/// An egress ask waiting for the user.
struct Waiting {
    id: ToolCallId,
    approved: bool,
    by: Approver,
    remember: Remember,
    grant: Option<(String, ActionKey)>,
    reply: oneshot::Sender<Verdict>,
}

impl Session {
    pub(crate) async fn prepare_extensions(&self, state: &mut State) -> Result<()> {
        if state.backend.is_none() {
            let config = state.config.clone();
            state.backend = Some(
                tokio::task::spawn_blocking(move || Backend::connect(&config))
                    .await
                    .map_err(|_| Error::Stopped)??
                    .into(),
            );
        }
        if !state.extensions.as_ref().is_some_and(|e| e.running()) {
            state.extensions = Some(
                self.0
                    .extensions
                    .load(
                        &state.config,
                        &state.tools_context,
                        state.log.file_path(),
                        &state.backend.as_ref().expect("connected backend").specs,
                    )
                    .await?,
            );
            self.0.commands.send_replace(
                state
                    .extensions
                    .as_ref()
                    .expect("loaded extensions")
                    .commands()
                    .cloned()
                    .collect(),
            );
        }
        Ok(())
    }

    /// Runs `future` while serving extension requests and egress proxy
    /// decisions that arrive meanwhile. Egress asks still open when `future`
    /// completes are cancelled, so every decision in the log gets its answer
    /// inside the wait that recorded it.
    pub(crate) async fn service<T>(
        &self,
        state: &mut State,
        future: impl Future<Output = Result<T>>,
        cancel: &CancellationToken,
        current: Option<Current<'_>>,
    ) -> Result<T> {
        let extensions = state.extensions.clone().unwrap_or_default();
        let mut scope = self.0.extensions.scope(&extensions);
        self.start_egress(state);
        // Cancelling this token after the wait answers open asks.
        let asks = cancel.child_token();
        let mut waiting: FuturesUnordered<BoxFuture<'static, Waiting>> = FuturesUnordered::new();
        tokio::pin!(future);
        let result = loop {
            let decided = tokio::select! {
                biased;
                incoming = scope.receiver.recv() => {
                    let Some(incoming) = incoming else { break Err(Error::Extension("extension scope closed".into())); };
                    match incoming {
                        Incoming::Request { extension, request, reply } => {
                            if request.method == "session/append_custom" {
                                let result = (|| {
                                    let params: AppendCustomParams = serde_json::from_value(request.params)?;
                                    self.append(state, EventKind::Custom { extension, kind: params.kind, data: params.data })?;
                                    Ok(serde_json::to_value(state.head)?)
                                })();
                                let _ = reply.send(result);
                            } else {
                                let task = self.child_request(state, request.params, cancel);
                                tokio::spawn(async move { let _ = reply.send(task.await); });
                            }
                            Ok(None)
                        }
                        Incoming::Connect(connect) => self.decide_egress(state, connect, None, &asks),
                    }
                }
                Some(connect) = state.egress.rx.recv() => self.decide_egress(state, connect, current, &asks),
                Some(done) = waiting.next() => self.finish_egress(state, done).map(|()| None),
                result = &mut future => break result,
            };
            match decided {
                Ok(Some(wait)) => waiting.push(wait),
                Ok(None) => {}
                Err(error) => break Err(error),
            }
        };
        asks.cancel();
        let mut result = result;
        while let Some(done) = waiting.next().await {
            if let Err(error) = self.finish_egress(state, done) {
                result = Err(error);
            }
        }
        result
    }

    /// Starts the tools listener once a runtime exists to accept on.
    fn start_egress(&self, state: &mut State) {
        let Some(listener) = &mut state.egress.listener else {
            return;
        };
        let tx = state.egress.tx.clone();
        listener.start(
            None,
            None,
            Box::new(move |connect| tx.send(connect).err().map(|error| error.0)),
        );
    }

    /// Records the decision for one proxy connection. Allowed and denied
    /// connections are answered at once; an ask returns the wait for its
    /// answer.
    fn decide_egress(
        &self,
        state: &mut State,
        connect: Connect,
        current: Option<Current<'_>>,
        cancel: &CancellationToken,
    ) -> Result<Option<BoxFuture<'static, Waiting>>> {
        let Connect {
            host,
            port,
            extension,
            permitted,
            reply,
        } = connect;
        let taint = if state.log.is_tainted(state.head) == Some(true) {
            Taint::Tainted
        } else {
            Taint::Clean
        };
        let action = ActionKey::Net {
            host: host.clone(),
            port,
        };
        let tool = current
            .map(|current| current.tool.to_owned())
            .or_else(|| extension.clone())
            .unwrap_or_default();
        let (mut decision, mut rule, taint_ask) = if permitted {
            let verdict = state
                .config
                .policy
                .evaluate(Action::Net { host: &host }, taint);
            (
                verdict.decision,
                verdict.rule.map(|rule| rule.to_string()),
                verdict.taint_downgraded,
            )
        } else {
            (Decision::Deny, Some("manifest".into()), false)
        };
        if decision == Decision::Ask
            && taint == Taint::Clean
            && state
                .grants
                .get(&tool)
                .is_some_and(|grants| grants.contains(&action))
        {
            decision = Decision::Allow;
            rule = Some("session".into());
        }
        let id = ToolCallId(format!("net-{}", xi_protocol::EventId::generate()));
        let during = current.map(|current| current.call.clone());
        let waiter = if decision == Decision::Ask
            && matches!(self.0.options.approval, ApprovalMode::Client)
        {
            let (tx, rx) = oneshot::channel();
            self.register_ask(
                id.clone(),
                PendingAsk {
                    answer: tx,
                    taint: taint_ask,
                    grant: (tool, action.clone()),
                    workspace: state.config.workspace.clone(),
                    trusted: state.config.trusted,
                    resolving: false,
                },
            );
            Some(rx)
        } else {
            None
        };
        self.append(
            state,
            EventKind::ToolDecision {
                call: id.clone(),
                action: action.to_string(),
                decision,
                rule: rule.clone(),
                taint: taint_ask,
                during,
                extension,
            },
        )?;
        match decision {
            Decision::Allow => {
                let _ = reply.send(Verdict::Allow);
                Ok(None)
            }
            Decision::Deny => {
                let _ = reply.send(Verdict::Deny(format!(
                    "egress to {host}:{port} denied by policy ({})",
                    rule.as_deref().unwrap_or("no matching rule")
                )));
                Ok(None)
            }
            Decision::Ask => match (self.0.options.approval, waiter) {
                (ApprovalMode::Headless(approved), _) => {
                    self.finish_egress(
                        state,
                        Waiting {
                            id,
                            approved,
                            by: Approver::Headless,
                            remember: Remember::Once,
                            grant: None,
                            reply,
                        },
                    )?;
                    Ok(None)
                }
                (ApprovalMode::Client, Some(rx)) => {
                    let cancel = cancel.clone();
                    let stop = self.0.stop.clone();
                    let timeout = state.config.daemon.approval_timeout;
                    Ok(Some(Box::pin(async move {
                        let timeout = async {
                            match timeout {
                                Some(seconds) => {
                                    tokio::time::sleep(Duration::from_secs(seconds)).await
                                }
                                None => std::future::pending::<()>().await,
                            }
                        };
                        let (approved, by, remember, grant) = tokio::select! {
                            biased;
                            () = cancel.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                            () = stop.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                            result = rx => match result {
                                Ok(answer) => (answer.approved, Approver::User, answer.remember, answer.grant),
                                Err(_) => (false, Approver::Cancelled, Remember::Once, None),
                            },
                            () = timeout => (false, Approver::Timeout, Remember::Once, None),
                        };
                        Waiting {
                            id,
                            approved,
                            by,
                            remember,
                            grant,
                            reply,
                        }
                    })))
                }
                (ApprovalMode::Client, None) => unreachable!("client asks register a waiter"),
            },
        }
    }

    /// Records an egress ask's answer and releases the connection.
    fn finish_egress(&self, state: &mut State, done: Waiting) -> Result<()> {
        self.remove_ask(done.id.clone());
        let verdict = if done.approved {
            Verdict::Allow
        } else {
            Verdict::Deny(match done.by {
                Approver::Timeout => "egress approval timed out".into(),
                Approver::User | Approver::Headless => "egress denied".into(),
                _ => "egress approval cancelled".into(),
            })
        };
        self.append(
            state,
            EventKind::ApprovalResolved {
                call: done.id,
                approved: done.approved,
                by: done.by,
                remember: done.remember,
            },
        )?;
        if let Some(grant) = done.grant {
            state.grants.entry(grant.0).or_default().insert(grant.1);
        }
        let _ = done.reply.send(verdict);
        Ok(())
    }

    fn child_request(
        &self,
        state: &State,
        params: Value,
        cancel: &CancellationToken,
    ) -> BoxFuture<'static, Result<Value>> {
        let child = serde_json::from_value(params)
            .map(|params| self.run_child(state, params, cancel))
            .map_err(Error::from);
        Box::pin(async move {
            let (result, _) = child?.await?;
            Ok(serde_json::to_value(result)?)
        })
    }

    /// Prepares a linked child from the parent's state now, and returns a
    /// future that runs its prompt to the end of its turn. The future also
    /// says whether the child's context ended tainted.
    pub(crate) fn run_child(
        &self,
        state: &State,
        params: AgentRunParams,
        cancel: &CancellationToken,
    ) -> BoxFuture<'static, Result<(AgentRunResult, bool)>> {
        let mut config = state.config.clone();
        let parent = state.backend.as_ref().expect("connected backend");
        let parent_provider = parent.provider.clone();
        let parent_model = parent.model.clone();
        let parent_specs = parent.specs.clone();
        let parent_tools = parent.tools.clone();
        let directory = state
            .log
            .file_path()
            .parent()
            .map(std::path::Path::to_owned);
        let bus = self.0.bus.clone();
        let options = self.0.options.clone();
        let parent_id = self.id();
        let parent_snapshot = self.0.snapshot.clone();
        let prepared = move || -> Result<_> {
            let names = params.tools.unwrap_or_else(|| {
                parent_specs
                    .iter()
                    .filter(|s| xi_tools::is_builtin(&s.name))
                    .map(|s| s.name.clone())
                    .collect()
            });
            let mut tools = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for name in &names {
                if !xi_tools::is_builtin(name) || !parent_tools.contains_key(name) {
                    return Err(Error::Config(format!(
                        "child tool {name} is not one of the parent's built-in tools"
                    )));
                }
                if !seen.insert(name) {
                    return Err(Error::Config(format!("duplicate child tool {name}")));
                }
                tools.push(parent_tools[name].clone());
            }
            config.tools = names;
            let mut backend = Backend {
                provider: parent_provider,
                model: parent_model,
                tools,
            };
            if let Some(model) = params.model.map(|model| config.resolve(&model))
                && model != config.model
            {
                config.model = model;
                let connected = Backend::connect(&config)?;
                backend.provider = connected.provider;
                backend.model = connected.model;
            }
            let child = Self::create_linked(
                config,
                directory.as_deref(),
                bus,
                TurnOptions {
                    max_turns: options.max_turns,
                    approval: ApprovalMode::Headless(false),
                    leaser: options.leaser,
                    auto_title: false,
                },
                Some(backend),
                Some((parent_id, parent_snapshot)),
            )?;
            Ok((child, params.prompt))
        };
        // Reserve order before spawning: scheduler order must not renumber children.
        let ticket = self
            .0
            .child_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let order = self.0.child_ready.clone();
        let mut ready = order.subscribe();
        let prepared = tokio::spawn(async move {
            ready
                .wait_for(|next| *next == ticket)
                .await
                .map_err(|_| Error::Stopped)?;
            let result = tokio::task::spawn_blocking(prepared)
                .await
                .map_err(|_| Error::Stopped)
                .and_then(|result| result);
            order.send_replace(ticket + 1);
            result
        });
        let cancel = cancel.clone();
        let stop = self.0.stop.clone();
        Box::pin(async move {
            let (child, prompt) = prepared.await.map_err(|_| Error::Stopped)??;
            let mut reply = child.submit(vec![Block::Text { text: prompt }]);
            let outcome = tokio::select! {
                biased;
                () = stop.cancelled() => { child.stop_requests(); reply.await }
                () = cancel.cancelled() => { child.cancel(); reply.await }
                result = &mut reply => result,
            }
            .map_err(|_| Error::Stopped)??;
            child.shutdown_extensions().await;
            let info = child.info().await;
            let tainted = info.tainted;
            let mut usage = Usage::default();
            for event in &info.events {
                let tokens = match event.kind {
                    EventKind::AssistantMessage { usage, .. }
                    | EventKind::Compaction {
                        usage: Some(usage), ..
                    } => usage,
                    _ => continue,
                };
                usage.input += tokens.input;
                usage.output += tokens.output;
                usage.cache_read += tokens.cache_read;
                usage.cache_write += tokens.cache_write;
            }
            let result = AgentRunResult {
                session: child.id(),
                path: info.path,
                text: outcome.text,
                failed: outcome.failed || outcome.cancelled,
                usage,
            };
            child.close();
            child.wait_idle().await;
            Ok((result, tainted))
        })
    }

    /// Routes a slash command without its leading slash. `None` means unknown.
    pub async fn command(&self, name: &str, args: &str) -> Result<Option<CommandResult>> {
        let (reply, rx) = oneshot::channel();
        self.idle_operation(
            super::actor::IdleCommand::ExtensionCommand(name.into(), args.into(), reply),
            rx,
        )
        .await
    }

    pub(crate) async fn command_state(
        &self,
        state: &mut State,
        name: &str,
        args: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<CommandResult>> {
        self.prepare_extensions(state).await?;
        let extensions = state.extensions.clone().unwrap_or_default();
        let params = CommandParams {
            session: self.id(),
            name: name.into(),
            args: args.into(),
        };
        self.service(state, extensions.command(&params, cancel), cancel, None)
            .await
    }

    /// Runs `compact` hooks, serving full-duplex custom appends and child
    /// requests. The last summary supplied replaces the kernel's own.
    pub(crate) async fn extension_summary_locked(
        &self,
        state: &mut State,
        tokens_before: u64,
        previous_summary: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>> {
        self.prepare_extensions(state).await?;
        let extensions = state.extensions.clone().unwrap_or_default();
        self.service(
            state,
            extensions.compact(self.id(), tokens_before, previous_summary, cancel),
            cancel,
            None,
        )
        .await
    }
}
