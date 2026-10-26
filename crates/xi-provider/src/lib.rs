//! Model providers.
//!
//! A [`Provider`] turns a provider-neutral [`Request`] into one streamed
//! model response. Anthropic thinking signatures, redacted thinking and
//! provider-specific blocks round-trip unchanged. OpenAI-compatible reasoning
//! is exposed as [`Block::Thinking`] but is not replayed.
//!
//! The [`Registry`] knows which providers and models exist. It starts from a
//! built-in table and is extended from config ([`RegistryConfig`]).
//!
//! Clients share a connection pool per provider, use a 30-second connection
//! timeout and a 120-second read/idle timeout, and have no total timeout.
//! Anthropic caches the system, final tool definition and final history block.
//! Current Claude models use adaptive thinking and effort; older models use
//! token budgets. Models that cannot disable thinking clamp [`Reasoning::Off`]
//! to their lowest effort (Sonnet 5.5 also disables up-front thinking).
//! Built-in prices are base, standard-tier USD per million tokens, not
//! long-context premiums. Config layers can replace all built-in metadata.
//!
//! The `live` example accepts `XI_BASE_URL` and `XI_MODEL` and exercises an
//! OpenAI-compatible stream plus a tool-result round trip without credentials.

mod anthropic;
mod openai;
mod registry;
mod sse;
mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use xi_protocol::event::{ModelRef, StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::Delta;
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

/// How much the model should reason before answering. Adapters map this to
/// the provider's own setting and ignore it for models without reasoning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// No reasoning.
    Off,
    /// Brief reasoning.
    Low,
    /// The provider's default depth.
    #[default]
    Medium,
    /// Extended reasoning.
    High,
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
}

/// Metadata for one model, as written in a `[[model]]` table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    /// Provider key.
    pub provider: String,
    /// The provider's model id.
    pub id: String,
    /// Context window in tokens.
    pub context_window: u64,
    /// Output token limit per request.
    pub max_output: u32,
    /// Whether the model supports reasoning.
    #[serde(default)]
    /// Anthropic models with mandatory reasoning clamp `Off` to low effort.
    pub reasoning: bool,
    /// Prices in US dollars per million tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
}

/// Prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    /// Uncached input.
    pub input: f64,
    /// Output.
    pub output: f64,
    /// Cache reads.
    #[serde(default)]
    pub cache_read: f64,
    /// Cache writes.
    #[serde(default)]
    pub cache_write: f64,
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
    pub model: Vec<ModelInfo>,
}

/// Known providers and models.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    providers: BTreeMap<String, ProviderConfig>,
    models: Vec<ModelInfo>,
    clients: Arc<Mutex<BTreeMap<String, reqwest::Client>>>,
}
