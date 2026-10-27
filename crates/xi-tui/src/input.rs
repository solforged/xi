use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::approvals::Approvals;
use crate::keys::Action;
use crate::overlay::{Command, Outcome};
use crate::overlays::ctx;
use crate::{App, Composer, Error};

impl App {
    /// Routes a key to the focused component and executes its command.
    pub(crate) async fn key(&mut self, key: KeyEvent) -> Result<(), Error> {
        self.composer.flash = None;
        let action = self.keymap.action(key);
        let outcome = if self.overlays.is_empty()
            && let Some(outcome) =
                self.display
                    .deck_key(key, action, &self.launch.attached.info.cwd)
        {
            outcome
        } else {
            match action {
                Some(Action::FocusSessions | Action::FocusLog | Action::FocusInspector) => {
                    Outcome::Stay
                }
                Some(Action::Interrupt) if !self.overlays.is_empty() => {
                    Outcome::Send(Box::new(Command::Dismiss))
                }
                Some(Action::Complete)
                    if !self.overlays.is_empty()
                        && !self.overlays.on_top::<Approvals>()
                        && self.waiting() > 0 =>
                {
                    Outcome::Send(Box::new(Command::OwnApprovals))
                }
                _ if !self.overlays.is_empty() => self.overlays.key(key, action, &mut ctx!(self)),
                _ => self.composer.key(key, action, self.turn.running()),
            }
        };
        let command = match outcome {
            Outcome::Stay => return Ok(()),
            Outcome::Close => Command::Close,
            Outcome::Send(command) => *command,
        };
        self.execute(command).await
    }
}

impl Composer {
    /// Edits the draft locally and hands application effects to its owner.
    pub(crate) fn key(&mut self, key: KeyEvent, action: Option<Action>, running: bool) -> Outcome {
        if let Some(menu) = self.completion.as_mut() {
            match action {
                Some(Action::Up) => {
                    menu.up();
                    return Outcome::Stay;
                }
                Some(Action::Down) => {
                    menu.down();
                    return Outcome::Stay;
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
                    return Outcome::Stay;
                }
                Some(Action::Escape) => {
                    self.completion = None;
                    return Outcome::Stay;
                }
                _ => {}
            }
        }
        let command = match action {
            Some(Action::Send) => Some(Command::Submit { queue: false }),
            Some(Action::Queue) => Some(Command::Submit { queue: true }),
            Some(Action::Newline) => {
                self.editor.insert("\n");
                None
            }
            Some(Action::Interrupt) => {
                if self.completion.is_some() {
                    Some(Command::Dismiss)
                } else if running {
                    Some(Command::Cancel)
                } else if !self.editor.is_empty() || !self.images.is_empty() {
                    self.editor.clear();
                    self.images.clear();
                    None
                } else {
                    Some(Command::Quit)
                }
            }
            Some(Action::Escape) => {
                return if running {
                    Outcome::Send(Box::new(Command::Cancel))
                } else {
                    Outcome::Stay
                };
            }
            Some(Action::Complete) => Some(Command::OwnApprovals),
            Some(Action::Search) => Some(Command::SearchHistory),
            Some(Action::External) => Some(Command::External),
            Some(Action::PasteImage) => Some(Command::PasteImage),
            Some(Action::Models) => Some(Command::Models),
            Some(Action::Reasoning) => Some(Command::Reasoning),
            Some(Action::Sandbox) => Some(Command::ToggleSandbox),
            Some(Action::Expand) => Some(Command::Expand),
            Some(Action::Thinking) => Some(Command::Thinking),
            Some(Action::DeleteOrQuit) => {
                if self.editor.is_empty() && self.images.is_empty() && !running {
                    Some(Command::Quit)
                } else {
                    self.editor.delete();
                    None
                }
            }
            Some(Action::Home) => {
                self.editor.home();
                None
            }
            Some(Action::End) => {
                self.editor.end();
                None
            }
            Some(Action::Left) => {
                self.editor.left();
                None
            }
            Some(Action::Right) => {
                self.editor.right();
                None
            }
            Some(Action::KillBefore) => {
                self.editor.kill_before();
                None
            }
            Some(Action::KillAfter) => {
                self.editor.kill_after();
                None
            }
            Some(Action::KillWord) => {
                self.editor.kill_word();
                None
            }
            Some(Action::Backspace) => {
                if self.editor.is_empty() {
                    self.images.pop();
                } else {
                    self.editor.backspace();
                }
                None
            }
            Some(Action::Delete) => {
                self.editor.delete();
                None
            }
            Some(Action::Up) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_back();
                }
                None
            }
            Some(Action::Down) => {
                if !self.editor.text().contains('\n') {
                    self.editor.history_forward();
                }
                None
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
                    return Outcome::Stay;
                }
                None
            }
            Some(Action::FocusSessions | Action::FocusLog | Action::FocusInspector) => {
                return Outcome::Stay;
            }
        };
        match command {
            Some(command) => Outcome::Send(Box::new(command)),
            None => {
                self.update_completion();
                Outcome::Stay
            }
        }
    }
}
