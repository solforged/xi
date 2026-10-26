//! Typed daemon requests, initialization and notification routing over duplex pipes.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{duplex, split};
use tokio::sync::mpsc;
use tokio::time::timeout;
use xi_client::client::{Client, ClientError, ClientNotification};
use xi_client::peer::{Incoming, Peer, PeerError};
use xi_protocol::content::Block;
use xi_protocol::event::{Event, EventKind, ModelRef, Remember};
use xi_protocol::ids::{EventId, SessionId, Timestamp};
use xi_protocol::manifest::CommandSpec;
use xi_protocol::methods::client::*;
use xi_protocol::methods::extension::CommandResult;
use xi_protocol::model::{ModelInfo, Reasoning};
use xi_protocol::rpc::{RequestId, RequestMethod, RpcError};

async fn request(incoming: &mut mpsc::UnboundedReceiver<Incoming>) -> (RequestId, String, Value) {
    let Incoming::Request { id, method, params } = timeout(Duration::from_secs(1), incoming.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected a request");
    };
    (id, method, params)
}

async fn answer<M: RequestMethod>(
    peer: &Peer,
    incoming: &mut mpsc::UnboundedReceiver<Incoming>,
    expected: &M::Params,
    result: &M::Result,
) {
    let (id, method, params) = request(incoming).await;
    assert_eq!(method, M::METHOD);
    assert_eq!(params, serde_json::to_value(expected).unwrap());
    peer.respond(id, Ok(serde_json::to_value(result).unwrap()))
        .unwrap();
}

fn initialized() -> InitializeResult {
    InitializeResult {
        protocol_version: xi_protocol::PROTOCOL_VERSION,
        daemon_version: "fake-daemon".into(),
        capabilities: Capabilities { private: true },
    }
}

async fn connected() -> (
    Client,
    mpsc::UnboundedReceiver<ClientNotification>,
    Peer,
    mpsc::UnboundedReceiver<Incoming>,
) {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(right);
    let (daemon, mut incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(left);
    let expected = InitializeParams {
        protocol_version: xi_protocol::PROTOCOL_VERSION,
        client: ClientInfo {
            name: "test-client".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
    };
    let result = initialized();
    let (client, ()) = tokio::join!(
        Client::over(reader, writer, "test-client"),
        answer::<Initialize>(&daemon, &mut incoming, &expected, &result),
    );
    let (client, notifications) = client.unwrap();
    assert_eq!(client.initialized(), &result);
    (client, notifications, daemon, incoming)
}

fn snapshot() -> AttachResult {
    AttachResult {
        info: SessionDetails {
            session: SessionId::generate(),
            path: "/sessions/test.jsonl".into(),
            head: EventId::generate(),
            cwd: "/workspace".into(),
            trusted: true,
            model: ModelRef {
                provider: "test".into(),
                model: "model".into(),
            },
            reasoning: Reasoning::Medium,
            model_info: None,
            parent_session: None,
        },
        events: Vec::new(),
        partial: Vec::new(),
        running: false,
        pending_approvals: Vec::new(),
        children: Vec::new(),
        stream_sequence: 0,
    }
}

macro_rules! exchange {
    ($call:expr, $marker:ty, $peer:expr, $incoming:expr, $params:expr, $result:expr) => {{
        let call = $call;
        let params = $params;
        let expected = $result;
        let (received, ()) = tokio::join!(
            call,
            answer::<$marker>($peer, $incoming, &params, &expected),
        );
        assert_eq!(received.unwrap(), expected);
    }};
}

#[tokio::test]
async fn initializes_first_and_sends_typed_create_prompt_and_approval_requests() {
    let (client, _notifications, daemon, mut incoming) = connected().await;
    let attached = snapshot();
    let session = attached.info.session;
    let create = CreateParams {
        cwd: attached.info.cwd.clone(),
        model: Some(attached.info.model.clone()),
        options: SessionOptions {
            max_turns: Some(12),
            ..Default::default()
        },
    };
    exchange!(
        client.create(create.clone()),
        SessionCreate,
        &daemon,
        &mut incoming,
        create,
        attached
    );

    let content = vec![Block::Text {
        text: "hello".into(),
    }];
    let head = EventId::generate();
    exchange!(
        client.prompt(session, content.clone()),
        SessionPrompt,
        &daemon,
        &mut incoming,
        PromptParams { session, content },
        PromptResult {
            head,
            failed: false,
            cancelled: false,
            text: "done".into(),
        }
    );
    let pending = PendingApproval {
        session,
        call: "call-1".into(),
        tool: "bash".into(),
        action: "git status".into(),
        rule: Some("user#1".into()),
        taint: false,
        trusted: true,
    };
    exchange!(
        client.approvals(),
        ApprovalList,
        &daemon,
        &mut incoming,
        (),
        vec![pending.clone()]
    );
    exchange!(
        client.resolve(session, pending.call.clone(), true, Remember::Session),
        ApprovalResolve,
        &daemon,
        &mut incoming,
        ResolveParams {
            session,
            call: pending.call,
            approved: true,
            remember: Remember::Session,
        },
        ()
    );
}

#[tokio::test]
async fn routes_remaining_typed_requests() {
    let (client, _notifications, daemon, mut incoming) = connected().await;
    let attached = snapshot();
    let session = attached.info.session;
    let head = attached.info.head;
    let open = OpenParams {
        path: attached.info.path.clone(),
        options: Default::default(),
    };
    exchange!(
        client.open(open.clone()),
        SessionOpen,
        &daemon,
        &mut incoming,
        open,
        attached.clone()
    );
    exchange!(
        client.attach(session, Some(head)),
        SessionAttach,
        &daemon,
        &mut incoming,
        AttachParams {
            session,
            since: Some(head)
        },
        attached.clone()
    );
    exchange!(
        client.detach(session),
        SessionDetach,
        &daemon,
        &mut incoming,
        SessionRef { session },
        ()
    );
    let filters = ListParams {
        cwd: Some(attached.info.cwd.clone()),
        running: Some(true),
    };
    let summary = SessionSummary {
        session,
        path: attached.info.path,
        cwd: attached.info.cwd,
        model: attached.info.model.clone(),
        head: Some(head),
        running: true,
        pending_approvals: 1,
        parent_session: None,
        started: Some(Timestamp(1)),
        updated: Some(Timestamp(2)),
        title: Some("first prompt".into()),
    };
    exchange!(
        client.list(filters.clone()),
        SessionList,
        &daemon,
        &mut incoming,
        filters,
        vec![summary]
    );
    exchange!(
        client.close(session),
        SessionClose,
        &daemon,
        &mut incoming,
        SessionRef { session },
        ()
    );
    exchange!(
        client.cancel(session),
        SessionCancel,
        &daemon,
        &mut incoming,
        SessionRef { session },
        ()
    );
    exchange!(
        client.checkout(session, head),
        SessionCheckout,
        &daemon,
        &mut incoming,
        CheckoutParams {
            session,
            event: head
        },
        ()
    );
    exchange!(
        client.compact(session),
        SessionCompact,
        &daemon,
        &mut incoming,
        SessionRef { session },
        CompactResult {
            head,
            tokens_before: 100
        }
    );
    let model = attached.info.model;
    exchange!(
        client.set_model(session, model.clone()),
        SessionSetModel,
        &daemon,
        &mut incoming,
        SetModelParams { session, model },
        ()
    );
    exchange!(
        client.set_reasoning(session, Reasoning::High),
        SessionSetReasoning,
        &daemon,
        &mut incoming,
        SetReasoningParams {
            session,
            reasoning: Reasoning::High
        },
        ()
    );
    exchange!(
        client.models(session),
        SessionModels,
        &daemon,
        &mut incoming,
        SessionRef { session },
        ModelsResult {
            current: ModelRef {
                provider: "test".into(),
                model: "model".into()
            },
            reasoning: Reasoning::High,
            models: vec![ModelEntry {
                model: ModelRef {
                    provider: "test".into(),
                    model: "model".into()
                },
                info: ModelInfo {
                    provider: "test".into(),
                    id: "model".into(),
                    context_window: 1000,
                    max_output: 100,
                    reasoning: Reasoning::ALL.to_vec(),
                    pricing: None,
                },
                available: false,
            }],
        }
    );
    exchange!(
        client.commands(session),
        SessionCommands,
        &daemon,
        &mut incoming,
        SessionRef { session },
        vec![CommandSpec {
            name: "hello".into(),
            description: "Greet.".into()
        }]
    );
    exchange!(
        client.command(session, "hello", "world"),
        SessionCommand,
        &daemon,
        &mut incoming,
        SessionCommandParams {
            session,
            name: "hello".into(),
            args: "world".into()
        },
        Some(CommandResult {
            message: Some("greeting".into()),
        })
    );
    exchange!(
        client.command(session, "unknown", ""),
        SessionCommand,
        &daemon,
        &mut incoming,
        SessionCommandParams {
            session,
            name: "unknown".into(),
            args: String::new()
        },
        None
    );
    exchange!(
        client.status(),
        DaemonStatus,
        &daemon,
        &mut incoming,
        (),
        StatusResult {
            version: "fake-daemon".into(),
            pid: 42,
            uptime_secs: 9,
            workers: 2,
            clients: 1,
            leases: vec![LeaseStatus {
                endpoint: "http://localhost".into(),
                concurrency: 2,
                active: 1,
                waiting: 3
            }],
        }
    );
    exchange!(
        client.shutdown(ShutdownWhen::Now),
        DaemonShutdown,
        &daemon,
        &mut incoming,
        ShutdownParams {
            when: ShutdownWhen::Now
        },
        ()
    );
}

#[tokio::test]
async fn rpc_errors_preserve_code_message_and_data() {
    let (client, _notifications, daemon, mut incoming) = connected().await;
    let data = json!({"session": SessionId::generate()});
    let (result, ()) = tokio::join!(client.approvals(), async {
        let (id, method, _) = request(&mut incoming).await;
        assert_eq!(method, ApprovalList::METHOD);
        daemon
            .respond(
                id,
                Err(RpcError::with_data(
                    RpcError::SESSION_HELD,
                    "held",
                    data.clone(),
                )),
            )
            .unwrap();
    });
    let Err(ClientError::Rpc {
        code,
        message,
        data: received,
    }) = result
    else {
        panic!("expected a typed RPC error");
    };
    assert_eq!(code, RpcError::SESSION_HELD);
    assert_eq!(message, "held");
    assert_eq!(received, Some(data));
}

#[tokio::test]
async fn initialization_errors_are_typed() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(right);
    let (daemon, mut incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(left);
    let (result, ()) = tokio::join!(Client::over(reader, writer, "test"), async {
        let (id, method, _) = request(&mut incoming).await;
        assert_eq!(method, Initialize::METHOD);
        daemon
            .respond(
                id,
                Err(RpcError::with_data(
                    RpcError::VERSION_MISMATCH,
                    "version mismatch",
                    json!({"daemon_protocol_version": 1}),
                )),
            )
            .unwrap();
    });
    assert!(matches!(
        result,
        Err(ClientError::Rpc {
            code: RpcError::VERSION_MISMATCH,
            data: Some(_),
            ..
        })
    ));
}

#[tokio::test]
async fn notifications_are_typed_and_unknown_methods_are_ignored() {
    let (_client, mut notifications, daemon, _incoming) = connected().await;
    let session = SessionId::generate();
    let event = EventParams {
        session,
        event: Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp(7),
            kind: EventKind::UserMessage {
                content: vec![Block::Text {
                    text: "hello".into(),
                }],
            },
        },
    };
    let delta = DeltaParams {
        session,
        delta: Delta::Text {
            text: "partial".into(),
        },
        attempt: 0,
        sequence: 1,
    };
    let reset = ResetParams {
        session,
        attempt: 1,
        sequence: 2,
    };
    let exited = ExitedParams {
        session,
        status: Some(0),
        message: None,
    };
    let pending = PendingApproval {
        session,
        call: "call-1".into(),
        tool: "bash".into(),
        action: "git status".into(),
        rule: None,
        taint: true,
        trusted: false,
    };
    daemon
        .notify_value("unknown/notification", json!({"anything": true}))
        .unwrap();
    daemon.notify::<EventAppended>(&event).unwrap();
    daemon.notify::<StreamDelta>(&delta).unwrap();
    daemon.notify::<StreamReset>(&reset).unwrap();
    daemon.notify::<SessionExited>(&exited).unwrap();
    daemon.notify::<ApprovalPending>(&pending).unwrap();
    for expected in [
        ClientNotification::Event(event),
        ClientNotification::Delta(delta),
        ClientNotification::Reset(reset),
        ClientNotification::Exited(exited),
        ClientNotification::ApprovalPending(pending),
    ] {
        assert_eq!(
            timeout(Duration::from_secs(1), notifications.recv())
                .await
                .unwrap(),
            Some(expected)
        );
    }
    daemon.notify_value("also/unknown", Value::Null).unwrap();
    daemon.close();
    assert_eq!(
        timeout(Duration::from_secs(1), notifications.recv())
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn incoming_daemon_requests_receive_method_not_found() {
    let (_client, _notifications, daemon, _incoming) = connected().await;
    let result = timeout(
        Duration::from_secs(1),
        daemon.request_value("daemon/callback", Value::Null),
    )
    .await
    .unwrap();
    let Err(PeerError::Rpc(error)) = result else {
        panic!("expected METHOD_NOT_FOUND");
    };
    assert_eq!(error.code, RpcError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn connection_and_decode_errors_are_typed() {
    let (client, _notifications, daemon, mut incoming) = connected().await;
    let (result, ()) = tokio::join!(client.approvals(), async {
        let (id, _, _) = request(&mut incoming).await;
        daemon
            .respond(id, Ok(json!("not an approval list")))
            .unwrap();
    });
    assert!(matches!(result, Err(ClientError::Json(_))));
    daemon.close();
    let result = timeout(Duration::from_secs(1), client.status())
        .await
        .unwrap();
    assert!(matches!(result, Err(ClientError::Closed)));
}

#[tokio::test]
async fn only_dropping_the_last_client_clone_disconnects() {
    let (client, _notifications, daemon, mut incoming) = connected().await;
    let clone = client.clone();
    drop(client);
    exchange!(
        clone.approvals(),
        ApprovalList,
        &daemon,
        &mut incoming,
        (),
        Vec::new()
    );
    drop(clone);
    timeout(Duration::from_secs(1), daemon.closed())
        .await
        .unwrap();
}
