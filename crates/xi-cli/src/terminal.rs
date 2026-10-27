//! Line-mode interaction and notification rendering for daemon clients.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{IsTerminal, Write};

use rustyline::error::ReadlineError;
use tokio::sync::mpsc;
use xi_client::client::{Client, ClientError, ClientNotification};
use xi_kernel::config::Paths;
use xi_protocol::event::{Decision, EventKind, Remember};
use xi_protocol::methods::client::{AttachResult, Delta, PromptResult};
use xi_protocol::model::{ModelSelector, Reasoning};
use xi_protocol::rpc::RpcError;
use xi_protocol::{Block, EventId, OutputBlock, SessionId, ToolCallId};

use crate::Result;

/// Output selection for one client invocation.
#[derive(Clone, Copy)]
pub(super) enum Render {
    /// Only report turn failures; the caller prints the final text.
    Quiet,
    /// Print every notification as a JSON-RPC envelope.
    Json,
    /// Print streaming text, tool summaries and approval prompts.
    Interactive,
}

/// Rendering state, including replay deduplication for the attached session.
pub(super) struct Renderer {
    mode: Render,
    session: Option<SessionId>,
    head: Option<EventId>,
    seen: HashSet<EventId>,
    stream_sequence: u64,
    approvals: HashMap<ToolCallId, String>,
    tools: HashMap<ToolCallId, String>,
    pending_input: VecDeque<ToolCallId>,
    line_mode: bool,
    reading: bool,
}

impl Renderer {
    /// Starts a renderer before any session has been attached.
    pub(super) fn new(mode: Render) -> Self {
        Self {
            mode,
            session: None,
            head: None,
            seen: HashSet::new(),
            stream_sequence: 0,
            approvals: HashMap::new(),
            tools: HashMap::new(),
            pending_input: VecDeque::new(),
            line_mode: false,
            reading: false,
        }
    }

    /// Seeds replay deduplication without printing the saved transcript.
    pub(super) fn attach(&mut self, attached: &AttachResult) {
        self.session = Some(attached.info.session);
        self.head = Some(attached.info.head);
        self.seen.clear();
        self.stream_sequence = attached.stream_sequence;
        if !matches!(self.mode, Render::Json) {
            self.seen
                .extend(attached.events.iter().map(|event| event.id));
        }
        self.approvals.clear();
        self.pending_input.clear();
        self.tools.clear();
        for event in &attached.events {
            self.track_tools(&event.kind);
        }
    }

    /// Prints one notification and starts approval input when needed.
    pub(super) fn display(
        &mut self,
        notification: ClientNotification,
        client: &Client,
    ) -> Result<()> {
        if matches!(self.mode, Render::Json) {
            println!("{}", notification_json(notification));
            return Ok(());
        }
        let session = match &notification {
            ClientNotification::Event(params) => params.session,
            ClientNotification::Delta(params) => params.session,
            ClientNotification::Reset(params) => params.session,
            ClientNotification::Exited(params) => params.session,
            ClientNotification::ApprovalPending(params) => params.session,
        };
        if self.session != Some(session) {
            return Ok(());
        }
        let sequence = match &notification {
            ClientNotification::Delta(params) => params.sequence,
            ClientNotification::Reset(params) => params.sequence,
            _ => 0,
        };
        if sequence != 0 {
            if sequence <= self.stream_sequence {
                return Ok(());
            }
            self.stream_sequence = sequence;
        }
        if let ClientNotification::Event(params) = &notification {
            if !self.seen.insert(params.event.id) {
                return Ok(());
            }
            self.head = Some(params.event.id);
            self.track_tools(&params.event.kind);
            match &params.event.kind {
                EventKind::ApprovalResolved { call, .. } | EventKind::ToolResult { call, .. } => {
                    self.pending_input.retain(|pending| pending != call);
                }
                _ => {}
            }
            if let EventKind::TurnFailed { message } = &params.event.kind {
                eprintln!("xi: {message}");
            }
        }
        if !matches!(self.mode, Render::Interactive) {
            return Ok(());
        }
        match notification {
            ClientNotification::Reset(_) => {}
            ClientNotification::Delta(params) => match params.delta {
                Delta::Text { text } => print!("{text}"),
                Delta::Thinking { text } => {
                    if std::io::stdout().is_terminal() {
                        print!("\x1b[2m{text}\x1b[0m");
                    } else {
                        print!("{text}");
                    }
                }
                Delta::ToolInput { .. } => {}
            },
            ClientNotification::Event(params) => match params.event.kind {
                EventKind::AssistantMessage { content, .. } => {
                    for block in content {
                        if let Block::ToolUse { name, input, .. } = block {
                            let args = input
                                .get("command")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| input.to_string());
                            println!("\n> {name}  {}", args.replace('\n', " "));
                        }
                    }
                }
                EventKind::ToolResult { output, .. } => {
                    let text = output
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            OutputBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    println!(
                        "{}{} [{} lines]",
                        if output.is_error { "error: " } else { "" },
                        text.lines().next().unwrap_or(""),
                        text.lines().count(),
                    );
                }
                EventKind::Compaction { tokens_before, .. } => {
                    println!("\nCompacted {tokens_before} estimated tokens.");
                }
                EventKind::ToolDecision {
                    call,
                    action,
                    during,
                    extension,
                    decision: Decision::Ask,
                    ..
                } => {
                    let tool = self
                        .tools
                        .get(during.as_ref().unwrap_or(&call))
                        .map(String::as_str)
                        .or(extension.as_deref());
                    let prompt = approval_prompt(&action, tool);
                    self.ask(client, session, call, prompt);
                }
                _ => {}
            },
            ClientNotification::ApprovalPending(params) => {
                let prompt = approval_prompt(&params.action, Some(&params.tool));
                self.ask(client, session, params.call, prompt);
            }
            ClientNotification::Exited(params) => {
                if let Some(message) = params.message {
                    eprintln!("xi: {message}");
                }
            }
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    fn track_tools(&mut self, kind: &EventKind) {
        match kind {
            EventKind::AssistantMessage { content, .. } => {
                for block in content {
                    if let Block::ToolUse { id, name, .. } = block {
                        self.tools.insert(id.clone(), name.clone());
                    }
                }
            }
            EventKind::ToolResult { call, .. } => {
                self.tools.remove(call);
            }
            _ => {}
        }
    }

    fn input_prompt(&self) -> &str {
        self.pending_input
            .front()
            .and_then(|call| self.approvals.get(call))
            .map_or("xi> ", String::as_str)
    }

    fn ask(&mut self, client: &Client, session: SessionId, call: ToolCallId, prompt: String) {
        if self.approvals.contains_key(&call) {
            return;
        }
        if self.line_mode {
            if self.reading && self.pending_input.is_empty() {
                print!("\n{prompt}");
            }
            self.approvals.insert(call.clone(), prompt);
            self.pending_input.push_back(call);
            return;
        }
        self.approvals.insert(call.clone(), prompt.clone());
        let client = client.clone();
        tokio::spawn(async move {
            let answer = tokio::task::spawn_blocking(move || {
                rustyline::DefaultEditor::new().and_then(|mut editor| editor.readline(&prompt))
            })
            .await;
            let result = match answer {
                Ok(Err(ReadlineError::Interrupted)) => client.cancel(session).await,
                answer => {
                    let approved =
                        matches!(answer, Ok(Ok(answer)) if matches!(answer.trim(), "y" | "Y"));
                    client
                        .resolve(session, call, approved, Remember::Once)
                        .await
                }
            };
            if let Err(error) = result {
                eprintln!("xi: {error}");
            }
        });
    }
}

fn approval_prompt(action: &str, tool: Option<&str>) -> String {
    if !action.starts_with("net ") {
        return "allow? [y/N] ".to_owned();
    }
    match tool.filter(|tool| !tool.is_empty()) {
        Some(tool) => format!("{tool} · {action} · allow? [y/N] "),
        None => format!("{action} · allow? [y/N] "),
    }
}

fn notification_json(notification: ClientNotification) -> serde_json::Value {
    use serde_json::json;
    match notification {
        ClientNotification::Event(params) => {
            json!({"jsonrpc": "2.0", "method": "event/appended", "params": params})
        }
        ClientNotification::Delta(params) => {
            json!({"jsonrpc": "2.0", "method": "stream/delta", "params": params})
        }
        ClientNotification::Reset(params) => {
            json!({"jsonrpc": "2.0", "method": "stream/reset", "params": params})
        }
        ClientNotification::Exited(params) => {
            json!({"jsonrpc": "2.0", "method": "session/exited", "params": params})
        }
        ClientNotification::ApprovalPending(params) => {
            json!({"jsonrpc": "2.0", "method": "approval/pending", "params": params})
        }
    }
}

/// Runs one prompt while rendering notifications and forwarding interruption.
pub(super) async fn turn(
    client: &Client,
    session: SessionId,
    prompt: String,
    notifications: &mut mpsc::UnboundedReceiver<ClientNotification>,
    renderer: &mut Renderer,
) -> Result<PromptResult> {
    let reply = client.prompt(session, vec![Block::Text { text: prompt }]);
    tokio::pin!(reply);
    let mut interrupted = false;
    loop {
        tokio::select! {
            biased;
            Some(notification) = notifications.recv() => renderer.display(notification, client)?,
            result = &mut reply => {
                while let Ok(notification) = notifications.try_recv() {
                    renderer.display(notification, client)?;
                }
                return result.map_err(Into::into);
            }
            result = tokio::signal::ctrl_c(), if !interrupted => {
                result?;
                interrupted = true;
                client.cancel(session).await?;
            }
        }
    }
}

/// Runs the persistent-daemon line client, detaching its session on exit.
pub(super) async fn run(launch: xi_tui::Launch, paths: &Paths) -> Result<()> {
    let xi_tui::Launch {
        client,
        mut notifications,
        template,
        attached,
        ..
    } = launch;
    let mut session = attached.info.session;
    let mut renderer = Renderer::new(Render::Interactive);
    renderer.attach(&attached);
    renderer.line_mode = true;
    for pending in attached.pending_approvals {
        renderer.display(ClientNotification::ApprovalPending(pending), &client)?;
    }
    let result: Result<()> = async {
        std::fs::create_dir_all(&paths.state)?;
        let history = paths.state.join("history");
        let mut editor = rustyline::DefaultEditor::new()?;
        if history.exists() {
            editor.load_history(&history)?;
        }
        loop {
            let prompt = renderer.input_prompt().to_owned();
            renderer.reading = true;
            let input = tokio::task::spawn_blocking(move || {
                let line = editor.readline(&prompt);
                (editor, line)
            });
            tokio::pin!(input);
            let (returned, line) = loop {
                tokio::select! {
                    biased;
                    Some(notification) = notifications.recv() => renderer.display(notification, &client)?,
                    result = &mut input => break result?,
                }
            };
            editor = returned;
            renderer.reading = false;
            if let Some(call) = renderer.pending_input.pop_front() {
                match line {
                    Ok(answer) => {
                        if let Err(error) = client
                            .resolve(session, call, matches!(answer.trim(), "y" | "Y"), Remember::Once)
                            .await
                        {
                            eprintln!("xi: {error}");
                        }
                    }
                    Err(ReadlineError::Interrupted) => client.cancel(session).await?,
                    Err(ReadlineError::Eof) => {
                        client.resolve(session, call, false, Remember::Once).await?;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
            let line = match line {
                Ok(line) => line,
                Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
                Err(error) => return Err(error.into()),
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            editor.add_history_entry(line)?;
            editor.save_history(&history)?;
            match line {
                "/quit" => break,
                "/new" => {
                    match client.create(template.clone()).await {
                        Ok(attached) => {
                            release(&client, session, renderer.head).await?;
                            session = attached.info.session;
                            renderer.attach(&attached);
                        }
                        Err(error) => eprintln!("xi: {error}"),
                    }
                    continue;
                }
                "/compact" => {
                    match client.compact(session).await {
                        Ok(result) => {
                            renderer.seen.insert(result.head);
                            renderer.head = Some(result.head);
                            eprintln!("Compacted {} estimated tokens.", result.tokens_before);
                        }
                        Err(error) => eprintln!("xi: {error}"),
                    }
                    continue;
                }
                _ => {}
            }
            if let Some(model) = line.strip_prefix("/model ") {
                match model.trim().parse::<ModelSelector>() {
                    Ok(model) => match client.set_model(session, model.clone()).await {
                        Ok(resolved) if ModelSelector::from(resolved.clone()) != model => {
                            eprintln!("{model} is {}/{}", resolved.provider, resolved.model);
                        }
                        Ok(_) => {}
                        Err(error) => eprintln!("xi: {error}"),
                    },
                    Err(error) => eprintln!("xi: {error}"),
                }
                continue;
            }
            if line == "/reasoning" {
                match client.models(session).await {
                    Ok(result) => {
                        let current = result.models.iter().find(|entry| entry.model == result.current);
                        match current.map(|entry| &entry.info) {
                            Some(info) if info.reasoning.is_empty() => {
                                println!("{} does not reason", info.id);
                            }
                            Some(info) => {
                                let levels: Vec<_> =
                                    info.reasoning.iter().map(|level| level.name()).collect();
                                println!(
                                    "Reasoning: {} (accepts {})",
                                    info.resolve(result.reasoning),
                                    levels.join(", ")
                                );
                            }
                            None => println!("Reasoning: {}", result.reasoning),
                        }
                    }
                    Err(error) => eprintln!("xi: {error}"),
                }
                continue;
            }
            if let Some(title) = line.strip_prefix("/rename ") {
                match client.rename(session, title).await {
                    Ok(()) => eprintln!("Renamed."),
                    Err(error) => eprintln!("xi: {error}"),
                }
                continue;
            }
            if let Some(level) = line.strip_prefix("/reasoning ") {
                match level.trim().parse::<Reasoning>() {
                    Ok(reasoning) => {
                        if let Err(error) = client.set_reasoning(session, reasoning).await {
                            eprintln!("xi: {error}");
                        }
                    }
                    Err(error) => eprintln!("xi: {error}"),
                }
                continue;
            }
            if let Some(command) = line.strip_prefix('/') {
                let (name, args) = command
                    .split_once(char::is_whitespace)
                    .unwrap_or((command, ""));
                match client.command(session, name, args.trim()).await {
                    Ok(Some(result)) => {
                        if let Some(message) = result.message {
                            println!("{message}");
                        }
                    }
                    Ok(None) => eprintln!(
                        "Unknown command: /{name}. Commands: /quit, /new, /compact, /rename TITLE, /model PROVIDER/MODEL, /reasoning [LEVEL]"
                    ),
                    Err(error) => eprintln!("xi: {error}"),
                }
                continue;
            }
            renderer.line_mode = false;
            let result = turn(&client, session, line.to_owned(), &mut notifications, &mut renderer).await;
            renderer.line_mode = true;
            match result {
                Ok(_) => println!(),
                Err(error) => eprintln!("xi: {error}"),
            }
        }
        editor.save_history(&history)?;
        Ok(())
    }
    .await;
    let released = release(&client, session, renderer.head).await;
    result?;
    released
}

async fn release(client: &Client, session: SessionId, head: Option<EventId>) -> Result<()> {
    let attached = match client.attach(session, head).await {
        Ok(attached) => attached,
        Err(ClientError::Rpc {
            code: RpcError::UNKNOWN_SESSION,
            ..
        }) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    client.detach(session).await?;
    if !attached.running {
        match client.close(session).await {
            Ok(()) => {}
            Err(ClientError::Rpc {
                code: RpcError::SESSION_RUNNING | RpcError::UNKNOWN_SESSION,
                ..
            }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use xi_protocol::methods::client::ResetParams;

    #[tokio::test]
    async fn synthetic_asks_prompt_and_resolve_by_connection_id() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use xi_protocol::event::{Approver, ModelRef, StopReason, Usage};
        use xi_protocol::methods::client::EventParams;
        use xi_protocol::{Event, Timestamp};

        let (connection, server) = tokio::io::duplex(16 * 1024);
        let (reader, mut writer) = tokio::io::split(server);
        let (observed, mut requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                let Some(id) = message.get("id") else {
                    continue;
                };
                let result = if message["method"] == "initialize" {
                    serde_json::json!({
                        "protocol_version": xi_protocol::PROTOCOL_VERSION,
                        "daemon_version": "test",
                        "capabilities": {"private": true},
                    })
                } else {
                    observed.send(message.clone()).unwrap();
                    serde_json::Value::Null
                };
                let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (reader, writer) = tokio::io::split(connection);
        let (client, _notifications) = Client::over(reader, writer, "test").await.unwrap();
        let session = SessionId::generate();
        let notification = |kind| {
            ClientNotification::Event(EventParams {
                session,
                event: Event {
                    id: EventId::generate(),
                    parent: None,
                    at: Timestamp(0),
                    kind,
                },
            })
        };
        let mut renderer = Renderer::new(Render::Interactive);
        renderer.session = Some(session);
        renderer.line_mode = true;
        renderer
            .display(
                notification(EventKind::AssistantMessage {
                    model: ModelRef {
                        provider: "test".into(),
                        model: "test".into(),
                    },
                    content: vec![Block::ToolUse {
                        id: "bash-call".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "curl github.com"}),
                        extra: Default::default(),
                    }],
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                }),
                &client,
            )
            .unwrap();
        renderer
            .display(
                notification(EventKind::ToolDecision {
                    call: "connection".into(),
                    action: "net github.com:443".into(),
                    decision: Decision::Ask,
                    rule: None,
                    taint: false,
                    during: Some("bash-call".into()),
                    extension: None,
                }),
                &client,
            )
            .unwrap();
        assert_eq!(
            renderer.input_prompt(),
            "bash · net github.com:443 · allow? [y/N] "
        );
        let call = renderer.pending_input.pop_front().unwrap();
        assert_eq!(call, "connection".into());
        client
            .resolve(session, call, true, Remember::Once)
            .await
            .unwrap();
        let request = requests.recv().await.unwrap();
        assert_eq!(request["method"], "approval/resolve");
        assert_eq!(request["params"]["call"], "connection");
        renderer
            .display(
                notification(EventKind::ApprovalResolved {
                    call: "connection".into(),
                    approved: true,
                    by: Approver::User,
                    remember: Remember::Once,
                }),
                &client,
            )
            .unwrap();
        assert!(renderer.pending_input.is_empty());
        assert_eq!(renderer.tools[&"bash-call".into()], "bash");
    }

    #[test]
    fn network_prompts_include_extension_names_without_changing_tool_prompts() {
        assert_eq!(
            approval_prompt("net example.org:443", Some("fetch")),
            "fetch · net example.org:443 · allow? [y/N] "
        );
        assert_eq!(
            approval_prompt("net example.org:443", None),
            "net example.org:443 · allow? [y/N] "
        );
        assert_eq!(
            approval_prompt("exec pytest", Some("bash")),
            "allow? [y/N] "
        );
    }

    #[test]
    fn json_notifications_keep_the_rpc_envelope() {
        let session = SessionId::generate();
        assert_eq!(
            notification_json(ClientNotification::Reset(ResetParams {
                session,
                attempt: 2,
                sequence: 3,
            })),
            serde_json::json!({
                "jsonrpc": "2.0", "method": "stream/reset",
                "params": {"session": session, "attempt": 2, "sequence": 3},
            }),
        );
    }
}
