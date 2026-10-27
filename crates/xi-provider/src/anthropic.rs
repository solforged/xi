use std::collections::BTreeMap;

use serde_json::{Value, json};
use xi_protocol::OutputBlock;

use crate::sse::Events;
use crate::transport::{api_error, decode};
use crate::*;

pub(crate) fn body(request: &Request, reasoning: bool, max_tokens: u32) -> Result<Value, Error> {
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
                let content = blocks
                    .iter()
                    .filter_map(|content| block(content).transpose())
                    .collect::<Result<Vec<_>, _>>()?;
                if content.is_empty() {
                    continue;
                }
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
        "model": request.model, "max_tokens": max_tokens, "stream": true,
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
        configure_thinking(&mut body, request, max_tokens)?;
    }
    Ok(body)
}

// https://platform.claude.com/docs/en/build-with-claude/thinking
// https://platform.claude.com/docs/en/build-with-claude/effort
// 4.6+ uses adaptive thinking. 4.5 and earlier use explicit budgets. Models
// that always think don't list `off`, so it never reaches them.
fn configure_thinking(body: &mut Value, request: &Request, max_tokens: u32) -> Result<(), Error> {
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
    let between_tools = family == "sonnet" && major == 5 && minor >= 5;
    if request.reasoning == Reasoning::Off {
        if between_tools {
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
        body["output_config"] = json!({"effort": request.reasoning.name()});
    } else {
        let requested = match request.reasoning {
            Reasoning::Off => unreachable!(),
            Reasoning::Minimal | Reasoning::Low => 1024,
            Reasoning::Medium => 4096,
            Reasoning::High => 16384,
            Reasoning::XHigh => 32768,
            Reasoning::Max => 65536,
        };
        let budget = requested.min(max_tokens.saturating_sub(1));
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

fn block(block: &Block) -> Result<Option<Value>, Error> {
    Ok(Some(match block {
        Block::Text { text } => json!({"type": "text", "text": text}),
        Block::Image { media_type, data } => image(media_type, data),
        Block::Thinking {
            text,
            signature: Some(signature),
        } => json!({"type": "thinking", "thinking": text, "signature": signature}),
        Block::Thinking {
            signature: None, ..
        } => {
            // Cancelled streams and other providers can leave unsigned thinking
            // in history. Keep it in the log, but Anthropic cannot replay it.
            return Ok(None);
        }
        Block::RedactedThinking { data } => json!({"type": "redacted_thinking", "data": data}),
        Block::ToolUse {
            id,
            name,
            input,
            extra,
        } => {
            let mut value = extra
                .get("anthropic")
                .filter(|value| value.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            // Interpreted fields are authoritative over retained wire metadata.
            value["type"] = "tool_use".into();
            value["id"] = json!(id);
            value["name"] = name.clone().into();
            value["input"] = input.clone();
            value
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
    }))
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
    subscription: bool,
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
                        .map(|mut block| {
                            if subscription {
                                crate::claude_code::decode_name(&mut block.value);
                            }
                            decode_block(block.value)
                        })
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
    // Additional wire fields do not change the semantics of a client tool call.
    // Preserve them without hiding the call from execution and history repair.
    if value["type"] == "tool_use" {
        let id = ToolCallId(take_string(&mut value, "id")?);
        let name = take_string(&mut value, "name")?;
        let input = value
            .get_mut("input")
            .ok_or_else(|| decode("Tool use has no input"))?
            .take();
        let fields = value.as_object_mut().expect("tool_use is an object");
        for key in ["type", "id", "name", "input"] {
            fields.remove(key);
        }
        let extra = if fields.is_empty() {
            BTreeMap::new()
        } else {
            BTreeMap::from([("anthropic".into(), value)])
        };
        return Ok(Block::ToolUse {
            id,
            name,
            input,
            extra,
        });
    }
    let fields: &[&str] = match value["type"].as_str() {
        Some("text") => &["type", "text"],
        Some("thinking") => &["type", "thinking", "signature"],
        Some("redacted_thinking") => &["type", "data"],
        _ => &[],
    };
    // Non-actionable known blocks remain opaque if extra fields would be lost.
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
        _ => unreachable!(),
    }
}

fn take_string(value: &mut Value, key: &str) -> Result<String, Error> {
    match value[key].take() {
        Value::String(text) => Ok(text),
        _ => Err(decode(format!("Missing string field: {key}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(messages: Vec<Message>) -> Request {
        Request {
            model: "claude-sonnet-4-6".into(),
            system: String::new(),
            messages,
            tools: Vec::new(),
            max_tokens: 8192,
            reasoning: Reasoning::Medium,
            cache_key: None,
        }
    }

    #[test]
    fn unsigned_thinking_only_turns_are_omitted() {
        let unsigned = Block::Thinking {
            text: "Unfinished reasoning".into(),
            signature: None,
        };
        let request = request(vec![
            Message::User(vec![Block::Text {
                text: "Hello".into(),
            }]),
            Message::Assistant(vec![unsigned.clone()]),
            Message::User(vec![Block::Text {
                text: "Resume".into(),
            }]),
            Message::Assistant(vec![unsigned]),
        ]);
        let history = request.messages.clone();
        let body = body(&request, true, request.max_tokens).unwrap();
        assert_eq!(request.messages, history);
        assert_eq!(
            body["messages"],
            json!([
                {"role":"user","content":[{"type":"text","text":"Hello"}]},
                {"role":"user","content":[{"type":"text","text":"Resume","cache_control":{"type":"ephemeral"}}]},
            ])
        );
    }

    #[test]
    fn unsigned_thinking_is_omitted_without_losing_replayable_blocks() {
        let request = request(vec![
            Message::User(vec![Block::Text {
                text: "Hello".into(),
            }]),
            Message::Assistant(vec![
                Block::Thinking {
                    text: "Unsigned reasoning".into(),
                    signature: None,
                },
                Block::Thinking {
                    text: "Signed reasoning".into(),
                    signature: Some("original-signature".into()),
                },
                Block::RedactedThinking {
                    data: "encrypted".into(),
                },
                Block::Text {
                    text: "Reading".into(),
                },
                Block::ToolUse {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: json!({"path":"a.rs"}),
                    extra: Default::default(),
                },
            ]),
        ]);
        let history = request.messages.clone();
        let body = body(&request, true, request.max_tokens).unwrap();
        assert_eq!(request.messages, history);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["messages"][1],
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"Signed reasoning","signature":"original-signature"},
                {"type":"redacted_thinking","data":"encrypted"},
                {"type":"text","text":"Reading"},
                {"type":"tool_use","id":"call_1","name":"read","input":{"path":"a.rs"},"cache_control":{"type":"ephemeral"}},
            ]})
        );
    }

    #[test]
    fn tool_metadata_stays_executable_and_round_trips_without_a_server() {
        for metadata in [
            json!({}),
            json!({"caller":{"type":"direct"}, "future_field":[1, 2]}),
        ] {
            let mut wire = metadata;
            wire["type"] = json!("tool_use");
            wire["id"] = json!("call");
            wire["name"] = json!("read");
            wire["input"] = json!({"path":"a"});
            let call = decode_block(wire.clone()).unwrap();
            assert!(matches!(call, Block::ToolUse { .. }));
            assert_eq!(block(&call).unwrap(), Some(wire.clone()));
            let persisted: Block =
                serde_json::from_value(serde_json::to_value(&call).unwrap()).unwrap();
            assert_eq!(block(&persisted).unwrap(), Some(wire));
        }
    }

    #[test]
    fn retained_metadata_cannot_override_interpreted_tool_fields() {
        let call = Block::ToolUse {
            id: "call".into(),
            name: "read".into(),
            input: json!({"path":"a"}),
            extra: BTreeMap::from([
                (
                    "anthropic".into(),
                    json!({
                        "type":"other", "id":"wrong", "name":"wrong", "input":null,
                        "caller":{"type":"direct"},
                    }),
                ),
                ("other".into(), json!({"foreign":true})),
            ]),
        };
        assert_eq!(
            block(&call).unwrap(),
            Some(json!({
                "type":"tool_use", "id":"call", "name":"read", "input":{"path":"a"},
                "caller":{"type":"direct"},
            }))
        );
    }
}
