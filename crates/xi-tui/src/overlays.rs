use crossterm::event::KeyEvent;
use futures::stream::FuturesUnordered;
use xi_client::client::{ClientError, ClientNotification};
use xi_protocol::SessionId;
use xi_protocol::methods::client::{
    AttachResult, EventParams, ListParams, ModelEntry, OpenParams, PendingApproval, SessionSummary,
    SetSettingParams, SettingsParams, SettingsReport,
};
use xi_protocol::model::{ModelRole, ModelSelector, Reasoning};
use xi_protocol::rpc::RpcError;

use crate::approvals::{Approval, Approvals, Choice};
use crate::editor::Editor;
use crate::history::History;
use crate::keys::Action;
use crate::overlay::{Command, Outcome};
use crate::picker::Picker;
use crate::roles::{self, Roles};
use crate::sessions::Sessions;
use crate::settings::{self, Settings};
use crate::style::{Line, Role};
use crate::transcript::Transcript;
use crate::tree::{Node, Tree};
use crate::{App, Error, ExternalRequest, config, lineage, style};

/// Borrows what overlays read, field by field, so the stack can be
/// borrowed mutably beside it.
macro_rules! ctx {
    ($app:ident) => {
        $crate::overlay::Ctx {
            keys: &$app.keymap,
            history: &$app.history,
            info: &$app.launch.attached.info,
            thinking: $app.transcript.thinking_display(),
            layout: $app.layout,
            tui_path: $app.launch.tui_config.as_deref(),
            now: ::xi_protocol::Timestamp::now(),
            rows: $app.rows,
            parent: None,
        }
    };
}
pub(crate) use ctx;

impl App {
    /// This session's asks that still wait.
    pub(crate) fn waiting(&self) -> usize {
        self.transcript
            .asks()
            .iter()
            .filter(|ask| !self.resolved.contains(&ask.call))
            .count()
    }

    /// Hands a key to the top overlay and carries out what it asks for.
    /// `tab` from any other overlay opens this session's waiting asks on
    /// top, and `esc` there returns to it.
    pub(crate) async fn overlay_key(&mut self, key: KeyEvent, action: Option<Action>) {
        if action == Some(Action::Complete)
            && !self.overlays.on_top::<Approvals>()
            && self.waiting() > 0
        {
            self.open_own_approvals();
            return;
        }
        let mut ctx = ctx!(self);
        match self.overlays.key(key, action, &mut ctx) {
            Outcome::Stay => {}
            Outcome::Close => self.overlays.pop(),
            Outcome::Send(command) => self.run(*command).await,
        }
    }

    async fn run(&mut self, command: Command) {
        match command {
            Command::Answer(choice) => self.answer(choice).await,
            Command::Checkout(node) => self.checkout(node).await,
            Command::SetModel(entry) => self.choose_model(entry).await,
            Command::Resume(session) => {
                if let Err(error) = self.resume(session).await {
                    self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                }
            }
            Command::Sessions { all } => {
                let cwd = (!all).then(|| self.launch.attached.info.cwd.clone());
                match self
                    .launch
                    .client
                    .list(ListParams { cwd, running: None })
                    .await
                {
                    Ok(sessions) => {
                        if let Some(picker) = self.overlays.get_mut::<Sessions>() {
                            picker.replace(sessions, all);
                        }
                    }
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                    }
                }
            }
            Command::Setting(action) => self.setting(action).await,
            Command::Role(action) => self.role(action).await,
            Command::UsePrompt(prompt) => {
                self.overlays.pop();
                self.editor.set_text(&prompt);
                self.update_completion();
            }
        }
    }

    async fn setting(&mut self, action: settings::Action) {
        match action {
            settings::Action::None | settings::Action::Close => {}
            settings::Action::Models => self.open_models().await,
            settings::Action::Roles => {
                if let Some(settings) = self.overlays.get::<Settings>() {
                    let roles = Roles::new(settings.report.clone());
                    self.overlays.push(Box::new(roles));
                }
            }
            settings::Action::Reasoning => self.cycle_reasoning().await,
            settings::Action::Thinking => {
                let thinking = self.transcript.thinking_display().next();
                self.transcript.set_thinking(thinking);
                let saved = self
                    .launch
                    .tui_config
                    .as_deref()
                    .map(|path| config::save_thinking(path, thinking));
                match saved {
                    Some(Ok(())) => self.settings_note("saved · thinking display"),
                    Some(Err(error)) => self.settings_note(error.to_string()),
                    None => self.settings_note("not saved: no config path"),
                }
            }
            settings::Action::Layout => {
                let layout = match self.layout {
                    crate::layout::Layout::Inline => crate::layout::Layout::Deck,
                    crate::layout::Layout::Deck => crate::layout::Layout::Auto,
                    crate::layout::Layout::Auto => crate::layout::Layout::Inline,
                };
                match self.set_layout(layout) {
                    Ok(()) if self.launch.tui_config.is_some() => {
                        self.settings_note("saved · layout")
                    }
                    Ok(()) => self.settings_note("not saved: no config path"),
                    Err(error) => self.settings_note(error.to_string()),
                }
            }
            settings::Action::Set { key, value } => match self.save_setting(key, value).await {
                Ok(report) => {
                    if let Some(settings) = self.overlays.get_mut::<Settings>() {
                        settings.saved(report);
                    }
                }
                Err(error) => self.settings_note(error.to_string()),
            },
            settings::Action::EditFile => {
                if let Some(settings) = self.overlays.get::<Settings>() {
                    self.external_requested = Some(ExternalRequest::Settings(
                        settings.report.files.global.clone(),
                    ));
                }
            }
        }
    }

    async fn role(&mut self, action: roles::Action) {
        match action {
            roles::Action::Set { key, value } => {
                let saved = self.save_setting(key, value).await;
                self.overlays.close::<Picker>();
                match saved {
                    Ok(report) => {
                        if let Some(settings) = self.overlays.get_mut::<Settings>() {
                            settings.replace(report.clone());
                        }
                        if let Some(roles) = self.overlays.get_mut::<Roles>() {
                            roles.saved(report);
                        }
                    }
                    Err(error) => self.roles_note(error.to_string()),
                }
            }
            roles::Action::Pick(role) => self.pick_role(role).await,
            roles::Action::Use(model) => {
                let name = format!("{}/{}", model.provider, model.model);
                if self.set_model(model.into()).await {
                    self.roles_note(format!("This session now uses {name}."));
                }
            }
            roles::Action::EditFile => {
                if let Some(roles) = self.overlays.get::<Roles>() {
                    self.external_requested =
                        Some(ExternalRequest::Settings(roles.report.files.global.clone()));
                }
            }
        }
    }

    fn roles_note(&mut self, note: impl Into<String>) {
        if let Some(roles) = self.overlays.get_mut::<Roles>() {
            roles.note(note);
        }
    }

    /// Opens the roles view on its own, like `/settings`.
    pub(crate) async fn open_roles(&mut self) {
        let report = self.launch.client.settings(self.settings_params()).await;
        self.close_overlay();
        match report {
            Ok(report) => self.overlays.push(Box::new(Roles::new(report))),
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    /// Opens the model picker over the roles view to assign `role`.
    async fn pick_role(&mut self, role: ModelRole) {
        let Some((current, roles, reset)) = self
            .overlays
            .get::<Roles>()
            .map(|view| view.assignment(role))
        else {
            return;
        };
        match self.launch.client.models(self.session_id()).await {
            Ok(models) => {
                self.overlays.push(Box::new(Picker::assign(
                    models, role, current, roles, reset,
                )));
            }
            Err(error) => self.roles_note(error.to_string()),
        }
    }

    async fn choose_model(&mut self, entry: ModelEntry) {
        if !self.idle_only("model selection") {
            return;
        }
        if !entry.available {
            let hint = if entry.model.provider == "chatgpt" {
                " · xi login chatgpt"
            } else {
                ""
            };
            self.note(vec![Line::default().dim(format!("  no credentials{hint}"))]);
        } else if self.set_model(entry.model.into()).await {
            // The model change event records the switch in scrollback.
            self.launch.attached.info.model_info = Some(entry.info);
            self.overlays.close::<Picker>();
        }
    }

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
        let session = self.session_id();
        match self.overlays.get_mut::<Approvals>() {
            Some(approvals) => approvals.replace_current(entries, session),
            None => self.overlays.push(Box::new(Approvals::new(entries, false))),
        }
        self.completion = None;
    }

    pub(crate) fn close_empty_approvals(&mut self) {
        if self
            .overlays
            .get::<Approvals>()
            .is_some_and(|approvals| approvals.current().is_none())
        {
            self.overlays.close::<Approvals>();
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
        match self.overlays.get_mut::<Approvals>() {
            Some(approvals) => approvals.replace(entries),
            None => self.overlays.push(Box::new(Approvals::new(entries, true))),
        }
        self.completion = None;
        self.close_empty_approvals();
    }

    /// Whether the TUI may leave this session. A running turn keeps running
    /// in its worker, but queued prompts live only here and would be lost.
    fn can_leave(&mut self) -> bool {
        if self.queued.is_empty() {
            return true;
        }
        self.note(vec![
            Line::default().dim("  /resume waits for queued prompts to send"),
        ]);
        false
    }

    pub(crate) async fn open_sessions(&mut self) {
        if !self.can_leave() {
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
                self.overlays
                    .push(Box::new(Sessions::new(sessions, self.session_id())));
                self.completion = None;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    pub(crate) async fn switch_session(
        &mut self,
        attached: AttachResult,
        clear_scrollback: bool,
    ) -> Result<(), Error> {
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
        self.images.clear();
        let error = if changed_workspace {
            let (history, error) =
                History::load(self.launch.history.clone(), &self.launch.attached.info.cwd);
            self.history = history;
            error
        } else {
            None
        };
        self.editor.set_history(self.history.workspace());
        if self.deck.is_some() {
            // The deck's log shows the attached session alone.
            self.reset_deck_log();
        } else if clear_scrollback {
            self.commit.clear();
            self.clear_scrollback = true;
        } else {
            self.note(vec![style::rule(self.cols)]);
        }
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
        if !self.can_leave() {
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
        self.switch_session(attached, false).await
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
                self.overlays.push(Box::new(Settings::new(report)));
            }
            Err(error) => {
                self.close_overlay();
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            }
        }
    }

    pub(crate) fn settings_note(&mut self, note: impl Into<String>) {
        if let Some(settings) = self.overlays.get_mut::<Settings>() {
            settings.note(note);
        }
    }

    /// Writes a global setting through the daemon.
    async fn save_setting(
        &self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<SettingsReport, ClientError> {
        let params = self.settings_params();
        self.launch
            .client
            .set_setting(SetSettingParams {
                cwd: params.cwd,
                configs: params.configs,
                key: key.into(),
                value,
            })
            .await
    }

    pub(crate) async fn refresh_settings(&mut self) {
        match self.launch.client.settings(self.settings_params()).await {
            Ok(report) => {
                if let Some(settings) = self.overlays.get_mut::<Settings>() {
                    settings.replace(report);
                }
            }
            Err(error) => self.settings_note(error.to_string()),
        }
    }

    /// Opens the model picker on top of whatever is open, so settings
    /// gets it as a step deeper.
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
                self.overlays.push(Box::new(Picker::new(models)));
                self.completion = None;
            }
            Err(error) => self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]),
        }
    }

    /// Selects a model or role; the worker answers with the model a role
    /// resolved to, which the footer shows until the next attach.
    pub(crate) async fn set_model(&mut self, model: ModelSelector) -> bool {
        if !self.idle_only("model selection") {
            return false;
        }
        match self.launch.client.set_model(self.session_id(), model).await {
            Ok(model) => {
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
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        }
        // The chosen level also becomes the global default for new sessions.
        match self
            .save_setting("reasoning", reasoning.name().into())
            .await
        {
            Ok(report) => {
                if let Some(settings) = self.overlays.get_mut::<Settings>() {
                    settings.replace(report);
                    settings.note("saved · new sessions use this reasoning level");
                }
            }
            Err(error) => self.note(vec![
                Line::default()
                    .sanguine("✕ ")
                    .ink(format!("reasoning default not saved: {error}")),
            ]),
        }
    }

    pub(crate) async fn toggle_sandbox(&mut self) {
        self.set_sandbox(!self.launch.attached.info.sandbox_enabled)
            .await;
    }

    pub(crate) async fn set_sandbox(&mut self, enabled: bool) {
        if !self.idle_only("sandbox changes") {
            return;
        }
        match self
            .launch
            .client
            .set_sandbox(self.session_id(), enabled)
            .await
        {
            Ok(()) => {
                self.launch.attached.info.sandbox_enabled = enabled;
            }
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
            }
        }
    }

    /// Answers the shown ask and records the answer in scrollback, such as
    /// `? allowed for this session · bash exec cargo test`.
    pub(crate) async fn answer(&mut self, choice: Choice) {
        let Some(ask) = self
            .overlays
            .get::<Approvals>()
            .and_then(Approvals::current)
            .cloned()
        else {
            return;
        };
        let pending = ask.pending;
        // An ask answered elsewhere first is not this client's answer, so
        // it leaves no record.
        let answered = match self
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
            Ok(()) => true,
            Err(ClientError::Rpc {
                code: RpcError::APPROVAL_NOT_PENDING,
                ..
            }) => false,
            Err(error) => {
                self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                return;
            }
        };
        if pending.session == self.session_id() {
            self.resolved.insert(pending.call.clone());
        } else {
            self.answered
                .insert((pending.session, pending.call.clone()));
        }
        if answered {
            let role = if choice.approved() {
                Role::Ink
            } else {
                Role::Sanguine
            };
            self.note(vec![
                Line::default()
                    .brass("? ")
                    .span(role, false, choice.record())
                    .dim(" · ")
                    .ink(format!("{} {}", pending.tool, pending.action))
                    .truncate(self.cols),
            ]);
        }
        let mut inbox = false;
        if let Some(approvals) = self.overlays.get_mut::<Approvals>() {
            inbox = approvals.inbox;
            approvals.remove(pending.session, &pending.call);
        }
        if inbox {
            self.refresh_inbox().await;
        } else {
            self.close_empty_approvals();
        }
    }

    /// Checks out a tree node and rebuilds what the client knows from the
    /// new head's branch. Scrollback stays as it was.
    pub(crate) async fn checkout(&mut self, node: Node) {
        self.overlays.close::<Tree>();
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
            transcript.apply(&n, self.measure());
        }
        transcript.end_turn();
        // Replayed rows are not committed, so nothing may inspect them.
        transcript.take_inspects();
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
