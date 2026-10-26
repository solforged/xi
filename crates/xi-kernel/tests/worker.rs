//! Worker pipe behavior with an injected provider and a daemon-side peer.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use xi_client::peer::{Incoming, Peer, PeerError};
use xi_kernel::Backend;
use xi_kernel::config::Paths;
use xi_kernel::worker::{WorkerSetup, run};
use xi_protocol::event::{ModelRef, StopReason, Usage};
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::client::{
    ApprovalResolve, AttachParams, AttachResult, CreateParams, Delta, DeltaParams, EventParams,
    OpenParams, PromptParams, ResetParams, SessionAttach, SessionCancel, SessionClose,
    SessionOptions, SessionPrompt, SessionRef,
};
use xi_protocol::methods::worker::{LeaseParams, WorkerStart, WorkerStartParams};
use xi_protocol::model::ModelInfo;
use xi_protocol::rpc::{RequestMethod, RpcError};
use xi_protocol::{Block, Decision, EventKind, OutputBlock, SessionId, ToolCallId, ToolOutput};
use xi_provider::{BoxFuture, Error, ErrorKind, Provider, Request, Response};
use xi_tools::{Context, Crossing, Ran, Tool};

const ENDPOINT: &str = "http://worker.test/v1";

struct Step {
    text: String,
    gate: Option<Arc<Semaphore>>,
    error: Option<Error>,
}

struct Script {
    steps: Mutex<VecDeque<Step>>,
    calls: AtomicUsize,
}

impl Script {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            calls: AtomicUsize::new(0),
        })
    }
}

impl Provider for Script {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let step = self
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted");
            on_delta(Delta::Text {
                text: step.text.clone(),
            });
            if let Some(gate) = step.gate {
                gate.acquire().await.unwrap().forget();
            }
            if let Some(error) = step.error {
                return Err(error);
            }
            Ok(Response {
                content: text(&step.text),
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            })
        })
    }
}

struct RetryScript {
    calls: AtomicUsize,
    gate: Semaphore,
}

impl Provider for RetryScript {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let content = if attempt == 0 { "discarded" } else { "current" };
            on_delta(Delta::Text {
                text: content.into(),
            });
            if attempt == 0 {
                return Err(Error {
                    kind: ErrorKind::Unavailable,
                    message: "retry".into(),
                    retry_after: Some(Duration::ZERO),
                });
            }
            self.gate.acquire().await.unwrap().forget();
            Ok(Response {
                content: text(content),
                stop: StopReason::EndTurn,
                usage: Usage::default(),
            })
        })
    }
}

struct Fixture {
    _dir: TempDir,
    paths: Paths,
    cwd: PathBuf,
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
        let cwd = root.join("workspace");
        for path in [&paths.home, &paths.config, &cwd] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            paths.config.join("config.toml"),
            format!(
                "default_model = 'test/script'\ntools = []\n\
                 [provider.test]\napi = 'openai-chat'\nbase_url = '{ENDPOINT}'\n\
                 [[model]]\nprovider = 'test'\nid = 'script'\n\
                 context_window = 8192\nmax_output = 1024\nreasoning = false\n"
            ),
        )
        .unwrap();
        Self {
            _dir: dir,
            paths,
            cwd,
        }
    }

    fn worker(&self, script: Arc<dyn Provider>) -> Harness {
        self.worker_with_tools(script, xi_tools::toolset(&[] as &[&str]).unwrap())
    }

    fn worker_with_tools(&self, script: Arc<dyn Provider>, tools: Vec<Arc<dyn Tool>>) -> Harness {
        let (daemon, worker) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(worker);
        let task = tokio::spawn(run(
            reader,
            writer,
            WorkerSetup {
                paths: self.paths.clone(),
                backend: Some(Backend {
                    provider: script,
                    model: model(),
                    tools,
                }),
            },
        ));
        let (reader, writer) = tokio::io::split(daemon);
        let (peer, incoming) = Peer::spawn(reader, writer);
        Harness {
            peer,
            incoming,
            task,
        }
    }

    fn create(&self) -> WorkerStartParams {
        WorkerStartParams::Create(CreateParams {
            cwd: self.cwd.clone(),
            model: None,
            options: SessionOptions::default(),
        })
    }
}

struct Harness {
    peer: Peer,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    task: JoinHandle<xi_kernel::Result<()>>,
}

impl Harness {
    async fn start(&self, fixture: &Fixture) -> AttachResult {
        request::<WorkerStart>(&self.peer, &fixture.create())
            .await
            .unwrap()
    }

    async fn next(&mut self) -> Incoming {
        tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
            .await
            .unwrap()
            .expect("worker pipe ended")
    }

    async fn lease(&mut self) -> Vec<EventParams> {
        let mut events = Vec::new();
        loop {
            match self.next().await {
                Incoming::Request { id, method, params } => {
                    assert_eq!(method, "model/lease");
                    assert_eq!(
                        serde_json::from_value::<LeaseParams>(params)
                            .unwrap()
                            .endpoint,
                        ENDPOINT
                    );
                    self.peer.respond(id, Ok(Value::Null)).unwrap();
                    return events;
                }
                Incoming::Notification { method, params } => {
                    assert_eq!(method, "event/appended");
                    events.push(serde_json::from_value(params).unwrap());
                }
            }
        }
    }

    async fn finish(self) {
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    async fn close(self, session: SessionId) {
        request::<SessionClose>(&self.peer, &SessionRef { session })
            .await
            .unwrap();
        self.finish().await;
    }
}

async fn request<M: RequestMethod>(
    peer: &Peer,
    params: &M::Params,
) -> Result<M::Result, PeerError> {
    tokio::time::timeout(Duration::from_secs(5), peer.request::<M>(params))
        .await
        .unwrap()
}

fn assert_code<T: std::fmt::Debug>(result: Result<T, PeerError>, code: i64) {
    let error = match result.unwrap_err() {
        PeerError::Rpc(error) => error,
        other => panic!("expected RPC error, got {other:?}"),
    };
    assert_eq!(error.code, code);
}

fn model() -> ModelInfo {
    ModelInfo {
        provider: "test".into(),
        id: "script".into(),
        context_window: 8192,
        max_output: 1024,
        reasoning: false,
        pricing: None,
    }
}

fn text(value: &str) -> Vec<Block> {
    vec![Block::Text { text: value.into() }]
}

fn reply(value: &str) -> Step {
    Step {
        text: value.into(),
        gate: None,
        error: None,
    }
}

#[tokio::test]
async fn requires_start_and_create_returns_root_and_config_details() {
    let fixture = Fixture::new();
    let harness = fixture.worker(Script::new(vec![]));
    for method in [
        "session/attach",
        "session/prompt",
        "session/cancel",
        "unknown",
    ] {
        assert_code(
            harness.peer.request_value(method, json!({})).await,
            RpcError::NOT_INITIALIZED,
        );
    }
    let attached = harness.start(&fixture).await;
    assert_eq!(attached.events.len(), 1);
    assert_eq!(attached.info.head, attached.events[0].id);
    assert_eq!(attached.info.cwd, fixture.cwd);
    assert_eq!(
        attached.info.model,
        ModelRef {
            provider: "test".into(),
            model: "script".into()
        }
    );
    assert_eq!(attached.info.model_info, Some(model()));
    assert!(!attached.info.trusted);
    assert!(attached.info.parent_session.is_none());
    assert!(attached.info.path.is_absolute());
    assert!(attached.partial.is_empty());
    assert_eq!(attached.stream_sequence, 0);
    assert!(!attached.running);
    assert!(attached.pending_approvals.is_empty());
    assert!(matches!(
        attached.events[0].kind,
        EventKind::SessionStarted { .. }
    ));
    harness.close(attached.info.session).await;
}

#[tokio::test]
async fn prompt_leases_before_provider_streams_releases_and_replays_since() {
    let fixture = Fixture::new();
    let script = Script::new(vec![reply("answer")]);
    let mut harness = fixture.worker(script.clone());
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
        .unwrap()
    });
    let mut events = Vec::new();
    loop {
        match harness.next().await {
            Incoming::Request { id, method, params } => {
                assert_eq!(method, "model/lease");
                assert_eq!(script.calls.load(Ordering::SeqCst), 0);
                assert_eq!(
                    serde_json::from_value::<LeaseParams>(params)
                        .unwrap()
                        .endpoint,
                    ENDPOINT
                );
                harness.peer.respond(id, Ok(Value::Null)).unwrap();
                break;
            }
            Incoming::Notification { method, params } => {
                assert_eq!(method, "event/appended");
                events.push(serde_json::from_value::<EventParams>(params).unwrap().event);
            }
        }
    }
    let mut released = false;
    let mut delta = false;
    let mut assistant = false;
    while !released || !delta || !assistant {
        let Incoming::Notification { method, params } = harness.next().await else {
            panic!("unexpected second model lease")
        };
        match method.as_str() {
            "event/appended" => {
                let event = serde_json::from_value::<EventParams>(params).unwrap().event;
                assistant |= matches!(event.kind, EventKind::AssistantMessage { .. });
                events.push(event);
            }
            "stream/delta" => {
                let params: DeltaParams = serde_json::from_value(params).unwrap();
                assert_eq!(params.session, session);
                assert_eq!(params.attempt, 0);
                assert_eq!(params.sequence, 1);
                assert_eq!(
                    params.delta,
                    Delta::Text {
                        text: "answer".into()
                    }
                );
                delta = true;
            }
            "model/release" => {
                assert_eq!(
                    serde_json::from_value::<LeaseParams>(params)
                        .unwrap()
                        .endpoint,
                    ENDPOINT
                );
                released = true;
            }
            other => panic!("unexpected notification {other}"),
        }
    }
    let outcome = prompt.await.unwrap();
    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.head, events.last().unwrap().id);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, EventKind::UserMessage { .. }))
    );
    let replay = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: Some(attached.info.head),
        },
    )
    .await
    .unwrap();
    assert_eq!(replay.info.head, outcome.head);
    assert_eq!(replay.events, events[1..]);
    assert!(replay.partial.is_empty());
    let latest = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: Some(outcome.head),
        },
    )
    .await
    .unwrap();
    assert!(latest.events.is_empty());
    harness.close(session).await;
}

#[tokio::test]
async fn attach_partial_and_cancel_stay_responsive_during_prompt() {
    let fixture = Fixture::new();
    let script = Script::new(vec![Step {
        text: "partial".into(),
        gate: Some(Arc::new(Semaphore::new(0))),
        error: None,
    }]);
    let mut harness = fixture.worker(script);
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    loop {
        if let Incoming::Notification { method, .. } = harness.next().await
            && method == "stream/delta"
        {
            break;
        }
    }
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert!(snapshot.running);
    assert_eq!(snapshot.partial, text("partial"));
    assert_code(
        request::<SessionClose>(&harness.peer, &SessionRef { session }).await,
        RpcError::SESSION_RUNNING,
    );
    assert_code(
        request::<ApprovalResolve>(
            &harness.peer,
            &xi_protocol::methods::client::ResolveParams {
                session,
                call: "unknown".into(),
                approved: true,
            },
        )
        .await,
        RpcError::APPROVAL_NOT_PENDING,
    );
    request::<SessionCancel>(&harness.peer, &SessionRef { session })
        .await
        .unwrap();
    let outcome = prompt.await.unwrap();
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(snapshot.info.head, outcome.head);
    assert!(snapshot.partial.is_empty());
    assert!(!snapshot.running);
    harness.close(session).await;
}

#[tokio::test]
async fn failed_provider_discards_partial_before_attach_and_the_next_turn() {
    let fixture = Fixture::new();
    let script = Script::new(vec![
        Step {
            error: Some(Error::new(ErrorKind::InvalidRequest, "provider failed")),
            ..reply("discarded")
        },
        Step {
            gate: Some(Arc::new(Semaphore::new(0))),
            ..reply("current")
        },
    ]);
    let mut harness = fixture.worker(script);
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("first"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    let mut released = false;
    let mut failed = false;
    let mut first_sequence = 0;
    while !released || !failed {
        let Incoming::Notification { method, params } = harness.next().await else {
            panic!("unexpected second model lease")
        };
        match method.as_str() {
            "model/release" => released = true,
            "event/appended" => {
                let params: EventParams = serde_json::from_value(params).unwrap();
                failed |= matches!(params.event.kind, EventKind::TurnFailed { .. });
            }
            "stream/delta" => {
                let params: DeltaParams = serde_json::from_value(params).unwrap();
                assert_eq!(
                    params.delta,
                    Delta::Text {
                        text: "discarded".into(),
                    },
                );
                first_sequence = params.sequence;
            }
            other => panic!("unexpected notification {other}"),
        }
    }
    assert!(prompt.await.unwrap().failed);
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert!(!snapshot.running);
    assert!(snapshot.partial.is_empty());
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| !matches!(event.kind, EventKind::AssistantMessage { .. },))
    );
    assert!(first_sequence > 0);
    assert_eq!(snapshot.stream_sequence, first_sequence);
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("second"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    loop {
        if let Incoming::Notification { method, .. } = harness.next().await
            && method == "stream/delta"
        {
            break;
        }
    }
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert!(snapshot.running);
    assert_eq!(snapshot.partial, text("current"));
    assert!(snapshot.stream_sequence > first_sequence);
    request::<SessionCancel>(&harness.peer, &SessionRef { session })
        .await
        .unwrap();
    assert!(prompt.await.unwrap().cancelled);
    harness.close(session).await;
}

#[tokio::test]
async fn close_rejects_steering_retained_after_cancellation() {
    let fixture = Fixture::new();
    let mut harness = fixture.worker(Script::new(vec![Step {
        gate: Some(Arc::new(Semaphore::new(0))),
        ..reply("partial")
    }]));
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("first"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    loop {
        if let Incoming::Notification { method, .. } = harness.next().await
            && method == "stream/delta"
        {
            break;
        }
    }
    let params = PromptParams {
        session,
        content: text("steering"),
    };
    let peer = harness.peer.clone();
    let steering = request::<SessionPrompt>(&peer, &params);
    tokio::pin!(steering);
    let attach = AttachParams {
        session,
        since: None,
    };
    let session_ref = SessionRef { session };
    tokio::select! {
        biased;
        result = &mut steering => panic!("steering completed before cancellation: {result:?}"),
        snapshot = request::<SessionAttach>(&harness.peer, &attach) => {
            assert!(snapshot.unwrap().running);
        }
    }
    request::<SessionCancel>(&harness.peer, &session_ref)
        .await
        .unwrap();
    assert!(prompt.await.unwrap().cancelled);
    let (closed, steering_result) = tokio::join!(
        request::<SessionClose>(&harness.peer, &session_ref),
        &mut steering,
    );
    closed.unwrap();
    assert_code(steering_result, RpcError::INTERNAL_ERROR);
    harness.finish().await;
}

#[tokio::test]
async fn rejects_each_relative_session_option_path() {
    let fixture = Fixture::new();
    let harness = fixture.worker(Script::new(vec![]));
    let relative = PathBuf::from("relative");
    for options in [
        SessionOptions {
            configs: vec![relative.clone()],
            ..SessionOptions::default()
        },
        SessionOptions {
            session_dir: Some(relative.clone()),
            ..SessionOptions::default()
        },
        SessionOptions {
            extensions: vec![relative],
            ..SessionOptions::default()
        },
    ] {
        let params = WorkerStartParams::Create(CreateParams {
            cwd: fixture.cwd.clone(),
            model: None,
            options,
        });
        assert_code(
            request::<WorkerStart>(&harness.peer, &params).await,
            RpcError::INVALID_PARAMS,
        );
    }
    let attached = harness.start(&fixture).await;
    harness.close(attached.info.session).await;
}

#[tokio::test]
async fn rejects_relative_create_cwd_and_open_path() {
    let fixture = Fixture::new();
    let harness = fixture.worker(Script::new(vec![]));
    for params in [
        WorkerStartParams::Create(CreateParams {
            cwd: ".".into(),
            model: None,
            options: SessionOptions::default(),
        }),
        WorkerStartParams::Open(OpenParams {
            path: "missing-session.jsonl".into(),
            options: SessionOptions::default(),
        }),
    ] {
        assert_code(
            request::<WorkerStart>(&harness.peer, &params).await,
            RpcError::INVALID_PARAMS,
        );
    }
    let attached = harness.start(&fixture).await;
    harness.close(attached.info.session).await;
}

#[tokio::test]
async fn open_replays_the_existing_log() {
    let fixture = Fixture::new();
    let harness = fixture.worker(Script::new(vec![]));
    let created = harness.start(&fixture).await;
    harness.close(created.info.session).await;
    let harness = fixture.worker(Script::new(vec![]));
    let reopened = request::<WorkerStart>(
        &harness.peer,
        &WorkerStartParams::Open(OpenParams {
            path: created.info.path.clone(),
            options: SessionOptions::default(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(reopened, created);
    harness.close(reopened.info.session).await;
}

#[tokio::test]
async fn eof_ends_an_idle_worker() {
    let fixture = Fixture::new();
    let harness = fixture.worker(Script::new(vec![]));
    harness.start(&fixture).await;
    harness.peer.close();
    harness.finish().await;
}

#[tokio::test]
async fn eof_stops_an_in_flight_model_request_and_records_failure() {
    let fixture = Fixture::new();
    let mut harness = fixture.worker(Script::new(vec![Step {
        text: "partial".into(),
        gate: Some(Arc::new(Semaphore::new(0))),
        error: None,
    }]));
    let attached = harness.start(&fixture).await;
    let peer = harness.peer.clone();
    let session = attached.info.session;
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
    });
    harness.lease().await;
    loop {
        if let Incoming::Notification { method, .. } = harness.next().await
            && method == "stream/delta"
        {
            break;
        }
    }
    harness.peer.close();
    harness.finish().await;
    assert!(matches!(prompt.await.unwrap(), Err(PeerError::Closed)));
    let log = xi_session::SessionLog::open(attached.info.path).unwrap();
    assert!(log.events().iter().any(|event| matches!(
        &event.kind, EventKind::TurnFailed { message } if message == "daemon connection lost"
    )));
}

#[tokio::test]
async fn cancellation_while_queued_releases_a_late_lease_grant() {
    let fixture = Fixture::new();
    let script = Script::new(vec![]);
    let mut harness = fixture.worker(script.clone());
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
        .unwrap()
    });
    let lease = loop {
        if let Incoming::Request { id, method, .. } = harness.next().await {
            assert_eq!(method, "model/lease");
            break id;
        }
    };
    request::<SessionCancel>(&harness.peer, &SessionRef { session })
        .await
        .unwrap();
    prompt.await.unwrap();
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    harness.peer.respond(lease, Ok(Value::Null)).unwrap();
    loop {
        if let Incoming::Notification { method, params } = harness.next().await
            && method == "model/release"
        {
            assert_eq!(
                serde_json::from_value::<LeaseParams>(params)
                    .unwrap()
                    .endpoint,
                ENDPOINT
            );
            break;
        }
    }
    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    harness.close(session).await;
}

#[tokio::test]
async fn retry_resets_partial_output_and_keeps_one_model_lease() {
    let fixture = Fixture::new();
    let script = Arc::new(RetryScript {
        calls: AtomicUsize::new(0),
        gate: Semaphore::new(0),
    });
    let mut harness = fixture.worker(script.clone());
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    let mut reset = false;
    loop {
        let Incoming::Notification { method, params } = harness.next().await else {
            panic!("retry acquired another lease")
        };
        match method.as_str() {
            "stream/reset" => {
                let params: ResetParams = serde_json::from_value(params).unwrap();
                assert_eq!(params.session, session);
                assert_eq!(params.attempt, 1);
                assert_eq!(params.sequence, 2);
                reset = true;
            }
            "stream/delta" => {
                let params: DeltaParams = serde_json::from_value(params).unwrap();
                if params.attempt == 1 {
                    assert_eq!(params.sequence, 3);
                    assert!(reset);
                    assert_eq!(
                        params.delta,
                        Delta::Text {
                            text: "current".into()
                        }
                    );
                    break;
                }
                assert_eq!(params.attempt, 0);
                assert_eq!(params.sequence, 1);
            }
            "event/appended" => {}
            other => panic!("unexpected notification {other}"),
        }
    }
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(snapshot.partial, text("current"));
    assert_eq!(snapshot.stream_sequence, 3);
    assert!(snapshot.running);
    script.gate.add_permits(1);
    prompt.await.unwrap();
    loop {
        let Incoming::Notification { method, .. } = harness.next().await else {
            panic!("retry acquired another lease")
        };
        if method == "model/release" {
            break;
        }
    }
    assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    harness.close(session).await;
}

struct ToolScript(AtomicUsize);

impl Provider for ToolScript {
    fn stream<'a>(
        &'a self,
        _: &'a Request,
        on_delta: &'a mut (dyn FnMut(Delta) + Send),
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
            let (content, stop) = if first {
                (
                    vec![Block::ToolUse {
                        id: "ask".into(),
                        name: "fake".into(),
                        input: json!({}),
                    }],
                    StopReason::ToolUse,
                )
            } else {
                on_delta(Delta::Text {
                    text: "done".into(),
                });
                (text("done"), StopReason::EndTurn)
            };
            Ok(Response {
                content,
                stop,
                usage: Usage::default(),
            })
        })
    }
}

struct ApprovalTool;

impl Tool for ApprovalTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fake".into(),
            description: "Approval test tool".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn crossings(&self, _: &Value, _: &Context) -> Result<Vec<Crossing>, String> {
        Ok(vec![Crossing::Net("example.org".into())])
    }

    fn run<'a>(&'a self, _: &'a ToolCallId, _: Value, _: &'a Context) -> BoxFuture<'a, Ran> {
        Box::pin(async {
            Ran {
                output: ToolOutput {
                    content: vec![OutputBlock::Text {
                        text: "result".into(),
                    }],
                    is_error: false,
                    untrusted: false,
                },
                spill: None,
            }
        })
    }
}

#[tokio::test]
async fn attach_mirrors_pending_approvals_and_resolve_stays_responsive() {
    let fixture = Fixture::new();
    let mut harness = fixture.worker_with_tools(
        Arc::new(ToolScript(AtomicUsize::new(0))),
        vec![Arc::new(ApprovalTool)],
    );
    let attached = harness.start(&fixture).await;
    let session = attached.info.session;
    let peer = harness.peer.clone();
    let prompt = tokio::spawn(async move {
        request::<SessionPrompt>(
            &peer,
            &PromptParams {
                session,
                content: text("hello"),
            },
        )
        .await
        .unwrap()
    });
    harness.lease().await;
    loop {
        if let Incoming::Notification { method, params } = harness.next().await
            && method == "event/appended"
        {
            let params: EventParams = serde_json::from_value(params).unwrap();
            if matches!(
                params.event.kind,
                EventKind::ToolDecision {
                    decision: Decision::Ask,
                    ..
                }
            ) {
                break;
            }
        }
    }
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert!(snapshot.running);
    assert_eq!(snapshot.pending_approvals.len(), 1);
    assert_eq!(snapshot.pending_approvals[0].session, session);
    assert_eq!(snapshot.pending_approvals[0].call, ToolCallId::from("ask"));
    assert_eq!(snapshot.pending_approvals[0].tool, "fake");
    let resolve = xi_protocol::methods::client::ResolveParams {
        session,
        call: "ask".into(),
        approved: true,
    };
    request::<ApprovalResolve>(&harness.peer, &resolve)
        .await
        .unwrap();
    assert_code(
        request::<ApprovalResolve>(&harness.peer, &resolve).await,
        RpcError::APPROVAL_NOT_PENDING,
    );
    loop {
        if let Incoming::Request { id, method, .. } = harness.next().await {
            assert_eq!(method, "model/lease");
            harness.peer.respond(id, Ok(Value::Null)).unwrap();
            break;
        }
    }
    prompt.await.unwrap();
    let snapshot = request::<SessionAttach>(
        &harness.peer,
        &AttachParams {
            session,
            since: None,
        },
    )
    .await
    .unwrap();
    assert!(!snapshot.running);
    assert!(snapshot.pending_approvals.is_empty());
    assert!(snapshot.events.iter().any(|event| matches!(
        event.kind,
        EventKind::ApprovalResolved { approved: true, .. }
    )));
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::ToolResult { .. }))
    );
    harness.close(session).await;
}
