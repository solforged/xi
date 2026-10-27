//! Context estimates, safe retained tails and summary requests.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};
use tokio_util::sync::CancellationToken;
use xi_protocol::event::Usage;
use xi_protocol::methods::client::CompactResult;
use xi_protocol::{Block, Event, EventKind, OutputBlock, ToolCallId};
use xi_provider::{Message, Request};

use crate::{Backend, Error, Result, Session, State, context};

/// The model-relative percentage or fixed token estimate that triggers compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// An integer percentage of the selected model's context window, from 1 to 100.
    Percent(u8),
    /// A positive token count that fits in a TOML integer.
    Tokens(u64),
}

impl<'de> Deserialize<'de> for Trigger {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Value {
            Percent(u8),
            Tokens(u64),
        }

        match Value::deserialize(deserializer)? {
            Value::Percent(percent @ 1..=100) => Ok(Self::Percent(percent)),
            Value::Tokens(tokens) if tokens > 0 && tokens <= i64::MAX as u64 => {
                Ok(Self::Tokens(tokens))
            }
            Value::Percent(_) => Err(serde::de::Error::custom(
                "compaction trigger percent must be between 1 and 100",
            )),
            Value::Tokens(_) => Err(serde::de::Error::custom(
                "compaction trigger tokens must be between 1 and 9223372036854775807",
            )),
        }
    }
}

/// Automatic compaction settings. Manual compaction ignores `enabled`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Enable threshold and context-overflow compaction.
    pub enabled: bool,
    /// A percentage of the selected context window or a fixed token estimate.
    pub trigger: Trigger,
    /// Approximate size of the recent conversation retained verbatim.
    pub keep_recent_tokens: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger: Trigger::Percent(80),
            keep_recent_tokens: 20_000,
        }
    }
}

impl Config {
    /// Effective threshold for the selected model.
    #[must_use]
    pub fn trigger(&self, context_window: u64) -> u64 {
        match self.trigger {
            Trigger::Percent(percent) => {
                (u128::from(context_window) * u128::from(percent) / 100) as u64
            }
            Trigger::Tokens(tokens) => tokens,
        }
    }

    /// Keep at most half the trigger, leaving room for a useful summary.
    #[must_use]
    pub fn keep(&self, context_window: u64) -> u64 {
        self.keep_recent_tokens
            .min(self.trigger(context_window) / 2)
    }
}

/// Optional fields preserve earlier settings when layering config files.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Layer {
    enabled: Option<bool>,
    trigger: Option<Trigger>,
    keep_recent_tokens: Option<u64>,
}

impl Layer {
    pub(crate) fn apply(self, config: &mut Config) {
        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
        if let Some(trigger) = self.trigger {
            config.trigger = trigger;
        }
        if let Some(keep) = self.keep_recent_tokens {
            config.keep_recent_tokens = keep;
        }
    }
}

fn chars(text: &str) -> u64 {
    text.chars().count() as u64
}

fn value_chars(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::String(text) => chars(text),
        serde_json::Value::Array(values) => values.iter().map(value_chars).sum(),
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| chars(key) + value_chars(value))
            .sum(),
        // Scalar JSON values need at most a few tokens.
        _ => 4,
    }
}

fn block_chars(block: &Block) -> u64 {
    match block {
        Block::Text { text } => chars(text),
        Block::Thinking { text, signature } => chars(text) + signature.as_deref().map_or(0, chars),
        Block::Image { media_type, data } => chars(media_type) + chars(data),
        Block::RedactedThinking { data } => chars(data),
        Block::ToolUse {
            id,
            name,
            input,
            extra,
        } => {
            chars(&id.0)
                + chars(name)
                + value_chars(input)
                + extra
                    .iter()
                    .map(|(provider, value)| chars(provider) + value_chars(value))
                    .sum::<u64>()
        }
        Block::Opaque { provider, value } => chars(provider) + value_chars(value),
    }
}

fn event_chars(event: &Event) -> u64 {
    match &event.kind {
        EventKind::UserMessage { content } | EventKind::AssistantMessage { content, .. } => {
            content.iter().map(block_chars).sum()
        }
        EventKind::ToolResult { output, .. } => output
            .content
            .iter()
            .map(|block| match block {
                OutputBlock::Text { text } => chars(text),
                OutputBlock::Image { media_type, data } => chars(media_type) + chars(data),
            })
            .sum(),
        _ => 0,
    }
}

fn total(usage: Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write)
        .saturating_add(usage.output)
}

pub(crate) fn estimate(state: &State) -> u64 {
    let path = state.log.lineage(state.head).expect("known head");
    let mut appended = 0u64;
    for event in path.into_iter().rev() {
        match event.kind {
            EventKind::Compaction { .. } => break,
            EventKind::AssistantMessage { usage, .. } if total(usage) > 0 => {
                return total(usage).saturating_add(appended.div_ceil(4));
            }
            _ => appended = appended.saturating_add(event_chars(event)),
        }
    }
    // Usage from a retained pre-compaction assistant describes the old request,
    // not the new prefix. Until the next response, estimate the rebuilt context.
    let context = state.log.context(state.head).expect("known head");
    let fixed = chars(&state.system.prompt)
        + state
            .tool_specs()
            .map(|spec| {
                chars(&spec.name) + chars(&spec.description) + value_chars(&spec.input_schema)
            })
            .sum::<u64>();
    (fixed
        + context.summary.map_or(0, chars)
        + context.events.into_iter().map(event_chars).sum::<u64>())
    .div_ceil(4)
}

// Candidates are message boundaries with no outstanding calls on either side.
// Select the first boundary whose suffix fits the budget. A newest oversized
// tool batch stays whole rather than being split to meet a token target.
fn kept_start(events: &[&Event], budget: u64) -> Result<usize> {
    let mut pending = HashSet::<&ToolCallId>::new();
    let mut boundaries = Vec::new();
    let mut size = 0u64;
    for (index, event) in events.iter().enumerate() {
        match &event.kind {
            EventKind::UserMessage { .. } | EventKind::AssistantMessage { .. }
                if pending.is_empty() =>
            {
                boundaries.push((index, size));
            }
            _ => {}
        }
        match &event.kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    if let Block::ToolUse { id, .. } = block {
                        pending.insert(id);
                    }
                }
            }
            EventKind::ToolResult { call, .. } => {
                pending.remove(call);
            }
            _ => {}
        }
        size = size.saturating_add(event_chars(event));
    }
    if !pending.is_empty() {
        return Err(Error::Config("cannot compact pending tool calls".into()));
    }
    if budget == 0 {
        return Ok(events.len());
    }
    Ok(boundaries
        .iter()
        .find(|(_, before)| (size - before).div_ceil(4) <= budget)
        .or_else(|| boundaries.last())
        .map_or(events.len(), |(index, _)| *index))
}

impl Session {
    /// Compacts an idle session, even when automatic compaction is disabled.
    pub async fn compact(&self) -> Result<CompactResult> {
        let mut state = self.0.state.try_lock().map_err(|_| Error::Running)?;
        let cancel = {
            let mut control = self.0.control.lock().expect("control lock");
            if control.running {
                return Err(Error::Running);
            }
            control.cancel = CancellationToken::new();
            control.cancel.clone()
        };
        if state.backend.is_none() {
            state.backend = Some(Backend::connect(&state.config)?.into());
        }
        self.compact_at_boundary(&mut state, &cancel)
            .await?
            .ok_or_else(|| {
                Error::Config(
                    if cancel.is_cancelled() {
                        "compaction cancelled"
                    } else {
                        "nothing to compact outside the retained tail"
                    }
                    .into(),
                )
            })
    }

    /// The caller holds the session state between complete model/tool batches.
    /// A `compact` extension hook may supply the summary, which skips the
    /// provider request and records no summary usage.
    pub(crate) async fn compact_at_boundary(
        &self,
        state: &mut State,
        cancel: &CancellationToken,
    ) -> Result<Option<CompactResult>> {
        let tokens_before = estimate(state);
        let (start, kept, previous_summary) = {
            let history = state.log.context(state.head).expect("known head");
            let backend = state.backend.as_ref().expect("connected backend");
            let start = kept_start(
                &history.events,
                state.config.compaction.keep(backend.model.context_window),
            )?;
            if !history.events[..start].iter().any(|event| {
                matches!(
                    event.kind,
                    EventKind::UserMessage { .. } | EventKind::AssistantMessage { .. }
                )
            }) {
                return Ok(None);
            }
            let kept = history.events.get(start).map(|event| event.id);
            (start, kept, history.summary.map(str::to_owned))
        };
        let hook_summary = self
            .extension_summary_locked(state, tokens_before, previous_summary, cancel)
            .await?;
        let (summary, usage) = if let Some(summary) = hook_summary {
            (summary, None)
        } else {
            // Hooks may have appended custom events at the head; the kept
            // boundary is unchanged because they come after it.
            let mut history = state.log.context(state.head).expect("known head");
            history.events.truncate(start);
            let mut messages = context::messages(history);
            messages.push(Message::User(vec![Block::Text {
                text: include_str!("compact.md").into(),
            }]));
            let backend = state.backend.as_ref().expect("connected backend");
            let request = Request {
                model: state.config.model.model.clone(),
                system: state.system.prompt.clone(),
                messages,
                tools: Vec::new(),
                max_tokens: backend.model.max_output,
                reasoning: backend.model.resolve(state.config.reasoning),
                cache_key: Some(self.id().to_string()),
            };
            // Summary deltas are not assistant output. Only the finished durable
            // compaction is broadcast, and cancellation never publishes a partial.
            let response = self
                .stream(
                    backend.provider.as_ref(),
                    &request,
                    &state.config,
                    cancel,
                    false,
                )
                .await?;
            if response.stop == xi_protocol::event::StopReason::Cancelled {
                return Ok(None);
            }
            if response.stop != xi_protocol::event::StopReason::EndTurn {
                return Err(Error::Config("compaction summary did not finish".into()));
            }
            let summary: String = response
                .content
                .into_iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text),
                    _ => None,
                })
                .collect();
            (summary, Some(response.usage))
        };
        if summary.trim().is_empty() {
            return Err(Error::Config("compaction summary is empty".into()));
        }
        if cancel.is_cancelled() {
            return Ok(None);
        }
        self.append(
            state,
            EventKind::Compaction {
                summary,
                tokens_before,
                kept,
                usage,
            },
        )?;
        Ok(Some(CompactResult {
            head: state.head,
            tokens_before,
        }))
    }
}
