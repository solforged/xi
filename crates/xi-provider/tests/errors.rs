//! HTTP, stream and decoding failure regression tests for both adapters.

mod common;

use common::*;
use serde_json::json;
use std::time::{Duration, SystemTime};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
use xi_protocol::event::{ModelRef, StopReason};
use xi_provider::{Api, Auth, ErrorKind};

#[tokio::test]
async fn both_adapters_classify_http_errors_and_retry_after() {
    for api in [Api::AnthropicMessages, Api::OpenaiChat] {
        for (status, message, kind) in [
            (401, "Invalid API key", ErrorKind::Auth),
            (403, "Forbidden", ErrorKind::Auth),
            (429, "Rate limit", ErrorKind::RateLimited),
            (529, "Overloaded", ErrorKind::Unavailable),
            (503, "Unavailable", ErrorKind::Unavailable),
            (500, "Internal error", ErrorKind::Unavailable),
            (502, "Bad gateway", ErrorKind::Unavailable),
            (
                400,
                "prompt is too long: 210000 tokens",
                ErrorKind::ContextOverflow,
            ),
            (
                400,
                "maximum context length exceeded",
                ErrorKind::ContextOverflow,
            ),
            (400, "Unknown model", ErrorKind::InvalidRequest),
            (404, "Not found", ErrorKind::InvalidRequest),
            (422, "Invalid input", ErrorKind::InvalidRequest),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(endpoint(api)))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("retry-after", "2.5")
                        .set_body_json(
                            json!({"error":{"type":"invalid_request_error","message":message}}),
                        ),
                )
                .mount(&server)
                .await;
            let provider = provider(&server, api, "test-model", &[], Auth::None);
            let error = provider.stream(&request(), &mut |_| {}).await.unwrap_err();
            assert_eq!(error.kind, kind, "{api:?}: {status}");
            assert_eq!(error.retry_after, Some(Duration::from_millis(2500)));
            assert_eq!(error.message, message);
        }
        let server = MockServer::start().await;
        let date = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60));
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", date)
                    .set_body_json(json!({"error":{"message":"Wait"}})),
            )
            .mount(&server)
            .await;
        let provider = provider(&server, api, "test-model", &[], Auth::None);
        let delay = provider
            .stream(&request(), &mut |_| {})
            .await
            .unwrap_err()
            .retry_after
            .unwrap();
        assert!(delay > Duration::from_secs(55) && delay <= Duration::from_secs(60));
    }
}

#[tokio::test]
async fn both_adapters_classify_in_stream_errors_after_partial_output() {
    for api in [Api::AnthropicMessages, Api::OpenaiChat] {
        for (error_type, message, kind) in [
            ("authentication_error", "Invalid key", ErrorKind::Auth),
            ("permission_error", "Forbidden", ErrorKind::Auth),
            ("rate_limit_error", "Slow down", ErrorKind::RateLimited),
            ("overloaded_error", "Overloaded", ErrorKind::Unavailable),
            ("api_error", "Internal error", ErrorKind::Unavailable),
            (
                "invalid_request_error",
                "prompt is too long",
                ErrorKind::ContextOverflow,
            ),
            (
                "invalid_request_error",
                "Bad input",
                ErrorKind::InvalidRequest,
            ),
            ("unknown_error", "Unrecognized error", ErrorKind::Decode),
        ] {
            let server = MockServer::start().await;
            let prefix = match api {
                Api::AnthropicMessages => anthropic(vec![
                    json!({"type":"message_start","message":{"usage":{}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Partial"}}),
                ]),
                Api::OpenaiChat => {
                    format!("data: {}\n\n", chunk(json!({"content":"Partial"}), None))
                }
                Api::OpenaiResponses => unreachable!("covered in tests/responses.rs"),
            };
            let error = json!({"type":"error","error":{"type":error_type,"message":message,"retry_after":3}});
            mock(
                &server,
                api,
                format!("{prefix}event: error\ndata: {error}\n\n"),
            )
            .await;
            let provider = provider(&server, api, "test-model", &[], Auth::None);
            let mut deltas = Vec::new();
            let error = provider
                .stream(&request(), &mut |delta| deltas.push(delta))
                .await
                .unwrap_err();
            assert_eq!(error.kind, kind, "{api:?}: {error_type}");
            assert_eq!(error.retry_after, Some(Duration::from_secs(3)));
            assert_eq!(deltas.len(), 1);
        }
    }
}

#[tokio::test]
async fn malformed_truncated_and_empty_streams_fail_instead_of_succeeding() {
    for api in [Api::AnthropicMessages, Api::OpenaiChat] {
        for (body, kind) in [
            ("data: {broken}\n\n".to_owned(), ErrorKind::Decode),
            ("data: {\"incomplete\":".to_owned(), ErrorKind::Network),
            (String::new(), ErrorKind::Network),
            (
                match api {
                    Api::AnthropicMessages => anthropic(vec![json!({"type":"message_stop"})]),
                    Api::OpenaiChat => "data: [DONE]\n\n".into(),
                    Api::OpenaiResponses => unreachable!("covered in tests/responses.rs"),
                },
                ErrorKind::Decode,
            ),
        ] {
            let server = MockServer::start().await;
            mock(&server, api, body).await;
            let provider = provider(&server, api, "test-model", &[], Auth::None);
            assert_eq!(
                provider
                    .stream(&request(), &mut |_| {})
                    .await
                    .unwrap_err()
                    .kind,
                kind
            );
        }
    }
}

#[tokio::test]
async fn malformed_tool_json_is_decode_error_for_each_adapter() {
    for api in [Api::AnthropicMessages, Api::OpenaiChat] {
        let server = MockServer::start().await;
        let body = match api {
            Api::AnthropicMessages => anthropic(vec![
                json!({"type":"message_start","message":{"usage":{}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call","name":"read","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{bad"}}),
                json!({"type":"content_block_stop","index":0}),
            ]),
            Api::OpenaiChat => openai(vec![chunk(
                json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"read","arguments":"{bad"}}]}),
                Some("tool_calls"),
            )]),
            Api::OpenaiResponses => unreachable!("covered in tests/responses.rs"),
        };
        mock(&server, api, body).await;
        let provider = provider(&server, api, "test-model", &[], Auth::None);
        assert_eq!(
            provider
                .stream(&request(), &mut |_| {})
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Decode
        );
    }
}

#[tokio::test]
async fn stop_reasons_are_not_conflated() {
    for (api, stops) in [
        (
            Api::AnthropicMessages,
            vec![
                ("end_turn", StopReason::EndTurn),
                ("stop_sequence", StopReason::EndTurn),
                ("pause_turn", StopReason::EndTurn),
                ("max_tokens", StopReason::MaxTokens),
                ("model_context_window_exceeded", StopReason::MaxTokens),
                ("refusal", StopReason::Refusal),
            ],
        ),
        (
            Api::OpenaiChat,
            vec![
                ("stop", StopReason::EndTurn),
                ("length", StopReason::MaxTokens),
                ("content_filter", StopReason::Refusal),
            ],
        ),
    ] {
        for (reason, expected) in stops {
            let server = MockServer::start().await;
            mock(
                &server,
                api,
                match api {
                    Api::AnthropicMessages => anthropic_text(reason),
                    Api::OpenaiChat => openai_text(reason),
                    Api::OpenaiResponses => unreachable!("covered in tests/responses.rs"),
                },
            )
            .await;
            let provider = provider(&server, api, "test-model", &[], Auth::None);
            assert_eq!(
                provider.stream(&request(), &mut |_| {}).await.unwrap().stop,
                expected
            );
        }
    }
}

#[tokio::test]
async fn abruptly_dropped_http_body_is_network_error() {
    use std::io::{Read, Write};
    for api in [Api::AnthropicMessages, Api::OpenaiChat] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::task::spawn_blocking(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut buffer = [0; 4096];
            assert!(socket.read(&mut buffer).unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 9999\r\nConnection: close\r\n\r\ndata: ").unwrap();
        });
        let registry = registry(
            format!("http://{address}"),
            api,
            "test-model",
            &[],
            Auth::None,
        );
        let provider = registry
            .connect(&ModelRef {
                provider: "test".into(),
                model: "test-model".into(),
            })
            .unwrap();
        assert_eq!(
            provider
                .stream(&request(), &mut |_| {})
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Network
        );
        server.await.unwrap();
    }
}
