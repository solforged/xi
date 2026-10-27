//! Explicitly opted-in Claude subscription sign-in with PKCE and private tokens.
//!
//! Access tokens are opaque. Expiry comes from the token endpoint rather than
//! JWT claims; a refresh retains the credential lock until its rotation is saved.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xi_protocol::BoxFuture;

use crate::oauth;
use crate::transport::network;
use crate::{Error, ErrorKind};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
const REDIRECT: &str = "http://localhost:54545/callback";
const SCOPE: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const REFRESH_WINDOW: u64 = 5 * 60;
const REFRESH_BETA: &str = "oauth-2025-04-20";
const REFRESH_USER_AGENT: &str = "anthropic-sdk-typescript/0.112.1 userOAuthProvider";

/// Where the Anthropic subscription tokens live inside a credential directory.
#[must_use]
pub fn anthropic_file(credentials: &Path) -> PathBuf {
    credentials.join("anthropic.json")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    /// Unix seconds, without subtracting the refresh window from the stored expiry.
    expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
}

impl Tokens {
    fn expiring(&self, now: u64) -> bool {
        self.expires_at <= now.saturating_add(REFRESH_WINDOW)
    }

    fn valid(&self) -> bool {
        valid_token(&self.access_token) && valid_token(&self.refresh_token) && self.expires_at != 0
    }
}

/// A browser sign-in waiting for its loopback callback or pasted redirect.
pub struct Login {
    listener: TcpListener,
    url: String,
    verifier: String,
    state: String,
}

impl Login {
    /// Binds the registered loopback port and builds the PKCE authorization URL.
    ///
    /// # Errors
    /// Returns `Config` when port 54545 is busy or randomness is unavailable.
    pub async fn start() -> Result<Self, Error> {
        let listener = TcpListener::bind(("127.0.0.1", 54545)).await.map_err(|_| {
            Error::new(
                ErrorKind::Config,
                "Port 54545 is busy; close the other Anthropic sign-in and retry",
            )
        })?;
        let (verifier, challenge) = oauth::pkce()?;
        let state = URL_SAFE_NO_PAD.encode(oauth::random::<32>()?);
        Ok(Self {
            listener,
            url: authorization_url(&challenge, &state),
            verifier,
            state,
        })
    }

    /// The URL to open in a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for a browser callback or pasted redirect URL / `code#state`, then
    /// exchanges the authorization code and atomically saves private tokens.
    ///
    /// # Errors
    /// Returns `Auth` on denial, end of input, mismatched state or invalid tokens,
    /// `Network` on transport failure, and `Config` if saving fails.
    pub async fn finish(
        self,
        file: &Path,
        pasted: impl Future<Output = Option<String>>,
    ) -> Result<(), Error> {
        let code = self.code(pasted).await?;
        let tokens = exchange(
            &oauth::client()?,
            TOKEN_URL,
            &code,
            &self.verifier,
            &self.state,
        )
        .await?;
        oauth::save(file, &tokens)
    }

    async fn code(&self, pasted: impl Future<Output = Option<String>>) -> Result<String, Error> {
        tokio::select! {
            code = serve(&self.listener, &self.state) => code,
            text = pasted => callback_code(
                &text.ok_or_else(|| Error::new(ErrorKind::Auth, "Anthropic sign-in input ended"))?,
                &self.state,
            ),
        }
    }
}

fn authorization_url(challenge: &str, state: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("the authorization URL is valid");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("code", "true");
    url.into()
}

/// Adds only the bearer credential. The Messages adapter owns wire fingerprinting.
pub(crate) struct Signer {
    file: PathBuf,
}

impl Signer {
    pub(crate) fn new(file: PathBuf) -> Self {
        Self { file }
    }

    async fn tokens(&self, client: &reqwest::Client, url: &str) -> Result<Tokens, Error> {
        let tokens = load(&self.file)?;
        if tokens.expiring(unix_now()?) {
            renew(client, &self.file, &tokens.access_token, url).await
        } else {
            Ok(tokens)
        }
    }

    async fn retry(
        &self,
        client: &reqwest::Client,
        headers: &HeaderMap,
        url: &str,
    ) -> Result<bool, Error> {
        let Some(stale) = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return Ok(false);
        };
        renew(client, &self.file, stale, url).await?;
        Ok(true)
    }
}

impl crate::transport::Signer for Signer {
    fn sign<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a mut HeaderMap,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let tokens = self.tokens(client, TOKEN_URL).await?;
            let mut bearer = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
                .map_err(|_| malformed())?;
            bearer.set_sensitive(true);
            headers.insert(AUTHORIZATION, bearer);
            Ok(())
        })
    }

    fn rejected<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(self.retry(client, headers, TOKEN_URL))
    }
}

/// Detaching the owned refresh task on cancellation preserves the lock until the
/// request completes and its rotated refresh token is durably replaced on disk.
async fn renew(
    client: &reqwest::Client,
    file: &Path,
    stale: &str,
    url: &str,
) -> Result<Tokens, Error> {
    let client = client.clone();
    let file = file.to_owned();
    let stale = stale.to_owned();
    let url = url.to_owned();
    tokio::spawn(async move {
        let _lock = oauth::lock(&file, "Anthropic").await?;
        let current = load(&file)?;
        if current.access_token != stale {
            return Ok(current);
        }
        let fresh = refresh(&client, &url, current).await?;
        oauth::save(&file, &fresh)?;
        Ok(fresh)
    })
    .await
    .map_err(|_| Error::new(ErrorKind::Auth, "Anthropic token refresh task failed"))?
}

async fn exchange(
    client: &reqwest::Client,
    url: &str,
    code: &str,
    verifier: &str,
    state: &str,
) -> Result<Tokens, Error> {
    let response = client
        .post(url)
        .json(&json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "redirect_uri": REDIRECT,
            "code_verifier": verifier,
            "state": state,
        }))
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    let body = response.json().await.map_err(|_| malformed())?;
    if !status.is_success() {
        return Err(Error::new(
            ErrorKind::Auth,
            format!("Anthropic sign-in token exchange failed with HTTP {status}"),
        ));
    }
    response_tokens(&body, None, unix_now()?)
}

async fn refresh(client: &reqwest::Client, url: &str, tokens: Tokens) -> Result<Tokens, Error> {
    let response = client
        .post(url)
        .header("anthropic-beta", REFRESH_BETA)
        .header("user-agent", REFRESH_USER_AGENT)
        .json(&json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": tokens.refresh_token,
        }))
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    if !status.is_success() {
        return Err(if status.is_client_error() && status.as_u16() != 429 {
            Error::new(
                ErrorKind::Auth,
                "The Anthropic sign-in has expired or was revoked; run `xi login anthropic`",
            )
        } else {
            Error::new(
                ErrorKind::Unavailable,
                format!("Anthropic token refresh failed with HTTP {status}"),
            )
        });
    }
    let body = response.json().await.map_err(|_| malformed())?;
    response_tokens(&body, Some(tokens), unix_now()?)
}

fn response_tokens(body: &Value, previous: Option<Tokens>, now: u64) -> Result<Tokens, Error> {
    let access_token = body["access_token"]
        .as_str()
        .filter(|token| valid_token(token))
        .ok_or_else(malformed)?
        .to_owned();
    let refresh_token = match body.get("refresh_token") {
        Some(value) => value
            .as_str()
            .filter(|token| valid_token(token))
            .map(str::to_owned),
        None => previous.as_ref().map(|tokens| tokens.refresh_token.clone()),
    }
    .ok_or_else(malformed)?;
    let expires_at = body["expires_in"]
        .as_u64()
        .filter(|seconds| *seconds != 0)
        .and_then(|seconds| now.checked_add(seconds))
        .ok_or_else(malformed)?;
    let identity = |name: &str| {
        body["account"][name]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at,
        account_id: identity("uuid").or_else(|| previous.as_ref()?.account_id.clone()),
        email: identity("email_address").or_else(|| previous.as_ref()?.email.clone()),
    })
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic())
}

fn unix_now() -> Result<u64, Error> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| {
            Error::new(
                ErrorKind::Config,
                "The system clock is before the Unix epoch",
            )
        })
}

fn malformed() -> Error {
    Error::new(ErrorKind::Auth, "The Anthropic credential is malformed")
}

fn load(file: &Path) -> Result<Tokens, Error> {
    let tokens: Tokens = oauth::load(
        file,
        "Anthropic",
        "Not signed in to Anthropic; run `xi login anthropic`",
    )?;
    if !tokens.valid() {
        return Err(malformed());
    }
    Ok(tokens)
}

/// A callback must identify this login exactly; raw codes without state, other
/// redirect origins, duplicated parameters and fragment ambiguity fail closed.
fn callback_code(text: &str, expected: &str) -> Result<String, Error> {
    let invalid = || {
        Error::new(
            ErrorKind::Auth,
            "The Anthropic callback does not match this login",
        )
    };
    let text = text.trim();
    if !text.contains("://") {
        let (code, state) = text.split_once('#').ok_or_else(invalid)?;
        if state != expected || !valid_token(code) {
            return Err(invalid());
        }
        return Ok(code.to_owned());
    }
    let url = reqwest::Url::parse(text).map_err(|_| invalid())?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port() != Some(54545)
        || url.path() != "/callback"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (name, value) in url.query_pairs() {
        let field = match name.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            "error_description" => &mut description,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    if state.as_deref() != Some(expected) {
        return Err(invalid());
    }
    if error.is_some() {
        return Err(Error::new(
            ErrorKind::Auth,
            "The Anthropic sign-in was refused",
        ));
    }
    let code = code.filter(|code| valid_token(code)).ok_or_else(invalid)?;
    // Some authorization pages return code#state in the code parameter too.
    if code.contains('#') {
        return callback_code(&code, expected);
    }
    Ok(code)
}

async fn serve(listener: &TcpListener, state: &str) -> Result<String, Error> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| Error::new(ErrorKind::Auth, "The Anthropic callback listener failed"))?;
        let request = tokio::time::timeout(Duration::from_secs(10), async {
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 16384 {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            Ok::<_, std::io::Error>(request)
        })
        .await;
        let Ok(Ok(request)) = request else {
            continue;
        };
        let line = String::from_utf8_lossy(&request);
        let target = line
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default();
        if !target.starts_with("/callback?") {
            let _ = respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        }
        let result = callback_code(&format!("http://127.0.0.1:54545{target}"), state);
        let (status, text) = if result.is_ok() {
            ("200 OK", "Authorization received. You can close this tab.")
        } else {
            (
                "400 Bad Request",
                "Sign-in failed. Return to xi and start again.",
            )
        };
        let _ = respond(&mut stream, status, text).await;
        return result;
    }
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    text: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Signer as _;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tokens() -> Tokens {
        Tokens {
            access_token: "opaque-access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_at: 4_000_000_000,
            account_id: Some("account-1".into()),
            email: Some("test@example.invalid".into()),
        }
    }

    fn stored(dir: &Path, tokens: &Tokens) -> PathBuf {
        let file = anthropic_file(&dir.join("credentials"));
        oauth::save(&file, tokens).unwrap();
        file
    }

    fn response() -> Value {
        json!({
            "access_token": "opaque-access-2",
            "refresh_token": "refresh-2",
            "expires_in": 3600,
        })
    }

    async fn refresh_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("anthropic-beta", REFRESH_BETA))
            .and(header("user-agent", REFRESH_USER_AGENT))
            .and(body_json(json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "refresh-1",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(response()))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn callback_requires_unambiguous_matching_state_and_loopback_origin() {
        for text in [
            "code#state",
            " http://localhost:54545/callback?code=code&state=state ",
            "http://127.0.0.1:54545/callback?code=code&state=state",
            "http://localhost:54545/callback?code=code%23state&state=state",
        ] {
            assert_eq!(callback_code(text, "state").unwrap(), "code");
        }
        for text in [
            "",
            "code",
            "code#other",
            "code#",
            "#state",
            "code#state#extra",
            "bad code#state",
            "http://localhost:54545/callback?code=code",
            "http://localhost:54545/callback?state=state",
            "http://localhost:54545/callback?code=&state=state",
            "http://localhost:54545/callback?code=code&state=other",
            "http://localhost:54545/callback?code=code&state=state&state=other",
            "http://localhost:54545/callback?code=code&code=other&state=state",
            "http://localhost:54545/callback?code=code%23other&state=state",
            "http://localhost:54545/callback?code=code&state=state#other",
            "http://localhost:54545/callback?error=access_denied&state=state",
            "http://localhost:54545/callback?code=code&error=access_denied&state=state",
            "https://localhost:54545/callback?code=code&state=state",
            "http://example.invalid:54545/callback?code=code&state=state",
            "http://localhost:54546/callback?code=code&state=state",
            "http://localhost:54545/other?code=code&state=state",
            "http://user@localhost:54545/callback?code=code&state=state",
        ] {
            assert_eq!(
                callback_code(text, "state").unwrap_err().kind,
                ErrorKind::Auth
            );
        }
    }

    #[test]
    fn opaque_token_expiry_has_an_exact_five_minute_window() {
        let fresh = response_tokens(&response(), None, 1_000).unwrap();
        assert_eq!(fresh.expires_at, 4_600);
        assert!(!fresh.expiring(4_299));
        assert!(fresh.expiring(4_300));
        assert!(fresh.expiring(4_600));
        assert!(fresh.expiring(u64::MAX));
        assert!(fresh.account_id.is_none());
    }

    #[test]
    fn invalid_exchange_fields_and_expiry_overflow_are_rejected() {
        for (name, value) in [
            ("access_token", json!("")),
            ("access_token", json!("bad\nheader")),
            ("access_token", json!("space token")),
            ("access_token", Value::Null),
            ("refresh_token", json!("")),
            ("refresh_token", Value::Null),
            ("expires_in", json!(0)),
            ("expires_in", json!(-1)),
            ("expires_in", json!(1.5)),
            ("expires_in", json!("3600")),
            ("expires_in", Value::Null),
        ] {
            let mut body = response();
            body[name] = value;
            assert_eq!(
                response_tokens(&body, None, 1_000).unwrap_err().kind,
                ErrorKind::Auth
            );
        }
        for name in ["access_token", "refresh_token", "expires_in"] {
            let mut body = response();
            body.as_object_mut().unwrap().remove(name);
            assert!(response_tokens(&body, None, 1_000).is_err());
        }
        assert!(response_tokens(&response(), None, u64::MAX).is_err());
    }

    #[test]
    fn refresh_can_omit_rotation_and_identity_but_cannot_replace_them_with_empty_tokens() {
        let mut body = response();
        body.as_object_mut().unwrap().remove("refresh_token");
        let fresh = response_tokens(&body, Some(tokens()), 1_000).unwrap();
        assert_eq!(fresh.refresh_token, "refresh-1");
        assert_eq!(fresh.account_id.as_deref(), Some("account-1"));
        assert_eq!(fresh.email.as_deref(), Some("test@example.invalid"));
        body["refresh_token"] = json!("");
        assert!(response_tokens(&body, Some(tokens()), 1_000).is_err());
    }

    #[tokio::test]
    async fn exchange_posts_json_pkce_and_state_without_refresh_fingerprint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "grant_type": "authorization_code",
                "client_id": CLIENT_ID,
                "redirect_uri": REDIRECT,
                "code": "code",
                "code_verifier": "verifier",
                "state": "state",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "opaque-access",
                "refresh_token": "refresh",
                "expires_in": 3600,
                "account": {"uuid": "account", "email_address": "test@example.invalid"},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let fresh = exchange(
            &oauth::client().unwrap(),
            &format!("{}/token", server.uri()),
            "code",
            "verifier",
            "state",
        )
        .await
        .unwrap();
        assert_eq!(fresh.access_token, "opaque-access");
        assert_eq!(fresh.account_id.as_deref(), Some("account"));
        let requests = server.received_requests().await.unwrap();
        assert!(!requests[0].headers.contains_key("anthropic-beta"));
        assert!(!requests[0].headers.contains_key("user-agent"));
    }

    #[tokio::test]
    async fn concurrent_refreshes_reread_rotation_under_the_file_lock() {
        let server = refresh_server().await;
        let dir = tempfile::tempdir().unwrap();
        let file = stored(dir.path(), &tokens());
        let client = oauth::client().unwrap();
        let url = format!("{}/token", server.uri());
        let (first, second) = tokio::join!(
            renew(&client, &file, "opaque-access-1", &url),
            renew(&client, &file, "opaque-access-1", &url),
        );
        assert_eq!(first.unwrap().refresh_token, "refresh-2");
        assert_eq!(second.unwrap().refresh_token, "refresh-2");
        assert_eq!(load(&file).unwrap().access_token, "opaque-access-2");
        assert_eq!(
            load(&file).unwrap().account_id.as_deref(),
            Some("account-1")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_release_a_refresh_in_flight() {
        let server = MockServer::start().await;
        let arrived = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify = arrived.clone();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(move |_: &wiremock::Request| {
                notify.notify_one();
                ResponseTemplate::new(200)
                    .set_body_json(response())
                    .set_delay(Duration::from_millis(50))
            })
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file = stored(dir.path(), &tokens());
        let client = oauth::client().unwrap();
        let url = format!("{}/token", server.uri());
        let waiting = {
            let client = client.clone();
            let file = file.clone();
            let url = url.clone();
            tokio::spawn(async move { renew(&client, &file, "opaque-access-1", &url).await })
        };
        arrived.notified().await;
        waiting.abort();
        let fresh = renew(&client, &file, "opaque-access-1", &url)
            .await
            .unwrap();
        assert_eq!(fresh.refresh_token, "refresh-2");
        assert_eq!(load(&file).unwrap().refresh_token, "refresh-2");
    }

    #[tokio::test]
    async fn expiry_and_auth_rejection_both_refresh_and_signing_adds_only_bearer() {
        let server = refresh_server().await;
        let dir = tempfile::tempdir().unwrap();
        let mut old = tokens();
        old.expires_at = 1;
        let file = stored(dir.path(), &old);
        let signer = Signer::new(file);
        let client = oauth::client().unwrap();
        let url = format!("{}/token", server.uri());
        assert_eq!(
            signer.tokens(&client, &url).await.unwrap().access_token,
            "opaque-access-2"
        );
        let mut headers = HeaderMap::new();
        signer.sign(&client, &mut headers).await.unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[AUTHORIZATION], "Bearer opaque-access-2");
        assert!(headers[AUTHORIZATION].is_sensitive());
        // A rejection of the old token reuses the rotation rather than spending it twice.
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer opaque-access-1"),
        );
        assert!(signer.rejected(&client, &headers).await.unwrap());
        assert!(!signer.rejected(&client, &HeaderMap::new()).await.unwrap());
    }

    #[tokio::test]
    async fn rejection_refreshes_even_when_stored_expiry_is_not_close() {
        let server = refresh_server().await;
        let dir = tempfile::tempdir().unwrap();
        let signer = Signer::new(stored(dir.path(), &tokens()));
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer opaque-access-1"),
        );
        assert!(
            signer
                .retry(
                    &oauth::client().unwrap(),
                    &headers,
                    &format!("{}/token", server.uri()),
                )
                .await
                .unwrap()
        );
        assert_eq!(load(&signer.file).unwrap().refresh_token, "refresh-2");
    }

    #[tokio::test]
    async fn refused_and_malformed_refreshes_leave_stored_tokens_unchanged() {
        for reply in [
            ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
            ResponseTemplate::new(200).set_body_json(json!({"access_token": ""})),
            ResponseTemplate::new(200).set_body_string("not JSON"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(reply)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let file = stored(dir.path(), &tokens());
            let error = renew(
                &oauth::client().unwrap(),
                &file,
                "opaque-access-1",
                &format!("{}/token", server.uri()),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Auth);
            assert_eq!(load(&file).unwrap().refresh_token, "refresh-1");
        }
    }

    fn login(listener: TcpListener) -> Login {
        Login {
            listener,
            url: String::new(),
            verifier: "verifier".into(),
            state: "state".into(),
        }
    }

    #[tokio::test]
    async fn pasted_callback_and_eof_finish_without_waiting_for_a_browser() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let login = login(listener);
        assert_eq!(
            login
                .code(async { Some("code#state".into()) })
                .await
                .unwrap(),
            "code"
        );
        assert_eq!(
            login.code(async { None }).await.unwrap_err().kind,
            ErrorKind::Auth
        );
        let dir = tempfile::tempdir().unwrap();
        let file = anthropic_file(dir.path());
        assert!(login.finish(&file, async { None }).await.is_err());
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn browser_callback_is_validated_and_unrelated_paths_do_not_finish_login() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move { serve(&listener, "state").await });
        let client = oauth::client().unwrap();
        let response = client
            .get(format!("http://{address}/favicon.ico"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        let response = client
            .get(format!(
                "http://{address}/callback?code=browser&state=state"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(serving.await.unwrap().unwrap(), "browser");
    }

    #[test]
    fn malformed_and_publicly_readable_credentials_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = stored(dir.path(), &tokens());
        let mut invalid = tokens();
        invalid.access_token.clear();
        oauth::save(&file, &invalid).unwrap();
        assert_eq!(load(&file).unwrap_err().kind, ErrorKind::Auth);
        oauth::save(&file, &tokens()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(load(&file).unwrap_err().kind, ErrorKind::Auth);
        }
    }
}
