//! Concurrent full-duplex NDJSON JSON-RPC connections.
//!
//! [`Peer`] is used by clients, the daemon and workers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use xi_protocol::rpc::{
    Message, Notification, NotificationMethod, Outcome, Request, RequestId, RequestMethod,
    Response, RpcError, Version,
};

type Reply = oneshot::Sender<Result<Value, PeerError>>;

/// Decodes incoming request parameters using the method's protocol type.
///
/// # Errors
/// Returns [`RpcError::INVALID_PARAMS`] if the parameters cannot be decoded.
pub fn decode_request<M: RequestMethod>(params: Value) -> Result<M::Params, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

/// Encodes an incoming request's result using the method's protocol type.
///
/// This is separate from decoding so handlers can queue or spawn delayed replies.
///
/// # Errors
/// Returns [`RpcError::INTERNAL_ERROR`] if the result cannot be encoded.
pub fn encode_result<M: RequestMethod>(result: M::Result) -> Result<Value, RpcError> {
    serde_json::to_value(result)
        .map_err(|error| RpcError::new(RpcError::INTERNAL_ERROR, error.to_string()))
}

/// Decodes incoming notification parameters using the method's protocol type.
///
/// # Errors
/// Returns [`RpcError::INVALID_PARAMS`] if the parameters cannot be decoded.
pub fn decode_notification<M: NotificationMethod>(params: Value) -> Result<M::Params, RpcError> {
    serde_json::from_value(params)
        .map_err(|error| RpcError::new(RpcError::INVALID_PARAMS, error.to_string()))
}

/// A full-duplex NDJSON JSON-RPC 2.0 connection. Cheap to clone.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

/// An incoming call, delivered in wire order.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// A request that needs a response through [`Peer::respond`].
    Request {
        /// Id to echo in the response.
        id: RequestId,
        /// Wire method name.
        method: String,
        /// Undecoded parameters, or null when omitted.
        params: Value,
    },
    /// A notification that does not need a response.
    Notification {
        /// Wire method name.
        method: String,
        /// Undecoded parameters, or null when omitted.
        params: Value,
    },
}

/// A failed peer operation.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    /// The connection has ended.
    #[error("connection closed")]
    Closed,
    /// The remote peer returned a JSON-RPC error.
    #[error("{0:?}")]
    Rpc(RpcError),
    /// Parameters or a result could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

struct Inner {
    state: Mutex<State>,
    closed: watch::Sender<Shutdown>,
}

struct State {
    outgoing: Option<mpsc::UnboundedSender<Vec<u8>>>,
    next_id: Option<i64>,
    pending: HashMap<RequestId, Reply>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    Open,
    Draining,
    Closed,
}

impl Peer {
    /// Starts reader and writer tasks. Incoming calls arrive in wire order and
    /// the receiver ends at EOF. Pending requests fail with [`PeerError::Closed`]
    /// at EOF.
    pub fn spawn<R, W>(reader: R, writer: W) -> (Peer, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, writes) = mpsc::unbounded_channel();
        let (incoming, calls) = mpsc::unbounded_channel();
        let (closed, ended) = watch::channel(Shutdown::Open);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                outgoing: Some(outgoing),
                next_id: Some(1),
                pending: HashMap::new(),
            }),
            closed,
        });
        tokio::spawn(read_messages(reader, incoming, Arc::downgrade(&inner)));
        tokio::spawn(write_messages(
            writer,
            writes,
            ended,
            Arc::downgrade(&inner),
        ));
        (Peer { inner }, calls)
    }

    /// Sends a typed request and decodes its response.
    ///
    /// # Errors
    /// Fails on closure, a remote RPC error, or invalid parameters or results.
    pub async fn request<M: RequestMethod>(
        &self,
        params: &M::Params,
    ) -> Result<M::Result, PeerError> {
        let value = self
            .request_value(M::METHOD, serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends a request with undecoded parameters and result.
    ///
    /// # Errors
    /// Fails on closure, a remote RPC error, or a serialization error.
    pub async fn request_value(&self, method: &str, params: Value) -> Result<Value, PeerError> {
        self.start_request_value(method, params)?.await
    }

    /// Enqueues a request immediately and returns its independent response wait.
    /// Dropping the returned future stops waiting without cancelling the request.
    ///
    /// # Errors
    /// Fails on closure or a serialization error before the request is queued.
    pub fn start_request_value(
        &self,
        method: &str,
        params: Value,
    ) -> Result<impl Future<Output = Result<Value, PeerError>> + Send + 'static + use<>, PeerError>
    {
        let (id, receiver) = {
            let mut state = self.inner.state.lock().expect("peer state");
            if state.outgoing.is_none() {
                return Err(PeerError::Closed);
            }
            let Some(next_id) = state.next_id else {
                drop(state);
                self.inner.finish();
                return Err(PeerError::Closed);
            };
            let id = RequestId::Number(next_id);
            state.next_id = next_id.checked_add(1);
            let message = Message::Request(Request {
                jsonrpc: Version,
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
            let (reply, receiver) = oneshot::channel();
            state.pending.insert(id.clone(), reply);
            if let Err(error) = state.send(&message) {
                state.pending.remove(&id);
                drop(state);
                if matches!(&error, PeerError::Closed) {
                    self.inner.finish();
                }
                return Err(error);
            }
            (id, receiver)
        };
        let pending = Pending {
            inner: self.inner.clone(),
            id,
        };
        Ok(async move {
            let result = receiver.await.map_err(|_| PeerError::Closed)?;
            drop(pending);
            result
        })
    }

    /// Sends a typed notification.
    ///
    /// # Errors
    /// Fails on closure or a serialization error.
    pub fn notify<M: NotificationMethod>(&self, params: &M::Params) -> Result<(), PeerError> {
        self.notify_value(M::METHOD, serde_json::to_value(params)?)
    }

    /// Sends a notification with undecoded parameters.
    ///
    /// # Errors
    /// Fails on closure or a serialization error.
    pub fn notify_value(&self, method: &str, params: Value) -> Result<(), PeerError> {
        self.inner.send(&Message::Notification(Notification {
            jsonrpc: Version,
            method: method.to_owned(),
            params,
        }))
    }

    /// Answers an incoming request with a result or an RPC error.
    ///
    /// # Errors
    /// Fails on closure or a serialization error.
    pub fn respond(&self, id: RequestId, result: Result<Value, RpcError>) -> Result<(), PeerError> {
        let outcome = match result {
            Ok(value) => Outcome::Ok(value),
            Err(error) => Outcome::Err(error),
        };
        self.inner.send(&Message::Response(Response {
            jsonrpc: Version,
            id: Some(id),
            outcome,
        }))
    }

    /// Resolves at EOF or write failure, or after [`Self::close`] has drained
    /// queued messages and shut down the writer.
    pub async fn closed(&self) {
        let mut ended = self.inner.closed.subscribe();
        let _ = ended
            .wait_for(|shutdown| *shutdown == Shutdown::Closed)
            .await;
    }

    /// Stops accepting writes, then drains queued messages and shuts down the
    /// writer without waiting. The reader ends at EOF.
    pub fn close(&self) {
        self.inner.end(Shutdown::Draining);
    }
}

impl Inner {
    fn send(&self, message: &Message) -> Result<(), PeerError> {
        let state = self.state.lock().expect("peer state");
        let result = state.send(message);
        let failed = state.outgoing.is_some() && matches!(&result, Err(PeerError::Closed));
        drop(state);
        if failed {
            self.finish();
        }
        result
    }

    fn finish(&self) {
        self.end(Shutdown::Closed);
    }

    fn end(&self, shutdown: Shutdown) {
        let mut state = self.state.lock().expect("peer state");
        state.outgoing.take();
        for (_, reply) in state.pending.drain() {
            let _ = reply.send(Err(PeerError::Closed));
        }
        self.closed.send_modify(|current| {
            if *current != Shutdown::Closed {
                *current = shutdown;
            }
        });
    }
}

impl State {
    fn send(&self, message: &Message) -> Result<(), PeerError> {
        let outgoing = self.outgoing.as_ref().ok_or(PeerError::Closed)?;
        let mut line = serde_json::to_vec(message)?;
        line.push(b'\n');
        outgoing.send(line).map_err(|_| PeerError::Closed)
    }
}

struct Pending {
    inner: Arc<Inner>,
    id: RequestId,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.inner
            .state
            .lock()
            .expect("peer state")
            .pending
            .remove(&self.id);
    }
}

async fn read_messages<R: AsyncRead + Unpin>(
    reader: R,
    incoming: mpsc::UnboundedSender<Incoming>,
    inner: Weak<Inner>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Some(inner) = inner.upgrade() else {
            break;
        };
        match serde_json::from_str::<Message>(&line) {
            Ok(Message::Response(response)) => {
                if let Some(id) = response.id {
                    let reply = inner.state.lock().expect("peer state").pending.remove(&id);
                    if let Some(reply) = reply {
                        let result = match response.outcome {
                            Outcome::Ok(value) => Ok(value),
                            Outcome::Err(error) => Err(PeerError::Rpc(error)),
                        };
                        let _ = reply.send(result);
                    }
                }
            }
            Ok(Message::Request(request)) => {
                let _ = incoming.send(Incoming::Request {
                    id: request.id,
                    method: request.method,
                    params: request.params,
                });
            }
            Ok(Message::Notification(notification)) => {
                let _ = incoming.send(Incoming::Notification {
                    method: notification.method,
                    params: notification.params,
                });
            }
            Err(error) => {
                let code = if error.is_data() {
                    RpcError::INVALID_REQUEST
                } else {
                    RpcError::PARSE_ERROR
                };
                let _ = inner.send(&Message::Response(Response::err(
                    None,
                    RpcError::new(code, error.to_string()),
                )));
            }
        }
    }
    if let Some(inner) = inner.upgrade() {
        inner.finish();
    }
}

async fn write_messages<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
    mut ended: watch::Receiver<Shutdown>,
    inner: Weak<Inner>,
) {
    loop {
        let line = tokio::select! {
            biased;
            () = aborted(&mut ended) => break,
            line = outgoing.recv() => match line {
                Some(line) => line,
                None => break,
            },
        };
        let result = tokio::select! {
            biased;
            () = aborted(&mut ended) => break,
            result = async {
                writer.write_all(&line).await?;
                writer.flush().await
            } => result,
        };
        if result.is_err() {
            if let Some(inner) = inner.upgrade() {
                inner.finish();
            }
            break;
        }
    }
    let _ = writer.shutdown().await;
    if let Some(inner) = inner.upgrade() {
        inner.finish();
    }
}

async fn aborted(ended: &mut watch::Receiver<Shutdown>) {
    if ended
        .wait_for(|shutdown| *shutdown == Shutdown::Closed)
        .await
        .is_err()
    {
        std::future::pending::<()>().await;
    }
}
