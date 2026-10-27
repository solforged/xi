//! Real sandboxed `bash` reaching a local server through the egress proxy.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;
use xi_kernel::config::{Config, Paths};
use xi_kernel::{ApprovalMode, Backend, EventBus, Session, TurnOptions};
use xi_protocol::BoxFuture;
use xi_protocol::event::{Approver, StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::Delta;
use xi_protocol::model::ModelInfo;
use xi_protocol::{Block, Decision, EventKind, OutputBlock, ToolCallId, ToolOutput};
use xi_provider::{Provider, Request, Response};
use xi_tools::{Context, Crossing, Ran, Tool};

struct Script(Mutex<VecDeque<Response>>);
impl Provider for Script {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        _: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, xi_provider::Error>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted"))
        })
    }
}

/// Returns untrusted output, so the branch becomes tainted.
struct Taint;
impl Tool for Taint {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "taint".into(),
            description: "Test tool".into(),
            input_schema: json!({"type":"object"}),
        }
    }
    fn crossings(&self, _: &serde_json::Value, _: &Context) -> Result<Vec<Crossing>, String> {
        Ok(Vec::new())
    }
    fn run<'a>(
        &'a self,
        _: &'a ToolCallId,
        _: serde_json::Value,
        _: &'a Context,
    ) -> BoxFuture<'a, Ran> {
        Box::pin(async move {
            Ran {
                output: ToolOutput {
                    content: vec![OutputBlock::Text {
                        text: "fetched".into(),
                    }],
                    is_error: false,
                    untrusted: true,
                },
                spill: None,
            }
        })
    }
}

/// A loopback HTTP server answering every request with `hello`.
fn server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                head.push(byte[0]);
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
        }
    });
    port
}

struct Fixture {
    _dir: TempDir,
    paths: Paths,
    workspace: std::path::PathBuf,
}
impl Fixture {
    fn new(rules: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let workspace = root.join("workspace");
        for path in [&workspace, &paths.config, &paths.home] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(paths.config.join("config.toml"), rules).unwrap();
        Self {
            _dir: dir,
            paths,
            workspace,
        }
    }
    fn session(&self, replies: Vec<Response>) -> Session {
        let config = Config::load(self.paths.clone(), &self.workspace, &[]).unwrap();
        let mut tools = xi_tools::toolset(&["bash"]).unwrap();
        tools.push(Arc::new(Taint));
        Session::create(
            config,
            None,
            EventBus::default(),
            TurnOptions {
                approval: ApprovalMode::Headless(false),
                ..TurnOptions::default()
            },
            Some(Backend {
                provider: Arc::new(Script(Mutex::new(replies.into()))),
                model: ModelInfo {
                    provider: "test".into(),
                    id: "test".into(),
                    context_window: 8192,
                    max_output: 1024,
                    reasoning: Vec::new(),
                    pricing: None,
                },
                tools,
            }),
        )
        .unwrap()
    }
}

fn calls(calls: &[(&str, &str, &str)]) -> Response {
    Response {
        content: calls
            .iter()
            .map(|(id, name, command)| Block::ToolUse {
                id: (*id).into(),
                name: (*name).into(),
                input: if *name == "bash" {
                    json!({"command": command})
                } else {
                    json!({})
                },
                extra: Default::default(),
            })
            .collect(),
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    }
}
fn done() -> Response {
    Response {
        content: vec![Block::Text {
            text: "done".into(),
        }],
        stop: StopReason::EndTurn,
        usage: Usage::default(),
    }
}
fn result(events: &[xi_protocol::Event], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult { call, output, .. } if call.0 == id => Some((
                output
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        OutputBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
                output.is_error,
            )),
            _ => None,
        })
        .unwrap()
}
fn decisions(
    events: &[xi_protocol::Event],
    during: &str,
) -> Vec<(String, Decision, Option<String>, bool)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::ToolDecision {
                action,
                decision,
                rule,
                taint,
                during: Some(call),
                extension: None,
                ..
            } if call.0 == during => Some((action.clone(), *decision, rule.clone(), *taint)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn bash_reaches_an_allowed_host_by_absolute_form_and_connect() {
    let port = server();
    let fixture = Fixture::new("[[rule]]\naction='net'\npattern='127.0.0.1'\neffect='allow'\n");
    let plain = format!("curl -sS http://127.0.0.1:{port}/");
    let tunnel = format!(
        "curl -sS -p http://127.0.0.1:{port}/ && echo && env | grep -ic '_PROXY=http://127.0.0.1:'"
    );
    let session = fixture.session(vec![
        calls(&[("plain", "bash", &plain), ("tunnel", "bash", &tunnel)]),
        done(),
    ]);
    session
        .prompt(vec![Block::Text { text: "go".into() }])
        .await
        .unwrap();
    let events = session.info().await.events;
    assert_eq!(result(&events, "plain"), ("hello\n[exit 0]".into(), false));
    assert_eq!(
        result(&events, "tunnel"),
        ("hello\n5\n\n[exit 0]".into(), false)
    );
    let expected = format!("net 127.0.0.1:{port}");
    for id in ["plain", "tunnel"] {
        let decided = decisions(&events, id);
        assert_eq!(decided.len(), 1, "{id}: {decided:?}");
        assert_eq!(decided[0].0, expected);
        assert_eq!(decided[0].1, Decision::Allow);
        assert!(
            decided[0]
                .2
                .as_deref()
                .is_some_and(|rule| rule.ends_with("#0"))
        );
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ApprovalResolved { .. }))
    );
}

#[tokio::test]
async fn egress_without_a_rule_asks_and_a_headless_deny_is_a_403() {
    let port = server();
    let fixture = Fixture::new("");
    let command = format!("curl -sS -f http://127.0.0.1:{port}/ ; echo exit=$?");
    let session = fixture.session(vec![calls(&[("fetch", "bash", &command)]), done()]);
    session
        .prompt(vec![Block::Text { text: "go".into() }])
        .await
        .unwrap();
    let events = session.info().await.events;
    let (text, _) = result(&events, "fetch");
    assert!(text.contains("403") && text.contains("exit=22"), "{text}");
    let decided = decisions(&events, "fetch");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].1, Decision::Ask);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ApprovalResolved { call, approved: false, by: Approver::Headless, .. }
            if call.0.starts_with("net-")
    )));
    // The connection the sandbox is allowed to make is only the proxy.
    let direct = format!("curl -sS --noproxy '*' -m 2 http://127.0.0.1:{port}/ ; echo exit=$?");
    let session = fixture.session(vec![calls(&[("direct", "bash", &direct)]), done()]);
    session
        .prompt(vec![Block::Text { text: "go".into() }])
        .await
        .unwrap();
    let (text, _) = result(&session.info().await.events, "direct");
    assert!(text.contains("exit=7"), "{text}");
}

#[tokio::test]
async fn taint_turns_an_allowed_host_into_an_ask() {
    let port = server();
    let fixture = Fixture::new("[[rule]]\naction='net'\npattern='**'\neffect='allow'\n");
    let command = format!("curl -sS -f http://127.0.0.1:{port}/ ; echo exit=$?");
    let session = fixture.session(vec![
        calls(&[
            ("clean", "bash", &command),
            ("taint", "taint", ""),
            ("dirty", "bash", &command),
        ]),
        done(),
    ]);
    session
        .prompt(vec![Block::Text { text: "go".into() }])
        .await
        .unwrap();
    let events = session.info().await.events;
    assert!(result(&events, "clean").0.starts_with("helloexit=0\n"));
    assert!(result(&events, "dirty").0.contains("exit=22"));
    assert_eq!(decisions(&events, "clean")[0].1, Decision::Allow);
    let dirty = &decisions(&events, "dirty")[0];
    assert_eq!((dirty.1, dirty.3), (Decision::Ask, true));
}

#[tokio::test]
async fn the_proxy_rejects_what_it_cannot_route() {
    let fixture = Fixture::new("[[rule]]\naction='net'\npattern='**'\neffect='allow'\n");
    let command = "curl -sS --noproxy '*' -o /dev/null -w '%{http_code}\\n' \"$HTTP_PROXY/path\" ; \
                   curl -sS -o /dev/null -w '%{http_code}\\n' http://127.0.0.1:9/";
    let session = fixture.session(vec![calls(&[("bad", "bash", command)]), done()]);
    session
        .prompt(vec![Block::Text { text: "go".into() }])
        .await
        .unwrap();
    let events = session.info().await.events;
    // Origin-form to the proxy itself is a 400; an unreachable upstream is a 502.
    assert!(result(&events, "bad").0.starts_with("400\n502\n"));
    assert_eq!(decisions(&events, "bad").len(), 1);
    assert!(Path::new("/usr/bin/curl").exists());
}
