//! Per-session sandbox construction and host configuration materialization.

use crate::config::{Config, Paths};
use crate::{Error, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Host settings used when materializing a session sandbox.
#[derive(Debug)]
pub struct HostProbe {
    git: HashMap<&'static str, String>,
    #[cfg(target_os = "macos")]
    developer_tools: Option<(String, Vec<PathBuf>)>,
}

/// Runs the blocking host git and developer-tool command probes.
/// This does not write sandbox files or grant access to host paths.
pub fn probe_host() -> HostProbe {
    let git = [
        "user.name",
        "user.email",
        "core.excludesFile",
        "gpg.format",
        "user.signingkey",
        "gpg.ssh.program",
        "gpg.ssh.defaultKeyCommand",
        "commit.gpgsign",
        "tag.gpgsign",
    ]
    .into_iter()
    .filter_map(|key| host_git_config(key, key == "core.excludesFile").map(|value| (key, value)))
    .collect();
    HostProbe {
        git,
        #[cfg(target_os = "macos")]
        developer_tools: developer_tools(),
    }
}

/// Constructs the sandbox using host environment and developer tools.
pub(crate) fn spec(
    config: &Config,
    tmp: &Path,
    spill: &Path,
    skills: &[PathBuf],
    git: &Path,
) -> Result<xi_sandbox::Spec> {
    let mut spec = xi_sandbox::Spec::new(&config.workspace, tmp);
    spec.enabled = config.sandbox.enabled;
    spec.readable.extend(
        config
            .sandbox
            .readable
            .iter()
            .map(|p| config.paths.expand(p)),
    );
    spec.writable.extend(
        config
            .sandbox
            .writable
            .iter()
            .map(|p| config.paths.expand(p)),
    );
    spec.env.extend(config.sandbox.env.iter().cloned());
    let overlay = host_env(config)?;
    let host = |name: &str| match overlay.get(name) {
        Some(value) => value.clone().map(OsString::from),
        None => std::env::var_os(name),
    };
    let ssh_agent = host("SSH_AUTH_SOCK");
    for path in [
        "~/.cargo",
        "~/.rustup",
        "~/.local/share/mise/installs",
        "~/.local/bin",
        "/opt/homebrew",
    ] {
        let path = config.paths.expand(Path::new(path));
        if path.exists() {
            spec.readable.push(path);
        }
    }
    // Toolchain homes that rustup and cargo find through the environment.
    for var in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = host(var).map(PathBuf::from)
            && path.is_absolute()
            && path.exists()
        {
            spec.readable.push(path);
            spec.env.push(var.into());
        }
    }
    // The toolchain pin (mise and rust-toolchain files set it), so cargo
    // picks the same toolchain as outside.
    spec.env.push("RUSTUP_TOOLCHAIN".into());
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut path: Vec<PathBuf> = host("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    if let Some((sdk, dirs)) = &config.host_probe().developer_tools {
        // /usr/bin's developer tools are xcrun shims. xcrun caches its
        // lookups in the per-user cache directory, which Seatbelt hides,
        // so inside every call runs xcodebuild, which prints noise. The
        // real directories, resolved here, go just ahead of /usr/bin, and
        // SDKROOT spares compilers the same lookup.
        if let Some(at) = path.iter().position(|d| d == Path::new("/usr/bin")) {
            let new: Vec<PathBuf> = dirs
                .iter()
                .filter(|d| !path[..at].contains(d))
                .cloned()
                .collect();
            path.splice(at..at, new);
        }
        if host("SDKROOT").is_some() {
            spec.env.push("SDKROOT".into());
        } else {
            spec.vars.insert("SDKROOT".into(), sdk.clone());
        }
    }
    // Every directory on PATH, so tools the user runs resolve the same
    // way inside. The home directory and its ancestors are skipped: they
    // would expose every credential store under home.
    for dir in &path {
        if dir.is_absolute() && dir.is_dir() && !config.paths.home.starts_with(dir) {
            spec.readable.push(dir.clone());
        }
    }
    if !path.is_empty() {
        let path =
            std::env::join_paths(&path).map_err(|e| Error::Config(format!("sandbox PATH: {e}")))?;
        spec.vars
            .insert("PATH".into(), path.to_string_lossy().into_owned());
    }
    // The command's values stand in for the host's, but only for names
    // that pass through anyway: anything else it prints, such as secrets
    // in a tool manager's environment, stays outside.
    for (name, value) in overlay {
        if name == "PATH" || !spec.env.contains(&name) {
            continue;
        }
        match value {
            Some(value) => {
                spec.vars.insert(name, value);
            }
            None => spec.env.retain(|n| *n != name),
        }
    }
    if let Ok(user) = std::env::var("USER") {
        let path = PathBuf::from("/etc/profiles/per-user").join(user);
        if path.exists() {
            spec.readable.push(path);
        }
    }
    spec.readable.extend(skills.iter().cloned());
    spec.readable.push(config.paths.config.join("AGENTS.md"));
    spec.readable.push(spill.to_owned());
    spec.readable.push(git.to_owned());
    spec.vars.insert(
        "GIT_CONFIG_GLOBAL".into(),
        git.join("config").to_string_lossy().into_owned(),
    );
    if config.sandbox.ssh_agent {
        allow_ssh_agent(&mut spec, ssh_agent)?;
    }
    validate_sandbox(&spec)?;
    Ok(spec)
}

/// Validates resolved roots when enabling a live worker's sandbox without
/// rerunning its host environment command.
pub(crate) fn validate_sandbox(spec: &xi_sandbox::Spec) -> Result<()> {
    spec.validate().map_err(|e| Error::Config(e.to_string()))?;
    if spec.enabled
        && let Ok(dir) = xi_protocol::runtime::dir()
    {
        let runtime = comparison_path(&dir);
        for root in spec.readable.iter().chain(&spec.writable) {
            if runtime.starts_with(comparison_path(root)) {
                return Err(Error::Config(format!(
                    "runtime directory {} is inside sandbox root {}",
                    dir.display(),
                    root.display()
                )));
            }
        }
        for socket in &spec.sockets {
            if comparison_path(socket).starts_with(&runtime) {
                return Err(Error::Config(format!(
                    "sandbox socket {} is inside runtime directory {}",
                    socket.display(),
                    dir.display()
                )));
            }
        }
    }
    Ok(())
}

/// The environment `sandbox.env_command` prints, or nothing without one.
fn host_env(config: &Config) -> Result<HashMap<String, Option<String>>> {
    let Some((program, args)) = config.sandbox.env_command.split_first() else {
        return Ok(HashMap::new());
    };
    let fail = |why: String| {
        Error::Config(format!(
            "sandbox.env_command `{}`: {why}",
            config.sandbox.env_command.join(" ")
        ))
    };
    let output = std::process::Command::new(config.paths.expand(Path::new(program)))
        .args(args)
        .current_dir(&config.workspace)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| fail(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(fail(format!("{}: {}", output.status, stderr.trim())));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| fail(format!("expected a JSON object of strings or nulls: {e}")))
}

/// Grants access to the selected socket, never its containing directory.
fn allow_ssh_agent(spec: &mut xi_sandbox::Spec, socket: Option<OsString>) -> Result<()> {
    let fail = |why| Error::Config(format!("sandbox.ssh_agent: {why}"));
    let socket = socket
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| fail("SSH_AUTH_SOCK must name an absolute socket path".into()))?;
    let socket = socket
        .canonicalize()
        .map_err(|error| fail(format!("{}: {error}", socket.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !socket.metadata()?.file_type().is_socket() {
            return Err(fail(format!("{} is not a Unix socket", socket.display())));
        }
    }
    #[cfg(not(unix))]
    return Err(fail("SSH agents require Unix sockets".into()));
    let value = socket
        .to_str()
        .ok_or_else(|| fail("SSH_AUTH_SOCK must be UTF-8".into()))?;
    spec.vars.insert("SSH_AUTH_SOCK".into(), value.into());
    spec.sockets.push(socket);
    Ok(())
}

fn comparison_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |cwd| cwd.join(path))
    };
    // A runtime directory may not exist yet beneath a symlinked ancestor.
    let resolved = absolute.ancestors().skip(1).find_map(|ancestor| {
        let root = ancestor.canonicalize().ok()?;
        let suffix = absolute.strip_prefix(ancestor).ok()?;
        Some(root.join(suffix))
    });
    let absolute = resolved.unwrap_or(absolute);
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// The SDK path and the directories xcrun's shims in /usr/bin point to, or
/// `None` without an active developer directory.
#[cfg(target_os = "macos")]
fn developer_tools() -> Option<(String, Vec<PathBuf>)> {
    let run = |program: &str, args: &[&str]| {
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        (output.status.success() && !text.is_empty()).then(|| text.to_owned())
    };
    // Checked first: without one, xcrun offers to install the tools.
    run("/usr/bin/xcode-select", &["-p"])?;
    let sdk = run("/usr/bin/xcrun", &["--show-sdk-path"])?;
    let mut dirs = Vec::new();
    for tool in ["git", "clang"] {
        if let Some(dir) = run("/usr/bin/xcrun", &["-f", tool])
            .and_then(|p| Path::new(&p).parent().map(Path::to_owned))
            && !dirs.contains(&dir)
        {
            dirs.push(dir);
        }
    }
    Some((sdk, dirs))
}

/// Writes the sandbox's global git config and a copy of its global ignores.
/// The host config stays unreadable because it can name hooks and helpers.
/// `ssh_agent` also preserves SSH signing settings and copies only the public
/// signing key. Otherwise global commit and tag signing remain disabled.
pub fn write_sandbox_git(
    dir: &Path,
    paths: &Paths,
    ssh_agent: bool,
    probe: &HostProbe,
) -> std::io::Result<()> {
    write_sandbox_git_with(dir, paths, ssh_agent, |key, _| probe.git.get(key).cloned())
}

fn write_sandbox_git_with(
    dir: &Path,
    paths: &Paths,
    ssh_agent: bool,
    get: impl Fn(&str, bool) -> Option<String>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let quote = |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
    let mut text = String::from("[user]\n");
    for key in ["name", "email"] {
        if let Some(value) = get(&format!("user.{key}"), false) {
            text.push_str(&format!("\t{key} = {}\n", quote(&value)));
        }
    }
    let excludes = get("core.excludesFile", true)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| paths.home.join(".config"), PathBuf::from)
                .join("git/ignore")
        });
    let ignore = dir.join("ignore");
    let mut patterns = std::fs::read(&excludes).unwrap_or_default();
    // Worktrees made inside the sandbox are nested checkouts; keep them out
    // of the outer repository's status and `git add -A`.
    if !patterns.is_empty() && !patterns.ends_with(b"\n") {
        patterns.push(b'\n');
    }
    patterns.extend_from_slice(format!("/{}/\n", xi_sandbox::WORKTREES).as_bytes());
    std::fs::write(&ignore, patterns)?;
    text.push_str(&format!(
        "[core]\n\texcludesFile = {}\n",
        quote(&ignore.to_string_lossy())
    ));
    if ssh_agent && get("gpg.format", false).as_deref() == Some("ssh") {
        text.push_str("[gpg]\n\tformat = ssh\n");
        if let Some(key) = get("user.signingkey", false) {
            let key = sandbox_ssh_signing_key(dir, paths, &key)?;
            text.push_str(&format!("[user]\n\tsigningkey = {}\n", quote(&key)));
        }
        for (section, key, name) in [
            ("gpg \"ssh\"", "program", "gpg.ssh.program"),
            (
                "gpg \"ssh\"",
                "defaultKeyCommand",
                "gpg.ssh.defaultKeyCommand",
            ),
            ("commit", "gpgsign", "commit.gpgsign"),
            ("tag", "gpgsign", "tag.gpgsign"),
        ] {
            if let Some(value) = get(name, false) {
                text.push_str(&format!("[{section}]\n\t{key} = {}\n", quote(&value)));
            }
        }
    } else {
        text.push_str("[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n");
    }
    std::fs::write(dir.join("config"), text)
}

/// Copies public-key text, using an adjacent `.pub` for private-key paths.
fn sandbox_ssh_signing_key(dir: &Path, paths: &Paths, key: &str) -> std::io::Result<String> {
    let public_key = |text: &str| {
        let text = text.trim();
        let mut fields = text.split_whitespace();
        let algorithm = fields.next().unwrap_or_default();
        !text.contains(['\r', '\n'])
            && (algorithm.starts_with("ssh-")
                || algorithm.starts_with("ecdsa-")
                || algorithm.starts_with("sk-"))
            && fields.next().is_some()
    };
    if public_key(key.strip_prefix("key::").unwrap_or(key)) {
        return Ok(key.into());
    }
    let mut path = paths.expand(Path::new(key)).into_os_string();
    if Path::new(&path).extension().is_none_or(|ext| ext != "pub") {
        path.push(".pub");
    }
    let path = PathBuf::from(path);
    let public = std::fs::read_to_string(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "SSH signing requires an inline public key or {}: {error}",
                path.display()
            ),
        )
    })?;
    if !public_key(&public) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a single SSH public key", path.display()),
        ));
    }
    let copy = dir.join("signing-key.pub");
    std::fs::write(&copy, format!("{}\n", public.trim()))?;
    Ok(copy.to_string_lossy().into_owned())
}

/// Reads one value from the host's global git config, outside the sandbox.
fn host_git_config(key: &str, path: bool) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command.args(["config", "--global", "--get"]);
    if path {
        command.arg("--type=path");
    }
    let out = command
        .arg(key)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!value.is_empty() && !value.contains('\n')).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC_KEY: &str = "ssh-ed25519 dGVzdA== fixture";

    fn paths(root: &Path) -> Paths {
        Paths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        }
    }

    fn git_value(dir: &Path, key: &str) -> Option<String> {
        let output = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(dir.join("config"))
            .args(["--get", key])
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    #[test]
    fn ssh_signing_settings_require_opt_in_and_do_not_copy_unrelated_helpers() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let host = |key: &str, _: bool| {
            match key {
                "user.name" => Some("Fixture"),
                "user.signingkey" => Some(PUBLIC_KEY),
                "gpg.format" => Some("ssh"),
                "gpg.ssh.defaultKeyCommand" => Some("ssh-add -L"),
                "gpg.ssh.program" => Some("ssh-keygen"),
                "commit.gpgsign" | "tag.gpgsign" => Some("true"),
                "credential.helper" => Some("secret-helper"),
                "core.hooksPath" => Some("/host/hooks"),
                _ => None,
            }
            .map(str::to_owned)
        };
        let git = root.path().join("git");
        write_sandbox_git_with(&git, &paths, false, host).unwrap();
        assert_eq!(git_value(&git, "user.name").as_deref(), Some("Fixture"));
        assert_eq!(git_value(&git, "commit.gpgsign").as_deref(), Some("false"));
        assert_eq!(git_value(&git, "user.signingkey"), None);
        assert_eq!(git_value(&git, "gpg.format"), None);
        write_sandbox_git_with(&git, &paths, true, host).unwrap();
        assert_eq!(git_value(&git, "gpg.format").as_deref(), Some("ssh"));
        assert_eq!(
            git_value(&git, "user.signingkey").as_deref(),
            Some(PUBLIC_KEY)
        );
        assert_eq!(git_value(&git, "commit.gpgsign").as_deref(), Some("true"));
        assert_eq!(git_value(&git, "tag.gpgsign").as_deref(), Some("true"));
        assert_eq!(
            git_value(&git, "gpg.ssh.defaultKeyCommand").as_deref(),
            Some("ssh-add -L")
        );
        assert_eq!(
            git_value(&git, "gpg.ssh.program").as_deref(),
            Some("ssh-keygen")
        );
        assert_eq!(git_value(&git, "credential.helper"), None);
        assert_eq!(git_value(&git, "core.hooksPath"), None);
    }

    #[test]
    fn ssh_signing_keeps_agent_key_selection_without_an_explicit_key() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join("git");
        write_sandbox_git_with(&git, &paths(root.path()), true, |key, _| {
            match key {
                "gpg.format" => Some("ssh"),
                "gpg.ssh.defaultKeyCommand" => Some("ssh-add -L"),
                "commit.gpgsign" => Some("true"),
                _ => None,
            }
            .map(str::to_owned)
        })
        .unwrap();
        assert_eq!(git_value(&git, "user.signingkey"), None);
        assert_eq!(
            git_value(&git, "gpg.ssh.defaultKeyCommand").as_deref(),
            Some("ssh-add -L")
        );
        assert_eq!(git_value(&git, "commit.gpgsign").as_deref(), Some("true"));
    }

    #[test]
    fn ssh_signing_copies_only_the_public_key_and_rejects_private_key_content() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        std::fs::create_dir_all(paths.home.join(".ssh")).unwrap();
        let private = paths.home.join(".ssh/signing");
        let public = paths.home.join(".ssh/signing.pub");
        std::fs::write(&private, "private key must stay outside").unwrap();
        std::fs::write(&public, format!("{PUBLIC_KEY}\n")).unwrap();
        let git = root.path().join("git");
        std::fs::create_dir_all(&git).unwrap();
        for key in ["~/.ssh/signing.pub", "~/.ssh/signing"] {
            let copy = sandbox_ssh_signing_key(&git, &paths, key).unwrap();
            assert_eq!(Path::new(&copy), git.join("signing-key.pub"));
            assert_eq!(
                std::fs::read_to_string(copy).unwrap(),
                format!("{PUBLIC_KEY}\n")
            );
        }
        let inline = format!("key::{PUBLIC_KEY}");
        assert_eq!(
            sandbox_ssh_signing_key(&git, &paths, &inline).unwrap(),
            inline
        );
        std::fs::write(&public, "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n").unwrap();
        assert!(sandbox_ssh_signing_key(&git, &paths, "~/.ssh/signing.pub").is_err());
        std::fs::remove_file(public).unwrap();
        assert!(sandbox_ssh_signing_key(&git, &paths, "~/.ssh/signing").is_err());
    }

    #[test]
    fn ssh_agent_rejects_missing_relative_and_non_socket_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = xi_sandbox::Spec::new(root.path(), root.path().join("tmp"));
        let writable = spec.writable.clone();
        for socket in [None, Some("relative.sock".into()), Some(root.path().into())] {
            assert!(allow_ssh_agent(&mut spec, socket).is_err());
        }
        assert!(!spec.vars.contains_key("SSH_AUTH_SOCK"));
        assert_eq!(spec.writable, writable);
    }

    #[cfg(unix)]
    #[test]
    fn ssh_agent_grants_only_the_selected_socket() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        // Session TMPDIR paths can exceed the Unix socket pathname limit.
        let executable = std::env::current_exe().unwrap();
        let root = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let socket = root.path().join("agent.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let link = root.path().join("agent-link.sock");
        symlink(&socket, &link).unwrap();
        let mut spec = xi_sandbox::Spec::new("/workspace", "/tmp/session");
        assert!(!spec.env.iter().any(|name| name == "SSH_AUTH_SOCK"));
        allow_ssh_agent(&mut spec, Some(link.into_os_string())).unwrap();
        let socket = socket.canonicalize().unwrap();
        assert_eq!(spec.sockets, std::slice::from_ref(&socket));
        assert!(!spec.writable.contains(&socket));
        assert!(!spec.writable.contains(&root.path().canonicalize().unwrap()));
        assert_eq!(spec.vars["SSH_AUTH_SOCK"], socket.to_str().unwrap());
    }
}
