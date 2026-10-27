//! Configuration layers and workspace trust.

pub(crate) mod approvals;
mod schema;

use crate::{Error, Result};
pub(crate) use approvals::append_workspace_allow;
use approvals::approval_rules;
use schema::FileConfig;
pub use schema::{
    Approvals, DaemonSettings, Limit, SandboxConfig, SearchBackend, TitleConfig, WebSearchConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use xi_policy::{ActionKind, Authority, Condition, Policy, RuleSpec};
use xi_protocol::model::{ModelRole, ModelSelector, Reasoning};
use xi_protocol::{Decision, event::ModelRef, methods::client::SettingLayer};
use xi_provider::{Auth, Registry, RegistryConfig};

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
    /// How `web_search` searches.
    pub web_search: WebSearchConfig,
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
    pub(crate) host_probe: Arc<LazyLock<crate::sandbox::HostProbe>>,
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
            web_search: WebSearchConfig::default(),
            daemon: DaemonSettings::default(),
            extensions: Vec::new(),
            registries: Vec::new(),
            policy: Arc::new(Policy::new()),
            warnings: Vec::new(),
            setting_sources: BTreeMap::new(),
            preset_sources: BTreeSet::new(),
            held_back: BTreeMap::new(),
            host_probe: Arc::new(LazyLock::new(crate::sandbox::probe_host)),
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
            if let Some(search) = layer.web_search {
                if let Some(backend) = search.backend {
                    config.web_search.backend = backend;
                }
                if let Some(model) = search.model {
                    config.web_search.model = model.parse().map_err(|error| {
                        Error::Config(format!("{}: web_search.model: {error}", path.display()))
                    })?;
                }
                if let Some(exa) = search.exa {
                    if !matches!(exa, Auth::Env(_) | Auth::File(_)) {
                        return Err(Error::Config(format!(
                            "{}: web_search.exa must be {{ env = NAME }} or {{ file = PATH }}",
                            path.display()
                        )));
                    }
                    config.web_search.exa = exa;
                }
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

    /// Returns the cached host git and developer-tool settings.
    /// The first call performs blocking host command probes.
    pub fn host_probe(&self) -> &crate::sandbox::HostProbe {
        &self.host_probe
    }

    /// Constructs the per-session sandbox and validates its paths.
    /// `git` holds the materialized sandbox global git config.
    pub fn sandbox_spec(
        &self,
        tmp: &Path,
        spill: &Path,
        skills: &[PathBuf],
        git: &Path,
    ) -> Result<xi_sandbox::Spec> {
        crate::sandbox::spec(self, tmp, spill, skills, git)
    }
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

/// Parses a provider/model selector, retaining slashes inside the model id.
pub fn parse_model(value: &str) -> Result<ModelRef> {
    value
        .parse()
        .map_err(|_| Error::Config(format!("invalid model {value:?}; expected PROVIDER/MODEL")))
}
