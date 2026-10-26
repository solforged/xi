use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Xi,
    Omp,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Arm {
    pub name: String,
    pub kind: Kind,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arms {
    arm: Vec<Arm>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    pub prompt: String,
    pub check: Vec<String>,
    pub timeout_seconds: u64,
    pub protected_paths: Vec<PathBuf>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct Task {
    pub name: String,
    pub root: PathBuf,
    pub spec: Spec,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn load_tasks(root: &Path, names: &[String]) -> Result<Vec<Task>> {
    let root = root.canonicalize()?;
    let mut selected = names.to_vec();
    if selected.is_empty() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("task.toml").is_file() {
                selected.push(
                    entry
                        .file_name()
                        .into_string()
                        .map_err(|_| "non-UTF-8 task name")?,
                );
            }
        }
        selected.sort();
    }
    let mut seen = BTreeSet::new();
    let mut tasks = Vec::new();
    for name in selected {
        if !valid_name(&name) || !seen.insert(name.clone()) {
            return Err(format!("invalid or duplicate task name: {name}").into());
        }
        let dir = root.join(&name);
        let spec: Spec = toml::from_str(&fs::read_to_string(dir.join("task.toml"))?)?;
        if spec.prompt.trim().is_empty()
            || spec.check.is_empty()
            || spec.check[0].is_empty()
            || spec.timeout_seconds == 0
        {
            return Err(format!(
                "{name}: prompt, check argv, and positive timeout_seconds are required"
            )
            .into());
        }
        for path in &spec.protected_paths {
            if path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
            {
                return Err(
                    format!("{name}: protected paths must be relative literal paths").into(),
                );
            }
            if !dir.join("repo").join(path).exists() {
                return Err(format!("{name}: missing protected path {}", path.display()).into());
            }
        }
        if !dir.join("repo").is_dir() || !dir.join("reference.patch").is_file() {
            return Err(format!("{name}: missing repo or reference.patch").into());
        }
        tasks.push(Task {
            name,
            root: dir,
            spec,
        });
    }
    if tasks.is_empty() {
        return Err("no tasks selected".into());
    }
    Ok(tasks)
}

pub(crate) fn load_arms(path: &Path, names: &[String]) -> Result<Vec<Arm>> {
    let config: Arms = toml::from_str(&fs::read_to_string(path)?)?;
    let mut seen = BTreeSet::new();
    for arm in &config.arm {
        if !valid_name(&arm.name)
            || !seen.insert(arm.name.as_str())
            || arm.argv.is_empty()
            || arm.argv[0].is_empty()
        {
            return Err(format!("invalid or duplicate arm: {}", arm.name).into());
        }
    }
    let mut arms = config.arm;
    if !names.is_empty() {
        let mut selected = Vec::new();
        for name in names {
            let index = arms
                .iter()
                .position(|a| &a.name == name)
                .ok_or_else(|| format!("unknown or duplicate arm: {name}"))?;
            selected.push(arms.remove(index));
        }
        arms = selected;
    }
    if arms.is_empty() {
        return Err("no arms selected".into());
    }
    Ok(arms)
}

pub(crate) fn expand(
    template: &str,
    prompt: &str,
    workspace: &Path,
    session_dir: &Path,
    model: &str,
) -> String {
    // A single pass keeps placeholders inside the prompt literal.
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let replacement = match &rest[..=end] {
            "{prompt}" => Some(prompt.into()),
            "{workspace}" => Some(workspace.to_string_lossy()),
            "{session_dir}" => Some(session_dir.to_string_lossy()),
            "{model}" => Some(model.into()),
            _ => None,
        };
        if let Some(value) = replacement {
            output.push_str(&value);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion_does_not_reinterpret_prompt_or_shell_quote_it() {
        assert_eq!(
            expand(
                "{prompt}",
                "keep {model}; 'literal'",
                Path::new("a"),
                Path::new("b"),
                "x"
            ),
            "keep {model}; 'literal'"
        );
        assert_eq!(
            expand(
                "{workspace}/{unknown}/{",
                "",
                Path::new("has spaces"),
                Path::new("b"),
                "x"
            ),
            "has spaces/{unknown}/{"
        );
    }
}
