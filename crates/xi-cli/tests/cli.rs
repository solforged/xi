//! Executable smoke tests that do not require a provider or sandbox helper.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xi"));
    command
        .current_dir(dir)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state"));
    command
}

#[test]
fn help_and_config_errors_have_documented_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let help = command(dir.path()).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--max-turns"));
    assert!(help.contains("--on-ask"));
    let invalid = dir.path().join("invalid.toml");
    std::fs::write(&invalid, "reasoning = 'impossible'").unwrap();
    let error = command(dir.path())
        .args(["--config", invalid.to_str().unwrap(), "-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(error.status.code(), Some(2));
    assert!(
        String::from_utf8(error.stderr)
            .unwrap()
            .contains("invalid.toml")
    );
    let usage = command(dir.path())
        .args(["-p", "hello", "--max-turns", "0"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
}

#[test]
fn trust_is_canonical_idempotent_and_external_to_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for _ in 0..2 {
        let output = command(dir.path())
            .args(["trust", workspace.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = std::fs::read_to_string(dir.path().join("config/xi/trust.toml")).unwrap();
    assert_eq!(
        text.matches(workspace.canonicalize().unwrap().to_str().unwrap())
            .count(),
        1
    );
    assert!(!workspace.join(".xi").exists());
}

#[test]
fn serve_creates_a_durable_session_over_stdio_without_model_access() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut child = command(dir.path())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "id":1, "method":"session/create", "params":{"cwd":workspace}})
    )
    .unwrap();
    writeln!(stdin, "{{bad-json").unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "id":2, "method":"not/a/method"})
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages[0]["method"], "event/appended");
    assert_eq!(messages[0]["params"]["event"]["type"], "session_started");
    let reply = messages.iter().find(|m| m["id"] == 1).unwrap();
    let result = &reply["result"];
    assert_eq!(result["events"].as_array().unwrap().len(), 1);
    let path = result["path"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
    assert!(messages.iter().any(|m| m["error"]["code"] == -32700));
    assert!(
        messages
            .iter()
            .any(|m| m["id"] == 2 && m["error"]["code"] == -32601)
    );
}
