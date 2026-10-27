//! Session log events.
//!
//! A session file is JSONL; each line is one [`Event`]. Parent links form a
//! tree rooted at a single [`EventKind::SessionStarted`] event.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{Block, ToolOutput};
use crate::ids::{EventId, ExtensionId, SessionId, Timestamp, ToolCallId};
use crate::model::Reasoning;

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
        /// The session that spawned this one as a subagent. Child logs live
        /// in the same directory as their parent's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<SessionId>,
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
        #[serde(deserialize_with = "crate::content::assistant_content")]
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
    /// Policy outcome for a tool call, or for one connection through the
    /// egress proxy. Recorded for every call and connection, so the log is
    /// a complete audit trail. A connection gets its own `call` id and names
    /// its origin in `during` or `extension`; its `action` is `net host:port`.
    ToolDecision {
        /// The call or connection being decided.
        call: ToolCallId,
        /// Human-readable description of the boundary crossing.
        action: String,
        /// The outcome.
        decision: Decision,
        /// The rule that decided, as `layer#index`. Absent when no rule matched.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// Whether branch taint changed an allow into an ask.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        taint: bool,
        /// Egress only: the tool call whose process connected, if one was
        /// running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        during: Option<ToolCallId>,
        /// Egress only: the extension whose process connected.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extension: Option<String>,
    },
    /// Answer to an [`Decision::Ask`].
    ApprovalResolved {
        /// The call that was asked about.
        call: ToolCallId,
        /// Whether the call may run.
        approved: bool,
        /// Who answered.
        by: Approver,
        /// How long an approved answer applies.
        #[serde(default, skip_serializing_if = "Remember::is_once")]
        remember: Remember,
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
    /// Summary standing in for older context. Context built from this
    /// event's descendants is: the summary, then the events from `kept`
    /// through this event's parent, then this event's descendants. Without
    /// `kept`, every ancestor is replaced. The ancestors stay in the log.
    Compaction {
        /// The summary given to the model.
        summary: String,
        /// Context size before compaction.
        tokens_before: u64,
        /// First ancestor kept verbatim after the summary. It is a
        /// `user_message` or `assistant_message`, so no tool call is
        /// separated from its result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kept: Option<EventId>,
        /// Token use of the request that wrote the summary. Absent when an
        /// extension supplied it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// The user switched models.
    ModelChanged {
        /// The new model.
        model: ModelRef,
    },
    /// The user changed reasoning depth.
    ReasoningChanged {
        /// The new reasoning depth.
        reasoning: Reasoning,
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

/// How long an approval applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Remember {
    /// Apply only to this call.
    #[default]
    Once,
    /// Remember on this session branch.
    Session,
    /// Also save an allow rule in the trusted workspace.
    Workspace,
}

impl Remember {
    /// Whether the approval applies only to this call.
    #[must_use]
    pub fn is_once(&self) -> bool {
        *self == Self::Once
    }
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
    /// The approval deadline expired.
    Timeout,
    /// The worker stopped before the approval was resolved.
    Interrupted,
    /// The turn was cancelled while approval was pending.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::content::OutputBlock;

    #[test]
    fn reasoning_changed_wire_shape() {
        for (reasoning, name) in [(Reasoning::High, "high"), (Reasoning::XHigh, "xhigh")] {
            let event = EventKind::ReasoningChanged { reasoning };
            let wire = json!({"type": "reasoning_changed", "reasoning": name});
            assert_eq!(serde_json::to_value(&event).unwrap(), wire);
            assert_eq!(serde_json::from_value::<EventKind>(wire).unwrap(), event);
        }
    }

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
    #[test]
    fn approval_scopes_and_legacy_events_wire_shapes() {
        for (remember, name) in [
            (Remember::Once, "once"),
            (Remember::Session, "session"),
            (Remember::Workspace, "workspace"),
        ] {
            assert_eq!(serde_json::to_value(remember).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<Remember>(json!(name)).unwrap(),
                remember
            );
            let event = EventKind::ApprovalResolved {
                call: "call".into(),
                approved: true,
                by: Approver::User,
                remember,
            };
            let wire = serde_json::to_value(&event).unwrap();
            assert_eq!(wire.get("remember").is_none(), remember.is_once());
            assert_eq!(serde_json::from_value::<EventKind>(wire).unwrap(), event);
        }
        let old = json!({
            "type": "approval_resolved", "call": "call", "approved": true,
            "by": {"kind": "user"},
        });
        let event: EventKind = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(
            event,
            EventKind::ApprovalResolved {
                remember: Remember::Once,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(event).unwrap(), old);
        let old = json!({
            "type": "tool_decision", "call": "call", "action": "tool read",
            "decision": "ask",
        });
        let event: EventKind = serde_json::from_value(old.clone()).unwrap();
        assert!(matches!(
            event,
            EventKind::ToolDecision {
                taint: false,
                during: None,
                extension: None,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(event).unwrap(), old);
        let event = EventKind::ToolDecision {
            call: "call".into(),
            action: "net example.org".into(),
            decision: Decision::Ask,
            rule: None,
            taint: true,
            during: None,
            extension: None,
        };
        assert_eq!(serde_json::to_value(&event).unwrap()["taint"], true);
        assert_eq!(
            serde_json::from_value::<EventKind>(serde_json::to_value(&event).unwrap()).unwrap(),
            event,
        );
        let event = EventKind::ToolDecision {
            call: "connection".into(),
            action: "net example.org:443".into(),
            decision: Decision::Ask,
            rule: None,
            taint: false,
            during: Some("c1".into()),
            extension: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({
                "type": "tool_decision",
                "call": "connection",
                "action": "net example.org:443",
                "decision": "ask",
                "during": "c1",
            })
        );
        assert_eq!(serde_json::from_value::<EventKind>(wire).unwrap(), event);
    }
}
