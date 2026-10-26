//! The OpenAI Responses API, as served by `api.openai.com` and by the
//! ChatGPT Codex backend.
//!
//! Requests are stateless (`store: false`). Reasoning comes back encrypted
//! and is kept as [`Block::Thinking`] whose signature is the JSON object
//! `{"id", "encrypted_content"}`, so it can be replayed on the next request.
//! Thinking from other providers has no such signature and is left out.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde_json::{Value, json};
use xi_protocol::OutputBlock;

use crate::sse::Events;
use crate::transport::{Shape, decode};
use crate::*;

pub(crate) fn body(request: &Request, shape: &Shape) -> Result<Value, Error> {
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function", "name": tool.name, "description": tool.description,
                "parameters": tool.input_schema, "strict": false,
            })
        })
        .collect();
    let mut input = Vec::with_capacity(request.messages.len() + 2);
    if shape.lite && !tools.is_empty() {
        input.push(json!({
            "type": "additional_tools", "role": "developer",
            "tools": [{"type": "namespace", "name": "functions", "description": "", "tools": tools}],
        }));
    }
    if !request.system.is_empty() {
        input.push(json!({
            "type": "message", "role": "developer",
            "content": [{"type": "input_text", "text": request.system}],
        }));
    }
    for message in &request.messages {
        match message {
            Message::User(blocks) => {
                let content: Vec<_> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(json!({"type": "input_text", "text": text})),
                        Block::Image { media_type, data } => Some(image(media_type, data)),
                        _ => None,
                    })
                    .collect();
                if !content.is_empty() {
                    input.push(json!({"type": "message", "role": "user", "content": content}));
                }
            }
            Message::Assistant(blocks) => assistant(blocks, shape.lite, &mut input),
            Message::ToolResults(results) => {
                for result in results {
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": result.call,
                        "output": tool_output(&result.output.content),
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": request.model,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": !shape.lite,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if !shape.lite && !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if shape.reasoning {
        let mut reasoning = match request.reasoning {
            Reasoning::Off => {
                json!({"effort": crate::anthropic::effort(shape.reasoning_off)})
            }
            depth => json!({"effort": crate::anthropic::effort(depth), "summary": "auto"}),
        };
        if shape.lite {
            reasoning["context"] = "all_turns".into();
        }
        body["reasoning"] = reasoning;
    }
    if shape.output_limit {
        body["max_output_tokens"] = request.max_tokens.into();
    }
    if let Some(key) = &request.cache_key {
        body["prompt_cache_key"] = key.clone().into();
    }
    Ok(body)
}

/// Appends one assistant turn: text runs become messages, and reasoning and
/// tool calls become their own items, in the original order. Lite models
/// mark text before a tool call as commentary and the rest as the answer.
fn assistant(blocks: &[Block], lite: bool, input: &mut Vec<Value>) {
    let last_call = blocks
        .iter()
        .rposition(|block| matches!(block, Block::ToolUse { .. }));
    let mut text = Vec::new();
    let flush = |text: &mut Vec<Value>, input: &mut Vec<Value>, at: usize| {
        if !text.is_empty() {
            let mut message =
                json!({"type": "message", "role": "assistant", "content": std::mem::take(text)});
            if lite {
                let before_call = last_call.is_some_and(|call| call >= at);
                message["phase"] = if before_call {
                    "commentary"
                } else {
                    "final_answer"
                }
                .into();
            }
            input.push(message);
        }
    };
    for (at, block) in blocks.iter().enumerate() {
        match block {
            Block::Text { text: part } => {
                text.push(json!({"type": "output_text", "text": part, "annotations": []}));
            }
            Block::Thinking {
                text: summary,
                signature: Some(signature),
            } => {
                let Some(sealed) = serde_json::from_str::<Value>(signature)
                    .ok()
                    .filter(|value| value["encrypted_content"].is_string())
                else {
                    continue; // Another provider's signature.
                };
                flush(&mut text, input, at);
                let mut item = json!({
                    "type": "reasoning",
                    "summary": if summary.is_empty() {
                        json!([])
                    } else {
                        json!([{"type": "summary_text", "text": summary}])
                    },
                    "encrypted_content": sealed["encrypted_content"],
                });
                if let Some(id) = sealed["id"].as_str() {
                    item["id"] = id.into();
                }
                input.push(item);
            }
            Block::ToolUse {
                id,
                name,
                input: arguments,
            } => {
                flush(&mut text, input, at);
                input.push(json!({
                    "type": "function_call", "call_id": id, "name": name,
                    "arguments": arguments.to_string(),
                }));
            }
            Block::Opaque { provider, value } if provider == "openai" && value.is_object() => {
                flush(&mut text, input, at);
                input.push(value.clone());
            }
            // Unsigned or foreign reasoning and other providers' blocks.
            Block::Thinking { .. }
            | Block::RedactedThinking { .. }
            | Block::Opaque { .. }
            | Block::Image { .. } => {}
        }
    }
    flush(&mut text, input, blocks.len());
}

fn tool_output(content: &[OutputBlock]) -> Value {
    if content
        .iter()
        .all(|block| matches!(block, OutputBlock::Text { .. }))
    {
        let text: Vec<&str> = content
            .iter()
            .map(|block| match block {
                OutputBlock::Text { text } => text.as_str(),
                OutputBlock::Image { .. } => unreachable!("all blocks are text"),
            })
            .collect();
        return text.join("\n").into();
    }
    content
        .iter()
        .map(|block| match block {
            OutputBlock::Text { text } => json!({"type": "input_text", "text": text}),
            OutputBlock::Image { media_type, data } => image(media_type, data),
        })
        .collect::<Vec<_>>()
        .into()
}

fn image(media_type: &str, data: &str) -> Value {
    json!({"type": "input_image", "image_url": format!("data:{media_type};base64,{data}")})
}

pub(crate) async fn consume(
    response: reqwest::Response,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, Error> {
    let mut events = Events::new(response);
    let mut items: BTreeMap<u64, Value> = BTreeMap::new();
    // Function call item ids to call ids, for argument deltas.
    let mut calls: HashMap<String, String> = HashMap::new();
    while let Some(event) = events.next().await? {
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            continue;
        };
        match value["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(text) = value["delta"].as_str().filter(|text| !text.is_empty()) {
                    on_delta(Delta::Text { text: text.into() });
                }
            }
            "response.reasoning_summary_part.added"
                if value["summary_index"].as_u64() > Some(0) =>
            {
                on_delta(Delta::Thinking {
                    text: "\n\n".into(),
                });
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(text) = value["delta"].as_str().filter(|text| !text.is_empty()) {
                    on_delta(Delta::Thinking { text: text.into() });
                }
            }
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                if let (Some(item), Some(call)) = (
                    value["item"]["id"].as_str(),
                    value["item"]["call_id"].as_str(),
                ) {
                    calls.insert(item.into(), call.into());
                }
            }
            "response.function_call_arguments.delta" => {
                if let (Some(call), Some(delta)) = (
                    value["item_id"].as_str().and_then(|item| calls.get(item)),
                    value["delta"].as_str(),
                ) {
                    on_delta(Delta::ToolInput {
                        call: ToolCallId(call.clone()),
                        partial_json: delta.into(),
                    });
                }
            }
            "response.output_item.done" => {
                let index = value["output_index"].as_u64().unwrap_or(items.len() as u64);
                items.insert(index, value["item"].clone());
            }
            "response.completed" => {
                return finish(items, &value["response"], None);
            }
            "response.incomplete" => {
                let stop = match value["response"]["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => StopReason::MaxTokens,
                    Some("content_filter") => StopReason::Refusal,
                    reason => {
                        return Err(Error::new(
                            ErrorKind::Unavailable,
                            format!(
                                "The response stopped early: {}",
                                reason.unwrap_or("no reason given")
                            ),
                        ));
                    }
                };
                return finish(items, &value["response"], Some(stop));
            }
            "response.failed" => return Err(failed(&value["response"]["error"])),
            "error" => return Err(failed(value.get("error").unwrap_or(&value))),
            _ => {}
        }
    }
    Err(Error::new(
        ErrorKind::Network,
        "Responses stream ended before response.completed",
    ))
}

fn finish(
    items: BTreeMap<u64, Value>,
    response: &Value,
    stop: Option<StopReason>,
) -> Result<Response, Error> {
    let mut content = Vec::with_capacity(items.len());
    for item in items.into_values() {
        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                let text: String = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
                    .collect();
                if !text.is_empty() {
                    content.push(Block::Text { text });
                }
            }
            "reasoning" => {
                let text = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let signature = item["encrypted_content"].as_str().map(|sealed| {
                    let mut value = json!({"encrypted_content": sealed});
                    if let Some(id) = item["id"].as_str() {
                        value["id"] = id.into();
                    }
                    value.to_string()
                });
                if !text.is_empty() || signature.is_some() {
                    content.push(Block::Thinking { text, signature });
                }
            }
            "function_call" => {
                let (Some(id), Some(name)) = (item["call_id"].as_str(), item["name"].as_str())
                else {
                    return Err(decode("Incomplete Responses function call"));
                };
                let arguments = item["arguments"].as_str().unwrap_or_default();
                let input = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(arguments).map_err(|error| decode(error.to_string()))?
                };
                content.push(Block::ToolUse {
                    id: ToolCallId(id.into()),
                    name: name.into(),
                    input,
                });
            }
            _ => content.push(Block::Opaque {
                provider: "openai".into(),
                value: item,
            }),
        }
    }
    let stop = stop.unwrap_or_else(|| {
        if content
            .iter()
            .any(|block| matches!(block, Block::ToolUse { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }
    });
    let usage = &response["usage"];
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    Ok(Response {
        content,
        stop,
        usage: Usage {
            input: usage["input_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cached),
            output: usage["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            cache_write: 0,
        },
    })
}

/// Classifies an in-stream error object by its `code`.
fn failed(error: &Value) -> Error {
    let code = error["code"].as_str().unwrap_or_default();
    let message = error["message"]
        .as_str()
        .filter(|message| !message.is_empty())
        .unwrap_or(code);
    let kind = match code {
        "rate_limit_exceeded" | "slow_down" | "usage_limit_reached" => ErrorKind::RateLimited,
        "context_length_exceeded" => ErrorKind::ContextOverflow,
        "server_is_overloaded" | "server_error" | "" => ErrorKind::Unavailable,
        _ => ErrorKind::InvalidRequest,
    };
    Error {
        kind,
        message: if message.is_empty() {
            "The Responses stream failed".into()
        } else {
            message.into()
        },
        retry_after: try_again_in(message),
    }
}

/// Parses "try again in 11.054s" or "try again in 250ms".
fn try_again_in(message: &str) -> Option<Duration> {
    let lower = message.to_ascii_lowercase();
    let rest = &lower[lower.find("try again in ")? + "try again in ".len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let number: f64 = rest[..end].parse().ok()?;
    let seconds = if rest[end..].starts_with("ms") {
        number / 1000.0
    } else {
        number
    };
    Duration::try_from_secs_f64(seconds).ok()
}
