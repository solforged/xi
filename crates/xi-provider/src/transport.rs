use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::{
    Api, Auth, BoxFuture, Compat, Delta, Error, ErrorKind, LoginService, Provider, ProviderConfig,
    Reasoning, Request, Response,
};

/// Adds a provider's credential to each request.
pub(crate) trait Signer: Send + Sync {
    /// Inserts the credential headers.
    fn sign<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a mut HeaderMap,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Handles a 401 for a request signed into `headers`. `Ok(true)` means
    /// the credential changed and the request may be retried once.
    fn rejected<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<bool, Error>>;
}

/// A fixed credential header, or none.
pub(crate) struct Key(pub(crate) Option<(HeaderName, HeaderValue)>);

impl Signer for Key {
    fn sign<'a>(
        &'a self,
        _: &'a reqwest::Client,
        headers: &'a mut HeaderMap,
    ) -> BoxFuture<'a, Result<(), Error>> {
        if let Some((name, value)) = &self.0 {
            headers.insert(name.clone(), value.clone());
        }
        Box::pin(async { Ok(()) })
    }

    fn rejected<'a>(
        &'a self,
        _: &'a reqwest::Client,
        _: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
}

/// What a model's requests look like beyond the request itself: its
/// [`Compat`] resolved against the provider's, with defaults applied.
#[derive(Clone, Debug)]
pub(crate) struct Shape {
    /// Reasoning levels the model accepts; empty when it doesn't reason.
    pub(crate) reasoning: Vec<Reasoning>,
    /// Responses requests carry the output-token limit.
    pub(crate) output_limit: bool,
    /// The header carrying the session's prompt-cache key.
    pub(crate) session_header: Option<HeaderName>,
    /// The Responses Lite request shape.
    pub(crate) lite: bool,
}

impl Shape {
    pub(crate) fn new(reasoning: Vec<Reasoning>, compat: &Compat) -> Result<Self, Error> {
        let session_header = compat
            .session_header
            .as_deref()
            .map(HeaderName::try_from)
            .transpose()
            .map_err(|_| Error::new(ErrorKind::Config, "Invalid compat session_header"))?;
        Ok(Self {
            reasoning,
            output_limit: compat.output_limit.unwrap_or(true),
            session_header,
            lite: compat.responses_lite.unwrap_or(false),
        })
    }

    /// Whether the model reasons, after checking that it accepts the
    /// request's level.
    fn reasons(&self, request: &Request) -> Result<bool, Error> {
        if self.reasoning.is_empty() || self.reasoning.contains(&request.reasoning) {
            return Ok(!self.reasoning.is_empty());
        }
        let levels: Vec<_> = self.reasoning.iter().map(|level| level.name()).collect();
        Err(Error::new(
            ErrorKind::Config,
            format!(
                "{} does not support {} reasoning; it accepts {}",
                request.model,
                request.reasoning,
                levels.join(", ")
            ),
        ))
    }
}

pub(crate) struct HttpProvider {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    api: Api,
    signer: Box<dyn Signer>,
    subscription: Option<crate::claude_code::Wire>,
    /// Shapes of the provider's listed models.
    shapes: BTreeMap<String, Shape>,
    /// The shape of unlisted models, which don't reason.
    unlisted: Shape,
}

impl HttpProvider {
    pub(crate) fn new(
        client: reqwest::Client,
        config: &ProviderConfig,
        signer: Box<dyn Signer>,
        shapes: BTreeMap<String, Shape>,
    ) -> Result<Self, Error> {
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert(
            "user-agent",
            HeaderValue::from_static(concat!("xi/", env!("CARGO_PKG_VERSION"))),
        );
        let suffix = match config.api {
            Api::AnthropicMessages => {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                "/v1/messages"
            }
            Api::OpenaiChat => "/chat/completions",
            Api::OpenaiResponses => "/responses",
        };
        let subscription = matches!(
            (&config.auth, config.api),
            (Auth::Login(LoginService::Anthropic), Api::AnthropicMessages)
        );
        Ok(Self {
            client,
            url: format!(
                "{}{suffix}{}",
                config.base_url.trim_end_matches('/'),
                if subscription { "?beta=true" } else { "" },
            ),
            headers,
            api: config.api,
            signer,
            subscription: subscription
                .then(crate::claude_code::Wire::new)
                .transpose()?,
            shapes,
            unlisted: Shape::new(Vec::new(), &config.compat)?,
        })
    }

    async fn send(&self, body: Bytes, mut headers: HeaderMap) -> Result<reqwest::Response, Error> {
        self.signer.sign(&self.client, &mut headers).await?;
        match self.post(body.clone(), headers.clone()).await {
            Err(error)
                if error.kind == ErrorKind::Auth
                    && self.signer.rejected(&self.client, &headers).await? =>
            {
                self.signer.sign(&self.client, &mut headers).await?;
                self.post(body, headers).await
            }
            result => result,
        }
    }

    async fn post(&self, body: Bytes, headers: HeaderMap) -> Result<reqwest::Response, Error> {
        let response = self
            .client
            .post(&self.url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(network)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
        let body = response.text().await.map_err(network)?;
        let value: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
        let mut error = api_error(Some(status), &value);
        error.retry_after = retry_after;
        Err(error)
    }
}

impl Provider for HttpProvider {
    fn stream<'a>(
        &'a self,
        request: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let mut shape = self.shapes.get(&request.model).unwrap_or(&self.unlisted);
            // Responses Lite only takes function tools, so a hosted search
            // uses the full Responses shape even for Lite models.
            let full;
            if request.web_search && shape.lite {
                full = Shape {
                    lite: false,
                    ..shape.clone()
                };
                shape = &full;
            }
            let reasoning = shape.reasons(request)?;
            let mut headers = self.headers.clone();
            if let Some(name) = &shape.session_header
                && let Some(value) = request
                    .cache_key
                    .as_deref()
                    .and_then(|key| HeaderValue::from_str(key).ok())
            {
                headers.insert(name.clone(), value);
            }
            match self.api {
                Api::AnthropicMessages => {
                    let body = match &self.subscription {
                        Some(wire) => wire.request(request, reasoning, &mut headers)?,
                        None => encode(&crate::anthropic::body(
                            request,
                            reasoning,
                            request.max_tokens,
                        )?)?,
                    };
                    crate::anthropic::consume(
                        self.send(body, headers).await?,
                        on_delta,
                        self.subscription.is_some(),
                    )
                    .await
                }
                Api::OpenaiChat => {
                    let body = crate::openai::body(request, reasoning)?;
                    crate::openai::consume(self.send(encode(&body)?, headers).await?, on_delta)
                        .await
                }
                Api::OpenaiResponses => {
                    let body = crate::responses::body(request, shape, reasoning)?;
                    if shape.lite {
                        headers.insert(
                            "x-openai-internal-codex-responses-lite",
                            HeaderValue::from_static("true"),
                        );
                    }
                    crate::responses::consume(self.send(encode(&body)?, headers).await?, on_delta)
                        .await
                }
            }
        })
    }
}

fn encode(body: &Value) -> Result<Bytes, Error> {
    serde_json::to_vec(body)
        .map(Bytes::from)
        .map_err(|error| Error::new(ErrorKind::InvalidRequest, error.to_string()))
}

pub(crate) fn network(error: reqwest::Error) -> Error {
    Error::new(ErrorKind::Network, error.without_url().to_string())
}

pub(crate) fn decode(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Decode, message)
}

pub(crate) fn api_error(status: Option<u16>, value: &Value) -> Error {
    let detail = value.get("error").unwrap_or(value);
    let message = detail
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| detail.as_str())
        .unwrap_or("Provider request failed");
    let code = detail
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = detail
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let lower = message.to_ascii_lowercase();
    let context = code == "context_length_exceeded"
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("maximum context length")
        || lower.contains("context window")
        || lower.contains("too many tokens");
    let category = match status {
        Some(401 | 403) => ErrorKind::Auth,
        Some(429) => ErrorKind::RateLimited,
        Some(500..=599) => ErrorKind::Unavailable,
        Some(400) if context => ErrorKind::ContextOverflow,
        Some(400..=499) => ErrorKind::InvalidRequest,
        _ => match kind {
            "authentication_error" | "permission_error" => ErrorKind::Auth,
            "rate_limit_error" => ErrorKind::RateLimited,
            "overloaded_error" | "api_error" | "server_error" => ErrorKind::Unavailable,
            _ if context => ErrorKind::ContextOverflow,
            "invalid_request_error" | "not_found_error" => ErrorKind::InvalidRequest,
            _ if status.is_some() => ErrorKind::InvalidRequest,
            _ => ErrorKind::Decode,
        },
    };
    let retry_after = detail
        .get("retry_after")
        .or_else(|| value.get("retry_after"))
        .and_then(|value| match value {
            Value::String(value) => parse_retry_after(value, SystemTime::now()),
            Value::Number(value) => value
                .as_f64()
                .and_then(|value| Duration::try_from_secs_f64(value).ok()),
            _ => None,
        });
    // The ChatGPT backend reports plan limits without a message.
    let message = match kind {
        "usage_limit_reached" => match detail.get("resets_at").and_then(Value::as_u64) {
            Some(resets) => {
                let left = (UNIX_EPOCH + Duration::from_secs(resets))
                    .duration_since(SystemTime::now())
                    .unwrap_or_default()
                    .as_secs();
                format!(
                    "ChatGPT usage limit reached; it resets in {}h {}m",
                    left / 3600,
                    left % 3600 / 60
                )
            }
            None => "ChatGPT usage limit reached".to_owned(),
        },
        "usage_not_included" => "This ChatGPT plan does not include Codex usage".to_owned(),
        _ => message.to_owned(),
    };
    Error {
        kind: category,
        message,
        retry_after,
    }
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return Duration::try_from_secs_f64(seconds).ok();
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_dates_and_invalid_values() {
        let now = httpdate::parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(
            parse_retry_after(" 1.5 ", now),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:12 GMT", now),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:27:00 GMT", now),
            Some(Duration::ZERO)
        );
        for value in ["-1", "NaN", "inf", "tomorrow", ""] {
            assert_eq!(parse_retry_after(value, now), None);
        }
    }

    #[tokio::test]
    async fn subscription_401_retry_reuses_identical_attested_bytes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::header};
        use xi_protocol::Block;

        struct Refresh(AtomicBool);

        impl Signer for Refresh {
            fn sign<'a>(
                &'a self,
                _: &'a reqwest::Client,
                headers: &'a mut HeaderMap,
            ) -> BoxFuture<'a, Result<(), Error>> {
                headers.insert(
                    "authorization",
                    HeaderValue::from_static(if self.0.load(Ordering::SeqCst) {
                        "Bearer refreshed"
                    } else {
                        "Bearer initial"
                    }),
                );
                Box::pin(async { Ok(()) })
            }

            fn rejected<'a>(
                &'a self,
                _: &'a reqwest::Client,
                _: &'a HeaderMap,
            ) -> BoxFuture<'a, Result<bool, Error>> {
                self.0.store(true, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            }
        }

        let server = MockServer::start().await;
        Mock::given(header("authorization", "Bearer initial"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                serde_json::json!({"error":{"type":"authentication_error","message":"expired"}}),
            ))
            .mount(&server)
            .await;
        Mock::given(header("authorization", "Bearer refreshed"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                )))
            .mount(&server)
            .await;
        let provider = HttpProvider::new(
            reqwest::Client::new(),
            &ProviderConfig {
                api: Api::AnthropicMessages,
                base_url: server.uri(),
                auth: Auth::Login(LoginService::Anthropic),
                compat: Compat::default(),
            },
            Box::new(Refresh(AtomicBool::new(false))),
            BTreeMap::new(),
        )
        .unwrap();
        provider
            .stream(
                &Request {
                    model: "test-model".into(),
                    system: "Keep user cch=00000.".into(),
                    messages: vec![crate::Message::User(vec![Block::Text {
                        text: "cch=00000".into(),
                    }])],
                    tools: Vec::new(),
                    max_tokens: 1024,
                    reasoning: Reasoning::Off,
                    cache_key: Some("session-1".into()),
                    web_search: false,
                },
                &mut |_| {},
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        assert_ne!(
            requests[0].headers["authorization"],
            requests[1].headers["authorization"]
        );
        let body: Value = requests[0].body_json().unwrap();
        assert_eq!(body["messages"][0]["content"][0]["text"], "cch=00000");
        assert_eq!(body["system"][2]["text"], "Keep user cch=00000.");
    }
}
