//! Effective configuration reports and format-preserving global edits.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, TableLike};
use xi_protocol::methods::client::{ConfigFiles, Setting, SettingLayer, SettingsReport};
use xi_protocol::model::Reasoning;

use crate::config::{Approvals, Config, Paths, parse_model, validate_global};
use crate::{Error, Result};

pub(crate) const KEYS: [&str; 13] = [
    "default_model",
    "reasoning",
    "approvals",
    "tools",
    "compaction.enabled",
    "compaction.trigger_tokens",
    "compaction.keep_recent_tokens",
    "extensions",
    "sandbox.readable",
    "sandbox.writable",
    "sandbox.env",
    "sandbox.env_command",
    "daemon.idle_exit",
];

/// Effective settings and their layers for a workspace, without running
/// `sandbox.env_command` or starting anything.
pub fn report(paths: &Paths, workspace: &Path, extra: &[PathBuf]) -> Result<SettingsReport> {
    let config = Config::load(paths.clone(), workspace, extra)?;
    let values = [
        json!(format!("{}/{}", config.model.provider, config.model.model)),
        json!(config.reasoning),
        json!(config.approvals),
        json!(config.tools),
        json!(config.compaction.enabled),
        json!(config.compaction.trigger_tokens),
        json!(config.compaction.keep_recent_tokens),
        json!(config.extensions),
        json!(config.sandbox.readable),
        json!(config.sandbox.writable),
        json!(config.sandbox.env),
        json!(config.sandbox.env_command),
        json!(config.daemon.idle_exit),
    ];
    let settings = KEYS
        .into_iter()
        .zip(values)
        .enumerate()
        .map(|(index, (key, value))| Setting {
            key: key.into(),
            value,
            source: config
                .setting_sources
                .get(key)
                .cloned()
                .unwrap_or(SettingLayer::Default),
            editable: index < 7,
            held_back: config.held_back.get(key).cloned(),
        })
        .collect();
    Ok(SettingsReport {
        files: ConfigFiles {
            global: paths.config.join("config.toml"),
            explicit: extra.iter().map(|path| paths.expand(path)).collect(),
            project: config.workspace.join(".xi/config.toml"),
            local: config.workspace.join(".xi/config.local.toml"),
        },
        trusted: config.trusted,
        settings,
        warnings: config.warnings,
    })
}

/// Sets or removes (`Value::Null`) one editable key in the global
/// `config.toml`, preserving comments, ordering and every other key.
pub fn set_global(paths: &Paths, key: &str, value: &Value) -> Result<()> {
    let replacement = validate_value(key, value)?;
    let path = paths.config.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let permissions = match std::fs::metadata(&path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let invalid = |message: String| Error::Config(format!("{key}: {message}"));
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| invalid(error.to_string()))?;
    match key.split_once('.') {
        Some((parent, field)) => {
            if document.get(parent).is_none() && replacement.is_some() {
                document[parent] = Item::Table(Table::new());
            }
            if let Some(item) = document.get_mut(parent) {
                let table = item
                    .as_table_like_mut()
                    .ok_or_else(|| invalid(format!("{parent} must be a table")))?;
                replace(table, field, replacement);
            }
        }
        None => replace(document.as_table_mut(), key, replacement),
    }
    let text = document.to_string();
    validate_global(&text, &path).map_err(|error| invalid(error.to_string()))?;
    std::fs::create_dir_all(&paths.config)?;
    let mut file = tempfile::NamedTempFile::new_in(&paths.config)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions)?;
    }
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(&path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

fn replace(table: &mut dyn TableLike, key: &str, replacement: Option<toml_edit::Value>) {
    let Some(mut value) = replacement else {
        table.remove(key);
        return;
    };
    if let Some(item) = table.get_mut(key) {
        if let Some(previous) = item.as_value() {
            *value.decor_mut() = previous.decor().clone();
        }
        *item = Item::Value(value);
    } else {
        table.insert(key, Item::Value(value));
    }
}

fn validate_value(key: &str, value: &Value) -> Result<Option<toml_edit::Value>> {
    if !KEYS[..7].contains(&key) {
        return Err(Error::Config(format!(
            "{key}: unknown or read-only setting"
        )));
    }
    if value.is_null() {
        return Ok(None);
    }
    let invalid = || Error::Config(format!("{key}: invalid value {value}"));
    let value = match key {
        "default_model" => {
            let model = value.as_str().ok_or_else(invalid)?;
            parse_model(model).map_err(|_| invalid())?;
            toml_edit::Value::from(model)
        }
        "reasoning" => {
            serde_json::from_value::<Reasoning>(value.clone()).map_err(|_| invalid())?;
            toml_edit::Value::from(value.as_str().ok_or_else(invalid)?)
        }
        "approvals" => {
            serde_json::from_value::<Approvals>(value.clone()).map_err(|_| invalid())?;
            toml_edit::Value::from(value.as_str().ok_or_else(invalid)?)
        }
        "tools" => {
            let values = value.as_array().ok_or_else(invalid)?;
            let mut array = toml_edit::Array::new();
            for value in values {
                let name = value
                    .as_str()
                    .filter(|name| xi_tools::is_builtin(name))
                    .ok_or_else(invalid)?;
                array.push(name);
            }
            toml_edit::Value::from(array)
        }
        "compaction.enabled" => toml_edit::Value::from(value.as_bool().ok_or_else(invalid)?),
        "compaction.trigger_tokens" | "compaction.keep_recent_tokens" => {
            let tokens = value
                .as_i64()
                .filter(|tokens| *tokens > 0)
                .ok_or_else(invalid)?;
            toml_edit::Value::from(tokens)
        }
        _ => return Err(invalid()),
    };
    Ok(Some(value))
}
