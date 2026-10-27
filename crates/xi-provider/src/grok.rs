//! Grok Build's public OAuth client, loopback PKCE flow and private tokens.
//!
//! Protocol reference: https://github.com/xai-org/grok-build/tree/main/crates/codegen/xai-grok-login

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xi_protocol::BoxFuture;

use crate::oauth;
use crate::transport::network;
use crate::{Error, ErrorKind};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
// Inference needs neither conversation nor workspace access.
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REFRESH_WINDOW: u64 = 5 * 60;

/// Where Grok OAuth tokens live inside a credential directory.
#[must_use]
pub fn grok_file(credentials: &Path) -> PathBuf {
    credentials.join("grok.json")
}

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

/// A Grok browser sign-in waiting for its loopback callback or pasted redirect.
pub struct Login {
    listener: TcpListener,
    url: String,
    redirect: String,
    verifier: String,
    state: String,
}

impl Login {
    /// Binds a random loopback port and builds the PKCE authorization URL.
    ///
    /// # Errors
    /// Returns `Config` if the listener or system random source is unavailable.
    pub async fn start() -> Result<Self, Error> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| Error::new(ErrorKind::Config, "Cannot bind the Grok callback listener"))?;
        let address = listener.local_addr().map_err(|_| {
            Error::new(
                ErrorKind::Config,
                "Cannot inspect the Grok callback listener",
            )
        })?;
        let redirect = format!("http://{address}/callback");
        let (verifier, challenge) = oauth::pkce()?;
        let state = URL_SAFE_NO_PAD.encode(oauth::random::<32>()?);
        let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("the authorization URL is valid");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &redirect)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("referrer", "xi");
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

    /// Waits up to ten minutes for a callback or pasted redirect URL, exchanges
    /// the code, and atomically saves the tokens with owner-only permissions.
    ///
    /// # Errors
    /// Returns `Auth` on denial, timeout or invalid credentials, `Network` on
    /// transport failure, and `Config` if saving fails.
    pub async fn finish(
        self,
        file: &Path,
        pasted: impl Future<Output = Option<String>>,
    ) -> Result<(), Error> {
        let code = self.code(pasted).await?;
        let tokens = token_request(
            &oauth::client()?,
            TOKEN_URL,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &code),
                ("redirect_uri", &self.redirect),
                ("code_verifier", &self.verifier),
            ],
            None,
        )
        .await?;
        oauth::save(file, &tokens)
    }

    async fn code(&self, pasted: impl Future<Output = Option<String>>) -> Result<String, Error> {
        tokio::time::timeout(Duration::from_secs(600), async {
            tokio::select! {
                code = serve(&self.listener, &self.redirect, &self.state) => code,
                text = pasted => match text {
                    Some(text) => callback(&text, &self.redirect, &self.state)?.code(),
                    // Redirected stdin must not disable the browser callback.
                    None => serve(&self.listener, &self.redirect, &self.state).await,
                },
            }
        })
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::Auth,
                "Grok sign-in timed out; run `xi login grok`",
            )
        })?
    }
}

pub(crate) struct Signer {
    file: PathBuf,
}

impl Signer {
    pub(crate) fn new(file: PathBuf) -> Self {
        Self { file }
    }

    async fn tokens(&self, client: &reqwest::Client, url: &str) -> Result<Tokens, Error> {
        let tokens = load(&self.file)?;
        if tokens.expires_at <= unix_now()?.saturating_add(REFRESH_WINDOW) {
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
            headers.insert("x-xai-token-auth", HeaderValue::from_static("xai-grok-cli"));
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

// An owned task keeps the cross-process lock until a rotated refresh token is
// saved, even when the model request that triggered the refresh is cancelled.
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
        let _lock = oauth::lock(&file, "Grok").await?;
        let current = load(&file)?;
        if current.access_token != stale {
            return Ok(current);
        }
        let fresh = token_request(
            &client,
            &url,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", &current.refresh_token),
            ],
            Some(&current),
        )
        .await?;
        oauth::save(&file, &fresh)?;
        Ok(fresh)
    })
    .await
    .map_err(|_| Error::new(ErrorKind::Auth, "Grok token refresh task failed"))?
}

async fn token_request(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    previous: Option<&Tokens>,
) -> Result<Tokens, Error> {
    let response = client
        .post(url)
        .form(form)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(network)?;
    let status = response.status();
    if !status.is_success() {
        let kind = match status.as_u16() {
            429 => ErrorKind::RateLimited,
            500..=599 => ErrorKind::Unavailable,
            _ => ErrorKind::Auth,
        };
        // Never include the response body; it may contain credentials.
        return Err(Error::new(
            kind,
            format!(
                "Grok token request failed with HTTP {status}; run `xi login grok` if sign-in was revoked"
            ),
        ));
    }
    let body = response.json().await.map_err(|_| malformed())?;
    response_tokens(&body, previous, unix_now()?)
}

fn response_tokens(body: &Value, previous: Option<&Tokens>, now: u64) -> Result<Tokens, Error> {
    let access_token = body["access_token"]
        .as_str()
        .filter(|token| valid_token(token))
        .ok_or_else(malformed)?
        .to_owned();
    let refresh_token = match body.get("refresh_token") {
        Some(value) => value.as_str().filter(|token| valid_token(token)),
        None => previous.map(|tokens| tokens.refresh_token.as_str()),
    }
    .ok_or_else(malformed)?
    .to_owned();
    let expires_at = body["expires_in"]
        .as_u64()
        .filter(|seconds| *seconds != 0)
        .and_then(|seconds| now.checked_add(seconds))
        .ok_or_else(malformed)?;
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at,
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
    Error::new(ErrorKind::Auth, "The Grok credential is malformed")
}

fn load(file: &Path) -> Result<Tokens, Error> {
    let tokens: Tokens = oauth::load(file, "Grok", "Not signed in to Grok; run `xi login grok`")?;
    if !valid_token(&tokens.access_token)
        || !valid_token(&tokens.refresh_token)
        || tokens.expires_at == 0
    {
        return Err(malformed());
    }
    Ok(tokens)
}

enum Callback {
    Code(String),
    Denied,
}

impl Callback {
    fn code(self) -> Result<String, Error> {
        match self {
            Self::Code(code) => Ok(code),
            Self::Denied => Err(Error::new(ErrorKind::Auth, "The Grok sign-in was refused")),
        }
    }
}

fn callback(text: &str, redirect: &str, expected: &str) -> Result<Callback, Error> {
    let invalid = || {
        Error::new(
            ErrorKind::Auth,
            "The Grok callback does not match this login",
        )
    };
    let url = reqwest::Url::parse(text.trim()).map_err(|_| invalid())?;
    let base = reqwest::Url::parse(redirect).map_err(|_| invalid())?;
    if url.origin() != base.origin()
        || url.path() != base.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (name, value) in url.query_pairs() {
        let field = match name.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return Err(invalid());
        }
    }
    if state.as_deref() != Some(expected) || (code.is_some() && error.is_some()) {
        return Err(invalid());
    }
    if error.is_some() {
        return Ok(Callback::Denied);
    }
    Ok(Callback::Code(
        code.filter(|code| valid_token(code)).ok_or_else(invalid)?,
    ))
}

async fn serve(listener: &TcpListener, redirect: &str, state: &str) -> Result<String, Error> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| Error::new(ErrorKind::Auth, "The Grok callback listener failed"))?;
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
        let request = String::from_utf8_lossy(&request);
        let mut line = request
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        let method = line.next().unwrap_or_default();
        let target = line.next().unwrap_or_default();
        let preflight =
            method == "OPTIONS" && (target == "/callback" || target.starts_with("/callback?"));
        let result = match (method, target.strip_prefix("/callback?")) {
            ("GET", Some(query)) => callback(&format!("{redirect}?{query}"), redirect, state),
            _ => Err(Error::new(ErrorKind::Auth, "Not a Grok callback")),
        };
        let (status, text) = match &result {
            _ if preflight => ("204 No Content", ""),
            Ok(Callback::Code(_)) => ("200 OK", "Authorization received. You can close this tab."),
            _ => (
                "400 Bad Request",
                "Sign-in failed. Return to xi and try again.",
            ),
        };
        // The accounts page may fetch the loopback URL instead of navigating.
        // Match Grok Build's CORS allowlist, never arbitrary browser origins.
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: https://accounts.x.ai\r\nAccess-Control-Allow-Methods: GET\r\nAccess-Control-Allow-Private-Network: true\r\nConnection: close\r\n\r\n{text}",
            text.len(),
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            stream.write_all(response.as_bytes()),
        )
        .await;
        // Unsolicited callbacks must not terminate the real login.
        if let Ok(callback) = result {
            return callback.code();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Signer as _;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    fn tokens() -> Tokens {
        Tokens {
            access_token: "opaque-access".into(),
            refresh_token: "opaque-refresh".into(),
            expires_at: unix_now().unwrap() + 3600,
        }
    }

    #[tokio::test]
    async fn authorization_uses_pkce_and_a_random_loopback_redirect() {
        let login = Login::start().await.unwrap();
        let url = reqwest::Url::parse(login.url()).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.origin().ascii_serialization(), "https://auth.x.ai");
        assert_eq!(url.path(), "/oauth2/authorize");
        assert_eq!(query["client_id"], CLIENT_ID);
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["redirect_uri"], login.redirect);
        assert_eq!(query["scope"], SCOPE);
        assert_eq!(query["state"], login.state);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["referrer"], "xi");
        assert_eq!(
            query["code_challenge"],
            URL_SAFE_NO_PAD.encode(aws_lc_rs::digest::digest(
                &aws_lc_rs::digest::SHA256,
                login.verifier.as_bytes(),
            ))
        );
        assert!(login.listener.local_addr().unwrap().ip().is_loopback());
        assert_ne!(login.listener.local_addr().unwrap().port(), 0);
    }

    #[test]
    fn callbacks_require_the_exact_redirect_and_unique_matching_state() {
        let redirect = "http://127.0.0.1:12345/callback";
        assert_eq!(
            callback(
                &format!("{redirect}?code=a%2Bb&state=ours"),
                redirect,
                "ours"
            )
            .unwrap()
            .code()
            .unwrap(),
            "a+b"
        );
        for text in [
            "raw-code",
            "http://127.0.0.1:12345/callback?code=secret",
            "http://127.0.0.1:12345/callback?code=secret&state=other",
            "http://127.0.0.1:12345/callback?code=secret&state=ours&state=ours",
            "http://127.0.0.1:12345/callback?code=secret&code=second&state=ours",
            "http://127.0.0.1:12345/callback?code=secret&error=denied&state=ours",
            "http://127.0.0.1:12345/callback?code=&state=ours",
            "http://127.0.0.1:12345/callback?code=a%0Ab&state=ours",
            "http://127.0.0.1:12345/callback?code=secret&state=ours#fragment",
            "http://127.0.0.1:12346/callback?code=secret&state=ours",
            "http://localhost:12345/callback?code=secret&state=ours",
            "https://127.0.0.1:12345/callback?code=secret&state=ours",
            "http://user@127.0.0.1:12345/callback?code=secret&state=ours",
            "http://127.0.0.1:12345/other?code=secret&state=ours",
        ] {
            assert!(callback(text, redirect, "ours").is_err(), "{text}");
        }
        let error = callback(
            &format!("{redirect}?error=denied&state=ours&error_description=secret"),
            redirect,
            "ours",
        )
        .unwrap()
        .code()
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(!error.message.contains("secret"));
    }

    #[tokio::test]
    async fn pasted_redirect_and_stdin_eof_both_work() {
        let login = Login::start().await.unwrap();
        let text = format!("{}?code=pasted&state={}", login.redirect, login.state);
        assert_eq!(login.code(async { Some(text) }).await.unwrap(), "pasted");

        let url = format!("{}?code=browser&state={}", login.redirect, login.state);
        let invalid = format!("{}?code=unrelated&state=wrong", login.redirect);
        let browser = async {
            let client = oauth::client().unwrap();
            let preflight = client
                .request(reqwest::Method::OPTIONS, &login.redirect)
                .header("origin", "https://accounts.x.ai")
                .header("access-control-request-method", "GET")
                .header("access-control-request-private-network", "true")
                .send()
                .await
                .unwrap();
            assert_eq!(preflight.status(), 204);
            assert_eq!(
                preflight.headers()["access-control-allow-origin"],
                "https://accounts.x.ai"
            );
            assert_eq!(
                preflight.headers()["access-control-allow-private-network"],
                "true"
            );
            assert_eq!(client.get(invalid).send().await.unwrap().status(), 400);
            assert_eq!(client.get(url).send().await.unwrap().status(), 200);
        };
        let (code, ()) = tokio::join!(login.code(async { None }), browser);
        assert_eq!(code.unwrap(), "browser");
    }

    #[tokio::test]
    async fn token_exchange_is_form_encoded_and_uses_server_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains(format!("client_id={CLIENT_ID}")))
            .and(body_string_contains("code=code%2Bvalue"))
            .and(body_string_contains("code_verifier=verifier"))
            .and(body_string_contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Fcallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "opaque-access", "refresh_token": "opaque-refresh", "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server).await;
        let before = unix_now().unwrap();
        let fresh = token_request(
            &oauth::client().unwrap(),
            &format!("{}/token", server.uri()),
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", "code+value"),
                ("code_verifier", "verifier"),
                ("redirect_uri", "http://127.0.0.1:12345/callback"),
            ],
            None,
        )
        .await
        .unwrap();
        assert_eq!(fresh.access_token, "opaque-access");
        assert!(fresh.expires_at >= before + 3600);
        assert!(fresh.expires_at <= unix_now().unwrap() + 3600);
    }

    #[test]
    fn malformed_tokens_are_rejected_and_an_omitted_refresh_token_is_retained() {
        let old = tokens();
        let body = json!({"access_token": "new-access", "expires_in": 3600});
        let fresh = response_tokens(&body, Some(&old), 100).unwrap();
        assert_eq!(fresh.refresh_token, old.refresh_token);
        assert_eq!(fresh.expires_at, 3700);
        assert!(response_tokens(&body, None, 100).is_err());
        let body = json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600});
        assert_eq!(
            response_tokens(&body, Some(&old), 100)
                .unwrap()
                .refresh_token,
            "new-refresh"
        );
        for (key, value) in [
            ("access_token", json!("")),
            ("access_token", json!("secret\nheader")),
            ("refresh_token", Value::Null),
            ("refresh_token", json!("")),
            ("expires_in", json!(0)),
            ("expires_in", json!(-1)),
            ("expires_in", json!("3600")),
            ("expires_in", json!(u64::MAX)),
        ] {
            let mut invalid = body.clone();
            invalid[key] = value;
            assert!(response_tokens(&invalid, Some(&old), 100).is_err());
        }
    }

    #[tokio::test]
    async fn signing_uses_sensitive_bearer_and_grok_token_routing() {
        let dir = tempfile::tempdir().unwrap();
        let file = grok_file(dir.path());
        oauth::save(&file, &tokens()).unwrap();
        let mut headers = HeaderMap::new();
        Signer::new(file)
            .sign(&oauth::client().unwrap(), &mut headers)
            .await
            .unwrap();
        assert_eq!(headers[AUTHORIZATION], "Bearer opaque-access");
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert_eq!(headers["x-xai-token-auth"], "xai-grok-cli");
    }

    #[tokio::test]
    async fn concurrent_refreshes_rotate_once_and_a_rejection_reuses_new_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let file = grok_file(dir.path());
        let mut old = tokens();
        old.expires_at = unix_now().unwrap() + 30;
        oauth::save(&file, &old).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains(format!("client_id={CLIENT_ID}")))
            .and(body_string_contains("refresh_token=opaque-refresh"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(50))
                .set_body_json(json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600})))
            .expect(1).mount(&server).await;
        let client = oauth::client().unwrap();
        let signer = Signer::new(file.clone());
        let url = server.uri();
        let (first, second) =
            tokio::join!(signer.tokens(&client, &url), signer.tokens(&client, &url));
        assert_eq!(first.unwrap().access_token, "new-access");
        assert_eq!(second.unwrap().refresh_token, "new-refresh");
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer opaque-access"),
        );
        assert!(signer.retry(&client, &headers, &url).await.unwrap());
        assert_eq!(load(&file).unwrap().refresh_token, "new-refresh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn rejected_unexpired_tokens_refresh_and_preserve_an_omitted_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = grok_file(dir.path());
        oauth::save(&file, &tokens()).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": "new-access", "expires_in": 3600})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer opaque-access"),
        );
        assert!(
            Signer::new(file.clone())
                .retry(&oauth::client().unwrap(), &headers, &server.uri())
                .await
                .unwrap()
        );
        let fresh = load(&file).unwrap();
        assert_eq!(fresh.access_token, "new-access");
        assert_eq!(fresh.refresh_token, "opaque-refresh");
    }

    #[tokio::test]
    async fn token_errors_preserve_credentials_and_do_not_disclose_response_bodies() {
        for (status, kind) in [
            (400, ErrorKind::Auth),
            (429, ErrorKind::RateLimited),
            (503, ErrorKind::Unavailable),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = grok_file(dir.path());
            oauth::save(&file, &tokens()).unwrap();
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string("secret-token"))
                .expect(1)
                .mount(&server)
                .await;
            let error = renew(
                &oauth::client().unwrap(),
                &file,
                "opaque-access",
                &server.uri(),
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.kind, kind);
            assert!(!error.message.contains("secret-token"));
            assert!(error.message.contains("xi login grok"));
            assert_eq!(load(&file).unwrap().refresh_token, "opaque-refresh");
        }
    }

    #[tokio::test]
    async fn cancellation_does_not_lose_a_rotated_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let file = grok_file(dir.path());
        oauth::save(&file, &tokens()).unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(100))
                .set_body_json(json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600})))
            .expect(1).mount(&server).await;
        let refresh = {
            let file = file.clone();
            let url = server.uri();
            tokio::spawn(async move {
                renew(&oauth::client().unwrap(), &file, "opaque-access", &url).await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        refresh.abort();
        let _ = refresh.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while load(&file).unwrap().refresh_token != "new-refresh" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn stored_credentials_are_validated_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = grok_file(dir.path());
        assert!(load(&file).err().unwrap().message.contains("xi login grok"));
        let mut invalid = tokens();
        invalid.access_token = "secret\nheader".into();
        oauth::save(&file, &invalid).unwrap();
        assert!(load(&file).is_err());
        oauth::save(&file, &tokens()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load(&file).is_err());
        }
    }
}
