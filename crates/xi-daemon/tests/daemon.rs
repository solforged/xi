//! Daemon routing over real worker pipes without network model requests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc, oneshot};
use xi_rpc::{Incoming, Peer, PeerError};
use xi_daemon::{Daemon, DaemonOptions, InProcessSpawner, Spawner, WorkerProcess};
use xi_kernel::Backend;
use xi_kernel::config::{DaemonSettings, Paths};
use xi_protocol::event::{ModelRef, Remember, StopReason, Usage};
use xi_protocol::methods::client::*;
use xi_protocol::model::{ModelInfo, Reasoning};
use xi_protocol::rpc::{RequestMethod, RpcError};
use xi_protocol::{Block, EventId, EventKind, SessionId, Timestamp};
use xi_provider::{Provider, Request, Response};

struct Script(Mutex<VecDeque<Response>>);

impl Provider for Script {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, xi_provider::Error>> {
        Box::pin(async move {
            let response = self.0.lock().unwrap().pop_front().unwrap_or_else(done);
            for block in &response.content {
                if let Block::Text { text } = block {
                    on_delta(Delta::Text { text: text.clone() });
                }
            }
            Ok(response)
        })
    }
}

struct WaitingProvider(Arc<Notify>);

impl Provider for WaitingProvider {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        _: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, xi_provider::Error>> {
        Box::pin(async move {
            self.0.notify_one();
            std::future::pending().await
        })
    }
}

struct Fixture {
    _temp: TempDir,
    paths: Paths,
    cwd: std::path::PathBuf,
    outside: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let paths = Paths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
        };
        let cwd = root.join("workspace");
        for path in [&paths.home, &paths.config, &cwd] {
            std::fs::create_dir_all(path).unwrap();
        }
        let outside = root.join("outside.txt");
        std::fs::write(&outside, "outside data").unwrap();
        Self {
            _temp: temp,
            paths,
            cwd,
            outside,
        }
    }

    fn spawner(&self, responses: Vec<Response>) -> Arc<dyn Spawner> {
        self.provider_spawner(Arc::new(Script(Mutex::new(responses.into()))))
    }

    fn provider_spawner(&self, provider: Arc<dyn Provider>) -> Arc<dyn Spawner> {
        Arc::new(InProcessSpawner {
            paths: self.paths.clone(),
            backend: Some(Arc::new(move || Backend {
                provider: provider.clone(),
                model: ModelInfo {
                    provider: "anthropic".into(),
                    id: "claude-sonnet-5-5".into(),
                    context_window: 8192,
                    max_output: 1024,
                    reasoning: Reasoning::ALL.to_vec(),
                    pricing: None,
                },
                tools: xi_tools::toolset(&["read"]).unwrap(),
            })),
        })
    }

    fn daemon(&self, responses: Vec<Response>, private: bool) -> Daemon {
        Daemon::new(DaemonOptions {
            settings: DaemonSettings::default(),
            paths: self.paths.clone(),
            spawner: self.spawner(responses),
            private,
        })
    }

    fn create(&self) -> CreateParams {
        CreateParams {
            cwd: self.cwd.clone(),
            model: None,
            options: SessionOptions::default(),
        }
    }
}

struct StubbornSpawner {
    session: AttachResult,
    answers_close: bool,
    closes: mpsc::UnboundedSender<tokio::time::Instant>,
    kills: mpsc::UnboundedSender<tokio::time::Instant>,
}

impl Spawner for StubbornSpawner {
    fn spawn(&self) -> BoxFuture<'_, std::io::Result<WorkerProcess>> {
        Box::pin(async move {
            let (daemon, worker) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(daemon);
            let (worker_reader, worker_writer) = tokio::io::split(worker);
            let (peer, mut incoming) = Peer::spawn(worker_reader, worker_writer);
            let (kill, mut killed) = oneshot::channel();
            let session = self.session.clone();
            let answers_close = self.answers_close;
            let closes = self.closes.clone();
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut killed => {
                            peer.close();
                            return None;
                        }
                        Some(Incoming::Request { id, method, .. }) = incoming.recv() => {
                            let result = match method.as_str() {
                                "worker/start" | "session/attach" => {
                                    serde_json::to_value(&session).unwrap()
                                }
                                "session/close" => {
                                    let _ = closes.send(tokio::time::Instant::now());
                                    if !answers_close {
                                        continue;
                                    }
                                    Value::Null
                                }
                                "session/cancel" => Value::Null,
                                _ => panic!("unexpected worker request {method}"),
                            };
                            peer.respond(id, Ok(result)).unwrap();
                        }
                    }
                }
            });
            let kills = self.kills.clone();
            Ok(WorkerProcess {
                reader: Box::new(reader),
                writer: Box::new(writer),
                exited: Box::pin(async move { task.await.unwrap() }),
                kill: Box::new(move || {
                    let _ = kills.send(tokio::time::Instant::now());
                    let _ = kill.send(());
                }),
            })
        })
    }
}

fn connection(daemon: &Daemon) -> (Peer, mpsc::UnboundedReceiver<Incoming>) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let daemon = daemon.clone();
    tokio::spawn(async move {
        daemon.serve(reader, writer).await;
    });
    let (reader, writer) = tokio::io::split(client);
    Peer::spawn(reader, writer)
}

async fn request<M: RequestMethod>(
    peer: &Peer,
    params: &M::Params,
) -> Result<M::Result, PeerError> {
    tokio::time::timeout(Duration::from_secs(8), peer.request::<M>(params))
        .await
        .unwrap()
}

async fn initialize(peer: &Peer) -> InitializeResult {
    request::<Initialize>(
        peer,
        &InitializeParams {
            protocol_version: xi_protocol::PROTOCOL_VERSION,
            client: ClientInfo {
                name: "daemon-test".into(),
                version: "0".into(),
            },
        },
    )
    .await
    .unwrap()
}

fn error_code<T>(result: Result<T, PeerError>) -> i64 {
    match result {
        Err(PeerError::Rpc(error)) => error.code,
        _ => panic!("expected RPC error"),
    }
}

async fn notification(rx: &mut mpsc::UnboundedReceiver<Incoming>, name: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(Incoming::Notification { method, params }) = rx.recv().await {
                if method == name {
                    return params;
                }
            } else {
                panic!("connection closed before notification {name}");
            }
        }
    })
    .await
    .unwrap()
}

async fn assistant(rx: &mut mpsc::UnboundedReceiver<Incoming>) -> EventParams {
    loop {
        let event: EventParams =
            serde_json::from_value(notification(rx, "event/appended").await).unwrap();
        if matches!(event.event.kind, EventKind::AssistantMessage { .. }) {
            return event;
        }
    }
}

async fn prompt(peer: &Peer, session: SessionId) -> PromptResult {
    request::<SessionPrompt>(
        peer,
        &PromptParams {
            session,
            content: vec![Block::Text {
                text: "hello".into(),
            }],
        },
    )
    .await
    .unwrap()
}

async fn stop(peer: &Peer, daemon: &Daemon) {
    request::<DaemonShutdown>(
        peer,
        &ShutdownParams {
            when: ShutdownWhen::Now,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(8), daemon.finished())
        .await
        .unwrap();
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

#[tokio::test]
async fn initialization_is_required_and_versions_must_match() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (peer, _) = connection(&daemon);
    assert_eq!(
        error_code(request::<DaemonStatus>(&peer, &()).await),
        RpcError::NOT_INITIALIZED
    );
    let error = request::<Initialize>(
        &peer,
        &InitializeParams {
            protocol_version: xi_protocol::PROTOCOL_VERSION + 1,
            client: ClientInfo {
                name: "test".into(),
                version: "0".into(),
            },
        },
    )
    .await
    .unwrap_err();
    match error {
        PeerError::Rpc(error) => {
            assert_eq!(error.code, RpcError::VERSION_MISMATCH);
            assert_eq!(
                error.data,
                Some(json!({ "daemon_protocol_version": xi_protocol::PROTOCOL_VERSION }))
            );
        }
        _ => panic!("expected version mismatch"),
    }
    assert_eq!(
        error_code(request::<SessionList>(&peer, &ListParams::default()).await),
        RpcError::NOT_INITIALIZED
    );
    assert!(!initialize(&peer).await.capabilities.private);
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn relative_create_and_open_paths_are_rejected_before_starting_workers() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (peer, _) = connection(&daemon);
    initialize(&peer).await;
    let mut create = fixture.create();
    create.cwd = ".".into();
    assert_eq!(
        error_code(request::<SessionCreate>(&peer, &create).await),
        RpcError::INVALID_PARAMS
    );
    assert_eq!(
        error_code(
            request::<SessionOpen>(
                &peer,
                &OpenParams {
                    path: ".".into(),
                    options: SessionOptions::default(),
                },
            )
            .await,
        ),
        RpcError::INVALID_PARAMS
    );
    assert_eq!(
        request::<DaemonStatus>(&peer, &()).await.unwrap().workers,
        0
    );
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn config_set_preserves_comments_and_reports_global_source() {
    let fixture = Fixture::new();
    let global = fixture.paths.config.join("config.toml");
    std::fs::write(
        &global,
        "# keep this comment\n[compaction]\nenabled = true\n",
    )
    .unwrap();
    let daemon = fixture.daemon(vec![], false);
    let (peer, _) = connection(&daemon);
    initialize(&peer).await;
    let report = request::<ConfigSettings>(
        &peer,
        &SettingsParams {
            cwd: fixture.cwd.clone(),
            configs: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(report.files.global, global);
    let setting = report
        .settings
        .iter()
        .find(|setting| setting.key == "compaction.enabled")
        .unwrap();
    assert_eq!(setting.value, json!(true));
    assert_eq!(setting.source, SettingLayer::Global);

    let mut params = SetSettingParams {
        cwd: fixture.cwd.clone(),
        configs: Vec::new(),
        key: "compaction.enabled".into(),
        value: json!(false),
    };
    let report = request::<ConfigSet>(&peer, &params).await.unwrap();
    let setting = report
        .settings
        .iter()
        .find(|setting| setting.key == "compaction.enabled")
        .unwrap();
    assert_eq!(setting.value, json!(false));
    assert_eq!(setting.source, SettingLayer::Global);
    let saved = std::fs::read_to_string(&global).unwrap();
    assert!(saved.contains("# keep this comment"));
    assert!(saved.contains("enabled = false"));
    params.value = json!("not a boolean");
    match request::<ConfigSet>(&peer, &params).await {
        Err(PeerError::Rpc(error)) => {
            assert_eq!(error.code, RpcError::INVALID_PARAMS);
            assert!(error.message.contains("compaction.enabled"));
        }
        result => panic!("expected invalid params, got {result:?}"),
    }
    assert_eq!(std::fs::read_to_string(&global).unwrap(), saved);
    assert_eq!(
        request::<DaemonStatus>(&peer, &()).await.unwrap().workers,
        0
    );
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn relative_settings_paths_are_rejected_before_editing_config() {
    let fixture = Fixture::new();
    let global = fixture.paths.config.join("config.toml");
    let original = "# do not edit\n[compaction]\nenabled = true\n";
    std::fs::write(&global, original).unwrap();
    let daemon = fixture.daemon(vec![], false);
    let (peer, _) = connection(&daemon);
    initialize(&peer).await;
    for (cwd, configs) in [
        (".".into(), Vec::new()),
        (fixture.cwd.clone(), vec!["extra.toml".into()]),
    ] {
        assert_eq!(
            error_code(
                request::<ConfigSettings>(
                    &peer,
                    &SettingsParams {
                        cwd: cwd.clone(),
                        configs: configs.clone(),
                    },
                )
                .await,
            ),
            RpcError::INVALID_PARAMS
        );
        assert_eq!(
            error_code(
                request::<ConfigSet>(
                    &peer,
                    &SetSettingParams {
                        cwd,
                        configs,
                        key: "compaction.enabled".into(),
                        value: json!(false),
                    },
                )
                .await,
            ),
            RpcError::INVALID_PARAMS
        );
    }
    assert_eq!(std::fs::read_to_string(&global).unwrap(), original);
    stop(&peer, &daemon).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_to_back_prompt_and_cancel_always_cancel_the_turn() {
    let fixture = Fixture::new();
    let daemon = Daemon::new(DaemonOptions {
        settings: DaemonSettings::default(),
        paths: fixture.paths.clone(),
        spawner: fixture.provider_spawner(Arc::new(WaitingProvider(Arc::new(Notify::new())))),
        private: false,
    });
    let (peer, _) = connection(&daemon);
    initialize(&peer).await;
    let session = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap()
        .info
        .session;
    for _ in 0..64 {
        let prompt = peer
            .start_request_value(
                "session/prompt",
                serde_json::to_value(PromptParams {
                    session,
                    content: vec![Block::Text {
                        text: "hello".into(),
                    }],
                })
                .unwrap(),
            )
            .unwrap();
        let cancel = peer
            .start_request_value(
                "session/cancel",
                serde_json::to_value(SessionRef { session }).unwrap(),
            )
            .unwrap();
        tokio::time::timeout(Duration::from_secs(8), cancel)
            .await
            .unwrap()
            .unwrap();
        let outcome: PromptResult = serde_json::from_value(
            tokio::time::timeout(Duration::from_secs(8), prompt)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(outcome.cancelled);
    }
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn retained_steering_is_not_running_and_does_not_prevent_idle_close() {
    let fixture = Fixture::new();
    let started = Arc::new(Notify::new());
    let daemon = Daemon::new(DaemonOptions {
        settings: DaemonSettings {
            idle_exit: 1,
            ..DaemonSettings::default()
        },
        paths: fixture.paths.clone(),
        spawner: fixture.provider_spawner(Arc::new(WaitingProvider(started.clone()))),
        private: false,
    });
    let (peer, mut events) = connection(&daemon);
    initialize(&peer).await;
    let created = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap();
    let session = created.info.session;
    let turn = peer
        .start_request_value(
            "session/prompt",
            serde_json::to_value(PromptParams {
                session,
                content: vec![Block::Text {
                    text: "first".into(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), started.notified())
        .await
        .unwrap();
    let steering = peer
        .start_request_value(
            "session/prompt",
            serde_json::to_value(PromptParams {
                session,
                content: vec![Block::Text {
                    text: "retain until another submission".into(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
    // The snapshot request is ordered after steering registration in the worker.
    assert!(
        request::<SessionAttach>(
            &peer,
            &AttachParams {
                session,
                since: None,
            },
        )
        .await
        .unwrap()
        .running
    );
    request::<SessionCancel>(&peer, &SessionRef { session })
        .await
        .unwrap();
    let outcome: PromptResult = serde_json::from_value(
        tokio::time::timeout(Duration::from_secs(8), turn)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(outcome.cancelled);
    tokio::pin!(steering);
    assert!(futures::poll!(&mut steering).is_pending());
    let listed = request::<SessionList>(&peer, &ListParams::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].running);
    assert!(
        !request::<SessionAttach>(
            &peer,
            &AttachParams {
                session,
                since: None,
            },
        )
        .await
        .unwrap()
        .running
    );
    request::<SessionList>(
        &peer,
        &ListParams {
            cwd: None,
            running: Some(true),
        },
    )
    .await
    .unwrap();
    request::<SessionDetach>(&peer, &SessionRef { session })
        .await
        .unwrap();
    let exited: ExitedParams =
        serde_json::from_value(notification(&mut events, "session/exited").await).unwrap();
    assert_eq!(exited.session, session);
    assert_eq!(exited.status, Some(0));
    assert!(steering.await.is_err());
    assert_eq!(
        request::<DaemonStatus>(&peer, &()).await.unwrap().workers,
        0
    );
    let reopened = request::<SessionOpen>(
        &peer,
        &OpenParams {
            path: created.info.path,
            options: SessionOptions::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(reopened.info.session, session);
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn create_attach_detach_and_live_session_index() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (first, mut first_events) = connection(&daemon);
    initialize(&first).await;
    let created = request::<SessionCreate>(&first, &fixture.create())
        .await
        .unwrap();
    let session = created.info.session;
    let root: EventParams =
        serde_json::from_value(notification(&mut first_events, "event/appended").await).unwrap();
    assert_eq!(root.session, session);
    assert!(matches!(root.event.kind, EventKind::SessionStarted { .. }));
    prompt(&first, session).await;
    assert_eq!(assistant(&mut first_events).await.session, session);

    let (second, mut second_events) = connection(&daemon);
    initialize(&second).await;
    let attached = request::<SessionAttach>(
        &second,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(attached.info.session, session);
    assert!(!attached.events.is_empty());
    prompt(&first, session).await;
    assistant(&mut first_events).await;
    assert_eq!(assistant(&mut second_events).await.session, session);
    request::<SessionDetach>(&second, &SessionRef { session })
        .await
        .unwrap();
    prompt(&first, session).await;
    assistant(&mut first_events).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), second_events.recv())
            .await
            .is_err()
    );

    let sessions = request::<SessionList>(&first, &ListParams::default())
        .await
        .unwrap();
    let summary = sessions
        .iter()
        .find(|item| item.session == session)
        .unwrap();
    assert_eq!(summary.cwd, fixture.cwd);
    assert!(summary.head.is_some());
    assert!(!summary.running);
    assert!(
        request::<SessionList>(
            &first,
            &ListParams {
                cwd: Some(fixture.paths.home.clone()),
                running: None
            }
        )
        .await
        .unwrap()
        .is_empty()
    );
    let status = request::<DaemonStatus>(&first, &()).await.unwrap();
    assert_eq!(status.workers, 1);
    assert_eq!(status.clients, 2);

    assert_eq!(
        error_code(
            request::<SessionOpen>(
                &first,
                &OpenParams {
                    path: created.info.path,
                    options: SessionOptions::default()
                }
            )
            .await
        ),
        RpcError::SESSION_HELD
    );
    stop(&first, &daemon).await;
}

#[tokio::test]
async fn reasoning_and_catalog_requests_are_forwarded_to_the_worker() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (peer, mut events) = connection(&daemon);
    initialize(&peer).await;
    let attached = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap();
    let session = attached.info.session;
    request::<SessionSetReasoning>(
        &peer,
        &SetReasoningParams {
            session,
            reasoning: Reasoning::High,
        },
    )
    .await
    .unwrap();
    loop {
        let event: EventParams =
            serde_json::from_value(notification(&mut events, "event/appended").await).unwrap();
        if let EventKind::ReasoningChanged { reasoning } = event.event.kind {
            assert_eq!(reasoning, Reasoning::High);
            break;
        }
    }
    let models = request::<SessionModels>(&peer, &SessionRef { session })
        .await
        .unwrap();
    assert_eq!(models.current, attached.info.model);
    assert_eq!(models.reasoning, Reasoning::High);
    assert!(!models.models.is_empty());
    assert!(
        request::<SessionCommands>(&peer, &SessionRef { session })
            .await
            .unwrap()
            .is_empty()
    );
    let attached = request::<SessionAttach>(
        &peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(attached.info.reasoning, Reasoning::High);
    assert_eq!(
        error_code(
            request::<SessionModels>(
                &peer,
                &SessionRef {
                    session: SessionId::generate()
                }
            )
            .await
        ),
        RpcError::UNKNOWN_SESSION,
    );
    stop(&peer, &daemon).await;
}

#[tokio::test]
async fn asks_enter_inbox_broadcast_and_resolve_once() {
    let fixture = Fixture::new();
    let call = Response {
        content: vec![Block::ToolUse {
            id: "outside-read".into(),
            name: "read".into(),
            input: json!({ "path": fixture.outside }),
            extra: Default::default(),
        }],
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    };
    let daemon = fixture.daemon(vec![call, done()], false);
    let (first, _) = connection(&daemon);
    let (observer, mut observed) = connection(&daemon);
    initialize(&first).await;
    initialize(&observer).await;
    let session = request::<SessionCreate>(&first, &fixture.create())
        .await
        .unwrap()
        .info
        .session;
    let prompting = first.clone();
    let turn = tokio::spawn(async move { prompt(&prompting, session).await });
    let pending: PendingApproval =
        serde_json::from_value(notification(&mut observed, "approval/pending").await).unwrap();
    assert_eq!(pending.session, session);
    assert_eq!(pending.call, "outside-read".into());
    assert_eq!(pending.tool, "read");
    assert!(!pending.taint);
    assert!(!pending.trusted);
    assert_eq!(
        request::<ApprovalList>(&observer, &()).await.unwrap(),
        vec![pending.clone()]
    );
    let running = request::<SessionList>(
        &observer,
        &ListParams {
            cwd: None,
            running: Some(true),
        },
    )
    .await
    .unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].pending_approvals, 1);
    request::<ApprovalResolve>(
        &observer,
        &ResolveParams {
            session,
            call: pending.call.clone(),
            approved: true,
            remember: Remember::Once,
        },
    )
    .await
    .unwrap();
    turn.await.unwrap();
    assert_eq!(
        error_code(
            request::<ApprovalResolve>(
                &observer,
                &ResolveParams {
                    session,
                    call: pending.call,
                    approved: true,
                    remember: Remember::Once,
                }
            )
            .await
        ),
        RpcError::APPROVAL_NOT_PENDING
    );
    assert!(
        request::<ApprovalList>(&observer, &())
            .await
            .unwrap()
            .is_empty()
    );
    stop(&first, &daemon).await;
}

#[tokio::test]
async fn headless_asks_do_not_enter_inbox_or_broadcast() {
    for approval in [ApprovalPolicy::Deny, ApprovalPolicy::Allow] {
        let fixture = Fixture::new();
        let call = Response {
            content: vec![Block::ToolUse {
                id: "outside-read".into(),
                name: "read".into(),
                input: json!({ "path": fixture.outside }),
                extra: Default::default(),
            }],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        };
        let daemon = fixture.daemon(vec![call, done()], true);
        let (peer, mut events) = connection(&daemon);
        initialize(&peer).await;
        let mut create = fixture.create();
        create.options.approval = approval;
        let session = request::<SessionCreate>(&peer, &create)
            .await
            .unwrap()
            .info
            .session;
        prompt(&peer, session).await;
        assert!(
            request::<ApprovalList>(&peer, &())
                .await
                .unwrap()
                .is_empty()
        );
        let sessions = request::<SessionList>(&peer, &ListParams::default())
            .await
            .unwrap();
        assert_eq!(sessions[0].pending_approvals, 0);

        // Attach is a wire-order barrier after all events from the completed turn.
        let attached = request::<SessionAttach>(
            &peer,
            &AttachParams {
                session,
                since: None,
            },
        )
        .await
        .unwrap();
        assert!(attached.events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ToolDecision {
                decision: xi_protocol::Decision::Ask,
                ..
            }
        )));
        assert!(attached.events.iter().any(|event| matches!(
            &event.kind,
            EventKind::ApprovalResolved {
                by: xi_protocol::event::Approver::Headless,
                ..
            }
        )));
        assert!(attached.pending_approvals.is_empty());
        while let Ok(event) = events.try_recv() {
            if let Incoming::Notification { method, .. } = event {
                assert_ne!(method, "approval/pending");
            }
        }
        stop(&peer, &daemon).await;
    }
}

#[tokio::test]
async fn worker_exit_notifies_attached_clients_and_running_list_watchers() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (first, mut attached_events) = connection(&daemon);
    let (watcher, mut watched_events) = connection(&daemon);
    initialize(&first).await;
    initialize(&watcher).await;
    let session = request::<SessionCreate>(&first, &fixture.create())
        .await
        .unwrap()
        .info
        .session;
    request::<SessionList>(
        &watcher,
        &ListParams {
            cwd: None,
            running: Some(true),
        },
    )
    .await
    .unwrap();
    request::<SessionClose>(&first, &SessionRef { session })
        .await
        .unwrap();
    for events in [&mut attached_events, &mut watched_events] {
        let exited: ExitedParams =
            serde_json::from_value(notification(events, "session/exited").await).unwrap();
        assert_eq!(exited.session, session);
        assert_eq!(exited.status, Some(0));
    }
    assert_eq!(
        request::<DaemonStatus>(&first, &()).await.unwrap().workers,
        0
    );
    let saved = request::<SessionList>(&first, &ListParams::default())
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].session, session);
    assert_eq!(saved[0].head, None);
    assert_eq!(
        error_code(
            request::<SessionAttach>(
                &watcher,
                &AttachParams {
                    session,
                    since: None
                }
            )
            .await
        ),
        RpcError::UNKNOWN_SESSION
    );
    let reopened = request::<SessionOpen>(
        &first,
        &OpenParams {
            path: saved[0].path.clone(),
            options: SessionOptions::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(reopened.info.session, session);
    stop(&first, &daemon).await;
}

#[tokio::test]
async fn private_instance_shuts_down_on_last_client_disconnect() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], true);
    let (peer, _) = connection(&daemon);
    let (second, _) = connection(&daemon);
    initialize(&second).await;
    assert!(initialize(&peer).await.capabilities.private);
    request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap();
    peer.close();
    assert_eq!(
        request::<DaemonStatus>(&second, &()).await.unwrap().workers,
        1
    );
    second.close();
    tokio::time::timeout(Duration::from_secs(8), daemon.finished())
        .await
        .unwrap();
}

#[tokio::test]
async fn private_helper_exposes_initialized_pipe_and_shutdown() {
    let fixture = Fixture::new();
    let (reader, writer) = xi_daemon::private(fixture.paths.clone(), fixture.spawner(vec![]));
    let (peer, _) = Peer::spawn(reader, writer);
    assert!(initialize(&peer).await.capabilities.private);
    request::<DaemonShutdown>(
        &peer,
        &ShutdownParams {
            when: ShutdownWhen::Now,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), peer.closed())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn idle_daemon_exits_without_clients_or_workers() {
    let fixture = Fixture::new();
    let daemon = Daemon::new(DaemonOptions {
        settings: DaemonSettings {
            idle_exit: 1,
            ..DaemonSettings::default()
        },
        paths: fixture.paths.clone(),
        spawner: fixture.spawner(vec![]),
        private: false,
    });
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::timeout(Duration::from_secs(1), daemon.finished())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn detached_idle_worker_closes_without_exiting_connected_daemon() {
    let fixture = Fixture::new();
    let daemon = Daemon::new(DaemonOptions {
        settings: DaemonSettings {
            idle_exit: 1,
            ..DaemonSettings::default()
        },
        paths: fixture.paths.clone(),
        spawner: fixture.spawner(vec![]),
        private: false,
    });
    let (peer, mut events) = connection(&daemon);
    initialize(&peer).await;
    let session = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap()
        .info
        .session;
    request::<SessionList>(
        &peer,
        &ListParams {
            cwd: None,
            running: Some(true),
        },
    )
    .await
    .unwrap();
    request::<SessionDetach>(&peer, &SessionRef { session })
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    let exited: ExitedParams =
        serde_json::from_value(notification(&mut events, "session/exited").await).unwrap();
    assert_eq!(exited.session, session);
    assert_eq!(
        request::<DaemonStatus>(&peer, &()).await.unwrap().workers,
        0
    );
    stop(&peer, &daemon).await;
}

#[tokio::test(start_paused = true)]
async fn stubborn_workers_are_killed_and_closing_sessions_are_not_live() {
    for answers_close in [true, false] {
        for close_path in ["explicit", "idle", "shutdown"] {
            // Keep filesystem work from auto-advancing the paused clock.
            let clock = tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            });
            let fixture = Fixture::new();
            let session = SessionId::generate();
            let (closes, mut closed) = mpsc::unbounded_channel();
            let (kills, mut killed) = mpsc::unbounded_channel();
            let daemon = Daemon::new(DaemonOptions {
                settings: DaemonSettings {
                    idle_exit: 1,
                    ..DaemonSettings::default()
                },
                paths: fixture.paths.clone(),
                spawner: Arc::new(StubbornSpawner {
                    session: AttachResult {
                        info: SessionDetails {
                            session,
                            path: fixture.cwd.join("session.jsonl"),
                            head: EventId::generate(),
                            cwd: fixture.cwd.clone(),
                            trusted: true,
                            model: ModelRef {
                                provider: "test".into(),
                                model: "test".into(),
                            },
                            reasoning: Reasoning::Medium,
                            model_info: None,
                            parent_session: None,
                        },
                        events: Vec::new(),
                        partial: Vec::new(),
                        stream_sequence: 0,
                        running: false,
                        pending_approvals: Vec::new(),
                        children: Vec::new(),
                    },
                    answers_close,
                    closes,
                    kills,
                }),
                private: false,
            });
            let (peer, mut events) = connection(&daemon);
            initialize(&peer).await;
            request::<SessionCreate>(&peer, &fixture.create())
                .await
                .unwrap();
            request::<SessionList>(
                &peer,
                &ListParams {
                    cwd: None,
                    running: Some(true),
                },
            )
            .await
            .unwrap();
            let close_request = match close_path {
                "explicit" => {
                    let peer = peer.clone();
                    Some(tokio::spawn(async move {
                        request::<SessionClose>(&peer, &SessionRef { session }).await
                    }))
                }
                "idle" => {
                    request::<SessionDetach>(&peer, &SessionRef { session })
                        .await
                        .unwrap();
                    tokio::time::advance(Duration::from_secs(1)).await;
                    None
                }
                "shutdown" => {
                    request::<DaemonShutdown>(
                        &peer,
                        &ShutdownParams {
                            when: ShutdownWhen::Now,
                        },
                    )
                    .await
                    .unwrap();
                    None
                }
                _ => unreachable!(),
            };
            let close_sent = closed.recv().await.unwrap();
            assert!(killed.try_recv().is_err());
            let listed = request::<SessionList>(&peer, &ListParams::default())
                .await
                .unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].session, session);
            assert_eq!(listed[0].head, None);
            assert!(!listed[0].running);
            assert_eq!(listed[0].pending_approvals, 0);
            assert_eq!(
                error_code(
                    request::<SessionAttach>(
                        &peer,
                        &AttachParams {
                            session,
                            since: None,
                        },
                    )
                    .await,
                ),
                RpcError::UNKNOWN_SESSION
            );
            tokio::time::advance(Duration::from_secs(4)).await;
            assert!(killed.try_recv().is_err());
            tokio::time::advance(Duration::from_secs(1)).await;
            clock.abort();
            let killed_at = tokio::time::timeout(Duration::from_secs(1), killed.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(killed_at.duration_since(close_sent) <= Duration::from_secs(5));
            let exited: ExitedParams =
                serde_json::from_value(notification(&mut events, "session/exited").await).unwrap();
            assert_eq!(exited.session, session);
            assert_eq!(exited.status, None);
            if let Some(close_request) = close_request {
                let result = close_request.await.unwrap();
                if answers_close {
                    result.unwrap();
                } else {
                    assert_eq!(error_code(result), RpcError::WORKER_EXITED);
                }
            }
            if close_path == "shutdown" {
                daemon.finished().await;
            } else {
                assert_eq!(
                    request::<DaemonStatus>(&peer, &()).await.unwrap().workers,
                    0
                );
                stop(&peer, &daemon).await;
            }
        }
    }
}

#[tokio::test]
async fn session_list_enriches_live_and_saved_sessions_and_orders_by_mtime() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon(vec![], false);
    let (peer, mut events) = connection(&daemon);
    initialize(&peer).await;
    let first = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap();
    request::<SessionPrompt>(
        &peer,
        &PromptParams {
            session: first.info.session,
            content: vec![
                Block::Text {
                    text: " \n  first ".into(),
                },
                Block::Text {
                    text: "prompt  \nnot the title".into(),
                },
            ],
        },
    )
    .await
    .unwrap();
    prompt(&peer, first.info.session).await;
    let second = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap();
    request::<SessionPrompt>(
        &peer,
        &PromptParams {
            session: second.info.session,
            content: vec![Block::Text {
                text: "界".repeat(81),
            }],
        },
    )
    .await
    .unwrap();
    for (path, seconds) in [(&first.info.path, 100), (&second.info.path, 200)] {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }
    let listed = request::<SessionList>(&peer, &ListParams::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].session, second.info.session);
    assert_eq!(listed[1].session, first.info.session);
    assert_eq!(listed[0].updated, Some(Timestamp(200_000)));
    assert_eq!(listed[1].updated, Some(Timestamp(100_000)));
    assert_eq!(listed[0].started, Some(second.events[0].at));
    assert_eq!(listed[1].started, Some(first.events[0].at));
    assert_eq!(listed[0].title, Some(format!("{}…", "界".repeat(80))));
    assert_eq!(listed[1].title.as_deref(), Some("first prompt"));
    for session in [first.info.session, second.info.session] {
        request::<SessionClose>(&peer, &SessionRef { session })
            .await
            .unwrap();
        notification(&mut events, "session/exited").await;
    }
    let saved = request::<SessionList>(&peer, &ListParams::default())
        .await
        .unwrap();
    assert_eq!(saved.len(), 2);
    for (saved, live) in saved.iter().zip(&listed) {
        assert_eq!(saved.session, live.session);
        assert_eq!(saved.head, None);
        assert_eq!(saved.title, live.title);
        assert_eq!(saved.started, live.started);
        assert_eq!(saved.updated, live.updated);
    }
    stop(&peer, &daemon).await;
}

struct ApprovalProbe;

impl xi_tools::Tool for ApprovalProbe {
    fn spec(&self) -> xi_protocol::manifest::ToolSpec {
        xi_protocol::manifest::ToolSpec {
            name: "probe".into(),
            description: "Approval metadata probe".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn crossings(
        &self,
        _: &Value,
        _: &xi_tools::Context,
    ) -> Result<Vec<xi_tools::Crossing>, String> {
        Ok(vec![xi_tools::Crossing::Net("example.org".into())])
    }

    fn run<'a>(
        &'a self,
        _: &'a xi_protocol::ToolCallId,
        input: Value,
        _: &'a xi_tools::Context,
    ) -> BoxFuture<'a, xi_tools::Ran> {
        Box::pin(async move {
            xi_tools::Ran {
                output: xi_protocol::ToolOutput {
                    content: Vec::new(),
                    is_error: false,
                    untrusted: input["untrusted"].as_bool().unwrap_or(false),
                },
                spill: None,
            }
        })
    }
}

#[tokio::test]
async fn pending_approvals_report_taint_and_workspace_trust() {
    let fixture = Fixture::new();
    fixture.paths.trust(&fixture.cwd).unwrap();
    std::fs::write(
        fixture.paths.config.join("config.toml"),
        "[[rule]]\neffect='allow'\naction='net'\npattern='**'\n",
    )
    .unwrap();
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        Response {
            content: vec![
                Block::ToolUse {
                    id: "ingest".into(),
                    name: "probe".into(),
                    input: json!({"untrusted": true}),
                    extra: Default::default(),
                },
                Block::ToolUse {
                    id: "taint-ask".into(),
                    name: "probe".into(),
                    input: json!({}),
                    extra: Default::default(),
                },
            ],
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        },
        done(),
    ]))));
    let spawner = InProcessSpawner {
        paths: fixture.paths.clone(),
        backend: Some(Arc::new(move || Backend {
            provider: script.clone(),
            model: ModelInfo {
                provider: "anthropic".into(),
                id: "claude-sonnet-5-5".into(),
                context_window: 8192,
                max_output: 1024,
                reasoning: Reasoning::ALL.to_vec(),
                pricing: None,
            },
            tools: vec![Arc::new(ApprovalProbe)],
        })),
    };
    let daemon = Daemon::new(DaemonOptions {
        settings: DaemonSettings::default(),
        paths: fixture.paths.clone(),
        spawner: Arc::new(spawner),
        private: false,
    });
    let (peer, mut events) = connection(&daemon);
    initialize(&peer).await;
    let session = request::<SessionCreate>(&peer, &fixture.create())
        .await
        .unwrap()
        .info
        .session;
    let prompting = peer.clone();
    let turn = tokio::spawn(async move { prompt(&prompting, session).await });
    let pending: PendingApproval =
        serde_json::from_value(notification(&mut events, "approval/pending").await).unwrap();
    assert_eq!(pending.call.0, "taint-ask");
    assert!(pending.taint);
    assert!(pending.trusted);
    assert_eq!(
        request::<ApprovalList>(&peer, &()).await.unwrap(),
        vec![pending.clone()]
    );
    let attached = request::<SessionAttach>(
        &peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(attached.pending_approvals, vec![pending.clone()]);
    assert_eq!(
        error_code(
            request::<ApprovalResolve>(
                &peer,
                &ResolveParams {
                    session,
                    call: pending.call.clone(),
                    approved: true,
                    remember: Remember::Session,
                }
            )
            .await
        ),
        RpcError::INVALID_PARAMS,
    );
    request::<ApprovalResolve>(
        &peer,
        &ResolveParams {
            session,
            call: pending.call,
            approved: false,
            remember: Remember::Once,
        },
    )
    .await
    .unwrap();
    turn.await.unwrap();
    stop(&peer, &daemon).await;
}
