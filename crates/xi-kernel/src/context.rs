//! Stable session prompts and lossless conversation conversion.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use xi_protocol::{Block, EventKind, ToolCallId, ToolOutput};
use xi_provider::{Message, ToolResult};

use crate::config::Config;
use crate::{Error, Result, error_output};

/// A system prompt assembled exactly once for an open session.
#[derive(Clone, Debug)]
pub struct SystemContext {
    /// Complete prompt, reused byte-for-byte on every request.
    pub prompt: String,
    /// Skill directories that tools need to read.
    pub skill_roots: Vec<PathBuf>,
}

impl SystemContext {
    /// Reads instructions and skill metadata at session start.
    pub fn build(config: &Config) -> Result<Self> {
        Self::at(config, xi_protocol::Timestamp::now())
    }

    pub(crate) fn at(config: &Config, started: xi_protocol::Timestamp) -> Result<Self> {
        let date =
            time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(started.0) * 1_000_000)
                .map_err(|error| Error::Config(error.to_string()))?
                .date();
        let mut prompt = include_str!("prompt.md").to_owned();
        prompt.push_str(&format!(
            "\nEnvironment\nWorkspace: {}\nOS: {}\nDate (UTC): {}\n",
            config.workspace.display(),
            std::env::consts::OS,
            date
        ));
        append_instructions(&mut prompt, &config.paths.config.join("AGENTS.md"))?;
        let root = config
            .workspace
            .ancestors()
            .find(|p| p.join(".git").exists())
            .unwrap_or(&config.workspace);
        let mut directories: Vec<_> = config
            .workspace
            .ancestors()
            .take_while(|p| p.starts_with(root))
            .collect();
        directories.reverse();
        for dir in directories {
            append_instructions(&mut prompt, &dir.join("AGENTS.md"))?;
        }
        let mut skill_roots = vec![
            config.paths.config.join("skills"),
            config.paths.home.join(".agents/skills"),
        ];
        if config.trusted {
            skill_roots.push(config.workspace.join(".agents/skills"));
        }
        skill_roots.retain(|path| path.is_dir());
        prompt.push_str("\nSkills (read the listed file to use a skill)\n");
        for root in &skill_roots {
            let mut entries = std::fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path().join("SKILL.md");
                let Some(text) = optional_text(&path)? else {
                    continue;
                };
                let mut lines = text.lines();
                if lines.next() != Some("---") {
                    continue;
                }
                let mut yaml = String::new();
                let mut closed = false;
                for line in lines {
                    if line == "---" {
                        closed = true;
                        break;
                    }
                    yaml.push_str(line);
                    yaml.push('\n');
                }
                if !closed {
                    return Err(Error::Config(format!(
                        "{}: unterminated skill frontmatter",
                        path.display()
                    )));
                }
                let skill: Skill = serde_yaml_ng::from_str(&yaml)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                // JSON quoting keeps multiline metadata on a single index line.
                prompt.push_str(&format!(
                    "- {}: {} ({})\n",
                    serde_json::to_string(&skill.name)?,
                    serde_json::to_string(&skill.description)?,
                    path.display()
                ));
            }
        }
        Ok(Self {
            prompt,
            skill_roots,
        })
    }
}

#[derive(Deserialize)]
struct Skill {
    name: String,
    description: String,
}

fn optional_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Config(format!("{}: {e}", path.display()))),
    }
}

fn append_instructions(prompt: &mut String, path: &Path) -> Result<()> {
    if let Some(text) = optional_text(path)? {
        prompt.push_str(&format!("\nInstructions: {}\n", path.display()));
        prompt.push_str(&text);
        prompt.push('\n');
    }
    Ok(())
}

/// Converts a branch context, completing interrupted tool batches in memory.
/// Audit-only events never interrupt the grouping of tool results.
/// The summary is the first user-side content after the static system prompt.
/// `xi-session` supplies the retained tail before post-compaction descendants.
#[must_use]
pub fn messages(context: xi_session::Context<'_>) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(summary) = context.summary {
        messages.push(Message::User(vec![Block::Text {
            text: summary.into(),
        }]));
    }
    let mut pending = Vec::<ToolCallId>::new();
    let mut results = Vec::<ToolResult>::new();
    for event in context.events {
        match &event.kind {
            EventKind::UserMessage { content } => {
                flush_results(&mut messages, &mut pending, &mut results);
                messages.push(Message::User(content.clone()));
            }
            EventKind::AssistantMessage { content, .. } => {
                flush_results(&mut messages, &mut pending, &mut results);
                pending.extend(content.iter().filter_map(|block| match block {
                    Block::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                }));
                messages.push(Message::Assistant(content.clone()));
            }
            EventKind::ToolResult { call, output, .. } => {
                results.push(ToolResult {
                    call: call.clone(),
                    output: output.clone(),
                });
            }
            _ => {}
        }
    }
    flush_results(&mut messages, &mut pending, &mut results);
    messages
}

fn flush_results(
    messages: &mut Vec<Message>,
    pending: &mut Vec<ToolCallId>,
    results: &mut Vec<ToolResult>,
) {
    for call in pending.drain(..) {
        if !results.iter().any(|result| result.call == call) {
            results.push(ToolResult {
                call,
                output: interrupted_output(),
            });
        }
    }
    if !results.is_empty() {
        messages.push(Message::ToolResults(std::mem::take(results)));
    }
}

fn interrupted_output() -> ToolOutput {
    error_output("tool call interrupted before a result was recorded")
}
