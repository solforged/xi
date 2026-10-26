//! Registry layering and credential boundary regression tests.

mod common;

use common::*;
use serde_json::Value;
use std::collections::BTreeMap;
use wiremock::MockServer;
use xi_protocol::event::ModelRef;
use xi_protocol::model::ModelInfo;
use xi_provider::{Api, Auth, ErrorKind, ProviderConfig, Registry, RegistryConfig};

fn model_ref(model: &str) -> ModelRef {
    ModelRef {
        provider: "test".into(),
        model: model.into(),
    }
}

fn error_kind(registry: &Registry, model: &ModelRef) -> ErrorKind {
    match registry.connect(model) {
        Ok(_) => panic!("connection should fail"),
        Err(error) => error.kind,
    }
}

#[tokio::test]
async fn provider_and_model_overrides_take_effect_and_unknown_models_connect() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mock(&first, Api::AnthropicMessages, anthropic_text("end_turn")).await;
    mock(&second, Api::OpenaiChat, openai_text("stop")).await;
    let mut registry = registry(
        first.uri(),
        Api::AnthropicMessages,
        "test-model",
        false,
        Auth::None,
    );
    registry
        .connect(&model_ref("test-model"))
        .unwrap()
        .stream(&request(), &mut |_| {})
        .await
        .unwrap();
    registry.extend(RegistryConfig {
        provider: BTreeMap::from([(
            "test".into(),
            ProviderConfig {
                api: Api::OpenaiChat,
                base_url: format!("{}/", second.uri()),
                auth: Auth::None,
                compat: Default::default(),
            },
        )]),
        model: vec![
            ModelInfo {
                provider: "test".into(),
                id: "test-model".into(),
                context_window: 100000,
                max_output: 20000,
                reasoning: true,
                pricing: None,
            }
            .into(),
        ],
    });
    assert_eq!(registry.models().count(), 1);
    assert_eq!(
        registry
            .model(&model_ref("test-model"))
            .unwrap()
            .context_window,
        100000
    );
    registry
        .connect(&model_ref("test-model"))
        .unwrap()
        .stream(&request(), &mut |_| {})
        .await
        .unwrap();
    assert_eq!(first.received_requests().await.unwrap().len(), 1);
    let received = second.received_requests().await.unwrap();
    let body: Value = received[0].body_json().unwrap();
    assert_eq!(body["reasoning_effort"], "medium");
    assert!(registry.model(&model_ref("unlisted")).is_none());
    let mut request = request();
    request.model = "unlisted".into();
    registry
        .connect(&model_ref("unlisted"))
        .unwrap()
        .stream(&request, &mut |_| {})
        .await
        .unwrap();
    let received = second.received_requests().await.unwrap();
    let body: Value = received[1].body_json().unwrap();
    assert_eq!(body["model"], "unlisted");
    assert!(body.get("reasoning_effort").is_none());
    assert_eq!(
        error_kind(
            &registry,
            &ModelRef {
                provider: "missing".into(),
                model: "whatever".into()
            }
        ),
        ErrorKind::Config
    );
}

#[test]
fn model_override_key_includes_provider() {
    let mut registry = Registry::builtin();
    let original = registry
        .model(&ModelRef {
            provider: "openai".into(),
            model: "gpt-5.5".into(),
        })
        .unwrap()
        .clone();
    let mut custom = original.clone();
    custom.provider = "local".into();
    custom.context_window = 4096;
    registry.extend(RegistryConfig {
        provider: BTreeMap::new(),
        model: vec![custom.clone().into()],
    });
    assert_eq!(
        registry.model(&ModelRef {
            provider: "openai".into(),
            model: original.id.clone()
        }),
        Some(&original)
    );
    assert_eq!(
        registry.model(&ModelRef {
            provider: "local".into(),
            model: custom.id.clone()
        }),
        Some(&custom)
    );
}

#[test]
fn model_tables_split_compat_and_still_reject_unknown_fields() {
    let table = |extra: &str| {
        toml::from_str::<RegistryConfig>(&format!(
            "[[model]]\nprovider = \"p\"\nid = \"m\"\ncontext_window = 1\nmax_output = 1\n{extra}"
        ))
    };
    let config = table("compat = { responses_lite = true }").unwrap();
    assert_eq!(config.model[0].info.id, "m");
    assert_eq!(config.model[0].compat.responses_lite, Some(true));
    assert!(table("responses_lite = true").is_err());
    assert!(table("compat = { lite = true }").is_err());
}

#[test]
fn invalid_credentials_and_urls_fail_without_disclosing_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let empty = tempfile::NamedTempFile::new().unwrap();
    for auth in [
        Auth::File(dir.path().into()),
        Auth::File(dir.path().join("missing")),
        Auth::File(empty.path().into()),
        Auth::Env("XI_TEST_MISSING_VARIABLE_14b270f9".into()),
    ] {
        let registry = registry(
            "http://localhost".into(),
            Api::OpenaiChat,
            "test-model",
            false,
            auth,
        );
        assert_eq!(
            error_kind(&registry, &model_ref("test-model")),
            ErrorKind::Auth
        );
    }
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b"secret\nheader-injection").unwrap();
    let registry = registry(
        "http://localhost".into(),
        Api::OpenaiChat,
        "test-model",
        false,
        Auth::File(file.path().into()),
    );
    let error = match registry.connect(&model_ref("test-model")) {
        Ok(_) => panic!("invalid header"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::Auth);
    assert!(!error.message.contains("secret"));
    for url in [
        "not-a-url",
        "file:///tmp/socket",
        "https://secret@example.com/v1",
        "https://example.com?key=secret",
    ] {
        let registry =
            common::registry(url.into(), Api::OpenaiChat, "test-model", false, Auth::None);
        assert_eq!(
            error_kind(&registry, &model_ref("test-model")),
            ErrorKind::Config
        );
    }
}

#[cfg(unix)]
#[test]
fn credential_permissions_are_checked_on_the_open_file() {
    use std::os::unix::fs::PermissionsExt;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, b" key \n").unwrap();
    for mode in [0o644, 0o640, 0o660, 0o700, 0o4600] {
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
        let registry = registry(
            "http://localhost".into(),
            Api::OpenaiChat,
            "test-model",
            false,
            Auth::File(file.path().into()),
        );
        assert_eq!(
            error_kind(&registry, &model_ref("test-model")),
            ErrorKind::Auth,
            "mode {mode:o}"
        );
    }
    for mode in [0o600, 0o400] {
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .unwrap();
        let registry = registry(
            "http://localhost".into(),
            Api::OpenaiChat,
            "test-model",
            false,
            Auth::File(file.path().into()),
        );
        assert!(
            registry.connect(&model_ref("test-model")).is_ok(),
            "mode {mode:o}"
        );
    }
}

#[tokio::test]
async fn environment_credentials_are_resolved_without_modifying_process_environment() {
    let server = MockServer::start().await;
    mock(&server, Api::OpenaiChat, openai_text("stop")).await;
    let name = "CARGO_MANIFEST_DIR";
    let value = std::env::var(name).unwrap();
    let provider = provider(
        &server,
        Api::OpenaiChat,
        "test-model",
        false,
        Auth::Env(name.into()),
    );
    provider.stream(&request(), &mut |_| {}).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {}", value.trim())
    );
}
