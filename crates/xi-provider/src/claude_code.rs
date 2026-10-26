//! Claude Code subscription inference transport, enabled only by Anthropic login.

use std::borrow::Cow;

use aws_lc_rs::digest::{Context, SHA256};
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Block, Error, ErrorKind, Message, Request};

const VERSION: &str = "2.1.310";
const SDK_VERSION: &str = "0.112.1";
const MAX_OUTPUT_TOKENS: u32 = 64000;
const SYSTEM_INSTRUCTION: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const BILLING_PREFIX: &[u8] = b"{\"system\":[{\"text\":\"x-anthropic-billing-header: ";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const CCH_PLACEHOLDER: &str = "cch=00000";
const UTILITY_BETAS: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15";
const AGENT_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07";

pub(crate) struct Wire {
    // Stable for this HttpProvider's lifetime, rather than new attribution on
    // every request. xi does not maintain omp's separate installation identity.
    device_id: String,
    session_id: String,
}

impl Wire {
    pub(crate) fn new() -> Result<Self, Error> {
        let mut entropy = [0; 48];
        aws_lc_rs::rand::fill(&mut entropy)
            .map_err(|_| Error::new(ErrorKind::Config, "The system random source failed"))?;
        entropy[38] = (entropy[38] & 0x0f) | 0x40;
        entropy[40] = (entropy[40] & 0x3f) | 0x80;
        let device_id = hex(&entropy[..32]);
        let session = hex(&entropy[32..]);
        Ok(Self {
            device_id,
            session_id: format!(
                "{}-{}-{}-{}-{}",
                &session[..8],
                &session[8..12],
                &session[12..16],
                &session[16..20],
                &session[20..]
            ),
        })
    }

    /// Serializes and attests once, before signing. A 401 retry reuses the bytes.
    pub(crate) fn request(
        &self,
        request: &Request,
        reasoning: bool,
        headers: &mut HeaderMap,
    ) -> Result<Bytes, Error> {
        let mut body = crate::anthropic::body(
            request,
            reasoning,
            request.max_tokens.min(MAX_OUTPUT_TOKENS),
        )?;
        self.headers(request, &body, headers)?;
        self.prepare(request, &mut body);
        serialize(body)
    }

    fn prepare(&self, request: &Request, body: &mut Value) {
        let session = self.session(request);
        body["metadata"] = json!({"user_id": json!({
            "device_id": self.device_id, "session_id": session,
        }).to_string()});
        // Both empty and nonempty subscription tool lists are explicit on wire.
        if body.get("tools").is_none() {
            body["tools"] = json!([]);
        }
        for tool in body["tools"].as_array_mut().expect("tools is an array") {
            prefix_name(tool);
        }
        for message in body["messages"]
            .as_array_mut()
            .expect("messages is an array")
        {
            for block in message["content"]
                .as_array_mut()
                .expect("content is an array")
            {
                if block["type"] == "tool_use" {
                    prefix_name(block);
                }
            }
        }
        let first_text = request
            .messages
            .iter()
            .find_map(|message| match message {
                Message::User(blocks) => Some(
                    blocks
                        .iter()
                        .find_map(|block| match block {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        // The identity is the cache breakpoint; the caller's system stays intact
        // but is not an additional breakpoint in the Claude Code profile.
        let mut system = vec![
            json!({"type": "text", "text": billing_header(first_text)}),
            json!({"type": "text", "text": SYSTEM_INSTRUCTION, "cache_control": {"type": "ephemeral"}}),
        ];
        if let Some(Value::Array(prompts)) = body
            .as_object_mut()
            .expect("body is an object")
            .remove("system")
        {
            for mut prompt in prompts {
                prompt
                    .as_object_mut()
                    .expect("system block is an object")
                    .remove("cache_control");
                system.push(prompt);
            }
        }
        body["system"] = system.into();
    }

    fn session<'a>(&'a self, request: &'a Request) -> &'a str {
        request
            .cache_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .unwrap_or(&self.session_id)
    }

    fn headers(
        &self,
        request: &Request,
        body: &Value,
        headers: &mut HeaderMap,
    ) -> Result<(), Error> {
        let thinking = matches!(
            body["thinking"]["type"].as_str(),
            Some("enabled" | "adaptive" | "between_tools")
        );
        let agent = !request.tools.is_empty() || thinking;
        let mut betas = if agent { AGENT_BETAS } else { UTILITY_BETAS }.to_owned();
        if (agent && thinking) || body.get("output_config").is_some() {
            betas.push_str(",effort-2025-11-24");
        }
        if agent {
            betas.push_str(",fallback-credit-2026-06-01");
        }
        for (name, value) in [
            ("accept", "application/json"),
            ("content-type", "application/json"),
            (
                "user-agent",
                concat!("claude-cli/", "2.1.310", " (external, cli)"),
            ),
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", SDK_VERSION),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", "v26.3.0"),
            ("x-stainless-timeout", "600"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
            ("connection", "keep-alive"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => Cow::Borrowed("x64"),
            "aarch64" => Cow::Borrowed("arm64"),
            "x86" => Cow::Borrowed("x86"),
            other => Cow::Owned(format!("other::{other}")),
        };
        let os = match std::env::consts::OS {
            "macos" => Cow::Borrowed("MacOS"),
            "windows" => Cow::Borrowed("Windows"),
            "linux" => Cow::Borrowed("Linux"),
            "freebsd" => Cow::Borrowed("FreeBSD"),
            other => Cow::Owned(format!("Other::{other}")),
        };
        for (name, value) in [
            ("x-stainless-arch", arch.as_ref()),
            ("x-stainless-os", os.as_ref()),
            ("anthropic-beta", betas.as_str()),
            ("x-claude-code-session-id", self.session(request)),
        ] {
            headers.insert(
                name,
                HeaderValue::from_str(value).map_err(|_| {
                    Error::new(
                        ErrorKind::InvalidRequest,
                        "Invalid Claude Code session header",
                    )
                })?,
            );
        }
        Ok(())
    }
}

fn prefix_name(value: &mut Value) {
    if let Some(name) = value["name"].as_str()
        && !["web_search", "code_execution", "text_editor", "computer"]
            .iter()
            .any(|builtin| name.eq_ignore_ascii_case(builtin))
    {
        value["name"] = format!("_{name}").into();
    }
}

/// Called before known/opaque block decoding so extra tool-use fields survive.
pub(crate) fn decode_name(value: &mut Value) {
    if value["type"] == "tool_use"
        && let Some(name) = value["name"]
            .as_str()
            .and_then(|name| name.strip_prefix('_'))
    {
        value["name"] = name.to_owned().into();
    }
}

fn billing_header(first_user_text: &str) -> String {
    let mut sample = [u16::from(b'0'); 3];
    for (index, unit) in first_user_text.encode_utf16().take(21).enumerate() {
        if let Some(position) = [4, 7, 20].iter().position(|selected| *selected == index) {
            sample[position] = unit;
        }
    }
    // JS string indexing selects UTF-16 code units; hashing replaces unpaired
    // surrogates with U+FFFD when Node converts the resulting string to UTF-8.
    let mut context = Context::new(&SHA256);
    context.update(b"59cf53e54c78");
    context.update(String::from_utf16_lossy(&sample).as_bytes());
    context.update(VERSION.as_bytes());
    let hash = context.finish();
    let suffix = hex(&hash.as_ref()[..2]);
    format!(
        "x-anthropic-billing-header: cc_version={VERSION}.{}; cc_entrypoint=cli; {CCH_PLACEHOLDER};",
        &suffix[..3],
    )
}

fn serialize(mut body: Value) -> Result<Bytes, Error> {
    // Put system first without changing serde_json's global map ordering. This
    // guarantees the marker is at byte zero: even nested user-supplied `system`
    // objects or literal cch placeholders cannot be mistaken for system[0].
    #[derive(Serialize)]
    struct Payload<'a> {
        system: Value,
        #[serde(flatten)]
        body: &'a Value,
    }
    let system = body
        .as_object_mut()
        .expect("body is an object")
        .remove("system")
        .expect("subscription system exists");
    let mut bytes = serde_json::to_vec(&Payload {
        system,
        body: &body,
    })
    .map_err(|error| Error::new(ErrorKind::InvalidRequest, error.to_string()))?;
    if !bytes.starts_with(BILLING_PREFIX) {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "Claude Code billing block lost its serialization anchor",
        ));
    }
    // The billing block is ASCII and controlled here. Search only its text,
    // ending before the next quote, never arbitrary caller-supplied content.
    let tail = &bytes[BILLING_PREFIX.len()..];
    let end = tail
        .iter()
        .position(|byte| *byte == b'"')
        .expect("billing text ends");
    let relative = tail[..end]
        .windows(CCH_PLACEHOLDER.len())
        .position(|window| window == CCH_PLACEHOLDER.as_bytes())
        .expect("billing placeholder exists");
    let offset = BILLING_PREFIX.len() + relative + 4;
    let checksum = xxhash_rust::xxh64::xxh64(&bytes, CCH_SEED) & 0x000f_ffff;
    for index in 0..5 {
        bytes[offset + index] =
            b"0123456789abcdef"[((checksum >> ((4 - index) * 4)) & 15) as usize];
    }
    Ok(bytes.into())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_checksum_matches_independent_bun_fixture() {
        // Expected bytes computed independently with Bun.hash.xxHash64 over
        // UTF-8 JSON and seed 0x4d659218e32a3268n, not this Rust implementation.
        let body = json!({
            "system": [{"type":"text","text":"x-anthropic-billing-header: cch=00000;"}],
            "messages": [{"role":"user","content":[{"type":"text","text":"cch=00000"}]}],
        });
        let actual = serialize(body).unwrap();
        assert_eq!(
            actual.as_ref(),
            br#"{"system":[{"text":"x-anthropic-billing-header: cch=6c333;","type":"text"}],"messages":[{"content":[{"text":"cch=00000","type":"text"}],"role":"user"}]}"#,
        );
    }

    #[test]
    fn billing_version_uses_javascript_utf16_indexing() {
        // Independent node:crypto SHA256 fixtures, including sampled unpaired
        // surrogate halves that Node encodes as U+FFFD before hashing.
        for (text, suffix) in [
            ("", "077"),
            ("012345678901234567890", "32c"),
            ("💠a💠bc💠def💠ghijklmnop", "7be"),
        ] {
            assert_eq!(
                billing_header(text),
                format!(
                    "x-anthropic-billing-header: cc_version={VERSION}.{suffix}; cc_entrypoint=cli; cch=00000;",
                )
            );
        }
    }

    #[test]
    fn user_system_objects_cannot_steal_checksum_patch() {
        let spoof = json!({
            "system": [{"type":"text","text":"x-anthropic-billing-header: cch=00000;"}],
        });
        let body = json!({
            "system": [{"type":"text","text":"x-anthropic-billing-header: cch=00000;"}],
            "messages": [{"role":"assistant","content":[{
                "type":"tool_use","id":"call-1","name":"_read","input":spoof,
            }]}],
            "tools": [{"name":"_read","input_schema":spoof}],
        });
        let serialized = serialize(body).unwrap();
        let actual: Value = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(actual["messages"][0]["content"][0]["input"], spoof);
        assert_eq!(actual["tools"][0]["input_schema"], spoof);
        assert_ne!(
            actual["system"][0]["text"],
            "x-anthropic-billing-header: cch=00000;"
        );
    }
}
