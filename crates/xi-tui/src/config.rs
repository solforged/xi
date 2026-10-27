//! Client key bindings and display settings from `tui.toml`.

use std::io::{self, Write};
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

/// Saves the thinking display mode while preserving other client settings.
///
/// Missing parent directories and files are created. Invalid TOML or a
/// non-table `display` setting leaves the existing file untouched.
pub fn save_thinking(path: &Path, thinking: Thinking) -> io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !document.contains_key("display") {
        document["display"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let display = document["display"]
        .as_table_like_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "display must be a table"))?;
    let value = match thinking {
        Thinking::Full => "full",
        Thinking::First => "first",
        Thinking::Hidden => "hidden",
    };
    let mut value = toml_edit::Value::from(value);
    // Replace in place so the key keeps its leading comments.
    match display.get_mut("thinking") {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = toml_edit::Item::Value(value);
        }
        None => {
            display.insert("thinking", toml_edit::Item::Value(value));
        }
    }
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        file.as_file().set_permissions(metadata.permissions())?;
    }
    file.write_all(document.to_string().as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
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

    #[test]
    fn saving_thinking_preserves_comments_and_unrelated_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tui.toml");
        std::fs::write(
            &path,
            "# client settings\n[keys]\nsearch = 'ctrl+y' # keep binding\n\n[display]\n# keep display comment\nthinking = 'full' # keep mode comment\n",
        )
        .unwrap();
        save_thinking(&path, Thinking::Hidden).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        for preserved in [
            "# client settings",
            "search = 'ctrl+y' # keep binding",
            "# keep display comment",
            "# keep mode comment",
        ] {
            assert!(saved.contains(preserved), "{saved}");
        }
        let (loaded, warnings) = TuiConfig::load(Some(&path));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.thinking, Thinking::Hidden);
        assert_eq!(
            loaded.keymap.bindings(Action::Search),
            [Binding::parse("ctrl+y").unwrap()]
        );
    }

    #[test]
    fn saving_thinking_creates_parent_directories_and_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/config/tui.toml");
        save_thinking(&path, Thinking::First).unwrap();
        assert_eq!(TuiConfig::load(Some(&path)).0.thinking, Thinking::First);
        for invalid in ["[display", "display = false"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(save_thinking(&path, Thinking::Full).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
    }
}
