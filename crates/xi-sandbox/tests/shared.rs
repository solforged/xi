//! Real-process sandbox boundaries shared by Linux and macOS.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;

use common::{Fixture, HELPER, run_probe, sh, text};
use xi_sandbox::{EXIT_CANNOT_EXECUTE, EXIT_NOT_FOUND, EXIT_SETUP_FAILED, Network};

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
    assert!(!output.status.success(), "{stderr}");
    assert_eq!(stderr.lines().count(), 2, "{stderr}");
    assert!(!fixture.outside.join("new").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("file")).unwrap(),
        "kept\n"
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
    #[cfg(target_os = "linux")]
    assert_eq!(
        stderr.matches("Read-only file system").count(),
        3,
        "{stderr}"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        stderr.matches("Operation not permitted").count(),
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
fn isolated_network_cannot_reach_host_but_host_network_can() {
    let fixture = Fixture::new();
    let host = TcpListener::bind("127.0.0.1:0").unwrap();
    let host_address = format!("tcp:{}", host.local_addr().unwrap());

    let isolated = fixture.spec();
    let output = run_probe(&isolated, &host_address);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));

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

    let output = run_probe(&fixture.spec(), &format!("unix:{}", inside.display()));
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn explicit_socket_access_does_not_allow_unlinking_or_expose_siblings() {
    let fixture = Fixture::new();
    let agent = fixture.outside.join("agent.sock");
    let sibling = fixture.outside.join("other.sock");
    let _agent = UnixListener::bind(&agent).unwrap();
    let _sibling = UnixListener::bind(&sibling).unwrap();
    let mut spec = fixture.spec();
    spec.sockets.push(agent.clone());
    let output = run_probe(&spec, &format!("unix:{}", agent.display()));
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = run_probe(&spec, &format!("unix:{}", sibling.display()));
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    let output = sh(&spec, &format!("cat {}/secret", fixture.outside.display()));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let output = sh(&spec, &format!("rm {}", agent.display()));
    assert!(!output.status.success());
    assert!(agent.exists());
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

    let output = fixture
        .spec()
        .command(Path::new(HELPER), "xi-no-such-program-9372", [""; 0])
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_NOT_FOUND));

    let nonexecutable = fixture.workspace.join("not-executable");
    std::fs::write(&nonexecutable, "not an executable").unwrap();
    std::fs::set_permissions(&nonexecutable, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = fixture
        .spec()
        .command(Path::new(HELPER), nonexecutable, [""; 0])
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_CANNOT_EXECUTE));
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

#[test]
fn readable_roots_allow_reads_not_writes_or_socket_connections() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec();
    spec.readable.push(fixture.outside.clone());
    let output = sh(
        &spec,
        &format!(
            "cat {0}/secret; echo changed > {0}/secret",
            fixture.outside.display()
        ),
    );
    assert_eq!(text(&output.stdout), "token");
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(fixture.outside.join("secret")).unwrap(),
        "token"
    );
    let socket = fixture.outside.join("socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    for network in [Network::Isolated, Network::Host] {
        spec.network = network;
        let output = run_probe(&spec, &format!("unix:{}", socket.display()));
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
    }
}

#[test]
fn symlinks_do_not_grant_access_outside_the_workspace() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink(&fixture.outside, fixture.workspace.join("escape")).unwrap();
    let output = sh(&fixture.spec(), "cat escape/secret; echo bad > escape/new");
    assert!(output.stdout.is_empty());
    assert!(!output.status.success());
    assert!(!fixture.outside.join("new").exists());
}

#[test]
fn protected_paths_cannot_be_replaced_or_moved_out_of_the_policy() {
    let fixture = Fixture::new();
    let git = fixture.workspace.join(".git");
    std::fs::create_dir(&git).unwrap();
    std::fs::write(git.join("config"), "kept").unwrap();
    let output = sh(
        &fixture.spec(),
        "rm .git/config; mv .git moved; rmdir .xi; echo overwritten > .git/config",
    );
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(git.join("config")).unwrap(), "kept");
    assert!(fixture.workspace.join(".xi").is_dir());
    assert!(!fixture.workspace.join("moved").exists());
}

#[test]
fn descendant_signals_work_and_the_helper_detaches_the_terminal() {
    let fixture = Fixture::new();
    let output = sh(
        &fixture.spec(),
        "sleep 30 & child=$!; kill \"$child\"; wait \"$child\"; echo signalled",
    );
    assert_eq!(
        text(&output.stdout),
        "signalled\n",
        "{}",
        text(&output.stderr)
    );
    let output = run_probe(&fixture.spec(), "session:");
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn named_programs_on_path_preserve_the_target_exit_status() {
    let fixture = Fixture::new();
    let output = fixture
        .spec()
        .command(Path::new(HELPER), "sh", ["-c", "exit 42"])
        .unwrap()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42), "{}", text(&output.stderr));
}
