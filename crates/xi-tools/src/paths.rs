use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::{Context, Crossing};

pub(crate) fn resolve(path: &str, ctx: &Context) -> Result<PathBuf, String> {
    if path.is_empty() || path.contains('\0') {
        return Err("Invalid path: expected a nonempty path without NUL".to_owned());
    }
    let path = Path::new(path);
    resolve_absolute(&ctx.workspace.join(path), 0)
        .map_err(|error| format!("Cannot resolve {}: {error}", path.display()))
}

fn resolve_absolute(path: &Path, depth: usize) -> io::Result<PathBuf> {
    if depth > 40 {
        return Err(io::Error::other("too many symbolic links"));
    }
    match fs::canonicalize(path) {
        Ok(path) => return Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Resolve each existing component before consuming `..`. Lexical cleanup
    // first would misclassify `link/../new` when link points outside the root.
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => {
                resolved.push(component.as_os_str());
                match fs::symlink_metadata(&resolved) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        let target = fs::read_link(&resolved)?;
                        let target = if target.is_absolute() {
                            target
                        } else {
                            resolved.parent().unwrap_or(Path::new("/")).join(target)
                        };
                        resolved = resolve_absolute(&target, depth + 1)?;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(resolved)
}

fn under(path: &Path, roots: &[PathBuf], ctx: &Context) -> Result<bool, String> {
    for root in roots {
        let root = resolve_absolute(&ctx.workspace.join(root), 0)
            .map_err(|error| format!("Cannot resolve sandbox root {}: {error}", root.display()))?;
        if path.starts_with(root) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn crossings(path: &str, write: bool, ctx: &Context) -> Result<Vec<Crossing>, String> {
    let path = resolve(path, ctx)?;
    let writable = under(&path, &ctx.sandbox.writable, ctx)?;
    let inside = if write {
        writable && !under(&path, &ctx.sandbox.protected, ctx)?
    } else {
        writable || under(&path, &ctx.sandbox.readable, ctx)?
    };
    Ok(if inside {
        Vec::new()
    } else if write {
        vec![Crossing::Write(path)]
    } else {
        vec![Crossing::Read(path)]
    })
}

pub(crate) fn relative(path: &Path, workspace: &Path) -> PathBuf {
    let common = path
        .components()
        .zip(workspace.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut relative = PathBuf::new();
    for _ in workspace.components().skip(common) {
        relative.push("..");
    }
    for component in path.components().skip(common) {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}
