use std::collections::BTreeMap;

use serde_json::{Value, json};
use xi_protocol::OutputBlock;

use crate::sse::Events;
use crate::transport::{api_error, decode};
use crate::*;

pub(crate) fn body(request: &Request, reasoning: bool) -> Result<Value, Error> {
    let cache = json!({"type": "ephemeral"});
    let mut messages: Vec<Value> = Vec::with_capacity(request.messages.len());
    let mut previous_results = false;
    for message in &request.messages {
        match message {
            Message::User(blocks) | Message::Assistant(blocks) => {
                let role = if matches!(message, Message::User(_)) {
                    "user"
                } else {
                    "assistant"
                };
                let content = blocks.iter().map(block).collect::<Result<Vec<_>, _>>()?;
                messages.push(json!({"role": role, "content": content}));
                previous_results = false;
            }
            Message::ToolResults(results) => {
                let content: Vec<_> = results
                    .iter()
                    .map(|result| {
                        json!({
                            "type": "tool_result", "tool_use_id": result.call,
                            "is_error": result.output.is_error,
                            "content": result.output.content.iter().map(output).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                if previous_results {
                    messages.last_mut().expect("previous tool results exist")["content"]
                        .as_array_mut()
                        .expect("content is an array")
                        .extend(content);
                } else {
                    messages.push(json!({"role": "user", "content": content}));
                }
                previous_results = true;
            }
        }
    }
    if let Some(last) = messages
        .last_mut()
        .and_then(|message| message["content"].as_array_mut())
        .and_then(|blocks| blocks.last_mut())
    {
        last["cache_control"] = cache.clone();
    }
    let mut body = json!({
        "model": request.model, "max_tokens": request.max_tokens, "stream": true,
        "messages": messages,
    });
    if !request.system.is_empty() {
        body["system"] = json!([{"type": "text", "text": request.system, "cache_control": cache}]);
    }
    if !request.tools.is_empty() {
        let mut tools: Vec<_> = request.tools.iter().map(|tool| json!({
            "name": tool.name, "description": tool.description, "input_schema": tool.input_schema,
        })).collect();
        tools.last_mut().expect("tools are nonempty")["cache_control"] = cache;
        body["tools"] = tools.into();
    }
    if reasoning {
        configure_thinking(&mut body, request)?;
    }
    Ok(body)
}

// https://platform.claude.com/docs/en/build-with-claude/thinking
// https://platform.claude.com/docs/en/build-with-claude/effort
// 4.6+ uses adaptive thinking. 4.5 and earlier use explicit budgets.
// Mandatory-thinking models clamp Off to low effort instead of failing a run.
fn configure_thinking(body: &mut Value, request: &Request) -> Result<(), Error> {
    let mut parts = request
        .model
        .strip_prefix("claude-")
        .unwrap_or(&request.model)
        .split('-');
    let family = parts.next().unwrap_or_default();
    let major = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let adaptive = matches!(family, "fable" | "mythos")
        || major >= 5
        || (major == 4 && minor >= 6 && matches!(family, "opus" | "sonnet"));
    let always_on = matches!(family, "fable" | "mythos")
        || (family == "opus" && (major > 5 || (major == 5 && minor >= 5)));
    let between_tools = family == "sonnet" && major == 5 && minor >= 5;
    if request.reasoning == Reasoning::Off {
        if always_on {
            body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            body["output_config"] = json!({"effort": "low"});
        } else if between_tools {
            body["thinking"] = json!({"type": "between_tools", "display": "summarized"});
            body["output_config"] = json!({"effort": "low"});
        } else {
            body["thinking"] = json!({"type": "disabled"});
            if adaptive {
                body["output_config"] = json!({"effort": "low"});
            }
        }
    } else if adaptive {
        body["thinking"] = if major >= 5 || matches!(family, "fable" | "mythos") {
            json!({"type": "adaptive", "display": "summarized"})
        } else {
            json!({"type": "adaptive"})
        };
        body["output_config"] = json!({"effort": effort(request.reasoning)});
    } else {
        let requested = match request.reasoning {
            Reasoning::Low => 1024,
            Reasoning::Medium => 4096,
            Reasoning::High => 16384,
            Reasoning::Off => unreachable!(),
        };
        let budget = requested.min(request.max_tokens.saturating_sub(1));
        if budget < 1024 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Extended thinking requires max_tokens greater than 1024",
            ));
        }
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
    Ok(())
}

pub(crate) fn effort(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Off => "none",
        Reasoning::Low => "low",
        Reasoning::Medium => "medium",
        Reasoning::High => "high",
    }
}

fn block(block: &Block) -> Result<Value, Error> {
    Ok(match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::Image { media_type, data } => image(media_type, data),
        Block::Thinking {
            text,
            signature: Some(signature),
        } => json!({"type": "thinking", "thinking": text, "signature": signature}),
        Block::Thinking {
            signature: None, ..
        } => {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Anthropic thinking requires its original signature",
            ));
        }
        Block::RedactedThinking { data } => json!({"type": "redacted_thinking", "data": data}),
        Block::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        Block::Opaque { provider, value } if provider == "anthropic" && value.is_object() => {
            value.clone()
        }
        Block::Opaque { .. } => {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Cannot replay a foreign or malformed opaque block to Anthropic",
            ));
        }
    })
}

fn output(block: &OutputBlock) -> Value {
    match block {
        OutputBlock::Text { text } => json!({"type": "text", "text": text}),
        OutputBlock::Image { media_type, data } => image(media_type, data),
    }
}

fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
}

struct Content {
    value: Value,
    input: String,
    stopped: bool,
}

pub(crate) async fn consume(
    response: reqwest::Response,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, Error> {
    let mut events = Events::new(response);
    let mut blocks: BTreeMap<u64, Content> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop = None;
    let mut started = false;
    while let Some(event) = events.next().await? {
        let mut value: Value =
            serde_json::from_str(&event.data).map_err(|error| decode(error.to_string()))?;
        let kind = value["type"].as_str().unwrap_or(&event.name).to_owned();
        match kind.as_str() {
            "ping" => {}
            "error" => return Err(api_error(None, &value)),
            "message_start" => {
                if started {
                    return Err(decode("Duplicate Anthropic message_start"));
                }
                started = true;
                update_usage(&mut usage, &value["message"]["usage"]);
            }
            "content_block_start" => {
                let index = index(&value)?;
                let raw = value["content_block"].take();
                if !raw.is_object() || raw["type"].as_str().is_none() || blocks.contains_key(&index)
                {
                    return Err(decode("Invalid Anthropic content block start"));
                }
                if let Some(text) = raw["text"].as_str().filter(|text| !text.is_empty()) {
                    on_delta(Delta::Text {
                        text: text.to_owned(),
                    });
                }
                if let Some(text) = raw["thinking"].as_str().filter(|text| !text.is_empty()) {
                    on_delta(Delta::Thinking {
                        text: text.to_owned(),
                    });
                }
                blocks.insert(
                    index,
                    Content {
                        value: raw,
                        input: String::new(),
                        stopped: false,
                    },
                );
            }
            "content_block_delta" => {
                let index = index(&value)?;
                let content = blocks
                    .get_mut(&index)
                    .filter(|block| !block.stopped)
                    .ok_or_else(|| decode("Delta has no open Anthropic content block"))?;
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = required_str(delta, "text")?;
                        append(&mut content.value, "text", text)?;
                        on_delta(Delta::Text {
                            text: text.to_owned(),
                        });
                    }
                    Some("thinking_delta") => {
                        let text = required_str(delta, "thinking")?;
                        append(&mut content.value, "thinking", text)?;
                        on_delta(Delta::Thinking {
                            text: text.to_owned(),
                        });
                    }
                    Some("signature_delta") => append(
                        &mut content.value,
                        "signature",
                        required_str(delta, "signature")?,
                    )?,
                    Some("input_json_delta") => {
                        let partial = required_str(delta, "partial_json")?;
                        content.input.push_str(partial);
                        if content.value["type"] == "tool_use" {
                            on_delta(Delta::ToolInput {
                                call: required_str(&content.value, "id")?.into(),
                                partial_json: partial.to_owned(),
                            });
                        }
                    }
                    Some("citations_delta") => {
                        if content.value.get("citations").is_none() {
                            content.value["citations"] = json!([]);
                        }
                        content.value["citations"]
                            .as_array_mut()
                            .ok_or_else(|| decode("Invalid citation array"))?
                            .push(
                                delta
                                    .get("citation")
                                    .ok_or_else(|| decode("Missing citation"))?
                                    .clone(),
                            );
                    }
                    _ => return Err(decode("Unsupported Anthropic content delta")),
                }
            }
            "content_block_stop" => {
                let index = index(&value)?;
                let content = blocks
                    .get_mut(&index)
                    .filter(|block| !block.stopped)
                    .ok_or_else(|| decode("Stop has no open Anthropic content block"))?;
                if !content.input.is_empty() {
                    content.value["input"] = serde_json::from_str(&content.input)
                        .map_err(|error| decode(error.to_string()))?;
                }
                content.stopped = true;
            }
            "message_delta" => {
                update_usage(&mut usage, &value["usage"]);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    stop = Some(match reason {
                        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                        "refusal" => StopReason::Refusal,
                        _ => {
                            return Err(decode(format!("Unknown Anthropic stop reason: {reason}")));
                        }
                    });
                }
            }
            "message_stop" => {
                if !started || blocks.values().any(|block| !block.stopped) {
                    return Err(decode(
                        "Anthropic message stopped before its blocks completed",
                    ));
                }
                return Ok(Response {
                    content: blocks
                        .into_values()
                        .map(|block| decode_block(block.value))
                        .collect::<Result<_, _>>()?,
                    stop: stop.ok_or_else(|| decode("Anthropic message has no stop reason"))?,
                    usage,
                });
            }
            _ => {} // The API may add non-content events; they do not change the message.
        }
    }
    Err(Error::new(
        ErrorKind::Network,
        "Anthropic stream ended before message_stop",
    ))
}

fn index(value: &Value) -> Result<u64, Error> {
    value["index"]
        .as_u64()
        .ok_or_else(|| decode("Missing content block index"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value[key]
        .as_str()
        .ok_or_else(|| decode(format!("Missing string field: {key}")))
}

fn append(value: &mut Value, key: &str, text: &str) -> Result<(), Error> {
    if value.get(key).is_none() {
        value[key] = Value::String(String::new());
    }
    match &mut value[key] {
        Value::String(existing) => {
            existing.push_str(text);
            Ok(())
        }
        _ => Err(decode(format!("Invalid streamed string field: {key}"))),
    }
}

fn update_usage(usage: &mut Usage, value: &Value) {
    for (key, target) in [
        ("input_tokens", &mut usage.input),
        ("output_tokens", &mut usage.output),
        ("cache_read_input_tokens", &mut usage.cache_read),
        ("cache_creation_input_tokens", &mut usage.cache_write),
    ] {
        if let Some(tokens) = value[key].as_u64() {
            *target = tokens;
        }
    }
}

fn decode_block(mut value: Value) -> Result<Block, Error> {
    let fields: &[&str] = match value["type"].as_str() {
        Some("text") => &["type", "text"],
        Some("thinking") => &["type", "thinking", "signature"],
        Some("redacted_thinking") => &["type", "data"],
        Some("tool_use") => &["type", "id", "name", "input"],
        _ => &[],
    };
    // Even known blocks become opaque when extra fields would otherwise be lost.
    if fields.is_empty()
        || value
            .as_object()
            .expect("content is an object")
            .keys()
            .any(|key| !fields.contains(&key.as_str()))
    {
        return Ok(Block::Opaque {
            provider: "anthropic".into(),
            value,
        });
    }
    match required_str(&value, "type")? {
        "text" => Ok(Block::Text {
            text: take_string(&mut value, "text")?,
        }),
        "thinking" => Ok(Block::Thinking {
            text: take_string(&mut value, "thinking")?,
            signature: Some(take_string(&mut value, "signature")?),
        }),
        "redacted_thinking" => Ok(Block::RedactedThinking {
            data: take_string(&mut value, "data")?,
        }),
        "tool_use" => {
            let id = ToolCallId(take_string(&mut value, "id")?);
            let name = take_string(&mut value, "name")?;
            let input = value
                .get_mut("input")
                .ok_or_else(|| decode("Tool use has no input"))?
                .take();
            Ok(Block::ToolUse { id, name, input })
        }
        _ => unreachable!(),
    }
}

fn take_string(value: &mut Value, key: &str) -> Result<String, Error> {
    match value[key].take() {
        Value::String(text) => Ok(text),
        _ => Err(decode(format!("Missing string field: {key}"))),
    }
}
