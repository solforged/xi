//! Message content and tool output.
//!
//! Blocks are provider-neutral but lossless: anything a provider needs echoed
//! back on the next request (thinking signatures, redacted thinking, provider
//! specific blocks) has a place to live.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::ToolCallId;

/// The wire API that owns an uninterpreted content block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderTag {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Responses.
    Openai,
}

/// Provider-owned reasoning data needed to replay a thinking block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Signature {
    /// An Anthropic thinking signature, stored as a plain string.
    Anthropic(String),
    /// Encrypted OpenAI Responses reasoning, stored as an object.
    Responses {
        /// The reasoning item's identifier.
        id: String,
        /// The encrypted reasoning payload.
        encrypted_content: String,
    },
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Responses {
            id: String,
            encrypted_content: String,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Text(String),
            Responses(Responses),
        }

        let sealed = match Stored::deserialize(deserializer)? {
            Stored::Responses(sealed) => sealed,
            Stored::Text(text) => match serde_json::from_str::<Responses>(&text) {
                Ok(sealed) => sealed,
                Err(_) => return Ok(Self::Anthropic(text)),
            },
        };
        Ok(Self::Responses {
            id: sealed.id,
            encrypted_content: sealed.encrypted_content,
        })
    }
}

/// One block of a user or assistant message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// An inline image.
    Image {
        /// MIME type, for example `image/png`.
        media_type: String,
        /// Base64-encoded bytes.
        data: String,
    },
    /// Visible model reasoning.
    Thinking {
        /// The reasoning text.
        text: String,
        /// Provider signature that must be sent back unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<Signature>,
    },
    /// Reasoning the provider returned only in encrypted form.
    RedactedThinking {
        /// Opaque provider payload.
        data: String,
    },
    /// A tool call requested by the model.
    ToolUse {
        /// Provider-assigned call id.
        id: ToolCallId,
        /// Registered tool name.
        name: String,
        /// Arguments as produced by the model.
        input: Value,
        /// Uninterpreted wire fields, keyed by the provider that owns them.
        /// They remain attached to an executable call across persistence.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        extra: BTreeMap<String, Value>,
    },
    /// A provider-specific block xi does not interpret, kept for round-tripping.
    Opaque {
        /// Provider that produced the block.
        provider: ProviderTag,
        /// The block exactly as received.
        value: Value,
    },
}

/// Upgrades the initial log format, which stored extended Anthropic tool calls
/// as opaque blocks. Read-time migration leaves the append-only file intact.
pub(crate) fn assistant_content<'de, D>(deserializer: D) -> Result<Vec<Block>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut blocks = Vec::<Block>::deserialize(deserializer)?;
    for block in &mut blocks {
        let Block::Opaque { provider, value } = block else {
            continue;
        };
        if *provider != ProviderTag::Anthropic || value["type"] != "tool_use" {
            continue;
        }
        let fields = value
            .as_object_mut()
            .expect("a tool_use has an object representation");
        let mut string = |key| match fields.remove(key) {
            Some(Value::String(value)) => Ok(value),
            _ => Err(<D::Error as serde::de::Error>::custom(format!(
                "Tool use has no {key}"
            ))),
        };
        let id = ToolCallId(string("id")?);
        let name = string("name")?;
        let input = fields
            .remove("input")
            .ok_or_else(|| <D::Error as serde::de::Error>::custom("Tool use has no input"))?;
        fields.remove("type");
        let extra = if fields.is_empty() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("anthropic".into(), value.take())])
        };
        *block = Block::ToolUse {
            id,
            name,
            input,
            extra,
        };
    }
    Ok(blocks)
}

/// One block of tool output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// An inline image.
    Image {
        /// MIME type, for example `image/png`.
        media_type: String,
        /// Base64-encoded bytes.
        data: String,
    },
}

/// The result of running a tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Output shown to the model.
    pub content: Vec<OutputBlock>,
    /// Whether the tool failed. The model still sees the content.
    #[serde(default)]
    pub is_error: bool,
    /// Whether the output carries untrusted third-party content. The kernel
    /// taints the session when this is set.
    #[serde(default)]
    pub untrusted: bool,
}

/// How a tool result starts when the OS sandbox could not launch the
/// command, so clients can tell a broken sandbox from a failed command.
pub const SANDBOX_START_FAILED: &str = "Sandbox could not start";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventKind, ModelRef, StopReason, Usage};
    use serde_json::json;

    #[test]
    fn thinking_signatures_read_plain_legacy_and_object_forms() {
        let sealed = json!({"id":"rs_1", "encrypted_content":"sealed"});
        let responses = Signature::Responses {
            id: "rs_1".into(),
            encrypted_content: "sealed".into(),
        };
        for (input, expected, stored) in [
            (
                json!("signed"),
                Signature::Anthropic("signed".into()),
                json!("signed"),
            ),
            (json!(sealed.to_string()), responses.clone(), sealed.clone()),
            (sealed.clone(), responses, sealed),
        ] {
            let block: Block = serde_json::from_value(json!({
                "type":"thinking", "text":"reasoning", "signature":input,
            }))
            .unwrap();
            assert_eq!(
                block,
                Block::Thinking {
                    text: "reasoning".into(),
                    signature: Some(expected),
                }
            );
            assert_eq!(serde_json::to_value(block).unwrap()["signature"], stored);
        }
    }

    #[test]
    fn opaque_provider_tags_keep_the_wire_names_and_reject_unknown_tags() {
        for (tag, name) in [
            (ProviderTag::Anthropic, "anthropic"),
            (ProviderTag::Openai, "openai"),
        ] {
            let value = json!({"type":"opaque", "provider":name, "value":{}});
            let block: Block = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                block,
                Block::Opaque {
                    provider: tag,
                    value: json!({})
                }
            );
            assert_eq!(serde_json::to_value(block).unwrap(), value);
        }
        assert!(
            serde_json::from_value::<Block>(
                json!({"type":"opaque", "provider":"other", "value":{}})
            )
            .is_err()
        );
    }

    #[test]
    fn tool_metadata_round_trips_and_old_calls_default_to_empty() {
        let plain = json!({"type":"tool_use", "id":"call", "name":"read", "input":{}});
        let mut block: Block = serde_json::from_value(plain.clone()).unwrap();
        assert_eq!(serde_json::to_value(&block).unwrap(), plain);
        let Block::ToolUse { extra, .. } = &mut block else {
            panic!("missing tool call");
        };
        assert!(extra.is_empty());
        extra.insert("anthropic".into(), json!({"caller":{"type":"direct"}}));
        let decoded: Block = serde_json::from_value(serde_json::to_value(&block).unwrap()).unwrap();
        assert_eq!(decoded, block);
    }

    #[test]
    fn only_legacy_anthropic_assistant_tool_calls_are_upgraded() {
        let legacy = Block::Opaque {
            provider: ProviderTag::Anthropic,
            value: json!({
                "type":"tool_use", "id":"call", "name":"read", "input":{"path":"a"},
                "caller":{"type":"direct"}, "future_field":[1, 2],
            }),
        };
        let untouched = vec![
            Block::Opaque {
                provider: ProviderTag::Openai,
                value: json!({"type":"tool_use", "id":"other", "name":"read", "input":{}}),
            },
            Block::Opaque {
                provider: ProviderTag::Anthropic,
                value: json!({"type":"server_tool_use", "id":"server", "name":"web_search", "input":{}}),
            },
        ];
        let mut content = vec![legacy.clone()];
        content.extend(untouched.clone());
        let user = EventKind::UserMessage {
            content: content.clone(),
        };
        assert_eq!(
            serde_json::from_value::<EventKind>(serde_json::to_value(&user).unwrap()).unwrap(),
            user
        );
        let event = EventKind::AssistantMessage {
            model: ModelRef {
                provider: "test".into(),
                model: "test".into(),
            },
            content,
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        };
        let decoded: EventKind =
            serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap();
        let EventKind::AssistantMessage { content, .. } = &decoded else {
            panic!("missing assistant event");
        };
        assert_eq!(
            content[0],
            Block::ToolUse {
                id: "call".into(),
                name: "read".into(),
                input: json!({"path":"a"}),
                extra: BTreeMap::from([(
                    "anthropic".into(),
                    json!({
                        "caller":{"type":"direct"}, "future_field":[1, 2],
                    })
                )]),
            }
        );
        assert_eq!(&content[1..], untouched);
        assert_eq!(
            serde_json::from_value::<EventKind>(serde_json::to_value(&decoded).unwrap()).unwrap(),
            decoded
        );
    }
}
