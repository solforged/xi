//! Opt-in Anthropic subscription transport and lossless history regressions.

mod common;

use std::sync::Arc;

use base64::Engine;
use common::*;
use serde_json::{Value, json};
use wiremock::MockServer;
use xi_protocol::{Block, Signature, event::ModelRef, model::Reasoning};
use xi_provider::{Api, Auth, LoginService, Message, Provider};

fn subscription(server: &MockServer, model: &str) -> (tempfile::TempDir, Arc<dyn Provider>) {
    let credentials = tempfile::tempdir().unwrap();
    let file = xi_auth::anthropic_file(credentials.path());
    std::fs::write(
        &file,
        json!({"access_token":"subscription-test-token","refresh_token":"refresh-test-token","expires_at":4_102_444_800_u64}).to_string(),
    ).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // A deliberately unrelated provider name: auth, not name or token prefix,
    // selects the subscription wire profile.
    let mut registry = registry(
        server.uri(),
        Api::AnthropicMessages,
        model,
        &Reasoning::ALL,
        Auth::Login(LoginService::Anthropic),
    );
    registry.set_credentials(credentials.path().into());
    let provider = registry
        .connect(&ModelRef {
            provider: "test".into(),
            model: model.into(),
        })
        .unwrap();
    (credentials, provider)
}

fn tool_transcript(names: &[&str]) -> String {
    let mut events = vec![json!({"type":"message_start","message":{"usage":{}}})];
    events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"keep","signature":"signed-history"}}));
    events.push(json!({"type":"content_block_stop","index":0}));
    events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque-encrypted"}}));
    events.push(json!({"type":"content_block_stop","index":1}));
    for (index, name) in names.iter().enumerate() {
        let mut block = json!({"type":"tool_use","id":format!("call-{index}"),"name":name,"input":{"path":"a"}});
        block["caller"] = json!({"type":"direct"});
        if index == 0 {
            block["future_field"] = json!({"signature":"opaque-tool-signature"});
        }
        events.push(json!({"type":"content_block_start","index":index + 2,"content_block":block}));
        events.push(json!({"type":"content_block_stop","index":index + 2}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}));
    events.push(json!({"type":"message_stop"}));
    anthropic(events)
}

#[tokio::test]
async fn tool_names_and_metadata_remain_executable_and_round_trip() {
    let server = MockServer::start().await;
    let internal = [
        "read",
        "_read",
        "_",
        "web_search",
        "CODE_EXECUTION",
        "text_editor",
        "computer",
        "web_search_extra",
    ];
    let wire = [
        "_read",
        "__read",
        "__",
        "web_search",
        "CODE_EXECUTION",
        "text_editor",
        "computer",
        "_web_search_extra",
    ];
    mock(&server, Api::AnthropicMessages, tool_transcript(&wire)).await;
    let (_credentials, provider) = subscription(&server, "claude-sonnet-4-6");
    let mut request = request();
    request.model = "claude-sonnet-4-6".into();
    request.cache_key = Some("session-7".into());
    let tool = request.tools[0].clone();
    request.tools = internal
        .iter()
        .map(|name| {
            let mut tool = tool.clone();
            tool.name = (*name).into();
            tool
        })
        .collect();
    let response = provider.stream(&request, &mut |_| {}).await.unwrap();
    assert_eq!(
        response.content[0],
        Block::Thinking {
            text: "keep".into(),
            signature: Some(Signature::Anthropic("signed-history".into())),
        }
    );
    assert_eq!(
        response.content[1],
        Block::RedactedThinking {
            data: "opaque-encrypted".into()
        }
    );
    for (index, expected) in internal.iter().enumerate() {
        let Block::ToolUse { name, extra, .. } = &response.content[index + 2] else {
            panic!("tool metadata must not hide an executable call");
        };
        assert_eq!(name.as_str(), *expected);
        assert_eq!(extra["anthropic"]["caller"], json!({"type":"direct"}));
    }
    request.messages.push(Message::Assistant(response.content));
    request.messages.push(Message::ToolResults(
        internal
            .iter()
            .enumerate()
            .map(|(index, _)| xi_provider::ToolResult {
                call: format!("call-{index}").as_str().into(),
                output: xi_protocol::ToolOutput {
                    content: vec![],
                    is_error: false,
                    untrusted: false,
                },
            })
            .collect(),
    ));
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].url.query(), Some("beta=true"));
    assert_eq!(
        received[0].headers["authorization"],
        "Bearer subscription-test-token"
    );
    assert!(!received[0].headers.contains_key("x-api-key"));
    assert_eq!(received[0].headers["x-claude-code-session-id"], "session-7");
    assert_eq!(
        received[0].headers["x-stainless-package-version"],
        "0.112.1"
    );
    assert_eq!(received[0].headers["accept"], "application/json");
    assert_eq!(received[0].headers["x-app"], "cli");
    let first: Value = received[0].body_json().unwrap();
    let second: Value = received[1].body_json().unwrap();
    for (index, expected) in wire.iter().enumerate() {
        assert_eq!(first["tools"][index]["name"], *expected);
        assert_eq!(
            second["messages"][1]["content"][index + 2]["name"],
            *expected
        );
        assert_eq!(
            second["messages"][1]["content"][index + 2]["caller"],
            json!({"type":"direct"}),
        );
    }
    assert_eq!(
        second["messages"][1]["content"][0]["signature"],
        "signed-history"
    );
    assert_eq!(
        second["messages"][1]["content"][1]["data"],
        "opaque-encrypted"
    );
    assert_eq!(
        second["messages"][1]["content"][2]["future_field"]["signature"],
        "opaque-tool-signature"
    );
    assert!(
        first["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("x-anthropic-billing-header:")
    );
    assert_eq!(
        first["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert_eq!(
        first["system"][1]["cache_control"],
        json!({"type":"ephemeral"})
    );
    assert_eq!(
        first["system"][2],
        json!({"type":"text","text":"Be concise."})
    );
    let user: Value = serde_json::from_str(first["metadata"]["user_id"].as_str().unwrap()).unwrap();
    let next: Value =
        serde_json::from_str(second["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(user, next);
    assert_eq!(user["session_id"], "session-7");
    assert_eq!(user["device_id"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn subscription_clamps_before_budgeting_and_keeps_fallback_session() {
    let server = MockServer::start().await;
    mock(&server, Api::AnthropicMessages, anthropic_text("end_turn")).await;
    let (_credentials, provider) = subscription(&server, "claude-haiku-4-5-20251001");
    let mut request = request();
    request.model = "claude-haiku-4-5-20251001".into();
    request.max_tokens = 128000;
    request.reasoning = Reasoning::Max;
    request.tools.clear();
    for _ in 0..2 {
        provider.stream(&request, &mut |_| {}).await.unwrap();
    }
    request.reasoning = Reasoning::Off;
    provider.stream(&request, &mut |_| {}).await.unwrap();
    let received = server.received_requests().await.unwrap();
    let first: Value = received[0].body_json().unwrap();
    let second: Value = received[1].body_json().unwrap();
    assert_eq!(first["max_tokens"], 64000);
    assert_eq!(first["thinking"]["budget_tokens"], 63999);
    assert_eq!(first["tools"], json!([]));
    assert_eq!(first["metadata"], second["metadata"]);
    let first_beta = received[0].headers["anthropic-beta"].to_str().unwrap();
    assert!(first_beta.contains("claude-code-20250219"));
    assert!(first_beta.contains("effort-2025-11-24"));
    assert!(first_beta.contains("fallback-credit-2026-06-01"));
    assert!(!first_beta.contains("context-1m"));
    let utility_beta = received[2].headers["anthropic-beta"].to_str().unwrap();
    assert!(utility_beta.contains("structured-outputs-2025-12-15"));
    assert!(!utility_beta.contains("claude-code-20250219"));
    assert!(!utility_beta.contains("effort-2025-11-24"));
}

#[tokio::test]
async fn api_key_that_looks_like_oauth_does_not_enable_subscription_wire() {
    let server = MockServer::start().await;
    mock(&server, Api::AnthropicMessages, tool_transcript(&["_read"])).await;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b"sk-ant-oat01-test-not-a-real-token").unwrap();
    let provider = provider(
        &server,
        Api::AnthropicMessages,
        "claude-sonnet-4-6",
        &Reasoning::ALL,
        Auth::File(file.path().into()),
    );
    let mut request = request();
    request.model = "claude-sonnet-4-6".into();
    request.max_tokens = 128000;
    request.tools[0].name = "_read".into();
    let response = provider.stream(&request, &mut |_| {}).await.unwrap();
    assert!(matches!(&response.content[2], Block::ToolUse { name, .. } if name == "_read"));
    let received = server.received_requests().await.unwrap();
    let sent = &received[0];
    assert_eq!(sent.url.query(), None);
    assert_eq!(
        sent.headers["x-api-key"],
        "sk-ant-oat01-test-not-a-real-token"
    );
    for name in [
        "authorization",
        "anthropic-beta",
        "x-app",
        "x-stainless-lang",
    ] {
        assert!(!sent.headers.contains_key(name));
    }
    let body: Value = sent.body_json().unwrap();
    assert_eq!(body["max_tokens"], 128000);
    assert_eq!(body["tools"][0]["name"], "_read");
    assert!(body.get("metadata").is_none());
    assert_eq!(
        body["system"],
        json!([{"type":"text","text":"Be concise.","cache_control":{"type":"ephemeral"}}])
    );
}

#[tokio::test]
async fn subscription_stream_decodes_advertised_compression() {
    for (encoding, payload) in [
        (
            "gzip",
            "H4sIAAAAAAAA/42QwQrCMAyG73sKyXmI5159kBKXIOKWljaDyei7m+qmDj14SsL///lICBXdbga9RQYHA+eMZ/ZZMSm06wxuhnFpSilNQ9tYF0RZ1J/60F1f4YsQT+AO7Vavy5ac8lSNj+LgGIaYDMi0hz8gIX4wvu3rKcS9ojmf1dg16RNjDmI2FvI6JvlFfH/DWCbfASvkKdEtAQAA",
        ),
        (
            "deflate",
            "eJyNkMEKwjAMhu97Csl5iOdefZASlyDilpY2g8nou5vqpg49eErC///5SAgV3W4GvUUGBwPnjGf2WTEptOsMboZxaUopTUPbWBdEWdSf+tBdX+GLEE/gDu1Wr8uWnPJUjY/i4BiGmAzItIc/ICF+ML7t6ynEvaI5n9XYNekTYw5iNhbyOib5RXx/w1gm3wFvl2eR",
        ),
    ] {
        let server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("content-encoding", encoding)
                    .set_body_bytes(
                        base64::engine::general_purpose::STANDARD
                            .decode(payload)
                            .unwrap(),
                    ),
            )
            .mount(&server)
            .await;
        let (_credentials, provider) = subscription(&server, "claude-sonnet-4-6");
        let mut request = request();
        request.model = "claude-sonnet-4-6".into();
        let response = provider.stream(&request, &mut |_| {}).await.unwrap();
        assert_eq!(
            response.content,
            vec![Block::Text {
                text: "Compressed.".into()
            }],
            "{encoding}",
        );
    }
}
