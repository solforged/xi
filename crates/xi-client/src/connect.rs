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
/// another reason, the daemon cannot be spawned, or connection retries time out.
/// On timeout, returns the last connection error.
pub async fn connect_or_start(exe: &Path) -> io::Result<UnixStream> {
    let socket = runtime::socket(&runtime::dir()?);
    let mut last_error = match UnixStream::connect(&socket).await {
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
    Command::new(exe)
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
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        match timeout_at(deadline, UnixStream::connect(&socket)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(error)) => last_error = error,
            Err(_) => return Err(last_error),
        }
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}
