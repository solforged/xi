//! macOS Seatbelt behavior, including the lack of private loopback or mounts.
#![cfg(target_os = "macos")]

mod common;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

use common::{Fixture, HELPER, run_probe, sh, text};
use xi_sandbox::{EXIT_CANNOT_EXECUTE, EXIT_NOT_FOUND, EXIT_SETUP_FAILED, Network};

#[test]
fn tmpdir_is_writable_without_granting_the_host_temporary_directories() {
    let fixture = Fixture::new();
    let host = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let var_folders = tempfile::NamedTempFile::new().unwrap();
    let mut spec = fixture.spec();
    spec.writable.retain(|path| path != &fixture.tmp);
    let output = sh(&spec, "echo inside > \"$TMPDIR/note\" && echo \"$TMPDIR\"");
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout).trim_end(),
        fixture.tmp.to_str().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tmp.join("note")).unwrap(),
        "inside\n"
    );
    let canonical = host.path().canonicalize().unwrap();
    for path in [host.path(), canonical.as_path(), var_folders.path()] {
        let output = sh(
            &spec,
            &format!(
                "cat '{}' ; echo leaked > '{}'",
                path.display(),
                path.display()
            ),
        );
        assert!(output.stdout.is_empty());
        assert!(!output.status.success(), "{}", text(&output.stderr));
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }
}

#[test]
fn isolated_network_cannot_even_bind_its_own_loopback() {
    let fixture = Fixture::new();
    for action in ["loopback:", "ipv6:", "udp:127.0.0.1:9"] {
        let output = run_probe(&fixture.spec(), action);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
        assert!(text(&output.stderr).contains("Operation not permitted"));
        let mut host = fixture.spec();
        host.network = Network::Host;
        let output = run_probe(&host, action);
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
}

#[test]
fn workspace_sockets_can_be_created_and_protected_sockets_cannot() {
    let fixture = Fixture::in_dir(Path::new("/tmp"));
    let output = run_probe(
        &fixture.spec(),
        &format!("unix-bind:{}/service", fixture.workspace.display()),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let output = run_probe(
        &fixture.spec(),
        &format!("unix-bind:{}/.xi/service", fixture.workspace.display()),
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
}

#[test]
fn quoted_root_names_and_symlinked_roots_do_not_inject_policy() {
    let fixture = Fixture::new();
    let quoted = fixture
        .workspace
        .join("quote\" (allow default) \\ brackets[ab]");
    std::fs::create_dir(&quoted).unwrap();
    let alias = fixture.workspace.join("alias");
    symlink(&quoted, &alias).unwrap();
    let spec = xi_sandbox::Spec::new(alias, &fixture.tmp);
    let output = sh(
        &spec,
        &format!(
            "echo yes > made; cat '{}'",
            fixture.outside.join("secret").display()
        ),
    );
    assert!(output.stdout.is_empty());
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(quoted.join("made")).unwrap(),
        "yes\n"
    );
}

#[test]
fn missing_protected_paths_and_symlink_targets_remain_read_only() {
    let fixture = Fixture::new();
    let target = fixture.workspace.join("original");
    std::fs::write(&target, "kept").unwrap();
    let protected = fixture.workspace.join("protected");
    symlink(&target, &protected).unwrap();
    let mut spec = fixture.spec();
    spec.protected.push(protected);
    spec.protected.push(fixture.workspace.join("missing"));
    let output = sh(
        &spec,
        "echo bad > original; rm protected; rmdir missing; touch missing/new",
    );
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "kept");
    assert!(fixture.workspace.join("protected").is_symlink());
    assert!(fixture.workspace.join("missing").is_dir());
    assert!(!fixture.workspace.join("missing/new").exists());
}

#[test]
fn denied_executable_and_missing_interpreter_keep_exec_exit_codes() {
    let fixture = Fixture::new();
    let script = fixture.outside.join("script");
    std::fs::write(&script, "#!/bin/sh\necho escaped\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = fixture
        .spec()
        .command(Path::new(HELPER), &script, [""; 0])
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(EXIT_CANNOT_EXECUTE),
        "{}",
        text(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let script = fixture.workspace.join("script");
    std::fs::write(&script, "#!/xi-no-such-interpreter\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = fixture
        .spec()
        .command(Path::new(HELPER), &script, [""; 0])
        .unwrap()
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(EXIT_NOT_FOUND),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn internal_exec_stage_refuses_accidental_direct_invocation() {
    let output = Command::new(HELPER)
        .args(["--internal-exec", "/usr/bin/true"])
        .env_remove("XI_SANDBOX_EXEC_STAGE")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(EXIT_SETUP_FAILED));
    let output = sh(
        &Fixture::new().spec(),
        "echo ${XI_SANDBOX_EXEC_STAGE-unset}",
    );
    assert_eq!(text(&output.stdout), "unset\n");
}

#[test]
fn home_is_not_exposed_by_the_system_data_volume() {
    let fixture = Fixture::new();
    let home = std::env::var_os("HOME").unwrap();
    let volume_home =
        Path::new("/System/Volumes/Data").join(Path::new(&home).strip_prefix("/").unwrap());
    for path in [Path::new(&home), volume_home.as_path()] {
        let output = sh(&fixture.spec(), &format!("ls '{}'", path.display()));
        assert!(output.stdout.is_empty());
        assert!(!output.status.success(), "{}", text(&output.stderr));
    }
}

#[test]
fn keychain_queries_fail_even_with_host_networking() {
    let fixture = Fixture::new();
    for network in [Network::Isolated, Network::Host] {
        let mut spec = fixture.spec();
        spec.network = network;
        let output = sh(
            &spec,
            "/usr/bin/security find-generic-password -s xi-sandbox-nonexistent-test-service",
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            text(&output.stderr).contains("keychain"),
            "{}",
            text(&output.stderr)
        );
    }
}

#[test]
fn native_dns_socket_is_a_host_only_exception() {
    let fixture = Fixture::new();
    for path in ["/private/var/run/mDNSResponder", "/var/run/mDNSResponder"] {
        let output = run_probe(&fixture.spec(), &format!("unix:{path}"));
        assert_eq!(output.status.code(), Some(3), "{}", text(&output.stderr));
        let mut host = fixture.spec();
        host.network = Network::Host;
        let output = run_probe(&host, &format!("unix:{path}"));
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
}
