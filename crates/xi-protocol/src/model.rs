//! Provider-neutral model metadata and reasoning settings.

use serde::{Deserialize, Serialize};

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
