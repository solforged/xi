//! Effective settings, their sources and the settings editor overlay.

use std::borrow::Cow;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{Setting, SettingLayer, SettingsReport};
use xi_protocol::model::Reasoning;

use crate::editor::Editor;
use crate::style::{Line, Role};
use crate::transcript::Thinking;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Model,
    Reasoning,
    Thinking,
    Approvals,
    Tool(&'static str),
    Compaction,
    Trigger,
    Tail,
    ReadOnly(&'static str, &'static str),
    Trust,
}

const ROWS: &[Row] = &[
    Row::Model,
    Row::Reasoning,
    Row::Thinking,
    Row::Approvals,
    Row::Tool("read"),
    Row::Tool("edit"),
    Row::Tool("write"),
    Row::Tool("bash"),
    Row::Tool("search"),
    Row::Tool("hashline"),
    Row::Compaction,
    Row::Trigger,
    Row::Tail,
    Row::ReadOnly("Sandbox readable", "sandbox.readable"),
    Row::ReadOnly("Sandbox writable", "sandbox.writable"),
    Row::ReadOnly("Sandbox env", "sandbox.env"),
    Row::ReadOnly("Sandbox env command", "sandbox.env_command"),
    Row::ReadOnly("Extensions", "extensions"),
    Row::Trust,
    Row::ReadOnly("Daemon idle exit", "daemon.idle_exit"),
];

/// A settings action that needs the connection or terminal owner.
pub(crate) enum Action {
    /// No external action.
    None,
    /// Close the view.
    Close,
    /// Open the existing session model picker.
    Models,
    /// Cycle the current session's reasoning level.
    Reasoning,
    /// Cycle and persist the client thinking display.
    Thinking,
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

struct NumberInput {
    key: &'static str,
    editor: Editor,
}

/// The settings overlay, keeping the selected row across report refreshes.
pub struct Settings {
    /// The latest effective settings and config file locations.
    pub report: SettingsReport,
    selected: usize,
    note: Option<String>,
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

    /// Shows a result or error below the header.
    pub fn note(&mut self, note: impl Into<String>) {
        self.note = Some(note.into());
    }

    /// Completes a successful global edit.
    pub fn saved(&mut self, report: SettingsReport) {
        self.replace(report);
        self.note("saved · new sessions use it");
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
        let mut editor = Editor::default();
        if !self.value(key).is_null() {
            editor.set_text(&self.value(key).to_string());
        }
        self.input = Some(NumberInput { key, editor });
        self.note = None;
    }

    /// Handles navigation and prepares a change for the application owner.
    pub(crate) fn key(&mut self, key: KeyEvent, model: &ModelRef, reasoning: Reasoning) -> Action {
        if key.code == KeyCode::Esc {
            return Action::Close;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain && key.code == KeyCode::Char('e') {
            return Action::EditFile;
        }
        if let Some(input) = self.input.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let text = input.editor.text();
                    let value = if text.is_empty() && input.key == "compaction.trigger_tokens" {
                        Value::Null
                    } else {
                        match text.parse::<u64>() {
                            Ok(value) if value > 0 && value <= i64::MAX as u64 => {
                                Value::from(value)
                            }
                            _ => {
                                self.note("enter a positive token count");
                                return Action::None;
                            }
                        }
                    };
                    return Action::Set {
                        key: input.key,
                        value,
                    };
                }
                KeyCode::Backspace => input.editor.backspace(),
                KeyCode::Delete => input.editor.delete(),
                KeyCode::Left => input.editor.left(),
                KeyCode::Right => input.editor.right(),
                KeyCode::Home => input.editor.home(),
                KeyCode::End => input.editor.end(),
                KeyCode::Char(c) if plain && c.is_ascii_digit() => {
                    input.editor.insert(&c.to_string())
                }
                _ => {}
            }
            return Action::None;
        }
        let row = ROWS[self.selected];
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            return match row {
                Row::Model => Action::Set {
                    key: "default_model",
                    value: Value::String(format!("{}/{}", model.provider, model.model)),
                },
                Row::Reasoning => Action::Set {
                    key: "reasoning",
                    value: Value::String(reasoning.name().into()),
                },
                _ => Action::None,
            };
        }
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(ROWS.len() - 1),
            KeyCode::Enter | KeyCode::Char(' ') if plain => {
                return match row {
                    Row::Model if key.code == KeyCode::Enter => Action::Models,
                    Row::Reasoning => Action::Reasoning,
                    Row::Thinking => Action::Thinking,
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
                                self.note("cannot remove the last tool");
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
                    Row::Compaction => Action::Set {
                        key: "compaction.enabled",
                        value: Value::Bool(
                            !self.value("compaction.enabled").as_bool().unwrap_or(false),
                        ),
                    },
                    Row::Trigger if key.code == KeyCode::Enter => {
                        self.start_number("compaction.trigger_tokens");
                        Action::None
                    }
                    Row::Tail if key.code == KeyCode::Enter => {
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

    fn row(
        &self,
        row: Row,
        model: &ModelRef,
        reasoning: Reasoning,
        thinking: Thinking,
        tui_path: Option<&Path>,
    ) -> (String, String, String) {
        let (label, key) = match row {
            Row::Model => {
                return (
                    "Model".into(),
                    format!("{}/{}", model.provider, model.model),
                    "session".into(),
                );
            }
            Row::Reasoning => {
                return (
                    "Reasoning".into(),
                    reasoning.name().into(),
                    "session".into(),
                );
            }
            Row::Thinking => {
                return (
                    "Thinking display".into(),
                    thinking.name().into(),
                    if tui_path.is_some() {
                        "tui.toml"
                    } else {
                        "session"
                    }
                    .into(),
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
            Row::Compaction => ("Automatic compaction".into(), "compaction.enabled"),
            Row::Trigger => ("Compaction trigger".into(), "compaction.trigger_tokens"),
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
            Row::Trigger if self.value(key).is_null() => "80% of window".into(),
            Row::ReadOnly(_, "daemon.idle_exit") => format!("{} seconds", display(self.value(key))),
            _ => display(self.value(key)),
        };
        (label, value, self.source(key))
    }

    fn source(&self, key: &str) -> String {
        let Some(setting) = self.setting(key) else {
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

    /// Renders the visible rows and the cursor of an inline token field.
    #[must_use]
    pub fn render(
        &self,
        cols: usize,
        rows: usize,
        model: &ModelRef,
        reasoning: Reasoning,
        thinking: Thinking,
        tui_path: Option<&Path>,
    ) -> (Vec<Line>, Option<(usize, usize)>) {
        if rows == 0 {
            return (Vec::new(), None);
        }
        let mut header = Line::default().brass("settings").dim(format!(
            " · saved to {} · enter changes · ctrl+s saves default · e edits file · esc closes",
            crate::home(&self.report.files.global)
        ));
        if header.width() > cols {
            header = Line::default()
                .brass("settings")
                .dim(" · enter changes · ctrl+s saves · e file · esc closes");
        }
        if header.width() > cols {
            header = Line::default()
                .brass("settings")
                .dim(" · enter · e file · esc closes");
        }
        let mut lines = vec![header.truncate(cols)];
        let note = self.note.as_deref().map(Cow::Borrowed).or_else(|| {
            (!self.report.warnings.is_empty()).then(|| Cow::Owned(self.report.warnings.join(" · ")))
        });
        if let Some(note) = note.filter(|_| lines.len() < rows) {
            lines.push(Line::default().dim(format!("  {note}")).truncate(cols));
        }
        let capacity = rows.saturating_sub(lines.len());
        let start = self
            .selected
            .saturating_sub(capacity.saturating_sub(1) / 2)
            .min(ROWS.len().saturating_sub(capacity));
        let mut cursor = None;
        let visible: Vec<_> = ROWS
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .map(|(index, row)| (index, self.row(*row, model, reasoning, thinking, tui_path)))
            .collect();
        // Sources line up in one column unless a long value pushes them out.
        let column = visible
            .iter()
            .map(|(_, (_, value, _))| unicode_width::UnicodeWidthStr::width(value.as_str()))
            .filter(|width| *width <= 30)
            .max()
            .unwrap_or(0);
        for (index, (label, value, source)) in visible {
            let selected = index == self.selected;
            let role = if selected { Role::Brass } else { Role::Ink };
            let prefix = Line::default().span(
                role,
                false,
                format!("{} {label:<22}  ", if selected { "›" } else { " " }),
            );
            let line = if selected && let Some(input) = &self.input {
                let prefix = prefix.truncate(cols.saturating_sub(5));
                let available = cols.saturating_sub(prefix.width() + 2).max(1);
                let from = input.editor.cursor().saturating_sub(available - 1);
                let to = (from + available).min(input.editor.text().len());
                cursor = Some((
                    lines.len(),
                    (prefix.width() + 1 + input.editor.cursor() - from).min(cols.saturating_sub(1)),
                ));
                prefix
                    .brass("[")
                    .ink(&input.editor.text()[from..to])
                    .brass("]")
                    .dim(" · enter saves")
            } else {
                let pad =
                    column.saturating_sub(unicode_width::UnicodeWidthStr::width(value.as_str()));
                prefix
                    .span(role, false, value)
                    .dim(format!("{}  {source}", " ".repeat(pad)))
            };
            lines.push(line.truncate(cols));
        }
        (lines, cursor)
    }
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
