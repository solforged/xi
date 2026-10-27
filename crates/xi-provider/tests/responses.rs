//! OpenAI Responses streaming, encrypted reasoning replay and ChatGPT signing.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use wiremock::MockServer;
use xi_protocol::event::{ModelRef, StopReason, Usage};
use xi_protocol::methods::client::Delta;
use xi_protocol::model::Reasoning;
use xi_protocol::{Block, OutputBlock, ProviderTag, Signature, ToolOutput};
use xi_provider::{Api, Auth, ErrorKind, Message, Registry, RegistryConfig, ToolResult};

fn sse(values: Vec<Value>) -> String {
    values
        .into_iter()
        .map(|value| {
            format!(
                "event: {}\ndata: {value}\n\n",
                value["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn completed(input: u64, cached: u64, output: u64) -> Value {
    json!({"type":"response.completed","response":{"id":"resp_1","usage":{
        "input_tokens":input,"input_tokens_details":{"cached_tokens":cached},
        "output_tokens":output,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":input + output}}})
}

fn transcript() -> String {
    sse(vec![
        json!({"type":"response.created","response":{"id":"resp_1"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
        json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":"Check the"}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":0,"delta":" file."}),
        json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}),
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","summary_index":1,"delta":"Then answer."}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1",
            "summary":[{"type":"summary_text","text":"Check the file."},{"type":"summary_text","text":"Then answer."}],
            "encrypted_content":"sealed-1"}}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"Reading "}),
        json!({"type":"response.output_text.delta","item_id":"msg_1","output_index":1,"content_index":0,"delta":"café."}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant",
            "content":[{"type":"output_text","text":"Reading café.","annotations":[]}]}}),
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"{\"path\":"}),
        json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":2,"delta":"\"src/one.rs\"}"}),
        json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"src/one.rs\"}"}}),
        completed(140, 100, 35),
    ])
}

#[tokio::test]
async fn listed_levels_are_sent_by_name_and_others_fail_before_sending() {
    for (api, response) in [
        (
            Api::OpenaiChat,
            openai(vec![chunk(json!({"content":"ok"}), Some("stop"))]),
        ),
        (Api::OpenaiResponses, sse(vec![completed(1, 0, 1)])),
    ] {
        let server = MockServer::start().await;
        mock(&server, api, response).await;
        let levels = [Reasoning::Off, Reasoning::Low, Reasoning::XHigh];
        let provider = provider(&server, api, "test-model", &levels, Auth::None);
        let mut request = request();
        for level in [Reasoning::Off, Reasoning::XHigh] {
            request.reasoning = level;
            provider.stream(&request, &mut |_| {}).await.unwrap();
        }
        request.reasoning = Reasoning::Max;
        let error = provider.stream(&request, &mut |_| {}).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Config, "{api:?}");
        assert!(error.message.contains("accepts off, low, xhigh"), "{api:?}");
        let efforts: Vec<_> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|received| {
                let body: Value = received.body_json().unwrap();
                match api {
                    Api::OpenaiChat => body["reasoning_effort"].clone(),
                    _ => body["reasoning"]["effort"].clone(),
                }
            })
            .collect();
        assert_eq!(efforts, ["none", "xhigh"], "{api:?}");
    }
}

#[tokio::test]
async fn reasoning_text_tools_and_usage_round_trip_statelessly() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiResponses, transcript()).await;
    let provider = provider(
        &server,
        Api::OpenaiResponses,
        "test-model",
        &Reasoning::ALL,
        Auth::None,
    );
    let mut request = request();
    request.cache_key = Some("session-affinity".into());
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
    let signature = Signature::Responses {
        id: "rs_1".into(),
        encrypted_content: "sealed-1".into(),
    };
    assert_eq!(
        response.content,
        vec![
            Block::Thinking {
                text: "Check the file.\n\nThen answer.".into(),
                signature: Some(signature.clone()),
            },
            Block::Text {
                text: "Reading café.".into()
            },
            Block::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({"path":"src/one.rs"}),
                extra: Default::default(),
            },
        ]
    );
    // Streamed deltas spell out the same thinking the final block holds.
    let thinking: String = deltas
        .iter()
        .filter_map(|delta| match delta {
            Delta::Thinking { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Check the file.\n\nThen answer.");
    assert!(deltas.contains(&Delta::ToolInput {
        call: "call_1".into(),
        partial_json: "\"src/one.rs\"}".into()
    }));

    request.messages.push(Message::Assistant(response.content));
    request.messages.push(Message::ToolResults(vec![ToolResult {
        call: "call_1".into(),
        output: ToolOutput {
            content: vec![
                OutputBlock::Text {
                    text: "fn one() {}".into(),
                },
                OutputBlock::Text {
                    text: "// end".into(),
                },
            ],
            is_error: false,
            untrusted: false,
        },
    }]));
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    let first: Value = received[0].body_json().unwrap();
    assert_eq!(
        first,
        json!({
            "model":"test-model","stream":true,"store":false,"tool_choice":"auto","parallel_tool_calls":true,
            "include":["reasoning.encrypted_content"],"max_output_tokens":8192,
            "reasoning":{"effort":"medium","summary":"auto"},"prompt_cache_key":"session-affinity",
            "input":[
                {"type":"message","role":"developer","content":[{"type":"input_text","text":"Be concise."}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]},
            ],
            "tools":[{"type":"function","name":"read","description":"Read a file.","parameters":request.tools[0].input_schema,"strict":false}],
        })
    );
    let second: Value = received[1].body_json().unwrap();
    assert_eq!(
        second["input"].as_array().unwrap()[2..],
        [
            json!({"type":"reasoning","id":"rs_1","encrypted_content":"sealed-1",
                "summary":[{"type":"summary_text","text":"Check the file.\n\nThen answer."}]}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Reading café.","annotations":[]}]}),
            json!({"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"src/one.rs\"}"}),
            json!({"type":"function_call_output","call_id":"call_1","output":"fn one() {}\n// end"}),
        ]
    );
}

#[tokio::test]
async fn other_providers_reasoning_is_left_out_of_the_replay() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiResponses, sse(vec![completed(1, 0, 1)])).await;
    let provider = provider(
        &server,
        Api::OpenaiResponses,
        "test-model",
        &Reasoning::ALL,
        Auth::None,
    );
    let mut request = request();
    request.messages.push(Message::Assistant(vec![
        Block::Thinking {
            text: "Anthropic reasoning".into(),
            signature: Some(Signature::Anthropic("EqQBCkYIBxgCKkA".into())),
        },
        Block::Thinking {
            text: "Chat Completions reasoning".into(),
            signature: None,
        },
        Block::RedactedThinking {
            data: "encrypted".into(),
        },
        Block::Opaque {
            provider: ProviderTag::Anthropic,
            value: json!({"type":"server_tool_use"}),
        },
        Block::Text {
            text: "Done.".into(),
        },
    ]));
    request.messages.push(Message::User(vec![Block::Text {
        text: "Next".into(),
    }]));
    let response = provider.stream(&request, &mut |_| {}).await.unwrap();
    assert_eq!(response.stop, StopReason::EndTurn);
    let body: Value = server.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(
        body["input"].as_array().unwrap()[2..],
        [
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done.","annotations":[]}]}),
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Next"}]}),
        ]
    );
}

#[tokio::test]
async fn failures_limits_and_truncation_are_classified() {
    for (events, expected) in [
        (
            vec![
                json!({"type":"response.failed","response":{"id":"resp_1","error":{
                "code":"rate_limit_exceeded","message":"Rate limit reached. Please try again in 1.5s."}}}),
            ],
            Err((ErrorKind::RateLimited, Some(Duration::from_millis(1500)))),
        ),
        (
            vec![json!({"type":"response.failed","response":{"error":{
                "code":"context_length_exceeded","message":"Your input exceeds the context window."}}})],
            Err((ErrorKind::ContextOverflow, None)),
        ),
        (
            vec![
                json!({"type":"error","error":{"code":"model_not_found","message":"Model unavailable"}}),
            ],
            Err((ErrorKind::InvalidRequest, None)),
        ),
        (
            vec![
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
            ],
            Ok(StopReason::MaxTokens),
        ),
        // A stream that stops without response.completed is not a success.
        (
            vec![json!({"type":"response.output_text.delta","delta":"Partial"})],
            Err((ErrorKind::Network, None)),
        ),
    ] {
        let server = MockServer::start().await;
        mock(&server, Api::OpenaiResponses, sse(events)).await;
        let provider = provider(&server, Api::OpenaiResponses, "test-model", &[], Auth::None);
        let result = provider.stream(&request(), &mut |_| {}).await;
        match expected {
            Ok(stop) => assert_eq!(result.unwrap().stop, stop),
            Err((kind, retry_after)) => {
                let error = result.unwrap_err();
                assert_eq!((error.kind, error.retry_after), (kind, retry_after));
            }
        }
    }
}

fn jwt(payload: Value) -> String {
    use base64::Engine;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).unwrap());
    format!("e30.{payload}.sig")
}

#[tokio::test]
async fn grok_account_requests_use_responses_and_token_routing() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiResponses, transcript()).await;
    let credentials = tempfile::tempdir().unwrap();
    let file = xi_auth::grok_file(credentials.path());
    std::fs::write(
        &file,
        json!({
            "access_token": "grok-access", "refresh_token": "grok-refresh",
            "expires_at": 4_102_444_800_u64,
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut registry = Registry::builtin();
    let mut config = registry.provider("grok").unwrap().clone();
    config.base_url = server.uri();
    registry.extend(RegistryConfig {
        provider: std::collections::BTreeMap::from([("grok".into(), config)]),
        ..Default::default()
    });
    registry.set_credentials(credentials.path().into());
    let mut request = request();
    request.model = "grok-4.7".into();
    let response = registry
        .connect(&ModelRef {
            provider: "grok".into(),
            model: request.model.clone(),
        })
        .unwrap()
        .stream(&request, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(response.stop, StopReason::ToolUse);
    assert!(
        response
            .content
            .iter()
            .any(|block| matches!(block, Block::ToolUse { name, .. } if name == "read"))
    );
    let received = &server.received_requests().await.unwrap()[0];
    assert_eq!(received.headers["authorization"], "Bearer grok-access");
    assert_eq!(received.headers["x-xai-token-auth"], "xai-grok-cli");
    assert!(!received.headers.contains_key("chatgpt-account-id"));
    let body: Value = received.body_json().unwrap();
    assert_eq!(body["model"], "grok-4.7");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["max_output_tokens"], request.max_tokens);
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["store"], false);
}

#[tokio::test]
async fn chatgpt_requests_are_signed_and_model_compat_overrides_the_provider() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiResponses, sse(vec![completed(1, 0, 1)])).await;
    let credentials = tempfile::tempdir().unwrap();
    let access = jwt(json!({"exp": 4_102_444_800_u64}));
    let file = xi_auth::chatgpt_file(credentials.path());
    std::fs::write(
        &file,
        json!({"access_token":access,"refresh_token":"r","id_token":"e30.e30.sig","account_id":"acct-1"})
            .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config: RegistryConfig = toml::from_str(&format!(
        r#"
        [provider.test]
        api = "openai-responses"
        base_url = "{}"
        auth = {{ login = "chatgpt" }}
        compat = {{ output_limit = false, session_header = "session-id" }}

        [[model]]
        provider = "test"
        id = "test-model"
        context_window = 32768
        max_output = 8192
        reasoning = ["low", "medium", "high"]

        [[model]]
        provider = "test"
        id = "lite-model"
        context_window = 272000
        max_output = 128000
        reasoning = ["low", "medium", "high"]
        compat = {{ responses_lite = true, output_limit = true }}
        "#,
        server.uri()
    ))
    .unwrap();
    let mut registry = Registry::default();
    registry.extend(config);
    let model = |id: &str| ModelRef {
        provider: "test".into(),
        model: id.into(),
    };
    assert_eq!(
        registry.connect(&model("test-model")).err().unwrap().kind,
        ErrorKind::Config,
        "ChatGPT auth needs a credential directory"
    );
    registry.set_credentials(credentials.path().into());
    let mut request = request();
    request.cache_key = Some("session-7".into());
    registry
        .connect(&model("test-model"))
        .unwrap()
        .stream(&request, &mut |_| {})
        .await
        .unwrap();
    let received = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        received.headers["authorization"],
        format!("Bearer {access}")
    );
    assert_eq!(received.headers["chatgpt-account-id"], "acct-1");
    assert_eq!(received.headers["originator"], "xi");
    assert_eq!(received.headers["session-id"], "session-7");
    let body: Value = received.body_json().unwrap();
    assert!(body.get("max_output_tokens").is_none());
    assert!(
        received
            .headers
            .get("x-openai-internal-codex-responses-lite")
            .is_none()
    );

    // The model turns on Lite and the output limit and keeps the provider's
    // session header.
    request.model = "lite-model".into();
    request.reasoning = Reasoning::Low;
    registry
        .connect(&model("lite-model"))
        .unwrap()
        .stream(&request, &mut |_| {})
        .await
        .unwrap();
    let received = &server.received_requests().await.unwrap()[1];
    assert_eq!(
        received.headers["x-openai-internal-codex-responses-lite"],
        "true"
    );
    assert_eq!(received.headers["session-id"], "session-7");
    let body: Value = received.body_json().unwrap();
    assert_eq!(body["max_output_tokens"], request.max_tokens);
    assert!(body.get("tools").is_none());
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(
        body["reasoning"],
        json!({"effort":"low","summary":"auto","context":"all_turns"})
    );
    assert_eq!(
        body["input"][0],
        json!({"type":"additional_tools","role":"developer","tools":[{
            "type":"namespace","name":"functions","description":"","tools":[{
                "type":"function","name":"read","description":"Read a file.",
                "parameters":request.tools[0].input_schema,"strict":false}]}]})
    );
    assert_eq!(body["input"][1]["role"], "developer");
}
