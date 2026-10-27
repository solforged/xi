//! Stable session prompts and lossless conversation conversion.

use std::collections::HashSet;
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
    /// Indexed skills in catalog order, one per name.
    pub skills: Vec<Skill>,
}

/// A skill listed in the session's catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Name from the frontmatter, invoked as `/skill:NAME`.
    pub name: String,
    /// Description from the frontmatter.
    pub description: String,
    /// Path of the skill's `SKILL.md`.
    pub path: PathBuf,
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
        for dir in &directories {
            append_instructions(&mut prompt, &dir.join("AGENTS.md"))?;
        }
        // Precedence order: project roots nearest the workspace first, then
        // the user's. The first skill with a name shadows later ones.
        let mut skill_roots = Vec::new();
        if config.trusted {
            skill_roots.extend(
                directories
                    .iter()
                    .rev()
                    .map(|dir| dir.join(".agents/skills")),
            );
        }
        skill_roots.push(config.paths.config.join("skills"));
        skill_roots.push(config.paths.home.join(".agents/skills"));
        let mut seen = HashSet::new();
        skill_roots.retain(|path| path.is_dir() && seen.insert(path.clone()));
        let mut skills: Vec<Skill> = Vec::new();
        for root in &skill_roots {
            let mut entries = std::fs::read_dir(root)?.collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let Some(skill) = load_skill(&entry.path()) else {
                    continue;
                };
                if let Some(winner) = skills.iter().find(|known| known.name == skill.name) {
                    eprintln!(
                        "xi: {}: skill {:?} is shadowed by {}",
                        skill.path.display(),
                        skill.name,
                        winner.path.display()
                    );
                    continue;
                }
                skills.push(skill);
            }
        }
        if !skills.is_empty() {
            prompt.push_str(
                "\nSkills\nWhen a task matches a skill's description, read its SKILL.md first, unless its <skill_content> is already in the conversation; read only matching skills. Resolve a skill's relative paths against its directory.\n",
            );
        }
        for skill in &skills {
            // JSON quoting keeps multiline metadata on a single index line.
            prompt.push_str(&format!(
                "- {}: {} ({})\n",
                serde_json::to_string(&skill.name)?,
                serde_json::to_string(&skill.description)?,
                skill.path.display()
            ));
        }
        Ok(Self {
            prompt,
            skill_roots,
            skills,
        })
    }
}

/// Frontmatter fields xi reads; other Agent Skills fields are ignored.
#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Reads `dir/SKILL.md` leniently, following the Agent Skills client guide:
/// a skill without parseable frontmatter or a description is skipped, while
/// spec violations that keep it usable only warn. Each problem goes to stderr.
fn load_skill(dir: &Path) -> Option<Skill> {
    let path = dir.join("SKILL.md");
    let warn = |message: &str| eprintln!("xi: {}: {message}", path.display());
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn(&format!("skill skipped: {e}"));
            return None;
        }
    };
    let Some((yaml, _)) = frontmatter(&text) else {
        warn("skill skipped: no `---` frontmatter block");
        return None;
    };
    let fields: Frontmatter = match serde_yaml_ng::from_str(yaml) {
        Ok(fields) => fields,
        Err(e) => {
            warn(&format!("skill skipped: {e}"));
            return None;
        }
    };
    let Some(description) = fields.description.filter(|d| !d.trim().is_empty()) else {
        warn("skill skipped: missing description");
        return None;
    };
    let directory = dir.file_name().and_then(|name| name.to_str()).unwrap_or("");
    let name = match fields.name {
        Some(name) => name,
        None => {
            warn("missing name; using the directory name");
            directory.to_owned()
        }
    };
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains(char::is_control) {
        warn(&format!(
            "skill skipped: {name:?} cannot be invoked as /skill:NAME"
        ));
        return None;
    }
    if name != directory {
        warn(&format!("name {name:?} differs from its directory"));
    }
    let valid = name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !valid {
        warn(&format!(
            "name {name:?} should be at most 64 lowercase letters, digits and single inner hyphens"
        ));
    }
    if description.chars().count() > 1024 {
        warn("description is longer than 1024 characters");
    }
    Some(Skill {
        name,
        description,
        path,
    })
}

/// Splits `SKILL.md` text into its YAML frontmatter and markdown body.
fn frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Prepends a skill's instructions when `content` starts with `/skill:NAME`.
/// The body is read now, so the stored message keeps the version used.
pub(crate) fn expand_skill(
    skills: &[Skill],
    content: Vec<Block>,
) -> std::result::Result<Vec<Block>, String> {
    let Some(Block::Text { text }) = content.first() else {
        return Ok(content);
    };
    let Some((name, _)) = xi_protocol::skill::invocation(text) else {
        return Ok(content);
    };
    let skill = skills
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| format!("unknown skill {name:?}"))?;
    let text = std::fs::read_to_string(&skill.path)
        .map_err(|e| format!("{}: {e}", skill.path.display()))?;
    let body = frontmatter(&text).map_or(text.as_str(), |(_, body)| body);
    let directory = skill
        .path
        .parent()
        .ok_or_else(|| format!("{}: skill path has no directory", skill.path.display()))?;
    let mut expanded = Vec::with_capacity(content.len() + 1);
    expanded.push(xi_protocol::skill::instructions(
        &skill.name,
        &directory.display().to_string(),
        body,
    ));
    expanded.extend(content);
    Ok(expanded)
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
