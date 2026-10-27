//! `fetch`: one public URL as text.
//!
//! The request runs in the worker, outside the sandbox, after policy has
//! answered the call's `net` crossing for the URL's host. Redirects on that
//! host are followed; a redirect to another host ends the call with its
//! target, so the next host gets its own crossing. HTML becomes Markdown,
//! other text passes through, and bodies stop at 5 MiB. Every result is
//! untrusted, so it taints the session.

use std::sync::LazyLock;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION, USER_AGENT};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use xi_protocol::ToolCallId;

use crate::{Context, Ran, output, parse};

const MAX_BYTES: usize = 5 << 20;
const MAX_REDIRECTS: usize = 5;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    url: String,
}

/// The URL a call would fetch.
pub(crate) fn input(value: &Value) -> Result<Url, String> {
    let input: Input = parse(value)?;
    let url = Url::parse(input.url.trim()).map_err(|error| format!("Invalid URL: {error}"))?;
    check(&url)?;
    Ok(url)
}

fn check(url: &Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "Only http and https URLs can be fetched, not {}",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err("The URL has no host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("The URL must not carry credentials".into());
    }
    Ok(())
}

/// The policy subject for a URL: its host, lowercased, without the port.
pub(crate) fn host(url: &Url) -> String {
    url.host_str().unwrap_or_default().to_ascii_lowercase()
}

static CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
});

pub(crate) async fn run(call: &ToolCallId, value: &Value, ctx: &Context) -> Result<Ran, String> {
    let client = CLIENT.as_ref().map_err(Clone::clone)?;
    let mut url = input(value)?;
    let origin = host(&url);
    let mut hops = 0;
    let mut response = loop {
        let response = client
            .get(url.clone())
            .header(USER_AGENT, concat!("xi/", env!("CARGO_PKG_VERSION")))
            .header(ACCEPT, "text/html,text/plain;q=0.9,*/*;q=0.5")
            .send()
            .await
            .map_err(|error| format!("Fetch failed: {}", error.without_url()))?;
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|location| location.to_str().ok());
        let (true, Some(location)) = (response.status().is_redirection(), location) else {
            break response;
        };
        let next = url
            .join(location)
            .map_err(|error| format!("Invalid redirect: {error}"))?;
        if check(&next).is_err() || host(&next) != origin {
            let mut ran = output::text(
                call,
                format!("Redirected to {next}\nFetch that URL to follow the redirect."),
                false,
                ctx,
            );
            ran.output.untrusted = true;
            return Ok(ran);
        }
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(format!("More than {MAX_REDIRECTS} redirects"));
        }
        url = next;
    };
    let status = response.status();
    let mime = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Fetch failed: {}", error.without_url()))?
    {
        let room = MAX_BYTES - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let text = match classify(&mime, &body) {
        Some(Format::Html) => {
            let html = String::from_utf8_lossy(&body).into_owned();
            tokio::task::spawn_blocking(move || markdown(&html))
                .await
                .map_err(|error| format!("Converting the page failed: {error}"))??
        }
        Some(Format::Text) => String::from_utf8_lossy(&body).into_owned(),
        None => {
            return Err(format!(
                "Cannot read {} as text; download it with bash instead",
                if mime.is_empty() {
                    "this response"
                } else {
                    &mime
                }
            ));
        }
    };
    let mut report = format!("URL: {url}\nStatus: {status}\n");
    if truncated {
        report.push_str("Truncated: the body exceeded 5 MiB\n");
    }
    report.push('\n');
    report.push_str(text.trim());
    let mut ran = output::text(call, report, !status.is_success(), ctx);
    ran.output.untrusted = true;
    Ok(ran)
}

#[derive(Debug, PartialEq, Eq)]
enum Format {
    Html,
    Text,
}

/// How to read a body, from its media type or, without one, its bytes.
fn classify(mime: &str, body: &[u8]) -> Option<Format> {
    match mime {
        "text/html" | "application/xhtml+xml" => Some(Format::Html),
        "" if body.trim_ascii_start().starts_with(b"<") => Some(Format::Html),
        "" => std::str::from_utf8(body).is_ok().then_some(Format::Text),
        _ if mime.starts_with("text/")
            || mime.ends_with("+json")
            || mime.ends_with("+xml")
            || matches!(
                mime,
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/toml"
                    | "application/yaml"
                    | "application/x-yaml"
            ) =>
        {
            Some(Format::Text)
        }
        _ => None,
    }
}

/// The page as Markdown, without scripts, styles, navigation or forms, and
/// with runs of blank lines collapsed.
fn markdown(html: &str) -> Result<String, String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "template", "svg", "iframe", "nav", "footer",
            "form", "button",
        ])
        .build();
    let text = converter
        .convert(html)
        .map_err(|error| format!("Converting the page failed: {error}"))?;
    let mut out = String::with_capacity(text.len());
    let mut blank = 0;
    for line in text.lines() {
        let line = line.trim_end();
        blank = if line.is_empty() { blank + 1 } else { 0 };
        if blank < 2 {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn urls_must_be_plain_http() {
        assert_eq!(
            host(&input(&json!({"url": "https://Docs.RS:443/htmd"})).unwrap()),
            "docs.rs"
        );
        for url in [
            "file:///etc/passwd",
            "https://user:pw@example.com/",
            "not a url",
        ] {
            assert!(input(&json!({ "url": url })).is_err(), "{url}");
        }
    }

    #[test]
    fn bodies_without_a_media_type_are_sniffed() {
        assert_eq!(classify("text/html", b"x"), Some(Format::Html));
        assert_eq!(classify("", b"  <!doctype html>"), Some(Format::Html));
        assert_eq!(classify("", b"plain"), Some(Format::Text));
        assert_eq!(classify("", &[0xff, 0xfe]), None);
        assert_eq!(
            classify("application/vnd.api+json", b"{}"),
            Some(Format::Text)
        );
        assert_eq!(classify("application/pdf", b"%PDF"), None);
    }

    #[test]
    fn pages_lose_scripts_and_navigation() {
        let text = markdown(
            "<html><head><title>T</title></head><body><nav>Menu</nav><h1>Title</h1>\
             <script>alert(1)</script><p>Body <a href=\"/x\">link</a></p>\n\n\n\n<p>End</p></body></html>",
        )
        .unwrap();
        assert_eq!(text, "# Title\n\nBody [link](/x)\n\nEnd\n");
    }
}
