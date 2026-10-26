//! Provider-neutral model metadata and reasoning settings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How much the model should reason before answering, lowest first. The
/// names are the union of the providers' effort scales; each model lists
/// the subset it accepts in [`ModelInfo::reasoning`].
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    /// No reasoning.
    Off,
    /// The least reasoning short of none.
    Minimal,
    /// Brief reasoning.
    Low,
    /// Moderate reasoning.
    #[default]
    Medium,
    /// Extended reasoning.
    High,
    /// Extra-high reasoning for long agentic work.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Reasoning without a spending limit.
    Max,
}

impl Reasoning {
    /// Every level, lowest first.
    pub const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The level's name in config, on the wire and in commands.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for Reasoning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A name that is not a [`Reasoning`] level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownReasoning(pub String);

impl fmt::Display for UnknownReasoning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown reasoning level {:?}", self.0)
    }
}

impl std::error::Error for UnknownReasoning {}

impl FromStr for Reasoning {
    type Err = UnknownReasoning;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.name() == name)
            .ok_or_else(|| UnknownReasoning(name.into()))
    }
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
    /// Reasoning levels the model accepts; empty when it doesn't reason.
    /// `off` is listed only when the model can stop reasoning.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<Reasoning>,
    /// Prices in US dollars per million tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
}

impl ModelInfo {
    /// The level a request at `requested` runs at. A listed level is kept.
    /// Otherwise the nearest listed level below it, so a model never costs
    /// more than asked, but never `off` unless `off` was asked for; failing
    /// that, the nearest level above. Models that don't reason keep
    /// `requested`, which their adapters ignore.
    #[must_use]
    pub fn resolve(&self, requested: Reasoning) -> Reasoning {
        let levels = self.reasoning.iter().copied();
        if self.reasoning.is_empty() || self.reasoning.contains(&requested) {
            return requested;
        }
        levels
            .clone()
            .filter(|&level| level < requested && level != Reasoning::Off)
            .max()
            .or_else(|| levels.clone().filter(|&level| level > requested).min())
            .or_else(|| levels.min())
            .unwrap_or(requested)
    }

    /// The listed level after `current`'s resolved level, wrapping to the
    /// lowest; `None` for models that don't reason.
    #[must_use]
    pub fn next_reasoning(&self, current: Reasoning) -> Option<Reasoning> {
        let current = self.resolve(current);
        let levels = self.reasoning.iter().copied();
        levels
            .clone()
            .filter(|&level| level > current)
            .min()
            .or_else(|| levels.min())
    }
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

#[cfg(test)]
mod tests {
    use super::Reasoning::{self, High, Low, Max, Medium, Minimal, Off, XHigh};
    use super::*;

    fn model(reasoning: &[Reasoning]) -> ModelInfo {
        ModelInfo {
            provider: "p".into(),
            id: "m".into(),
            context_window: 1,
            max_output: 1,
            reasoning: reasoning.to_vec(),
            pricing: None,
        }
    }

    #[test]
    fn names_match_the_wire() {
        for level in Reasoning::ALL {
            assert_eq!(serde_json::to_value(level).unwrap(), level.name());
            assert_eq!(level.name().parse::<Reasoning>(), Ok(level));
        }
        assert!("none".parse::<Reasoning>().is_err());
    }

    #[test]
    fn resolve_rounds_down_but_never_to_off() {
        let sonnet_4_6 = model(&[Off, Low, Medium, High, Max]);
        let always_on = model(&[Low, Medium, High, XHigh, Max]);
        for (model, requested, resolved) in [
            (&sonnet_4_6, XHigh, High),
            (&sonnet_4_6, Max, Max),
            (&sonnet_4_6, Minimal, Low),
            (&sonnet_4_6, Off, Off),
            (&always_on, Off, Low),
            (&always_on, Minimal, Low),
            (&always_on, Max, Max),
            (&model(&[Low, Medium]), XHigh, Medium),
            (&model(&[Off]), High, Off),
            (&model(&[]), XHigh, XHigh),
        ] {
            assert_eq!(model.resolve(requested), resolved, "{requested}");
        }
    }

    #[test]
    fn next_reasoning_cycles_listed_levels() {
        let always_on = model(&[Max, Low, High]);
        assert_eq!(always_on.next_reasoning(Low), Some(High));
        assert_eq!(always_on.next_reasoning(Max), Some(Low));
        assert_eq!(always_on.next_reasoning(Medium), Some(High));
        assert_eq!(always_on.next_reasoning(Off), Some(High));
        assert_eq!(model(&[]).next_reasoning(Low), None);
    }
}
