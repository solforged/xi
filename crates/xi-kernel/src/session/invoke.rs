//! Tool policy decisions, approval waits and runtime dispatch.

use super::*;
use xi_protocol::methods::extension::{HookEvent, HookOutcome, InvokeParams};

pub(super) trait KernelTool: Send + Sync {
    fn run<'a>(
        &'a self,
        session: &'a Session,
        state: &'a mut State,
        call: &'a ToolCallId,
        input: serde_json::Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, xi_tools::Ran>;
}

struct Search;
impl KernelTool for task::Task {
    fn run<'a>(
        &'a self,
        session: &'a Session,
        state: &'a mut State,
        call: &'a ToolCallId,
        input: serde_json::Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, xi_tools::Ran> {
        Box::pin(session.run_tasks(
            state,
            input,
            cancel,
            extension_session::Current {
                call,
                tool: task::NAME,
            },
        ))
    }
}
impl KernelTool for Search {
    fn run<'a>(
        &'a self,
        session: &'a Session,
        state: &'a mut State,
        _: &'a ToolCallId,
        input: serde_json::Value,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, xi_tools::Ran> {
        Box::pin(async move {
            xi_tools::Ran {
                output: session.run_search(state, input, cancel).await,
                spill: None,
            }
        })
    }
}

/// The kernel's tool registrations, shared by configuration and backend wiring.
pub(super) enum KernelKind {
    Task,
    Search,
}

impl KernelKind {
    pub(super) fn named(name: &str) -> Option<Self> {
        match name {
            task::NAME => Some(Self::Task),
            web_search::NAME => Some(Self::Search),
            _ => None,
        }
    }

    pub(super) fn catalog(self, config: &Config) -> Arc<dyn Tool> {
        match self {
            Self::Task => Arc::new(task::Task),
            Self::Search => Arc::new(web_search::WebSearch::new(config)),
        }
    }

    fn runtime(self) -> Arc<dyn KernelTool> {
        match self {
            Self::Task => Arc::new(task::Task),
            Self::Search => Arc::new(Search),
        }
    }
}

pub(super) fn kernel_tools(
    tools: &HashMap<String, Arc<dyn Tool>>,
) -> HashMap<String, Arc<dyn KernelTool>> {
    tools
        .keys()
        .filter_map(|name| KernelKind::named(name).map(|kind| (name.clone(), kind.runtime())))
        .collect()
}

enum Decision {
    Run(DecisionEvent),
    Ask(PendingAsk, DecisionEvent, oneshot::Receiver<ApprovalAnswer>),
    Deny(String, DecisionEvent),
}
struct DecisionEvent {
    action: ActionKey,
    decision: xi_protocol::Decision,
    rule: Option<String>,
    taint: bool,
}
enum Approval {
    Granted,
    Denied(String),
    Cancelled,
}

fn decide(
    state: &State,
    name: &str,
    input: &serde_json::Value,
) -> std::result::Result<Decision, String> {
    let backend = state.backend.as_ref().expect("connected backend");
    let crossings = backend.tools.get(name).map_or_else(
        || Ok(Vec::new()),
        |tool| tool.crossings(input, &state.tools_context),
    )?;
    let taint = if state.log.is_tainted(state.head) == Some(true) {
        Taint::Tainted
    } else {
        Taint::Clean
    };
    let mut verdict = state.config.policy.evaluate(Action::Tool { name }, taint);
    let mut action = ActionKey::Tool(name.into());
    for crossing in &crossings {
        let candidate = state.config.policy.evaluate(
            match crossing {
                Crossing::Read(path) => Action::Read { path },
                Crossing::Write(path) => Action::Write { path },
                Crossing::Exec(command) => Action::Exec { command },
                Crossing::Net(host) => Action::Net { host },
            },
            taint,
        );
        if candidate.decision > verdict.decision {
            verdict = candidate;
            action = ActionKey::crossing(crossing);
        }
    }
    let mut decision = verdict.decision;
    let mut rule = verdict.rule.map(|rule| rule.to_string());
    if decision == xi_protocol::Decision::Ask
        && !verdict.taint_downgraded
        && !(taint == Taint::Tainted && matches!(action, ActionKey::Net { .. }))
        && state
            .grants
            .get(name)
            .is_some_and(|grants| grants.contains(&action))
    {
        decision = xi_protocol::Decision::Allow;
        rule = Some("session".into());
    }
    let event = DecisionEvent {
        action,
        decision,
        rule,
        taint: verdict.taint_downgraded,
    };
    Ok(match decision {
        xi_protocol::Decision::Allow => Decision::Run(event),
        xi_protocol::Decision::Deny => Decision::Deny(
            format!(
                "denied by policy ({})",
                event.rule.as_deref().unwrap_or("no matching rule")
            ),
            event,
        ),
        xi_protocol::Decision::Ask => {
            let (answer, waiter) = oneshot::channel();
            Decision::Ask(
                PendingAsk {
                    answer,
                    taint: verdict.taint_downgraded,
                    grant: (name.into(), event.action.clone()),
                    workspace: state.config.workspace.clone(),
                    trusted: state.config.trusted,
                    resolving: false,
                },
                event,
                waiter,
            )
        }
    })
}

impl Session {
    pub(crate) async fn invoke(
        &self,
        state: &mut State,
        call: ToolCallId,
        name: &str,
        mut input: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let extensions = state.extensions.clone().unwrap_or_default();
        let current = extension_session::Current {
            call: &call,
            tool: name,
        };
        if extensions.has_hook(xi_protocol::manifest::HookPoint::PreTool) {
            let event = HookEvent::PreTool {
                call: call.clone(),
                tool: name.into(),
                input: input.clone(),
            };
            match self
                .service(
                    state,
                    extensions.hook(self.id(), event, cancel),
                    cancel,
                    Some(current),
                )
                .await
            {
                Ok(HookOutcome::RewriteInput { input: rewritten }) => input = rewritten,
                Ok(HookOutcome::Deny { reason }) => {
                    self.append(
                        state,
                        EventKind::ToolDecision {
                            call: call.clone(),
                            action: ActionKey::Tool(name.into()).to_string(),
                            decision: xi_protocol::Decision::Deny,
                            rule: Some("extension pre_tool".into()),
                            taint: false,
                            during: None,
                            extension: None,
                        },
                    )?;
                    return self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output(reason),
                            spill: None,
                        },
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    return self.append(
                        state,
                        EventKind::ToolResult {
                            call,
                            output: error_output(error.to_string()),
                            spill: None,
                        },
                    );
                }
            }
        }

        let backend = state.backend.as_ref().expect("connected backend");
        if !backend.tools.contains_key(name) && !extensions.has_tool(name) {
            return self.append(
                state,
                EventKind::ToolResult {
                    call,
                    output: error_output(format!("unknown tool {name}")),
                    spill: None,
                },
            );
        }
        let plan = match decide(state, name, &input) {
            Ok(plan) => plan,
            Err(error) => {
                return self.append(
                    state,
                    EventKind::ToolResult {
                        call,
                        output: error_output(error),
                        spill: None,
                    },
                );
            }
        };
        match self.await_approval(state, &call, plan, cancel).await? {
            Approval::Granted => {}
            Approval::Denied(reason) => {
                return self.append(
                    state,
                    EventKind::ToolResult {
                        call,
                        output: error_output(reason),
                        spill: None,
                    },
                );
            }
            Approval::Cancelled => {
                return self.append(
                    state,
                    EventKind::ToolResult {
                        call,
                        output: error_output("cancelled"),
                        spill: None,
                    },
                );
            }
        }
        // Hooks consume input after the runner has consumed its own owned value.
        // Keeping one copy here avoids copies when no post-tool hook exists.
        let post_input = extensions
            .has_hook(xi_protocol::manifest::HookPoint::PostTool)
            .then(|| input.clone());
        let mut result = self.dispatch(state, &call, name, input, cancel).await;
        if !cancel.is_cancelled()
            && let Some(input) = post_input
        {
            let event = HookEvent::PostTool {
                call: call.clone(),
                tool: name.into(),
                input,
                output: result.output.clone(),
            };
            match self
                .service(
                    state,
                    extensions.hook(self.id(), event, cancel),
                    cancel,
                    Some(current),
                )
                .await
            {
                Ok(HookOutcome::ReplaceOutput { output }) => {
                    result.output = output;
                    result.spill = None;
                }
                Ok(_) => {}
                Err(error) => {
                    let untrusted = result.output.untrusted;
                    result.output = error_output(error.to_string());
                    result.output.untrusted = untrusted;
                    result.spill = None;
                }
            }
        }
        if result.spill.is_none()
            && result
                .output
                .content
                .iter()
                .all(|block| matches!(block, OutputBlock::Text { .. }))
            && result
                .output
                .content
                .iter()
                .map(|block| match block {
                    OutputBlock::Text { text } => text.len().saturating_add(1),
                    _ => 0,
                })
                .sum::<usize>()
                > state.tools_context.output_limit
        {
            let text = result
                .output
                .content
                .iter()
                .filter_map(|block| match block {
                    OutputBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.len() > state.tools_context.output_limit {
                let (text, spill) = xi_tools::truncate(&call, text, &state.tools_context);
                result.output.content = vec![OutputBlock::Text { text }];
                result.spill = spill;
            }
        }
        let untrusted = result.output.untrusted;
        self.append(
            state,
            EventKind::ToolResult {
                call: call.clone(),
                output: result.output,
                spill: result.spill,
            },
        )?;
        if untrusted {
            self.append(
                state,
                EventKind::Tainted {
                    call: Some(call),
                    reason: format!("untrusted output from {name}"),
                },
            )?;
        }
        Ok(())
    }

    async fn await_approval(
        &self,
        state: &mut State,
        call: &ToolCallId,
        decision: Decision,
        cancel: &CancellationToken,
    ) -> Result<Approval> {
        let (event, ask, denied, waiter) = match decision {
            Decision::Ask(pending, event, waiter) => {
                if matches!(self.0.options.approval, ApprovalMode::Client) {
                    self.register_ask(call.clone(), pending);
                }
                (event, true, None, Some(waiter))
            }
            Decision::Deny(reason, event) => (event, false, Some(reason), None),
            Decision::Run(event) => (event, false, None, None),
        };
        self.append(
            state,
            EventKind::ToolDecision {
                call: call.clone(),
                action: event.action.to_string(),
                decision: event.decision,
                rule: event.rule,
                taint: event.taint,
                during: None,
                extension: None,
            },
        )?;
        if let Some(reason) = denied {
            return Ok(Approval::Denied(reason));
        }
        if !ask {
            return Ok(Approval::Granted);
        }
        let (approved, by, remember, grant) = match self.0.options.approval {
            ApprovalMode::Headless(approved) => {
                (approved, Approver::Headless, Remember::Once, None)
            }
            ApprovalMode::Client => {
                let timeout = async {
                    match state.config.daemon.approval_timeout {
                        Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                let result = tokio::select! {
                    biased;
                    () = cancel.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                    () = self.0.stop.cancelled() => (false, Approver::Cancelled, Remember::Once, None),
                    result = waiter.expect("client waiter") => match result {
                        Ok(answer) => (answer.approved, Approver::User, answer.remember, answer.grant),
                        Err(_) => (false, Approver::Cancelled, Remember::Once, None),
                    },
                    () = timeout => (false, Approver::Timeout, Remember::Once, None),
                };
                self.remove_ask(call.clone());
                result
            }
        };
        let cancelled = matches!(by, Approver::Cancelled);
        self.append(
            state,
            EventKind::ApprovalResolved {
                call: call.clone(),
                approved,
                by,
                remember,
            },
        )?;
        if let Some(grant) = grant {
            state.grants.entry(grant.0).or_default().insert(grant.1);
        }
        Ok(if cancelled {
            Approval::Cancelled
        } else if approved {
            Approval::Granted
        } else {
            Approval::Denied("denied by policy".into())
        })
    }

    async fn dispatch(
        &self,
        state: &mut State,
        call: &ToolCallId,
        name: &str,
        input: serde_json::Value,
        cancel: &CancellationToken,
    ) -> xi_tools::Ran {
        let backend = state.backend.as_ref().expect("connected backend");
        let kernel = backend.kernel_tools.get(name).cloned();
        let tool = backend.tools.get(name).cloned();
        let current = extension_session::Current { call, tool: name };
        if let Some(kernel) = kernel {
            return kernel.run(self, state, call, input, cancel).await;
        }
        if let Some(tool) = tool {
            let context = state.tools_context.clone();
            let run = async {
                Ok(tokio::select! {
                    biased;
                    () = cancel.cancelled() => xi_tools::Ran { output: error_output("cancelled"), spill: None },
                    result = tool.run(call, input, &context) => result,
                })
            };
            self.service(state, run, cancel, Some(current))
                .await
                .unwrap_or_else(|error| xi_tools::Ran {
                    output: error_output(error.to_string()),
                    spill: None,
                })
        } else {
            let extensions = state.extensions.clone().unwrap_or_default();
            let params = InvokeParams {
                session: self.id(),
                call: call.clone(),
                tool: name.into(),
                input,
            };
            let output = self
                .service(
                    state,
                    extensions.invoke(&params, cancel),
                    cancel,
                    Some(current),
                )
                .await
                .unwrap_or_else(|error| error_output(error.to_string()));
            xi_tools::Ran {
                output,
                spill: None,
            }
        }
    }
}
