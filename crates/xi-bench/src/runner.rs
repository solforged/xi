use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader as AsyncBufReader},
    process::Command,
};

use crate::{
    Result,
    config::{Arm, Kind, Task, expand},
    metrics::{self, Metrics, TimedLine},
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Outcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub wall_ms: f64,
    pub error: Option<String>,
}

impl Outcome {
    fn success(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.error.is_none()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Record {
    pub schema_version: u32,
    pub arm: String,
    pub kind: Kind,
    pub task: String,
    pub tags: Vec<String>,
    pub model: String,
    pub trial: u32,
    pub order: usize,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub pass: bool,
    pub protected_untouched: bool,
    pub protected_status: String,
    pub agent: Outcome,
    pub check: Outcome,
    pub metrics: Option<Metrics>,
    pub metrics_error: Option<String>,
    pub diff_stat: String,
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            return Err("task templates must not contain .git".into());
        }
        let dest = target.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), dest)?;
        } else {
            return Err(format!("unsupported template entry {}", entry.path().display()).into());
        }
    }
    Ok(())
}

async fn git(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

async fn initialize(workspace: &Path) -> Result<String> {
    git(workspace, &["init", "--quiet", "--template="]).await?;
    git(workspace, &["add", "--all"]).await?;
    git(
        workspace,
        &[
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=xi-bench",
            "-c",
            "user.email=xi-bench@localhost",
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            "Benchmark baseline",
        ],
    )
    .await?;
    Ok(git(workspace, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_owned())
}

async fn capture(
    reader: impl AsyncRead + Unpin,
    raw: PathBuf,
    timed: Option<PathBuf>,
    start: Instant,
) -> Result<()> {
    let mut reader = AsyncBufReader::new(reader);
    let mut raw = tokio::fs::File::create(raw).await?;
    let mut timed = match timed {
        Some(path) => Some(tokio::fs::File::create(path).await?),
        None => None,
    };
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes).await? == 0 {
            break;
        }
        let at_ms = start.elapsed().as_secs_f64() * 1000.0;
        raw.write_all(&bytes).await?;
        if let Some(file) = &mut timed {
            let line = TimedLine {
                at_ms,
                line: String::from_utf8_lossy(&bytes)
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
            };
            let mut json = serde_json::to_vec(&line)?;
            json.push(b'\n');
            file.write_all(&json).await?;
        }
    }
    raw.flush().await?;
    if let Some(file) = &mut timed {
        file.flush().await?;
    }
    Ok(())
}

async fn execute(
    argv: &[String],
    env: &BTreeMap<String, String>,
    workspace: &Path,
    timeout: Duration,
    log_dir: &Path,
    prefix: &str,
) -> Result<Outcome> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .envs(env)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("CARGO_TARGET_DIR", workspace.join("target"));
    command.process_group(0);
    let start = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(Outcome {
                exit_code: None,
                timed_out: false,
                wall_ms: start.elapsed().as_secs_f64() * 1000.0,
                error: Some(error.to_string()),
            });
        }
    };
    let pid = Pid::from_raw(i32::try_from(
        child.id().ok_or("spawned child has no pid")?,
    )?);
    let mut stdout = tokio::spawn(capture(
        child.stdout.take().ok_or("missing child stdout")?,
        log_dir.join(format!("{prefix}.stdout.log")),
        Some(log_dir.join(format!("{prefix}.stdout.jsonl"))),
        start,
    ));
    let mut stderr = tokio::spawn(capture(
        child.stderr.take().ok_or("missing child stderr")?,
        log_dir.join(format!("{prefix}.stderr.log")),
        None,
        start,
    ));
    let waited = tokio::time::timeout(timeout, child.wait()).await;
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    // Stop descendants too, including background jobs that kept the pipes open.
    let _ = killpg(pid, Signal::SIGKILL);
    let (exit_code, timed_out, mut error) = match waited {
        Ok(Ok(status)) => (status.code(), false, None),
        Ok(Err(error)) => (None, false, Some(error.to_string())),
        Err(_) => {
            child.kill().await.ok();
            child.wait().await.ok();
            (None, true, None)
        }
    };
    let drained = tokio::time::timeout(Duration::from_secs(2), async {
        (&mut stdout).await??;
        (&mut stderr).await??;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await;
    match drained {
        Ok(Ok(())) => {}
        Ok(Err(cause)) => error = Some(format!("log capture: {cause}")),
        Err(_) => {
            stdout.abort();
            stderr.abort();
            error = Some("log pipes remained open after process-group termination".into());
        }
    }
    Ok(Outcome {
        exit_code,
        timed_out,
        wall_ms,
        error,
    })
}

async fn protected_status(task: &Task, workspace: &Path, baseline: &str) -> Result<String> {
    if task.spec.protected_paths.is_empty() {
        return Ok(String::new());
    }
    let paths: Vec<String> = task
        .spec
        .protected_paths
        .iter()
        .map(|p| format!(":(literal){}", p.display()))
        .collect();
    let mut args = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    args.extend(paths.iter().map(String::as_str));
    let mut status = git(workspace, &args).await?;
    // Compare the original commit too, so an agent cannot hide edits by committing them.
    let mut args = vec!["diff", "--name-status", baseline, "--"];
    args.extend(paths.iter().map(String::as_str));
    status.push_str(&git(workspace, &args).await?);
    Ok(status)
}

pub(crate) async fn verify(tasks: &[Task]) -> Result<()> {
    let mut failures = Vec::new();
    for task in tasks {
        match verify_task(task).await {
            Ok((before, after)) => println!(
                "PASS {}: fresh exit {} ({:.2}s); reference exit 0 ({:.2}s); protected paths unchanged",
                task.name,
                before.exit_code.unwrap_or(-1),
                before.wall_ms / 1000.0,
                after.wall_ms / 1000.0
            ),
            Err(error) => {
                eprintln!("FAIL {}: {error}", task.name);
                failures.push(task.name.as_str());
            }
        }
    }
    if failures.is_empty() {
        println!("Verified {} tasks.", tasks.len());
        Ok(())
    } else {
        Err(format!("verification failed: {}", failures.join(", ")).into())
    }
}

async fn verify_task(task: &Task) -> Result<(Outcome, Outcome)> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("repo");
    copy_tree(&task.root.join("repo"), &workspace)?;
    let baseline = initialize(&workspace).await?;
    let before = execute(
        &task.spec.check,
        &BTreeMap::new(),
        &workspace,
        CHECK_TIMEOUT,
        temp.path(),
        "before",
    )
    .await?;
    if before.success() || before.timed_out || before.error.is_some() || before.exit_code.is_none()
    {
        return Err(format!("fresh check must fail normally: {before:?}").into());
    }
    let patch = task.root.join("reference.patch");
    git(
        &workspace,
        &["apply", patch.to_str().ok_or("non-UTF-8 patch path")?],
    )
    .await?;
    let after = execute(
        &task.spec.check,
        &BTreeMap::new(),
        &workspace,
        CHECK_TIMEOUT,
        temp.path(),
        "after",
    )
    .await?;
    if !after.success() {
        return Err(format!(
            "reference check failed: {after:?}\n{}\n{}",
            fs::read_to_string(temp.path().join("after.stdout.log")).unwrap_or_default(),
            fs::read_to_string(temp.path().join("after.stderr.log")).unwrap_or_default()
        )
        .into());
    }
    let status = protected_status(task, &workspace, &baseline).await?;
    if !status.is_empty() {
        return Err(format!("reference changed protected paths: {status}").into());
    }
    Ok((before, after))
}

fn parse_metrics(kind: Kind, sessions: &Path, logs: &Path, wall_ms: f64) -> Result<Metrics> {
    let metrics = match kind {
        Kind::Omp | Kind::Pi => metrics::parse_omp(
            BufReader::new(File::open(logs.join("agent.stdout.jsonl"))?),
            wall_ms,
        )?,
        Kind::Xi => metrics::parse_xi(sessions, wall_ms)?,
    };
    if metrics.model_calls == 0 && metrics.turn_failures == 0 {
        return Err("no model responses or turn failures in event stream".into());
    }
    Ok(metrics)
}

pub(crate) async fn run(
    tasks: &[Task],
    arms: &[Arm],
    model: &str,
    trials: u32,
    out: &Path,
) -> Result<()> {
    fs::create_dir_all(out)?;
    let out = out.canonicalize()?;
    for trial in 1..=trials {
        for task in tasks {
            for (order, index) in arm_order(arms.len(), trial).into_iter().enumerate() {
                let arm = &arms[index];
                let logs = out.join(format!("{}--{}--{trial:03}", task.name, arm.name));
                fs::create_dir(&logs)?;
                let record = run_one(task, arm, model, trial, order, &logs).await?;
                let mut file = File::create(logs.join("record.json"))?;
                serde_json::to_writer_pretty(&mut file, &record)?;
                writeln!(file)?;
                println!(
                    "{} {} trial {}: {} ({:.2}s)",
                    arm.name,
                    task.name,
                    trial,
                    if record.pass { "PASS" } else { "FAIL" },
                    record.agent.wall_ms / 1000.0
                );
            }
        }
    }
    Ok(())
}

fn arm_order(count: usize, trial: u32) -> Vec<usize> {
    if trial % 2 == 1 {
        (0..count).collect()
    } else {
        (0..count).rev().collect()
    }
}

async fn run_one(
    task: &Task,
    arm: &Arm,
    model: &str,
    trial: u32,
    order: usize,
    logs: &Path,
) -> Result<Record> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("repo");
    copy_tree(&task.root.join("repo"), &workspace)?;
    let baseline = initialize(&workspace).await?;
    let sessions = logs.join("sessions");
    fs::create_dir(&sessions)?;
    if matches!(arm.kind, Kind::Omp) {
        fs::write(sessions.join("omp.yml"), "memory:\n  backend: off\n")?;
    }
    let argv: Vec<_> = arm
        .argv
        .iter()
        .map(|value| {
            expand(
                value,
                &task.spec.prompt,
                &workspace,
                &sessions,
                model,
                &arm.directory,
            )
        })
        .collect();
    let env = arm
        .env
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                expand(
                    value,
                    &task.spec.prompt,
                    &workspace,
                    &sessions,
                    model,
                    &arm.directory,
                ),
            )
        })
        .collect();
    fs::write(logs.join("prompt.txt"), &task.spec.prompt)?;
    let agent = execute(
        &argv,
        &env,
        &workspace,
        Duration::from_secs(task.spec.timeout_seconds),
        logs,
        "agent",
    )
    .await?;
    let check = execute(
        &task.spec.check,
        &BTreeMap::new(),
        &workspace,
        CHECK_TIMEOUT,
        logs,
        "check",
    )
    .await?;
    let protected_status = protected_status(task, &workspace, &baseline).await?;
    let protected_untouched = protected_status.is_empty();
    let diff_stat = git(&workspace, &["diff", "--stat", &baseline]).await?;
    fs::write(
        logs.join("changes.patch"),
        git(&workspace, &["diff", "--binary", &baseline]).await?,
    )?;
    fs::write(
        logs.join("git-status.txt"),
        git(
            &workspace,
            &["status", "--porcelain", "--untracked-files=all"],
        )
        .await?,
    )?;
    let (metrics, metrics_error) = match parse_metrics(arm.kind, &sessions, logs, agent.wall_ms) {
        Ok(metrics) => (Some(metrics), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let pass = check.success()
        && protected_untouched
        && agent.success()
        && metrics.as_ref().is_some_and(|m| m.turn_failures == 0);
    Ok(Record {
        schema_version: 1,
        arm: arm.name.clone(),
        kind: arm.kind,
        task: task.name.clone(),
        tags: task.spec.tags.clone(),
        model: model.to_owned(),
        trial,
        order,
        argv,
        env,
        pass,
        protected_untouched,
        protected_status,
        agent,
        check,
        metrics,
        metrics_error,
        diff_stat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trials_reverse_arm_order() {
        assert_eq!(arm_order(3, 1), vec![0, 1, 2]);
        assert_eq!(arm_order(3, 2), vec![2, 1, 0]);
        assert_eq!(arm_order(3, 3), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn timeout_kills_descendants_and_keeps_partial_logs() {
        let temp = tempfile::tempdir().unwrap();
        let argv = ["sh", "-c", "printf 'started\\n'; sleep 30 & wait"].map(str::to_owned);
        let outcome = execute(
            &argv,
            &BTreeMap::new(),
            temp.path(),
            Duration::from_millis(100),
            temp.path(),
            "child",
        )
        .await
        .unwrap();
        assert!(outcome.timed_out);
        assert!(outcome.wall_ms < 2000.0);
        assert!(outcome.error.is_none());
        assert_eq!(
            fs::read_to_string(temp.path().join("child.stdout.log")).unwrap(),
            "started\n"
        );
        let timed: TimedLine = serde_json::from_str(
            &fs::read_to_string(temp.path().join("child.stdout.jsonl")).unwrap(),
        )
        .unwrap();
        assert!(timed.at_ms <= outcome.wall_ms);
    }

    #[tokio::test]
    async fn committed_protected_edits_are_still_detected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(root.join("tests/check.py"), "original\n").unwrap();
        let baseline = initialize(root).await.unwrap();
        let task = Task {
            name: "protected".into(),
            root: root.to_path_buf(),
            spec: crate::config::Spec {
                prompt: "test".into(),
                check: vec!["true".into()],
                timeout_seconds: 1,
                protected_paths: vec!["tests".into()],
                tags: vec![],
            },
        };
        assert!(
            protected_status(&task, root, &baseline)
                .await
                .unwrap()
                .is_empty()
        );
        fs::write(root.join("tests/check.py"), "changed\n").unwrap();
        initialize(root).await.unwrap();
        assert!(
            git(root, &["status", "--porcelain"])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            protected_status(&task, root, &baseline)
                .await
                .unwrap()
                .contains("tests/check.py")
        );
    }

    #[tokio::test]
    async fn missing_executable_is_a_run_failure_not_a_panic() {
        let temp = tempfile::tempdir().unwrap();
        let argv = vec![
            temp.path()
                .join("missing-program")
                .to_string_lossy()
                .into_owned(),
        ];
        let outcome = execute(
            &argv,
            &BTreeMap::new(),
            temp.path(),
            Duration::from_secs(1),
            temp.path(),
            "missing",
        )
        .await
        .unwrap();
        assert!(outcome.error.is_some());
        assert!(!outcome.success());
        assert!(!outcome.timed_out);
    }
}
