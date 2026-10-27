//! Subscription sign-in and private token storage.
//!
//! Browser logins use PKCE and loopback callbacks. Credentials refresh tokens
//! under a cross-process lock and add authentication headers without knowing
//! model request or response formats.

mod anthropic;
mod chatgpt;
mod grok;
mod oauth;

pub use anthropic::{Credentials as AnthropicCredentials, Login as AnthropicLogin, anthropic_file};
pub use chatgpt::{Account, Credentials as ChatgptCredentials, Login, chatgpt_file};
pub use grok::{Credentials as GrokCredentials, Login as GrokLogin, grok_file};

/// Why authentication failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A credential is missing, malformed, expired or rejected.
    Auth,
    /// Local configuration or credential storage failed.
    Config,
    /// The connection failed.
    Network,
    /// The token service rate-limited the request.
    RateLimited,
    /// The token service is temporarily unavailable.
    Unavailable,
}

/// A failed sign-in, credential read or token refresh.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    /// The failure class.
    pub kind: ErrorKind,
    /// Description safe to show without exposing tokens.
    pub message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

fn network(error: reqwest::Error) -> Error {
    Error::new(ErrorKind::Network, error.without_url().to_string())
}
