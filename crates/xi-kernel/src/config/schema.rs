//! Configuration file schema and default values.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use xi_policy::RuleSpec;
use xi_protocol::model::{ModelRole, ModelSelector, Reasoning};
use xi_provider::{Auth, ModelEntry, ProviderConfig};

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

/// Which service runs `web_search` queries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    /// The search model's hosted search when its provider has one, else Exa.
    #[default]
    Auto,
    /// The search model's hosted search.
    Model,
    /// The Exa search API.
    Exa,
}

/// `[web_search]`: how the opt-in `web_search` tool searches.
#[derive(Clone, Debug, PartialEq)]
pub struct WebSearchConfig {
    /// The service that runs searches.
    pub backend: SearchBackend,
    /// The model whose provider runs hosted searches. Default `@tiny`.
    pub model: ModelSelector,
    /// The Exa API key. Default `{ env = "EXA_API_KEY" }`.
    pub exa: Auth,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            backend: SearchBackend::Auto,
            model: ModelRole::Tiny.into(),
            exa: Auth::Env("EXA_API_KEY".into()),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WebSearchLayer {
    pub(super) backend: Option<SearchBackend>,
    pub(super) model: Option<String>,
    pub(super) exa: Option<Auth>,
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
pub(super) struct FileConfig {
    pub(super) default_model: Option<String>,
    pub(super) preset: Option<String>,
    pub(super) roles: Option<RolesLayer>,
    pub(super) reasoning: Option<Reasoning>,
    pub(super) approvals: Option<Approvals>,
    pub(super) tools: Option<Vec<String>>,
    pub(super) extensions: Option<Vec<PathBuf>>,
    pub(super) sandbox: Option<SandboxLayer>,
    pub(super) compaction: Option<crate::compaction::Layer>,
    pub(super) title: Option<TitleConfig>,
    pub(super) web_search: Option<WebSearchLayer>,
    #[serde(default)]
    pub(super) provider: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub(super) model: Vec<ModelEntry>,
    #[serde(default)]
    pub(super) rule: Vec<RuleSpec>,
}

/// `[roles]`: one `PROVIDER/MODEL` per role. `default_model` sets `default`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RolesLayer {
    pub(super) slow: Option<String>,
    pub(super) smol: Option<String>,
    pub(super) tiny: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SandboxLayer {
    pub(super) enabled: Option<bool>,
    pub(super) ssh_agent: Option<bool>,
    pub(super) readable: Option<Vec<PathBuf>>,
    pub(super) writable: Option<Vec<PathBuf>>,
    pub(super) env: Option<Vec<String>>,
    pub(super) env_command: Option<Vec<String>>,
}
