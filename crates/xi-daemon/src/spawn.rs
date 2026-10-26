//! Process and in-memory session worker supervision.

use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use futures::future::BoxFuture;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncWrite};
use xi_kernel::config::Paths;

/// Creates a worker's transport and lifetime controls.
pub trait Spawner: Send + Sync {
    /// Starts one worker, without starting its session yet.
    fn spawn(&self) -> BoxFuture<'_, io::Result<WorkerProcess>>;
}

/// A worker's daemon-side pipe and lifetime controls.
pub struct WorkerProcess {
    /// Reads worker responses and notifications.
    pub reader: Box<dyn AsyncRead + Unpin + Send>,
    /// Writes daemon requests and responses.
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    /// Resolves with the exit status, or none when unknown or killed by signal.
    pub exited: BoxFuture<'static, Option<i32>>,
    /// Kills the worker's process group, or aborts its in-process task.
    pub kill: Box<dyn FnOnce() + Send>,
}

/// Runs `<exe> worker`, appending stderr to `log` when given.
pub struct ProcessSpawner {
    /// Executable containing the hidden `worker` subcommand.
    pub exe: PathBuf,
    /// Worker stderr destination; absent inherits stderr.
    pub log: Option<PathBuf>,
}

impl Spawner for ProcessSpawner {
    fn spawn(&self) -> BoxFuture<'_, io::Result<WorkerProcess>> {
        Box::pin(async move {
            let stderr = match &self.log {
                Some(path) => Stdio::from(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)?,
                ),
                None => Stdio::inherit(),
            };
            let mut child = tokio::process::Command::new(&self.exe)
                .arg("worker")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(stderr)
                .process_group(0)
                .kill_on_drop(true)
                .spawn()?;
            let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
            let reader = child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("worker stdout missing"))?;
            let writer = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("worker stdin missing"))?;
            Ok(WorkerProcess {
                reader: Box::new(reader),
                writer: Box::new(writer),
                exited: Box::pin(async move {
                    child.wait().await.ok().and_then(|status| status.code())
                }),
                kill: Box::new(move || {
                    if let Some(pid) = pid {
                        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                    }
                }),
            })
        })
    }
}

/// Runs workers on Tokio tasks over in-memory duplex pipes.
pub struct InProcessSpawner {
    /// Configuration and persistence directories for each worker.
    pub paths: Paths,
    /// Test or demo backend factory, called once per worker.
    pub backend: Option<Arc<dyn Fn() -> xi_kernel::Backend + Send + Sync>>,
}

impl Spawner for InProcessSpawner {
    fn spawn(&self) -> BoxFuture<'_, io::Result<WorkerProcess>> {
        Box::pin(async move {
            let (daemon, worker) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(daemon);
            let (worker_reader, worker_writer) = tokio::io::split(worker);
            let setup = xi_kernel::worker::WorkerSetup {
                paths: self.paths.clone(),
                backend: self.backend.as_ref().map(|backend| backend()),
            };
            let task = tokio::spawn(xi_kernel::worker::run(worker_reader, worker_writer, setup));
            let abort = task.abort_handle();
            Ok(WorkerProcess {
                reader: Box::new(reader),
                writer: Box::new(writer),
                exited: Box::pin(async move {
                    match task.await {
                        Ok(Ok(())) => Some(0),
                        Ok(Err(error)) => {
                            eprintln!("xi worker: {error}");
                            Some(1)
                        }
                        Err(_) => None,
                    }
                }),
                kill: Box::new(move || abort.abort()),
            })
        })
    }
}
