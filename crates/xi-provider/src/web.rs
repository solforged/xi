//! Web search results: read back from a hosted search's response blocks,
//! or fetched from Exa.
//!
//! Anthropic returns `server_tool_use` and `web_search_tool_result` blocks,
//! and text blocks whose `citations` name the pages they quote. Responses
//! returns `web_search_call` items whose `action.sources` list the pages it
//! read. Both arrive as [`Block::Opaque`]; cited text stays opaque too,
//! because a plain text block would drop its citations.

use std::sync::LazyLock;
use std::time::Duration;

use serde_json::{Value, json};

use crate::registry::resolve_auth;
use crate::transport::{api_error, decode, network};
use crate::{Auth, Block, Error, ErrorKind};

/// The Exa search API's host, which policy sees as the search's crossing.
pub const EXA_HOST: &str = "api.exa.ai";
/// Results one Exa search returns.
const EXA_RESULTS: u32 = 8;
/// The most sources a hosted search reports.
const MAX_SOURCES: usize = 10;

/// One Exa search result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExaResult {
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
///
/// # Errors
/// Returns `Auth` for a missing or rejected key, network errors, and
/// `Decode` for an unexpected response.
pub async fn exa_search(auth: &Auth, query: &str) -> Result<Vec<ExaResult>, Error> {
    exa_search_at(&format!("https://{EXA_HOST}"), auth, query).await
}

async fn exa_search_at(base: &str, auth: &Auth, query: &str) -> Result<Vec<ExaResult>, Error> {
    let key =
        resolve_auth(auth)?.ok_or_else(|| Error::new(ErrorKind::Config, "Exa needs an API key"))?;
    static CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())
    });
    let client = CLIENT
        .as_ref()
        .map_err(|error| Error::new(ErrorKind::Config, error.clone()))?;
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
        return Err(api_error(Some(status.as_u16()), &value));
    }
    let results = value["results"]
        .as_array()
        .ok_or_else(|| decode("Exa response has no results"))?;
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

/// A page a search read or cited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The page title, or its URL when the provider gave none.
    pub title: String,
    /// The page URL.
    pub url: String,
}

/// What a hosted search request produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebResults {
    /// Whether the model ran at least one search.
    pub searched: bool,
    /// The model's answer text.
    pub answer: String,
    /// The pages the answer cited, in citation order; without citations,
    /// the pages the searches returned. Each URL once, at most ten.
    pub sources: Vec<Source>,
    /// Error codes the provider reported for failed searches.
    pub errors: Vec<String>,
}

/// Reads the searches, answer and sources out of a response's blocks. The
/// answer is the text after the last search; text before it narrates the
/// search rather than answering.
#[must_use]
pub fn web_results(content: &[Block]) -> WebResults {
    let mut results = WebResults::default();
    let mut cited = Vec::new();
    let mut found = Vec::new();
    for block in content {
        match block {
            Block::Text { text } => results.answer.push_str(text),
            Block::Opaque { provider, value } => match (provider.as_str(), kind(value)) {
                ("anthropic", "server_tool_use") if value["name"] == "web_search" => {
                    results.searched = true;
                    results.answer.clear();
                }
                ("anthropic", "web_search_tool_result") => {
                    results.searched = true;
                    results.answer.clear();
                    match &value["content"] {
                        Value::Array(pages) => found.extend(pages.iter().filter_map(source)),
                        error => results
                            .errors
                            .push(error["error_code"].as_str().unwrap_or("unknown").into()),
                    }
                }
                ("anthropic", "text") => {
                    results
                        .answer
                        .push_str(value["text"].as_str().unwrap_or_default());
                    cited.extend(
                        value["citations"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(source),
                    );
                }
                ("openai", "web_search_call") => {
                    results.searched = true;
                    results.answer.clear();
                    found.extend(
                        value["action"]["sources"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(source),
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }
    results.answer = results.answer.trim().to_owned();
    // Searches return pages the answer never used; citations name the ones
    // it did. Responses reports no citations here, so its pages stand in.
    let pages = if cited.is_empty() { found } else { cited };
    for page in pages {
        if results.sources.len() == MAX_SOURCES {
            break;
        }
        if !results.sources.iter().any(|known| known.url == page.url) {
            results.sources.push(page);
        }
    }
    results
}

fn kind(value: &Value) -> &str {
    value["type"].as_str().unwrap_or_default()
}

fn source(value: &Value) -> Option<Source> {
    let url = value["url"].as_str().filter(|url| !url.is_empty())?;
    // Responses tags the links it returns; the tag is not part of the page.
    let url = url
        .strip_suffix("?utm_source=openai")
        .or_else(|| url.strip_suffix("&utm_source=openai"))
        .unwrap_or(url);
    let title = value["title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(url);
    Some(Source {
        title: title.into(),
        url: url.into(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn opaque(provider: &str, value: Value) -> Block {
        Block::Opaque {
            provider: provider.into(),
            value,
        }
    }

    #[test]
    fn anthropic_answers_follow_the_search_and_list_only_cited_pages() {
        let content = [
            Block::Text {
                text: "I'll search for that.".into(),
            },
            opaque(
                "anthropic",
                json!({"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "zig"}}),
            ),
            opaque(
                "anthropic",
                json!({"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [
                    {"type": "web_search_result", "url": "https://a.example", "title": "A"},
                    {"type": "web_search_result", "url": "https://b.example", "title": "B"},
                ]}),
            ),
            Block::Text {
                text: "Zig 0.17 shipped. ".into(),
            },
            opaque(
                "anthropic",
                json!({"type": "text", "text": "It adds X.", "citations": [
                    {"type": "web_search_result_location", "url": "https://b.example", "title": "B", "cited_text": "X"},
                ]}),
            ),
        ];
        let results = web_results(&content);
        assert!(results.searched);
        assert_eq!(results.answer, "Zig 0.17 shipped. It adds X.");
        let urls: Vec<_> = results
            .sources
            .iter()
            .map(|page| page.url.as_str())
            .collect();
        assert_eq!(urls, ["https://b.example"]);
    }

    #[test]
    fn anthropic_search_errors_are_reported() {
        let content = [opaque(
            "anthropic",
            json!({"type": "web_search_tool_result", "tool_use_id": "srv_1", "content":
                {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}),
        )];
        let results = web_results(&content);
        assert!(results.searched);
        assert_eq!(results.errors, ["max_uses_exceeded"]);
        assert!(results.sources.is_empty());
    }

    #[test]
    fn responses_sources_without_titles_use_their_urls() {
        let content = [
            opaque(
                "openai",
                json!({"type": "web_search_call", "status": "completed", "action": {"type": "search", "query": "zig",
                    "sources": [{"type": "url", "url": "https://a.example"}, {"type": "url", "url": "https://a.example"}]}}),
            ),
            Block::Text {
                text: "Answer.".into(),
            },
        ];
        let results = web_results(&content);
        assert!(results.searched);
        assert_eq!(
            results.sources,
            [Source {
                title: "https://a.example".into(),
                url: "https://a.example".into(),
            }]
        );
    }

    #[test]
    fn a_reply_without_a_search_call_is_not_a_search() {
        let results = web_results(&[Block::Text {
            text: "From memory.".into(),
        }]);
        assert!(!results.searched);
    }

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
        assert_eq!(missing.kind, ErrorKind::Auth);
    }
}
