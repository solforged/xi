//! Per-user daemon connections with detached autostart.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::{Instant, sleep_until, timeout_at};
use xi_protocol::runtime;

/// Connects to the per-user daemon socket without starting anything.
///
/// # Errors
/// Fails when the runtime directory cannot be resolved or the socket connection
/// fails.
pub async fn connect() -> io::Result<UnixStream> {
    UnixStream::connect(runtime::socket(&runtime::dir()?)).await
}

/// Connects to the per-user daemon socket, starting `<exe> daemon` when absent.
///
/// On a missing socket or a refused connection, starts a detached process group
/// with null standard streams, then retries for up to five seconds. Retries
/// start after 25 ms and double to a maximum delay of 500 ms.
///
/// # Errors
/// Fails when runtime paths cannot be resolved, the initial connection fails for
/// another reason, the daemon cannot be spawned, the daemon exits unsuccessfully
/// before accepting a connection, or connection retries time out. On timeout,
/// returns the last connection error.
pub async fn connect_or_start(exe: &Path) -> io::Result<UnixStream> {
    connect_or_start_at(&runtime::socket(&runtime::dir()?), exe).await
}

async fn connect_or_start_at(socket: &Path, exe: &Path) -> io::Result<UnixStream> {
    let mut last_error = match UnixStream::connect(socket).await {
        Ok(stream) => return Ok(stream),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            error
        }
        Err(error) => return Err(error),
    };
    let mut daemon = Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut delay = Duration::from_millis(25);
    loop {
        sleep_until((Instant::now() + delay).min(deadline)).await;
        // A daemon that loses the lock race exits successfully and the winner
        // listens; any other exit means nothing will ever listen.
        if let Some(status) = daemon.try_wait()?
            && !status.success()
        {
            return Err(io::Error::other(format!(
                "the xi daemon exited ({status}) before listening; run `xi daemon` to see why"
            )));
        }
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        match timeout_at(deadline, UnixStream::connect(socket)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = error,
            Err(_) => return Err(last_error),
        }
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn failed_daemon_start_is_reported_before_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("xi");
        std::fs::write(&exe, "#!/bin/sh\nexit 2\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let error = connect_or_start_at(&dir.path().join("daemon.sock"), &exe)
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(error.to_string().contains("exited"), "{error}");
    }
}
