//! Private PKCE, HTTP and credential-file mechanics shared by browser logins.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Error, ErrorKind};

pub(crate) fn pkce() -> Result<(String, String), Error> {
    let verifier = URL_SAFE_NO_PAD.encode(random::<64>()?);
    let challenge = URL_SAFE_NO_PAD.encode(aws_lc_rs::digest::digest(
        &aws_lc_rs::digest::SHA256,
        verifier.as_bytes(),
    ));
    Ok((verifier, challenge))
}

pub(crate) fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    aws_lc_rs::rand::fill(&mut bytes)
        .map_err(|_| Error::new(ErrorKind::Config, "The system random source failed"))?;
    Ok(bytes)
}

pub(crate) fn client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| Error::new(ErrorKind::Config, error.to_string()))
}

pub(crate) async fn lock(file: &Path, service: &'static str) -> Result<std::fs::File, Error> {
    let mut path = file.as_os_str().to_owned();
    path.push(".lock");
    let path = PathBuf::from(path);
    tokio::task::spawn_blocking(move || {
        if let Ok(metadata) = std::fs::symlink_metadata(&path)
            && !metadata.is_file()
        {
            return Err(std::io::Error::other("not a regular lock file"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = options.open(path)?;
        lock.lock()?;
        Ok::<_, std::io::Error>(lock)
    })
    .await
    .map_err(|_| Error::new(ErrorKind::Auth, format!("{service} token lock task failed")))?
    .map_err(|error| {
        Error::new(
            ErrorKind::Auth,
            format!("Cannot lock the {service} credential: {error}"),
        )
    })
}

pub(crate) fn load<T: DeserializeOwned>(
    file: &Path,
    service: &str,
    missing: &str,
) -> Result<T, Error> {
    let fail = || {
        Error::new(
            ErrorKind::Auth,
            format!("Cannot read the {service} credential"),
        )
    };
    let metadata = match std::fs::symlink_metadata(file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::new(ErrorKind::Auth, missing));
        }
        Err(_) => return Err(fail()),
    };
    if !metadata.is_file() {
        return Err(fail());
    }
    let mut input = std::fs::File::open(file).map_err(|_| fail())?;
    let opened = input.metadata().map_err(|_| {
        Error::new(
            ErrorKind::Auth,
            format!("Cannot inspect the {service} credential"),
        )
    })?;
    if !opened.is_file() {
        return Err(fail());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err(fail());
        }
        if opened.permissions().mode() & 0o7177 != 0 {
            return Err(Error::new(
                ErrorKind::Auth,
                format!("The {service} credential must have mode 0600 or stricter"),
            ));
        }
    }
    let mut text = String::new();
    input.read_to_string(&mut text).map_err(|_| fail())?;
    serde_json::from_str(&text).map_err(|_| {
        Error::new(
            ErrorKind::Auth,
            format!("The {service} credential is malformed"),
        )
    })
}

/// Atomically replaces a credential using a new, private file in the same directory.
pub(crate) fn save<T: Serialize>(file: &Path, tokens: &T) -> Result<(), Error> {
    let fail = |error: std::io::Error| {
        Error::new(
            ErrorKind::Config,
            format!("Cannot save {}: {error}", file.display()),
        )
    };
    let dir = file.parent().unwrap_or(Path::new("."));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(fail)?;
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", URL_SAFE_NO_PAD.encode(random::<16>()?)));
    let temporary = PathBuf::from(temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&temporary).map_err(fail)?;
    let result = (|| {
        serde_json::to_writer_pretty(&mut out, tokens)
            .map_err(|error| fail(std::io::Error::other(error)))?;
        out.flush().and_then(|()| out.sync_all()).map_err(fail)?;
        std::fs::rename(&temporary, file).map_err(fail)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::Error as _;
    use serde_json::{Value, json};

    #[test]
    fn failed_serialization_preserves_the_previous_file_and_removes_temporary() {
        struct Invalid;

        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(S::Error::custom("deliberate serialization failure"))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tokens.json");
        save(&file, &json!({"previous": true})).unwrap();
        assert!(save(&file, &Invalid).is_err());
        assert_eq!(
            load::<Value>(&file, "Test", "missing").unwrap(),
            json!({"previous": true}),
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn credential_and_lock_symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        save(&target, &json!({"private": true})).unwrap();
        let file = dir.path().join("tokens.json");
        symlink(&target, &file).unwrap();
        assert!(load::<Value>(&file, "Test", "missing").is_err());
        symlink(&target, dir.path().join("tokens.json.lock")).unwrap();
        assert!(lock(&file, "Test").await.is_err());
        // Saving replaces the link itself, never the file it pointed at.
        save(&file, &json!({"replacement": true})).unwrap();
        assert_eq!(
            load::<Value>(&target, "Test", "missing").unwrap(),
            json!({"private": true}),
        );
        assert_eq!(
            load::<Value>(&file, "Test", "missing").unwrap(),
            json!({"replacement": true}),
        );
    }
}
