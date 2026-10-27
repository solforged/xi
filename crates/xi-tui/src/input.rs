use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::history::Search;
use crate::keys::Action;
use crate::style::Line;
use crate::{App, Error, ExternalRequest};

impl App {
    pub(crate) async fn key(&mut self, key: KeyEvent) -> Result<(), Error> {
        let action = self.keymap.action(key);
        if !self.deck
            && matches!(
                action,
                Some(Action::FocusSessions | Action::FocusLog | Action::FocusInspector)
            )
        {
            return Ok(());
        }
        if matches!(action, Some(Action::Interrupt)) {
            if !self.overlays.is_empty() || self.completion.is_some() {
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
        if !self.overlays.is_empty() {
            self.overlay_key(key, action).await;
            return Ok(());
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
                self.overlays.push(Box::new(Search::default()));
                self.completion = None;
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
                let lines = self.transcript.expand(self.measure()).unwrap_or_else(|| {
                    let why = if self.transcript.collapsed() {
                        "  that was the oldest cut-short result · ctrl+o starts again from the newest"
                    } else {
                        "  nothing cut short yet · results longer than three lines show whole here"
                    };
                    self.transcript.note(vec![Line::default().dim(why)])
                });
                self.commit_lines(lines);
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
            Some(
                Action::Interrupt
                | Action::FocusSessions
                | Action::FocusLog
                | Action::FocusInspector,
            ) => return Ok(()),
        }
        self.update_completion();
        Ok(())
    }
}
