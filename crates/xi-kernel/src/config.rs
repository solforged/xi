//! Configuration layers, workspace trust and sandbox construction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use xi_policy::{ActionKind, Authority, Condition, Policy, RuleSpec};
use xi_protocol::Decision;
use xi_protocol::event::ModelRef;
use xi_protocol::methods::client::SettingLayer;
use xi_protocol::model::{ModelRole, ModelSelector, Reasoning};
use xi_provider::{ModelEntry, ProviderConfig, Registry, RegistryConfig};

use crate::{Error, Result};

/// User-owned directories, injectable without changing process environment.
#[derive(Clone, Debug)]
pub struct Paths {
    /// User home.
    pub home: PathBuf,
    /// Xi configuration directory, including `xi`.
    pub config: PathBuf,
    /// Xi persistent data directory, including `xi`.
    pub data: PathBuf,
    /// Xi state directory, including `xi`.
    pub state: PathBuf,
}

impl Paths {
    /// Resolves XDG directories with their home-directory defaults.
    pub fn discover() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        let xdg = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
                .join("xi")
        };
        Ok(Self {
            config: xdg("XDG_CONFIG_HOME", ".config"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
            home,
        })
    }

    /// Expands a leading `~` or `~/` only, without shell expansion.
    pub fn expand(&self, path: &Path) -> PathBuf {
        if path == Path::new("~") {
            return self.home.clone();
        }
        path.strip_prefix("~/")
            .map_or_else(|_| path.to_owned(), |rest| self.home.join(rest))
    }

    /// Credentials xi manages itself, such as `xi login chatgpt` tokens.
    /// No sandbox root covers this directory.
    pub fn credentials(&self) -> PathBuf {
        self.data.join("auth")
    }

    /// Records an exact canonical workspace root in the user trust store.
    pub fn trust(&self, workspace: &Path) -> Result<PathBuf> {
        let root = workspace.canonicalize()?;
        if !root.is_dir() {
            return Err(Error::Config("workspace is not a directory".into()));
        }
        let path = self.config.join("trust.toml");
        let mut store = self.trust_store()?;
        if !store.roots.contains(&root) {
            store.roots.push(root.clone());
        }
        std::fs::create_dir_all(&self.config)?;
        std::fs::write(
            path,
            toml::to_string(&store).map_err(|e| Error::Config(e.to_string()))?,
        )?;
        Ok(root)
    }

    fn trust_store(&self) -> Result<TrustStore> {
        let path = self.config.join("trust.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustStore {
    #[serde(default)]
    roots: Vec<PathBuf>,
}

/// `[daemon]` settings, honoured only from the global user config file.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonSettings {
    /// Seconds without clients or work before the daemon exits.
    pub idle_exit: u64,
    /// Notification command argv, receiving a pending approval as JSON on stdin.
    pub notify: Option<Vec<String>>,
    /// Seconds before an unanswered client approval is denied.
    pub approval_timeout: Option<u64>,
    /// Per-endpoint model request concurrency limits.
    pub limit: Vec<Limit>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            idle_exit: 600,
            notify: None,
            approval_timeout: None,
            limit: Vec::new(),
        }
    }
}

/// A positive model request concurrency limit for one endpoint.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    /// Provider base URL to limit.
    pub endpoint: String,
    /// Maximum simultaneous requests, at least one.
    pub concurrency: usize,
}

/// Reads only the global config file's `[daemon]` table, defaulting when absent.
pub fn daemon_settings(paths: &Paths) -> Result<DaemonSettings> {
    let path = paths.config.join("config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DaemonSettings::default());
        }
        Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
    };
    let mut table: toml::Table =
        toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    parse_daemon(table.remove("daemon"), &path)
}

fn parse_daemon(value: Option<toml::Value>, path: &Path) -> Result<DaemonSettings> {
    let settings: DaemonSettings = match value {
        Some(value) => value
            .try_into()
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?,
        None => DaemonSettings::default(),
    };
    if let Some(limit) = settings.limit.iter().find(|limit| limit.concurrency == 0) {
        return Err(Error::Config(format!(
            "{}: daemon.limit concurrency must be at least 1 for endpoint {}",
            path.display(),
            limit.endpoint
        )));
    }
    Ok(settings)
}

/// Root holding the per-workspace session directories.
pub fn sessions_root(paths: &Paths) -> PathBuf {
    paths.data.join("sessions")
}

/// Tool sandbox settings, extra roots and environment variable names.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Whether tool processes run in the OS sandbox. Enabled by default.
    pub enabled: bool,
    /// Exposes the host SSH agent and SSH git signing settings. Off by default.
    pub ssh_agent: bool,
    /// Readable roots.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
    /// Writable roots.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Environment variable names passed through to tools.
    #[serde(default)]
    pub env: Vec<String>,
    /// A host command, run once per session in the workspace, that prints
    /// the environment tools should see as a JSON object of names to
    /// strings, or to `null` to unset, such as `mise env --json` or
    /// `direnv export json`. Its values replace the host's only for
    /// variables already passed through.
    #[serde(default)]
    pub env_command: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ssh_agent: false,
            readable: Vec::new(),
            writable: Vec::new(),
            env: Vec::new(),
            env_command: Vec::new(),
        }
    }
}

/// `[title]`: automatic session naming.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TitleConfig {
    /// Name sessions with the `tiny` model. On by default; it applies only
    /// to sessions whose client asks for titles.
    pub auto: bool,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self { auto: true }
    }
}

/// Built-in approval policy applied before user rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approvals {
    /// Ask for every boundary crossing without an explicit rule.
    #[default]
    Ask,
    /// Allow reads except in xi's own directories.
    Reads,
    /// Allow ordinary reads, writes and clean network egress.
    Auto,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    default_model: Option<String>,
    preset: Option<String>,
    roles: Option<RolesLayer>,
    reasoning: Option<Reasoning>,
    approvals: Option<Approvals>,
    tools: Option<Vec<String>>,
    extensions: Option<Vec<PathBuf>>,
    sandbox: Option<SandboxLayer>,
    compaction: Option<crate::compaction::Layer>,
    title: Option<TitleConfig>,
    #[serde(default)]
    provider: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    model: Vec<ModelEntry>,
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

/// `[roles]`: one `PROVIDER/MODEL` per role. `default_model` sets `default`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesLayer {
    slow: Option<String>,
    smol: Option<String>,
    tiny: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxLayer {
    enabled: Option<bool>,
    ssh_agent: Option<bool>,
    readable: Option<Vec<PathBuf>>,
    writable: Option<Vec<PathBuf>>,
    env: Option<Vec<String>>,
    env_command: Option<Vec<String>>,
}

/// Fully layered settings for one canonical workspace.
#[derive(Clone, Debug)]
pub struct Config {
    /// Canonical workspace root.
    pub workspace: PathBuf,
    /// User-owned locations.
    pub paths: Paths,
    /// Whether this exact workspace root is trusted.
    pub trusted: bool,
    /// Selected provider and model.
    pub model: ModelRef,
    /// Models assigned to roles by `preset`, `[roles]` and `default_model`.
    /// `default` is always present; see [`Config::role`] for the rest.
    pub roles: BTreeMap<ModelRole, ModelRef>,
    /// The last preset applied, which later layers may have partly overridden.
    pub preset: Option<String>,
    /// Reasoning setting.
    pub reasoning: Reasoning,
    /// Explicit per-invocation override, which takes precedence over a saved level.
    pub reasoning_override: Option<Reasoning>,
    /// Built-in approval policy preset.
    pub approvals: Approvals,
    /// Enabled tool names in request order.
    pub tools: Vec<String>,
    /// Extension package directories, resolved against their config files.
    pub extensions: Vec<PathBuf>,
    /// Sandbox additions.
    pub sandbox: SandboxConfig,
    /// Automatic context compaction and retained history budget.
    pub compaction: crate::compaction::Config,
    /// Automatic session naming.
    pub title: TitleConfig,
    /// Daemon settings from the global user config file only.
    pub daemon: DaemonSettings,
    /// Ordered registry layers, resolved lazily at the first model request.
    pub registries: Vec<RegistryConfig>,
    /// Compiled policy.
    pub policy: Arc<Policy>,
    /// Ignored settings, also printed to stderr during loading.
    pub warnings: Vec<String>,
    pub(crate) setting_sources: BTreeMap<&'static str, SettingLayer>,
    /// Keys in `setting_sources` whose layer's `preset` supplied the value.
    pub(crate) preset_sources: BTreeSet<&'static str>,
    pub(crate) held_back: BTreeMap<&'static str, SettingLayer>,
}

impl Config {
    /// Loads global, explicit, project and local configuration in that order.
    pub fn load(paths: Paths, workspace: &Path, extra: &[PathBuf]) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(Error::Config("workspace is not a directory".into()));
        }
        let trusted = paths.trust_store()?.roots.contains(&workspace);
        let mut rule_layers = Vec::new();
        let default = parse_model("chatgpt/gpt-6.1-sol")?;
        let mut config = Self {
            workspace,
            paths,
            trusted,
            roles: BTreeMap::from([(ModelRole::Default, default.clone())]),
            preset: None,
            model: default,
            reasoning: Reasoning::Medium,
            reasoning_override: None,
            approvals: Approvals::default(),
            tools: ["read", "edit", "write", "bash", "search"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            sandbox: SandboxConfig::default(),
            compaction: crate::compaction::Config::default(),
            title: TitleConfig::default(),
            daemon: DaemonSettings::default(),
            extensions: Vec::new(),
            registries: Vec::new(),
            policy: Arc::new(Policy::new()),
            warnings: Vec::new(),
            setting_sources: BTreeMap::new(),
            preset_sources: BTreeSet::new(),
            held_back: BTreeMap::new(),
        };
        let mut layers = vec![(
            config.paths.config.join("config.toml"),
            Authority::Full,
            true,
            false,
            SettingLayer::Global,
        )];
        layers.extend(extra.iter().map(|p| {
            let path = config.paths.expand(p);
            (
                path.clone(),
                Authority::Full,
                true,
                true,
                SettingLayer::Explicit { path },
            )
        }));
        layers.push((
            config.workspace.join(".xi/config.toml"),
            Authority::TightenOnly,
            trusted,
            false,
            SettingLayer::Project,
        ));
        layers.push((
            config.workspace.join(".xi/config.local.toml"),
            if trusted {
                Authority::Full
            } else {
                Authority::TightenOnly
            },
            trusted,
            false,
            SettingLayer::Local,
        ));
        for (path, authority, settings, required, source) in layers {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
            };
            let mut value: toml::Value = toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
            let table = value
                .as_table_mut()
                .ok_or_else(|| Error::Config("config must be a table".into()))?;
            if source == SettingLayer::Project && table.remove("approvals").is_some() {
                let warning = format!(
                    "{}: ignoring approvals outside user and local config",
                    path.display()
                );
                eprintln!("xi: {warning}");
                config.warnings.push(warning);
            }
            for key in ["enabled", "ssh_agent"] {
                if source == SettingLayer::Project
                    && table
                        .get_mut("sandbox")
                        .and_then(toml::Value::as_table_mut)
                        .and_then(|sandbox| sandbox.remove(key))
                        .is_some()
                {
                    let warning = format!(
                        "{}: ignoring sandbox.{key} outside user and local config",
                        path.display()
                    );
                    eprintln!("xi: {warning}");
                    config.warnings.push(warning);
                }
            }
            if !settings {
                record_sources(table, &source, &mut config.held_back, None);
            } else {
                record_sources(
                    table,
                    &source,
                    &mut config.setting_sources,
                    Some(&mut config.preset_sources),
                );
            }
            if !settings {
                let table = value
                    .as_table_mut()
                    .ok_or_else(|| Error::Config("config must be a table".into()))?;
                if table.keys().any(|key| key != "rule") {
                    let warning = format!(
                        "{}: ignoring non-rule settings in untrusted workspace",
                        path.display()
                    );
                    eprintln!("xi: {warning}");
                    config.warnings.push(warning);
                }
                table.retain(|key, _| key == "rule");
            }
            let table = value
                .as_table_mut()
                .ok_or_else(|| Error::Config("config must be a table".into()))?;
            let daemon = table.remove("daemon");
            if source == SettingLayer::Global {
                config.daemon = parse_daemon(daemon, &path)?;
            } else if daemon.is_some() {
                let warning = format!(
                    "{}: ignoring daemon settings outside global user config",
                    path.display()
                );
                eprintln!("xi: {warning}");
                config.warnings.push(warning);
            }
            let layer = parse_file(value, &path)?;
            rule_layers.push((path.display().to_string(), authority, layer.rule));
            // A preset replaces every role; `[roles]` and `default_model`
            // in the same or a later layer then override single roles.
            if let Some(name) = layer.preset {
                let registry = Registry::builtin();
                let preset = registry.preset(&name).ok_or_else(|| {
                    let known: Vec<_> = registry.presets().collect();
                    Error::Config(format!(
                        "{}: unknown preset {name:?}; expected one of {}",
                        path.display(),
                        known.join(", ")
                    ))
                })?;
                for role in ModelRole::ALL {
                    config.roles.insert(role, parse_model(preset.get(role))?);
                }
                config.preset = Some(name);
            }
            if let Some(roles) = layer.roles {
                for (role, model) in [
                    (ModelRole::Slow, roles.slow),
                    (ModelRole::Smol, roles.smol),
                    (ModelRole::Tiny, roles.tiny),
                ] {
                    if let Some(model) = model {
                        let model = parse_model(&model).map_err(|error| {
                            Error::Config(format!("{}: roles.{role}: {error}", path.display()))
                        })?;
                        config.roles.insert(role, model);
                    }
                }
            }
            if let Some(model) = layer.default_model {
                config
                    .roles
                    .insert(ModelRole::Default, parse_model(&model)?);
            }
            if let Some(reasoning) = layer.reasoning {
                config.reasoning = reasoning;
            }
            if let Some(approvals) = layer.approvals {
                config.approvals = approvals;
            }
            if let Some(tools) = layer.tools {
                config.tools = tools;
            }
            if let Some(compaction) = layer.compaction {
                compaction.apply(&mut config.compaction);
            }
            if let Some(title) = layer.title {
                config.title = title;
            }
            if let Some(extensions) = layer.extensions {
                let base = path.parent().expect("config file has a parent");
                config.extensions = extensions
                    .iter()
                    .map(|entry| base.join(config.paths.expand(entry)))
                    .collect();
            }
            if let Some(sandbox) = layer.sandbox {
                if let Some(enabled) = sandbox.enabled {
                    config.sandbox.enabled = enabled;
                }
                if let Some(ssh_agent) = sandbox.ssh_agent {
                    config.sandbox.ssh_agent = ssh_agent;
                }
                if let Some(paths) = sandbox.readable {
                    config.sandbox.readable = paths;
                }
                if let Some(paths) = sandbox.writable {
                    config.sandbox.writable = paths;
                }
                if let Some(env) = sandbox.env {
                    config.sandbox.env = env;
                }
                if let Some(command) = sandbox.env_command {
                    config.sandbox.env_command = command;
                }
            }
            config.registries.push(RegistryConfig {
                provider: layer.provider,
                model: layer.model,
                ..Default::default()
            });
        }
        let mut policy = Policy::new();
        policy.add_layer(
            "builtin",
            Authority::Full,
            &[
                RuleSpec {
                    effect: Decision::Allow,
                    action: ActionKind::Tool,
                    pattern: "*".into(),
                    when: Condition::Always,
                },
                RuleSpec {
                    effect: Decision::Allow,
                    action: ActionKind::Exec,
                    pattern: "**".into(),
                    when: Condition::Always,
                },
            ],
        )?;
        policy.add_layer("approvals", Authority::Full, &approval_rules(&config))?;
        for (name, authority, rules) in rule_layers {
            policy.add_layer(name, authority, &rules)?;
        }
        config.policy = Arc::new(policy);
        config.model = config.role(ModelRole::Default).clone();
        Ok(config)
    }

    /// The model for `role`, or for the nearest role up its fallback chain
    /// when none is assigned: `tiny` falls back to `smol`, and `smol` and
    /// `slow` to `default`.
    pub fn role(&self, role: ModelRole) -> &ModelRef {
        let mut role = role;
        loop {
            if let Some(model) = self.roles.get(&role) {
                return model;
            }
            role = role
                .fallback()
                .expect("the default role is always assigned");
        }
    }

    /// Every role's model after fallbacks.
    pub fn resolved_roles(&self) -> BTreeMap<ModelRole, ModelRef> {
        ModelRole::ALL
            .into_iter()
            .map(|role| (role, self.role(role).clone()))
            .collect()
    }

    /// The concrete model a selector names.
    pub fn resolve(&self, selector: &ModelSelector) -> ModelRef {
        match selector {
            ModelSelector::Model(model) => model.clone(),
            ModelSelector::Role { role } => self.role(*role).clone(),
        }
    }

    /// Builds the registry after all authoritative config layers are known.
    pub fn registry(&self) -> Registry {
        let mut registry = Registry::builtin();
        registry.set_credentials(self.paths.credentials());
        for layer in &self.registries {
            registry.extend(layer.clone());
        }
        registry
    }

    /// The default session directory, keyed by the entire canonical path.
    pub fn session_dir(&self) -> PathBuf {
        let slug = self
            .workspace
            .to_string_lossy()
            .replace(['/', '\\', ':'], "_");
        sessions_root(&self.paths).join(slug)
    }

    /// Constructs the per-session sandbox and validates its paths. `git` is
    /// the directory from [`write_sandbox_git`]; its `config` becomes the
    /// sandbox's global git config. Runs `sandbox.env_command` on the host
    /// when one is set.
    pub fn sandbox_spec(
        &self,
        tmp: &Path,
        spill: &Path,
        skills: &[PathBuf],
        git: &Path,
    ) -> Result<xi_sandbox::Spec> {
        let mut spec = xi_sandbox::Spec::new(&self.workspace, tmp);
        spec.enabled = self.sandbox.enabled;
        spec.readable
            .extend(self.sandbox.readable.iter().map(|p| self.paths.expand(p)));
        spec.writable
            .extend(self.sandbox.writable.iter().map(|p| self.paths.expand(p)));
        spec.env.extend(self.sandbox.env.iter().cloned());
        let overlay = self.host_env()?;
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
            let path = self.paths.expand(Path::new(path));
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
        if let Some((sdk, dirs)) = developer_tools() {
            // /usr/bin's developer tools are xcrun shims. xcrun caches its
            // lookups in the per-user cache directory, which Seatbelt hides,
            // so inside every call runs xcodebuild, which prints noise. The
            // real directories, resolved here, go just ahead of /usr/bin, and
            // SDKROOT spares compilers the same lookup.
            if let Some(at) = path.iter().position(|d| d == Path::new("/usr/bin")) {
                let new: Vec<PathBuf> = dirs
                    .into_iter()
                    .filter(|d| !path[..at].contains(d))
                    .collect();
                path.splice(at..at, new);
            }
            if host("SDKROOT").is_some() {
                spec.env.push("SDKROOT".into());
            } else {
                spec.vars.insert("SDKROOT".into(), sdk);
            }
        }
        // Every directory on PATH, so tools the user runs resolve the same
        // way inside. The home directory and its ancestors are skipped: they
        // would expose every credential store under home.
        for dir in &path {
            if dir.is_absolute() && dir.is_dir() && !self.paths.home.starts_with(dir) {
                spec.readable.push(dir.clone());
            }
        }
        if !path.is_empty() {
            let path = std::env::join_paths(&path)
                .map_err(|e| Error::Config(format!("sandbox PATH: {e}")))?;
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
        spec.readable.push(self.paths.config.join("AGENTS.md"));
        spec.readable.push(spill.to_owned());
        spec.readable.push(git.to_owned());
        spec.vars.insert(
            "GIT_CONFIG_GLOBAL".into(),
            git.join("config").to_string_lossy().into_owned(),
        );
        if self.sandbox.ssh_agent {
            allow_ssh_agent(&mut spec, ssh_agent)?;
        }
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
        Ok(spec)
    }

    /// The environment `sandbox.env_command` prints, or nothing without one.
    fn host_env(&self) -> Result<HashMap<String, Option<String>>> {
        let Some((program, args)) = self.sandbox.env_command.split_first() else {
            return Ok(HashMap::new());
        };
        let fail = |why: String| {
            Error::Config(format!(
                "sandbox.env_command `{}`: {why}",
                self.sandbox.env_command.join(" ")
            ))
        };
        let output = std::process::Command::new(self.paths.expand(Path::new(program)))
            .args(args)
            .current_dir(&self.workspace)
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

/// Records `source` for each key the layer's `table` sets. `preset`, when
/// given, tracks which of those keys only the layer's `preset` supplied.
fn record_sources(
    table: &toml::Table,
    source: &SettingLayer,
    sources: &mut BTreeMap<&'static str, SettingLayer>,
    mut preset: Option<&mut BTreeSet<&'static str>>,
) {
    for key in crate::settings::KEYS {
        if key == "daemon.idle_exit" && *source != SettingLayer::Global {
            continue;
        }
        // A preset assigns every role, so it is the source of each one
        // until a later `[roles]` entry or `default_model` replaces it.
        let by_preset =
            (key == "default_model" || key.starts_with("roles.")) && table.contains_key("preset");
        let explicit = match key.split_once('.') {
            Some((parent, field)) => table
                .get(parent)
                .and_then(toml::Value::as_table)
                .is_some_and(|table| table.contains_key(field)),
            None => table.contains_key(key),
        };
        if by_preset || explicit {
            sources.insert(key, source.clone());
            if let Some(preset) = preset.as_deref_mut() {
                if explicit {
                    preset.remove(key);
                } else {
                    preset.insert(key);
                }
            }
        }
    }
}

fn parse_file(value: toml::Value, path: &Path) -> Result<FileConfig> {
    value
        .try_into()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

pub(crate) fn validate_global(text: &str, path: &Path) -> Result<()> {
    let mut value: toml::Value =
        toml::from_str(text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| Error::Config("config must be a table".into()))?;
    parse_daemon(table.remove("daemon"), path)?;
    parse_file(value, path)?;
    Ok(())
}

fn approval_rules(config: &Config) -> Vec<RuleSpec> {
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
pub fn write_sandbox_git(dir: &Path, paths: &Paths, ssh_agent: bool) -> std::io::Result<()> {
    write_sandbox_git_with(dir, paths, ssh_agent, host_git_config)
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

/// Parses a provider/model selector, retaining slashes inside the model id.
pub fn parse_model(value: &str) -> Result<ModelRef> {
    value
        .parse()
        .map_err(|_| Error::Config(format!("invalid model {value:?}; expected PROVIDER/MODEL")))
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
