//! Session log events.
//!
//! A session file is JSONL; each line is one [`Event`]. Parent links form a
//! tree rooted at a single [`EventKind::SessionStarted`] event.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{Block, ToolOutput};
use crate::ids::{EventId, ExtensionId, SessionId, Timestamp, ToolCallId};

/// One entry in a session log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unique id within the session.
    pub id: EventId,
    /// The event this one continues from. Only the root has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<EventId>,
    /// When the kernel appended the event.
    pub at: Timestamp,
    /// The payload, tagged by `type`.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Event payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// The root of every session.
    SessionStarted {
        /// Session identity, stable across file moves.
        session: SessionId,
        /// Workspace root the session runs in.
        cwd: PathBuf,
        /// Model selected at start.
        model: ModelRef,
        /// Version of the xi binary that created the session.
        xi_version: String,
    },
    /// Input from the user.
    UserMessage {
        /// Message blocks.
        content: Vec<Block>,
    },
    /// A complete model response.
    AssistantMessage {
        /// Model that produced the response.
        model: ModelRef,
        /// Response blocks, including tool calls.
        content: Vec<Block>,
        /// Why the model stopped.
        stop: StopReason,
        /// Token accounting for this request.
        usage: Usage,
    },
    /// A model request that failed after any retries.
    TurnFailed {
        /// Error shown to the user.
        message: String,
    },
    /// Policy outcome for a tool call. Recorded for every call, so the log is
    /// a complete audit trail.
    ToolDecision {
        /// The call being decided.
        call: ToolCallId,
        /// Human-readable description of the boundary crossing.
        action: String,
        /// The outcome.
        decision: Decision,
        /// The rule that decided, as `layer#index`. Absent when no rule matched.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    /// Answer to an [`Decision::Ask`].
    ApprovalResolved {
        /// The call that was asked about.
        call: ToolCallId,
        /// Whether the call may run.
        approved: bool,
        /// Who answered.
        by: Approver,
    },
    /// Output of a tool call.
    ToolResult {
        /// The call this answers.
        call: ToolCallId,
        /// What the tool returned.
        #[serde(flatten)]
        output: ToolOutput,
        /// File holding the full output when the content was truncated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spill: Option<PathBuf>,
    },
    /// The branch ingested untrusted content. Applies to this event's
    /// descendants only.
    Tainted {
        /// The call that brought the content in, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<ToolCallId>,
        /// Why the content is untrusted.
        reason: String,
    },
    /// Summary standing in for every ancestor of this event when building
    /// context. The ancestors stay in the log.
    Compaction {
        /// The summary given to the model.
        summary: String,
        /// Context size before compaction.
        tokens_before: u64,
    },
    /// The user switched models.
    ModelChanged {
        /// The new model.
        model: ModelRef,
    },
    /// State persisted by an extension. The kernel does not interpret it.
    Custom {
        /// The extension that wrote the event.
        extension: ExtensionId,
        /// Extension-defined discriminator.
        kind: String,
        /// Extension-defined payload.
        data: Value,
    },
}

/// A model on a provider.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider key, for example `anthropic`.
    pub provider: String,
    /// Provider's model id.
    pub model: String,
}

/// Why a model response ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model is waiting for tool results.
    ToolUse,
    /// The output token limit was reached.
    MaxTokens,
    /// The model declined to answer.
    Refusal,
    /// The user cancelled the turn.
    Cancelled,
}

/// Token counts for one model request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens, including reasoning.
    pub output: u64,
    /// Input tokens served from the prompt cache.
    #[serde(default)]
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    #[serde(default)]
    pub cache_write: u64,
}

/// A policy outcome. Variants are ordered by strictness, so `max` picks the
/// stricter of two decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Proceed without asking.
    Allow,
    /// Proceed only after approval.
    Ask,
    /// Refuse.
    Deny,
}

/// Who resolved an approval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Approver {
    /// A person, through any attached client.
    User,
    /// A `pre_tool` hook.
    Hook {
        /// The extension that answered.
        extension: ExtensionId,
    },
    /// No client was attached; the headless default (`--on-ask`) applied.
    Headless,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::content::OutputBlock;

    #[test]
    fn tool_result_is_one_flat_object_on_disk() {
        let id = EventId::generate();
        let parent = EventId::generate();
        let event = Event {
            id,
            parent: Some(parent),
            at: Timestamp(7),
            kind: EventKind::ToolResult {
                call: "c1".into(),
                output: ToolOutput {
                    content: vec![OutputBlock::Text { text: "ok".into() }],
                    is_error: false,
                    untrusted: true,
                },
                spill: None,
            },
        };
        let wire = json!({
            "id": id, "parent": parent, "at": 7, "type": "tool_result", "call": "c1",
            "content": [{"type": "text", "text": "ok"}], "is_error": false, "untrusted": true,
        });
        assert_eq!(serde_json::to_value(&event).unwrap(), wire);
        assert_eq!(serde_json::from_value::<Event>(wire).unwrap(), event);
    }
}
