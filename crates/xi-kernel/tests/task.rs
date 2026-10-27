//! The built-in `task` tool with a deterministic provider.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use xi_kernel::config::{Config, Paths};
use xi_kernel::{ApprovalMode, Backend, EventBus, Session, TurnOptions};
use xi_protocol::BoxFuture;
use xi_protocol::event::{Approver, StopReason, TitleSource, Usage};
use xi_protocol::methods::client::Delta;
use xi_protocol::model::ModelInfo;
use xi_protocol::{Block, Decision, EventKind, OutputBlock, ProviderTag, SessionId};
use xi_provider::{Message, Provider, Request, Response};

/// Answers the parent with one `task` call, then children by their prompt.
struct Fake {
    tasks: serde_json::Value,
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
            if request.web_search {
                return Ok(Response {
                    content: vec![
                        Block::Opaque {
                            provider: ProviderTag::Openai,
                            value: json!({
                                "type": "web_search_call",
                                "status": "completed",
                                "action": {
                                    "type": "search",
                                    "query": "xi task documentation",
                                    "sources": [{"type": "url", "url": "https://docs.example/tasks"}],
                                },
                            }),
                        },
                        Block::Text {
                            text: "Tasks run linked children.".into(),
                        },
                    ],
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                });
            }
            let prompt = match request.messages.first() {
                Some(Message::User(blocks)) => match blocks.first() {
                    Some(Block::Text { text }) => text.as_str(),
                    _ => "",
                },
                _ => "",
            };
            let seen_result = request
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResults(_)));
            let call = |name: &str, input| Block::ToolUse {
                id: format!("{name}-call").as_str().into(),
                name: name.into(),
                input,
                extra: Default::default(),
            };
            let content = if prompt == "child search" {
                if seen_result {
                    Block::Text {
                        text: "search child done".into(),
                    }
                } else {
                    call("web_search", json!({"query": "xi task documentation"}))
                }
            } else if prompt == "child fetch" {
                if seen_result {
                    Block::Text {
                        text: "fetch child done".into(),
                    }
                } else {
                    call("fetch", json!({"url": "http://127.0.0.1:9/"}))
                }
            } else if prompt == "child one" {
                Block::Text {
                    text: "first child".into(),
                }
            } else if prompt == "child two" {
                Block::Text {
                    text: "second child".into(),
                }
            } else if seen_result {
                Block::Text {
                    text: "parent done".into(),
                }
            } else {
                call("task", json!({"tasks": self.tasks}))
            };
            let stop = if matches!(content, Block::ToolUse { .. }) {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            Ok(Response {
                content: vec![content],
                stop,
                usage: Usage::default(),
            })
        })
    }
}

struct Fixture {
    _dir: TempDir,
    paths: Paths,
    workspace: PathBuf,
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
        for path in [&workspace, &paths.home, &paths.config] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            _dir: dir,
            paths,
            workspace,
        }
    }

    async fn run(&self, tools: &[&str], tasks: serde_json::Value) -> (Session, Arc<Fake>) {
        self.run_with_approval(tools, tasks, ApprovalMode::Headless(false))
            .await
    }

    async fn run_with_approval(
        &self,
        tools: &[&str],
        tasks: serde_json::Value,
        approval: ApprovalMode,
    ) -> (Session, Arc<Fake>) {
        let mut config = Config::load(self.paths.clone(), &self.workspace, &[]).unwrap();
        config.tools = tools.iter().map(|&name| name.into()).collect();
        let fake = Arc::new(Fake {
            tasks,
            requests: Mutex::new(Vec::new()),
        });
        let session = Session::create(
            config.clone(),
            None,
            EventBus::default(),
            TurnOptions {
                max_turns: 4,
                approval,
                leaser: None,
                auto_title: false,
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
                tools: xi_kernel::toolset(&config).unwrap(),
            }),
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            session.prompt(vec![Block::Text {
                text: "parent prompt".into(),
            }]),
        )
        .await
        .expect("task deadlocked")
        .unwrap();
        (session, fake)
    }
}

fn result_text(events: &[xi_protocol::Event]) -> (String, bool) {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ToolResult { output, .. } => Some((
                output
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        OutputBlock::Text { text } => Some(text.as_str()),
                        OutputBlock::Image { .. } => None,
                    })
                    .collect(),
                output.is_error,
            )),
            _ => None,
        })
        .unwrap()
}

fn linked_children(path: &Path, parent: SessionId) -> Vec<xi_session::SessionLog> {
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|child| child != path)
        .filter(|child| {
            child
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|child| xi_session::SessionLog::open(&child).unwrap())
        .filter(|log| {
            matches!(log.root().kind, EventKind::SessionStarted { parent_session: Some(id), .. }
                if id == parent)
        })
        .collect()
}

#[tokio::test]
async fn task_runs_linked_children_that_cannot_delegate() {
    let fixture = Fixture::new();
    let (session, fake) = fixture
        .run(
            &["read", "task", "bash"],
            json!([
                {"prompt": "child one", "tools": ["read"], "model": "@smol"},
                {"prompt": "child two"},
            ]),
        )
        .await;
    let info = session.info().await;
    assert_eq!(
        result_text(&info.events),
        (
            "Task 1 [done]\nfirst child\n\nTask 2 [done]\nsecond child".into(),
            false
        )
    );
    let requests = fake.requests.lock().unwrap();
    let names = |request: &Request| -> Vec<String> {
        request.tools.iter().map(|tool| tool.name.clone()).collect()
    };
    assert_eq!(names(&requests[0]), ["read", "task", "bash"]);
    let mut children: Vec<_> = requests
        .iter()
        .filter(|request| {
            matches!(request.messages.first(), Some(Message::User(blocks))
                if blocks.iter().any(|block| matches!(block, Block::Text { text }
                    if text == "child one" || text == "child two")))
        })
        .map(names)
        .collect();
    children.sort();
    assert_eq!(children, [vec!["read"], vec!["read", "bash"]]);
    assert_eq!(linked_children(&info.path, session.id()).len(), 2);
}

#[tokio::test]
async fn children_that_ask_for_task_fail_without_stopping_their_siblings() {
    let fixture = Fixture::new();
    let (session, _) = fixture
        .run(
            &["read", "task"],
            json!([
                {"prompt": "child one", "tools": ["task"]},
                {"prompt": "child two"},
            ]),
        )
        .await;
    let (text, is_error) = result_text(&session.info().await.events);
    assert!(is_error);
    assert!(
        text.starts_with(
            "Task 1 [FAILED]\nchild tool task is not one of the parent's built-in tools"
        ),
        "{text}"
    );
    assert!(text.ends_with("Task 2 [done]\nsecond child"), "{text}");
}

#[tokio::test]
async fn invalid_task_input_runs_no_children() {
    let fixture = Fixture::new();
    let (session, fake) = fixture
        .run(
            &["task"],
            json!([{"prompt": "child one", "model": "@huge"}]),
        )
        .await;
    let (text, is_error) = result_text(&session.info().await.events);
    assert!(is_error);
    assert!(text.contains("unknown model role \"huge\""), "{text}");
    // Only the parent's call and its follow-up reached the provider.
    assert_eq!(fake.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn named_tasks_trim_names_and_persist_user_titles_before_the_first_prompt() {
    let fixture = Fixture::new();
    let long_name = "界".repeat(80);
    let (session, _) = fixture
        .run(
            &["task"],
            json!([
                {"prompt": "child one", "name": "  Investigate α  "},
                {"prompt": "child two", "name": long_name},
            ]),
        )
        .await;
    let info = session.info().await;
    assert_eq!(
        result_text(&info.events),
        (
            format!(
                "Task 1 · Investigate α [done]\nfirst child\n\nTask 2 · {long_name} [done]\nsecond child"
            ),
            false,
        )
    );
    let children = linked_children(&info.path, session.id());
    assert_eq!(children.len(), 2);
    for child in children {
        let events = child.events();
        let prompt = events
            .iter()
            .position(|event| matches!(event.kind, EventKind::UserMessage { .. }))
            .unwrap();
        let expected = match &events[prompt].kind {
            EventKind::UserMessage { content }
                if content
                    == &[Block::Text {
                        text: "child one".into(),
                    }] =>
            {
                "Investigate α"
            }
            EventKind::UserMessage { content }
                if content
                    == &[Block::Text {
                        text: "child two".into(),
                    }] =>
            {
                &long_name
            }
            other => panic!("unexpected child prompt: {other:?}"),
        };
        let titles: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| matches!(event.kind, EventKind::SessionTitled { .. }))
            .collect();
        assert_eq!(titles.len(), 1);
        assert!(titles[0].0 < prompt, "title must precede the first prompt");
        assert!(matches!(&titles[0].1.kind, EventKind::SessionTitled {
            title, by: TitleSource::User, model: None, usage: None,
        } if title == expected));
    }
}

#[tokio::test]
async fn invalid_task_names_run_no_children() {
    for name in [
        String::new(),
        " \t\u{2003} ".into(),
        "first\nsecond".into(),
        "first\rsecond".into(),
        "界".repeat(81),
    ] {
        let fixture = Fixture::new();
        let (session, fake) = fixture
            .run(
                &["task"],
                json!([
                    {"prompt": "child one", "name": "Valid"},
                    {"prompt": "child two", "name": name},
                ]),
            )
            .await;
        let info = session.info().await;
        let (text, is_error) = result_text(&info.events);
        assert!(is_error, "{name:?}: {text}");
        assert!(text.contains("name"), "{name:?}: {text}");
        assert_eq!(fake.requests.lock().unwrap().len(), 2, "{name:?}");
        assert!(linked_children(&info.path, session.id()).is_empty());
    }
}

#[tokio::test]
async fn children_inherit_web_tools_or_only_their_explicit_subset() {
    let fixture = Fixture::new();
    let (session, fake) = fixture
        .run(
            &["read", "task", "fetch", "web_search"],
            json!([
                {"prompt": "child one"},
                {"prompt": "child two", "tools": ["web_search"]},
                {"prompt": "child one", "tools": ["fetch"]},
                {"prompt": "child two", "tools": []},
            ]),
        )
        .await;
    let info = session.info().await;
    assert!(!result_text(&info.events).1);
    assert_eq!(linked_children(&info.path, session.id()).len(), 4);
    let requests = fake.requests.lock().unwrap();
    let mut catalogs: Vec<Vec<&str>> = requests
        .iter()
        .filter(|request| {
            matches!(request.messages.first(), Some(Message::User(blocks))
                if blocks.iter().any(|block| matches!(block, Block::Text { text }
                    if text == "child one" || text == "child two")))
        })
        .map(|request| {
            request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect()
        })
        .collect();
    catalogs.sort();
    assert_eq!(
        catalogs,
        [
            vec![],
            vec!["fetch"],
            vec!["read", "fetch", "web_search"],
            vec!["web_search"],
        ]
    );
}

#[tokio::test]
async fn children_cannot_enable_parent_disabled_web_tools() {
    for (tools, disabled) in [
        (vec!["task", "fetch"], "web_search"),
        (vec!["task", "web_search"], "fetch"),
    ] {
        let fixture = Fixture::new();
        let (session, fake) = fixture
            .run(
                &tools,
                json!([
                    {"prompt": "child one", "tools": [disabled]},
                    {"prompt": "child two"},
                ]),
            )
            .await;
        let info = session.info().await;
        let (text, is_error) = result_text(&info.events);
        assert!(is_error);
        assert!(
            text.starts_with(&format!(
                "Task 1 [FAILED]\nchild tool {disabled} is not one of the parent's built-in tools"
            )),
            "{text}"
        );
        assert!(text.ends_with("Task 2 [done]\nsecond child"), "{text}");
        assert_eq!(fake.requests.lock().unwrap().len(), 3);
        assert_eq!(linked_children(&info.path, session.id()).len(), 1);
    }
}

#[tokio::test]
async fn child_web_tools_deny_network_asks_even_when_the_parent_approves() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.paths.config.join("config.toml"),
        "[web_search]\nbackend='model'\nmodel='chatgpt/gpt-6.1-sol'\n",
    )
    .unwrap();
    let (session, fake) = fixture
        .run_with_approval(
            &["task", "web_search", "fetch"],
            json!([
                {"prompt": "child search", "tools": ["web_search"]},
                {"prompt": "child fetch", "tools": ["fetch"]},
            ]),
            ApprovalMode::Headless(true),
        )
        .await;
    let info = session.info().await;
    assert!(!info.tainted);
    let children = linked_children(&info.path, session.id());
    assert_eq!(children.len(), 2);
    for child in children {
        let events = child.events();
        let (text, is_error) = result_text(events);
        assert!(is_error, "{text}");
        assert!(text.contains("denied by policy"), "{text}");
        assert!(events.iter().any(|event| matches!(&event.kind,
            EventKind::ToolDecision { action, decision: Decision::Ask, .. }
                if action.starts_with("net ")
        )));
        assert!(events.iter().any(|event| matches!(
            event.kind,
            EventKind::ApprovalResolved {
                approved: false,
                by: Approver::Headless,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, EventKind::Tainted { .. }))
        );
    }
    assert!(
        !fake
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.web_search)
    );
}

#[tokio::test]
async fn child_web_search_dispatch_taints_the_child_and_its_task_result() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.paths.config.join("config.toml"),
        "[web_search]\nbackend='model'\nmodel='chatgpt/gpt-6.1-sol'\n\
         [[rule]]\naction='net'\npattern='chatgpt.com'\neffect='allow'\n",
    )
    .unwrap();
    let (session, fake) = fixture
        .run(
            &["task", "web_search"],
            json!([{"prompt": "child search", "name": "Documentation"}]),
        )
        .await;
    let info = session.info().await;
    assert_eq!(
        result_text(&info.events),
        (
            "Task 1 · Documentation [done]\nsearch child done".into(),
            false
        )
    );
    assert!(info.tainted);
    assert!(info.events.iter().any(|event| matches!(&event.kind,
        EventKind::ToolResult { call, output, .. }
            if call.0 == "task-call" && output.untrusted && !output.is_error
    )));
    let children = linked_children(&info.path, session.id());
    assert_eq!(children.len(), 1);
    assert_eq!(
        result_text(children[0].events()),
        (
            "Tasks run linked children.\n\nSources:\n1. <https://docs.example/tasks>".into(),
            false,
        )
    );
    assert!(
        children[0]
            .events()
            .iter()
            .any(|event| matches!(&event.kind,
                EventKind::Tainted { call: Some(call), .. } if call.0 == "web_search-call"
            ))
    );
    let requests = fake.requests.lock().unwrap();
    let searches: Vec<_> = requests
        .iter()
        .filter(|request| request.web_search)
        .collect();
    assert_eq!(searches.len(), 1);
    assert!(searches[0].tools.is_empty());
    assert_eq!(
        searches[0].messages,
        [Message::User(vec![Block::Text {
            text: "xi task documentation".into(),
        }])]
    );
}
