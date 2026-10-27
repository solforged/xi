use xi_protocol::manifest::CommandSpec;
use xi_protocol::model::Reasoning;

use crate::approvals::Approvals;
use crate::callout::Kind;
use crate::layout::Layout;
use crate::overlay::Command;
use crate::style::{Line, Role};
use crate::transcript::{notice_rows, tokens};
use crate::tree::Tree;
use crate::{App, Error, clipboard};

#[derive(Clone, Copy)]
enum Builtin {
    Help,
    Quit,
    New,
    Resume,
    Approvals,
    Settings,
    Roles,
    Copy,
    Tree,
    Model,
    Reasoning,
    Sandbox,
    Compact,
    Layout,
    Rename,
}

const COMMANDS: &[(&str, &str, Builtin)] = &[
    ("help", "show keys and commands", Builtin::Help),
    ("hotkeys", "show keys and commands", Builtin::Help),
    ("quit", "leave; a running turn keeps running", Builtin::Quit),
    ("exit", "leave; a running turn keeps running", Builtin::Quit),
    (
        "new",
        "clear scrollback and start a session in this workspace",
        Builtin::New,
    ),
    (
        "resume",
        "pick a session to attach or open",
        Builtin::Resume,
    ),
    (
        "approvals",
        "answer pending approvals in every session",
        Builtin::Approvals,
    ),
    ("settings", "show and change settings", Builtin::Settings),
    (
        "roles",
        "show and assign the preset and model roles",
        Builtin::Roles,
    ),
    (
        "copy",
        "copy the last response to the clipboard",
        Builtin::Copy,
    ),
    (
        "tree",
        "browse branches and check out an earlier point",
        Builtin::Tree,
    ),
    (
        "model",
        "switch PROVIDER/MODEL; without an argument, open the picker",
        Builtin::Model,
    ),
    (
        "reasoning",
        "set off, low, medium, high; without an argument, show it",
        Builtin::Reasoning,
    ),
    (
        "sandbox",
        "toggle this session's tool sandbox; or set on or off",
        Builtin::Sandbox,
    ),
    ("compact", "summarize older context now", Builtin::Compact),
    ("rename", "set this session's title", Builtin::Rename),
    (
        "layout",
        "set inline, deck or auto; without an argument, show it",
        Builtin::Layout,
    ),
];

pub(crate) fn builtin_commands() -> Vec<CommandSpec> {
    COMMANDS
        .iter()
        .map(|(name, description, _)| CommandSpec {
            name: (*name).into(),
            description: (*description).into(),
        })
        .collect()
}

/// The levels a model lists, lowest first, joined by `separator`.
fn reasoning_levels(model: &xi_protocol::model::ModelInfo, separator: &str) -> String {
    let mut levels = model.reasoning.clone();
    levels.sort_unstable();
    levels.dedup();
    levels
        .iter()
        .map(|level| level.name())
        .collect::<Vec<_>>()
        .join(separator)
}

impl App {
    /// Changes and saves the client's layout choice, entering or leaving
    /// the deck as it says.
    pub(crate) fn set_layout(&mut self, layout: Layout) -> std::io::Result<()> {
        self.display.layout = layout;
        if layout.deck_at(self.display.cols, self.display.rows) {
            self.enter_deck();
        } else {
            self.leave_deck();
        }
        self.launch
            .tui_config
            .as_deref()
            .map_or(Ok(()), |path| crate::config::save_layout(path, layout))
    }

    pub(crate) async fn command(&mut self, line: &str) -> Result<(), Error> {
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let name = name.trim_start_matches('/');
        let rest = rest.trim();
        let builtin = COMMANDS
            .iter()
            .find(|(command, ..)| *command == name)
            .map(|(_, _, kind)| *kind);
        match builtin {
            Some(Builtin::Help) => {
                let mut lines = vec![Line::default().strong(Role::Ink, "  keys")];
                for (key, description) in self.keymap.help() {
                    lines.push(Line::default().ink(format!("  {key:<22}")).dim(description));
                }
                lines.push(Line::default().strong(Role::Ink, "  commands"));
                for (command, description, _) in COMMANDS {
                    lines.push(
                        Line::default()
                            .ink(format!("  /{command:<21}"))
                            .dim(description),
                    );
                }
                lines.push(
                    Line::default()
                        .ink("  /name args            ")
                        .dim("run an extension command"),
                );
                lines.push(
                    Line::default()
                        .ink("  /skill:name args      ")
                        .dim("send a prompt with a skill's instructions"),
                );
                self.note(lines);
            }
            Some(Builtin::Quit) => self.quit = true,
            Some(Builtin::New) => {
                if !self.idle_only("/new") {
                    return Ok(());
                }
                let attached = self
                    .launch
                    .client
                    .create(self.launch.template.clone())
                    .await?;
                self.switch_session(attached, true).await?;
            }
            Some(Builtin::Resume) => self.open_sessions().await,
            Some(Builtin::Approvals) => {
                self.overlays.close::<Approvals>();
                self.refresh_inbox().await;
                if self.overlays.get::<Approvals>().is_none() {
                    self.notice(Kind::Info, "no pending approvals");
                }
            }
            Some(Builtin::Settings) => self.open_settings().await,
            Some(Builtin::Roles) => self.open_roles().await,
            Some(Builtin::Copy) => {
                if let Some(text) = self.transcript.last_text() {
                    let lines = text.lines().count();
                    self.display.controls.push(clipboard::osc52(text));
                    self.notice(Kind::Done, format!("copied {lines} lines"));
                } else {
                    self.notice(Kind::Info, "nothing to copy yet");
                }
            }
            Some(Builtin::Tree) => {
                if !self.idle_only("/tree") {
                    return Ok(());
                }
                self.overlays.push(Box::new(Tree::new(
                    &self.launch.attached.events,
                    self.launch.attached.info.head,
                )));
                self.composer.completion = None;
            }
            Some(Builtin::Model) if rest.is_empty() => self.open_models().await,
            Some(Builtin::Model) => match rest.parse() {
                Ok(model) => {
                    self.set_model(model).await;
                }
                Err(_) => self.notice(
                    Kind::Warn,
                    "/model expects PROVIDER/MODEL or @slow, @default, @smol, @tiny",
                ),
            },
            Some(Builtin::Reasoning) if rest.is_empty() => {
                let info = &self.launch.attached.info;
                let line = match &info.model_info {
                    Some(model) if model.reasoning.is_empty() => {
                        Line::default().ink(format!("{} does not reason", model.id))
                    }
                    Some(model) => Line::default()
                        .ink(format!("reasoning {}", model.resolve(info.reasoning)))
                        .dim(format!(" · /reasoning {}", reasoning_levels(model, "|"))),
                    None => Line::default().ink(format!("reasoning {}", info.reasoning)),
                };
                self.notice(Kind::Info, line);
            }
            Some(Builtin::Reasoning) => match rest.parse::<Reasoning>() {
                Ok(reasoning) => self.set_reasoning(reasoning).await,
                Err(error) => {
                    let names = match &self.launch.attached.info.model_info {
                        Some(model) if !model.reasoning.is_empty() => reasoning_levels(model, ", "),
                        _ => Reasoning::ALL.map(Reasoning::name).join(", "),
                    };
                    self.notice(Kind::Warn, format!("{error}; use {names}"));
                }
            },
            Some(Builtin::Sandbox) => {
                let command = match rest {
                    "" => Command::ToggleSandbox,
                    "on" => Command::Sandbox(true),
                    "off" => Command::Sandbox(false),
                    _ => {
                        self.notice(Kind::Warn, "/sandbox expects on or off");
                        return Ok(());
                    }
                };
                // Submit reaches this handler through execute; box the return edge.
                Box::pin(self.execute(command)).await?;
            }
            Some(Builtin::Layout) if rest.is_empty() => {
                self.notice(
                    Kind::Info,
                    Line::default()
                        .ink(format!("layout {}", self.display.layout.name()))
                        .dim(format!(
                            " · {}, {}×{}",
                            self.display.measure().tier.name(),
                            self.display.cols,
                            self.display.rows
                        )),
                );
            }
            Some(Builtin::Layout) => match Layout::parse(rest) {
                Some(layout) => {
                    let saved = self.set_layout(layout);
                    self.notice(Kind::Info, format!("layout → {}", layout.name()));
                    if let Err(error) = saved {
                        self.notice(Kind::Error, format!("layout not saved: {error}"));
                    }
                }
                None => self.notice(Kind::Warn, "/layout expects inline, deck or auto"),
            },
            Some(Builtin::Compact) => {
                if !self.idle_only("/compact") {
                    return Ok(());
                }
                match self.launch.client.compact(self.session_id()).await {
                    Ok(result) => self.notice(
                        Kind::Done,
                        Line::default()
                            .ink("compacted")
                            .dim(format!(" from {} tokens", tokens(result.tokens_before))),
                    ),
                    Err(error) => self.notice(Kind::Error, error.to_string()),
                }
            }
            Some(Builtin::Rename) if rest.is_empty() => {
                self.notice(Kind::Warn, "/rename expects a title");
            }
            Some(Builtin::Rename) => {
                // The `session_titled` event commits the scrollback line.
                if let Err(error) = self.launch.client.rename(self.session_id(), rest).await {
                    self.notice(Kind::Error, error.to_string());
                }
            }
            None => {
                match self
                    .launch
                    .client
                    .command(self.session_id(), name, rest)
                    .await
                {
                    Ok(Some(result)) => {
                        if let Some(message) = result.message {
                            let m = self.display.measure();
                            let rows = message
                                .lines()
                                .enumerate()
                                .flat_map(|(i, line)| {
                                    let mark = if i == 0 {
                                        Kind::Info.mark()
                                    } else {
                                        Line::default().ink(" ")
                                    };
                                    notice_rows(mark, line.into(), m)
                                })
                                .collect();
                            self.note(rows);
                        }
                    }
                    Ok(None) => self.notice(
                        Kind::Warn,
                        Line::default()
                            .ink(format!("unknown command /{name}"))
                            .dim(" · /help"),
                    ),
                    Err(error) => self.notice(Kind::Error, error.to_string()),
                }
            }
        }
        Ok(())
    }
}
