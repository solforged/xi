//! Concurrent newline-delimited JSON-RPC client service.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use xi_protocol::SessionId;
use xi_protocol::methods::client::{
    CheckoutParams, CreateParams, OpenParams, PromptParams, ResolveParams, SessionRef,
};

use crate::config::{Config, Paths};
use crate::{Error, EventBus, Result, Session, TurnOptions};

/// Server-wide user configuration; workspace layers load per session.
#[derive(Clone)]
pub struct Options {
    /// User-owned paths.
    pub paths: Paths,
    /// Extra full-authority config files.
    pub configs: Vec<PathBuf>,
    /// Explicit session storage location.
    pub session_dir: Option<PathBuf>,
    /// CLI-selected model, overriding loaded settings on creation.
    pub model: Option<xi_protocol::event::ModelRef>,
    /// CLI-selected reasoning level.
    pub reasoning: Option<xi_provider::Reasoning>,
    /// CLI-selected tools.
    pub tools: Option<Vec<String>>,
    /// CLI extension directories, resolved against the process directory.
    pub extensions: Vec<PathBuf>,
}

impl Options {
    /// Loads configuration for a new workspace.
    pub fn config(&self, cwd: &std::path::Path) -> Result<Config> {
        let mut config = Config::load(self.paths.clone(), cwd, &self.configs)?;
        if let Some(model) = &self.model {
            config.model = model.clone();
        }
        if let Some(reasoning) = self.reasoning {
            config.reasoning = reasoning;
        }
        if let Some(tools) = &self.tools {
            config.tools = tools.clone();
        }
        for path in &self.extensions {
            config
                .extensions
                .push(std::env::current_dir()?.join(self.paths.expand(path)));
        }
        Ok(config)
    }
}

/// Serves requests until input EOF, cancellation of active turns, and replies.
/// Prompt requests are submitted before reading the next line so controls
/// cannot overtake their turn registration.
pub async fn serve<R, W>(reader: R, mut writer: W, options: Options) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let bus = EventBus::default();
    let mut notifications = bus.subscribe();
    let (reply_tx, mut replies) = mpsc::unbounded_channel::<Value>();
    let mut sessions = HashMap::<SessionId, Session>::new();
    let mut lines = reader.lines();
    let mut pending = 0usize;
    let mut eof = false;
    loop {
        tokio::select! {
            Some(notification) = notifications.recv() => write_value(&mut writer, &notification.json()).await?,
            Some(reply) = replies.recv(), if pending > 0 => {
                pending -= 1;
                write_value(&mut writer, &reply).await?;
            }
            line = lines.next_line(), if !eof => {
                let Some(line) = line? else {
                    eof = true;
                    for session in sessions.values() { session.close(); }
                    if pending == 0 { break; }
                    continue;
                };
                let request: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(error) => { write_value(&mut writer, &failure(Value::Null, -32700, error.to_string())).await?; continue; }
                };
                let id = request.get("id").cloned();
                let valid_id = id.as_ref().is_none_or(|id| id.is_string() || id.is_i64() || id.is_u64() || id.is_null());
                if request.get("jsonrpc") != Some(&json!("2.0")) || !request.get("method").is_some_and(Value::is_string) || !valid_id {
                    write_value(&mut writer, &failure(Value::Null, -32600, "invalid JSON-RPC request".into())).await?;
                    continue;
                }
                let method = request["method"].as_str().expect("validated method");
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                if method == "session/prompt" {
                    let submitted = (|| -> Result<_> {
                        let params: PromptParams = decode(params)?;
                        let session = lookup(&sessions, params.session)?;
                        Ok((session.clone(), session.submit(params.content)))
                    })();
                    match submitted {
                        Ok((session, rx)) => {
                            pending += 1;
                            let tx = reply_tx.clone();
                            tokio::spawn(async move {
                                let result = match rx.await {
                                    Ok(Ok(outcome)) => success(id.clone().unwrap_or(Value::Null), json!({"head":outcome.head})),
                                    Ok(Err(error)) => failure(id.clone().unwrap_or(Value::Null), -32000, error),
                                    Err(_) => failure(id.clone().unwrap_or(Value::Null), -32000, "turn task stopped".into()),
                                };
                                // Keep a sentinel for notifications so pending accounting
                                // remains exact without emitting an RPC response.
                                let _ = tx.send(if id.is_some() { result } else { Value::Null });
                                drop(session);
                            });
                        }
                        Err(error) => if let Some(id) = id { write_value(&mut writer, &failure(id, -32602, error.to_string())).await?; },
                    }
                    continue;
                }
                if method == "session/compact" {
                    let session = decode::<SessionRef>(params)
                        .and_then(|params| lookup(&sessions, params.session).cloned());
                    match session {
                        Ok(session) => {
                            pending += 1;
                            let tx = reply_tx.clone();
                            tokio::spawn(async move {
                                let result = match session.compact().await {
                                    Ok(result) => success(id.clone().unwrap_or(Value::Null), json!(result)),
                                    Err(error) => failure(id.clone().unwrap_or(Value::Null), -32000, error.to_string()),
                                };
                                let _ = tx.send(if id.is_some() { result } else { Value::Null });
                            });
                        }
                        Err(error) => if let Some(id) = id { write_value(&mut writer, &failure(id, -32602, error.to_string())).await?; },
                    }
                    continue;
                }
                let result = dispatch(method, params, &mut sessions, &bus, &options).await;
                if let Some(id) = id {
                    let reply = match result {
                        Ok(result) => success(id, result),
                        Err((code, message)) => failure(id, code, message),
                    };
                    // Root events were emitted during create; flush them before
                    // acknowledging the corresponding operation.
                    while let Ok(notification) = notifications.try_recv() { write_value(&mut writer, &notification.json()).await?; }
                    write_value(&mut writer, &reply).await?;
                }
            }
            else => break,
        }
        if eof && pending == 0 {
            while let Ok(notification) = notifications.try_recv() {
                write_value(&mut writer, &notification.json()).await?;
            }
            break;
        }
    }
    bus.shutdown_extensions().await;
    Ok(())
}

async fn dispatch(
    method: &str,
    params: Value,
    sessions: &mut HashMap<SessionId, Session>,
    bus: &EventBus,
    options: &Options,
) -> std::result::Result<Value, (i32, String)> {
    if !matches!(
        method,
        "session/create"
            | "session/open"
            | "session/cancel"
            | "session/checkout"
            | "approval/resolve"
    ) {
        return Err((-32601, format!("unknown method {method}")));
    }
    let result: Result<Value> = async {
        match method {
            "session/create" => {
                let params: CreateParams = decode(params)?;
                let mut config = options.config(&params.cwd)?;
                if let Some(model) = params.model {
                    config.model = model;
                }
                let session = Session::create(
                    config,
                    options.session_dir.as_deref(),
                    bus.clone(),
                    TurnOptions::default(),
                    None,
                )?;
                let info = session.info().await;
                sessions.insert(session.id(), session);
                Ok(serde_json::to_value(info)?)
            }
            "session/open" => {
                let params: OpenParams = decode(params)?;
                let log = xi_session::SessionLog::open(params.path)?;
                let xi_protocol::EventKind::SessionStarted { cwd, .. } = &log.root().kind else {
                    unreachable!("validated root")
                };
                let config = options.config(cwd)?;
                let session =
                    Session::from_log(log, config, bus.clone(), TurnOptions::default(), None)?;
                if sessions.contains_key(&session.id()) {
                    return Err(Error::Config("session identity already open".into()));
                }
                let info = session.info().await;
                sessions.insert(session.id(), session);
                Ok(serde_json::to_value(info)?)
            }
            "session/cancel" => {
                let params: SessionRef = decode(params)?;
                lookup(sessions, params.session)?.cancel();
                Ok(Value::Null)
            }
            "session/checkout" => {
                let params: CheckoutParams = decode(params)?;
                lookup(sessions, params.session)?
                    .checkout(params.event)
                    .await?;
                Ok(Value::Null)
            }
            "approval/resolve" => {
                let params: ResolveParams = decode(params)?;
                lookup(sessions, params.session)?.approve(&params.call, params.approved)?;
                Ok(Value::Null)
            }
            _ => unreachable!("checked method"),
        }
    }
    .await;
    result.map_err(|error| {
        (
            if matches!(error, Error::Json(_)) {
                -32602
            } else {
                -32000
            },
            error.to_string(),
        )
    })
}

fn lookup(sessions: &HashMap<SessionId, Session>, id: SessionId) -> Result<&Session> {
    sessions
        .get(&id)
        .ok_or_else(|| Error::Config("unknown session".into()))
}
fn decode<T: DeserializeOwned>(params: Value) -> Result<T> {
    Ok(serde_json::from_value(params)?)
}
fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}
fn failure(id: Value, code: i32, message: String) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}
async fn write_value<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
