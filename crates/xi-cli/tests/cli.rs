//! Executable smoke tests that do not require a provider or sandbox helper.

use std::io::{BufRead, BufReader, Write};
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
    assert!(help.contains("daemon"));
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("worker"))
    );
    let install_help = command(dir.path())
        .args(["daemon", "install", "--help"])
        .output()
        .unwrap();
    assert!(install_help.status.success());
    let invalid = dir.path().join("invalid.toml");
    std::fs::write(&invalid, "reasoning = 'impossible'").unwrap();
    let error = command(dir.path())
        .args(["--config", invalid.to_str().unwrap(), "-p", "hello"])
        .env("XI_BIN", dir.path().join("must-not-start"))
        .output()
        .unwrap();
    assert_eq!(error.status.code(), Some(2));
    assert!(
        String::from_utf8(error.stderr)
            .unwrap()
            .contains("invalid.toml")
    );
    assert!(!dir.path().join("data/xi/sessions").exists());
    assert!(!dir.path().join("state/xi").exists());
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
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut messages = Vec::new();
    writeln!(
        stdin,
        "{}",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocol_version": xi_protocol::PROTOCOL_VERSION,
                "client": {"name": "cli-test", "version": "0"},
            },
        })
    )
    .unwrap();
    read_until(&mut stdout, &mut messages, |message| message["id"] == 1);
    assert_eq!(
        messages.last().unwrap()["result"]["capabilities"]["private"],
        true,
    );
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "id":2, "method":"session/create", "params":{"cwd":workspace}})
    )
    .unwrap();
    read_until(&mut stdout, &mut messages, |message| message["id"] == 2);
    let reply = messages.iter().find(|message| message["id"] == 2).unwrap();
    let result = &reply["result"];
    let events = result["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "session_started");
    assert!(!result["running"].as_bool().unwrap());
    let path = result["info"]["path"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
    writeln!(stdin, "{{bad-json").unwrap();
    read_until(&mut stdout, &mut messages, |message| {
        message["error"]["code"] == -32700
    });
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "id":3, "method":"not/a/method"})
    )
    .unwrap();
    read_until(&mut stdout, &mut messages, |message| message["id"] == 3);
    assert_eq!(messages.last().unwrap()["error"]["code"], -32601);
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_until(reader: &mut impl BufRead, messages: &mut Vec<Value>, done: impl Fn(&Value) -> bool) {
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0, "unexpected EOF");
        let message = serde_json::from_str(&line).unwrap();
        let finished = done(&message);
        messages.push(message);
        if finished {
            break;
        }
    }
}
