//! Per-user daemon runtime paths.

use std::io;
use std::path::{Path, PathBuf};

/// Overrides the per-user runtime directory.
pub const ENV: &str = "XI_RUNTIME_DIR";

/// Returns the per-user runtime directory without creating it.
///
/// Uses `$XI_RUNTIME_DIR` first. On Linux, uses `$XDG_RUNTIME_DIR/xi`, falling
/// back to `/run/user/<uid>/xi` with the uid read from `/proc/self` metadata.
/// On macOS, uses `$HOME/Library/Application Support/xi/run`.
///
/// # Errors
/// Fails when the platform default cannot be resolved.
pub fn dir() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(ENV) {
        return Ok(PathBuf::from(path));
    }
    platform_dir()
}

#[cfg(target_os = "linux")]
fn platform_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path).join("xi"));
    }
    let uid = std::fs::metadata("/proc/self")?.uid();
    Ok(PathBuf::from("/run/user").join(uid.to_string()).join("xi"))
}

#[cfg(target_os = "macos")]
fn platform_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join("Library/Application Support/xi/run"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_dir() -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no runtime directory default for this platform",
    ))
}

/// Returns the daemon socket path within `dir`.
#[must_use]
pub fn socket(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

/// Returns the daemon lock path within `dir`.
#[must_use]
pub fn lock(dir: &Path) -> PathBuf {
    dir.join("daemon.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_honours_override() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::tests::runtime_override_child"])
            .env(ENV, "/tmp/xi-runtime-override")
            .env("XI_RUNTIME_OVERRIDE_TEST", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn runtime_override_child() {
        if std::env::var_os("XI_RUNTIME_OVERRIDE_TEST").is_none() {
            return;
        }
        let path = PathBuf::from("/tmp/xi-runtime-override");
        assert_eq!(dir().unwrap(), path);
        assert_eq!(socket(&path), path.join("daemon.sock"));
        assert_eq!(lock(&path), path.join("daemon.lock"));
    }
}
