//! Model providers.
//!
//! A [`Provider`] turns a provider-neutral [`Request`] into one streamed
//! model response. Anthropic thinking signatures, redacted thinking and
//! provider-specific blocks round-trip unchanged. OpenAI Responses reasoning
//! round-trips in encrypted form; Chat Completions reasoning is exposed as
//! [`Block::Thinking`] but is not replayed.
//!
//! `auth = { login = "chatgpt" }` or `{ login = "anthropic" }` signs
//! requests with stored subscription tokens instead of an API key.
//! [`Login`] and [`AnthropicLogin`] obtain them; requests refresh them.
//! Providers that speak the same [`Api`] differently say so in [`Compat`].
//!
//! The [`Registry`] knows which providers and models exist. It starts from a
//! built-in table and is extended from config ([`RegistryConfig`]). The
//! built-in table also names [`Preset`]s, one model per role for each lab.
//!
//! Clients share a connection pool per provider, use a 30-second connection
//! timeout and a 120-second read/idle timeout, and have no total timeout.
//! Anthropic caches the system, final tool definition and final history block.
//! Current Claude models use adaptive thinking and effort; older models use
//! token budgets. A model's `reasoning` list names the levels it accepts;
//! requests at any other level fail before they are sent.
//! Built-in prices are base, standard-tier USD per million tokens, not
//! long-context premiums. Config layers can replace all built-in metadata.
//!
//! The `live` example accepts `XI_BASE_URL` and `XI_MODEL` and exercises an
//! OpenAI-compatible stream plus a tool-result round trip without credentials.

mod anthropic;
mod anthropic_login;
mod chatgpt;
mod claude_code;
mod oauth;
mod openai;
mod registry;
mod responses;
mod sse;
mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use anthropic_login::{Login as AnthropicLogin, anthropic_file};
pub use chatgpt::{Account, Login, chatgpt_file};
pub use futures::future::BoxFuture;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use xi_protocol::event::{ModelRef, StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::Delta;
use xi_protocol::model::{ModelInfo, ModelRole, Reasoning};
use xi_protocol::{Block, ToolCallId, ToolOutput};

/// One message of history in provider-neutral form.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// Input from the user.
    User(Vec<Block>),
    /// A previous model response, echoed back exactly as received.
    Assistant(Vec<Block>),
    /// Results for the tool calls in the preceding assistant message.
    ToolResults(Vec<ToolResult>),
}

/// The output of one tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// The call this answers.
    pub call: ToolCallId,
    /// What the tool returned.
    pub output: ToolOutput,
}

/// One model request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The provider's model id.
    pub model: String,
    /// The system prompt. Kept identical across a session's requests so the
    /// prompt cache prefix stays stable.
    pub system: String,
    /// History, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolSpec>,
    /// Output token limit.
    pub max_tokens: u32,
    /// Reasoning depth.
    pub reasoning: Reasoning,
    /// Stable per-session key for provider-side cache affinity, where the
    /// API accepts one.
    pub cache_key: Option<String>,
}

/// A complete model response.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    /// Response blocks in order, including tool calls.
    pub content: Vec<Block>,
    /// Why the model stopped.
    pub stop: StopReason,
    /// Token accounting.
    pub usage: Usage,
}

/// Why a request failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The connection failed or dropped mid-stream.
    Network,
    /// The provider rate-limited the request.
    RateLimited,
    /// The provider is overloaded or returned a 5xx status.
    Unavailable,
    /// Credentials are missing or were rejected.
    Auth,
    /// The prompt is larger than the model's context window.
    ContextOverflow,
    /// The provider rejected the request as malformed.
    InvalidRequest,
    /// The response could not be decoded.
    Decode,
    /// The provider or model is not configured.
    Config,
}

/// A failed model request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    /// The failure class.
    pub kind: ErrorKind,
    /// Description shown to the user.
    pub message: String,
    /// Delay the provider asked for before retrying.
    pub retry_after: Option<Duration>,
}

impl Error {
    /// Creates an error without a retry hint.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Whether retrying the same request can succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Network | ErrorKind::RateLimited | ErrorKind::Unavailable
        )
    }
}

/// A model API.
pub trait Provider: Send + Sync {
    /// Sends `request` and streams the response. `on_delta` receives text,
    /// thinking and tool-input increments as they arrive; the returned
    /// [`Response`] holds the complete message. Dropping the future cancels
    /// the request.
    fn stream<'a>(
        &'a self,
        request: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>>;
}

/// Wire format a provider speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// OpenAI-compatible Chat Completions (OpenAI, llama.cpp, vLLM, ...).
    OpenaiChat,
    /// OpenAI Responses API, including the ChatGPT Codex backend.
    OpenaiResponses,
}

/// Where a provider's credential comes from. Credentials stay in the
/// kernel and never reach tool or extension environments.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// No credential.
    #[default]
    None,
    /// An environment variable of the kernel process.
    Env(String),
    /// A file readable only by the user (mode `0600` or stricter).
    File(PathBuf),
    /// Tokens from `xi login <service>`, kept in the registry's credential
    /// directory and refreshed as they expire.
    Login(LoginService),
}

/// An account `xi login` signs in to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginService {
    /// A ChatGPT plan, through the Codex backend.
    Chatgpt,
    /// A Claude subscription through Claude Code's OAuth client.
    Anthropic,
}

/// Wire differences between providers that speak the same [`Api`]. Set
/// under `[provider.<name>]` and per `[[model]]`; a model's settings
/// override its provider's one field at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compat {
    /// Whether Responses requests carry the output-token limit. Default
    /// `true`; the ChatGPT backend rejects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_limit: Option<bool>,
    /// A header that carries the session's prompt-cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_header: Option<String>,
    /// The Responses Lite request shape: tools travel as a leading input
    /// item, tool calls are sequential and reasoning spans all turns.
    /// Default `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_lite: Option<bool>,
}

impl Compat {
    /// These settings, with unset fields taken from `base`.
    #[must_use]
    pub fn or(&self, base: &Self) -> Self {
        Self {
            output_limit: self.output_limit.or(base.output_limit),
            session_header: self
                .session_header
                .clone()
                .or_else(|| base.session_header.clone()),
            responses_lite: self.responses_lite.or(base.responses_lite),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A configured provider, as written under `[provider.<name>]`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Wire format.
    pub api: Api,
    /// Base URL, for example `https://api.anthropic.com`.
    pub base_url: String,
    /// Credential source.
    #[serde(default)]
    pub auth: Auth,
    /// Wire differences shared by the provider's models.
    #[serde(default, skip_serializing_if = "Compat::is_empty")]
    pub compat: Compat,
}

/// One `[[model]]` table: provider-neutral metadata plus the model's wire
/// differences, which stay out of the protocol's [`ModelInfo`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelEntry {
    /// Metadata clients see.
    #[serde(flatten)]
    pub info: ModelInfo,
    /// Overrides of the provider's [`Compat`].
    #[serde(skip_serializing_if = "Compat::is_empty")]
    pub compat: Compat,
}

impl From<ModelInfo> for ModelEntry {
    fn from(info: ModelInfo) -> Self {
        Self {
            info,
            compat: Compat::default(),
        }
    }
}

impl<'de> Deserialize<'de> for ModelEntry {
    // `#[serde(flatten)]` would hide unknown fields from `ModelInfo`'s
    // `deny_unknown_fields`, so the table is split by hand.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut table = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let compat = table
            .remove("compat")
            .map(serde_json::from_value)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or_default();
        let info = serde_json::from_value(Value::Object(table)).map_err(D::Error::custom)?;
        Ok(Self { info, compat })
    }
}

/// The registry sections of a config file: `[provider.<name>]` tables and
/// `[[model]]` tables. Later configs override earlier entries with the same
/// provider name or the same provider and model id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RegistryConfig {
    /// Providers by name.
    #[serde(default)]
    pub provider: BTreeMap<String, ProviderConfig>,
    /// Models.
    #[serde(default)]
    pub model: Vec<ModelEntry>,
    /// Role presets by name, as `[preset.<name>]` tables.
    #[serde(default)]
    pub preset: BTreeMap<String, Preset>,
}

/// One `PROVIDER/MODEL` for every [`ModelRole`], selected with `preset`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// The most capable model.
    pub slow: String,
    /// The model new sessions start with.
    pub default: String,
    /// A faster, cheaper model.
    pub smol: String,
    /// The fastest, cheapest model.
    pub tiny: String,
}

impl Preset {
    /// The `PROVIDER/MODEL` for `role`.
    #[must_use]
    pub fn get(&self, role: ModelRole) -> &str {
        match role {
            ModelRole::Slow => &self.slow,
            ModelRole::Default => &self.default,
            ModelRole::Smol => &self.smol,
            ModelRole::Tiny => &self.tiny,
        }
    }
}

/// Known providers and models.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    providers: BTreeMap<String, ProviderConfig>,
    models: Vec<ModelEntry>,
    presets: BTreeMap<String, Preset>,
    clients: Arc<Mutex<BTreeMap<String, reqwest::Client>>>,
    credentials: Option<PathBuf>,
}
