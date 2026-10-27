use xi_protocol::event::ModelRef;
use xi_protocol::manifest::CommandSpec;
use xi_protocol::model::Reasoning;

use crate::approvals::Approvals;
use crate::layout::Layout;
use crate::style::{Line, Role};
use crate::transcript::tokens;
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
    Copy,
    Tree,
    Model,
    Reasoning,
    Compact,
    Layout,
}

const COMMANDS: &[(&str, &str, Builtin)] = &[
    ("help", "show keys and commands", Builtin::Help),
    ("hotkeys", "show keys and commands", Builtin::Help),
    ("quit", "leave; a running turn keeps running", Builtin::Quit),
    ("exit", "leave; a running turn keeps running", Builtin::Quit),
    ("new", "start a session in this workspace", Builtin::New),
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
    ("compact", "summarize older context now", Builtin::Compact),
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
    /// Changes and saves the client's layout choice.
    pub(crate) fn set_layout(&mut self, layout: Layout) -> std::io::Result<()> {
        self.layout = layout;
        self.deck = layout.deck_at(self.cols, self.rows);
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
                self.switch_session(attached).await?;
            }
            Some(Builtin::Resume) => self.open_sessions().await,
            Some(Builtin::Approvals) => {
                self.overlays.close::<Approvals>();
                self.refresh_inbox().await;
                if self.overlays.get::<Approvals>().is_none() {
                    self.note(vec![Line::default().dim("  no pending approvals")]);
                }
            }
            Some(Builtin::Settings) => self.open_settings().await,
            Some(Builtin::Copy) => {
                if let Some(text) = self.transcript.last_text() {
                    let lines = text.lines().count();
                    self.controls.push(clipboard::osc52(text));
                    self.note(vec![Line::default().dim(format!("  copied {lines} lines"))]);
                } else {
                    self.note(vec![Line::default().dim("  nothing to copy yet")]);
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
                self.completion = None;
            }
            Some(Builtin::Model) if rest.is_empty() => self.open_models().await,
            Some(Builtin::Model) => match rest.split_once('/') {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                    self.set_model(ModelRef {
                        provider: provider.into(),
                        model: model.into(),
                    })
                    .await;
                }
                _ => self.note(vec![Line::default().dim("  /model expects PROVIDER/MODEL")]),
            },
            Some(Builtin::Reasoning) if rest.is_empty() => {
                let info = &self.launch.attached.info;
                let line = match &info.model_info {
                    Some(model) if model.reasoning.is_empty() => {
                        format!("  {} does not reason", model.id)
                    }
                    Some(model) => format!(
                        "  reasoning {} · /reasoning {}",
                        model.resolve(info.reasoning),
                        reasoning_levels(model, "|")
                    ),
                    None => format!("  reasoning {}", info.reasoning),
                };
                self.note(vec![Line::default().dim(line)]);
            }
            Some(Builtin::Reasoning) => match rest.parse::<Reasoning>() {
                Ok(reasoning) => self.set_reasoning(reasoning).await,
                Err(error) => {
                    let names = match &self.launch.attached.info.model_info {
                        Some(model) if !model.reasoning.is_empty() => reasoning_levels(model, ", "),
                        _ => Reasoning::ALL.map(Reasoning::name).join(", "),
                    };
                    self.note(vec![Line::default().dim(format!("  {error}; use {names}"))]);
                }
            },
            Some(Builtin::Layout) if rest.is_empty() => {
                self.note(vec![Line::default().dim(format!(
                    "  layout · {} ({}, {}×{})",
                    self.layout.name(),
                    self.measure().tier.name(),
                    self.cols,
                    self.rows
                ))]);
            }
            Some(Builtin::Layout) => match Layout::parse(rest) {
                Some(layout) => {
                    let saved = self.set_layout(layout);
                    self.note(vec![
                        Line::default().dim(format!("  layout → {}", layout.name())),
                    ]);
                    if let Err(error) = saved {
                        self.note(vec![
                            Line::default().dim(format!("  layout not saved: {error}")),
                        ]);
                    }
                }
                None => self.note(vec![
                    Line::default().dim("  /layout expects inline, deck or auto"),
                ]),
            },
            Some(Builtin::Compact) => {
                if !self.idle_only("/compact") {
                    return Ok(());
                }
                match self.launch.client.compact(self.session_id()).await {
                    Ok(result) => self.note(vec![Line::default().dim(format!(
                        "  compacted · {} tokens before",
                        tokens(result.tokens_before)
                    ))]),
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
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
                            self.note(
                                message
                                    .lines()
                                    .map(|line| Line::default().dim(format!("  {line}")))
                                    .collect(),
                            );
                        }
                    }
                    Ok(None) => self.note(vec![
                        Line::default().dim(format!("  unknown command /{name} · /help")),
                    ]),
                    Err(error) => {
                        self.note(vec![Line::default().sanguine("✕ ").ink(error.to_string())])
                    }
                }
            }
        }
        Ok(())
    }
}
