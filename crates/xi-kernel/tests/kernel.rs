//! End-to-end kernel behavior with scripted providers and tools.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Notify, Semaphore};
use xi_kernel::config::{Config, DaemonSettings, Paths, daemon_settings, sessions_root};
use xi_kernel::{ApprovalMode, Backend, EventBus, Leaser, Notification, Session, TurnOptions};
use xi_policy::{Action, Taint};
use xi_protocol::event::{Approver, ModelRef, Remember, StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::Delta;
use xi_protocol::model::{ModelInfo, Reasoning};
use xi_protocol::{Block, Decision, EventKind, OutputBlock, ToolCallId, ToolOutput};
use xi_provider::{BoxFuture, Error, ErrorKind, Message, Provider, Request, Response};
use xi_tools::{Context, Crossing, Ran, Tool};

struct Gate {
    entered: Notify,
    release: Semaphore,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Semaphore::new(0),
        })
    }
}
enum Step {
    Reply(Response),
    Fail(Error),
    DeltaFail(Error),
    Wait(Arc<Gate>),
}
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Request>>,
}
impl Script {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
}
impl Provider for Script {
    fn stream<'a>(
        &'a self,
        request: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted");
            match step {
                Step::Reply(response) => {
                    for block in &response.content {
                        if let Block::Text { text } = block {
                            on_delta(Delta::Text { text: text.clone() });
                        }
                    }
                    Ok(response)
                }
                Step::Fail(error) => Err(error),
                Step::DeltaFail(error) => {
                    on_delta(Delta::Text {
                        text: "discarded".into(),
                    });
                    Err(error)
                }
                Step::Wait(gate) => {
                    on_delta(Delta::Text {
                        text: "partial".into(),
                    });
                    gate.entered.notify_one();
                    gate.release.acquire().await.unwrap().forget();
                    Ok(done())
                }
            }
        })
    }
}

#[derive(Clone)]
struct TestLeaser {
    script: Arc<Script>,
    spans: Arc<Mutex<Vec<LeaseSpan>>>,
}
struct LeaseSpan {
    endpoint: String,
    acquired_after: usize,
    released_after: Option<usize>,
}
struct TestLease {
    leaser: TestLeaser,
    index: usize,
}
impl Drop for TestLease {
    fn drop(&mut self) {
        let requests = self.leaser.script.requests.lock().unwrap().len();
        self.leaser.spans.lock().unwrap()[self.index].released_after = Some(requests);
    }
}
impl Leaser for TestLeaser {
    fn lease<'a>(&'a self, endpoint: &'a str) -> BoxFuture<'a, xi_kernel::Result<Box<dyn Send>>> {
        Box::pin(async move {
            let requests = self.script.requests.lock().unwrap().len();
            let mut spans = self.spans.lock().unwrap();
            let index = spans.len();
            spans.push(LeaseSpan {
                endpoint: endpoint.into(),
                acquired_after: requests,
                released_after: None,
            });
            Ok(Box::new(TestLease {
                leaser: self.clone(),
                index,
            }) as Box<dyn Send>)
        })
    }
}
struct FakeTool {
    gate: Option<Arc<Gate>>,
}
impl Tool for FakeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fake".into(),
            description: "Test tool".into(),
            input_schema: json!({"type":"object"}),
        }
    }
    fn crossings(&self, input: &serde_json::Value, _: &Context) -> Result<Vec<Crossing>, String> {
        if input.get("invalid").is_some() {
            return Err("invalid input".into());
        }
        if let Some(net) = input.get("net") {
            Ok(vec![Crossing::Net(
                net.as_str().unwrap_or("example.org").into(),
            )])
        } else {
            Ok(vec![Crossing::Exec("test command".into())])
        }
    }
    fn run<'a>(
        &'a self,
        _: &'a ToolCallId,
        input: serde_json::Value,
        _: &'a Context,
    ) -> BoxFuture<'a, Ran> {
        Box::pin(async move {
            if let Some(gate) = &self.gate {
                gate.entered.notify_one();
                gate.release.acquire().await.unwrap().forget();
            }
            Ran {
                output: ToolOutput {
                    content: vec![OutputBlock::Text {
                        text: "result\nsecond line".into(),
                    }],
                    is_error: false,
                    untrusted: input.get("untrusted").is_some(),
                },
                spill: None,
            }
        })
    }
}
struct Fixture {
    _dir: TempDir,
    paths: Paths,
    workspace: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
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
        Self {
            _dir: dir,
            paths,
            workspace,
        }
    }
    fn config(&self) -> Config {
        Config::load(self.paths.clone(), &self.workspace, &[]).unwrap()
    }
    fn session(
        &self,
        script: Arc<Script>,
        tool: FakeTool,
        options: TurnOptions,
    ) -> (Session, tokio::sync::mpsc::UnboundedReceiver<Notification>) {
        let bus = EventBus::default();
        let rx = bus.subscribe();
        let session = Session::create(
            self.config(),
            None,
            bus,
            options,
            Some(Backend {
                provider: script,
                model: model(),
                tools: vec![Arc::new(tool)],
            }),
        )
        .unwrap();
        (session, rx)
    }
}
fn model() -> ModelInfo {
    ModelInfo {
        provider: "test".into(),
        id: "test".into(),
        context_window: 8192,
        max_output: 1024,
        reasoning: Reasoning::ALL.to_vec(),
        pricing: None,
    }
}
fn done() -> Response {
    Response {
        content: vec![Block::Text {
            text: "done".into(),
        }],
        stop: StopReason::EndTurn,
        usage: Usage {
            input: 4,
            output: 2,
            ..Usage::default()
        },
    }
}
fn call(id: &str, name: &str, input: serde_json::Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}
fn batch(calls: Vec<Block>) -> Step {
    Step::Reply(Response {
        content: calls,
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    })
}
fn input(text: &str) -> Vec<Block> {
    vec![Block::Text { text: text.into() }]
}
fn opts() -> TurnOptions {
    TurnOptions {
        approval: ApprovalMode::Headless(false),
        ..TurnOptions::default()
    }
}
fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn kind(event: &xi_protocol::Event) -> String {
    serde_json::to_value(event).unwrap()["type"]
        .as_str()
        .unwrap()
        .into()
}

async fn wait_for_ask(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Notification>) -> ToolCallId {
    loop {
        if let Notification::Event(params) = rx.recv().await.unwrap()
            && let EventKind::ToolDecision {
                call,
                decision: Decision::Ask,
                ..
            } = params.event.kind
        {
            return call;
        }
    }
}

#[tokio::test]
async fn full_tool_loop_events_context_cache_and_usage() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![call("c1", "fake", json!({"untrusted":true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    let result = session.prompt(input("work")).await.unwrap();
    assert!(!result.failed);
    assert_eq!(result.text, "done");
    let info = session.info().await;
    assert_eq!(
        info.events.iter().map(kind).collect::<Vec<_>>(),
        [
            "session_started",
            "user_message",
            "assistant_message",
            "tool_decision",
            "tool_result",
            "tainted",
            "assistant_message"
        ]
    );
    let notified = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|n| {
            if let Notification::Event(p) = n {
                Some(p.event)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(notified, info.events);
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].cache_key, Some(session.id().to_string()));
    assert_eq!(requests[0].max_tokens, 1024);
    assert!(
        matches!(&requests[1].messages[2], Message::ToolResults(results) if results.len() == 1)
    );
    assert!(matches!(
        info.events.last().unwrap().kind,
        EventKind::AssistantMessage {
            usage: Usage {
                input: 4,
                output: 2,
                ..
            },
            ..
        }
    ));
}

#[test]
fn context_groups_results_and_repairs_dangling_calls_after_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = xi_session::SessionLog::create(
        dir.path().join("log.jsonl"),
        EventKind::SessionStarted {
            session: xi_protocol::SessionId::generate(),
            cwd: dir.path().into(),
            model: ModelRef {
                provider: "test".into(),
                model: "test".into(),
            },
            xi_version: "test".into(),
            parent_session: None,
        },
    )
    .unwrap();
    let mut head = log.root().id;
    for event in [
        EventKind::UserMessage {
            content: input("old"),
        },
        EventKind::Compaction {
            summary: "summary".into(),
            tokens_before: 42,
            kept: None,
            usage: None,
        },
        EventKind::AssistantMessage {
            model: ModelRef {
                provider: "test".into(),
                model: "test".into(),
            },
            content: vec![
                call("a", "fake", json!({})),
                call("b", "fake", json!({})),
                call("c", "fake", json!({})),
            ],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        },
        EventKind::ToolResult {
            call: "a".into(),
            output: ToolOutput {
                content: vec![],
                is_error: false,
                untrusted: false,
            },
            spill: None,
        },
        EventKind::Tainted {
            call: None,
            reason: "test".into(),
        },
        EventKind::ToolResult {
            call: "b".into(),
            output: ToolOutput {
                content: vec![],
                is_error: false,
                untrusted: false,
            },
            spill: None,
        },
        EventKind::UserMessage {
            content: input("after crash"),
        },
    ] {
        head = log.append(head, event).unwrap().id;
    }
    let messages = xi_kernel::context::messages(log.context(head).unwrap());
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], Message::User(input("summary")));
    let Message::ToolResults(results) = &messages[2] else {
        panic!("missing grouped results")
    };
    assert_eq!(
        results
            .iter()
            .map(|r| r.call.0.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert!(results[2].output.is_error);
    assert_eq!(log.events().len(), 8, "synthetic results are not persisted");
}

#[test]
fn trust_gates_settings_but_project_policy_only_tightens() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "default_model = 'global/model'\nreasoning = 'low'\n[[rule]]\neffect='deny'\naction='net'\npattern='**'\n",
    );
    write(
        &fixture.workspace.join(".xi/config.toml"),
        "default_model = 'project/model'\n[[rule]]\neffect='allow'\naction='net'\npattern='**'\n[[rule]]\neffect='deny'\naction='exec'\npattern='git push**'\n",
    );
    write(
        &fixture.workspace.join(".xi/config.local.toml"),
        "reasoning='high'\n[[rule]]\neffect='allow'\naction='exec'\npattern='git push**'\n",
    );
    let config = fixture.config();
    assert_eq!(config.model.provider, "global");
    assert_eq!(config.reasoning, xi_protocol::model::Reasoning::Low);
    assert_eq!(config.warnings.len(), 2);
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Net {
                    host: "example.org"
                },
                Taint::Clean
            )
            .decision,
        Decision::Deny
    );
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Exec {
                    command: "git push origin"
                },
                Taint::Clean
            )
            .decision,
        Decision::Deny
    );
    assert_eq!(
        fixture.paths.trust(&fixture.workspace).unwrap(),
        fixture.workspace
    );
    let config = fixture.config();
    assert!(config.trusted);
    assert_eq!(config.model.provider, "project");
    assert_eq!(config.reasoning, xi_protocol::model::Reasoning::High);
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Exec {
                    command: "git push origin"
                },
                Taint::Clean
            )
            .decision,
        Decision::Deny
    );
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Net {
                    host: "example.org"
                },
                Taint::Clean
            )
            .decision,
        Decision::Deny
    );
}

#[test]
fn explicit_config_order_and_sandbox_field_layering() {
    let fixture = Fixture::new();
    let first = fixture.paths.home.join("first.toml");
    let second = fixture.paths.home.join("second.toml");
    write(
        &first,
        "default_model='one/m'\n[sandbox]\nreadable=['~/.cargo']\nenv=['FIRST']\n",
    );
    write(
        &second,
        "default_model='two/m'\n[sandbox]\nenv=['SECOND']\n[[model]]\nprovider='two'\nid='m'\ncontext_window=1000\nmax_output=100\n",
    );
    let config = Config::load(fixture.paths.clone(), &fixture.workspace, &[first, second]).unwrap();
    assert_eq!(config.model.provider, "two");
    let tmp = fixture.workspace.join("tmp");
    let spill = fixture.paths.data.join("spill");
    let git = fixture.paths.data.join("git");
    let spec = config
        .sandbox_spec(&tmp, &spill, &[fixture.paths.config.join("skills")], &git)
        .unwrap();
    assert!(spec.readable.contains(&fixture.paths.home.join(".cargo")));
    assert!(spec.readable.contains(&spill));
    assert!(spec.env.contains(&"SECOND".into()));
    assert!(!spec.env.contains(&"FIRST".into()));
}

#[test]
fn daemon_defaults_and_global_settings() {
    let fixture = Fixture::new();
    let defaults = daemon_settings(&fixture.paths).unwrap();
    assert_eq!(defaults, DaemonSettings::default());
    assert_eq!(defaults.idle_exit, 600);
    assert_eq!(defaults.notify, None);
    assert_eq!(defaults.approval_timeout, None);
    assert!(defaults.limit.is_empty());
    assert_eq!(fixture.config().daemon, defaults);
    assert_eq!(
        sessions_root(&fixture.paths),
        fixture.paths.data.join("sessions")
    );
    assert_eq!(
        fixture.config().session_dir().parent(),
        Some(sessions_root(&fixture.paths).as_path())
    );

    write(
        &fixture.paths.config.join("config.toml"),
        "default_model='global/model'\n[daemon]\nidle_exit=42\nnotify=['notify', '--json']\napproval_timeout=17\n[[daemon.limit]]\nendpoint='https://example.test/v1'\nconcurrency=3\n",
    );
    let settings = daemon_settings(&fixture.paths).unwrap();
    assert_eq!(settings.idle_exit, 42);
    assert_eq!(
        settings.notify,
        Some(vec!["notify".into(), "--json".into()])
    );
    assert_eq!(settings.approval_timeout, Some(17));
    assert_eq!(settings.limit.len(), 1);
    assert_eq!(settings.limit[0].endpoint, "https://example.test/v1");
    assert_eq!(settings.limit[0].concurrency, 3);
    assert_eq!(fixture.config().daemon, settings);
}

#[test]
fn daemon_settings_ignore_every_non_global_layer_even_when_trusted() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "[daemon]\nidle_exit=42\n",
    );
    let explicit = fixture.paths.home.join("extra.toml");
    for path in [
        explicit.clone(),
        fixture.workspace.join(".xi/config.toml"),
        fixture.workspace.join(".xi/config.local.toml"),
    ] {
        // Ignored tables are not parsed or validated.
        write(
            &path,
            "[daemon]\nunknown=true\n[[daemon.limit]]\nconcurrency=0\n",
        );
    }
    for trusted in [false, true] {
        if trusted {
            fixture.paths.trust(&fixture.workspace).unwrap();
        }
        let config = Config::load(
            fixture.paths.clone(),
            &fixture.workspace,
            std::slice::from_ref(&explicit),
        )
        .unwrap();
        assert_eq!(config.daemon.idle_exit, 42);
        assert!(config.daemon.limit.is_empty());
        assert_eq!(config.warnings.len(), 3);
        assert!(
            config
                .warnings
                .iter()
                .all(|warning| warning.contains("ignoring")),
            "{:?}",
            config.warnings
        );
    }
}

#[test]
fn global_daemon_rejects_zero_concurrency_and_unknown_fields() {
    let fixture = Fixture::new();
    for (text, expected) in [
        (
            "[[daemon.limit]]\nendpoint='local'\nconcurrency=0\n",
            "concurrency must be at least 1",
        ),
        ("[daemon]\nunknown=true\n", "unknown field"),
        (
            "[[daemon.limit]]\nendpoint='local'\nconcurrency=1\nunknown=true\n",
            "unknown field",
        ),
    ] {
        write(&fixture.paths.config.join("config.toml"), text);
        for error in [
            daemon_settings(&fixture.paths).unwrap_err(),
            Config::load(fixture.paths.clone(), &fixture.workspace, &[]).unwrap_err(),
        ] {
            assert!(matches!(&error, xi_kernel::Error::Config(_)));
            assert!(error.to_string().contains(expected), "{error}");
        }
    }
}

#[test]
fn sandbox_runtime_directory_guard() {
    let fixture = Fixture::new();
    let readable = fixture.paths.home.join("readable");
    let writable = fixture.paths.home.join("writable");
    let alias = fixture.paths.home.join("workspace-link");
    std::os::unix::fs::symlink(&fixture.workspace, &alias).unwrap();
    write(
        &fixture.paths.config.join("config.toml"),
        "[sandbox]\nreadable=['~/readable']\nwritable=['~/writable']\n",
    );
    let cases = [
        (fixture.workspace.join("run"), true),
        (fixture.workspace.clone(), true),
        (fixture.workspace.join("missing/../run"), true),
        (alias.clone(), true),
        (alias.join("run"), true),
        (readable.join("run"), true),
        (writable.join("run"), true),
        (fixture.paths.state.join("run"), false),
        (fixture.workspace.join("../state/run"), false),
    ];
    for (runtime, reject) in cases {
        // Child process environments isolate XI_RUNTIME_DIR from every other test.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox_runtime_directory_guard_child",
                "--nocapture",
            ])
            .env("XI_RUNTIME_DIR", &runtime)
            .env("XI_TEST_RUNTIME_GUARD_ROOT", fixture._dir.path())
            .env(
                "XI_TEST_RUNTIME_GUARD_REJECT",
                if reject { "1" } else { "0" },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "runtime {}:\n{}\n{}",
            runtime.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn sandbox_runtime_directory_guard_child() {
    let Some(root) = std::env::var_os("XI_TEST_RUNTIME_GUARD_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root).canonicalize().unwrap();
    let paths = Paths {
        home: root.join("home"),
        config: root.join("config"),
        data: root.join("data"),
        state: root.join("state"),
    };
    let workspace = root.join("workspace");
    let config = Config::load(paths, &workspace, &[]).unwrap();
    let result = Session::create(
        config,
        None,
        EventBus::default(),
        opts(),
        Some(Backend {
            provider: Script::new(vec![]),
            model: model(),
            tools: vec![Arc::new(FakeTool { gate: None })],
        }),
    );
    if std::env::var("XI_TEST_RUNTIME_GUARD_REJECT").unwrap() == "1" {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("runtime directory inside sandbox was accepted"),
        };
        let xi_kernel::Error::Config(message) = error else {
            panic!("expected config error, got {error}");
        };
        let runtime = xi_protocol::runtime::dir().unwrap();
        assert!(
            message.starts_with(&format!(
                "runtime directory {} is inside sandbox root ",
                runtime.display()
            )),
            "{message}"
        );
    } else {
        assert!(result.is_ok(), "{}", result.err().unwrap());
    }
}

#[test]
fn env_command_fills_only_passed_through_variables() {
    let fixture = Fixture::new();
    let layer = |command: &str| {
        let path = fixture.paths.home.join("env.toml");
        write(
            &path,
            &format!("[sandbox]\nenv = ['EXTRA']\nenv_command = ['/bin/sh', '-c', '{command}']\n"),
        );
        let config = Config::load(fixture.paths.clone(), &fixture.workspace, &[path]).unwrap();
        let data = &fixture.paths.data;
        config.sandbox_spec(
            &data.join("tmp"),
            &data.join("spill"),
            &[],
            &data.join("git"),
        )
    };
    let spec = layer(
        r#"pwd >&2; printf "{\"PATH\":\"/opt/tool/bin:/usr/bin\",\"RUSTUP_TOOLCHAIN\":\"1.2.3\",\"EXTRA\":\"yes\",\"API_TOKEN\":\"secret\",\"TERM\":null}""#,
    )
    .unwrap();
    assert_eq!(spec.vars["RUSTUP_TOOLCHAIN"], "1.2.3");
    assert_eq!(spec.vars["EXTRA"], "yes");
    assert!(
        spec.vars["PATH"].starts_with("/opt/tool/bin:"),
        "{}",
        spec.vars["PATH"]
    );
    assert!(!spec.vars.contains_key("API_TOKEN"));
    assert!(!spec.env.contains(&"API_TOKEN".into()));
    assert!(
        !spec.env.contains(&"TERM".into()),
        "null unsets a passthrough"
    );
    let error = layer("echo no >&2; exit 3").unwrap_err().to_string();
    assert!(
        error.contains("sandbox.env_command") && error.contains("no"),
        "{error}"
    );
    let error = layer("echo not json").unwrap_err().to_string();
    assert!(error.contains("JSON object"), "{error}");
}

#[test]
fn instructions_order_skills_frontmatter_and_workspace_trust() {
    let fixture = Fixture::new();
    write(&fixture.paths.config.join("AGENTS.md"), "GLOBAL_MARKER");
    write(&fixture.workspace.join(".git/config"), "");
    write(&fixture.workspace.join("AGENTS.md"), "ROOT_MARKER");
    let nested = fixture.workspace.join("a/b");
    write(&fixture.workspace.join("a/AGENTS.md"), "MIDDLE_MARKER");
    write(&nested.join("AGENTS.md"), "LEAF_MARKER");
    write(
        &fixture.paths.config.join("skills/global/SKILL.md"),
        "---\nname: global-skill\ndescription: >-\n  a folded\n  description\n---\nSECRET_BODY",
    );
    write(
        &fixture.paths.home.join(".agents/skills/user/SKILL.md"),
        "---\nname: user-skill\ndescription: 'quoted: value'\n---\nSECRET_BODY",
    );
    write(
        &nested.join(".agents/skills/local/SKILL.md"),
        "---\nname: local-skill\ndescription: project\n---\nSECRET_BODY",
    );
    let config = Config::load(fixture.paths.clone(), &nested, &[]).unwrap();
    let context = xi_kernel::context::SystemContext::build(&config).unwrap();
    let positions: Vec<_> = [
        "GLOBAL_MARKER",
        "ROOT_MARKER",
        "MIDDLE_MARKER",
        "LEAF_MARKER",
    ]
    .into_iter()
    .map(|s| context.prompt.find(s).unwrap())
    .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]));
    assert!(context.prompt.contains("a folded description"));
    assert!(context.prompt.contains("quoted: value"));
    assert!(!context.prompt.contains("SECRET_BODY"));
    assert!(!context.prompt.contains("local-skill"));
    fixture.paths.trust(&nested).unwrap();
    let config = Config::load(fixture.paths.clone(), &nested, &[]).unwrap();
    let context = xi_kernel::context::SystemContext::build(&config).unwrap();
    assert!(context.prompt.contains("local-skill"));
    assert!(context.skill_roots.contains(&nested.join(".agents/skills")));
}

#[tokio::test]
async fn approvals_resolve_both_ways_and_record_user() {
    for approved in [false, true] {
        let fixture = Fixture::new();
        let script = Script::new(vec![
            batch(vec![call("ask", "fake", json!({"net":true}))]),
            Step::Reply(done()),
        ]);
        let (session, mut rx) =
            fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
        let reply = session.submit(input("ask"));
        loop {
            if let Notification::Event(params) = rx.recv().await.unwrap()
                && matches!(
                    params.event.kind,
                    EventKind::ToolDecision {
                        decision: Decision::Ask,
                        ..
                    }
                )
            {
                break;
            }
        }
        session
            .approve(&"ask".into(), approved, Remember::Once)
            .unwrap();
        assert!(
            session
                .approve(&"ask".into(), approved, Remember::Once)
                .is_err()
        );
        reply.await.unwrap().unwrap();
        let info = session.info().await;
        assert!(info.events.iter().any(|e| matches!(e.kind, EventKind::ApprovalResolved { approved: value, by: Approver::User, .. } if value == approved)));
        assert!(info.events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.is_error != approved)));
    }
}

#[tokio::test]
async fn taint_in_current_batch_downgrades_network_allow() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "[[rule]]\naction='net'\npattern='**'\neffect='allow'\n",
    );
    let script = Script::new(vec![
        batch(vec![
            call("clean", "fake", json!({"net":true})),
            call("taint", "fake", json!({"untrusted":true})),
            call("dirty", "fake", json!({"net":true})),
        ]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    session.prompt(input("work")).await.unwrap();
    let info = session.info().await;
    let decisions: Vec<_> = info
        .events
        .iter()
        .filter_map(|e| {
            if let EventKind::ToolDecision { decision, .. } = e.kind {
                Some(decision)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(decisions, [Decision::Allow, Decision::Allow, Decision::Ask]);
    assert!(info.events.iter().any(|e| matches!(
        e.kind,
        EventKind::ApprovalResolved {
            by: Approver::Headless,
            approved: false,
            ..
        }
    )));
}

#[tokio::test]
async fn invalid_and_unknown_calls_return_errors_without_running_tools() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![
            call("bad", "fake", json!({"invalid":true})),
            call("unknown", "missing", json!({})),
        ]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    session.prompt(input("work")).await.unwrap();
    let info = session.info().await;
    assert_eq!(
        info.events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.is_error))
            .count(),
        2
    );
    assert!(
        !info
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolDecision { .. }))
    );
}

#[tokio::test]
async fn retries_retryable_failure_then_success() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        Step::Fail(Error::new(ErrorKind::Unavailable, "retry")),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    assert!(!session.prompt(input("work")).await.unwrap().failed);
    {
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
    }
    assert!(
        !session
            .info()
            .await
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::TurnFailed { .. }))
    );
}

#[tokio::test]
async fn nonretryable_failure_and_turn_limit_are_persisted() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Fail(Error::new(
        ErrorKind::Auth,
        "bad credentials",
    ))]);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    assert!(session.prompt(input("work")).await.unwrap().failed);
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    assert!(matches!(
        session.info().await.events.last().unwrap().kind,
        EventKind::TurnFailed { .. }
    ));
    let script = Script::new(vec![batch(vec![call("one", "fake", json!({}))])]);
    let (session, _) = fixture.session(
        script,
        FakeTool { gate: None },
        TurnOptions {
            max_turns: 1,
            ..opts()
        },
    );
    assert!(session.prompt(input("work")).await.unwrap().failed);
    assert!(matches!(
        session.info().await.events.last().unwrap().kind,
        EventKind::TurnFailed { .. }
    ));
}

#[tokio::test]
async fn cancel_mid_stream_keeps_partial_assistant() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![Step::Wait(gate.clone())]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    let reply = session.submit(input("work"));
    gate.entered.notified().await;
    session.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(1), reply)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(outcome.cancelled);
    assert!(
        matches!(&session.info().await.events.last().unwrap().kind, EventKind::AssistantMessage { stop: StopReason::Cancelled, content, .. } if *content == input("partial"))
    );
}

#[tokio::test]
async fn steering_is_injected_after_the_complete_tool_batch() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![
        batch(vec![
            call("a", "fake", json!({})),
            call("b", "fake", json!({})),
        ]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(
        script.clone(),
        FakeTool {
            gate: Some(gate.clone()),
        },
        opts(),
    );
    let first = session.submit(input("initial"));
    gate.entered.notified().await;
    let steering = session.submit(input("steer"));
    gate.release.add_permits(2);
    let first = first.await.unwrap().unwrap();
    let steering = steering.await.unwrap().unwrap();
    assert_eq!(first.head, steering.head);
    let info = session.info().await;
    let user = info
        .events
        .iter()
        .position(
            |e| matches!(&e.kind, EventKind::UserMessage { content } if *content == input("steer")),
        )
        .unwrap();
    let last_result = info
        .events
        .iter()
        .rposition(|e| matches!(e.kind, EventKind::ToolResult { .. }))
        .unwrap();
    assert!(user > last_result);
    let requests = script.requests.lock().unwrap();
    assert!(
        matches!(&requests[1].messages[2], Message::ToolResults(results) if results.len() == 2)
    );
    assert_eq!(requests[1].messages[3], Message::User(input("steer")));
}

#[tokio::test]
async fn cancellation_keeps_queued_steering_for_the_next_turn() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![Step::Wait(gate.clone()), Step::Reply(done())]);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    let first = session.submit(input("initial"));
    gate.entered.notified().await;
    let mut steering = session.submit(input("keep me"));
    session.cancel();
    assert!(first.await.unwrap().unwrap().cancelled);
    assert!(matches!(
        steering.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(!session.info().await.events.iter().any(
        |e| matches!(&e.kind, EventKind::UserMessage { content } if *content == input("keep me"))
    ));
    let next = session.prompt(input("resume")).await.unwrap();
    assert_eq!(steering.await.unwrap().unwrap().head, next.head);
    let requests = script.requests.lock().unwrap();
    assert!(
        requests[1]
            .messages
            .contains(&Message::User(input("keep me")))
    );
    assert!(
        requests[1]
            .messages
            .contains(&Message::User(input("resume")))
    );
}

#[tokio::test]
async fn cancelled_tool_batch_records_all_results_before_next_user() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![batch(vec![
        call("a", "fake", json!({})),
        call("b", "fake", json!({})),
    ])]);
    let (session, _) = fixture.session(
        script,
        FakeTool {
            gate: Some(gate.clone()),
        },
        opts(),
    );
    let reply = session.submit(input("work"));
    gate.entered.notified().await;
    session.cancel();
    assert!(reply.await.unwrap().unwrap().cancelled);
    let info = session.info().await;
    assert_eq!(
        info.events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolResult { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn resume_latest_leaf_and_checkout_branch_without_rewriting() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done())]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    session.prompt(input("work")).await.unwrap();
    let info = session.info().await;
    session.checkout(info.events[1].id).await.unwrap();
    assert_eq!(session.info().await.head, info.events[1].id);
    assert_eq!(session.info().await.events, info.events);
    drop(session);
    let log = xi_session::SessionLog::open(&info.path).unwrap();
    let resumed =
        Session::from_log(log, fixture.config(), EventBus::default(), opts(), None).unwrap();
    assert_eq!(resumed.info().await.head, info.head);
}

#[tokio::test]
async fn selecting_the_current_model_records_nothing() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "default_model = 'local/m'\n[provider.local]\napi = 'openai-chat'\nbase_url = 'http://127.0.0.1:9/v1'\nauth = 'none'\n[[model]]\nprovider = 'local'\nid = 'm'\ncontext_window = 1000\nmax_output = 100\n[[model]]\nprovider = 'local'\nid = 'n'\ncontext_window = 1000\nmax_output = 100\n",
    );
    let (session, _) = fixture.session(Script::new(vec![]), FakeTool { gate: None }, opts());
    let before = session.info().await.events.len();
    session.set_model(fixture.config().model).await.unwrap();
    assert_eq!(session.info().await.events.len(), before);
    let other = ModelRef {
        provider: "local".into(),
        model: "n".into(),
    };
    session.set_model(other.clone()).await.unwrap();
    let events = session.info().await.events;
    assert_eq!(events.len(), before + 1);
    assert!(matches!(&events[before].kind, EventKind::ModelChanged { model } if *model == other));
}

#[tokio::test]
async fn reasoning_changes_record_once_and_reach_the_next_provider_request() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done())]);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    session.set_reasoning(Reasoning::Medium).await.unwrap();
    assert_eq!(session.info().await.events.len(), 1);
    session.set_reasoning(Reasoning::High).await.unwrap();
    session.set_reasoning(Reasoning::High).await.unwrap();
    let info = session.info().await;
    assert_eq!(info.events.len(), 2);
    assert!(matches!(
        info.events[1].kind,
        EventKind::ReasoningChanged {
            reasoning: Reasoning::High
        }
    ));
    session.prompt(input("work")).await.unwrap();
    assert_eq!(
        script.requests.lock().unwrap()[0].reasoning,
        Reasoning::High
    );
}

#[tokio::test]
async fn requests_run_at_a_listed_level_and_unlisted_choices_are_refused() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done()), Step::Reply(done())]);
    let mut config = fixture.config();
    config.reasoning = Reasoning::Off;
    let session = Session::create(
        config,
        None,
        EventBus::default(),
        opts(),
        Some(Backend {
            provider: script.clone(),
            model: ModelInfo {
                reasoning: vec![Reasoning::Low, Reasoning::High, Reasoning::Max],
                ..model()
            },
            tools: Vec::new(),
        }),
    )
    .unwrap();
    // A default the model can't run still reaches it as its nearest level.
    session.prompt(input("default")).await.unwrap();
    let error = session.set_reasoning(Reasoning::XHigh).await.unwrap_err();
    assert!(matches!(error, xi_kernel::Error::Config(_)));
    assert!(error.to_string().contains("accepts low, high, max"));
    session.set_reasoning(Reasoning::Max).await.unwrap();
    session.prompt(input("max")).await.unwrap();
    let sent: Vec<_> = script
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.reasoning)
        .collect();
    assert_eq!(sent, [Reasoning::Low, Reasoning::Max]);
}

#[tokio::test]
async fn reasoning_changes_are_refused_while_a_turn_runs() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![Step::Wait(gate.clone())]);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    let prompt = session.submit(input("work"));
    gate.entered.notified().await;
    assert!(matches!(
        session.set_reasoning(Reasoning::Off).await,
        Err(xi_kernel::Error::Running)
    ));
    session.cancel();
    prompt.await.unwrap().unwrap();
    assert_eq!(
        script.requests.lock().unwrap()[0].reasoning,
        Reasoning::Medium
    );
    assert!(
        !session
            .info()
            .await
            .events
            .iter()
            .any(|event| { matches!(event.kind, EventKind::ReasoningChanged { .. }) })
    );
}

#[tokio::test]
async fn reasoning_reopens_from_the_head_lineage_and_explicit_override_wins() {
    let fixture = Fixture::new();
    let (session, _) = fixture.session(Script::new(vec![]), FakeTool { gate: None }, opts());
    let root = session.info().await.head;
    session.set_reasoning(Reasoning::High).await.unwrap();
    session.checkout(root).await.unwrap();
    session.set_reasoning(Reasoning::Low).await.unwrap();
    let path = session.info().await.path;
    drop(session);

    let reopen = |config: Config, script: Arc<Script>| {
        Session::from_log(
            xi_session::SessionLog::open(&path).unwrap(),
            config,
            EventBus::default(),
            opts(),
            Some(Backend {
                provider: script,
                model: model(),
                tools: Vec::new(),
            }),
        )
        .unwrap()
    };
    let mut config = fixture.config();
    config.reasoning = Reasoning::High;
    let script = Script::new(vec![Step::Reply(done()), Step::Reply(done())]);
    let session = reopen(config, script.clone());
    session.prompt(input("restored")).await.unwrap();
    assert_eq!(script.requests.lock().unwrap()[0].reasoning, Reasoning::Low);
    session.checkout(root).await.unwrap();
    session
        .prompt(input("default on original root"))
        .await
        .unwrap();
    assert_eq!(
        script.requests.lock().unwrap()[1].reasoning,
        Reasoning::High
    );
    session.set_reasoning(Reasoning::Low).await.unwrap();
    drop(session);

    let mut config = fixture.config();
    config.reasoning = Reasoning::Off;
    config.reasoning_override = Some(Reasoning::Off);
    let script = Script::new(vec![Step::Reply(done())]);
    let session = reopen(config, script.clone());
    session.prompt(input("explicit override")).await.unwrap();
    assert_eq!(script.requests.lock().unwrap()[0].reasoning, Reasoning::Off);
}

#[tokio::test(start_paused = true)]
async fn retry_limit_backoff_and_retry_after_cap() {
    let fixture = Fixture::new();
    let failures = (0..5)
        .map(|_| Step::Fail(Error::new(ErrorKind::RateLimited, "busy")))
        .collect();
    let script = Script::new(failures);
    let (session, _) = fixture.session(script.clone(), FakeTool { gate: None }, opts());
    let start = tokio::time::Instant::now();
    assert!(session.prompt(input("work")).await.unwrap().failed);
    assert_eq!(script.requests.lock().unwrap().len(), 5);
    assert_eq!(start.elapsed(), Duration::from_secs(15));

    let error = Error {
        kind: ErrorKind::RateLimited,
        message: "retry later".into(),
        retry_after: Some(Duration::from_secs(1000)),
    };
    let script = Script::new(vec![Step::Fail(error), Step::Reply(done())]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    let start = tokio::time::Instant::now();
    assert!(!session.prompt(input("work")).await.unwrap().failed);
    assert_eq!(start.elapsed(), Duration::from_secs(60));
}

#[tokio::test]
async fn cached_instructions_ignore_mid_session_file_changes() {
    let fixture = Fixture::new();
    write(
        &fixture.workspace.join("AGENTS.md"),
        "original instructions",
    );
    let gate = Gate::new();
    let script = Script::new(vec![
        batch(vec![call("a", "fake", json!({}))]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(
        script.clone(),
        FakeTool {
            gate: Some(gate.clone()),
        },
        opts(),
    );
    let reply = session.submit(input("work"));
    gate.entered.notified().await;
    write(&fixture.workspace.join("AGENTS.md"), "changed instructions");
    gate.release.add_permits(1);
    reply.await.unwrap().unwrap();
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests[0].system, requests[1].system);
    assert!(requests[1].system.contains("original instructions"));
    assert!(!requests[1].system.contains("changed instructions"));
}

#[tokio::test]
async fn checkout_removes_taint_on_a_sibling_branch() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "[[rule]]\naction='net'\npattern='**'\neffect='allow'\n",
    );
    let script = Script::new(vec![
        batch(vec![call("dirty", "fake", json!({"untrusted":true}))]),
        Step::Reply(done()),
        batch(vec![call("clean", "fake", json!({"net":true}))]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    session.prompt(input("dirty branch")).await.unwrap();
    let info = session.info().await;
    session.checkout(info.events[0].id).await.unwrap();
    session.prompt(input("clean branch")).await.unwrap();
    let info = session.info().await;
    assert!(info.events.iter().any(|event| matches!(&event.kind, EventKind::ToolDecision { call, decision: Decision::Allow, .. } if call.0 == "clean")));
}

#[tokio::test]
async fn denied_tool_names_the_deciding_rule() {
    let fixture = Fixture::new();
    write(
        &fixture.workspace.join(".xi/config.toml"),
        "[[rule]]\naction='exec'\npattern='test command'\neffect='deny'\n",
    );
    let script = Script::new(vec![
        batch(vec![call("deny", "fake", json!({}))]),
        Step::Reply(done()),
    ]);
    let (session, _) = fixture.session(script, FakeTool { gate: None }, opts());
    session.prompt(input("work")).await.unwrap();
    let info = session.info().await;
    let expected = format!("{}#0", fixture.workspace.join(".xi/config.toml").display());
    assert!(info.events.iter().any(|event| matches!(&event.kind, EventKind::ToolDecision { decision: Decision::Deny, rule: Some(rule), .. } if rule == &expected)));
    assert!(info.events.iter().any(|event| matches!(&event.kind, EventKind::ToolResult { output, .. } if output.content.iter().any(|block| matches!(block, OutputBlock::Text { text } if text.contains("denied by policy") && text.contains(&expected))))));
}

fn assistant(content: Vec<Block>, usage: Usage) -> EventKind {
    EventKind::AssistantMessage {
        model: ModelRef {
            provider: "test".into(),
            model: "test".into(),
        },
        content,
        stop: StopReason::EndTurn,
        usage,
    }
}

fn seeded_session(
    fixture: &Fixture,
    script: Arc<Script>,
    events: Vec<EventKind>,
    compaction: xi_kernel::compaction::Config,
) -> (Session, tokio::sync::mpsc::UnboundedReceiver<Notification>) {
    seeded_session_with_options(fixture, script, events, compaction, opts())
}

fn seeded_session_with_options(
    fixture: &Fixture,
    script: Arc<Script>,
    events: Vec<EventKind>,
    compaction: xi_kernel::compaction::Config,
    options: TurnOptions,
) -> (Session, tokio::sync::mpsc::UnboundedReceiver<Notification>) {
    let mut log = xi_session::SessionLog::create(
        fixture.workspace.join("seed.jsonl"),
        EventKind::SessionStarted {
            session: xi_protocol::SessionId::generate(),
            cwd: fixture.workspace.clone(),
            model: ModelRef {
                provider: "test".into(),
                model: "test".into(),
            },
            xi_version: "test".into(),
            parent_session: None,
        },
    )
    .unwrap();
    let mut head = log.root().id;
    for event in events {
        head = log.append(head, event).unwrap().id;
    }
    let mut config = fixture.config();
    config.compaction = compaction;
    let bus = EventBus::default();
    let rx = bus.subscribe();
    let session = Session::from_log(
        log,
        config,
        bus,
        options,
        Some(Backend {
            provider: script,
            model: model(),
            tools: vec![Arc::new(FakeTool { gate: None })],
        }),
    )
    .unwrap();
    (session, rx)
}

#[test]
fn compaction_defaults_layering_and_keep_clamp() {
    let fixture = Fixture::new();
    let config = fixture.config().compaction;
    assert!(!config.enabled);
    assert_eq!(config.trigger_tokens, None);
    assert_eq!(config.trigger(10_000), 8_000);
    assert_eq!(config.keep_recent_tokens, 20_000);
    assert_eq!(config.keep(10_000), 4_000);
    write(
        &fixture.paths.config.join("config.toml"),
        "[compaction]\nenabled=true\ntrigger_tokens=24000\nkeep_recent_tokens=20000\n",
    );
    let extra = fixture.workspace.join("extra.toml");
    write(&extra, "[compaction]\ntrigger_tokens=8000\n");
    let config = Config::load(fixture.paths.clone(), &fixture.workspace, &[extra])
        .unwrap()
        .compaction;
    assert!(config.enabled);
    assert_eq!(config.keep_recent_tokens, 20_000);
    assert_eq!(config.keep(100_000), 4_000);
    write(
        &fixture.workspace.join(".xi/config.toml"),
        "[compaction]\nenabled=false\nkeep_recent_tokens=3\n",
    );
    assert!(
        fixture.config().compaction.enabled,
        "untrusted layer ignored"
    );
    fixture.paths.trust(&fixture.workspace).unwrap();
    assert!(!fixture.config().compaction.enabled);
    assert_eq!(fixture.config().compaction.keep_recent_tokens, 3);
}

#[tokio::test]
async fn compaction_triggers_at_threshold_using_all_usage_fields_and_new_characters() {
    for output in [98, 99] {
        let fixture = Fixture::new();
        let triggered = output == 99;
        let script = Script::new(if triggered {
            vec![Step::Reply(done()), Step::Reply(done())]
        } else {
            vec![Step::Reply(done())]
        });
        let (session, mut rx) = seeded_session(
            &fixture,
            script.clone(),
            vec![
                EventKind::UserMessage {
                    content: input(&"old ".repeat(100)),
                },
                assistant(
                    input("old answer"),
                    Usage {
                        input: 600,
                        cache_read: 200,
                        cache_write: 100,
                        output,
                    },
                ),
            ],
            xi_kernel::compaction::Config {
                enabled: true,
                trigger_tokens: Some(1_000),
                keep_recent_tokens: 2,
            },
        );
        assert!(!session.prompt(input("next")).await.unwrap().failed);
        let info = session.info().await;
        let compactions: Vec<_> = info
            .events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Compaction { .. }))
            .collect();
        assert_eq!(compactions.len(), usize::from(triggered));
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), if triggered { 2 } else { 1 });
        if triggered {
            assert!(matches!(
                compactions[0].kind,
                EventKind::Compaction {
                    tokens_before: 1_000,
                    kept: Some(kept),
                    usage: Some(_),
                    ..
                } if kept == info.events[3].id
            ));
            assert!(requests[0].tools.is_empty());
            assert_eq!(requests[0].system, requests[1].system);
            assert_eq!(requests[1].messages[0], Message::User(input("done")));
            assert_eq!(requests[1].messages[1], Message::User(input("next")));
            assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(
                |event| matches!(event, Notification::Event(params) if params.event == *compactions[0])
            ));
        }
    }
}

#[tokio::test]
async fn compaction_keeps_an_oversized_tool_batch_whole() {
    let fixture = Fixture::new();
    let summary = Response {
        content: vec![Block::Text {
            text: "model summary".into(),
        }],
        ..done()
    };
    let script = Script::new(vec![Step::Reply(summary), Step::Reply(done())]);
    let result = |id: &str| EventKind::ToolResult {
        call: id.into(),
        output: ToolOutput {
            content: vec![OutputBlock::Text {
                text: "result ".repeat(80),
            }],
            is_error: false,
            untrusted: false,
        },
        spill: None,
    };
    let (session, _) = seeded_session(
        &fixture,
        script.clone(),
        vec![
            EventKind::UserMessage {
                content: input("older work"),
            },
            assistant(
                vec![call("a", "fake", json!({})), call("b", "fake", json!({}))],
                Usage::default(),
            ),
            result("a"),
            EventKind::Tainted {
                call: None,
                reason: "audit".into(),
            },
            result("b"),
        ],
        xi_kernel::compaction::Config {
            keep_recent_tokens: 1,
            ..Default::default()
        },
    );
    let before = session.info().await;
    let compact = session.compact().await.unwrap();
    let info = session.info().await;
    assert_eq!(compact.head, info.head);
    assert!(matches!(
        info.events.last().unwrap().kind,
        EventKind::Compaction {
            kept: Some(kept),
            usage: Some(_),
            ..
        } if kept == before.events[2].id
    ));
    session.prompt(input("next")).await.unwrap();
    let requests = script.requests.lock().unwrap();
    assert!(requests[0].tools.is_empty());
    let messages = &requests[1].messages;
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], Message::User(input("model summary")));
    let Message::ToolResults(results) = &messages[2] else {
        panic!("missing retained results")
    };
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.output.is_error));
}

#[tokio::test]
async fn summary_includes_previous_summary_and_resume_rebuilds_identical_context() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done()), Step::Reply(done())]);
    let (session, _) = seeded_session(
        &fixture,
        script.clone(),
        vec![
            EventKind::UserMessage {
                content: input("ancient text"),
            },
            EventKind::Compaction {
                summary: "previous summary".into(),
                tokens_before: 800,
                kept: None,
                usage: None,
            },
            EventKind::UserMessage {
                content: input("work since previous summary"),
            },
            assistant(input("tail"), Usage::default()),
        ],
        xi_kernel::compaction::Config {
            keep_recent_tokens: 1,
            ..Default::default()
        },
    );
    session.compact().await.unwrap();
    let compacted = session.info().await;
    assert!(!session.prompt(input("continue")).await.unwrap().failed);
    let resumed_at = session.info().await;
    drop(session);
    let expected = {
        let script_requests = script.requests.lock().unwrap();
        assert_eq!(script_requests.len(), 2);
        let summary = &script_requests[0];
        assert!(summary.tools.is_empty());
        assert_eq!(
            summary.messages[0],
            Message::User(input("previous summary"))
        );
        assert_eq!(
            summary.messages[1],
            Message::User(input("work since previous summary"))
        );
        assert_eq!(summary.messages.len(), 3, "tail excluded, prompt appended");
        script_requests[1].clone()
    };
    let log = xi_session::SessionLog::open(resumed_at.path).unwrap();
    let resume_script = Script::new(vec![Step::Reply(done())]);
    let resumed = Session::from_log(
        log,
        fixture.config(),
        EventBus::default(),
        opts(),
        Some(Backend {
            provider: resume_script.clone(),
            model: model(),
            tools: vec![Arc::new(FakeTool { gate: None })],
        }),
    )
    .unwrap();
    resumed.checkout(compacted.head).await.unwrap();
    resumed.prompt(input("continue")).await.unwrap();
    assert_eq!(resume_script.requests.lock().unwrap()[0], expected);
}

#[tokio::test]
async fn overflow_compacts_and_retries_once_even_when_disabled() {
    for (enabled, second_overflow) in [(true, false), (false, false), (false, true)] {
        let fixture = Fixture::new();
        let overflow = || Step::Fail(Error::new(ErrorKind::ContextOverflow, "context full"));
        let steps = vec![
            overflow(),
            Step::Reply(done()),
            if second_overflow {
                overflow()
            } else {
                Step::Reply(done())
            },
        ];
        let script = Script::new(steps);
        let (session, _) = seeded_session(
            &fixture,
            script.clone(),
            vec![EventKind::UserMessage {
                content: input("older context"),
            }],
            xi_kernel::compaction::Config {
                enabled,
                trigger_tokens: Some(100_000),
                keep_recent_tokens: 1,
            },
        );
        let outcome = session.prompt(input("next")).await.unwrap();
        assert_eq!(outcome.failed, second_overflow);
        let info = session.info().await;
        let count = info
            .events
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Compaction { .. }))
            .count();
        assert_eq!(count, 1);
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].tools.is_empty());
        assert!(!requests[2].tools.is_empty());
        assert_eq!(requests[2].messages[0], Message::User(input("done")));
    }
}

#[tokio::test]
async fn manual_compaction_accepts_repaired_interrupted_calls() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done())]);
    let (session, _) = seeded_session(
        &fixture,
        script,
        vec![
            EventKind::UserMessage {
                content: input("old context"),
            },
            assistant(
                vec![call("unfinished", "fake", json!({}))],
                Usage::default(),
            ),
        ],
        xi_kernel::compaction::Config {
            keep_recent_tokens: 0,
            ..Default::default()
        },
    );
    let before = session.info().await;
    assert!(matches!(
        &before.events.last().unwrap().kind,
        EventKind::ToolResult { call, output, .. }
            if call.0 == "unfinished"
                && output.is_error
                && output.content == vec![OutputBlock::Text { text: "interrupted".into() }]
    ));
    session.compact().await.unwrap();
    let after = session.info().await;
    assert_eq!(after.events.len(), before.events.len() + 1);
    assert!(matches!(
        after.events.last().unwrap().kind,
        EventKind::Compaction { .. }
    ));
}

#[tokio::test]
async fn cancelled_summary_preserves_context_and_does_not_emit_partial_assistant() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![Step::Wait(gate.clone())]);
    let (session, mut rx) = seeded_session(
        &fixture,
        script,
        vec![EventKind::UserMessage {
            content: input("old context"),
        }],
        xi_kernel::compaction::Config {
            keep_recent_tokens: 0,
            ..Default::default()
        },
    );
    let before = session.info().await;
    let task = {
        let session = session.clone();
        tokio::spawn(async move { session.compact().await })
    };
    gate.entered.notified().await;
    session.cancel();
    assert!(task.await.unwrap().is_err());
    assert_eq!(session.info().await.events, before.events);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn compaction_runs_after_tool_results_and_resets_old_usage_estimates() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        Step::Reply(Response {
            content: vec![call("a", "fake", json!({}))],
            stop: StopReason::ToolUse,
            usage: Usage {
                input: 1_000,
                ..Usage::default()
            },
        }),
        Step::Reply(done()),
        batch(vec![call("b", "fake", json!({}))]),
        Step::Reply(done()),
    ]);
    let (session, _) = seeded_session(
        &fixture,
        script.clone(),
        vec![EventKind::UserMessage {
            content: input(&"old ".repeat(100)),
        }],
        xi_kernel::compaction::Config {
            enabled: true,
            trigger_tokens: Some(1_000),
            keep_recent_tokens: 10,
        },
    );
    let outcome = session.prompt(input("work")).await.unwrap();
    assert!(!outcome.failed);
    let info = session.info().await;
    let compactions: Vec<_> = info
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.kind, EventKind::Compaction { .. }))
        .collect();
    assert_eq!(compactions.len(), 1);
    assert!(matches!(
        &info.events[compactions[0].0 - 1].kind,
        EventKind::ToolResult { call, .. } if call.0 == "a"
    ));
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].tools.is_empty());
    assert_eq!(requests[2].messages[0], Message::User(input("done")));
    assert_eq!(requests[3].messages[0], Message::User(input("done")));
    assert_eq!(requests[0].system, requests[3].system);
}

#[tokio::test]
async fn compaction_without_an_older_prefix_is_a_noop() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step::Reply(done())]);
    let (session, _) = seeded_session(
        &fixture,
        script.clone(),
        vec![],
        xi_kernel::compaction::Config {
            enabled: true,
            trigger_tokens: Some(1),
            keep_recent_tokens: 20_000,
        },
    );
    assert!(session.compact().await.is_err());
    assert!(script.requests.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_resets_partial_output_and_numbers_each_attempt() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        Step::DeltaFail(Error::new(ErrorKind::Unavailable, "retry")),
        Step::Reply(done()),
    ]);
    let (session, mut rx) = fixture.session(script, FakeTool { gate: None }, opts());
    assert_eq!(session.prompt(input("work")).await.unwrap().text, "done");
    let stream: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|notification| {
            matches!(
                notification,
                Notification::Delta(_) | Notification::Reset(_)
            )
        })
        .collect();
    assert_eq!(stream.len(), 3);
    assert!(matches!(
        &stream[0],
        Notification::Delta(params)
            if params.attempt == 0
                && params.delta == (Delta::Text { text: "discarded".into() })
    ));
    assert!(matches!(
        &stream[1],
        Notification::Reset(params) if params.session == session.id() && params.attempt == 1
    ));
    assert_eq!(stream[1].json()["method"], "stream/reset");
    assert!(matches!(
        &stream[2],
        Notification::Delta(params)
            if params.attempt == 1
                && params.delta == (Delta::Text { text: "done".into() })
    ));
    let info = session.info().await;
    assert!(matches!(
        &info.events.last().unwrap().kind,
        EventKind::AssistantMessage { content, .. } if *content == input("done")
    ));
}

#[tokio::test]
async fn cancelled_ask_is_resolved_before_its_cancelled_result() {
    let fixture = Fixture::new();
    let script = Script::new(vec![batch(vec![call("ask", "fake", json!({"net": true}))])]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("ask"));
    let call = wait_for_ask(&mut rx).await;
    session.cancel();
    assert!(reply.await.unwrap().unwrap().cancelled);
    assert!(matches!(
        session.approve(&call, true, Remember::Once),
        Err(xi_kernel::Error::NotPending)
    ));
    let info = session.info().await;
    let resolved = info
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::ApprovalResolved {
                    call: actual,
                    approved: false,
                    by: Approver::Cancelled,
                    remember: Remember::Once,
                } if actual == &call
            )
        })
        .unwrap();
    assert!(matches!(
        &info.events[resolved + 1].kind,
        EventKind::ToolResult { call: actual, output, .. }
            if actual == &call
                && output.is_error
                && output.content == vec![OutputBlock::Text { text: "cancelled".into() }]
    ));
}

#[tokio::test(start_paused = true)]
async fn client_approval_timeout_records_a_timeout_denial() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "[daemon]\napproval_timeout = 1\n",
    );
    let script = Script::new(vec![
        batch(vec![call("ask", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("ask"));
    let call = wait_for_ask(&mut rx).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!reply.await.unwrap().unwrap().failed);
    assert!(matches!(
        session.approve(&call, true, Remember::Once),
        Err(xi_kernel::Error::NotPending)
    ));
    let info = session.info().await;
    let resolved = info
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::ApprovalResolved {
                    call: actual,
                    approved: false,
                    by: Approver::Timeout,
                    remember: Remember::Once,
                } if actual == &call
            )
        })
        .unwrap();
    assert!(matches!(
        &info.events[resolved + 1].kind,
        EventKind::ToolResult { call: actual, output, .. }
            if actual == &call && output.is_error
    ));
}

#[tokio::test]
async fn reopening_repairs_unresolved_asks_and_every_unfinished_call() {
    let fixture = Fixture::new();
    let path = fixture.workspace.join("interrupted.jsonl");
    let mut log = xi_session::SessionLog::create(
        &path,
        EventKind::SessionStarted {
            session: xi_protocol::SessionId::generate(),
            cwd: fixture.workspace.clone(),
            model: fixture.config().model,
            xi_version: "test".into(),
            parent_session: None,
        },
    )
    .unwrap();
    let mut head = log.root().id;
    for kind in [
        EventKind::UserMessage {
            content: input("work"),
        },
        assistant(
            vec![
                call("asked", "fake", json!({})),
                call("resolved", "fake", json!({})),
                call("undecided", "fake", json!({})),
                call("finished", "fake", json!({})),
            ],
            Usage::default(),
        ),
        EventKind::ToolDecision {
            call: "asked".into(),
            action: "net example.org".into(),
            decision: Decision::Ask,
            rule: None,
            taint: false,
        },
        EventKind::ToolDecision {
            call: "resolved".into(),
            action: "net example.org".into(),
            decision: Decision::Ask,
            rule: None,
            taint: false,
        },
        EventKind::ApprovalResolved {
            call: "resolved".into(),
            approved: true,
            by: Approver::User,
            remember: Remember::Once,
        },
        EventKind::ToolResult {
            call: "finished".into(),
            output: ToolOutput {
                content: Vec::new(),
                is_error: false,
                untrusted: false,
            },
            spill: None,
        },
    ] {
        head = log.append(head, kind).unwrap().id;
    }
    let original = log.events().to_vec();
    drop(log);
    let bus = EventBus::default();
    let mut rx = bus.subscribe();
    let session = Session::from_log(
        xi_session::SessionLog::open(&path).unwrap(),
        fixture.config(),
        bus,
        opts(),
        None,
    )
    .unwrap();
    let info = session.info().await;
    assert_eq!(&info.events[..original.len()], &original);
    let repaired = &info.events[original.len()..];
    assert_eq!(repaired.len(), 4);
    assert!(matches!(
        &repaired[0].kind,
        EventKind::ApprovalResolved {
            call,
            approved: false,
            by: Approver::Interrupted,
            remember: Remember::Once,
        } if call.0 == "asked"
    ));
    for (event, expected) in repaired[1..].iter().zip(["asked", "resolved", "undecided"]) {
        assert!(matches!(
            &event.kind,
            EventKind::ToolResult { call, output, .. }
                if call.0 == expected
                    && output.is_error
                    && output.content == vec![OutputBlock::Text { text: "interrupted".into() }]
        ));
    }
    let broadcast: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|notification| match notification {
            Notification::Event(params) => params.event,
            _ => panic!("unexpected ephemeral notification"),
        })
        .collect();
    assert_eq!(broadcast, repaired);
    drop(session);
    let resumed = Session::from_log(
        xi_session::SessionLog::open(&path).unwrap(),
        fixture.config(),
        EventBus::default(),
        opts(),
        None,
    )
    .unwrap();
    assert_eq!(resumed.info().await.events, info.events);
}

#[tokio::test(start_paused = true)]
async fn model_leases_cover_retries_and_end_before_the_tool_batch() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "default_model = 'test/test'\n[provider.test]\napi = 'openai-chat'\nbase_url = 'http://127.0.0.1:9876/v1'\n",
    );
    let gate = Gate::new();
    let script = Script::new(vec![
        Step::Fail(Error::new(ErrorKind::Unavailable, "retry")),
        batch(vec![call("one", "fake", json!({}))]),
        Step::Wait(gate.clone()),
    ]);
    let spans = Arc::new(Mutex::new(Vec::new()));
    let (session, _) = fixture.session(
        script.clone(),
        FakeTool { gate: None },
        TurnOptions {
            leaser: Some(Arc::new(TestLeaser {
                script: script.clone(),
                spans: spans.clone(),
            })),
            ..opts()
        },
    );
    let reply = session.submit(input("work"));
    gate.entered.notified().await;
    {
        let spans = spans.lock().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].endpoint, "http://127.0.0.1:9876/v1");
        assert_eq!(spans[0].acquired_after, 0);
        assert_eq!(spans[0].released_after, Some(2));
        assert_eq!(spans[1].acquired_after, 2);
        assert_eq!(spans[1].released_after, None);
    }
    gate.release.add_permits(1);
    assert!(!reply.await.unwrap().unwrap().failed);
    assert_eq!(spans.lock().unwrap()[1].released_after, Some(3));
}

#[tokio::test(start_paused = true)]
async fn summary_requests_hold_one_lease_across_retries_without_streaming() {
    let fixture = Fixture::new();
    write(
        &fixture.paths.config.join("config.toml"),
        "[provider.test]\napi = 'openai-chat'\nbase_url = 'http://127.0.0.1:9876/v1'\n",
    );
    let script = Script::new(vec![
        Step::DeltaFail(Error::new(ErrorKind::Unavailable, "retry summary")),
        Step::Reply(done()),
    ]);
    let spans = Arc::new(Mutex::new(Vec::new()));
    let (session, mut rx) = seeded_session_with_options(
        &fixture,
        script.clone(),
        vec![EventKind::UserMessage {
            content: input("old context"),
        }],
        xi_kernel::compaction::Config {
            keep_recent_tokens: 0,
            ..Default::default()
        },
        TurnOptions {
            leaser: Some(Arc::new(TestLeaser {
                script: script.clone(),
                spans: spans.clone(),
            })),
            ..opts()
        },
    );
    session.compact().await.unwrap();
    let spans = spans.lock().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].endpoint, "http://127.0.0.1:9876/v1");
    assert_eq!(spans[0].acquired_after, 0);
    assert_eq!(spans[0].released_after, Some(2));
    assert!(
        std::iter::from_fn(|| rx.try_recv().ok())
            .all(|notification| { matches!(notification, Notification::Event(_)) })
    );
}

#[tokio::test]
async fn stopping_requests_finishes_the_tool_batch_then_records_daemon_loss() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![batch(vec![
        call("one", "fake", json!({})),
        call("two", "fake", json!({})),
    ])]);
    let (session, _) = fixture.session(
        script.clone(),
        FakeTool {
            gate: Some(gate.clone()),
        },
        opts(),
    );
    assert!(!session.is_running());
    let reply = session.submit(input("work"));
    assert!(session.is_running());
    gate.entered.notified().await;
    let queued = session.submit(input("queued"));
    session.stop_requests();
    session.stop_requests();
    assert_eq!(queued.await.unwrap().unwrap_err(), "daemon connection lost");
    assert_eq!(
        session
            .submit(input("too late"))
            .await
            .unwrap()
            .unwrap_err(),
        "daemon connection lost"
    );
    let idle = {
        let session = session.clone();
        tokio::spawn(async move { session.wait_idle().await })
    };
    tokio::task::yield_now().await;
    assert!(session.is_running());
    assert!(!idle.is_finished());
    gate.release.add_permits(1);
    gate.entered.notified().await;
    assert!(!idle.is_finished());
    gate.release.add_permits(1);
    let outcome = reply.await.unwrap().unwrap();
    assert!(outcome.failed);
    assert!(!outcome.cancelled);
    idle.await.unwrap();
    assert!(!session.is_running());
    assert_eq!(script.requests.lock().unwrap().len(), 1);
    let info = session.info().await;
    let results: Vec<_> = info
        .events
        .iter()
        .filter_map(|event| {
            if let EventKind::ToolResult { call, output, .. } = &event.kind {
                Some((call.0.as_str(), output.is_error))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results, [("one", false), ("two", false)]);
    assert!(matches!(
        &info.events.last().unwrap().kind,
        EventKind::TurnFailed { message } if message == "daemon connection lost"
    ));
    assert_eq!(
        info.events
            .iter()
            .filter(|event| { matches!(event.kind, EventKind::TurnFailed { .. }) })
            .count(),
        1
    );
}

#[tokio::test]
async fn stopping_requests_aborts_an_inflight_model_and_releases_its_lease() {
    let fixture = Fixture::new();
    let gate = Gate::new();
    let script = Script::new(vec![Step::Wait(gate.clone())]);
    let spans = Arc::new(Mutex::new(Vec::new()));
    let (session, _) = fixture.session(
        script.clone(),
        FakeTool { gate: None },
        TurnOptions {
            leaser: Some(Arc::new(TestLeaser {
                script,
                spans: spans.clone(),
            })),
            ..opts()
        },
    );
    let reply = session.submit(input("work"));
    gate.entered.notified().await;
    session.stop_requests();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .failed
    );
    session.wait_idle().await;
    assert_eq!(spans.lock().unwrap()[0].released_after, Some(1));
    assert!(matches!(
        &session.info().await.events.last().unwrap().kind,
        EventKind::TurnFailed { message } if message == "daemon connection lost"
    ));
}

#[tokio::test]
async fn session_grant_skips_the_same_action_and_survives_reopen() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![call("first", "fake", json!({"net": true}))]),
        Step::Reply(done()),
        batch(vec![call("second", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("first ask"));
    let asked = wait_for_ask(&mut rx).await;
    session.approve(&asked, true, Remember::Session).unwrap();
    reply.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), session.prompt(input("same action")))
        .await
        .unwrap()
        .unwrap();
    let info = session.info().await;
    assert!(info.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolDecision { call, decision: Decision::Allow, rule: Some(rule), taint: false, .. }
            if call.0 == "second" && rule == "session"
    )));
    assert!(info.events.iter().any(|event| matches!(
        event.kind,
        EventKind::ApprovalResolved {
            approved: true,
            remember: Remember::Session,
            ..
        }
    )));
    session.wait_idle().await;
    drop(session);
    let script = Script::new(vec![
        batch(vec![call("reopened", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let resumed = Session::from_log(
        xi_session::SessionLog::open(&info.path).unwrap(),
        fixture.config(),
        EventBus::default(),
        TurnOptions::default(),
        Some(Backend {
            provider: script,
            model: model(),
            tools: vec![Arc::new(FakeTool { gate: None })],
        }),
    )
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        resumed.prompt(input("after reopen")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(resumed.info().await.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolDecision { call, decision: Decision::Allow, rule: Some(rule), .. }
            if call.0 == "reopened" && rule == "session"
    )));
}

#[tokio::test]
async fn session_grants_do_not_cross_a_checkout_branch() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![call("first", "fake", json!({"net": true}))]),
        Step::Reply(done()),
        batch(vec![call("sibling", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let root = session.info().await.events[0].id;
    let reply = session.submit(input("grant branch"));
    let call = wait_for_ask(&mut rx).await;
    session.approve(&call, true, Remember::Session).unwrap();
    reply.await.unwrap().unwrap();
    session.checkout(root).await.unwrap();
    let reply = session.submit(input("sibling branch"));
    let call = wait_for_ask(&mut rx).await;
    assert_eq!(call.0, "sibling");
    session.approve(&call, false, Remember::Once).unwrap();
    reply.await.unwrap().unwrap();
}

#[tokio::test]
async fn remembered_scopes_reject_taint_asks_without_consuming_them() {
    let fixture = Fixture::new();
    fixture.paths.trust(&fixture.workspace).unwrap();
    write(
        &fixture.paths.config.join("config.toml"),
        "[[rule]]\naction='net'\npattern='**'\neffect='allow'\n",
    );
    let script = Script::new(vec![
        batch(vec![
            call("ingest", "fake", json!({"untrusted": true})),
            call("taint-ask", "fake", json!({"net": true})),
        ]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("tainted branch"));
    let call = wait_for_ask(&mut rx).await;
    for remember in [Remember::Session, Remember::Workspace] {
        assert!(matches!(
            session.approve(&call, true, remember),
            Err(xi_kernel::Error::InvalidRemember(_))
        ));
    }
    session.approve(&call, true, Remember::Once).unwrap();
    reply.await.unwrap().unwrap();
    assert!(session.info().await.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolDecision { call, decision: Decision::Ask, taint: true, .. }
            if call.0 == "taint-ask"
    )));
}

#[tokio::test]
async fn remembered_denials_and_untrusted_workspace_scopes_leave_the_ask_pending() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![call("ask", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("ask"));
    let call = wait_for_ask(&mut rx).await;
    for (approved, remember) in [
        (false, Remember::Session),
        (false, Remember::Workspace),
        (true, Remember::Workspace),
    ] {
        assert!(matches!(
            session.approve(&call, approved, remember),
            Err(xi_kernel::Error::InvalidRemember(_))
        ));
    }
    assert!(!fixture.workspace.join(".xi/config.local.toml").exists());
    session.approve(&call, false, Remember::Once).unwrap();
    reply.await.unwrap().unwrap();
    assert!(session.info().await.events.iter().any(|event| matches!(
        event.kind,
        EventKind::ApprovalResolved {
            approved: false,
            remember: Remember::Once,
            ..
        }
    )));
}

#[tokio::test]
async fn workspace_grant_appends_an_exact_rule_and_fresh_sessions_use_policy() {
    let fixture = Fixture::new();
    fixture.paths.trust(&fixture.workspace).unwrap();
    let local = fixture.workspace.join(".xi/config.local.toml");
    let original = "default_model = 'test/test' # keep this comment";
    write(&local, original);
    let subject = "host[1]*?\"\\\n.example";
    let script = Script::new(vec![
        batch(vec![call("workspace", "fake", json!({"net": subject}))]),
        Step::Reply(done()),
        batch(vec![call("same-session", "fake", json!({"net": subject}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("remember here"));
    let asked = wait_for_ask(&mut rx).await;
    session.approve(&asked, true, Remember::Workspace).unwrap();
    reply.await.unwrap().unwrap();
    let text = std::fs::read_to_string(&local).unwrap();
    assert!(text.starts_with(&format!("{original}\n")));
    let value: toml::Value = toml::from_str(&text).unwrap();
    let rule = &value["rule"][0];
    assert_eq!(rule["effect"].as_str(), Some("allow"));
    assert_eq!(rule["action"].as_str(), Some("net"));
    assert_eq!(
        rule["pattern"].as_str(),
        Some(globset::escape(subject).replace('\\', "\\\\").as_str())
    );
    assert_eq!(rule["when"].as_str(), Some("always"));
    tokio::time::timeout(
        Duration::from_secs(2),
        session.prompt(input("same session")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(session.info().await.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolDecision { call, decision: Decision::Allow, rule: Some(rule), .. }
            if call.0 == "same-session" && rule == "session"
    )));
    let config = fixture.config();
    assert_eq!(
        config
            .policy
            .evaluate(Action::Net { host: subject }, Taint::Clean)
            .decision,
        Decision::Allow
    );
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Net {
                    host: "host11anything.example"
                },
                Taint::Clean
            )
            .decision,
        Decision::Ask
    );
    let script = Script::new(vec![
        batch(vec![call("fresh", "fake", json!({"net": subject}))]),
        Step::Reply(done()),
    ]);
    let (fresh, _) = fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    tokio::time::timeout(Duration::from_secs(2), fresh.prompt(input("fresh session")))
        .await
        .unwrap()
        .unwrap();
    let info = fresh.info().await;
    // The written rule allows the net crossing, so the strictest decision is
    // the tool's own allow and nothing asks.
    assert!(info.events.iter().any(|event| matches!(
        &event.kind,
        EventKind::ToolDecision { call, decision: Decision::Allow, .. } if call.0 == "fresh"
    )));
    assert!(
        !info
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ApprovalResolved { .. }))
    );
}

#[tokio::test]
async fn workspace_rule_write_failure_keeps_the_ask_pending() {
    let fixture = Fixture::new();
    fixture.paths.trust(&fixture.workspace).unwrap();
    let script = Script::new(vec![
        batch(vec![call("ask", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("ask"));
    let call = wait_for_ask(&mut rx).await;
    std::fs::create_dir_all(fixture.workspace.join(".xi/config.local.toml")).unwrap();
    assert!(matches!(
        session.approve(&call, true, Remember::Workspace),
        Err(xi_kernel::Error::Io(_))
    ));
    session.approve(&call, true, Remember::Once).unwrap();
    reply.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_clean_network_grant_cannot_bypass_taint_on_a_later_ask() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        batch(vec![call("clean", "fake", json!({"net": true}))]),
        Step::Reply(done()),
        batch(vec![
            call("ingest", "fake", json!({"untrusted": true})),
            call("dirty", "fake", json!({"net": true})),
        ]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("clean grant"));
    let call = wait_for_ask(&mut rx).await;
    session.approve(&call, true, Remember::Session).unwrap();
    reply.await.unwrap().unwrap();
    let reply = session.submit(input("tainted branch"));
    let call = wait_for_ask(&mut rx).await;
    assert_eq!(call.0, "dirty");
    session.approve(&call, false, Remember::Once).unwrap();
    reply.await.unwrap().unwrap();
}

#[tokio::test]
async fn workspace_grants_create_a_missing_local_config() {
    let fixture = Fixture::new();
    fixture.paths.trust(&fixture.workspace).unwrap();
    let script = Script::new(vec![
        batch(vec![call("ask", "fake", json!({"net": true}))]),
        Step::Reply(done()),
    ]);
    let (session, mut rx) =
        fixture.session(script, FakeTool { gate: None }, TurnOptions::default());
    let reply = session.submit(input("remember here"));
    let call = wait_for_ask(&mut rx).await;
    session.approve(&call, true, Remember::Workspace).unwrap();
    reply.await.unwrap().unwrap();
    let config = fixture.config();
    assert!(fixture.workspace.join(".xi/config.local.toml").is_file());
    assert_eq!(
        config
            .policy
            .evaluate(
                Action::Net {
                    host: "example.org"
                },
                Taint::Clean
            )
            .decision,
        Decision::Allow,
    );
}
