//! Real sandboxed extension processes with a deterministic provider.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use xi_kernel::config::{Config, Paths};
use xi_kernel::{ApprovalMode, Backend, EventBus, Session, TurnOptions};
use xi_protocol::event::{Approver, StopReason, Usage};
use xi_protocol::methods::client::Delta;
use xi_protocol::model::ModelInfo;
use xi_protocol::{Block, Decision, EventKind, OutputBlock};
use xi_provider::{BoxFuture, Provider, Request, Response};

struct Fake {
    input: Value,
    requests: Mutex<Vec<Request>>,
}
impl Provider for Fake {
    fn stream<'a>(
        &'a self,
        request: &'a Request,
        _: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, xi_provider::Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            let wire = format!("{:?}", request.messages);
            if wire.contains("slow child") {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            let child = wire.contains("child one") || wire.contains("child two");
            let seen_result = request
                .messages
                .iter()
                .any(|message| matches!(message, xi_provider::Message::ToolResults(_)));
            let content = if wire.contains("ask child") && !seen_result {
                vec![Block::ToolUse {
                    id: "ask-call".into(),
                    name: "bash".into(),
                    input: json!({"command":"touch forbidden"}),
                    extra: Default::default(),
                }]
            } else if child && self.input["read_children"] == true && !seen_result {
                vec![Block::ToolUse {
                    id: "read-call".into(),
                    name: "read".into(),
                    input: json!({"path":"sample"}),
                    extra: Default::default(),
                }]
            } else if child {
                vec![Block::Text {
                    text: if wire.contains("child one") {
                        "first child"
                    } else {
                        "second child"
                    }
                    .into(),
                }]
            } else if seen_result {
                vec![Block::Text {
                    text: "parent done".into(),
                }]
            } else {
                vec![Block::ToolUse {
                    id: "probe-call".into(),
                    name: if self.input.get("command").is_some() {
                        "bash"
                    } else {
                        "probe"
                    }
                    .into(),
                    input: self.input.clone(),
                    extra: Default::default(),
                }]
            };
            let stop = if matches!(content[0], Block::ToolUse { .. }) {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            Ok(Response {
                content,
                stop,
                usage: Usage {
                    input: 5,
                    output: 3,
                    cache_read: 2,
                    cache_write: 1,
                },
            })
        })
    }
}

struct Fixture {
    _dir: TempDir,
    paths: Paths,
    workspace: PathBuf,
    package: PathBuf,
}
impl Fixture {
    fn new(capabilities: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let workspace = root.join("workspace");
        let package = root.join("extension");
        for path in [&workspace, &package, &paths.home, &paths.config] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(workspace.join("secret"), "not readable").unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .canonicalize()
            .unwrap();
        let sdk = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../sdk/typescript")
            .canonicalize()
            .unwrap();
        std::fs::write(package.join("xi-extension.toml"), format!("name = 'probe'\nversion = '0.1.0'\nrun = ['node', '{}']\n[capabilities]\nfs_read = ['{}', '{}']\n{capabilities}\n", source.join("extension.ts").display(), source.display(), sdk.display())).unwrap();
        Self {
            _dir: dir,
            paths,
            workspace,
            package,
        }
    }
    fn config(&self) -> Config {
        let mut config = Config::load(self.paths.clone(), &self.workspace, &[]).unwrap();
        config.extensions.push(self.package.clone());
        config
    }
    fn session(&self, input: Value) -> (Session, EventBus, Arc<Fake>) {
        self.session_with(input, self.config())
    }
    fn session_with(&self, input: Value, config: Config) -> (Session, EventBus, Arc<Fake>) {
        let fake = Arc::new(Fake {
            input,
            requests: Mutex::new(Vec::new()),
        });
        let bus = EventBus::default();
        let tools = xi_tools::toolset(&config.tools).unwrap();
        let session = Session::create(
            config,
            None,
            bus.clone(),
            TurnOptions {
                max_turns: 4,
                approval: ApprovalMode::Headless(false),
                leaser: None,
            },
            Some(Backend {
                provider: fake.clone(),
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
        .unwrap();
        (session, bus, fake)
    }
}
async fn run(session: &Session) -> xi_kernel::TurnOutcome {
    tokio::time::timeout(
        Duration::from_secs(20),
        session.prompt(vec![Block::Text {
            text: "parent prompt".into(),
        }]),
    )
    .await
    .expect("extension deadlocked")
    .unwrap()
}
fn output(events: &[xi_protocol::Event]) -> &xi_protocol::ToolOutput {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap()
}
fn text(output: &xi_protocol::ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            OutputBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn registration_full_duplex_hooks_commands_compact_and_sandbox() {
    let fixture = Fixture::new("");
    let mut config = fixture.config();
    config.compaction.keep_recent_tokens = 0;
    let (session, bus, fake) = fixture.session_with(json!({"mode":"replace"}), config);
    assert!(!run(&session).await.failed);
    let info = session.info().await;
    assert_eq!(text(output(&info.events)), "replacement");
    assert!(
        output(&info.events).untrusted,
        "post hook must not clear taint"
    );
    assert!(
        info.events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Tainted { .. }))
    );
    assert!(
        info.events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Custom { kind, .. } if kind == "turn_end"))
    );
    assert_eq!(
        fake.requests.lock().unwrap()[0].tools.last().unwrap().name,
        "probe"
    );
    assert_eq!(
        session
            .command("echo", "hello")
            .await
            .unwrap()
            .unwrap()
            .message
            .as_deref(),
        Some("hello")
    );
    assert!(session.command("missing", "").await.unwrap().is_none());
    let requests = fake.requests.lock().unwrap().len();
    session.compact().await.unwrap();
    let compacted = session.info().await;
    assert!(matches!(
        &compacted.events.last().unwrap().kind,
        EventKind::Compaction { summary, usage: None, .. } if summary == "extension summary"
    ));
    assert_eq!(fake.requests.lock().unwrap().len(), requests);
    let log = info.path.with_extension("probe.log");
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("probe invoked")
    );
    bus.shutdown_extensions().await;

    let (session, bus, _) = fixture.session(json!({"mode":"sandbox"}));
    assert!(!run(&session).await.failed);
    assert!(!output(&session.info().await.events).is_error);
    assert!(!fixture.workspace.join("escape").exists());
    bus.shutdown_extensions().await;
}

#[tokio::test]
async fn pre_tool_denies_and_rewrites_before_policy() {
    let fixture = Fixture::new("");
    std::fs::write(
        fixture.paths.config.join("config.toml"),
        "[[rule]]\neffect='deny'\naction='exec'\npattern='blocked command'\n",
    )
    .unwrap();
    for (input, reason) in [
        (json!({"mode":"deny"}), "hook denied"),
        (json!({"command":"rewrite me"}), "denied by policy"),
        (json!({"mode":"invalid"}), "invalid outcome"),
    ] {
        let (session, bus, _) = fixture.session(input);
        run(&session).await;
        let info = session.info().await;
        assert!(output(&info.events).is_error);
        assert!(text(output(&info.events)).contains(reason));
        if reason == "denied by policy" {
            assert!(info.events.iter().any(|e| matches!(&e.kind, EventKind::ToolDecision { action, decision: Decision::Deny, .. } if action == "exec blocked command")));
        }
        bus.shutdown_extensions().await;
    }
}

#[tokio::test]
async fn crash_is_an_error_result_logged_and_restarted_on_next_use() {
    let fixture = Fixture::new("");
    let (session, bus, _) = fixture.session(json!({"mode":"crash"}));
    run(&session).await;
    let info = session.info().await;
    assert!(output(&info.events).is_error);
    assert!(
        std::fs::read_to_string(info.path.with_extension("probe.log"))
            .unwrap()
            .contains("extension probe stopped")
    );
    let echoed = session.command("echo", "again").await.unwrap().unwrap();
    assert_eq!(echoed.message.as_deref(), Some("again"));
    bus.shutdown_extensions().await;
}

#[tokio::test]
async fn net_refuses_before_start_and_subagents_require_capability() {
    let fixture = Fixture::new("net=['example.org']");
    let (session, bus, _) = fixture.session(json!({}));
    let error = session
        .prompt(vec![Block::Text {
            text: "hello".into(),
        }])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("egress proxy"));
    bus.shutdown_extensions().await;
    let fixture = Fixture::new("");
    let (session, bus, _) = fixture.session(json!({"mode":"child"}));
    run(&session).await;
    assert!(text(output(&session.info().await.events)).contains("subagents capability"));
    bus.shutdown_extensions().await;
}

#[tokio::test]
async fn concurrent_children_have_linked_logs_reduced_tools_and_usage() {
    let fixture = Fixture::new("subagents=true");
    let (session, bus, fake) = fixture.session(json!({"mode":"child"}));
    run(&session).await;
    let info = session.info().await;
    assert_eq!(text(output(&info.events)), "first child\nsecond child");
    let results = info
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Custom { kind, data, .. } if kind == "children" => Some(&data["results"]),
            _ => None,
        })
        .unwrap();
    assert_eq!(results.as_array().unwrap().len(), 2);
    for result in results.as_array().unwrap() {
        assert_eq!(
            result["usage"],
            json!({"input":5,"output":3,"cache_read":2,"cache_write":1})
        );
        let path = PathBuf::from(result["path"].as_str().unwrap());
        assert_eq!(path.parent(), info.path.parent());
        let log = xi_session::SessionLog::open(path).unwrap();
        assert!(
            matches!(log.root().kind, EventKind::SessionStarted { parent_session: Some(parent), .. } if parent == session.id())
        );
    }
    for request in fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.tools.len() == 1)
    {
        assert_eq!(request.tools[0].name, "read");
    }
    assert_eq!(
        fake.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.tools.len() == 1)
            .count(),
        2
    );
    bus.shutdown_extensions().await;
}

#[tokio::test]
async fn children_inherit_parent_read_format_without_the_editor() {
    for editor in ["edit", "hashline"] {
        let fixture = Fixture::new("subagents=true");
        std::fs::write(fixture.workspace.join("sample"), "sample\n").unwrap();
        let mut config = fixture.config();
        config.tools = ["read", editor, "write", "bash", "search"]
            .map(str::to_owned)
            .into();
        let (session, bus, fake) =
            fixture.session_with(json!({"mode":"child","read_children":true}), config);
        assert!(!run(&session).await.failed);
        let requests = fake.requests.lock().unwrap().clone();
        let children: Vec<_> = requests.iter().filter(|r| r.tools.len() == 1).collect();
        assert_eq!(children.len(), 4);
        for request in children {
            assert_eq!(request.tools[0].name, "read");
            assert_eq!(
                request.tools[0].description.contains("LINE:HH|text"),
                editor == "hashline"
            );
            for message in &request.messages {
                if let xi_provider::Message::ToolResults(results) = message {
                    for result in results {
                        let row = text(&result.output);
                        if editor == "hashline" {
                            assert!(row.starts_with("1:"));
                            assert!(row.ends_with("|sample\n"));
                        } else {
                            assert_eq!(row, "1|sample\n");
                        }
                    }
                }
            }
        }
        bus.shutdown_extensions().await;
    }
}

#[tokio::test]
async fn children_reject_extension_tools_and_cancel_with_parent() {
    let fixture = Fixture::new("subagents=true");
    let (session, bus, _) = fixture.session(json!({"mode":"unknown_child"}));
    run(&session).await;
    assert!(text(output(&session.info().await.events)).contains("parent's built-in tools"));
    bus.shutdown_extensions().await;
    let (session, bus, fake) = fixture.session(json!({"mode":"slow_child"}));
    let reply = session.submit(vec![Block::Text {
        text: "parent prompt".into(),
    }]);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if fake.requests.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    session.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(3), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(outcome.cancelled);
    bus.shutdown_extensions().await;
}

#[test]
fn project_extensions_are_trust_gated_and_relative_paths_resolve() {
    let fixture = Fixture::new("");
    let project = fixture.workspace.join(".xi");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("config.toml"),
        "extensions=['../../extension']\n",
    )
    .unwrap();
    assert!(
        Config::load(fixture.paths.clone(), &fixture.workspace, &[])
            .unwrap()
            .extensions
            .is_empty()
    );
    fixture.paths.trust(&fixture.workspace).unwrap();
    let config = Config::load(fixture.paths.clone(), &fixture.workspace, &[]).unwrap();
    assert_eq!(
        config.extensions[0].canonicalize().unwrap(),
        fixture.package
    );
}

#[tokio::test]
async fn children_deny_asks_and_record_headless_approval() {
    let fixture = Fixture::new("subagents=true");
    std::fs::write(
        fixture.paths.config.join("config.toml"),
        "[[rule]]\neffect='ask'\naction='exec'\npattern='touch forbidden'\n",
    )
    .unwrap();
    let (session, bus, _) = fixture.session(json!({"mode":"ask_child"}));
    run(&session).await;
    let info = session.info().await;
    let path = info
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Custom { kind, data, .. } if kind == "ask_child" => data["path"].as_str(),
            _ => None,
        })
        .unwrap();
    let child = xi_session::SessionLog::open(path).unwrap();
    assert!(child.events().iter().any(|event| matches!(
        event.kind,
        EventKind::ApprovalResolved {
            approved: false,
            by: Approver::Headless,
            ..
        }
    )));
    assert!(!fixture.workspace.join("forbidden").exists());
    bus.shutdown_extensions().await;
}
