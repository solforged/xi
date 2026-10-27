//! The built-in `task` tool with a deterministic provider.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;
use xi_kernel::config::{Config, Paths};
use xi_kernel::{ApprovalMode, Backend, EventBus, Session, TurnOptions};
use xi_protocol::BoxFuture;
use xi_protocol::event::{StopReason, Usage};
use xi_protocol::methods::client::Delta;
use xi_protocol::model::ModelInfo;
use xi_protocol::{Block, EventKind, OutputBlock};
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
            let wire = format!("{:?}", request.messages);
            let seen_result = request
                .messages
                .iter()
                .any(|message| matches!(message, Message::ToolResults(_)));
            let content = if wire.contains("child one") {
                Block::Text {
                    text: "first child".into(),
                }
            } else if wire.contains("child two") {
                Block::Text {
                    text: "second child".into(),
                }
            } else if seen_result {
                Block::Text {
                    text: "parent done".into(),
                }
            } else {
                Block::ToolUse {
                    id: "task-call".into(),
                    name: "task".into(),
                    input: json!({"tasks": self.tasks}),
                    extra: Default::default(),
                }
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
                approval: ApprovalMode::Headless(false),
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
    let directory = info.path.parent().unwrap();
    let linked = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .filter(|path| *path != info.path)
        .filter(|path| {
            let log = xi_session::SessionLog::open(path).unwrap();
            matches!(log.root().kind, EventKind::SessionStarted { parent_session: Some(parent), .. } if parent == session.id())
        })
        .count();
    assert_eq!(linked, 2);
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
