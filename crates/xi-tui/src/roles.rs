//! The roles overlay: the preset and each role's model, where they come
//! from, and edits to the global config.

use std::collections::BTreeMap;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{Setting, SettingLayer, SettingsReport};
use xi_protocol::model::ModelRole;

use crate::callout::Kind;
use crate::frame::{Body, Frame, Hint, Note, Tone};
use crate::keys::{Action as Key, Keymap};
use crate::overlay::{Command, Ctx, Outcome, Overlay};
use crate::settings;
use crate::style::{Line, Role, row};

/// Label width, so models line up.
const LABEL: usize = 16;

/// A roles action that needs the connection or terminal owner.
pub(crate) enum Action {
    /// Change or remove (`Value::Null`) a key in the global config.
    Set {
        /// `preset`, `default_model` or `roles.<role>`.
        key: &'static str,
        /// New global value.
        value: Value,
    },
    /// Open the model picker to assign a role.
    Pick(ModelRole),
    /// Switch this session to a role's model.
    Use(ModelRef),
    /// Open the global config file in the external editor.
    EditFile,
}

/// The config key that assigns `role`.
pub(crate) const fn key(role: ModelRole) -> &'static str {
    match role {
        ModelRole::Slow => "roles.slow",
        ModelRole::Default => "default_model",
        ModelRole::Smol => "roles.smol",
        ModelRole::Tiny => "roles.tiny",
    }
}

/// Where a role's model comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The preset of its source layer.
    Preset,
    /// Unassigned, so the role uses the next tier up.
    Fallback(ModelRole),
    /// Its own key, or the built-in default for `default`.
    Key,
}

/// What dropping a role's global key reveals, shown as the first row of
/// the model picker.
pub(crate) struct Reset {
    /// `preset`, `fallback` or `built-in`.
    pub label: &'static str,
    /// The model it reveals, when known.
    pub value: String,
}

/// The roles overlay, keeping the selected row across report refreshes.
pub struct Roles {
    /// The latest effective settings, presets included.
    pub report: SettingsReport,
    /// 0 is the preset row, then one row per role in [`ModelRole::ALL`].
    selected: usize,
    note: Option<(Kind, String)>,
}

impl Roles {
    /// Opens the view at the preset row.
    #[must_use]
    pub fn new(report: SettingsReport) -> Self {
        Self {
            report,
            selected: 0,
            note: None,
        }
    }

    /// Completes a successful global edit.
    pub fn saved(&mut self, report: SettingsReport) {
        self.report = report;
        self.note(
            Kind::Done,
            "Saved to the global config. New sessions use it; u switches this one now.",
        );
    }

    /// Shows a result or error under the list.
    pub fn note(&mut self, kind: Kind, note: impl Into<String>) {
        self.note = Some((kind, note.into()));
    }

    fn setting(&self, key: &str) -> Option<&Setting> {
        setting(&self.report, key)
    }

    fn value(&self, key: &str) -> &str {
        self.setting(key)
            .and_then(|row| row.value.as_str())
            .unwrap_or("")
    }

    fn role(&self) -> Option<ModelRole> {
        self.selected
            .checked_sub(1)
            .map(|index| ModelRole::ALL[index])
    }

    fn preset(&self) -> Option<&str> {
        preset(&self.report)
    }

    fn origin(&self, role: ModelRole) -> Origin {
        origin(&self.report, role)
    }
}

fn setting<'a>(report: &'a SettingsReport, key: &str) -> Option<&'a Setting> {
    report.settings.iter().find(|row| row.key == key)
}

/// The preset in effect, if any.
fn preset(report: &SettingsReport) -> Option<&str> {
    setting(report, "preset").and_then(|row| row.value.as_str())
}

fn origin(report: &SettingsReport, role: ModelRole) -> Origin {
    match setting(report, key(role)) {
        Some(row) if row.from_preset => Origin::Preset,
        Some(row) if row.source != SettingLayer::Default => Origin::Key,
        _ => role.fallback().map_or(Origin::Key, Origin::Fallback),
    }
}

/// ` · 2 overrides` for the roles with their own key while a preset is in
/// effect; empty without a preset or overrides.
fn overrides(report: &SettingsReport) -> String {
    if preset(report).is_none() {
        return String::new();
    }
    match ModelRole::ALL
        .into_iter()
        .filter(|role| origin(report, *role) == Origin::Key)
        .count()
    {
        0 => String::new(),
        1 => " · 1 override".into(),
        n => format!(" · {n} overrides"),
    }
}

/// The preset and its overrides, as `claude · 2 overrides`, or `none`.
pub(crate) fn summary(report: &SettingsReport) -> String {
    preset(report).map_or_else(
        || "none".into(),
        |preset| format!("{preset}{}", overrides(report)),
    )
}

impl Roles {
    /// Why the global config cannot change `key`, if it cannot: a later
    /// file sets it, or the daemon reports it read only.
    fn blocked(&self, key: &str) -> Option<String> {
        let row = self.setting(key)?;
        if !row.editable {
            return Some("This daemon reports roles as read only.".into());
        }
        let files = &self.report.files;
        let path: &Path = match &row.source {
            SettingLayer::Default | SettingLayer::Global => return None,
            SettingLayer::Explicit { path } => path,
            SettingLayer::Project => &files.project,
            SettingLayer::Local => &files.local,
        };
        Some(format!(
            "Set in {}; edit that file to change it.",
            crate::view::home(path)
        ))
    }

    /// Whether `role` has its own key in the global config to drop.
    fn resettable(&self, role: ModelRole) -> bool {
        self.blocked(key(role)).is_none()
            && self.origin(role) == Origin::Key
            && self
                .setting(key(role))
                .is_some_and(|row| row.source == SettingLayer::Global)
    }

    /// The role's model and the models of every role, for the picker's
    /// selection and tags, plus the reset row when the role has a key to
    /// drop. Removing a global key reveals the global preset, the next
    /// tier up, or the built-in default, since the global file is the
    /// first layer.
    pub(crate) fn assignment(
        &self,
        role: ModelRole,
    ) -> (
        Option<ModelRef>,
        BTreeMap<ModelRole, ModelRef>,
        Option<Reset>,
    ) {
        let roles: BTreeMap<_, _> = ModelRole::ALL
            .into_iter()
            .filter_map(|role| Some((role, self.value(key(role)).parse().ok()?)))
            .collect();
        let reset = self.resettable(role).then(|| {
            if let Some(model) = self
                .preset()
                .and_then(|preset| self.report.presets.get(preset))
                .and_then(|preset| preset.get(&role))
            {
                Reset {
                    label: "preset",
                    value: format!("{}/{}", model.provider, model.model),
                }
            } else if let Some(fallback) = role.fallback() {
                Reset {
                    label: "fallback",
                    value: format!("↑ {fallback} · {}", self.value(key(fallback))),
                }
            } else {
                Reset {
                    label: "built-in",
                    value: String::new(),
                }
            }
        });
        (roles.get(&role).cloned(), roles, reset)
    }

    /// Handles navigation and prepares a change for the application owner.
    pub(crate) fn key(&mut self, key: KeyEvent, action: Option<Key>) -> Option<Action> {
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match action {
            Some(Key::Up) => {
                self.selected = self.selected.saturating_sub(1);
                self.note = None;
                return None;
            }
            Some(Key::Down) => {
                self.selected = (self.selected + 1).min(ModelRole::ALL.len());
                self.note = None;
                return None;
            }
            Some(Key::Send) => return self.enter(),
            _ => {}
        }
        if !plain {
            return None;
        }
        match key.code {
            KeyCode::Char('e') => Some(Action::EditFile),
            KeyCode::Char('r') => {
                let role = self.role()?;
                if self.resettable(role) {
                    return Some(Action::Set {
                        key: self::key(role),
                        value: Value::Null,
                    });
                }
                self.note(
                    Kind::Warn,
                    format!(
                        "{} has no override in the global config to drop.",
                        role.name()
                    ),
                );
                None
            }
            KeyCode::Char('u') => {
                let role = self.role()?;
                match self.value(self::key(role)).parse() {
                    Ok(model) => Some(Action::Use(model)),
                    Err(_) => {
                        self.note(Kind::Warn, format!("{} has no model to use.", role.name()));
                        None
                    }
                }
            }
            _ => None,
        }
    }

    fn enter(&mut self) -> Option<Action> {
        let Some(role) = self.role() else {
            if let Some(blocked) = self.blocked("preset") {
                self.note(Kind::Warn, blocked);
                return None;
            }
            let names: Vec<&String> = self.report.presets.keys().collect();
            let next = match self.preset() {
                None => names.first().copied(),
                Some(current) => names
                    .iter()
                    .position(|name| *name == current)
                    .and_then(|at| names.get(at + 1).copied()),
            };
            return Some(Action::Set {
                key: "preset",
                value: next.map_or(Value::Null, |name| Value::String(name.clone())),
            });
        };
        if let Some(blocked) = self.blocked(key(role)) {
            self.note(Kind::Warn, blocked);
            return None;
        }
        Some(Action::Pick(role))
    }

    /// The border, the preset in effect and the keys for the selected row.
    #[must_use]
    pub fn frame(&self, keys: &Keymap, back: Option<&str>) -> Frame {
        let mut hints = Vec::new();
        match self.role() {
            None => hints.extend(Hint::key(keys, Key::Send, "cycles", 0)),
            Some(role) => {
                hints.extend(Hint::key(keys, Key::Send, "picks", 0));
                if self.resettable(role) {
                    hints.push(Hint::new("r resets", 1).short("r"));
                }
                hints.push(Hint::new("u uses now", 2).short("u"));
            }
        }
        hints.push(Hint::new("e edits file", 3));
        hints.extend(match back {
            Some(back) => keys.key(Key::Escape).map(|key| {
                Hint::new(format!("{key} back to {back}"), 0).short(format!("{key} back"))
            }),
            None => Hint::key(keys, Key::Escape, "closes", 0),
        });
        let info = self.preset().map_or_else(
            || "no preset".into(),
            |preset| format!("{preset} preset{}", overrides(&self.report)),
        );
        Frame {
            tone: Tone::Focus,
            title: Line::default().ink("roles"),
            info: Some(Line::default().dim(info)),
            hints,
        }
    }

    /// The preset row, then one row per role, `inner` wide. `current` is
    /// the session's model, which `●` marks.
    #[must_use]
    pub fn body(&self, inner: usize, current: &ModelRef) -> Body {
        let current = format!("{}/{}", current.provider, current.model);
        let line = |index: usize, mark: bool, label: &str, value: Line, source: String| {
            let here = index == self.selected;
            let tint = if here { Role::Brass } else { Role::Ink };
            let left = Line::default()
                .span(tint, false, if here { "› " } else { "  " })
                .extend(if mark {
                    Line::default().brass("● ")
                } else {
                    Line::default().ink("  ")
                })
                .span(tint, false, format!("{label:<LABEL$}"))
                .extend(value);
            row(left, Line::default().dim(source), inner)
        };
        let tint = |index: usize| {
            if index == self.selected {
                Role::Brass
            } else {
                Role::Ink
            }
        };
        let preset = self.preset().unwrap_or("none");
        let mut list = vec![
            Line::default().strong(Role::Ink, "preset"),
            line(
                0,
                false,
                "Preset",
                Line::default().span(tint(0), false, preset),
                settings::layer(self.setting("preset")),
            ),
            Line::default().strong(Role::Ink, "roles"),
        ];
        for (offset, role) in ModelRole::ALL.into_iter().enumerate() {
            let index = offset + 1;
            let model = self.value(key(role));
            let (value, source) = match self.origin(role) {
                Origin::Fallback(fallback) => (
                    Line::default().dim(format!("↑ {fallback} · {model}")),
                    "fallback".into(),
                ),
                Origin::Preset => (
                    Line::default().span(tint(index), false, model),
                    "preset".into(),
                ),
                Origin::Key => (
                    Line::default().span(tint(index), false, model),
                    settings::layer(self.setting(key(role))),
                ),
            };
            list.push(line(index, model == current, role.name(), value, source));
        }
        let selected = if self.selected == 0 {
            1
        } else {
            self.selected + 2
        };
        let (kind, note) = self
            .note
            .clone()
            .unwrap_or_else(|| (Kind::Info, self.explain()));
        Body {
            list,
            selected: Some(selected),
            foot: vec![Note::new(kind, note)],
            ..Body::default()
        }
    }

    /// What `enter` and `r` do on the selected row.
    fn explain(&self) -> String {
        let Some(role) = self.role() else {
            if let Some(blocked) = self.blocked("preset") {
                return blocked;
            }
            let mut names: Vec<&str> = self.report.presets.keys().map(String::as_str).collect();
            names.push("none");
            return format!(
                "enter cycles {}. A preset replaces every role; [roles] and default_model in config.toml stay on top.",
                names.join(", ")
            );
        };
        let key = key(role);
        if let Some(blocked) = self.blocked(key) {
            return blocked;
        }
        match self.origin(role) {
            Origin::Preset => format!(
                "From the {} preset. enter picks a model to override it in config.toml.",
                self.preset().unwrap_or("")
            ),
            Origin::Fallback(fallback) => format!(
                "Unassigned, so {} uses {fallback}'s model. enter picks one.",
                role.name()
            ),
            Origin::Key if self.resettable(role) => {
                let after = if self.preset().is_some() {
                    "the preset applies again".into()
                } else if let Some(fallback) = role.fallback() {
                    format!("{} falls back to {fallback}", role.name())
                } else {
                    "the built-in default applies again".into()
                };
                if self.preset().is_some() {
                    format!(
                        "{key} in config.toml overrides the preset. enter picks a model, r drops the override so {after}."
                    )
                } else {
                    format!(
                        "{key} in config.toml sets this role. enter picks a model, r drops it so {after}."
                    )
                }
            }
            Origin::Key => "Built-in default. enter picks a model.".into(),
        }
    }
}

impl Overlay for Roles {
    fn name(&self) -> &'static str {
        "roles"
    }

    fn frame(&self, ctx: &Ctx) -> Frame {
        Self::frame(self, ctx.keys, ctx.parent)
    }

    fn body(&self, inner: usize, ctx: &Ctx) -> Body {
        Self::body(self, inner, &ctx.info.model)
    }

    fn key(&mut self, key: KeyEvent, action: Option<Key>, _: &Ctx) -> Outcome {
        if action == Some(Key::Escape) {
            return Outcome::Close;
        }
        match Self::key(self, key, action) {
            Some(action) => Outcome::Send(Box::new(Command::Role(action))),
            None => Outcome::Stay,
        }
    }
}
