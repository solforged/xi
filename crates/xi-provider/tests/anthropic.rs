//! Anthropic wire-format and lossless history regression tests.

mod common;

use common::*;
use serde_json::{Value, json};
use wiremock::MockServer;
use xi_protocol::model::Reasoning;
use xi_protocol::{
    Block, OutputBlock, ProviderTag, Signature, ToolOutput,
    event::{StopReason, Usage},
    methods::client::Delta,
};
use xi_provider::{Api, Auth, Message, ToolResult};

fn transcript() -> String {
    anthropic(vec![
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":11,"output_tokens":1,"cache_read_input_tokens":1200,"cache_creation_input_tokens":800}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Inspect the files."}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"encrypted"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"query\":"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"Rust\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"web_search_tool_result","tool_use_id":"srv_1","content":[{"type":"web_search_result","url":"https://rust-lang.org/","title":"Rust","encrypted_content":"opaque"}]}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"Reading "}}),
        json!({"type":"ping"}),
        json!({"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"café."}}),
        json!({"type":"content_block_stop","index":4}),
        json!({"type":"content_block_start","index":5,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{},"caller":{"type":"direct"}}}),
        json!({"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/"}}),
        json!({"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"one.rs\"}"}}),
        json!({"type":"content_block_stop","index":5}),
        json!({"type":"content_block_start","index":6,"content_block":{"type":"tool_use","id":"call_2","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":6,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":6,"delta":{"type":"input_json_delta","partial_json":"\"two.rs\"}"}}),
        json!({"type":"content_block_stop","index":6}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":96}}),
        json!({"type":"message_stop"}),
    ])
}

#[tokio::test]
async fn signed_thinking_unknown_blocks_tools_usage_and_request_round_trip() {
    let server = MockServer::start().await;
    mock(&server, Api::AnthropicMessages, transcript()).await;
    let mut key = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut key, b"  secret-test-key\n").unwrap();
    let provider = provider(
        &server,
        Api::AnthropicMessages,
        "claude-sonnet-4-6",
        &Reasoning::ALL,
        Auth::File(key.path().into()),
    );
    let mut request = request();
    request.model = "claude-sonnet-4-6".into();
    request.messages = vec![Message::User(vec![
        Block::Text {
            text: "Inspect".into(),
        },
        Block::Image {
            media_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
        },
    ])];
    let mut deltas = Vec::new();
    let response = provider
        .stream(&request, &mut |delta| deltas.push(delta))
        .await
        .unwrap();
    assert_eq!(response.stop, StopReason::ToolUse);
    assert_eq!(
        response.usage,
        Usage {
            input: 11,
            output: 96,
            cache_read: 1200,
            cache_write: 800
        }
    );
    assert_eq!(
        response.content[0],
        Block::Thinking {
            text: "Inspect the files.".into(),
            signature: Some(Signature::Anthropic("sig-abc".into()))
        }
    );
    assert_eq!(
        response.content[1],
        Block::RedactedThinking {
            data: "encrypted".into()
        }
    );
    let server_call =
        json!({"type":"server_tool_use","id":"srv_1","name":"web_search","input":{"query":"Rust"}});
    assert_eq!(
        response.content[2],
        Block::Opaque {
            provider: ProviderTag::Anthropic,
            value: server_call.clone()
        }
    );
    assert!(
        matches!(&response.content[3], Block::Opaque { provider: ProviderTag::Anthropic, value } if value["content"][0]["encrypted_content"] == "opaque")
    );
    assert_eq!(
        response.content[4],
        Block::Text {
            text: "Reading café.".into()
        }
    );
    assert_eq!(
        response.content[5],
        Block::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: json!({"path":"src/one.rs"}),
            extra: std::collections::BTreeMap::from([(
                "anthropic".into(),
                json!({"caller":{"type":"direct"}}),
            )]),
        }
    );
    assert_eq!(
        response.content[6],
        Block::ToolUse {
            id: "call_2".into(),
            name: "read".into(),
            input: json!({"path":"two.rs"}),
            extra: Default::default(),
        }
    );
    assert_eq!(
        deltas,
        vec![
            Delta::Thinking {
                text: "Inspect the files.".into()
            },
            Delta::Text {
                text: "Reading ".into()
            },
            Delta::Text {
                text: "café.".into()
            },
            Delta::ToolInput {
                call: "call_1".into(),
                partial_json: "{\"path\":\"src/".into()
            },
            Delta::ToolInput {
                call: "call_1".into(),
                partial_json: "one.rs\"}".into()
            },
            Delta::ToolInput {
                call: "call_2".into(),
                partial_json: "{\"path\":".into()
            },
            Delta::ToolInput {
                call: "call_2".into(),
                partial_json: "\"two.rs\"}".into()
            },
        ]
    );
    request.messages.push(Message::Assistant(response.content));
    for (call, is_error) in [("call_1", false), ("call_2", true)] {
        request.messages.push(Message::ToolResults(vec![ToolResult {
            call: call.into(),
            output: ToolOutput {
                content: vec![
                    OutputBlock::Text {
                        text: "result".into(),
                    },
                    OutputBlock::Image {
                        media_type: "image/jpeg".into(),
                        data: "YWJj".into(),
                    },
                ],
                is_error,
                untrusted: false,
            },
        }]));
    }
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].headers["x-api-key"], "secret-test-key");
    assert_eq!(received[0].headers["anthropic-version"], "2023-06-01");
    let first: Value = received[0].body_json().unwrap();
    assert_eq!(
        first,
        json!({
            "model":"claude-sonnet-4-6", "max_tokens":8192,"stream":true,
            "system":[{"type":"text","text":"Be concise.","cache_control":{"type":"ephemeral"}}],
            "tools":[{"name":"read","description":"Read a file.","input_schema":request.tools[0].input_schema,"cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"text","text":"Inspect"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="},"cache_control":{"type":"ephemeral"}}]}],
            "thinking":{"type":"adaptive"},"output_config":{"effort":"medium"},
        })
    );
    let second: Value = received[1].body_json().unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 3);
    let assistant = &second["messages"][1];
    assert_eq!(
        assistant["content"][0],
        json!({"type":"thinking","thinking":"Inspect the files.","signature":"sig-abc"})
    );
    assert_eq!(
        assistant["content"][1],
        json!({"type":"redacted_thinking","data":"encrypted"})
    );
    assert_eq!(assistant["content"][2], server_call);
    assert_eq!(assistant["content"][5]["caller"], json!({"type":"direct"}));
    assert_eq!(
        assistant["content"][3]["content"][0]["encrypted_content"],
        "opaque"
    );
    assert_eq!(
        second["messages"][2],
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"call_1","is_error":false,"content":[{"type":"text","text":"result"},{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"YWJj"}}]},
            {"type":"tool_result","tool_use_id":"call_2","is_error":true,"content":[{"type":"text","text":"result"},{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"YWJj"}}],"cache_control":{"type":"ephemeral"}},
        ]})
    );
    assert!(
        second["messages"][0]["content"][1]
            .get("cache_control")
            .is_none()
    );
}

#[tokio::test]
async fn thinking_modes_budgets_and_cache_only_on_last_tool() {
    for (model, reasoning, max_tokens, expected, effort) in [
        (
            "claude-sonnet-4-6",
            Reasoning::Off,
            8192,
            json!({"type":"disabled"}),
            Some("low"),
        ),
        (
            "claude-opus-5-5",
            Reasoning::Max,
            8192,
            json!({"type":"adaptive","display":"summarized"}),
            Some("max"),
        ),
        (
            "claude-sonnet-5-5",
            Reasoning::Off,
            8192,
            json!({"type":"between_tools","display":"summarized"}),
            Some("low"),
        ),
        (
            "claude-fable-5-1",
            Reasoning::High,
            8192,
            json!({"type":"adaptive","display":"summarized"}),
            Some("high"),
        ),
        (
            "claude-haiku-4-5-20251001",
            Reasoning::High,
            4096,
            json!({"type":"enabled","budget_tokens":4095}),
            None,
        ),
        (
            "claude-opus-4-7",
            Reasoning::XHigh,
            8192,
            json!({"type":"adaptive"}),
            Some("xhigh"),
        ),
        (
            "claude-sonnet-4-5",
            Reasoning::XHigh,
            65536,
            json!({"type":"enabled","budget_tokens":32768}),
            None,
        ),
        (
            "claude-sonnet-4-5",
            Reasoning::Low,
            8192,
            json!({"type":"enabled","budget_tokens":1024}),
            None,
        ),
    ] {
        let server = MockServer::start().await;
        mock(&server, Api::AnthropicMessages, anthropic_text("end_turn")).await;
        let provider = provider(
            &server,
            Api::AnthropicMessages,
            model,
            &Reasoning::ALL,
            Auth::None,
        );
        let mut request = request();
        request.model = model.into();
        request.reasoning = reasoning;
        request.max_tokens = max_tokens;
        let mut second = request.tools[0].clone();
        second.name = "write".into();
        request.tools.push(second);
        provider.stream(&request, &mut |_| {}).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests[0].body_json().unwrap();
        assert_eq!(body["thinking"], expected, "{model}");
        assert_eq!(body["output_config"]["effort"].as_str(), effort, "{model}");
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type":"ephemeral"})
        );
    }
}

#[tokio::test]
async fn known_blocks_with_extra_fields_remain_lossless() {
    let server = MockServer::start().await;
    mock(&server, Api::AnthropicMessages, anthropic(vec![
        json!({"type":"message_start","message":{"usage":{}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Citation","citations":[]}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"https://example.com","cited_text":"source"}}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}),
        json!({"type":"message_stop"}),
    ])).await;
    let provider = provider(
        &server,
        Api::AnthropicMessages,
        "test-model",
        &[],
        Auth::None,
    );
    let mut request = request();
    let response = provider.stream(&request, &mut |_| {}).await.unwrap();
    let Block::Opaque { value, .. } = &response.content[0] else {
        panic!("citations must survive")
    };
    let expected = value.clone();
    request.messages.push(Message::Assistant(response.content));
    request.messages.push(Message::User(vec![Block::Text {
        text: "Next".into(),
    }]));
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[1].body_json().unwrap();
    assert_eq!(body["messages"][1]["content"][0], expected);
}
