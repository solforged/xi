//! Sign in with ChatGPT: the Codex CLI's OAuth flow and the stored tokens.
//!
//! [`Login`] runs the browser authorization-code flow with PKCE against a
//! loopback callback and saves the tokens to a `0600` file. Requests read
//! that file, refresh the access token within five minutes of its expiry or
//! after a 401, and write the rotated tokens back under a lock file so
//! several xi processes never spend the same refresh token twice.

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

use crate::oauth::{client, random, save};
use crate::transport::network;
use crate::{Error, ErrorKind};

const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const SCOPE: &str = "openid profile email offline_access";
/// Sent as `originator` on authorization and model requests.
const ORIGINATOR: &str = "xi";
/// The redirect ports registered for the Codex client, in preference order.
const PORTS: [u16; 2] = [1455, 1457];
const REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Where the ChatGPT tokens live inside a credential directory.
#[must_use]
pub fn chatgpt_file(credentials: &Path) -> PathBuf {
    credentials.join("chatgpt.json")
}

/// Tokens as stored on disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
}

/// What a completed sign-in reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    /// The account's email address, when the identity token carries one.
    pub email: Option<String>,
    /// The ChatGPT plan, for example `plus` or `pro`.
    pub plan: Option<String>,
}

/// A started browser sign-in, waiting for its callback.
pub struct Login {
    listener: TcpListener,
    url: String,
    redirect: String,
    verifier: String,
    state: String,
}

impl Login {
    /// Binds the loopback callback server and builds the authorization URL.
    ///
    /// # Errors
    /// Returns `Config` when neither registered callback port is free.
    pub async fn start() -> Result<Self, Error> {
        let mut bound = None;
        for port in PORTS {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                bound = Some((listener, port));
                break;
            }
        }
        let (listener, port) = bound.ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "Ports 1455 and 1457 are busy; close the other sign-in and retry",
            )
        })?;
        let (verifier, challenge) = crate::oauth::pkce()?;
        let state = URL_SAFE_NO_PAD.encode(random::<32>()?);
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let mut url = reqwest::Url::parse(&format!("{ISSUER}/oauth/authorize"))
            .expect("the issuer URL is valid");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("scope", SCOPE)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
        Ok(Self {
            listener,
            url: url.into(),
            redirect,
            verifier,
            state,
        })
    }

    /// The URL to open in a browser.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Waits for the browser callback, or for `pasted` to yield the
    /// redirected URL copied from a browser on another machine, then
    /// exchanges the code and saves the tokens to `file`.
    ///
    /// # Errors
    /// Returns `Auth` when the authorization is denied or the state does not
    /// match, and `Config` when the tokens cannot be saved.
    pub async fn finish(
        self,
        file: &Path,
        pasted: impl Future<Output = Option<String>>,
    ) -> Result<Account, Error> {
        let code = tokio::select! {
            code = serve(&self.listener, &self.state) => code?,
            Some(text) = pasted => callback_code(&text, &self.state)?,
        };
        let client = client()?;
        let tokens = exchange(
            &client,
            &format!("{ISSUER}/oauth/token"),
            &code,
            &self.redirect,
            &self.verifier,
        )
        .await?;
        save(file, &tokens)?;
        let claims = claims(&tokens.id_token).unwrap_or_default();
        Ok(Account {
            email: claims["email"].as_str().map(str::to_owned),
            plan: claims[AUTH_CLAIM]["chatgpt_plan_type"]
                .as_str()
                .map(str::to_owned),
        })
    }
}

/// Signs model requests with the stored tokens: the bearer token, the
/// account id and the `originator` the sign-in used.
pub(crate) struct Signer {
    file: PathBuf,
}

impl Signer {
    pub(crate) fn new(file: PathBuf) -> Self {
        Self { file }
    }
}

impl crate::transport::Signer for Signer {
    /// Refreshes the tokens first when the access token expires within five
    /// minutes.
    fn sign<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a mut HeaderMap,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let tokens = load(&self.file)?;
            let tokens = if expiring(&tokens.access_token, SystemTime::now()) {
                renew(client, &self.file, &tokens.access_token, &token_url()).await?
            } else {
                tokens
            };
            let invalid = |_| Error::new(ErrorKind::Auth, "The ChatGPT credential is malformed");
            let mut bearer = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
                .map_err(invalid)?;
            bearer.set_sensitive(true);
            headers.insert(AUTHORIZATION, bearer);
            headers.insert(
                "chatgpt-account-id",
                HeaderValue::from_str(&tokens.account_id).map_err(invalid)?,
            );
            headers.insert("originator", HeaderValue::from_static(ORIGINATOR));
            Ok(())
        })
    }

    /// Refreshes the rejected token, unless another process already has.
    fn rejected<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let Some(rejected) = headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
            else {
                return Ok(false);
            };
            renew(client, &self.file, rejected, &token_url()).await?;
            Ok(true)
        })
    }
}

fn token_url() -> String {
    format!("{ISSUER}/oauth/token")
}

/// Refreshes the stored tokens if they still hold `stale`. Holding the lock
/// file across the request keeps a rotated refresh token from being used
/// twice by concurrent xi processes.
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
    // The owned task keeps the lock until rotated tokens are saved even if
    // the model request waiting for this refresh is cancelled.
    tokio::spawn(async move {
        let _lock = crate::oauth::lock(&file, "ChatGPT").await?;
        let current = load(&file)?;
        if current.access_token != stale {
            return Ok(current);
        }
        let fresh = refresh(&client, &url, current).await?;
        save(&file, &fresh)?;
        Ok(fresh)
    })
    .await
    .map_err(|_| Error::new(ErrorKind::Auth, "ChatGPT token refresh task failed"))?
}

async fn refresh(client: &reqwest::Client, url: &str, tokens: Tokens) -> Result<Tokens, Error> {
    let response = client
        .post(url)
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": tokens.refresh_token,
        }))
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        let code = body["error"]["code"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or_default();
        let permanent = status == reqwest::StatusCode::UNAUTHORIZED
            || code.eq_ignore_ascii_case("invalid_grant")
            || code.starts_with("refresh_token_");
        return Err(if permanent {
            Error::new(ErrorKind::Auth, signed_out())
        } else {
            Error::new(
                ErrorKind::Unavailable,
                format!("ChatGPT token refresh failed with HTTP {status}"),
            )
        });
    }
    let field = |name: &str| body[name].as_str().map(str::to_owned);
    let id_token = field("id_token").unwrap_or(tokens.id_token);
    let access_token = field("access_token").unwrap_or(tokens.access_token);
    Ok(Tokens {
        account_id: account_id(&id_token, &access_token).unwrap_or(tokens.account_id),
        refresh_token: field("refresh_token").unwrap_or(tokens.refresh_token),
        id_token,
        access_token,
    })
}

async fn exchange(
    client: &reqwest::Client,
    url: &str,
    code: &str,
    redirect: &str,
    verifier: &str,
) -> Result<Tokens, Error> {
    let response = client
        .post(url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    let field = |name: &str| body[name].as_str().map(str::to_owned);
    match (
        status.is_success(),
        field("access_token"),
        field("refresh_token"),
        field("id_token"),
    ) {
        (true, Some(access_token), Some(refresh_token), Some(id_token)) => {
            let account_id = account_id(&id_token, &access_token).ok_or_else(|| {
                Error::new(
                    ErrorKind::Auth,
                    "The ChatGPT sign-in has no account id; is the account a ChatGPT plan?",
                )
            })?;
            Ok(Tokens {
                access_token,
                refresh_token,
                id_token,
                account_id,
            })
        }
        _ => Err(Error::new(
            ErrorKind::Auth,
            format!("ChatGPT sign-in token exchange failed with HTTP {status}"),
        )),
    }
}

/// Accepts loopback connections until one carries the callback.
async fn serve(listener: &TcpListener, state: &str) -> Result<String, Error> {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 16384 {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => request.extend_from_slice(&buffer[..read]),
            }
        }
        let line = String::from_utf8_lossy(&request);
        let target = line
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_default();
        if !target.starts_with("/auth/callback?") {
            let _ = respond(&mut stream, "404 Not Found", "Not found.").await;
            continue;
        }
        let result = callback_code(&format!("http://127.0.0.1{target}"), state);
        let page = match &result {
            Ok(_) => "Signed in to xi. You can close this tab.",
            Err(error) => &error.message,
        };
        let status = if result.is_ok() {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let _ = respond(&mut stream, status, page).await;
        return result;
    }
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    text: &str,
) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>xi</title><p style=\"font:16px system-ui;margin:3em\">{}</p>",
        text.replace('&', "&amp;").replace('<', "&lt;")
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// The authorization code in a callback URL, after checking its state.
fn callback_code(text: &str, state: &str) -> Result<String, Error> {
    let url = reqwest::Url::parse(text.trim()).map_err(|_| {
        Error::new(
            ErrorKind::Auth,
            "Paste the full URL the browser was sent to, starting with http://127.0.0.1",
        )
    })?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if param("state").as_deref() != Some(state) {
        return Err(Error::new(
            ErrorKind::Auth,
            "The sign-in callback does not match this login; start it again",
        ));
    }
    if let Some(error) = param("error") {
        let detail = param("error_description").unwrap_or(error);
        return Err(Error::new(
            ErrorKind::Auth,
            format!("ChatGPT sign-in was refused: {detail}"),
        ));
    }
    param("code")
        .filter(|code| !code.is_empty())
        .ok_or_else(|| Error::new(ErrorKind::Auth, "The sign-in callback has no code"))
}

/// The unverified payload of a JWT. Only used to read our own tokens.
fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn account_id(id_token: &str, access_token: &str) -> Option<String> {
    [id_token, access_token].into_iter().find_map(|token| {
        claims(token)?[AUTH_CLAIM]["chatgpt_account_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

fn expiring(access_token: &str, now: SystemTime) -> bool {
    let Some(exp) = claims(access_token).and_then(|claims| claims["exp"].as_u64()) else {
        return false;
    };
    UNIX_EPOCH + Duration::from_secs(exp) <= now + REFRESH_WINDOW
}

fn signed_out() -> String {
    "The ChatGPT sign-in has expired or was revoked; run `xi login chatgpt`".into()
}

fn load(file: &Path) -> Result<Tokens, Error> {
    crate::oauth::load(
        file,
        "ChatGPT",
        "Not signed in to ChatGPT; run `xi login chatgpt`",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn jwt(payload: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    fn access(exp: u64, account: &str) -> String {
        jwt(json!({"exp": exp, AUTH_CLAIM: {"chatgpt_account_id": account}}))
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn stored(dir: &Path, access_token: String) -> PathBuf {
        let file = dir.join("auth/chatgpt.json");
        save(
            &file,
            &Tokens {
                access_token,
                refresh_token: "refresh-1".into(),
                id_token: jwt(json!({})),
                account_id: "acct-old".into(),
            },
        )
        .unwrap();
        file
    }

    #[test]
    fn expiry_uses_a_five_minute_window() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(expiring(&access(1_000_299, "a"), now));
        assert!(!expiring(&access(1_000_301, "a"), now));
        assert!(!expiring("not-a-jwt", now));
    }

    #[test]
    fn callback_requires_this_login_state() {
        let url = "http://127.0.0.1:1455/auth/callback?code=abc&state=s1";
        assert_eq!(callback_code(url, "s1").unwrap(), "abc");
        assert_eq!(callback_code(url, "s2").unwrap_err().kind, ErrorKind::Auth);
        let denied = "http://127.0.0.1:1455/auth/callback?error=access_denied&state=s1";
        assert!(
            callback_code(denied, "s1")
                .unwrap_err()
                .message
                .contains("access_denied")
        );
        assert!(callback_code("code=abc&state=s1", "s1").is_err());
    }

    #[tokio::test]
    async fn refresh_rotates_tokens_once_and_keeps_the_file_private() {
        let server = MockServer::start().await;
        let fresh = access(now() + 3600, "acct-new");
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_json(json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "refresh-1",
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": fresh, "refresh_token": "refresh-2"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let stale = access(now() + 60, "acct-old");
        let file = stored(dir.path(), stale.clone());
        let url = format!("{}/oauth/token", server.uri());
        let client = client().unwrap();
        let renewed = renew(&client, &file, &stale, &url).await.unwrap();
        assert_eq!(renewed.access_token, fresh);
        assert_eq!(renewed.account_id, "acct-new");
        // A second caller still holding the stale token reuses the rotation.
        let again = renew(&client, &file, &stale, &url).await.unwrap();
        assert_eq!(again.refresh_token, "refresh-2");
        let saved = load(&file).unwrap();
        assert_eq!(saved.refresh_token, "refresh-2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn rejected_refresh_asks_for_a_new_sign_in() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": {"code": "refresh_token_reused"}})),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let stale = access(now(), "acct-old");
        let file = stored(dir.path(), stale.clone());
        let url = format!("{}/oauth/token", server.uri());
        let error = renew(&client().unwrap(), &file, &stale, &url)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(error.message.contains("xi login chatgpt"));
        assert_eq!(load(&file).unwrap().refresh_token, "refresh-1");
    }

    #[test]
    fn credentials_readable_by_others_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = stored(dir.path(), access(now() + 3600, "a"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load(&file).unwrap_err().message.contains("0600"));
        }
    }
}
