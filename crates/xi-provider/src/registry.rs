use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use reqwest::header::{HeaderName, HeaderValue};

use crate::transport::{HttpProvider, Key, Shape, Signer};
use crate::*;

impl Registry {
    /// The built-in providers and models, with base standard-tier pricing.
    #[must_use]
    pub fn builtin() -> Self {
        let config: RegistryConfig = toml::from_str(include_str!("models.toml"))
            .expect("the bundled model registry is valid");
        let mut registry = Self::default();
        registry.extend(config);
        registry
    }

    /// Sets the directory holding credentials that xi manages itself, such
    /// as the ChatGPT tokens. It must not be readable inside the sandbox.
    pub fn set_credentials(&mut self, dir: PathBuf) {
        self.credentials = Some(dir);
    }

    /// Adds or overrides providers and models from a config layer.
    pub fn extend(&mut self, config: RegistryConfig) {
        self.providers.extend(config.provider);
        for model in config.model {
            if let Some(existing) = self.models.iter_mut().find(|existing| {
                existing.info.provider == model.info.provider && existing.info.id == model.info.id
            }) {
                *existing = model;
            } else {
                self.models.push(model);
            }
        }
    }

    /// Metadata for a model. Unknown models have no advertised metadata.
    #[must_use]
    pub fn model(&self, model: &ModelRef) -> Option<&ModelInfo> {
        self.models
            .iter()
            .map(|entry| &entry.info)
            .find(|info| info.provider == model.provider && info.id == model.model)
    }

    /// All known models.
    pub fn models(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.iter().map(|entry| &entry.info)
    }

    /// Connection settings for a provider key.
    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.get(name)
    }

    /// Builds an adapter, sharing its provider's connection pool.
    ///
    /// Unknown model ids connect with reasoning disabled unless configured.
    ///
    /// # Errors
    /// Returns `Config` for unknown providers, invalid URLs, invalid compat
    /// settings and a login without a credential directory, and `Auth` for
    /// missing, empty or unreadable credentials and insecure credential files.
    pub fn connect(&self, model: &ModelRef) -> Result<Arc<dyn Provider>, Error> {
        let config = self.providers.get(&model.provider).ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                format!("Unknown provider: {}", model.provider),
            )
        })?;
        let url = reqwest::Url::parse(&config.base_url)
            .map_err(|_| Error::new(ErrorKind::Config, "Invalid provider base URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::new(
                ErrorKind::Config,
                "Provider URL must be an HTTP(S) base URL without credentials, query or fragment",
            ));
        }
        let signer = signer(config, self.credentials.as_deref())?;
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| Error::new(ErrorKind::Config, "Provider client lock is poisoned"))?;
        let client = match clients.entry(model.provider.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.get().clone(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(30))
                    .read_timeout(Duration::from_secs(120))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|error| Error::new(ErrorKind::Config, error.to_string()))?;
                entry.insert(client).clone()
            }
        };
        let shapes = self
            .models
            .iter()
            .filter(|entry| entry.info.provider == model.provider)
            .map(|entry| {
                let shape = Shape::new(
                    entry.info.reasoning.clone(),
                    &entry.compat.or(&config.compat),
                )?;
                Ok((entry.info.id.clone(), shape))
            })
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        Ok(Arc::new(HttpProvider::new(client, config, signer, shapes)?))
    }
}

fn signer(config: &ProviderConfig, credentials: Option<&Path>) -> Result<Box<dyn Signer>, Error> {
    if let Auth::Login(LoginService::Chatgpt) = config.auth {
        let credentials = credentials.ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                "No credential directory for ChatGPT tokens",
            )
        })?;
        return Ok(Box::new(chatgpt::Signer::new(chatgpt_file(credentials))));
    }
    let Some(credential) = resolve_auth(&config.auth)? else {
        return Ok(Box::new(Key(None)));
    };
    let (name, value) = match config.api {
        Api::AnthropicMessages => ("x-api-key", credential),
        Api::OpenaiChat | Api::OpenaiResponses => ("authorization", format!("Bearer {credential}")),
    };
    let mut value = HeaderValue::from_str(&value).map_err(|_| {
        Error::new(
            ErrorKind::Auth,
            "Credential contains invalid header characters",
        )
    })?;
    value.set_sensitive(true);
    Ok(Box::new(Key(Some((HeaderName::from_static(name), value)))))
}

fn resolve_auth(auth: &Auth) -> Result<Option<String>, Error> {
    let raw = match auth {
        Auth::None | Auth::Login(_) => return Ok(None),
        Auth::Env(name) => std::env::var(name).map_err(|_| {
            Error::new(
                ErrorKind::Auth,
                format!("Credential environment variable {name} is missing or not Unicode"),
            )
        })?,
        Auth::File(path) => {
            let mut file = std::fs::File::open(path)
                .map_err(|_| Error::new(ErrorKind::Auth, "Cannot open credential file"))?;
            let metadata = file
                .metadata()
                .map_err(|_| Error::new(ErrorKind::Auth, "Cannot inspect credential file"))?;
            if !metadata.is_file() {
                return Err(Error::new(
                    ErrorKind::Auth,
                    "Credential path is not a regular file",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o7177 != 0 {
                    return Err(Error::new(
                        ErrorKind::Auth,
                        "Credential file must have mode 0600 or stricter",
                    ));
                }
            }
            let mut raw = String::new();
            file.read_to_string(&mut raw)
                .map_err(|_| Error::new(ErrorKind::Auth, "Cannot read credential file"))?;
            raw
        }
    };
    let credential = raw.trim();
    if credential.is_empty() {
        return Err(Error::new(ErrorKind::Auth, "Credential is empty"));
    }
    Ok(Some(credential.to_owned()))
}
