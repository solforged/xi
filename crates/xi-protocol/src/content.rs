//! Message content and tool output.
//!
//! Blocks are provider-neutral but lossless: anything a provider needs echoed
//! back on the next request (thinking signatures, redacted thinking, provider
//! specific blocks) has a place to live.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::ToolCallId;

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
        signature: Option<String>,
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
    },
    /// A provider-specific block xi does not interpret, kept for round-tripping.
    Opaque {
        /// Provider that produced the block.
        provider: String,
        /// The block exactly as received.
        value: Value,
    },
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
