//! Client key bindings and display settings from `tui.toml`.

use std::path::Path;

use crate::keys::{Action, Binding, Keymap};
use crate::transcript::Thinking;

/// Effective client settings.
#[derive(Clone, Debug, Default)]
pub struct TuiConfig {
    /// Effective composer bindings.
    pub keymap: Keymap,
    /// How reasoning text is displayed.
    pub thinking: Thinking,
}

impl TuiConfig {
    /// Loads settings from a client configuration file, without failing.
    ///
    /// An absent path or missing file uses defaults without warnings. Invalid
    /// TOML uses defaults and one warning. Unknown tables, keys and actions,
    /// invalid values and invalid bindings each produce one warning and are
    /// skipped. Valid bindings in a mixed array still replace the defaults;
    /// an entirely invalid array leaves them unchanged. An empty array unbinds.
    #[must_use]
    pub fn load(path: Option<&Path>) -> (TuiConfig, Vec<String>) {
        let mut config = Self::default();
        let Some(path) = path else {
            return (config, Vec::new());
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (config, Vec::new());
            }
            Err(error) => return (config, vec![format!("cannot read settings: {error}")]),
        };
        let table = match text.parse::<toml::Table>() {
            Ok(table) => table,
            Err(error) => return (config, vec![format!("invalid TOML: {error}")]),
        };
        let mut warnings = Vec::new();
        for (name, value) in table {
            match name.as_str() {
                "keys" => config.keys(&value, &mut warnings),
                "display" => config.display(&value, &mut warnings),
                _ => warnings.push(format!("unknown setting {name:?}")),
            }
        }
        (config, warnings)
    }

    fn keys(&mut self, value: &toml::Value, warnings: &mut Vec<String>) {
        let Some(table) = value.as_table() else {
            warnings.push("keys must be a table".into());
            return;
        };
        for (name, value) in table {
            let Some(action) = Action::ALL.iter().copied().find(|a| a.name() == name) else {
                warnings.push(format!("unknown action {name:?}"));
                continue;
            };
            let mut bindings = Vec::new();
            match value {
                toml::Value::String(key) => {
                    if let Some(binding) = parse_binding(name, key, warnings) {
                        bindings.push(binding);
                    } else {
                        continue;
                    }
                }
                toml::Value::Array(keys) => {
                    for key in keys {
                        if let Some(key) = key.as_str() {
                            if let Some(binding) = parse_binding(name, key, warnings) {
                                bindings.push(binding);
                            }
                        } else {
                            warnings.push(format!("keys.{name}: binding must be a string"));
                        }
                    }
                    if !keys.is_empty() && bindings.is_empty() {
                        continue;
                    }
                }
                _ => {
                    warnings.push(format!(
                        "keys.{name} must be a string or an array of strings"
                    ));
                    continue;
                }
            }
            self.keymap.bind(action, bindings);
        }
    }

    fn display(&mut self, value: &toml::Value, warnings: &mut Vec<String>) {
        let Some(table) = value.as_table() else {
            warnings.push("display must be a table".into());
            return;
        };
        for (name, value) in table {
            if name != "thinking" {
                warnings.push(format!("unknown setting display.{name}"));
                continue;
            }
            self.thinking = match value.as_str() {
                Some("full") => Thinking::Full,
                Some("first") => Thinking::First,
                Some("hidden") => Thinking::Hidden,
                _ => {
                    warnings.push("display.thinking must be full, first or hidden".into());
                    continue;
                }
            };
        }
    }
}

fn parse_binding(name: &str, key: &str, warnings: &mut Vec<String>) -> Option<Binding> {
    match Binding::parse(key) {
        Ok(binding) => Some(binding),
        Err(error) => {
            warnings.push(format!("keys.{name}: {error}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn load(text: &str) -> (TuiConfig, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        std::fs::write(&path, text).unwrap();
        TuiConfig::load(Some(&path))
    }

    #[test]
    fn loads_keys_and_display() {
        let (config, warnings) = load(
            "[keys]\nsearch = 'ctrl+x'\nkill_word = ['ctrl+w', 'alt+backspace']\nsend = []\n[display]\nthinking = 'full'\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.thinking, Thinking::Full);
        assert!(config.keymap.bindings(Action::Send).is_empty());
        assert_eq!(
            config
                .keymap
                .action(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some(Action::Search)
        );
        assert_eq!(
            config
                .keymap
                .action(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(config.keymap.bindings(Action::KillWord).len(), 2);
        for (name, expected) in [("first", Thinking::First), ("hidden", Thinking::Hidden)] {
            assert_eq!(
                load(&format!("[display]\nthinking = '{name}'")).0.thinking,
                expected
            );
        }
    }

    #[test]
    fn warns_for_unknown_actions_and_bad_keys_without_losing_defaults() {
        let (config, warnings) = load(
            "[keys]\nunknown = 'ctrl+y'\nsearch = 'notakey'\nkill_word = ['ctrl+x', 'badkey', 42]\n",
        );
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("unknown action")));
        assert!(warnings.iter().any(|w| w.contains("notakey")));
        assert_eq!(
            config.keymap.bindings(Action::Search),
            Keymap::default().bindings(Action::Search)
        );
        assert_eq!(
            config.keymap.bindings(Action::KillWord),
            [Binding::parse("ctrl+x").unwrap()]
        );
    }

    #[test]
    fn warns_once_per_unknown_setting_or_invalid_value() {
        let (config, warnings) = load(
            "unexpected = true\n[keys]\nsearch = 1\n[display]\nthinking = 'other'\nunknown = false\n[extra]\nignored = 1\n",
        );
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        assert_eq!(config.thinking, Thinking::First);
        let (_, warnings) = load("keys = true\ndisplay = []");
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn bad_toml_returns_defaults_and_one_warning() {
        let (config, warnings) = load("[keys\nsearch = 'ctrl+y'");
        assert_eq!(warnings.len(), 1);
        assert_eq!(config.thinking, Thinking::First);
        assert_eq!(config.keymap.help(), Keymap::default().help());
    }

    #[test]
    fn missing_path_and_file_are_quiet_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("missing.toml");
        for path in [None, Some(absent.as_path())] {
            let (config, warnings) = TuiConfig::load(path);
            assert!(warnings.is_empty());
            assert_eq!(config.thinking, Thinking::First);
            assert_eq!(config.keymap.help(), Keymap::default().help());
        }
    }
}
