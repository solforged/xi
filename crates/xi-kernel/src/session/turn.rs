//! Model turns, streaming and retries.

use super::*;

impl Session {
    async fn drain(&self, state: &mut State, replies: &mut Vec<Reply>) -> Result<bool> {
        let queued = self.queued().await;
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
    pub(super) async fn run(
        &self,
        state: &mut State,
        cancel: &CancellationToken,
        replies: &mut Vec<Reply>,
        first: Queued,
    ) -> Result<TurnOutcome> {
        // Even an immediate cancel acknowledges the initiating prompt, but not
        // later steering. The first prompt is the current turn's input.
        replies.push(first.reply);
        self.append(
            state,
            EventKind::UserMessage {
                content: first.content,
            },
        )?;
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
            self.drain(state, replies).await?;
            self.apply_titles(state).await?;
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
                let config = state.config.clone();
                state.backend = Some(
                    tokio::task::spawn_blocking(move || Backend::connect(&config))
                        .await
                        .map_err(|_| Error::Stopped)??
                        .into(),
                );
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
                if !self.has_queue().await {
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
        self.apply_titles(state).await?;
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
                web_search: false,
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
                        return Err(Error::Provider(error));
                    }
                }
                result => return result,
            }
        }
    }

    pub(crate) async fn stream(
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
                () = self.0.stop.cancelled() => return Err(Error::Stopped),
                () = cancel.cancelled() => return Ok(cancelled(Vec::new())),
                guard = leaser.lease(endpoint) => guard?,
            })
        } else {
            None
        };
        for attempt in 0..=4 {
            if self.0.stop.is_cancelled() {
                return Err(Error::Stopped);
            }
            if cancel.is_cancelled() {
                return Ok(cancelled(Vec::new()));
            }
            if attempt > 0 && publish {
                self.publish_stream(Notification::Reset(ResetParams {
                    session: self.id(),
                    attempt,
                    sequence: self.0.sequence.fetch_add(1, Ordering::Relaxed) + 1,
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
                self.publish_stream(Notification::Delta(DeltaParams {
                    session: self.id(),
                    delta,
                    attempt,
                    sequence: self.0.sequence.fetch_add(1, Ordering::Relaxed) + 1,
                }));
            };
            let result = tokio::select! {
                biased;
                () = self.0.stop.cancelled() => return Err(Error::Stopped),
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
                        () = self.0.stop.cancelled() => return Err(Error::Stopped),
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
    pub(crate) async fn run_tasks(
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
}
