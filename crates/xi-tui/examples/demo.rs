//! The terminal client against a scripted model and fake tools, so the whole
//! grammar can be seen without a provider: streaming prose, calls committing
//! in order, a stale anchor that retries, a network ask on a tainted branch,
//! and the done line.
//!
//! ```sh
//! cargo run -p xi-tui --example demo
//! ```
//!
//! Type anything to start the scripted turn. Nothing runs and nothing leaves
//! the machine; the session log is written to a temporary directory.
//! Pass `--peer` to inject a steering note from a second prompt source.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use xi_client::client::ClientNotification;
use xi_kernel::config::{Config, Paths};
use xi_protocol::event::{StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::{CreateParams, Delta};
use xi_protocol::{Block, OutputBlock, ToolCallId, ToolOutput};
use xi_provider::{BoxFuture, Error, ErrorKind, Provider, Request, Response};
use xi_tools::{Context, Crossing, Ran, Tool};

struct Turn {
    /// The reasoning summary, as OpenAI models title it; empty for none.
    thinking: &'static str,
    text: &'static str,
    calls: Vec<(&'static str, Value)>,
}

struct Script(Mutex<VecDeque<Turn>>);

fn script() -> Script {
    let t = |thinking, text, calls| Turn {
        thinking,
        text,
        calls,
    };
    Script(Mutex::new(VecDeque::from([
        t(
            "**Reproducing the failure**\n\nRun the failing test before reading code.",
            "I'll run the export tests first.",
            vec![("bash", json!({"command": "pytest tests/test_export.py -x"}))],
        ),
        t(
            "**Finding the filter**\n\nThe `visible` check decides what the export keeps.",
            "",
            vec![
                (
                    "search",
                    json!({"pattern": "def visible", "path": "policies/"}),
                ),
                ("read", json!({"path": "policies/archive.py"})),
            ],
        ),
        t(
            "",
            "archive.py still registers under the old key `archived`, but the registry now looks policies up by `archive`, so the **filter never runs**. Renaming the key.",
            vec![("edit", json!({"path": "policies/archive.py"}))],
        ),
        t(
            "",
            "",
            vec![
                ("read", json!({"path": "policies/archive.py", "offset": 10})),
                ("edit", json!({"path": "policies/archive.py"})),
            ],
        ),
        t(
            "",
            "Issue #41 has a repro. Reading it before I run the suite.",
            vec![(
                "fetch",
                json!({"url": "https://github.com/acme/ledger/issues/41"}),
            )],
        ),
        t(
            "",
            "",
            vec![("bash", json!({"command": "pytest tests/ -q"}))],
        ),
        t(
            "",
            "Fixed. All 11 policies now register singular keys and the suite passes.",
            vec![],
        ),
    ])))
}

impl Provider for Script {
    fn stream<'a>(
        &'a self,
        request: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let turn = self
                .0
                .lock()
                .map_err(|_| Error::new(ErrorKind::Config, "demo script lock poisoned"))?
                .pop_front()
                .unwrap_or(Turn {
                    thinking: "",
                    text: "That's the end of the script. /new starts it again.",
                    calls: vec![],
                });
            let thinking = if turn.thinking.is_empty() {
                "…"
            } else {
                turn.thinking
            };
            on_delta(Delta::Thinking {
                text: thinking.into(),
            });
            tokio::time::sleep(Duration::from_millis(700)).await;
            for word in turn.text.split_inclusive(' ') {
                on_delta(Delta::Text { text: word.into() });
                tokio::time::sleep(Duration::from_millis(35)).await;
            }
            let mut content = Vec::new();
            if !turn.thinking.is_empty() {
                content.push(Block::Thinking {
                    text: turn.thinking.into(),
                    signature: None,
                });
            }
            if !turn.text.is_empty() {
                content.push(Block::Text {
                    text: turn.text.into(),
                });
            }
            let n = request.messages.len();
            for (i, (name, input)) in turn.calls.into_iter().enumerate() {
                content.push(Block::ToolUse {
                    id: format!("call-{n}-{i}").as_str().into(),
                    name: name.into(),
                    input,
                    extra: Default::default(),
                });
            }
            let stop = if content.iter().any(|b| matches!(b, Block::ToolUse { .. })) {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            Ok(Response {
                content,
                stop,
                usage: Usage {
                    input: 900,
                    output: 120,
                    cache_read: 6_000 + 2_400 * n as u64,
                    cache_write: 400,
                },
            })
        })
    }
}

/// A tool that answers from a table instead of doing anything.
struct Fake {
    name: &'static str,
    edits: Mutex<usize>,
}

fn text(text: &str, is_error: bool, untrusted: bool) -> Ran {
    Ran {
        output: ToolOutput {
            content: vec![OutputBlock::Text { text: text.into() }],
            is_error,
            untrusted,
        },
        spill: None,
    }
}

impl Tool for Fake {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "Scripted demo tool".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn crossings(&self, input: &Value, _: &Context) -> Result<Vec<Crossing>, String> {
        Ok(match self.name {
            "bash" => vec![Crossing::Exec(
                input["command"].as_str().unwrap_or("").into(),
            )],
            "fetch" => vec![Crossing::Net("github.com".into())],
            _ => vec![],
        })
    }

    fn run<'a>(&'a self, _: &'a ToolCallId, input: Value, _: &'a Context) -> BoxFuture<'a, Ran> {
        Box::pin(async move {
            let command = input["command"].as_str().unwrap_or("");
            let (wait, ran) = match self.name {
                "bash" if command.contains("-x") => (
                    1800,
                    text(
                        format!("{}\n[exit 1]", "FAILED tests/test_export.py::test_archived_excluded\n    AssertionError: 'archive-2024' unexpectedly in export\n\n========= 1 failed, 11 passed in 1.62s =========\n".repeat(3).trim_end()).as_str(),
                        true,
                        false,
                    ),
                ),
                "bash" => (2600, text("48 passed in 3.61s\n[exit 0]", false, false)),
                "search" => (300, text("policies/archive.py:8\npolicies/deleted.py:7\npolicies/shared.py:9", false, false)),
                "read" => (200, text(&(1..=42).map(|n| format!("{n}|line\n")).collect::<String>(), false, false)),
                "edit" => {
                    let Ok(mut edits) = self.edits.lock() else {
                        return text("demo edit lock poisoned", true, false);
                    };
                    *edits += 1;
                    if *edits == 1 {
                        (400, text("Stale anchors. No changes written. Fresh context:\n12a3|    key = \"archived\"", true, false))
                    } else {
                        (400, text("policies/archive.py: 1 edit\n-    key = \"archived\"\n+    key = \"archive\"", false, false))
                    }
                }
                _ => (900, text("200 · 14 KB · \"Export drops archived rows\"", false, true)),
            };
            tokio::time::sleep(Duration::from_millis(wait)).await;
            ran
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), xi_tui::Error> {
    if !xi_tui::available() {
        return Err("the demo needs a terminal".into());
    }
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let paths = Paths {
        home: root.join("home"),
        config: root.join("config"),
        data: root.join("data"),
        state: root.join("state"),
    };
    let workspace = root.join("ledger");
    for path in [&workspace, &paths.config, &paths.home] {
        std::fs::create_dir_all(path)?;
    }
    let config = Config::load(paths.clone(), &workspace, &[])?;
    let model = config
        .registry()
        .model(&config.model)
        .cloned()
        .ok_or("the default model is not in the registry")?;
    let spawner = xi_daemon::InProcessSpawner {
        paths: paths.clone(),
        backend: Some(Arc::new(move || {
            let tools: Vec<Arc<dyn Tool>> = ["bash", "read", "search", "edit", "fetch"]
                .into_iter()
                .map(|name| {
                    Arc::new(Fake {
                        name,
                        edits: Mutex::new(0),
                    }) as Arc<dyn Tool>
                })
                .collect();
            xi_kernel::Backend {
                provider: Arc::new(script()),
                model: model.clone(),
                tools,
            }
        })),
    };
    let (reader, writer) = xi_daemon::private(paths, Arc::new(spawner));
    let (client, mut notifications) =
        xi_client::client::Client::over(reader, writer, "xi-demo").await?;
    let template = CreateParams {
        cwd: workspace,
        model: Some(config.model.into()),
        options: Default::default(),
    };
    let attached = client.create(template.clone()).await?;
    if std::env::args().any(|arg| arg == "--peer") {
        let peer = client.clone();
        let session = attached.info.session;
        let (send, receive) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut inject = true;
            while let Some(notification) = notifications.recv().await {
                if inject
                    && matches!(&notification, ClientNotification::Event(params)
                    if matches!(&params.event.kind, xi_protocol::EventKind::UserMessage { .. }))
                {
                    inject = false;
                    let peer = peer.clone();
                    // The App did not send this prompt, just as when another
                    // attached terminal steers the same session.
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        let _ = peer
                            .prompt(
                                session,
                                vec![Block::Text {
                                    text: "Keep archived rows out of the export, please.".into(),
                                }],
                            )
                            .await;
                    });
                }
                if send.send(notification).is_err() {
                    break;
                }
            }
        });
        notifications = receive;
    }
    xi_tui::run(xi_tui::Launch {
        client,
        notifications,
        template,
        attached,
        pick_session: false,
        history: None,
        tui_config: None,
    })
    .await
}
