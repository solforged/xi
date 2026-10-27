//! A filterable model registry, grouped by provider. It switches the
//! session's model, or assigns one to a role from the roles view.

use std::collections::BTreeMap;

use serde_json::Value;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{ModelEntry, ModelsResult};
use xi_protocol::model::ModelRole;

use crossterm::event::KeyEvent;

use crate::frame::{Body, Cursor, Frame, Hint, Tone};
use crate::keys::{Action, Keymap};
use crate::overlay::{Command, Ctx, ListKey, Outcome, Overlay, list_key};
use crate::roles::{self, Reset};
use crate::style::{Line, Role, row};

/// The role a picker assigns, and what dropping its override reveals.
struct Assign {
    role: ModelRole,
    reset: Option<Reset>,
}

/// The model-selection overlay.
pub struct Picker {
    models: Vec<ModelEntry>,
    roles: BTreeMap<ModelRole, ModelRef>,
    current: ModelRef,
    query: String,
    matches: Vec<usize>,
    /// A position among the reset row, when shown, then the matches.
    selected: usize,
    assign: Option<Assign>,
}

impl Picker {
    /// Opens the registry with the current model selected.
    #[must_use]
    pub fn new(result: ModelsResult) -> Self {
        let mut models = result.models;
        models.sort_by(|a, b| {
            (&a.model.provider, &a.model.model).cmp(&(&b.model.provider, &b.model.model))
        });
        let selected = models
            .iter()
            .position(|entry| entry.model == result.current)
            .unwrap_or(0);
        let matches = (0..models.len()).collect();
        Self {
            models,
            roles: result.roles,
            current: result.current,
            query: String::new(),
            matches,
            selected,
            assign: None,
        }
    }

    /// Opens the registry to assign `role`, with its model selected and
    /// marked. `roles` tags rows with the configured roles, which may
    /// differ from the ones the session started with. `reset`, when
    /// given, is a first row that drops the role's override.
    pub(crate) fn assign(
        mut result: ModelsResult,
        role: ModelRole,
        current: Option<ModelRef>,
        roles: BTreeMap<ModelRole, ModelRef>,
        reset: Option<Reset>,
    ) -> Self {
        if let Some(current) = current {
            result.current = current;
        }
        result.roles = roles;
        let mut picker = Self::new(result);
        picker.selected += usize::from(reset.is_some());
        picker.assign = Some(Assign { role, reset });
        picker
    }

    /// 1 while the reset row shows: in assign mode, with an empty filter.
    fn offset(&self) -> usize {
        usize::from(
            self.query.is_empty()
                && self
                    .assign
                    .as_ref()
                    .is_some_and(|assign| assign.reset.is_some()),
        )
    }

    /// The roles that resolve to `model`, as `@smol @tiny`.
    fn tags(&self, model: &ModelRef) -> String {
        let tags: Vec<_> = self
            .roles
            .iter()
            .filter(|(_, assigned)| *assigned == model)
            .map(|(role, _)| format!("@{role}"))
            .collect();
        tags.join(" ")
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let model = format!("{}/{}", entry.model.provider, entry.model.model);
                (model.to_lowercase().contains(&query) || self.tags(&entry.model).contains(&query))
                    .then_some(index)
            })
            .collect();
        self.selected = self.offset();
    }

    /// Adds a character to the filter.
    pub fn input(&mut self, c: char) {
        self.query.push(c);
        self.filter();
    }

    /// Removes the last filter character.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.filter();
    }

    /// Selects the previous matching model.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Selects the next matching model.
    pub fn down(&mut self) {
        self.selected =
            (self.selected + 1).min((self.offset() + self.matches.len()).saturating_sub(1));
    }

    /// The selected model, including its credential availability; `None`
    /// on the reset row.
    #[must_use]
    pub fn selected(&self) -> Option<&ModelEntry> {
        self.selected
            .checked_sub(self.offset())
            .and_then(|position| self.matches.get(position))
            .map(|index| &self.models[*index])
    }

    /// The border, the count and the keys. `back` names the overlay `esc`
    /// returns to, when the picker was opened from one.
    #[must_use]
    pub fn frame(&self, keys: &Keymap, back: Option<&str>) -> Frame {
        let info = if self.query.is_empty() {
            format!("{} models", self.models.len())
        } else {
            format!("{} of {}", self.matches.len(), self.models.len())
        };
        let verb = if self.assign.is_some() {
            "assigns"
        } else {
            "selects"
        };
        let mut hints: Vec<Hint> = Hint::key(keys, Action::Send, verb, 0).into_iter().collect();
        hints.push(Hint::new("type to filter", 2).short("type"));
        hints.extend(match back {
            Some(back) => keys.key(Action::Escape).map(|key| {
                Hint::new(format!("{key} back to {back}"), 0).short(format!("{key} back"))
            }),
            None => Hint::key(keys, Action::Escape, "closes", 0),
        });
        let title = self
            .assign
            .as_ref()
            .map_or("models", |assign| assign.role.name());
        Frame {
            tone: Tone::Focus,
            title: Line::default().ink(title),
            info: Some(Line::default().dim(info)),
            hints,
        }
    }

    /// The filter row, then models under their provider headings, `inner`
    /// wide. Headings scroll with their models and are never selected.
    #[must_use]
    pub fn body(&self, inner: usize) -> Body {
        let head = vec![Line::default().dim("filter ").ink(&self.query)];
        let cursor = Some(Cursor::Head(
            0,
            unicode_width::UnicodeWidthStr::width(format!("filter {}", self.query).as_str()),
        ));
        if self.matches.is_empty() {
            return Body {
                head,
                list: vec![Line::default().dim("no matching models")],
                cursor,
                ..Body::default()
            };
        }
        let mut list = Vec::new();
        let mut selected = 0;
        let offset = self.offset();
        if let Some(assign) = self.assign.as_ref().filter(|_| offset > 0)
            && let Some(reset) = &assign.reset
        {
            let here = self.selected == 0;
            let left = Line::default()
                .span(
                    if here { Role::Brass } else { Role::Ink },
                    false,
                    format!(
                        "{}↺ {:<16}{}",
                        if here { "› " } else { "  " },
                        reset.label,
                        reset.value
                    ),
                )
                .truncate(inner);
            list.push(row(
                left,
                Line::default().dim(format!("drops {}", roles::key(assign.role))),
                inner,
            ));
        }
        let mut provider = None;
        for (position, index) in self.matches.iter().enumerate() {
            let entry = &self.models[*index];
            if provider != Some(entry.model.provider.as_str()) {
                provider = Some(entry.model.provider.as_str());
                list.push(Line::default().strong(Role::Ink, &entry.model.provider));
            }
            let here = position + offset == self.selected;
            if here {
                selected = list.len();
            }
            let mark = if entry.model == self.current {
                Line::default().brass("●")
            } else {
                Line::default().ink(" ")
            };
            let left = if here {
                Line::default()
                    .brass("› ")
                    .extend(mark)
                    .brass(format!(" {}", entry.model.model))
            } else {
                let id = if entry.available {
                    Line::default().ink(&entry.model.model)
                } else {
                    Line::default().dim(&entry.model.model)
                };
                Line::default().ink("  ").extend(mark).ink(" ").extend(id)
            };
            let mut meta = vec![window(entry.info.context_window)];
            if !entry.info.reasoning.is_empty() {
                meta.push("think".into());
            }
            if !entry.available {
                meta = vec![match entry.model.provider.as_str() {
                    "chatgpt" => "no credentials · xi login chatgpt".into(),
                    "grok" => "no credentials · xi login grok".into(),
                    _ => "no credentials".into(),
                }];
            } else if let Some(pricing) = entry.info.pricing {
                meta.push(format!("${}/${}", pricing.input, pricing.output));
            }
            let tags = self.tags(&entry.model);
            if !tags.is_empty() {
                meta.insert(0, tags);
            }
            list.push(row(left, Line::default().dim(meta.join(" · ")), inner));
        }
        Body {
            head,
            list,
            selected: Some(selected),
            cursor,
            ..Body::default()
        }
    }
}

impl Overlay for Picker {
    fn name(&self) -> &'static str {
        "models"
    }

    fn frame(&self, ctx: &Ctx) -> Frame {
        Self::frame(self, ctx.keys, ctx.parent)
    }

    fn body(&self, inner: usize, _: &Ctx) -> Body {
        Self::body(self, inner)
    }

    fn key(&mut self, key: KeyEvent, action: Option<Action>, _: &Ctx) -> Outcome {
        match list_key(key, action) {
            ListKey::Choose => {
                if let Some(assign) = &self.assign {
                    let value = match self.selected() {
                        Some(entry) => {
                            Value::String(format!("{}/{}", entry.model.provider, entry.model.model))
                        }
                        None if self.selected < self.offset() => Value::Null,
                        None => return Outcome::Stay,
                    };
                    return Outcome::Send(Box::new(Command::Role(roles::Action::Set {
                        key: roles::key(assign.role),
                        value,
                    })));
                }
                if let Some(entry) = self.selected() {
                    return Outcome::Send(Box::new(Command::SetModel(entry.clone())));
                }
            }
            ListKey::Close => return Outcome::Close,
            ListKey::Up => self.up(),
            ListKey::Down => self.down(),
            ListKey::Backspace => self.backspace(),
            ListKey::Type(c) => self.input(c),
            _ => {}
        }
        Outcome::Stay
    }
}

fn window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        let millions = tokens / 1_000_000;
        let tenths = tokens % 1_000_000 / 100_000;
        if tenths == 0 {
            format!("{millions}M")
        } else {
            format!("{millions}.{tenths}M")
        }
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xi_protocol::model::{ModelInfo, Reasoning};

    fn entry(provider: &str, id: &str, available: bool) -> ModelEntry {
        ModelEntry {
            model: ModelRef {
                provider: provider.into(),
                model: id.into(),
            },
            info: ModelInfo {
                provider: provider.into(),
                id: id.into(),
                context_window: 1_000_000,
                max_output: 4_096,
                reasoning: vec![Reasoning::Low, Reasoning::Medium, Reasoning::High],
                pricing: None,
            },
            available,
        }
    }

    #[test]
    fn filters_models_and_explains_missing_credentials() {
        let current = entry("local", "small", true);
        let mut picker = Picker::new(ModelsResult {
            current: current.model.clone(),
            reasoning: Reasoning::Medium,
            roles: BTreeMap::new(),
            models: vec![current, entry("chatgpt", "large", false)],
        });
        for c in "chatgpt".chars() {
            picker.input(c);
        }
        assert_eq!(picker.selected().unwrap().model.model, "large");
        let body = picker.body(116);
        let rows: Vec<String> = body.list.iter().map(Line::plain).collect();
        assert_eq!(rows[0], "chatgpt");
        assert!(rows[1].starts_with("›   large"), "{rows:?}");
        assert!(rows[1].ends_with("no credentials · xi login chatgpt"));
        assert_eq!(body.head[0].plain(), "filter chatgpt");
        assert_eq!(body.cursor, Some(Cursor::Head(0, 14)));
        assert_eq!(
            picker.frame(&Keymap::default(), None).info.unwrap().plain(),
            "1 of 2"
        );
        for _ in 0..7 {
            picker.backspace();
        }
        picker.down();
        let rows: Vec<String> = picker.body(116).list.iter().map(Line::plain).collect();
        assert!(rows[3].starts_with("› ● small"), "{rows:?}");
        assert!(rows[3].ends_with("1M · think"), "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= 116)
        );
    }

    #[test]
    fn grok_models_explain_how_to_sign_in() {
        let grok = entry("grok", "grok-4.6", false);
        let picker = Picker::new(ModelsResult {
            current: grok.model.clone(),
            reasoning: Reasoning::Medium,
            roles: BTreeMap::new(),
            models: vec![grok],
        });
        assert!(
            picker.body(116).list[1]
                .plain()
                .ends_with("no credentials · xi login grok")
        );
    }

    #[test]
    fn rows_name_their_roles_and_filter_on_them() {
        let big = entry("lab", "big", true);
        let small = entry("lab", "small", false);
        let roles = BTreeMap::from([
            (ModelRole::Slow, big.model.clone()),
            (ModelRole::Default, big.model.clone()),
            (ModelRole::Smol, small.model.clone()),
            (ModelRole::Tiny, small.model.clone()),
        ]);
        let mut picker = Picker::new(ModelsResult {
            current: big.model.clone(),
            reasoning: Reasoning::Medium,
            roles,
            models: vec![big, small, entry("lab", "other", true)],
        });
        let rows: Vec<String> = picker.body(116).list.iter().map(Line::plain).collect();
        assert!(rows[1].ends_with("@slow @default · 1M · think"), "{rows:?}");
        assert!(
            rows[3].ends_with("@smol @tiny · no credentials"),
            "{rows:?}"
        );
        assert!(rows[2].ends_with("  1M · think"), "{rows:?}");
        for c in "@tiny".chars() {
            picker.input(c);
        }
        assert_eq!(picker.selected().unwrap().model.model, "small");
        assert_eq!(picker.matches.len(), 1);
    }

    #[test]
    fn short_frames_keep_the_selection_under_its_provider() {
        let models: Vec<_> = (0..20)
            .map(|index| entry(&format!("provider-{index:02}"), "model", true))
            .collect();
        let mut picker = Picker::new(ModelsResult {
            current: models[0].model.clone(),
            reasoning: Reasoning::Medium,
            roles: BTreeMap::new(),
            models,
        });
        for _ in 0..15 {
            picker.down();
        }
        for rows in 5..12 {
            let (lines, _) = crate::frame::draw(
                &picker.frame(&Keymap::default(), Some("settings")),
                picker.body(76),
                80,
                rows,
            );
            let text: Vec<String> = lines.iter().map(Line::plain).collect();
            assert!(lines.len() <= rows);
            let at = text
                .iter()
                .position(|line| line.starts_with("│ › ● model") || line.starts_with("│ ›   model"))
                .unwrap_or_else(|| panic!("selection hidden at {rows} rows: {text:#?}"));
            if rows >= 7 {
                assert!(text[at - 1].starts_with("│ provider-15"), "{text:#?}");
            }
            assert!(
                text.last().unwrap().contains("esc back to settings"),
                "{text:#?}"
            );
        }
    }
}
