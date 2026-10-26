//! Session-side extension callbacks. The turn owns the log, so custom appends
//! are serviced here rather than trying to reacquire the session mutex.

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use xi_protocol::event::Usage;
use xi_protocol::methods::extension::{
    AgentRunParams, AgentRunResult, AppendCustomParams, CommandParams, CommandResult,
};
use xi_protocol::{Block, EventKind};

use crate::{ApprovalMode, Backend, Error, Result, Session, State, TurnOptions};

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
        }
        Ok(())
    }

    pub(crate) async fn extension_wait<T>(
        &self,
        state: &mut State,
        future: impl Future<Output = Result<T>>,
        cancel: &CancellationToken,
    ) -> Result<T> {
        let extensions = state.extensions.clone().unwrap_or_default();
        let mut scope = self.0.bus.1.scope(self.id(), &extensions);
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                incoming = scope.receiver.recv() => {
                    let Some(incoming) = incoming else { return Err(Error::Config("extension scope closed".into())); };
                    if incoming.request.method == "session/append_custom" {
                        let result = (|| {
                            let params: AppendCustomParams = serde_json::from_value(incoming.request.params)?;
                            self.append(state, EventKind::Custom { extension: incoming.extension, kind: params.kind, data: params.data })?;
                            Ok(serde_json::to_value(state.head)?)
                        })();
                        let _ = incoming.reply.send(result);
                    } else {
                        let task = self.child_request(state, incoming.request.params, cancel);
                        tokio::spawn(async move { let _ = incoming.reply.send(task.await); });
                    }
                }
                result = &mut future => return result,
            }
        }
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
        self.extension_wait(&mut state, extensions.command(&params, &cancel), &cancel)
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
        self.extension_wait(
            state,
            extensions.compact(self.id(), tokens_before, previous_summary, cancel),
            cancel,
        )
        .await
    }
}
