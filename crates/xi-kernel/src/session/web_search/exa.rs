//! The Exa search client and its policy-visible destination.

use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};
use xi_provider::Auth;

use super::results::source;

/// The Exa search API's host, which policy sees as the search's crossing.
pub(super) const EXA_HOST: &str = "api.exa.ai";
const EXA_RESULTS: u32 = 8;

/// One Exa search result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExaResult {
    /// The page title, or its URL when Exa gave none.
    pub title: String,
    /// The page URL.
    pub url: String,
    /// Exa's estimate of the publication date, `YYYY-MM-DD`.
    pub published: Option<String>,
    /// The passages Exa picked as most relevant to the query.
    pub highlights: Vec<String>,
}

/// Runs one Exa search with the credential `auth` names.
pub(super) async fn exa_search(auth: &Auth, query: &str) -> Result<Vec<ExaResult>, String> {
    exa_search_at(&format!("https://{EXA_HOST}"), auth, query).await
}

async fn exa_search_at(base: &str, auth: &Auth, query: &str) -> Result<Vec<ExaResult>, String> {
    let key = auth
        .resolve()
        .map_err(|error| error.to_string())?
        .ok_or("Exa needs an API key")?;
    static CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())
    });
    let client = CLIENT.as_ref().map_err(Clone::clone)?;
    let response = client
        .post(format!("{base}/search"))
        .header("x-api-key", key)
        .header("user-agent", concat!("xi/", env!("CARGO_PKG_VERSION")))
        .json(&json!({
            "query": query, "type": "auto", "numResults": EXA_RESULTS,
            "contents": {"highlights": true},
        }))
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    let body = response.text().await.map_err(network)?;
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
    if !status.is_success() {
        let detail = value.get("error").unwrap_or(&value);
        let message = detail
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| detail.as_str())
            .unwrap_or("Exa request failed");
        return Err(format!("{message} (HTTP {status})"));
    }
    let results = value["results"]
        .as_array()
        .ok_or("Exa response has no results")?;
    Ok(results
        .iter()
        .filter_map(|result| {
            let page = source(result)?;
            Some(ExaResult {
                title: page.title,
                url: page.url,
                published: result["publishedDate"]
                    .as_str()
                    .map(|date| date.chars().take(10).collect()),
                highlights: result["highlights"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|text| Some(text.as_str()?.trim().to_owned()))
                    .filter(|text| !text.is_empty())
                    .collect(),
            })
        })
        .collect())
}

fn network(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn exa_sends_the_key_and_reads_ranked_results() {
        use std::os::unix::fs::PermissionsExt;

        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("x-api-key", "exa-key"))
            .and(body_partial_json(
                json!({"query": "zig release", "contents": {"highlights": true}}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"requestId": "r", "results": [
                    {"id": "1", "url": "https://ziglang.org/download/", "title": "Download",
                     "publishedDate": "2026-10-01T00:00:00.000Z", "highlights": [" 0.17.0 ", ""]},
                    {"id": "2", "title": "No URL"},
                ]}),
            ))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("exa");
        std::fs::write(&key, "exa-key\n").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let results = exa_search_at(&server.uri(), &Auth::File(key), "zig release")
            .await
            .unwrap();
        assert_eq!(
            results,
            [ExaResult {
                title: "Download".into(),
                url: "https://ziglang.org/download/".into(),
                published: Some("2026-10-01".into()),
                highlights: vec!["0.17.0".into()],
            }]
        );
        let missing = exa_search_at(&server.uri(), &Auth::File(dir.path().join("none")), "zig")
            .await
            .unwrap_err();
        assert!(missing.contains("Cannot open credential file"));
    }
}
