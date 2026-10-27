//! Peer framing, routing and shutdown over in-memory duplex pipes.

use std::time::Duration;

use futures::poll;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex, split};
use tokio::time::timeout;
use xi_protocol::rpc::{Message, NotificationMethod, Outcome, RequestId, RequestMethod, RpcError};
use xi_rpc::{Incoming, Peer, PeerError};

struct Echo;

impl RequestMethod for Echo {
    const METHOD: &'static str = "test/echo";
    type Params = String;
    type Result = String;
}

struct Notice;

impl NotificationMethod for Notice {
    const METHOD: &'static str = "test/notice";
    type Params = String;
}

#[tokio::test]
async fn concurrent_requests_resolve_out_of_order() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(right);
    let (remote, mut incoming) = Peer::spawn(reader, writer);

    let first_peer = peer.clone();
    let first = tokio::spawn(async move { first_peer.request_value("first", json!(1)).await });
    let Incoming::Request {
        id: first_id,
        method,
        params,
    } = incoming.recv().await.unwrap()
    else {
        panic!("expected the first request");
    };
    assert_eq!(method, "first");
    assert_eq!(params, json!(1));

    let second_peer = peer.clone();
    let second = tokio::spawn(async move { second_peer.request_value("second", json!(2)).await });
    let Incoming::Request {
        id: second_id,
        method,
        params,
    } = incoming.recv().await.unwrap()
    else {
        panic!("expected the second request");
    };
    assert_eq!(method, "second");
    assert_eq!(params, json!(2));
    let (RequestId::Number(first_number), RequestId::Number(second_number)) =
        (&first_id, &second_id)
    else {
        panic!("expected numeric request ids");
    };
    assert!(second_number > first_number);

    remote.respond(second_id, Ok(json!("second"))).unwrap();
    assert_eq!(second.await.unwrap().unwrap(), json!("second"));
    assert!(!first.is_finished());
    remote.respond(first_id, Ok(json!("first"))).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), json!("first"));
}

#[tokio::test]
async fn started_requests_are_enqueued_before_their_response_futures_are_polled() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(right);
    let (remote, mut incoming) = Peer::spawn(reader, writer);

    let first = peer.start_request_value("first", json!(1)).unwrap();
    let second = peer.start_request_value("second", json!(2)).unwrap();
    let Incoming::Request {
        id: first_id,
        method,
        params,
    } = timeout(Duration::from_secs(1), incoming.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("expected the first request before polling its response");
    };
    assert_eq!(method, "first");
    assert_eq!(params, json!(1));
    let Incoming::Request {
        id: second_id,
        method,
        params,
    } = incoming.recv().await.unwrap()
    else {
        panic!("expected the second request");
    };
    assert_eq!(method, "second");
    assert_eq!(params, json!(2));

    remote.respond(second_id, Ok(json!(2))).unwrap();
    assert_eq!(second.await.unwrap(), json!(2));
    remote.respond(first_id, Ok(json!(1))).unwrap();
    assert_eq!(first.await.unwrap(), json!(1));
}

#[tokio::test]
async fn notifications_and_requests_arrive_in_wire_order() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(right);
    let (remote, mut incoming) = Peer::spawn(reader, writer);

    peer.notify::<Notice>(&"before".into()).unwrap();
    let request = peer.request_value("middle", json!({"order": 2}));
    tokio::pin!(request);
    assert!(poll!(&mut request).is_pending());
    peer.notify_value("after", json!(3)).unwrap();

    assert_eq!(
        incoming.recv().await.unwrap(),
        Incoming::Notification {
            method: Notice::METHOD.into(),
            params: json!("before"),
        }
    );
    let Incoming::Request { id, method, params } = incoming.recv().await.unwrap() else {
        panic!("expected the middle request");
    };
    assert_eq!(method, "middle");
    assert_eq!(params, json!({"order": 2}));
    assert_eq!(
        incoming.recv().await.unwrap(),
        Incoming::Notification {
            method: "after".into(),
            params: json!(3),
        }
    );
    remote.respond(id, Ok(Value::Null)).unwrap();
    assert_eq!(request.await.unwrap(), Value::Null);
}

#[tokio::test]
async fn respond_delivers_typed_success_and_rpc_error() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let (reader, writer) = split(right);
    let (remote, mut incoming) = Peer::spawn(reader, writer);

    let request_peer = peer.clone();
    let request = tokio::spawn(async move { request_peer.request::<Echo>(&"hello".into()).await });
    let Incoming::Request { id, method, params } = incoming.recv().await.unwrap() else {
        panic!("expected an echo request");
    };
    assert_eq!(method, Echo::METHOD);
    assert_eq!(params, json!("hello"));
    remote.respond(id, Ok(params)).unwrap();
    assert_eq!(request.await.unwrap().unwrap(), "hello");

    let request_peer = peer.clone();
    let request = tokio::spawn(async move { request_peer.request::<Echo>(&"error".into()).await });
    let Incoming::Request { id, .. } = incoming.recv().await.unwrap() else {
        panic!("expected another echo request");
    };
    let error = RpcError::new(RpcError::INVALID_PARAMS, "rejected");
    remote.respond(id, Err(error.clone())).unwrap();
    let Err(PeerError::Rpc(received)) = request.await.unwrap() else {
        panic!("expected the RPC error");
    };
    assert_eq!(received, error);
}

#[tokio::test]
async fn pending_request_fails_with_closed_at_eof() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, mut incoming) = Peer::spawn(reader, writer);
    let mut remote = BufReader::new(right);
    let request_peer = peer.clone();
    let request =
        tokio::spawn(async move { request_peer.request_value("pending", Value::Null).await });
    let mut line = String::new();
    assert!(remote.read_line(&mut line).await.unwrap() > 0);
    drop(remote);

    assert!(matches!(request.await.unwrap(), Err(PeerError::Closed)));
    timeout(Duration::from_secs(1), peer.closed())
        .await
        .unwrap();
    assert!(incoming.recv().await.is_none());
    assert!(matches!(
        peer.notify_value("closed", Value::Null),
        Err(PeerError::Closed)
    ));
}

#[tokio::test]
async fn malformed_lines_get_errors_and_the_connection_survives() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, mut incoming) = Peer::spawn(reader, writer);
    let (reader, mut writer) = split(right);
    let mut lines = BufReader::new(reader).lines();

    for (line, expected_code) in [
        ("{\n", RpcError::PARSE_ERROR),
        (
            "{\"jsonrpc\":\"1.0\",\"method\":\"invalid\"}\n",
            RpcError::INVALID_REQUEST,
        ),
        ("[]\n", RpcError::INVALID_REQUEST),
    ] {
        writer.write_all(line.as_bytes()).await.unwrap();
        let response = lines.next_line().await.unwrap().unwrap();
        let Message::Response(response) = serde_json::from_str(&response).unwrap() else {
            panic!("expected an error response");
        };
        assert_eq!(response.id, None);
        let Outcome::Err(error) = response.outcome else {
            panic!("expected an error outcome");
        };
        assert_eq!(error.code, expected_code);
    }

    writer
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":999,\"result\":null}\n\
              {\"jsonrpc\":\"2.0\",\"id\":\"alive\",\"method\":\"echo\",\"params\":\"ok\"}\n",
        )
        .await
        .unwrap();
    let Incoming::Request { id, method, params } = incoming.recv().await.unwrap() else {
        panic!("expected a request after malformed lines and an unknown response");
    };
    assert_eq!(id, RequestId::String("alive".into()));
    assert_eq!(method, "echo");
    peer.respond(id.clone(), Ok(params)).unwrap();
    let response = lines.next_line().await.unwrap().unwrap();
    let Message::Response(response) = serde_json::from_str(&response).unwrap() else {
        panic!("expected a success response");
    };
    assert_eq!(response.id, Some(id));
    assert_eq!(response.outcome, Outcome::Ok(json!("ok")));
}

#[tokio::test]
async fn close_signals_closure_and_shuts_down_the_writer() {
    let (left, right) = duplex(4096);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let mut remote = BufReader::new(right);

    peer.close();
    peer.close();
    timeout(Duration::from_secs(1), peer.closed())
        .await
        .unwrap();
    let mut line = String::new();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_line(&mut line))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(matches!(
        peer.request_value("closed", Value::Null).await,
        Err(PeerError::Closed)
    ));
    assert!(matches!(
        peer.respond(RequestId::Number(1), Ok(Value::Null)),
        Err(PeerError::Closed)
    ));
}

#[tokio::test]
async fn close_drains_queued_messages_before_eof_even_when_peer_is_dropped() {
    let (left, right) = duplex(1);
    let (reader, writer) = split(left);
    let (peer, _incoming) = Peer::spawn(reader, writer);
    let mut lines = BufReader::new(right).lines();

    peer.respond(RequestId::Number(1), Ok(json!("first")))
        .unwrap();
    peer.notify_value("middle", json!(2)).unwrap();
    peer.respond(RequestId::Number(3), Ok(json!("last")))
        .unwrap();
    peer.close();

    assert!(matches!(
        peer.notify_value("rejected", Value::Null),
        Err(PeerError::Closed)
    ));
    assert!(matches!(
        peer.respond(RequestId::Number(4), Ok(Value::Null)),
        Err(PeerError::Closed)
    ));
    {
        let closed = peer.closed();
        tokio::pin!(closed);
        assert!(poll!(&mut closed).is_pending());
    }
    drop(peer);

    for expected in [
        json!({"jsonrpc": "2.0", "id": 1, "result": "first"}),
        json!({"jsonrpc": "2.0", "method": "middle", "params": 2}),
        json!({"jsonrpc": "2.0", "id": 3, "result": "last"}),
    ] {
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
    }
    assert!(
        timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
}
