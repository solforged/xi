//! Typed requests and notifications for the xi daemon.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use xi_protocol::content::Block;
use xi_protocol::event::{ModelRef, Remember};
use xi_protocol::ids::{EventId, SessionId, ToolCallId};
use xi_protocol::manifest::CommandSpec;
use xi_protocol::methods::client::*;
use xi_protocol::methods::extension::CommandResult;
use xi_protocol::model::{ModelSelector, Reasoning};
use xi_protocol::rpc::{NotificationMethod, RequestMethod, RpcError};

use crate::connect;
use xi_rpc::{Incoming, Peer, PeerError, decode_notification};

/// A negotiated daemon connection. Clones share the connection and its identity.
///
/// Dropping the last clone closes the connection without cancelling session turns.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Connection,
    initialized: InitializeResult,
}

struct Connection(Peer);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A typed notification from the daemon, delivered in wire order.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientNotification {
    /// A persisted session event.
    Event(EventParams),
    /// An increment of in-flight model output.
    Delta(DeltaParams),
    /// A new attempt that replaces earlier partial output.
    Reset(ResetParams),
    /// A session's worker has exited.
    Exited(ExitedParams),
    /// A tool call requires approval.
    ApprovalPending(PendingApproval),
}

/// A failed daemon connection or operation.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket could not be connected or the daemon could not be started.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The daemon rejected a request.
    #[error("{message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable explanation.
        message: String,
        /// Additional structured error details.
        data: Option<Value>,
    },
    /// The connection has ended.
    #[error("connection closed")]
    Closed,
    /// A payload could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<PeerError> for ClientError {
    fn from(error: PeerError) -> Self {
        match error {
            PeerError::Closed => Self::Closed,
            PeerError::Rpc(error) => Self::Rpc {
                code: error.code,
                message: error.message,
                data: error.data,
            },
            PeerError::Json(error) => Self::Json(error),
        }
    }
}

impl Client {
    /// Wraps a connection, negotiates the protocol, and routes notifications.
    ///
    /// Sends the caller's name and this crate's version in `initialize`. Unknown
    /// notification methods and malformed notification payloads are ignored.
    /// Incoming daemon requests receive `METHOD_NOT_FOUND`.
    ///
    /// # Errors
    /// Fails if initialization is rejected, its response is invalid, or the
    /// connection closes before initialization completes.
    pub async fn over<R, W>(
        reader: R,
        writer: W,
        name: &str,
    ) -> Result<(Client, mpsc::UnboundedReceiver<ClientNotification>), ClientError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (peer, incoming) = Peer::spawn(reader, writer);
        let connection = Connection(peer);
        let (notifications, receiver) = mpsc::unbounded_channel();
        tokio::spawn(route(connection.0.clone(), incoming, notifications));
        let initialized = connection
            .0
            .request::<Initialize>(&InitializeParams {
                protocol_version: xi_protocol::PROTOCOL_VERSION,
                client: ClientInfo {
                    name: name.to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            })
            .await?;
        Ok((
            Client {
                inner: Arc::new(Inner {
                    connection,
                    initialized,
                }),
            },
            receiver,
        ))
    }

    /// Connects to the per-user daemon, starting it when needed, then initializes.
    ///
    /// # Errors
    /// Fails on socket connection, daemon startup, or initialization errors.
    pub async fn connect(
        exe: &Path,
        name: &str,
    ) -> Result<(Client, mpsc::UnboundedReceiver<ClientNotification>), ClientError> {
        let stream = connect::connect_or_start(exe).await?;
        let (reader, writer) = stream.into_split();
        Self::over(reader, writer, name).await
    }

    /// Returns the daemon's negotiated identity and capabilities.
    pub fn initialized(&self) -> &InitializeResult {
        &self.inner.initialized
    }

    /// Creates a session and attaches this client to it.
    pub async fn create(&self, p: CreateParams) -> Result<AttachResult, ClientError> {
        self.request::<SessionCreate>(&p).await
    }

    /// Opens a saved session and attaches this client to it.
    pub async fn open(&self, p: OpenParams) -> Result<AttachResult, ClientError> {
        self.request::<SessionOpen>(&p).await
    }

    /// Attaches to a live session, optionally replaying only events after `since`.
    pub async fn attach(
        &self,
        session: SessionId,
        since: Option<EventId>,
    ) -> Result<AttachResult, ClientError> {
        self.request::<SessionAttach>(&AttachParams { session, since })
            .await
    }

    /// Stops receiving events from a session without cancelling its turn.
    pub async fn detach(&self, session: SessionId) -> Result<(), ClientError> {
        self.request::<SessionDetach>(&SessionRef { session }).await
    }

    /// Lists live and saved sessions matching the supplied filters.
    pub async fn list(&self, p: ListParams) -> Result<Vec<SessionSummary>, ClientError> {
        self.request::<SessionList>(&p).await
    }

    /// Reports effective settings and their configuration layers for a workspace.
    pub async fn settings(&self, p: SettingsParams) -> Result<SettingsReport, ClientError> {
        self.request::<ConfigSettings>(&p).await
    }

    /// Sets or removes a global setting and reports the resulting effective settings.
    pub async fn set_setting(&self, p: SetSettingParams) -> Result<SettingsReport, ClientError> {
        self.request::<ConfigSet>(&p).await
    }

    /// Closes a session's idle worker.
    pub async fn close(&self, session: SessionId) -> Result<(), ClientError> {
        self.request::<SessionClose>(&SessionRef { session }).await
    }

    /// Runs a turn and resolves when it ends; notifications stream meanwhile.
    pub async fn prompt(
        &self,
        session: SessionId,
        content: Vec<Block>,
    ) -> Result<PromptResult, ClientError> {
        self.request::<SessionPrompt>(&PromptParams { session, content })
            .await
    }

    /// Cancels a running turn.
    pub async fn cancel(&self, session: SessionId) -> Result<(), ClientError> {
        self.request::<SessionCancel>(&SessionRef { session }).await
    }

    /// Moves a session's head to an existing event.
    pub async fn checkout(&self, session: SessionId, event: EventId) -> Result<(), ClientError> {
        self.request::<SessionCheckout>(&CheckoutParams { session, event })
            .await
    }

    /// Compacts a session's context while idle.
    pub async fn compact(&self, session: SessionId) -> Result<CompactResult, ClientError> {
        self.request::<SessionCompact>(&SessionRef { session })
            .await
    }

    /// Selects a session's model or role, returning the concrete model.
    pub async fn set_model(
        &self,
        session: SessionId,
        model: impl Into<ModelSelector>,
    ) -> Result<ModelRef, ClientError> {
        self.request::<SessionSetModel>(&SetModelParams {
            session,
            model: model.into(),
        })
        .await
    }

    /// Selects a session's reasoning depth while idle.
    pub async fn set_reasoning(
        &self,
        session: SessionId,
        reasoning: Reasoning,
    ) -> Result<(), ClientError> {
        self.request::<SessionSetReasoning>(&SetReasoningParams { session, reasoning })
            .await
    }

    /// Enables or disables the live worker's tool sandbox while idle.
    /// Does not change the saved default or extension sandboxes.
    pub async fn set_sandbox(&self, session: SessionId, enabled: bool) -> Result<(), ClientError> {
        self.request::<SessionSetSandbox>(&SetSandboxParams { session, enabled })
            .await
    }

    /// Sets a session's title, at once when idle or at the running turn's
    /// next model boundary.
    pub async fn rename(
        &self,
        session: SessionId,
        title: impl Into<String>,
    ) -> Result<(), ClientError> {
        self.request::<SessionRename>(&RenameParams {
            session,
            title: title.into(),
        })
        .await
    }

    /// Lists registry models and credential availability without exposing credentials.
    pub async fn models(&self, session: SessionId) -> Result<ModelsResult, ClientError> {
        self.request::<SessionModels>(&SessionRef { session }).await
    }

    /// Lists extension slash commands in registration order.
    pub async fn commands(&self, session: SessionId) -> Result<Vec<CommandSpec>, ClientError> {
        self.request::<SessionCommands>(&SessionRef { session })
            .await
    }

    /// Runs an extension slash command; returns `None` for an unknown command.
    pub async fn command(
        &self,
        session: SessionId,
        name: &str,
        args: &str,
    ) -> Result<Option<CommandResult>, ClientError> {
        self.request::<SessionCommand>(&SessionCommandParams {
            session,
            name: name.to_owned(),
            args: args.to_owned(),
        })
        .await
    }

    /// Lists all pending tool approvals in the daemon.
    pub async fn approvals(&self) -> Result<Vec<PendingApproval>, ClientError> {
        self.request::<ApprovalList>(&()).await
    }

    /// Resolves a pending tool approval.
    pub async fn resolve(
        &self,
        session: SessionId,
        call: ToolCallId,
        approved: bool,
        remember: Remember,
    ) -> Result<(), ClientError> {
        self.request::<ApprovalResolve>(&ResolveParams {
            session,
            call,
            approved,
            remember,
        })
        .await
    }

    /// Reads daemon and model scheduler state.
    pub async fn status(&self) -> Result<StatusResult, ClientError> {
        self.request::<DaemonStatus>(&()).await
    }

    /// Shuts the daemon down now or once all workers close.
    pub async fn shutdown(&self, when: ShutdownWhen) -> Result<(), ClientError> {
        self.request::<DaemonShutdown>(&ShutdownParams { when })
            .await
    }

    async fn request<M: RequestMethod>(
        &self,
        params: &M::Params,
    ) -> Result<M::Result, ClientError> {
        Ok(self.inner.connection.0.request::<M>(params).await?)
    }
}

async fn route(
    peer: Peer,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    notifications: mpsc::UnboundedSender<ClientNotification>,
) {
    while let Some(call) = incoming.recv().await {
        match call {
            Incoming::Request { id, method, .. } => {
                let _ = peer.respond(
                    id,
                    Err(RpcError::new(
                        RpcError::METHOD_NOT_FOUND,
                        format!("unknown method: {method}"),
                    )),
                );
            }
            Incoming::Notification { method, params } => {
                let notification =
                    match method.as_str() {
                        EventAppended::METHOD => decode_notification::<EventAppended>(params)
                            .map(ClientNotification::Event),
                        StreamDelta::METHOD => decode_notification::<StreamDelta>(params)
                            .map(ClientNotification::Delta),
                        StreamReset::METHOD => decode_notification::<StreamReset>(params)
                            .map(ClientNotification::Reset),
                        SessionExited::METHOD => decode_notification::<SessionExited>(params)
                            .map(ClientNotification::Exited),
                        ApprovalPending::METHOD => decode_notification::<ApprovalPending>(params)
                            .map(ClientNotification::ApprovalPending),
                        _ => continue,
                    };
                if let Ok(notification) = notification {
                    let _ = notifications.send(notification);
                }
            }
        }
    }
}
