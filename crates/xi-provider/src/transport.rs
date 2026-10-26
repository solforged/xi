use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;

use crate::{Api, BoxFuture, Delta, Error, ErrorKind, Provider, ProviderConfig, Request, Response};

pub(crate) struct HttpProvider {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    api: Api,
    reasoning_models: BTreeSet<String>,
}

impl HttpProvider {
    pub(crate) fn new(
        client: reqwest::Client,
        config: &ProviderConfig,
        credential: Option<String>,
        reasoning_models: BTreeSet<String>,
    ) -> Result<Self, Error> {
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        let suffix = match config.api {
            Api::AnthropicMessages => {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
                "/v1/messages"
            }
            Api::OpenaiChat => "/chat/completions",
        };
        if let Some(credential) = credential {
            let (name, value) = match config.api {
                Api::AnthropicMessages => ("x-api-key", credential),
                Api::OpenaiChat => ("authorization", format!("Bearer {credential}")),
            };
            let mut value = HeaderValue::from_str(&value).map_err(|_| {
                Error::new(
                    ErrorKind::Auth,
                    "Credential contains invalid header characters",
                )
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(Self {
            client,
            url: format!("{}{suffix}", config.base_url.trim_end_matches('/')),
            headers,
            api: config.api,
            reasoning_models,
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response, Error> {
        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .json(body)
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
            let reasoning = self.reasoning_models.contains(&request.model);
            match self.api {
                Api::AnthropicMessages => {
                    let body = crate::anthropic::body(request, reasoning)?;
                    crate::anthropic::consume(self.send(&body).await?, on_delta).await
                }
                Api::OpenaiChat => {
                    let body = crate::openai::body(request, reasoning)?;
                    crate::openai::consume(self.send(&body).await?, on_delta).await
                }
            }
        })
    }
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
    Error {
        kind: category,
        message: message.to_owned(),
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
}
