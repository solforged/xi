//! OpenAI-compatible streaming and request projection regression tests.

mod common;

use common::*;
use serde_json::{Value, json};
use wiremock::MockServer;
use xi_protocol::model::Reasoning;
use xi_protocol::{
    Block, OutputBlock, ToolOutput,
    event::{StopReason, Usage},
    methods::client::Delta,
};
use xi_provider::{Api, Auth, Message, ToolResult};

#[tokio::test]
async fn indexed_tools_reasoning_usage_and_request_round_trip() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiChat, openai(vec![
        chunk(json!({"role":"assistant","reasoning_content":"Inspect "}), None),
        chunk(json!({"reasoning_content":"both files."}), None),
        chunk(json!({"content":"Reading."}), None),
        chunk(json!({"tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}},
            {"index":1,"id":"call_2","type":"function","function":{"name":"read","arguments":"{\"path\":\""}},
        ]}), None),
        chunk(json!({"tool_calls":[
            {"index":1,"function":{"arguments":"two.rs\"}"}},
            {"index":0,"function":{"arguments":"\"one.rs\"}"}},
        ]}), None),
        chunk(json!({}), Some("tool_calls")),
        json!({"id":"chatcmpl-test","choices":[],"usage":{"prompt_tokens":140,"completion_tokens":35,"prompt_tokens_details":{"cached_tokens":100},"completion_tokens_details":{"reasoning_tokens":20}}}),
    ])).await;
    let mut key = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut key, b" bearer-key \n").unwrap();
    let provider = provider(
        &server,
        Api::OpenaiChat,
        "test-model",
        &Reasoning::ALL,
        Auth::File(key.path().into()),
    );
    let mut request = request();
    request.cache_key = Some("session-affinity".into());
    request.messages = vec![Message::User(vec![
        Block::Text {
            text: "Inspect".into(),
        },
        Block::Image {
            media_type: "image/png".into(),
            data: "YWJj".into(),
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
            input: 40,
            output: 35,
            cache_read: 100,
            cache_write: 0
        }
    );
    assert_eq!(
        response.content,
        vec![
            Block::Thinking {
                text: "Inspect both files.".into(),
                signature: None
            },
            Block::Text {
                text: "Reading.".into()
            },
            Block::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({"path":"one.rs"}),
                extra: Default::default(),
            },
            Block::ToolUse {
                id: "call_2".into(),
                name: "read".into(),
                input: json!({"path":"two.rs"}),
                extra: Default::default(),
            },
        ]
    );
    assert_eq!(
        deltas,
        vec![
            Delta::Thinking {
                text: "Inspect ".into()
            },
            Delta::Thinking {
                text: "both files.".into()
            },
            Delta::Text {
                text: "Reading.".into()
            },
            Delta::ToolInput {
                call: "call_1".into(),
                partial_json: "{\"path\":".into()
            },
            Delta::ToolInput {
                call: "call_2".into(),
                partial_json: "{\"path\":\"".into()
            },
            Delta::ToolInput {
                call: "call_2".into(),
                partial_json: "two.rs\"}".into()
            },
            Delta::ToolInput {
                call: "call_1".into(),
                partial_json: "\"one.rs\"}".into()
            },
        ]
    );
    request.messages.push(Message::Assistant(response.content));
    request.messages.push(Message::ToolResults(vec![ToolResult {
        call: "call_1".into(),
        output: ToolOutput {
            content: vec![OutputBlock::Text {
                text: "File contents".into(),
            }],
            is_error: false,
            untrusted: false,
        },
    }]));
    request.messages.push(Message::ToolResults(vec![ToolResult {
        call: "call_2".into(),
        output: ToolOutput {
            content: vec![OutputBlock::Text {
                text: "Not found".into(),
            }],
            is_error: true,
            untrusted: false,
        },
    }]));
    request.reasoning = Reasoning::Off;
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].headers["authorization"], "Bearer bearer-key");
    let first: Value = received[0].body_json().unwrap();
    assert_eq!(
        first,
        json!({
            "model":"test-model","max_tokens":8192,"stream":true,"stream_options":{"include_usage":true},
            "reasoning_effort":"medium","prompt_cache_key":"session-affinity",
            "messages":[{"role":"system","content":"Be concise."},{"role":"user","content":[{"type":"text","text":"Inspect"},{"type":"image_url","image_url":{"url":"data:image/png;base64,YWJj"}}]}],
            "tools":[{"type":"function","function":{"name":"read","description":"Read a file.","parameters":request.tools[0].input_schema}}],
        })
    );
    let second: Value = received[1].body_json().unwrap();
    assert_eq!(second["reasoning_effort"], "none");
    assert_eq!(
        second["chat_template_kwargs"],
        json!({"enable_thinking":false})
    );
    assert_eq!(
        second["messages"][2],
        json!({"role":"assistant","content":[{"type":"text","text":"Reading."}],"tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"one.rs\"}"}},
            {"id":"call_2","type":"function","function":{"name":"read","arguments":"{\"path\":\"two.rs\"}"}},
        ]})
    );
    assert_eq!(
        second["messages"][3],
        json!({"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"File contents"}]})
    );
    assert_eq!(
        second["messages"][4],
        json!({"role":"tool","tool_call_id":"call_2","content":[{"type":"text","text":"Not found"}]})
    );
}

#[tokio::test]
async fn alternate_reasoning_field_is_not_replayed_and_auth_is_optional() {
    let server = MockServer::start().await;
    mock(
        &server,
        Api::OpenaiChat,
        openai(vec![
            chunk(json!({"reasoning":"Consider."}), None),
            chunk(json!({"content":"Answer"}), Some("stop")),
        ]),
    )
    .await;
    let provider = provider(&server, Api::OpenaiChat, "test-model", &[], Auth::None);
    let mut request = request();
    let response = provider.stream(&request, &mut |_| {}).await.unwrap();
    assert_eq!(
        response.content[0],
        Block::Thinking {
            text: "Consider.".into(),
            signature: None
        }
    );
    request.messages.push(Message::Assistant(response.content));
    request.reasoning = Reasoning::Off;
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert!(!received[0].headers.contains_key("authorization"));
    let second: Value = received[1].body_json().unwrap();
    assert!(second.get("reasoning_effort").is_none());
    assert_eq!(second["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(
        second["messages"][2],
        json!({"role":"assistant","content":[{"type":"text","text":"Answer"}]})
    );
}

#[tokio::test]
async fn modern_openai_output_limit_and_delayed_call_id() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiChat, openai(vec![
        chunk(json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}), None),
        chunk(json!({"tool_calls":[{"index":0,"id":"late_id","function":{"name":"re","arguments":"\"file\"}"}}]}), None),
        chunk(json!({"tool_calls":[{"index":0,"function":{"name":"ad"}}]}), Some("tool_calls")),
    ])).await;
    let provider = provider(
        &server,
        Api::OpenaiChat,
        "gpt-5.5",
        &Reasoning::ALL,
        Auth::None,
    );
    let mut request = request();
    request.model = "gpt-5.5".into();
    let mut deltas = Vec::new();
    let response = provider
        .stream(&request, &mut |delta| deltas.push(delta))
        .await
        .unwrap();
    assert_eq!(
        response.content,
        vec![Block::ToolUse {
            id: "late_id".into(),
            name: "read".into(),
            input: json!({"path":"file"}),
            extra: Default::default(),
        }]
    );
    assert_eq!(
        deltas,
        vec![Delta::ToolInput {
            call: "late_id".into(),
            partial_json: "{\"path\":\"file\"}".into()
        }]
    );
    let received = server.received_requests().await.unwrap();
    let body: Value = received[0].body_json().unwrap();
    assert_eq!(body["max_completion_tokens"], 8192);
    assert!(body.get("max_tokens").is_none());
}
