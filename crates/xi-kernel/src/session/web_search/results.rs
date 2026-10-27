//! Results read back from a hosted search's response blocks.
//!
//! Anthropic returns `server_tool_use` and `web_search_tool_result` blocks,
//! and text blocks whose `citations` name the pages they quote. Responses
//! returns `web_search_call` items whose `action.sources` list the pages it
//! read. Both arrive as [`Block::Opaque`]; cited text stays opaque too,
//! because a plain text block would drop its citations.

use serde_json::Value;
use xi_protocol::{Block, ProviderTag};

/// The most sources a hosted search reports.
const MAX_SOURCES: usize = 10;

/// A page a search read or cited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Source {
    /// The page title, or its URL when the provider gave none.
    pub title: String,
    /// The page URL.
    pub url: String,
}

/// What a hosted search request produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WebResults {
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
pub(super) fn web_results(content: &[Block]) -> WebResults {
    let mut results = WebResults::default();
    let mut cited = Vec::new();
    let mut found = Vec::new();
    for block in content {
        match block {
            Block::Text { text } => results.answer.push_str(text),
            Block::Opaque { provider, value } => match (provider, kind(value)) {
                (ProviderTag::Anthropic, "server_tool_use") if value["name"] == "web_search" => {
                    results.searched = true;
                    results.answer.clear();
                }
                (ProviderTag::Anthropic, "web_search_tool_result") => {
                    results.searched = true;
                    results.answer.clear();
                    match &value["content"] {
                        Value::Array(pages) => found.extend(pages.iter().filter_map(source)),
                        error => results
                            .errors
                            .push(error["error_code"].as_str().unwrap_or("unknown").into()),
                    }
                }
                (ProviderTag::Anthropic, "text") => {
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
                (ProviderTag::Openai, "web_search_call") => {
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

pub(super) fn source(value: &Value) -> Option<Source> {
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

    fn opaque(provider: ProviderTag, value: Value) -> Block {
        Block::Opaque { provider, value }
    }

    #[test]
    fn anthropic_answers_follow_the_search_and_list_only_cited_pages() {
        let content = [
            Block::Text {
                text: "I'll search for that.".into(),
            },
            opaque(
                ProviderTag::Anthropic,
                json!({"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "zig"}}),
            ),
            opaque(
                ProviderTag::Anthropic,
                json!({"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [
                    {"type": "web_search_result", "url": "https://a.example", "title": "A"},
                    {"type": "web_search_result", "url": "https://b.example", "title": "B"},
                ]}),
            ),
            Block::Text {
                text: "Zig 0.17 shipped. ".into(),
            },
            opaque(
                ProviderTag::Anthropic,
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
            ProviderTag::Anthropic,
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
                ProviderTag::Openai,
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
}
