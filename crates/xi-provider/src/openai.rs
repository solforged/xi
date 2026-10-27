use std::collections::BTreeMap;

use serde_json::{Value, json};
use xi_protocol::OutputBlock;

use crate::sse::Events;
use crate::transport::{api_error, decode};
use crate::*;

pub(crate) fn body(request: &Request, reasoning: bool) -> Result<Value, Error> {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        match message {
            Message::User(blocks) | Message::Assistant(blocks) => {
                let role = if matches!(message, Message::User(_)) {
                    "user"
                } else {
                    "assistant"
                };
                let mut content = Vec::new();
                let mut calls = Vec::new();
                for block in blocks {
                    match block {
                        Block::Text { text } => content.push(json!({"type": "text", "text": text})),
                        Block::Image { media_type, data } => content.push(image(media_type, data)),
                        Block::ToolUse { id, name, input, .. } => calls.push(json!({
                            "id": id, "type": "function", "function": {"name": name, "arguments": input.to_string()},
                        })),
                        Block::Thinking { .. } => {} // Reasoning is never replayed on Chat Completions.
                        Block::RedactedThinking { .. } | Block::Opaque { .. } => return Err(Error::new(
                            ErrorKind::InvalidRequest, "Chat Completions cannot replay provider-specific blocks",
                        )),
                    }
                }
                let mut message = json!({"role": role, "content": if content.is_empty() { Value::Null } else { content.into() }});
                if !calls.is_empty() {
                    message["tool_calls"] = calls.into();
                }
                messages.push(message);
            }
            Message::ToolResults(results) => {
                for result in results {
                    let content: Vec<_> = result
                        .output
                        .content
                        .iter()
                        .map(|block| match block {
                            OutputBlock::Text { text } => json!({"type": "text", "text": text}),
                            OutputBlock::Image { media_type, data } => image(media_type, data),
                        })
                        .collect();
                    // Chat Completions has no is_error field. Preserve the actual output.
                    messages.push(
                        json!({"role": "tool", "tool_call_id": result.call, "content": content}),
                    );
                }
            }
        }
    }
    if request.web_search {
        return Err(Error::new(
            ErrorKind::Config,
            "Chat Completions has no hosted web search",
        ));
    }
    let mut body = json!({
        "model": request.model, "messages": messages, "stream": true,
        "stream_options": {"include_usage": true},
    });
    let modern_openai = request.model.starts_with("gpt-5")
        || request.model.starts_with("o1")
        || request.model.starts_with("o3")
        || request.model.starts_with("o4");
    body[if modern_openai {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }] = request.max_tokens.into();
    if reasoning {
        body["reasoning_effort"] = effort(request.reasoning).into();
    }
    if request.reasoning == Reasoning::Off {
        body["chat_template_kwargs"] = json!({"enable_thinking": false});
    }
    if !request.tools.is_empty() {
        body["tools"] = request.tools.iter().map(|tool| json!({
            "type": "function", "function": {"name": tool.name, "description": tool.description, "parameters": tool.input_schema},
        })).collect::<Vec<_>>().into();
    }
    if let Some(key) = &request.cache_key {
        body["prompt_cache_key"] = key.clone().into();
    }
    Ok(body)
}

/// The OpenAI effort for a level: `none` for off, otherwise its name.
pub(crate) fn effort(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Off => "none",
        level => level.name(),
    }
}

fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}")}})
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
    emitted: usize,
}

enum Content {
    Text(String),
    Thinking(String),
    Tool(u64),
}

pub(crate) async fn consume(
    response: reqwest::Response,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, Error> {
    let mut events = Events::new(response);
    let mut content = Vec::new();
    let mut calls: BTreeMap<u64, Call> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut stop = None;
    while let Some(event) = events.next().await? {
        if event.data.trim() == "[DONE]" {
            let stop = stop.ok_or_else(|| decode("OpenAI stream has no finish reason"))?;
            return Ok(Response {
                content: finish(content, calls)?,
                stop,
                usage,
            });
        }
        let value: Value =
            serde_json::from_str(&event.data).map_err(|error| decode(error.to_string()))?;
        if event.name == "error" || value.get("error").is_some() {
            return Err(api_error(None, &value));
        }
        if let Some(value) = value.get("usage").filter(|value| !value.is_null()) {
            let cached = value["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            usage.cache_read = cached;
            usage.input = value["prompt_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cached);
            usage.output = value["completion_tokens"].as_u64().unwrap_or(0);
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| decode("OpenAI chunk has no choices array"))?;
        for choice in choices {
            if choice["index"].as_u64() != Some(0) {
                continue;
            }
            let delta = &choice["delta"];
            for key in ["reasoning_content", "reasoning"] {
                if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
                    match content.last_mut() {
                        Some(Content::Thinking(existing)) => existing.push_str(text),
                        _ => content.push(Content::Thinking(text.to_owned())),
                    }
                    on_delta(Delta::Thinking {
                        text: text.to_owned(),
                    });
                    break;
                }
            }
            for key in ["content", "refusal"] {
                if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
                    match content.last_mut() {
                        Some(Content::Text(existing)) => existing.push_str(text),
                        _ => content.push(Content::Text(text.to_owned())),
                    }
                    on_delta(Delta::Text {
                        text: text.to_owned(),
                    });
                }
            }
            if let Some(parts) = delta["tool_calls"].as_array() {
                for part in parts {
                    let index = part["index"]
                        .as_u64()
                        .ok_or_else(|| decode("Tool delta has no index"))?;
                    let call = calls.entry(index).or_insert_with(|| {
                        content.push(Content::Tool(index));
                        Call::default()
                    });
                    if let Some(id) = part["id"].as_str() {
                        call.id.push_str(id);
                    }
                    if let Some(name) = part["function"]["name"].as_str() {
                        call.name.push_str(name);
                    }
                    if let Some(arguments) = part["function"]["arguments"].as_str() {
                        call.arguments.push_str(arguments);
                    }
                    if !call.id.is_empty() && call.emitted < call.arguments.len() {
                        on_delta(Delta::ToolInput {
                            call: ToolCallId(call.id.clone()),
                            partial_json: call.arguments[call.emitted..].to_owned(),
                        });
                        call.emitted = call.arguments.len();
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                stop = Some(match reason {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => return Err(decode(format!("Unknown OpenAI finish reason: {reason}"))),
                });
            }
        }
    }
    Err(Error::new(
        ErrorKind::Network,
        "OpenAI stream ended before [DONE]",
    ))
}

fn finish(content: Vec<Content>, mut calls: BTreeMap<u64, Call>) -> Result<Vec<Block>, Error> {
    content
        .into_iter()
        .map(|block| {
            Ok(match block {
                Content::Text(text) => Block::Text { text },
                Content::Thinking(text) => Block::Thinking {
                    text,
                    signature: None,
                },
                Content::Tool(index) => {
                    let call = calls.remove(&index).expect("every tool slot has a call");
                    if call.id.is_empty() || call.name.is_empty() {
                        return Err(decode("Incomplete OpenAI tool call"));
                    }
                    let input = serde_json::from_str(&call.arguments)
                        .map_err(|error| decode(error.to_string()))?;
                    Block::ToolUse {
                        id: ToolCallId(call.id),
                        name: call.name,
                        input,
                        extra: Default::default(),
                    }
                }
            })
        })
        .collect()
}
