//! Best-effort workspace Git state, refreshed without blocking the composer.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::task::JoinHandle;

const INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(crate) struct Status {
    cwd: PathBuf,
    label: Option<String>,
    checked: Option<Instant>,
    pending: Option<JoinHandle<Option<String>>>,
}

impl Status {
    /// Never show the previous workspace's branch while switching sessions.
    pub(crate) fn label(&self, cwd: &Path) -> Option<&str> {
        (self.cwd == cwd).then_some(self.label.as_deref()).flatten()
    }

    /// Collect only an already-finished probe; start the next one when due.
    pub(crate) async fn refresh(&mut self, cwd: &Path) {
        if self.cwd != cwd {
            self.cancel();
            self.cwd = cwd.to_owned();
            self.label = None;
            self.checked = None;
        }
        if self.pending.as_ref().is_some_and(|task| task.is_finished()) {
            self.label = self.pending.take().unwrap().await.ok().flatten();
        }
        if self.pending.is_none() && self.checked.is_none_or(|at| at.elapsed() >= INTERVAL) {
            self.checked = Some(Instant::now());
            self.pending = Some(tokio::spawn(probe(self.cwd.clone())));
        }
    }

    fn cancel(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn probe(cwd: PathBuf) -> Option<String> {
    // Status must not refresh the index or run a repository's fsmonitor hook.
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("git")
            .args(["--no-optional-locks", "-c", "core.fsmonitor=false", "-C"])
            .arg(cwd)
            .args([
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=normal",
                "--ignore-submodules=dirty",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output
        .status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

fn parse(output: &str) -> Option<String> {
    let mut head = None;
    let mut oid = None;
    let mut dirty = false;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value);
        } else if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value);
        } else if matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u' | b'?')) {
            dirty = true;
        }
    }
    let mut label = match head? {
        "(detached)" => format!("@{}", oid?.get(..7)?),
        head => head.to_owned(),
    };
    if dirty {
        label.push('*');
    }
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_clean_dirty_unborn_and_detached_heads() {
        let branch = "# branch.oid abcdef0123456789\n# branch.head main\n";
        assert_eq!(parse(branch).as_deref(), Some("main"));
        for record in ["1 changed", "2 renamed", "u conflict", "? untracked"] {
            assert_eq!(
                parse(&format!("{branch}{record}\n")).as_deref(),
                Some("main*")
            );
        }
        assert_eq!(
            parse("# branch.oid (initial)\n# branch.head new\n").as_deref(),
            Some("new")
        );
        assert_eq!(
            parse("# branch.oid abcdef0123456789\n# branch.head (detached)\n").as_deref(),
            Some("@abcdef0")
        );
        assert_eq!(parse(""), None);
        assert_eq!(parse("# branch.head (detached)\n"), None);
    }

    #[tokio::test]
    async fn probes_repositories_and_hides_unavailable_workspaces() {
        let repo = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .arg(repo.path())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(probe(repo.path().into()).await.as_deref(), Some("main"));
        std::fs::write(repo.path().join("new.txt"), "new\n").unwrap();
        assert_eq!(probe(repo.path().into()).await.as_deref(), Some("main*"));
        assert_eq!(probe(repo.path().join("missing-workspace")).await, None);
    }

    #[tokio::test]
    async fn switching_workspaces_clears_the_label_and_cancels_the_old_probe() {
        let task = tokio::spawn(std::future::pending());
        let abort = task.abort_handle();
        let mut status = Status {
            cwd: "/old".into(),
            label: Some("old-branch".into()),
            checked: Some(Instant::now()),
            pending: Some(task),
        };
        assert_eq!(status.label(Path::new("/old")), Some("old-branch"));
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(status.label(outside.path()), None);
        status.refresh(outside.path()).await;
        assert_eq!(status.label(outside.path()), None);
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
    }
}
