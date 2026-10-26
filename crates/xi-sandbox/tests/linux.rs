//! Runs real processes through `xi-sandbox-exec` and checks what they can
//! reach. Needs Landlock ABI 9 and unprivileged user namespaces.
#![cfg(target_os = "linux")]

use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use xi_sandbox::{EXIT_NOT_FOUND, EXIT_SETUP_FAILED, Network, Spec};

const HELPER: &str = env!("CARGO_BIN_EXE_xi-sandbox-exec");
const PROBE_ENV: &str = "XI_SANDBOX_PROBE";

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    tmp: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    /// Lays out a workspace, a session tmp and an unrelated directory under
    /// `parent`.
    fn in_dir(parent: &Path) -> Self {
        let root = tempfile::tempdir_in(parent).unwrap();
        let [workspace, tmp, outside] =
            ["workspace", "tmp", "outside"].map(|name| root.path().join(name));
        for dir in [&workspace, &tmp, &outside] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(outside.join("secret"), "token").unwrap();
        Self {
            _root: root,
            workspace,
            tmp,
            outside,
        }
    }

    fn new() -> Self {
        Self::in_dir(Path::new(env!("CARGO_TARGET_TMPDIR")))
    }

    fn spec(&self) -> Spec {
        Spec::new(&self.workspace, &self.tmp)
    }
}

fn sh(spec: &Spec, script: &str) -> Output {
    spec.command(Path::new(HELPER), "/bin/sh", ["-c", script])
        .unwrap()
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs the `probe` test of this binary inside the sandbox, so the probe can
/// use std networking instead of depending on host tools.
fn run_probe(spec: &Spec, action: &str) -> Output {
    let exe = std::env::current_exe().unwrap();
    let mut spec = spec.clone();
    spec.readable.push(exe.parent().unwrap().to_owned());
    let mut command: Command = spec
        .command(
            Path::new(HELPER),
            &exe,
            ["probe", "--exact", "--nocapture", "--test-threads=1"],
        )
        .unwrap();
    command.env(PROBE_ENV, action).output().unwrap()
}

/// Entry point for [`probe`]; does nothing in a normal test run.
#[test]
fn probe() {
    let Ok(action) = std::env::var(PROBE_ENV) else {
        return;
    };
    let result: io::Result<()> = match action.split_once(':') {
        Some(("unix", path)) => UnixStream::connect(path).map(drop),
        Some(("tcp", address)) => TcpStream::connect(address).map(drop),
        Some(("loopback", _)) => TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| TcpStream::connect(listener.local_addr()?))
            .map(drop),
        _ => panic!("unknown probe {action}"),
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("probe failed: {error}");
            std::process::exit(3);
        }
    }
}

#[test]
fn workspace_is_writable_and_the_rest_of_home_is_not() {
    let fixture = Fixture::new();
    let secret = fixture.outside.join("secret");
    let output = sh(
        &fixture.spec(),
        &format!(
            "echo kept > file && cat file && cat {secret} ; echo leaked > {outside}/new",
            secret = secret.display(),
            outside = fixture.outside.display(),
        ),
    );
    assert_eq!(text(&output.stdout), "kept\n");
    let stderr = text(&output.stderr);
    assert_eq!(stderr.matches("Permission denied").count(), 2, "{stderr}");
    assert!(!fixture.outside.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("file")).unwrap(),
        "kept\n"
    );
}

#[test]
fn tmp_is_private_to_the_session() {
    let fixture = Fixture::new();
    let host_file = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let output = sh(
        &fixture.spec(),
        &format!(
            "test ! -e {} && echo inside > /tmp/note && echo $TMPDIR",
            host_file.path().display()
        ),
    );
    assert_eq!(text(&output.stdout), "/tmp\n", "{}", text(&output.stderr));
    assert_eq!(
        std::fs::read_to_string(fixture.tmp.join("note")).unwrap(),
        "inside\n"
    );
}

#[test]
fn workspace_under_host_tmp_keeps_its_path() {
    let fixture = Fixture::in_dir(Path::new("/tmp"));
    let target = fixture.workspace.join("made-inside");
    let output = sh(
        &fixture.spec(),
        &format!("touch {} && pwd", target.display()),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout).trim_end(),
        fixture.workspace.to_str().unwrap()
    );
    assert!(target.exists());
}

#[test]
fn protected_paths_stay_read_only() {
    let fixture = Fixture::new();
    let git = fixture.workspace.join(".git");
    std::fs::create_dir_all(git.join("objects")).unwrap();
    std::fs::write(git.join("config"), "[core]\n").unwrap();

    let output = sh(
        &fixture.spec(),
        "echo '[core] hooksPath = /x' >> .git/config; touch .git/hooks/pre-commit; \
         touch .xi/local.toml; touch .git/objects/written && echo objects-ok",
    );
    assert_eq!(text(&output.stdout), "objects-ok\n");
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr.matches("Read-only file system").count(),
        3,
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(git.join("config")).unwrap(),
        "[core]\n"
    );
    assert!(git.join("hooks").is_dir());
    assert!(!git.join("hooks/pre-commit").exists());
    assert!(!fixture.workspace.join(".xi/local.toml").exists());
}

#[test]
fn environment_is_reduced_to_the_allowlist() {
    let fixture = Fixture::new();
    // Cargo sets CARGO_MANIFEST_DIR for test processes; it is not allowlisted.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let output = sh(
        &fixture.spec(),
        "echo ${CARGO_MANIFEST_DIR-unset} ${PATH:+path}",
    );
    assert_eq!(text(&output.stdout), "unset path\n");
}

#[test]
fn isolated_network_reaches_only_its_own_loopback() {
    let fixture = Fixture::new();
    let host = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_address = format!("tcp:{}", host.local_addr().unwrap());

    let isolated = fixture.spec();
    let output = run_probe(&isolated, &host_address);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    let output = run_probe(&isolated, "loopback:");
    assert!(output.status.success(), "{}", text(&output.stderr));

    let mut shared = fixture.spec();
    shared.network = Network::Host;
    let output = run_probe(&shared, &host_address);
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn unix_sockets_are_reachable_only_inside_allowed_roots() {
    let fixture = Fixture::new();
    let outside = fixture.outside.join("agent.sock");
    let inside = fixture.workspace.join("service.sock");
    let _outside_listener = UnixListener::bind(&outside).unwrap();
    let _inside_listener = UnixListener::bind(&inside).unwrap();

    let output = run_probe(&fixture.spec(), &format!("unix:{}", outside.display()));
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    assert!(text(&output.stderr).contains("Permission denied"));

    let output = run_probe(&fixture.spec(), &format!("unix:{}", inside.display()));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn cannot_signal_processes_outside_the_sandbox() {
    let fixture = Fixture::new();
    let output = sh(
        &fixture.spec(),
        &format!("kill -0 {} && echo reached", std::process::id()),
    );
    assert_eq!(text(&output.stdout), "");
    assert!(!output.status.success());
}

#[test]
fn setup_failures_and_missing_programs_have_distinct_exit_codes() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec();
    spec.writable.push(fixture.outside.join("missing"));
    let output = sh(&spec, "true");
    assert_eq!(output.status.code(), Some(EXIT_SETUP_FAILED));
    assert!(text(&output.stderr).contains("missing"));

    let output = fixture
        .spec()
        .command(Path::new(HELPER), "/nonexistent/program", [""; 0])
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_NOT_FOUND));
}

#[test]
fn relative_paths_are_rejected_before_spawning() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec();
    spec.readable.push("relative".into());
    let error = spec
        .command(Path::new(HELPER), "/bin/true", [""; 0])
        .unwrap_err();
    assert!(error.to_string().contains("readable"), "{error}");
}
