#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
use xi_protocol::model::{ModelInfo, Reasoning};
use xi_protocol::{Block, event::ModelRef, manifest::ToolSpec};
use xi_provider::{Api, Auth, Provider, ProviderConfig, Registry, RegistryConfig, Request};

pub fn request() -> Request {
    Request {
        model: "test-model".into(),
        system: "Be concise.".into(),
        messages: vec![xi_provider::Message::User(vec![Block::Text {
            text: "Hello".into(),
        }])],
        tools: vec![ToolSpec {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }],
        max_tokens: 8192,
        reasoning: Reasoning::Medium,
        cache_key: None,
    }
}

pub fn provider(
    server: &MockServer,
    api: Api,
    model: &str,
    reasoning: &[Reasoning],
    auth: Auth,
) -> Arc<dyn Provider> {
    registry(server.uri(), api, model, reasoning, auth)
        .connect(&ModelRef {
            provider: "test".into(),
            model: model.into(),
        })
        .unwrap()
}

pub fn registry(
    base_url: String,
    api: Api,
    model: &str,
    reasoning: &[Reasoning],
    auth: Auth,
) -> Registry {
    let mut registry = Registry::default();
    registry.extend(RegistryConfig {
        provider: BTreeMap::from([(
            "test".into(),
            ProviderConfig {
                api,
                base_url,
                auth,
                compat: Default::default(),
            },
        )]),
        model: vec![
            ModelInfo {
                provider: "test".into(),
                id: model.into(),
                context_window: 32768,
                max_output: 8192,
                reasoning: reasoning.to_vec(),
                pricing: None,
            }
            .into(),
        ],
        ..Default::default()
    });
    registry
}

pub fn endpoint(api: Api) -> &'static str {
    match api {
        Api::AnthropicMessages => "/v1/messages",
        Api::OpenaiChat => "/chat/completions",
        Api::OpenaiResponses => "/responses",
    }
}

pub async fn mock(server: &MockServer, api: Api, body: String) {
    Mock::given(method("POST"))
        .and(path(endpoint(api)))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

pub fn anthropic(values: Vec<Value>) -> String {
    let mut body = ": keepalive\r\n\r\n".to_owned();
    for value in values {
        body.push_str(&format!(
            "event: {}\r\ndata: {value}\r\n\r\n",
            value["type"].as_str().unwrap()
        ));
    }
    body
}

pub fn anthropic_text(stop: &str) -> String {
    anthropic(vec![
        json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","content":[],"usage":{"input_tokens":12,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ])
}

pub fn openai(values: Vec<Value>) -> String {
    let mut body = String::new();
    for value in values {
        body.push_str(&format!("data: {value}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

pub fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1792998000,"model":"test-model","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}

pub fn openai_text(stop: &str) -> String {
    openai(vec![
        chunk(json!({"role":"assistant","content":"Hello"}), None),
        chunk(json!({}), Some(stop)),
    ])
}
