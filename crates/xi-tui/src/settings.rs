//! Effective settings, their sources and the settings editor overlay.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use xi_protocol::methods::client::{Setting, SettingLayer, SettingsReport};

use crate::callout::Kind;
use crate::editor::Editor;
use crate::frame::{Body, Cursor, Frame, Hint, Note, Tone};
use crate::keys::{Action as Key, Keymap};
use crate::overlay::{Command, Ctx, Outcome, Overlay};
use crate::style::{self, Line, Role};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Model,
    Reasoning,
    Thinking,
    Layout,
    Roles,
    Approvals,
    Tool(&'static str),
    Compaction,
    Trigger,
    Tail,
    Sandbox,
    Titles,
    ReadOnly(&'static str, &'static str),
    Trust,
}

const ROWS: &[Row] = &[
    Row::Model,
    Row::Reasoning,
    Row::Thinking,
    Row::Layout,
    Row::Roles,
    Row::Approvals,
    Row::Compaction,
    Row::Trigger,
    Row::Tail,
    Row::Sandbox,
    Row::Titles,
    Row::Tool("read"),
    Row::Tool("edit"),
    Row::Tool("write"),
    Row::Tool("bash"),
    Row::Tool("search"),
    Row::Tool("hashline"),
    Row::Tool("task"),
    Row::Tool("web_search"),
    Row::Tool("fetch"),
    Row::ReadOnly("Sandbox readable", "sandbox.readable"),
    Row::ReadOnly("Sandbox writable", "sandbox.writable"),
    Row::ReadOnly("Sandbox env", "sandbox.env"),
    Row::ReadOnly("Sandbox env command", "sandbox.env_command"),
    Row::ReadOnly("SSH agent", "sandbox.ssh_agent"),
    Row::ReadOnly("Web search backend", "web_search.backend"),
    Row::ReadOnly("Web search model", "web_search.model"),
    Row::ReadOnly("Extensions", "extensions"),
    Row::Trust,
    Row::ReadOnly("Daemon idle exit", "daemon.idle_exit"),
];

/// The heading a section starts with, by the index of its first row.
fn heading(index: usize) -> Option<&'static str> {
    match ROWS[index] {
        Row::Model => Some("this session"),
        Row::Thinking => Some("client"),
        Row::Roles => Some("defaults"),
        Row::Tool("read") => Some("tools"),
        Row::ReadOnly(_, "sandbox.readable") => Some("read only"),
        _ => None,
    }
}

/// Label width, so values line up.
const LABEL: usize = 22;

/// A settings action that needs the connection or terminal owner.
pub(crate) enum Action {
    /// No external action.
    None,
    /// Close the view.
    Close,
    /// Open the existing session model picker.
    Models,
    /// Open the roles view.
    Roles,
    /// Cycle the current session's reasoning level.
    Reasoning,
    /// Cycle and persist the client thinking display.
    Thinking,
    /// Cycle and persist the client layout.
    Layout,
    /// Change an editable global setting.
    Set {
        /// Dotted setting key.
        key: &'static str,
        /// New global value.
        value: Value,
    },
    /// Open the global config file in the external editor.
    EditFile,
}

#[derive(Clone, Copy)]
enum NumberUnit {
    Percent,
    Tokens,
}

impl NumberUnit {
    fn label(self) -> &'static str {
        match self {
            Self::Percent => "%",
            Self::Tokens => "tokens",
        }
    }
}

struct NumberInput {
    key: &'static str,
    editor: Editor,
    unit: NumberUnit,
}

impl NumberInput {
    fn number(&self) -> Result<u64, &'static str> {
        match self.unit {
            NumberUnit::Percent => self
                .editor
                .text()
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=100).contains(value))
                .ok_or("enter a percentage from 1 to 100"),
            NumberUnit::Tokens => self
                .editor
                .text()
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0 && *value <= i64::MAX as u64)
                .ok_or("enter a positive token count"),
        }
    }

    fn switch_unit(&mut self, window: Option<u64>) {
        let number = self.number().ok();
        let converted = match self.unit {
            NumberUnit::Percent => {
                self.unit = NumberUnit::Tokens;
                number
                    .zip(window)
                    .map(|(percent, window)| threshold(percent, window))
                    .filter(|tokens| *tokens > 0 && *tokens <= i64::MAX as u64)
            }
            NumberUnit::Tokens => {
                self.unit = NumberUnit::Percent;
                number.zip(window).and_then(|(tokens, window)| {
                    let scaled = u128::from(tokens) * 100;
                    let percent = scaled / u128::from(window);
                    (scaled % u128::from(window) == 0 && (1..=100).contains(&percent))
                        .then_some(percent as u64)
                })
            }
        };
        self.editor
            .set_text(&converted.map_or_else(String::new, |value| value.to_string()));
    }
}

/// The settings overlay, keeping the selected row across report refreshes.
pub struct Settings {
    /// The latest effective settings and config file locations.
    pub report: SettingsReport,
    selected: usize,
    note: Option<(Kind, String)>,
    input: Option<NumberInput>,
}

impl Settings {
    /// Opens the view at the session model row.
    #[must_use]
    pub fn new(report: SettingsReport) -> Self {
        Self {
            report,
            selected: 0,
            note: None,
            input: None,
        }
    }

    /// Replaces the effective settings without moving the selection.
    pub fn replace(&mut self, report: SettingsReport) {
        self.report = report;
        self.input = None;
        self.note = None;
    }

    /// Shows a result or error below the list.
    pub fn note(&mut self, kind: Kind, note: impl Into<String>) {
        self.note = Some((kind, note.into()));
    }

    /// Completes a successful global edit.
    pub fn saved(&mut self, report: SettingsReport) {
        self.replace(report);
        self.note(
            Kind::Done,
            "Saved to the global config. Running sessions keep the settings they started with.",
        );
    }

    fn setting(&self, key: &str) -> Option<&Setting> {
        self.report
            .settings
            .iter()
            .find(|setting| setting.key == key)
    }

    fn value(&self, key: &str) -> &Value {
        self.setting(key)
            .map_or(&Value::Null, |setting| &setting.value)
    }

    fn start_number(&mut self, key: &'static str) {
        let value = self.value(key);
        let (unit, number) = if key == "compaction.trigger" {
            if let Some(tokens) = value.get("tokens").and_then(Value::as_u64) {
                (NumberUnit::Tokens, tokens)
            } else {
                (
                    NumberUnit::Percent,
                    value.get("percent").and_then(Value::as_u64).unwrap_or(80),
                )
            }
        } else {
            (NumberUnit::Tokens, value.as_u64().unwrap_or(0))
        };
        let mut editor = Editor::default();
        editor.set_text(&number.to_string());
        self.input = Some(NumberInput { key, editor, unit });
        self.note = None;
    }

    /// Handles navigation and prepares a change for the application owner.
    /// `action` is the key's keymap action, so rebound keys work here too.
    pub(crate) fn key(&mut self, key: KeyEvent, action: Option<Key>, ctx: &Ctx) -> Action {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let Some(input) = self.input.as_mut() {
            if plain && input.key == "compaction.trigger" {
                match key.code {
                    KeyCode::Char('u') => {
                        input.switch_unit(context_window(ctx));
                        self.note = input.editor.text().is_empty().then(|| {
                            (Kind::Warn, "Cannot preserve this threshold in the other unit; enter a new value before saving.".into())
                        });
                        return Action::None;
                    }
                    KeyCode::Char('r') => {
                        return Action::Set {
                            key: "compaction.trigger",
                            value: Value::Null,
                        };
                    }
                    _ => {}
                }
            }
            match (action, key.code) {
                (_, KeyCode::Char(c)) if plain && c.is_ascii_digit() => {
                    input.editor.insert(&c.to_string());
                }
                (Some(Key::Escape), _) => self.input = None,
                (Some(Key::Send), _) => {
                    let number = match input.number() {
                        Ok(number) => number,
                        Err(message) => {
                            self.note(Kind::Warn, message);
                            return Action::None;
                        }
                    };
                    let value = if input.key == "compaction.trigger" {
                        match input.unit {
                            NumberUnit::Percent => json!({ "percent": number }),
                            NumberUnit::Tokens => json!({ "tokens": number }),
                        }
                    } else {
                        Value::from(number)
                    };
                    return Action::Set {
                        key: input.key,
                        value,
                    };
                }
                (Some(Key::Backspace), _) => input.editor.backspace(),
                (Some(Key::Delete | Key::DeleteOrQuit), _) => input.editor.delete(),
                (Some(Key::Left), _) => input.editor.left(),
                (Some(Key::Right), _) => input.editor.right(),
                (Some(Key::Home), _) => input.editor.home(),
                (Some(Key::End), _) => input.editor.end(),
                _ => {}
            }
            return Action::None;
        }
        if action == Some(Key::Escape) {
            return Action::Close;
        }
        if plain && key.code == KeyCode::Char('e') {
            return Action::EditFile;
        }
        let row = ROWS[self.selected];
        if row == Row::Trigger && plain && key.code == KeyCode::Char('r') {
            return Action::Set {
                key: "compaction.trigger",
                value: Value::Null,
            };
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return match row {
                Row::Model => Action::Set {
                    key: "default_model",
                    value: Value::String(format!(
                        "{}/{}",
                        ctx.info.model.provider, ctx.info.model.model
                    )),
                },
                Row::Reasoning => Action::Set {
                    key: "reasoning",
                    value: Value::String(ctx.info.reasoning.name().into()),
                },
                _ => Action::None,
            };
        }
        let enter = action == Some(Key::Send);
        match action {
            Some(Key::Up) => self.selected = self.selected.saturating_sub(1),
            Some(Key::Down) => self.selected = (self.selected + 1).min(ROWS.len() - 1),
            _ if enter || (plain && key.code == KeyCode::Char(' ')) => {
                return match row {
                    Row::Model if enter => Action::Models,
                    Row::Roles if enter => Action::Roles,
                    Row::Reasoning => Action::Reasoning,
                    Row::Thinking => Action::Thinking,
                    Row::Layout => Action::Layout,
                    Row::Approvals => Action::Set {
                        key: "approvals",
                        value: Value::String(
                            match self.value("approvals").as_str() {
                                Some("ask") => "reads",
                                Some("reads") => "auto",
                                _ => "ask",
                            }
                            .into(),
                        ),
                    },
                    Row::Tool(tool) => {
                        let Some(tools) = self.value("tools").as_array() else {
                            return Action::None;
                        };
                        let mut tools = tools.clone();
                        if tools.iter().any(|value| value.as_str() == Some(tool)) {
                            tools.retain(|value| value.as_str() != Some(tool));
                            if tools.is_empty() {
                                self.note(Kind::Warn, "cannot remove the last tool");
                                return Action::None;
                            }
                        } else {
                            tools.push(Value::String(tool.into()));
                        }
                        Action::Set {
                            key: "tools",
                            value: Value::Array(tools),
                        }
                    }
                    Row::Sandbox => Action::Set {
                        key: "sandbox.enabled",
                        value: Value::Bool(
                            !self.value("sandbox.enabled").as_bool().unwrap_or(true),
                        ),
                    },
                    Row::Titles => Action::Set {
                        key: "title.auto",
                        value: Value::Bool(!self.value("title.auto").as_bool().unwrap_or(true)),
                    },
                    Row::Compaction => Action::Set {
                        key: "compaction.enabled",
                        value: Value::Bool(
                            !self.value("compaction.enabled").as_bool().unwrap_or(false),
                        ),
                    },
                    Row::Trigger if enter => {
                        self.start_number("compaction.trigger");
                        Action::None
                    }
                    Row::Tail if enter => {
                        self.start_number("compaction.keep_recent_tokens");
                        Action::None
                    }
                    _ => Action::None,
                };
            }
            _ => {}
        }
        Action::None
    }

    fn row(&self, row: Row, ctx: &Ctx) -> (String, String, String) {
        let (label, key) = match row {
            Row::Model => {
                return (
                    "Model".into(),
                    format!("{}/{}", ctx.info.model.provider, ctx.info.model.model),
                    "session".into(),
                );
            }
            Row::Reasoning => {
                return (
                    "Reasoning".into(),
                    ctx.info.reasoning.name().into(),
                    "session".into(),
                );
            }
            Row::Thinking => {
                return (
                    "Thinking display".into(),
                    ctx.thinking.name().into(),
                    if ctx.tui_path.is_some() {
                        "tui.toml"
                    } else {
                        "session"
                    }
                    .into(),
                );
            }
            Row::Layout => {
                return (
                    "Layout".into(),
                    ctx.layout.name().into(),
                    if ctx.tui_path.is_some() {
                        "tui.toml"
                    } else {
                        "session"
                    }
                    .into(),
                );
            }
            Row::Roles => {
                return (
                    "Roles".into(),
                    crate::roles::summary(&self.report),
                    self.source("preset"),
                );
            }
            Row::Approvals => ("Approvals".into(), "approvals"),
            Row::Tool(tool) => {
                let checked = self
                    .value("tools")
                    .as_array()
                    .is_some_and(|tools| tools.iter().any(|value| value.as_str() == Some(tool)));
                return (
                    format!("Tool {tool}"),
                    if checked { "[x]" } else { "[ ]" }.into(),
                    self.source("tools"),
                );
            }
            Row::Sandbox => ("Tool sandbox".into(), "sandbox.enabled"),
            Row::Titles => ("Session titles".into(), "title.auto"),
            Row::Compaction => ("Automatic compaction".into(), "compaction.enabled"),
            Row::Trigger => ("Compaction trigger".into(), "compaction.trigger"),
            Row::Tail => ("Kept tail".into(), "compaction.keep_recent_tokens"),
            Row::ReadOnly(label, key) => (label.into(), key),
            Row::Trust => {
                return (
                    "Trust".into(),
                    if self.report.trusted {
                        "trusted"
                    } else {
                        "untrusted · xi trust"
                    }
                    .into(),
                    "workspace".into(),
                );
            }
        };
        let value = match row {
            Row::Trigger => {
                if let Some(tokens) = self.value(key).get("tokens").and_then(Value::as_u64) {
                    format!("{tokens} tokens (fixed)")
                } else {
                    let percent = self
                        .value(key)
                        .get("percent")
                        .and_then(Value::as_u64)
                        .unwrap_or(80);
                    trigger_display(percent, context_window(ctx))
                }
            }
            Row::Tail => format!("{} tokens", display(self.value(key))),
            Row::ReadOnly(_, "daemon.idle_exit") => format!("{} seconds", display(self.value(key))),
            _ => display(self.value(key)),
        };
        (label, value, self.source(key))
    }

    fn source(&self, key: &str) -> String {
        layer(self.setting(key))
    }

    /// The border, the global config path and the keys. `ctrl+s` shows
    /// only on rows it saves.
    #[must_use]
    pub fn frame(&self, keys: &Keymap) -> Frame {
        let mut hints = Vec::new();
        if self.input.is_some() {
            hints.extend(Hint::key(keys, Key::Send, "saves", 0));
            hints.extend(Hint::key(keys, Key::Escape, "cancels", 0));
            if self
                .input
                .as_ref()
                .is_some_and(|input| input.key == "compaction.trigger")
            {
                hints.push(Hint::new("u switches %/tokens", 0).short("u unit"));
                hints.push(Hint::new("r resets to inherited/80%", 0).short("r reset 80%"));
            }
        } else {
            let verb = if ROWS[self.selected] == Row::Roles {
                "opens"
            } else {
                "changes"
            };
            hints.extend(Hint::key(keys, Key::Send, verb, 0));
            if matches!(ROWS[self.selected], Row::Model | Row::Reasoning) {
                hints.push(Hint::new("ctrl+s saves default", 2).short("ctrl+s"));
            }
            if ROWS[self.selected] == Row::Trigger {
                hints.push(Hint::new("r resets to inherited/80%", 0).short("r reset 80%"));
            }
            hints.push(Hint::new("e edits file", 3));
            hints.extend(Hint::key(keys, Key::Escape, "closes", 0));
        }
        Frame {
            tone: Tone::Focus,
            title: Line::default().ink("settings"),
            info: (!self.report.files.global.as_os_str().is_empty())
                .then(|| Line::default().dim(crate::view::home(&self.report.files.global))),
            hints,
        }
    }

    /// Rows under section headings, `inner` wide, with sources at the
    /// right where they truncate before labels and values. An inline
    /// number field holds the cursor.
    #[must_use]
    fn body(&self, inner: usize, ctx: &Ctx) -> Body {
        let mut list = Vec::new();
        let mut selected = 0;
        let mut cursor = None;
        for (index, row) in ROWS.iter().enumerate() {
            if let Some(heading) = heading(index) {
                list.push(Line::default().strong(Role::Ink, heading));
            }
            let (label, value, source) = self.row(*row, ctx);
            let here = index == self.selected;
            let role = if here { Role::Brass } else { Role::Ink };
            let prefix = Line::default().span(
                role,
                false,
                format!("{} {label:<LABEL$}  ", if here { "›" } else { " " }),
            );
            if here {
                selected = list.len();
            }
            let line = if here && let Some(input) = &self.input {
                let text = input.editor.text();
                let unit = input.unit.label();
                let unit = if inner < unit.len() + 4 && matches!(input.unit, NumberUnit::Tokens) {
                    "tok"
                } else {
                    unit
                };
                let gap = if inner < unit.len() + 5 { "[" } else { " [" };
                let reserve = (unit.len() + gap.len() + 1 + (text.len() + 1).min(6)).min(inner);
                let room = inner.saturating_sub(reserve);
                let prefix = if prefix.width() > room && room > 0 {
                    prefix.truncate(room - 1).ink(" ")
                } else {
                    prefix.truncate(room)
                };
                let prefix = prefix.brass(unit).brass(gap);
                let available = inner.saturating_sub(prefix.width() + 1).max(1);
                let from = input.editor.cursor().saturating_sub(available - 1);
                let to = (from + available).min(text.len());
                cursor = Some(Cursor::List(
                    list.len(),
                    prefix.width() + input.editor.cursor() - from,
                ));
                let mut line = prefix.ink(&text[from..to]).brass("]");
                if matches!(input.unit, NumberUnit::Percent)
                    && let Ok(percent) = input.number()
                {
                    let effective = context_window(ctx).map_or_else(
                        || "token count unknown".into(),
                        |window| format!("{} tokens", threshold(percent, window)),
                    );
                    line = line.dim(format!(" · {effective}"));
                }
                line
            } else {
                let prefix = if matches!(row, Row::Trigger | Row::Tail) {
                    let room = inner.saturating_sub(value.chars().count().min(24));
                    if prefix.width() > room && room > 0 {
                        prefix.truncate(room - 1).ink(" ")
                    } else {
                        prefix.truncate(room)
                    }
                } else {
                    prefix
                };
                style::row(
                    prefix.span(role, false, value),
                    Line::default().dim(source),
                    inner,
                )
            };
            list.push(line);
        }
        let note = self.note.clone().or_else(|| {
            if ROWS[self.selected] == Row::Sandbox {
                Some((Kind::Warn, "Off gives tool commands unrestricted host file and network access. Extensions stay sandboxed.".into()))
            } else if ROWS[self.selected] == Row::Trigger {
                Some((Kind::Info, "u switches units while editing; r removes the global override and uses inherited settings (default 80%).".into()))
            } else {
                (!self.report.warnings.is_empty())
                    .then(|| (Kind::Warn, self.report.warnings.join(" · ")))
            }
        });
        Body {
            list,
            selected: Some(selected),
            foot: note
                .into_iter()
                .map(|(kind, text)| Note::new(kind, text))
                .collect(),
            cursor,
            ..Body::default()
        }
    }
}

impl Overlay for Settings {
    fn name(&self) -> &'static str {
        "settings"
    }

    fn frame(&self, ctx: &Ctx) -> Frame {
        Self::frame(self, ctx.keys)
    }

    fn body(&self, inner: usize, ctx: &Ctx) -> Body {
        Self::body(self, inner, ctx)
    }

    fn key(&mut self, key: KeyEvent, action: Option<Key>, ctx: &Ctx) -> Outcome {
        match Self::key(self, key, action, ctx) {
            Action::None => Outcome::Stay,
            Action::Close => Outcome::Close,
            change => Outcome::Send(Box::new(Command::Setting(change))),
        }
    }
}

fn context_window(ctx: &Ctx) -> Option<u64> {
    ctx.info
        .model_info
        .as_ref()
        .map(|model| model.context_window)
        .filter(|window| *window > 0)
}

fn threshold(percent: u64, window: u64) -> u64 {
    (u128::from(window) * u128::from(percent) / 100) as u64
}

fn trigger_display(percent: u64, window: Option<u64>) -> String {
    window.map_or_else(
        || format!("{percent}% · token count unknown"),
        |window| format!("{percent}% · {} tokens", threshold(percent, window)),
    )
}

fn display(value: &Value) -> String {
    match value {
        Value::Null => "unset".into(),
        Value::String(value) => value.clone(),
        Value::Array(values) if values.is_empty() => "none".into(),
        Value::Array(values) => values.iter().map(display).collect::<Vec<_>>().join(", "),
        _ => value.to_string(),
    }
}

/// The layer that supplied a setting, as the source column shows it.
pub(crate) fn layer(setting: Option<&Setting>) -> String {
    let Some(setting) = setting else {
        return "default".into();
    };
    let mut source = match &setting.source {
        SettingLayer::Default => "default".into(),
        SettingLayer::Global => "global".into(),
        SettingLayer::Explicit { path } => format!(
            "--config {}",
            path.file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
        ),
        SettingLayer::Project => "project".into(),
        SettingLayer::Local => "local".into(),
    };
    if setting.held_back.is_some() {
        source.push_str(" · held back (untrusted)");
    }
    source
}
