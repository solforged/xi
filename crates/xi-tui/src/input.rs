use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use xi_protocol::methods::client::{ListParams, SetSettingParams};

use crate::approvals::Approvals;
use crate::history::Search;
use crate::keys::Action;
use crate::picker::Picker;
use crate::sessions::Sessions;
use crate::style::Line;
use crate::{App, Error, ExternalRequest, Focus, config, settings};

impl App {
    async fn sessions_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => {
                if let Some(session) = self.sessions.as_ref().and_then(Sessions::selected).cloned()
                    && let Err(error) = self.resume(session).await
                {
                    self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())]);
                }
            }
            KeyCode::Tab => {
                let all = self.sessions.as_ref().is_some_and(|picker| !picker.all);
                let cwd = (!all).then(|| self.launch.attached.info.cwd.clone());
                match self
                    .launch
                    .client
                    .list(ListParams { cwd, running: None })
                    .await
                {
                    Ok(sessions) => {
                        if let Some(picker) = self.sessions.as_mut() {
                            picker.replace(sessions, all);
                        }
                    }
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
                }
            }
            _ => {
                if let Some(picker) = self.sessions.as_mut() {
                    match key.code {
                        KeyCode::Up => picker.up(),
                        KeyCode::Down => picker.down(),
                        KeyCode::Backspace => picker.backspace(),
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            picker.input(c)
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn settings_key(&mut self, key: KeyEvent) {
        let info = &self.launch.attached.info;
        let Some(settings) = self.settings.as_mut() else {
            self.focus = Focus::Composer;
            return;
        };
        match settings.key(key, &info.model, info.reasoning) {
            settings::Action::None => {}
            settings::Action::Close => self.close_overlay(),
            settings::Action::Models => self.open_models().await,
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
            settings::Action::Set { key, value } => {
                let params = self.settings_params();
                match self
                    .launch
                    .client
                    .set_setting(SetSettingParams {
                        cwd: params.cwd,
                        configs: params.configs,
                        key: key.into(),
                        value,
                    })
                    .await
                {
                    Ok(report) => {
                        if let Some(settings) = self.settings.as_mut() {
                            settings.saved(report);
                        }
                    }
                    Err(error) => self.settings_note(error.to_string()),
                }
            }
            settings::Action::EditFile => {
                if let Some(settings) = &self.settings {
                    self.external_requested = Some(ExternalRequest::Settings(
                        settings.report.files.global.clone(),
                    ));
                }
            }
        }
    }

    async fn tree_key(&mut self, key: KeyEvent) {
        let action = self.keymap.action(key);
        let Some(tree) = self.tree.as_mut() else {
            self.focus = Focus::Composer;
            return;
        };
        let page = isize::try_from(self.rows / 2).unwrap_or(1).max(1);
        match (action, key.code) {
            (Some(Action::Up), _) | (_, KeyCode::Char('k')) => tree.step(-1),
            (Some(Action::Down), _) | (_, KeyCode::Char('j')) => tree.step(1),
            (Some(Action::Send), _) => self.checkout().await,
            (Some(Action::Escape), _) | (_, KeyCode::Char('q')) => self.close_tree(),
            (_, KeyCode::PageUp) => tree.step(-page),
            (_, KeyCode::PageDown) => tree.step(page),
            (_, KeyCode::Home) => tree.jump(false),
            (_, KeyCode::End) => tree.jump(true),
            _ => {}
        }
    }

    async fn model_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Enter {
            let entry = self.picker.as_ref().and_then(Picker::selected).cloned();
            if let Some(entry) = entry {
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
                } else if self.set_model(entry.model).await {
                    self.launch.attached.info.model_info = Some(entry.info);
                    self.close_models();
                }
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.close_models();
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            match key.code {
                KeyCode::Up => picker.up(),
                KeyCode::Down => picker.down(),
                KeyCode::Backspace => picker.backspace(),
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    picker.input(c)
                }
                _ => {}
            }
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => {
                let selected = self
                    .search
                    .as_ref()
                    .and_then(|search| search.current(&self.history))
                    .map(str::to_owned);
                if let Some(selected) = selected {
                    self.editor.set_text(&selected);
                    self.search = None;
                    self.focus = Focus::Composer;
                    self.update_completion();
                }
            }
            _ => {
                if let Some(search) = self.search.as_mut() {
                    match key.code {
                        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            search.older()
                        }
                        KeyCode::Backspace => search.backspace(),
                        KeyCode::Char(c)
                            if !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            search.input(c)
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub(crate) async fn key(&mut self, key: KeyEvent) -> Result<(), Error> {
        let action = self.keymap.action(key);
        if matches!(action, Some(Action::Interrupt)) {
            if self.focus != Focus::Composer || self.completion.is_some() {
                self.close_overlay();
            } else if self.running() {
                self.cancel().await;
            } else if !self.editor.is_empty() || !self.images.is_empty() {
                self.editor.clear();
                self.images.clear();
                self.update_completion();
            } else {
                self.quit = true;
            }
            return Ok(());
        }
        match self.focus {
            Focus::Tree => {
                self.tree_key(key).await;
                return Ok(());
            }
            Focus::Models => {
                self.model_key(key).await;
                return Ok(());
            }
            Focus::Search => {
                self.search_key(key);
                return Ok(());
            }
            Focus::Sessions => {
                self.sessions_key(key).await;
                return Ok(());
            }
            Focus::Settings => {
                self.settings_key(key).await;
                return Ok(());
            }
            Focus::Approval => {
                let choice = match (action, key.code) {
                    (Some(Action::Send), _) => {
                        self.approvals.as_ref().and_then(Approvals::selected)
                    }
                    (Some(direction @ (Action::Up | Action::Down)), _) => {
                        if let Some(approvals) = self.approvals.as_mut() {
                            approvals.step(direction == Action::Down);
                        }
                        None
                    }
                    (Some(Action::Complete), _) => {
                        if let Some(approvals) = self.approvals.as_mut() {
                            approvals.next();
                        }
                        None
                    }
                    (Some(Action::Escape), _) => {
                        self.close_overlay();
                        None
                    }
                    (_, KeyCode::Char(c))
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.approvals
                            .as_ref()
                            .and_then(|approvals| approvals.shortcut(c))
                    }
                    _ => None,
                };
                if let Some(choice) = choice {
                    self.answer(choice).await;
                }
                return Ok(());
            }
            Focus::Composer => {}
        }
        if let Some(menu) = self.completion.as_mut() {
            match action {
                Some(Action::Up) => {
                    menu.up();
                    return Ok(());
                }
                Some(Action::Down) => {
                    menu.down();
                    return Ok(());
                }
                // A token that already equals the selection is complete, so
                // enter sends it instead of accepting the same text again.
                Some(Action::Send)
                    if menu.selected().is_some_and(|item| {
                        self.editor.text().get(menu.range()) == Some(item.insert.trim_end())
                    }) =>
                {
                    self.completion = None;
                }
                Some(Action::Send | Action::Complete) => {
                    self.accept_completion();
                    return Ok(());
                }
                Some(Action::Escape) => {
                    self.completion = None;
                    return Ok(());
                }
                _ => {}
            }
        }
        match action {
            Some(Action::Send) => {
                self.submit(false).await?;
                return Ok(());
            }
            Some(Action::Queue) => {
                self.submit(true).await?;
                return Ok(());
            }
            Some(Action::Newline) => self.editor.insert("\n"),
            Some(Action::Escape) => {
                if self.running() {
                    self.cancel().await;
                }
                return Ok(());
            }
            Some(Action::Complete) => {
                self.open_own_approvals();
                return Ok(());
            }
            Some(Action::Search) => {
                self.search = Some(Search::new(self.editor.text().to_owned()));
                self.completion = None;
                self.focus = Focus::Search;
                return Ok(());
            }
            Some(Action::External) => {
                self.external_requested = Some(ExternalRequest::Composer);
                return Ok(());
            }
            Some(Action::PasteImage) => {
                self.paste_image().await;
                return Ok(());
            }
            Some(Action::Models) => {
                self.open_models().await;
                return Ok(());
            }
            Some(Action::Reasoning) => {
                self.cycle_reasoning().await;
                return Ok(());
            }
            Some(Action::Expand) => {
                let lines = self.transcript.expand(self.cols).unwrap_or_else(|| {
                    let why = if self.transcript.collapsed() {
                        "  that was the oldest cut-short result · ctrl+o starts again from the newest"
                    } else {
                        "  nothing cut short yet · results longer than three lines show whole here"
                    };
                    self.transcript.note(vec![Line::default().dim(why)])
                });
                self.commit.extend(lines);
                return Ok(());
            }
            Some(Action::Thinking) => {
                let thinking = self.transcript.thinking_display().next();
                self.transcript.set_thinking(thinking);
                self.note(vec![
                    Line::default().dim(format!("  thinking display → {}", thinking.name())),
                ]);
                return Ok(());
            }
            Some(Action::DeleteOrQuit) => {
                if self.editor.is_empty() && self.images.is_empty() && !self.running() {
                    self.quit = true;
                    return Ok(());
                }
                self.editor.delete();
            }
            Some(Action::Home) => self.editor.home(),
            Some(Action::End) => self.editor.end(),
            Some(Action::Left) => self.editor.left(),
            Some(Action::Right) => self.editor.right(),
            Some(Action::KillBefore) => self.editor.kill_before(),
            Some(Action::KillAfter) => self.editor.kill_after(),
            Some(Action::KillWord) => self.editor.kill_word(),
            Some(Action::Backspace) => {
                if self.editor.is_empty() {
                    self.images.pop();
                } else {
                    self.editor.backspace();
                }
            }
            Some(Action::Delete) => self.editor.delete(),
            Some(Action::Up) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_back();
                }
            }
            Some(Action::Down) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_forward();
                }
            }
            None => {
                if let KeyCode::Char(c) = key.code
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    let mut buf = [0; 4];
                    self.editor.insert(c.encode_utf8(&mut buf));
                } else {
                    return Ok(());
                }
            }
            Some(Action::Interrupt) => return Ok(()),
        }
        self.update_completion();
        Ok(())
    }
}
