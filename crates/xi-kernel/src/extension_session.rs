//! Session-side servicing while a turn owns the log: extension callbacks and
//! egress proxy decisions. Custom appends and connection decisions need the
//! log, so they are serviced here rather than trying to reacquire the
//! session mutex.

use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use xi_policy::{Action, ActionKind, Taint};
use xi_protocol::event::{Approver, Remember, Usage};
use xi_protocol::methods::extension::{
    AgentRunParams, AgentRunResult, AppendCustomParams, CommandParams, CommandResult,
};
use xi_protocol::{Block, Decision, EventKind, ToolCallId};

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
    grant: Option<(String, String)>,
    reply: oneshot::Sender<Verdict>,
}

impl Session {
    pub(crate) async fn prepare_extensions(&self, state: &mut State) -> Result<()> {
        if state.backend.is_none() {
            state.backend = Some(Backend::connect(&state.config)?.into());
        }
        if !state.extensions.as_ref().is_some_and(|e| e.running()) {
            state.extensions = Some(
                self.0
                    .bus
                    .1
                    .load(
                        &state.config,
                        &state.tools_context,
                        state.log.file_path(),
                        &state.backend.as_ref().expect("connected backend").specs,
                    )
                    .await?,
            );
            *self.0.commands.lock().expect("command catalog") = state
                .extensions
                .as_ref()
                .expect("loaded extensions")
                .commands()
                .cloned()
                .collect();
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
        let mut scope = self.0.bus.1.scope(self.id(), &extensions);
        self.start_egress(state);
        let mut rx = state.egress.rx.take().expect("egress receiver");
        // Cancelling this token after the wait answers open asks.
        let asks = cancel.child_token();
        let mut waiting: FuturesUnordered<BoxFuture<'static, Waiting>> = FuturesUnordered::new();
        tokio::pin!(future);
        let result = loop {
            let decided = tokio::select! {
                biased;
                incoming = scope.receiver.recv() => {
                    let Some(incoming) = incoming else { break Err(Error::Config("extension scope closed".into())); };
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
                Some(connect) = rx.recv() => self.decide_egress(state, connect, current, &asks),
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
        state.egress.rx = Some(rx);
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
        let action = format!("net {host}:{port}");
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
            && state.grants.contains(&(tool.clone(), action.clone()))
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
            self.0
                .control
                .lock()
                .expect("control lock")
                .approvals
                .insert(
                    id.clone(),
                    PendingAsk {
                        answer: tx,
                        taint: taint_ask,
                        kind: ActionKind::Net,
                        subject: host.clone(),
                        grant: (tool, action.clone()),
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
                call: id.clone(),
                action,
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
        self.0
            .control
            .lock()
            .expect("control lock")
            .approvals
            .remove(&done.id);
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
            state.grants.insert(grant);
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
        let prepared = (|| -> Result<_> {
            let params: AgentRunParams = serde_json::from_value(params)?;
            let mut config = state.config.clone();
            config.extensions.clear();
            let parent = state.backend.as_ref().expect("connected backend");
            let names = params.tools.unwrap_or_else(|| {
                parent
                    .specs
                    .iter()
                    .filter(|s| xi_tools::is_builtin(&s.name))
                    .map(|s| s.name.clone())
                    .collect()
            });
            let mut tools = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for name in &names {
                if !xi_tools::is_builtin(name) || !parent.tools.contains_key(name) {
                    return Err(Error::Config(format!(
                        "child tool {name} is not one of the parent's built-in tools"
                    )));
                }
                if !seen.insert(name) {
                    return Err(Error::Config(format!("duplicate child tool {name}")));
                }
                tools.push(parent.tools[name].clone());
            }
            config.tools = names;
            let mut backend = Backend {
                provider: parent.provider.clone(),
                model: parent.model.clone(),
                tools,
            };
            if let Some(model) = params.model
                && model != config.model
            {
                config.model = model;
                let connected = Backend::connect(&config)?;
                backend.provider = connected.provider;
                backend.model = connected.model;
            }
            let child = Self::create_linked(
                config,
                state.log.file_path().parent(),
                self.0.bus.clone(),
                TurnOptions {
                    max_turns: self.0.options.max_turns,
                    approval: ApprovalMode::Headless(false),
                    leaser: self.0.options.leaser.clone(),
                },
                Some(backend),
                Some(self.id()),
            )?;
            Ok((child, params.prompt))
        })();
        let cancel = cancel.clone();
        let stop = self.0.stop.clone();
        Box::pin(async move {
            let (child, prompt) = prepared?;
            let mut reply = child.submit(vec![Block::Text { text: prompt }]);
            let outcome = tokio::select! {
                biased;
                () = stop.cancelled() => { child.stop_requests(); reply.await }
                () = cancel.cancelled() => { child.cancel(); reply.await }
                result = &mut reply => result,
            }
            .map_err(|_| Error::Config("child turn stopped".into()))?
            .map_err(Error::Config)?;
            let info = child.info().await;
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
            Ok(serde_json::to_value(AgentRunResult {
                session: child.id(),
                path: info.path,
                text: outcome.text,
                failed: outcome.failed || outcome.cancelled,
                usage,
            })?)
        })
    }

    /// Routes a slash command without its leading slash. `None` means unknown.
    pub async fn command(&self, name: &str, args: &str) -> Result<Option<CommandResult>> {
        let mut state = self.0.state.try_lock().map_err(|_| Error::Running)?;
        if self.is_running() {
            return Err(Error::Running);
        }
        self.prepare_extensions(&mut state).await?;
        let extensions = state.extensions.clone().unwrap_or_default();
        let params = CommandParams {
            session: self.id(),
            name: name.into(),
            args: args.into(),
        };
        let cancel = CancellationToken::new();
        self.service(
            &mut state,
            extensions.command(&params, &cancel),
            &cancel,
            None,
        )
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
