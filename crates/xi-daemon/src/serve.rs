//! Per-user daemon startup and authenticated socket serving.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use xi_kernel::config::{Paths, daemon_settings};

use crate::activation::{check_peer, listener};
use crate::runtime_dir::{prepare_runtime_dir, try_lock};
use crate::{Daemon, DaemonError, DaemonOptions, ProcessSpawner};

/// Runs the locked, socket-activated per-user daemon until shutdown.
/// Returns immediately when another daemon already holds the runtime lock.
pub async fn run_user_daemon(paths: Paths) -> Result<(), DaemonError> {
    let settings = daemon_settings(&paths)?;
    let dir = prepare_runtime_dir()?;
    let Some(_lock) = try_lock(&dir)? else {
        return Ok(());
    };
    let listener = listener(&dir)?;
    std::fs::create_dir_all(&paths.state)?;
    let exe = std::env::current_exe()?;
    let initial_exe = executable_stamp(&exe)?;
    let daemon = Daemon::new(DaemonOptions {
        settings,
        spawner: Arc::new(ProcessSpawner {
            exe: exe.clone(),
            log: Some(paths.state.join("daemon.log")),
        }),
        paths,
        private: false,
    });
    let mut executable_timer = tokio::time::interval(Duration::from_secs(5));
    executable_timer.tick().await;
    loop {
        tokio::select! {
            _ = daemon.finished() => return Ok(()),
            _ = executable_timer.tick() => {
                if executable_stamp(&exe).ok().as_ref() != Some(&initial_exe) {
                    daemon.executable_changed();
                }
            }
            connection = listener.accept() => {
                let (stream, _) = connection?;
                if let Err(error) = check_peer(&stream) {
                    eprintln!("xi daemon rejected peer: {error}");
                    continue;
                }
                let (reader, writer) = stream.into_split();
                let daemon = daemon.clone();
                tokio::spawn(async move { daemon.serve(reader, writer).await; });
            }
        }
    }
}

#[derive(PartialEq)]
struct ExecutableStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: SystemTime,
}

fn executable_stamp(path: &Path) -> std::io::Result<ExecutableStamp> {
    let metadata = std::fs::metadata(path)?;
    Ok(ExecutableStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified: metadata.modified()?,
    })
}
