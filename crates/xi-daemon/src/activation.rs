//! Socket activation, fallback binding and peer authentication.

use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::Path;

use nix::unistd::getuid;
use tokio::net::{UnixListener, UnixStream};
use xi_protocol::runtime;

/// Takes the socket-activated listener, or binds `daemon.sock` with mode 0600.
///
/// The caller must hold the daemon lock. Only a stale socket is removed before
/// binding; an existing non-socket path is left untouched and causes an error.
pub fn listener(dir: &Path) -> io::Result<UnixListener> {
    #[cfg(target_os = "macos")]
    if let Some(listener) = launchd_listener()? {
        return into_tokio(listener);
    }

    #[cfg(target_os = "linux")]
    {
        let mut activation = listenfd::ListenFd::from_env();
        if let Some(listener) = activation.take_unix_listener(0)? {
            return into_tokio(listener);
        }
    }

    let path = runtime::socket(dir);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing to replace non-socket path {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = StdUnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    into_tokio(listener)
}

fn into_tokio(listener: StdUnixListener) -> io::Result<UnixListener> {
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// Rejects a connection whose peer uid differs from the current user's uid.
pub fn check_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon connection peer belongs to another user",
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn launchd_listener() -> io::Result<Option<StdUnixListener>> {
    use std::os::fd::{FromRawFd, OwnedFd};

    #[link(name = "System")]
    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            count: *mut libc::size_t,
        ) -> libc::c_int;
    }

    let mut fds = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: The name is NUL-terminated and both output pointers remain valid.
    let code = unsafe { launch_activate_socket(c"Listeners".as_ptr(), &mut fds, &mut count) };
    if code != 0 {
        // SAFETY: The array is either null or allocated by launch_activate_socket.
        unsafe { libc::free(fds.cast()) };
        return match code {
            libc::ESRCH | libc::ENOENT => Ok(None),
            _ => Err(io::Error::from_raw_os_error(code)),
        };
    }
    if fds.is_null() || count == 0 {
        // SAFETY: free accepts null and arrays allocated by launch_activate_socket.
        unsafe { libc::free(fds.cast()) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "launchd returned no Listeners sockets",
        ));
    }

    let mut listener = None;
    for index in 0..count {
        // SAFETY: On success the array holds count valid descriptors transferred
        // to this process. Each is adopted exactly once, including unused sockets.
        let fd = unsafe { OwnedFd::from_raw_fd(*fds.add(index)) };
        if listener.is_none() {
            listener = Some(StdUnixListener::from(fd));
        }
    }
    // SAFETY: All descriptors are owned or closed; only the allocated array is freed.
    unsafe { libc::free(fds.cast()) };
    Ok(listener)
}
