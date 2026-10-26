//! Configuration layers, workspace trust and sandbox construction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use xi_policy::{ActionKind, Authority, Condition, Policy, RuleSpec};
use xi_protocol::Decision;
use xi_protocol::event::ModelRef;
use xi_provider::{ModelInfo, ProviderConfig, Reasoning, Registry, RegistryConfig};

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

/// Extra sandbox roots and environment variable names.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Readable roots.
    #[serde(default)]
    pub readable: Vec<PathBuf>,
    /// Writable roots.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Environment variable names passed through to tools.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    default_model: Option<String>,
    reasoning: Option<Reasoning>,
    tools: Option<Vec<String>>,
    sandbox: Option<SandboxLayer>,
    #[serde(default)]
    provider: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    model: Vec<ModelInfo>,
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxLayer {
    readable: Option<Vec<PathBuf>>,
    writable: Option<Vec<PathBuf>>,
    env: Option<Vec<String>>,
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
    /// Reasoning setting.
    pub reasoning: Reasoning,
    /// Enabled tool names in request order.
    pub tools: Vec<String>,
    /// Sandbox additions.
    pub sandbox: SandboxConfig,
    /// Ordered registry layers, resolved lazily at the first model request.
    pub registries: Vec<RegistryConfig>,
    /// Compiled policy.
    pub policy: Arc<Policy>,
    /// Ignored untrusted settings, also printed to stderr during loading.
    pub warnings: Vec<String>,
}

impl Config {
    /// Loads global, explicit, project and local configuration in that order.
    pub fn load(paths: Paths, workspace: &Path, extra: &[PathBuf]) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        if !workspace.is_dir() {
            return Err(Error::Config("workspace is not a directory".into()));
        }
        let trusted = paths.trust_store()?.roots.contains(&workspace);
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
        let mut config = Self {
            workspace,
            paths,
            trusted,
            model: parse_model("anthropic/claude-sonnet-5-5")?,
            reasoning: Reasoning::Medium,
            tools: ["read", "edit", "write", "bash", "search"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            sandbox: SandboxConfig::default(),
            registries: Vec::new(),
            policy: Arc::new(Policy::new()),
            warnings: Vec::new(),
        };
        let mut layers = vec![(
            config.paths.config.join("config.toml"),
            Authority::Full,
            true,
            false,
        )];
        layers.extend(
            extra
                .iter()
                .map(|p| (config.paths.expand(p), Authority::Full, true, true)),
        );
        layers.push((
            config.workspace.join(".xi/config.toml"),
            Authority::TightenOnly,
            trusted,
            false,
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
        ));
        for (path, authority, settings, required) in layers {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
            };
            let mut value: toml::Value = toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
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
            let layer: FileConfig = value
                .try_into()
                .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
            policy.add_layer(path.display().to_string(), authority, &layer.rule)?;
            if let Some(model) = layer.default_model {
                config.model = parse_model(&model)?;
            }
            if let Some(reasoning) = layer.reasoning {
                config.reasoning = reasoning;
            }
            if let Some(tools) = layer.tools {
                config.tools = tools;
            }
            if let Some(sandbox) = layer.sandbox {
                if let Some(paths) = sandbox.readable {
                    config.sandbox.readable = paths;
                }
                if let Some(paths) = sandbox.writable {
                    config.sandbox.writable = paths;
                }
                if let Some(env) = sandbox.env {
                    config.sandbox.env = env;
                }
            }
            config.registries.push(RegistryConfig {
                provider: layer.provider,
                model: layer.model,
            });
        }
        config.policy = Arc::new(policy);
        Ok(config)
    }

    /// Builds the registry after all authoritative config layers are known.
    pub fn registry(&self) -> Registry {
        let mut registry = Registry::builtin();
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
        self.paths.data.join("sessions").join(slug)
    }

    /// Constructs the per-session sandbox and validates its paths.
    pub fn sandbox_spec(
        &self,
        tmp: &Path,
        spill: &Path,
        skills: &[PathBuf],
    ) -> Result<xi_sandbox::Spec> {
        let mut spec = xi_sandbox::Spec::new(&self.workspace, tmp);
        spec.readable
            .extend(self.sandbox.readable.iter().map(|p| self.paths.expand(p)));
        spec.writable
            .extend(self.sandbox.writable.iter().map(|p| self.paths.expand(p)));
        spec.env.extend(self.sandbox.env.iter().cloned());
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
        if let Ok(user) = std::env::var("USER") {
            let path = PathBuf::from("/etc/profiles/per-user").join(user);
            if path.exists() {
                spec.readable.push(path);
            }
        }
        spec.readable.extend(skills.iter().cloned());
        spec.readable.push(spill.to_owned());
        spec.validate().map_err(|e| Error::Config(e.to_string()))?;
        Ok(spec)
    }
}

/// Parses a provider/model selector, retaining slashes inside the model id.
pub fn parse_model(value: &str) -> Result<ModelRef> {
    match value.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => Ok(ModelRef {
            provider: provider.into(),
            model: model.into(),
        }),
        _ => Err(Error::Config(format!(
            "invalid model {value:?}; expected PROVIDER/MODEL"
        ))),
    }
}
