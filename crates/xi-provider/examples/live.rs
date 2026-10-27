//! Live OpenAI-compatible smoke: stream text, invoke a tool, send its result.
//!
//! Run with `XI_BASE_URL=https://amarna.ussuri-alphard.ts.net/v1`
//! and `XI_MODEL=qwen3.8-27b-256k`. No credential is used.

use std::collections::BTreeMap;
use std::io::Write;

use serde_json::json;
use xi_protocol::model::{ModelInfo, Reasoning};
use xi_protocol::{
    Block, OutputBlock, ToolOutput,
    event::{ModelRef, StopReason},
    manifest::ToolSpec,
    methods::client::Delta,
};
use xi_provider::{
    Api, Auth, Message, ProviderConfig, Registry, RegistryConfig, Request, ToolResult,
};

fn print_delta(delta: Delta) {
    match delta {
        Delta::Text { text } => print!("{text}"),
        Delta::Thinking { text } => print!("[thinking] {text}"),
        Delta::ToolInput { call, partial_json } => print!("[tool {call}] {partial_json}"),
    }
    let _ = std::io::stdout().flush();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url = std::env::var("XI_BASE_URL")?;
    let model = std::env::var("XI_MODEL")?;
    let mut registry = Registry::default();
    registry.extend(RegistryConfig {
        provider: BTreeMap::from([(
            "live".into(),
            ProviderConfig {
                api: Api::OpenaiChat,
                base_url,
                auth: Auth::None,
                compat: Default::default(),
            },
        )]),
        model: vec![
            ModelInfo {
                provider: "live".into(),
                id: model.clone(),
                context_window: 262144,
                max_output: 4096,
                reasoning: Reasoning::ALL.to_vec(),
                pricing: None,
            }
            .into(),
        ],
        ..Default::default()
    });
    let provider = registry.connect(&ModelRef {
        provider: "live".into(),
        model: model.clone(),
    })?;
    let mut request = Request {
        model, system: "Follow instructions exactly. Use the supplied tool when asked to calculate. After a tool result, answer with the result and do not call it again.".into(),
        messages: vec![Message::User(vec![Block::Text { text: "Reply with exactly: Ready.".into() }])],
        tools: Vec::new(), max_tokens: 1024, reasoning: Reasoning::Off, cache_key: None,
    };
    println!("Text stream:");
    let reply = provider.stream(&request, &mut print_delta).await?;
    println!("\nstop={:?} usage={:?}", reply.stop, reply.usage);
    assert_eq!(reply.stop, StopReason::EndTurn);
    assert!(
        reply
            .content
            .iter()
            .any(|block| matches!(block, Block::Text { text } if !text.is_empty()))
    );
    request.messages.push(Message::Assistant(reply.content));
    request.messages.push(Message::User(vec![Block::Text { text: "Call add exactly once with a=19 and b=23. Do not calculate yourself. Wait for the tool result before answering.".into() }]));
    request.tools.push(ToolSpec {
        name: "add".into(), description: "Add two integers. Use this tool for arithmetic.".into(),
        input_schema: json!({"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"],"additionalProperties":false}),
    });
    println!("Tool stream:");
    let reply = provider.stream(&request, &mut print_delta).await?;
    println!("\nstop={:?} usage={:?}", reply.stop, reply.usage);
    assert_eq!(reply.stop, StopReason::ToolUse);
    let mut results = Vec::new();
    for block in &reply.content {
        if let Block::ToolUse {
            id, name, input, ..
        } = block
        {
            assert_eq!(name, "add");
            assert_eq!(input, &json!({"a":19,"b":23}));
            let sum = input["a"].as_i64().unwrap() + input["b"].as_i64().unwrap();
            results.push(ToolResult {
                call: id.clone(),
                output: ToolOutput {
                    content: vec![OutputBlock::Text {
                        text: sum.to_string(),
                    }],
                    is_error: false,
                    untrusted: false,
                },
            });
        }
    }
    assert_eq!(results.len(), 1);
    request.messages.push(Message::Assistant(reply.content));
    request.messages.push(Message::ToolResults(results));
    println!("Tool-result stream:");
    let reply = provider.stream(&request, &mut print_delta).await?;
    println!("\nstop={:?} usage={:?}", reply.stop, reply.usage);
    assert_eq!(reply.stop, StopReason::EndTurn);
    assert!(
        reply
            .content
            .iter()
            .any(|block| matches!(block, Block::Text { text } if text.contains("42")))
    );
    println!("PASS: text stream and one tool round trip completed.");
    Ok(())
}
