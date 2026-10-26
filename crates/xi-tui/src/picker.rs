//! A filterable model registry, grouped by provider.

use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::{ModelEntry, ModelsResult};

use crate::style::{Line, Role};

/// The model-selection overlay.
pub struct Picker {
    models: Vec<ModelEntry>,
    current: ModelRef,
    query: String,
    matches: Vec<usize>,
    selected: usize,
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
            current: result.current,
            query: String::new(),
            matches,
            selected,
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                format!("{}/{}", entry.model.provider, entry.model.model)
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect();
        self.selected = 0;
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
        self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1));
    }

    /// The selected model, including its credential availability.
    #[must_use]
    pub fn selected(&self) -> Option<&ModelEntry> {
        self.matches
            .get(self.selected)
            .map(|index| &self.models[*index])
    }

    /// Overlay rows with the selection kept visible within `rows`.
    #[must_use]
    pub fn render(&self, cols: usize, rows: usize) -> Vec<Line> {
        if rows == 0 {
            return Vec::new();
        }
        let mut lines = vec![
            Line::default()
                .brass("models")
                .dim(format!(
                    " · {} · type to filter · enter selects · esc closes",
                    self.query
                ))
                .truncate(cols),
        ];
        if rows == 1 {
            return lines;
        }
        if self.matches.is_empty() {
            lines.push(Line::default().dim("  no matching models").truncate(cols));
            return lines;
        }
        let capacity = rows - 1;
        let start = self.selected.saturating_sub(capacity.saturating_sub(2) / 2);
        let mut provider = None;
        for (position, index) in self.matches.iter().enumerate().skip(start) {
            if lines.len() == rows {
                break;
            }
            let entry = &self.models[*index];
            if provider != Some(entry.model.provider.as_str()) {
                provider = Some(entry.model.provider.as_str());
                if lines.len() + 1 >= rows && capacity > 1 {
                    break;
                }
                if lines.len() + 1 < rows {
                    lines.push(
                        Line::default()
                            .strong(Role::Ink, &entry.model.provider)
                            .truncate(cols),
                    );
                }
            }
            let cursor = if position == self.selected {
                "›"
            } else {
                " "
            };
            let current = if entry.model == self.current {
                "●"
            } else {
                " "
            };
            let mut label = format!(
                "{cursor} {current} {} · {}",
                entry.model.model,
                window(entry.info.context_window)
            );
            if !entry.info.reasoning.is_empty() {
                label.push_str(" · think");
            }
            if let Some(pricing) = entry.info.pricing {
                label.push_str(&format!(
                    " · ${}/${} /M in/out",
                    pricing.input, pricing.output
                ));
            }
            let line = if entry.available {
                Line::default().span(
                    if position == self.selected {
                        Role::Brass
                    } else {
                        Role::Ink
                    },
                    false,
                    label,
                )
            } else {
                label.push_str(if entry.model.provider == "chatgpt" {
                    " · no credentials · xi login chatgpt"
                } else {
                    " · no credentials"
                });
                Line::default().dim(label)
            };
            lines.push(line.truncate(cols));
        }
        lines
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
            models: vec![current, entry("chatgpt", "large", false)],
        });
        for c in "chatgpt".chars() {
            picker.input(c);
        }
        assert_eq!(picker.selected().unwrap().model.model, "large");
        let lines = picker.render(120, 10);
        assert!(
            lines
                .iter()
                .any(|line| line.plain().contains("xi login chatgpt"))
        );
        assert!(lines.iter().any(|line| line.plain().contains("1M")));
        assert!(lines.iter().all(|line| line.width() <= 120));
        picker.backspace();
        picker.up();
        picker.down();
    }

    #[test]
    fn short_overlays_keep_the_selection_and_provider_visible() {
        let models: Vec<_> = (0..20)
            .map(|index| entry(&format!("provider-{index:02}"), "model", true))
            .collect();
        let mut picker = Picker::new(ModelsResult {
            current: models[0].model.clone(),
            reasoning: Reasoning::Medium,
            models,
        });
        for _ in 0..15 {
            picker.down();
        }
        for rows in 2..9 {
            let lines = picker.render(80, rows);
            assert!(lines.len() <= rows);
            assert!(lines.iter().any(|line| line.plain().starts_with("›")));
            if rows >= 3 {
                assert!(lines.iter().any(|line| line.plain() == "provider-15"));
            }
        }
    }
}
