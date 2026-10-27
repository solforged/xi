//! Approval presets and persisted workspace allow rules.

use super::{Approvals, Config};
use crate::Result;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use xi_policy::{ActionKind, Condition, RuleSpec};
use xi_protocol::Decision;

pub(super) fn approval_rules(config: &Config) -> Vec<RuleSpec> {
    if config.approvals == Approvals::Ask {
        return Vec::new();
    }
    let mut rules = vec![RuleSpec {
        effect: Decision::Allow,
        action: ActionKind::Read,
        pattern: "**".into(),
        when: Condition::Always,
    }];
    if config.approvals == Approvals::Auto {
        rules.extend([
            RuleSpec {
                effect: Decision::Allow,
                action: ActionKind::Write,
                pattern: "**".into(),
                when: Condition::Always,
            },
            RuleSpec {
                effect: Decision::Allow,
                action: ActionKind::Net,
                pattern: "*".into(),
                when: Condition::Always,
            },
        ]);
    }
    for path in [
        &config.paths.config,
        &config.paths.data,
        &config.paths.state,
    ] {
        ask_path(&mut rules, ActionKind::Read, path, true);
        if config.approvals == Approvals::Auto {
            ask_path(&mut rules, ActionKind::Write, path, true);
        }
    }
    if config.approvals == Approvals::Auto {
        for (path, descendants) in [
            (config.workspace.join(".git/config"), false),
            (config.workspace.join(".git/hooks"), true),
            (config.workspace.join(".xi"), true),
        ] {
            path_rules(
                &mut rules,
                Decision::Ask,
                ActionKind::Write,
                &path,
                descendants,
            );
        }
        // The sandbox keeps the worktree directory writable inside `.xi`.
        path_rules(
            &mut rules,
            Decision::Allow,
            ActionKind::Write,
            &config.workspace.join(xi_sandbox::WORKTREES),
            true,
        );
    }
    rules
}

fn ask_path(rules: &mut Vec<RuleSpec>, action: ActionKind, path: &Path, descendants: bool) {
    path_rules(rules, Decision::Ask, action, path, descendants);
}

fn path_rules(
    rules: &mut Vec<RuleSpec>,
    effect: Decision,
    action: ActionKind,
    path: &Path,
    descendants: bool,
) {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let pattern = globset::escape(&path.to_string_lossy()).replace('\\', "\\\\");
    if descendants {
        rules.push(RuleSpec {
            effect,
            action,
            pattern: format!("{}/**", pattern.trim_end_matches('/')),
            when: Condition::Always,
        });
    }
    rules.push(RuleSpec {
        effect,
        action,
        pattern,
        when: Condition::Always,
    });
}

/// Appends one exact allow rule without rewriting existing local settings.
pub(crate) fn append_workspace_allow(
    workspace: &Path,
    action: ActionKind,
    subject: &str,
) -> Result<()> {
    let directory = workspace.join(".xi");
    std::fs::create_dir_all(&directory)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(directory.join("config.local.toml"))?;
    let separator = if file.metadata()?.len() == 0 {
        ""
    } else {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] == b'\n' { "" } else { "\n" }
    };
    let action = match action {
        ActionKind::Read => "read",
        ActionKind::Write => "write",
        ActionKind::Exec => "exec",
        ActionKind::Net => "net",
        ActionKind::Tool => "tool",
    };
    // Policy globs treat `\` as an escape, which `globset::escape` leaves alone.
    let pattern = toml::Value::String(globset::escape(subject).replace('\\', "\\\\"));
    let text = format!(
        "{separator}\n[[rule]]\neffect = \"allow\"\naction = \"{action}\"\npattern = {pattern}\nwhen = \"always\"\n"
    );
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
