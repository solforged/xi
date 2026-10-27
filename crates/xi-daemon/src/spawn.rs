//! Process and in-memory session worker supervision.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncRead, AsyncWrite};
use xi_kernel::config::Paths;
use xi_protocol::BoxFuture;

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
///
/// The executable is opened at construction. Later workers run that image
/// even if the path is replaced, so a new binary is not paired with this
/// daemon.
pub struct ProcessSpawner {
    exe: PathBuf,
    image: Mutex<Image>,
    /// Worker stderr destination; absent inherits stderr.
    log: Option<PathBuf>,
}

/// The executable image captured when the spawner was opened.
struct Image {
    file: File,
    device: u64,
    inode: u64,
    /// Private copy of `file`, created once `exe` no longer names that inode.
    copy: Option<PathBuf>,
}

impl ProcessSpawner {
    /// Opens `exe` for later worker spawns.
    ///
    /// # Errors
    /// Fails when `exe` cannot be opened.
    pub fn open(exe: impl Into<PathBuf>, log: Option<PathBuf>) -> io::Result<Self> {
        let exe = exe.into();
        let file = File::open(&exe)?;
        let meta = file.metadata()?;
        Ok(Self {
            exe,
            image: Mutex::new(Image {
                file,
                device: meta.dev(),
                inode: meta.ino(),
                copy: None,
            }),
            log,
        })
    }

    /// Path to exec. The original path when it still names the opened inode;
    /// otherwise a private copy of that inode.
    fn executable(&self) -> io::Result<PathBuf> {
        let mut image = self.image.lock().expect("worker image lock");
        if same_file(&self.exe, image.device, image.inode) {
            return Ok(self.exe.clone());
        }
        if let Some(copy) = &image.copy
            && copy.is_file()
        {
            return Ok(copy.clone());
        }
        let device = image.device;
        let inode = image.inode;
        let copy = materialize(&mut image.file, device, inode)?;
        image.copy = Some(copy.clone());
        Ok(copy)
    }
}

impl Drop for ProcessSpawner {
    fn drop(&mut self) {
        let Ok(image) = self.image.get_mut() else {
            return;
        };
        if let Some(copy) = image.copy.take() {
            let _ = fs::remove_file(copy);
        }
    }
}

fn same_file(path: &Path, device: u64, inode: u64) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.dev() == device && meta.ino() == inode)
}

/// Copies the opened inode aside. macOS has no `fexecve`, and executing
/// `/dev/fd` is denied, so a replaced path has to be spawned from a file.
fn materialize(file: &mut File, device: u64, inode: u64) -> io::Result<PathBuf> {
    let dir = std::env::temp_dir();
    let dest = dir.join(format!(
        "xi-worker-{}-{}-{}",
        std::process::id(),
        device,
        inode
    ));
    let tmp = dir.join(format!(
        "xi-worker-{}-{}-{}.tmp",
        std::process::id(),
        device,
        inode
    ));
    let result = (|| {
        file.seek(SeekFrom::Start(0))?;
        let mut out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o700)
            .open(&tmp)?;
        io::copy(file, &mut out)?;
        out.sync_all()?;
        drop(out);
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&tmp, perms)?;
        fs::rename(&tmp, &dest)?;
        Ok(dest)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

impl Spawner for ProcessSpawner {
    fn spawn(&self) -> BoxFuture<'_, io::Result<WorkerProcess>> {
        Box::pin(async move {
            let program = self.executable()?;
            let stderr = match &self.log {
                Some(path) => Stdio::from(OpenOptions::new().create(true).append(true).open(path)?),
                None => Stdio::inherit(),
            };
            let mut child = tokio::process::Command::new(&program)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tokio::io::AsyncReadExt;

    use super::*;

    fn write_script(path: &Path, word: &str) {
        fs::write(path, format!("#!/bin/sh\nprintf '{word}\\n'\n")).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn unchanged_executable_is_used_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_script(&path, "old");
        let spawner = ProcessSpawner::open(&path, None).unwrap();
        assert_eq!(spawner.executable().unwrap(), path);
    }

    #[tokio::test]
    async fn replaced_executable_still_runs_the_opened_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_script(&path, "old");
        let spawner = ProcessSpawner::open(&path, None).unwrap();
        let staged = dir.path().join("staged");
        write_script(&staged, "new");
        fs::rename(&staged, &path).unwrap();

        let mut worker = spawner.spawn().await.unwrap();
        let mut out = String::new();
        worker.reader.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "old\n");
        assert_eq!(worker.exited.await, Some(0));
        assert!(fs::read_to_string(&path).unwrap().contains("new"));
    }
}
