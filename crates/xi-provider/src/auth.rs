use reqwest::header::HeaderMap;
use xi_protocol::BoxFuture;

use crate::Error;
use crate::transport::Signer;

pub(crate) enum LoginSigner {
    Chatgpt(xi_auth::ChatgptCredentials),
    Anthropic(xi_auth::AnthropicCredentials),
    Grok(xi_auth::GrokCredentials),
}

impl Signer for LoginSigner {
    fn sign<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a mut HeaderMap,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            match self {
                Self::Chatgpt(tokens) => tokens.sign(client, headers).await,
                Self::Anthropic(tokens) => tokens.sign(client, headers).await,
                Self::Grok(tokens) => tokens.sign(client, headers).await,
            }
            .map_err(Error::from)
        })
    }

    fn rejected<'a>(
        &'a self,
        client: &'a reqwest::Client,
        headers: &'a HeaderMap,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            match self {
                Self::Chatgpt(tokens) => tokens.rejected(client, headers).await,
                Self::Anthropic(tokens) => tokens.rejected(client, headers).await,
                Self::Grok(tokens) => tokens.rejected(client, headers).await,
            }
            .map_err(Error::from)
        })
    }
}
