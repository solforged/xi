//! Private runtime-directory creation and daemon locking.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::unistd::getuid;
use xi_protocol::runtime;

/// Holds the exclusive daemon lock until dropped.
#[derive(Debug)]
pub struct LockGuard {
    _file: Flock<File>,
}

/// Creates the runtime directory with mode 0700, owned by the current user.
///
/// An existing directory with another owner or group/world access is rejected.
pub fn prepare_runtime_dir() -> io::Result<PathBuf> {
    prepare_runtime_dir_at(&runtime::dir()?)
}

fn prepare_runtime_dir_at(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_directory(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            match DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            check_directory(path, &fs::symlink_metadata(path)?)?;
        }
        Err(error) => return Err(error),
    }
    Ok(path.to_path_buf())
}

fn check_directory(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("runtime directory {} is not a directory", path.display()),
        ));
    }
    if metadata.uid() != getuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory {} must be owned by the current user with no group/world access",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Acquires an exclusive, non-blocking lock on `daemon.lock`.
///
/// Returns `Ok(None)` when another daemon already holds the lock.
pub fn try_lock(dir: &Path) -> io::Result<Option<LockGuard>> {
    try_lock_at(&runtime::lock(dir))
}

fn try_lock_at(path: &Path) -> io::Result<Option<LockGuard>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(file) => Ok(Some(LockGuard { _file: file })),
        Err((_, Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_private_runtime_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("parent/run");

        assert_eq!(prepare_runtime_dir_at(&path).unwrap(), path);
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), getuid().as_raw());
        assert_eq!(prepare_runtime_dir_at(&path).unwrap(), path);
    }

    #[test]
    fn rejects_existing_public_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        let error = prepare_runtime_dir_at(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(path.to_str().unwrap()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn independent_open_cannot_lock_twice() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.lock");
        let first = try_lock_at(&path).unwrap().unwrap();

        assert!(try_lock_at(&path).unwrap().is_none());
        drop(first);
        assert!(try_lock_at(&path).unwrap().is_some());
    }
}
