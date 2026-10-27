//! Incremental attach state. Appends copy only the new event; full history is
//! copied only when a caller explicitly requests a materialized snapshot.

use super::*;
use xi_protocol::methods::client::{ChildEvents, PendingApproval};

pub(super) struct Snapshot {
    pub(super) details: SessionDetails,
    events: Vec<xi_protocol::Event>,
    partial: Vec<Block>,
    sequence: u64,
    tools: HashMap<ToolCallId, String>,
    pending: Vec<PendingApproval>,
    children: Vec<ChildEvents>,
    tainted: bool,
}

impl Snapshot {
    pub(super) fn new(id: SessionId, state: &State) -> Self {
        let parent_session = match state.log.root().kind {
            EventKind::SessionStarted { parent_session, .. } => parent_session,
            _ => None,
        };
        let mut snapshot = Self {
            details: SessionDetails {
                session: id,
                path: state.log.file_path().to_owned(),
                head: state.head,
                cwd: state.config.workspace.clone(),
                trusted: state.config.trusted,
                model: state.config.model.clone(),
                reasoning: state.config.reasoning,
                sandbox_enabled: state.config.sandbox.enabled,
                model_info: state.config.registry().model(&state.config.model).cloned(),
                parent_session,
            },
            events: Vec::with_capacity(state.log.events().len()),
            partial: Vec::new(),
            sequence: 0,
            tools: HashMap::new(),
            pending: Vec::new(),
            children: Vec::new(),
            tainted: false,
        };
        for event in state.log.events() {
            snapshot.reduce(event);
            snapshot.events.push(event.clone());
        }
        snapshot.refresh(state);
        snapshot
    }

    pub(super) fn refresh(&mut self, state: &State) {
        self.details.head = state.head;
        self.details.model.clone_from(&state.config.model);
        self.details.reasoning = state.config.reasoning;
        // Sandbox events audit prior workers; they never restore an override.
        self.details.sandbox_enabled = state.config.sandbox.enabled;
        self.details.model_info = state
            .backend
            .as_ref()
            .map(|backend| backend.model.clone())
            .or_else(|| state.config.registry().model(&state.config.model).cloned());
        self.tainted = state.log.is_tainted(state.head) == Some(true);
    }

    pub(super) fn append(&mut self, event: &xi_protocol::Event, state: &State) {
        self.reduce(event);
        self.events.push(event.clone());
        self.details.head = state.head;
        self.tainted |= matches!(event.kind, EventKind::Tainted { .. });
    }

    fn reduce(&mut self, event: &xi_protocol::Event) {
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
            EventKind::TurnFailed { .. } => self.partial.clear(),
            EventKind::Compaction { .. } => self.children.clear(),
            EventKind::SandboxChanged { enabled } => {
                self.details.sandbox_enabled = *enabled;
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
                self.pending.push(PendingApproval {
                    session: self.details.session,
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
                    trusted: self.details.trusted,
                });
            }
            EventKind::ApprovalResolved { call, .. } => {
                self.pending.retain(|pending| pending.call != *call)
            }
            EventKind::ToolResult { call, .. } => {
                self.pending.retain(|pending| pending.call != *call);
                self.tools.remove(call);
                self.children.clear();
            }
            _ => {}
        }
    }

    pub(super) fn stream(&mut self, notification: &Notification) {
        match notification {
            Notification::Delta(params) => {
                self.sequence = params.sequence;
                match &params.delta {
                    Delta::Text { text } => push_partial(&mut self.partial, text, false),
                    Delta::Thinking { text } => push_partial(&mut self.partial, text, true),
                    Delta::ToolInput { .. } => {}
                }
            }
            Notification::Reset(params) => {
                self.sequence = params.sequence;
                self.partial.clear();
            }
            Notification::Event(_) => {}
        }
    }

    pub(super) fn child(&mut self, params: &EventParams) {
        if let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.session == params.session)
        {
            child.events.push(params.event.clone());
        } else if matches!(params.event.kind, EventKind::SessionStarted { parent_session: Some(parent), .. } if parent == self.details.session)
        {
            self.children.push(ChildEvents {
                session: params.session,
                events: vec![params.event.clone()],
            });
        }
    }

    pub(super) fn materialize(&self) -> SessionSnapshot {
        SessionSnapshot {
            session: self.details.session,
            path: self.details.path.clone(),
            head: self.details.head,
            events: self.events.clone(),
            details: self.details.clone(),
            partial: self.partial.clone(),
            stream_sequence: self.sequence,
            pending_approvals: self.pending.clone(),
            children: self.children.clone(),
            tainted: self.tainted,
        }
    }
}
