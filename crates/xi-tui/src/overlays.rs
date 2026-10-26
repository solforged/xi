use futures::stream::FuturesUnordered;
use xi_client::client::{ClientError, ClientNotification};
use xi_protocol::SessionId;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{
    AttachResult, EventParams, ListParams, OpenParams, PendingApproval, SessionSummary,
    SettingsParams,
};
use xi_protocol::model::Reasoning;
use xi_protocol::rpc::RpcError;

use crate::approvals::{Approval, Approvals, Choice};
use crate::editor::Editor;
use crate::history::History;
use crate::picker::Picker;
use crate::sessions::Sessions;
use crate::settings::Settings;
use crate::style::Line;
use crate::transcript::Transcript;
use crate::tree::Tree;
use crate::{App, Error, Focus, lineage, style};

impl App {
    pub(crate) fn own_approvals(&self) -> Vec<Approval> {
        let info = &self.launch.attached.info;
        self.transcript
            .asks()
            .iter()
            .filter(|ask| !self.resolved.contains(&ask.call))
            .map(|ask| Approval {
                pending: PendingApproval {
                    session: info.session,
                    call: ask.call.clone(),
                    tool: ask.tool.clone(),
                    action: ask.action.clone(),
                    rule: ask.rule.clone(),
                    taint: ask.taint,
                    trusted: ask.trusted.unwrap_or(info.trusted),
                },
                cwd: Some(info.cwd.clone()),
            })
            .collect()
    }

    pub(crate) fn open_own_approvals(&mut self) {
        let entries = self.own_approvals();
        if entries.is_empty() {
            return;
        }
        self.approvals = Some(Approvals::new(entries, false));
        self.completion = None;
        self.focus = Focus::Approval;
    }

    pub(crate) fn close_empty_approvals(&mut self) {
        if self
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.current().is_none())
        {
            self.approvals = None;
            self.focus = Focus::Composer;
        }
    }

    pub(crate) async fn refresh_inbox(&mut self) {
        self.inbox_refresh = false;
        let pending = match self.launch.client.approvals().await {
            Ok(pending) => pending,
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        };
        let current = self.session_id();
        if pending.iter().any(|ask| {
            ask.session != current && !self.approval_workspaces.contains_key(&ask.session)
        }) {
            match self.launch.client.list(ListParams::default()).await {
                Ok(sessions) => {
                    self.approval_workspaces = sessions
                        .into_iter()
                        .map(|session| (session.session, session.cwd))
                        .collect();
                }
                Err(error) => self.note(vec![
                    Line::default().dim(format!("  approval workspaces: {error}")),
                ]),
            }
        }
        let mut entries = self.own_approvals();
        entries.extend(
            pending
                .into_iter()
                .filter(|ask| {
                    ask.session != current
                        && !self.answered.contains(&(ask.session, ask.call.clone()))
                })
                .map(|pending| {
                    let cwd = self.approval_workspaces.get(&pending.session).cloned();
                    Approval { pending, cwd }
                }),
        );
        if let Some(approvals) = self.approvals.as_mut() {
            approvals.replace(entries);
        } else {
            self.approvals = Some(Approvals::new(entries, true));
        }
        self.completion = None;
        self.focus = Focus::Approval;
        self.close_empty_approvals();
    }

    pub(crate) async fn open_sessions(&mut self) {
        if !self.idle_only("/resume") {
            return;
        }
        match self
            .launch
            .client
            .list(ListParams {
                cwd: Some(self.launch.attached.info.cwd.clone()),
                running: None,
            })
            .await
        {
            Ok(sessions) => {
                self.sessions = Some(Sessions::new(sessions, self.session_id()));
                self.completion = None;
                self.focus = Focus::Sessions;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    pub(crate) async fn switch_session(&mut self, attached: AttachResult) -> Result<(), Error> {
        self.release().await?;
        let thinking = self.transcript.thinking_display();
        let changed_workspace = attached.info.cwd != self.launch.attached.info.cwd;
        self.launch.template.cwd = attached.info.cwd.clone();
        self.launch.attached = attached;
        self.exited = false;
        self.refresh = false;
        self.failed = false;
        self.started = None;
        self.pending = FuturesUnordered::new();
        self.transcript = Transcript::default();
        self.transcript.set_thinking(thinking);
        self.close_overlay();
        self.inbox_refresh = false;
        self.steering.clear();
        self.queued.clear();
        self.editor = Editor::default();
        let error = if changed_workspace {
            let (history, error) =
                History::load(self.launch.history.clone(), &self.launch.attached.info.cwd);
            self.history = history;
            error
        } else {
            None
        };
        self.editor.set_history(self.history.workspace());
        self.note(vec![style::rule(self.cols)]);
        self.attach();
        if let Some(error) = error {
            self.note(vec![
                Line::default().dim(format!("  prompt history: {error}")),
            ]);
        }
        self.initialize_composer().await;
        Ok(())
    }

    pub(crate) async fn resume(&mut self, session: SessionSummary) -> Result<(), Error> {
        if session.session == self.session_id() {
            self.close_overlay();
            return Ok(());
        }
        if !self.idle_only("/resume") {
            return Ok(());
        }
        let result = if session.head.is_some() {
            self.launch.client.attach(session.session, None).await
        } else {
            self.launch
                .client
                .open(OpenParams {
                    path: session.path,
                    options: self.launch.template.options.clone(),
                })
                .await
        };
        let attached = match result {
            Ok(attached) => attached,
            Err(ClientError::Rpc {
                code: RpcError::SESSION_HELD,
                data: Some(data),
                ..
            }) if data.get("session").is_some() => {
                let held: SessionId = serde_json::from_value(data["session"].clone())?;
                self.launch.client.attach(held, None).await?
            }
            Err(error) => return Err(error.into()),
        };
        self.switch_session(attached).await
    }

    pub(crate) fn settings_params(&self) -> SettingsParams {
        SettingsParams {
            cwd: self.launch.template.cwd.clone(),
            configs: self.launch.template.options.configs.clone(),
        }
    }

    pub(crate) async fn open_settings(&mut self) {
        match self.launch.client.settings(self.settings_params()).await {
            Ok(report) => {
                self.close_overlay();
                self.settings = Some(Settings::new(report));
                self.focus = Focus::Settings;
            }
            Err(error) => {
                self.close_overlay();
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            }
        }
    }

    pub(crate) fn settings_note(&mut self, note: impl Into<String>) {
        if let Some(settings) = self.settings.as_mut() {
            settings.note(note);
        }
    }

    pub(crate) async fn refresh_settings(&mut self) {
        match self.launch.client.settings(self.settings_params()).await {
            Ok(report) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.replace(report);
                }
            }
            Err(error) => self.settings_note(error.to_string()),
        }
    }

    pub(crate) fn close_models(&mut self) {
        if self.settings.is_some() {
            self.picker = None;
            self.focus = Focus::Settings;
        } else {
            self.close_overlay();
        }
    }

    pub(crate) async fn open_models(&mut self) {
        match self.launch.client.models(self.session_id()).await {
            Ok(models) => {
                self.launch.attached.info.model = models.current.clone();
                self.launch.attached.info.reasoning = models.reasoning;
                self.launch.attached.info.model_info = models
                    .models
                    .iter()
                    .find(|entry| entry.model == models.current)
                    .map(|entry| entry.info.clone());
                self.picker = Some(Picker::new(models));
                self.completion = None;
                self.focus = Focus::Models;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    pub(crate) async fn set_model(&mut self, model: ModelRef) -> bool {
        if !self.idle_only("model selection") {
            return false;
        }
        match self
            .launch
            .client
            .set_model(self.session_id(), model.clone())
            .await
        {
            Ok(()) => {
                self.launch.attached.info.model = model;
                self.launch.attached.info.model_info = None;
                self.refresh = true;
                true
            }
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                false
            }
        }
    }

    /// Steps to the model's next accepted level. Does nothing until the
    /// next attach supplies a switched model's levels.
    pub(crate) async fn cycle_reasoning(&mut self) {
        let info = &self.launch.attached.info;
        if let Some(next) = info
            .model_info
            .as_ref()
            .and_then(|model| model.next_reasoning(info.reasoning))
        {
            self.set_reasoning(next).await;
        }
    }

    pub(crate) async fn set_reasoning(&mut self, reasoning: Reasoning) {
        if !self.idle_only("reasoning") {
            return;
        }
        match self
            .launch
            .client
            .set_reasoning(self.session_id(), reasoning)
            .await
        {
            Ok(()) => self.launch.attached.info.reasoning = reasoning,
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    pub(crate) async fn answer(&mut self, choice: Choice) {
        let Some(ask) = self
            .approvals
            .as_ref()
            .and_then(Approvals::current)
            .cloned()
        else {
            return;
        };
        let pending = ask.pending;
        match self
            .launch
            .client
            .resolve(
                pending.session,
                pending.call.clone(),
                choice.approved(),
                choice.remember(),
            )
            .await
        {
            Ok(())
            | Err(ClientError::Rpc {
                code: RpcError::APPROVAL_NOT_PENDING,
                ..
            }) => {}
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        }
        if pending.session == self.session_id() {
            self.resolved.insert(pending.call.clone());
        } else {
            self.answered
                .insert((pending.session, pending.call.clone()));
        }
        let inbox = self
            .approvals
            .as_ref()
            .is_some_and(|approvals| approvals.inbox);
        if let Some(approvals) = self.approvals.as_mut() {
            approvals.remove(pending.session, &pending.call);
        }
        if inbox {
            self.refresh_inbox().await;
        } else {
            self.close_empty_approvals();
        }
    }

    pub(crate) fn close_tree(&mut self) {
        self.tree = None;
        self.focus = Focus::Composer;
    }

    /// Checks out the selected tree node and rebuilds what the client knows
    /// from the new head's branch. Scrollback stays as it was.
    pub(crate) async fn checkout(&mut self) {
        let Some(node) = self.tree.as_ref().and_then(Tree::current).cloned() else {
            return;
        };
        self.close_tree();
        if let Err(error) = self
            .launch
            .client
            .checkout(self.session_id(), node.target)
            .await
        {
            self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            return;
        }
        self.launch.attached.info.head = node.target;
        let mut transcript = Transcript::default();
        transcript.set_thinking(self.transcript.thinking_display());
        for event in lineage(&self.launch.attached.events, node.target) {
            let n = ClientNotification::Event(EventParams {
                session: self.session_id(),
                event,
            });
            transcript.apply(&n, self.cols);
        }
        transcript.end_turn();
        transcript.session = self.transcript.session;
        transcript.keep_details(&mut self.transcript);
        self.transcript = transcript;
        self.refresh_details().await;
        let what = match &node.prompt {
            Some(prompt) => {
                if self.editor.is_empty() {
                    self.editor.insert(prompt);
                }
                "before this prompt · edit it or send it as is to start a new branch"
            }
            None => "your next prompt continues from here",
        };
        self.update_completion();
        let mut line = Line::default()
            .brass("  ↳ ")
            .ink("checked out · ")
            .dim(what);
        if self.transcript.tainted {
            line = line.dim(" · ").murex("◆ tainted");
        }
        self.note(vec![line.truncate(self.cols)]);
    }
}
