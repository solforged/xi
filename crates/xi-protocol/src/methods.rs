//! Protocol methods, grouped by the peers that use them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{Block, ToolOutput};
use crate::event::{Event, ModelRef, Usage};
use crate::ids::{EventId, SessionId, ToolCallId};
use crate::model::{ModelInfo, Reasoning};
use crate::rpc::{NotificationMethod, RequestMethod};

macro_rules! request {
    ($(#[$meta:meta])* $name:ident, $method:literal, $params:ty => $result:ty) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub enum $name {}

        impl RequestMethod for $name {
            const METHOD: &'static str = $method;
            type Params = $params;
            type Result = $result;
        }
    };
}

macro_rules! notification {
    ($(#[$meta:meta])* $name:ident, $method:literal, $params:ty) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub enum $name {}

        impl NotificationMethod for $name {
            const METHOD: &'static str = $method;
            type Params = $params;
        }
    };
}

/// Methods between a client (TUI, editor, SDK) and the daemon.
pub mod client {
    use super::*;

    request!(
        /// Client → daemon: negotiate the protocol version.
        Initialize, "initialize", InitializeParams => InitializeResult
    );
    request!(
        /// Client → kernel: start a new session.
        SessionCreate, "session/create", CreateParams => AttachResult
    );
    request!(
        /// Client → kernel: resume a session file.
        SessionOpen, "session/open", OpenParams => AttachResult
    );
    request!(
        /// Client → kernel: run a turn from the current head. Resolves when
        /// the turn ends; events stream as notifications meanwhile.
        SessionPrompt, "session/prompt", PromptParams => PromptResult
    );
    request!(
        /// Client → kernel: stop the running turn.
        SessionCancel, "session/cancel", SessionRef => ()
    );
    request!(
        /// Client → kernel: move the head to an existing event. The next
        /// prompt branches from there.
        SessionCheckout, "session/checkout", CheckoutParams => ()
    );
    request!(
        /// Client → kernel: answer a pending `ask` decision.
        ApprovalResolve, "approval/resolve", ResolveParams => ()
    );
    request!(
        /// Client → kernel: summarise older context now, at an idle prompt.
        SessionCompact, "session/compact", SessionRef => CompactResult
    );
    request!(
        /// Client → daemon: attach to a live session and replay its events.
        SessionAttach, "session/attach", AttachParams => AttachResult
    );
    request!(
        /// Client → daemon: stop receiving a session's events.
        SessionDetach, "session/detach", SessionRef => ()
    );
    request!(
        /// Client → daemon: list live and saved sessions.
        SessionList, "session/list", ListParams => Vec<SessionSummary>
    );
    request!(
        /// Client → daemon: close an idle session's worker.
        SessionClose, "session/close", SessionRef => ()
    );
    request!(
        /// Client → daemon: select a session's model.
        SessionSetModel, "session/set_model", SetModelParams => ()
    );
    request!(
        /// Client → daemon: run an extension slash command.
        SessionCommand, "session/command", SessionCommandParams => Option<super::extension::CommandResult>
    );
    request!(
        /// Client → daemon: list unresolved approvals.
        ApprovalList, "approval/list", () => Vec<PendingApproval>
    );
    request!(
        /// Client → daemon: inspect daemon and scheduler state.
        DaemonStatus, "daemon/status", () => StatusResult
    );
    request!(
        /// Client → daemon: request shutdown.
        DaemonShutdown, "daemon/shutdown", ShutdownParams => ()
    );
    notification!(
        /// Kernel → client: an event was persisted.
        EventAppended, "event/appended", EventParams
    );
    notification!(
        /// Kernel → client: streaming model output. Not persisted; the
        /// complete message arrives later as an event.
        StreamDelta, "stream/delta", DeltaParams
    );
    notification!(
        /// Daemon → client: discard partial output from the previous attempt.
        StreamReset, "stream/reset", ResetParams
    );
    notification!(
        /// Daemon → client: a session's worker exited.
        SessionExited, "session/exited", ExitedParams
    );
    notification!(
        /// Daemon → client: a call needs approval.
        ApprovalPending, "approval/pending", PendingApproval
    );

    /// Parameters for [`Initialize`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct InitializeParams {
        /// The client's [`crate::PROTOCOL_VERSION`].
        pub protocol_version: u32,
        /// Client identity.
        pub client: ClientInfo,
    }

    /// Identifies a client application.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ClientInfo {
        /// Application name.
        pub name: String,
        /// Application version.
        pub version: String,
    }

    /// Result of [`Initialize`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct InitializeResult {
        /// The daemon's [`crate::PROTOCOL_VERSION`].
        pub protocol_version: u32,
        /// Daemon binary version.
        pub daemon_version: String,
        /// Available daemon capabilities.
        pub capabilities: Capabilities,
    }

    /// Capabilities of the connected daemon.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Capabilities {
        /// True for a private instance with no listener.
        pub private: bool,
    }

    /// Per-invocation settings. Paths must be absolute; the worker rejects relative ones.
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SessionOptions {
        /// Extra full-authority config layers (`--config`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub configs: Vec<PathBuf>,
        /// Session directory override (`--session-dir`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_dir: Option<PathBuf>,
        /// Reasoning depth override (`--reasoning`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning: Option<Reasoning>,
        /// Enabled tools override (`--tools`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tools: Option<Vec<String>>,
        /// Extra extension directories (`--extension`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub extensions: Vec<PathBuf>,
        /// Turn limit override (`--max-turns`); absent means 200.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_turns: Option<usize>,
        /// How unresolved asks are handled.
        #[serde(default)]
        pub approval: ApprovalPolicy,
    }

    /// Approval behaviour for a session invocation.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ApprovalPolicy {
        /// Ask an attached client.
        #[default]
        Client,
        /// Approve asks without a client.
        Allow,
        /// Deny asks without a client.
        Deny,
    }

    /// Parameters for [`SessionCreate`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CreateParams {
        /// Workspace root.
        pub cwd: PathBuf,
        /// Model to start with; the configured default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<ModelRef>,
        /// Per-invocation settings.
        #[serde(default)]
        pub options: SessionOptions,
    }

    /// Parameters for [`SessionOpen`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct OpenParams {
        /// Session file.
        pub path: PathBuf,
        /// Per-invocation settings.
        #[serde(default)]
        pub options: SessionOptions,
    }

    /// Parameters for [`SessionAttach`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AttachParams {
        /// Session to attach.
        pub session: SessionId,
        /// Replay only events after this event; absent replays the whole log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub since: Option<EventId>,
    }

    /// Session snapshot returned when attaching, creating or opening.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AttachResult {
        /// Session metadata.
        pub info: SessionDetails,
        /// Events after `since` in file order, or the whole log without it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub events: Vec<Event>,
        /// In-flight assistant blocks of the current attempt.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub partial: Vec<Block>,
        /// Whether a turn is running.
        pub running: bool,
        /// Calls awaiting approval.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub pending_approvals: Vec<PendingApproval>,
    }

    /// Metadata for a live session.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SessionDetails {
        /// Session identity.
        pub session: SessionId,
        /// Session file.
        pub path: PathBuf,
        /// Event the next prompt continues from.
        pub head: EventId,
        /// Workspace root.
        pub cwd: PathBuf,
        /// Whether the workspace is trusted.
        pub trusted: bool,
        /// Selected model.
        pub model: ModelRef,
        /// Registry metadata for the selected model, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model_info: Option<ModelInfo>,
        /// Parent session for a child session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent_session: Option<SessionId>,
    }

    /// Filters for [`SessionList`].
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ListParams {
        /// Match this workspace root exactly.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cwd: Option<PathBuf>,
        /// Match sessions whose turn-running state equals this value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub running: Option<bool>,
    }

    /// A live or saved session in the daemon index.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SessionSummary {
        /// Session identity.
        pub session: SessionId,
        /// Session file.
        pub path: PathBuf,
        /// Workspace root.
        pub cwd: PathBuf,
        /// Selected model.
        pub model: ModelRef,
        /// Current head, absent when no worker has the session open.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub head: Option<EventId>,
        /// Whether a turn is running.
        pub running: bool,
        /// Number of calls awaiting approval.
        pub pending_approvals: usize,
        /// Parent session for a child session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent_session: Option<SessionId>,
    }

    /// Parameters for [`SessionSetModel`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SetModelParams {
        /// Session to update.
        pub session: SessionId,
        /// Model to select.
        pub model: ModelRef,
    }

    /// Parameters for [`SessionCommand`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SessionCommandParams {
        /// Session receiving the command.
        pub session: SessionId,
        /// Command name without the slash.
        pub name: String,
        /// Raw command arguments.
        pub args: String,
    }

    /// A tool call awaiting approval.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PendingApproval {
        /// Session containing the call.
        pub session: SessionId,
        /// Call awaiting approval.
        pub call: ToolCallId,
        /// Tool name.
        pub tool: String,
        /// Human-readable action.
        pub action: String,
        /// Matched policy rule, when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rule: Option<String>,
    }

    /// Result of [`DaemonStatus`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct StatusResult {
        /// Daemon binary version.
        pub version: String,
        /// Daemon process id.
        pub pid: u32,
        /// Seconds since startup.
        pub uptime_secs: u64,
        /// Live worker count.
        pub workers: usize,
        /// Connected client count.
        pub clients: usize,
        /// Model endpoint scheduler state.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub leases: Vec<LeaseStatus>,
    }

    /// Scheduler state for one model endpoint.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LeaseStatus {
        /// Provider endpoint.
        pub endpoint: String,
        /// Configured concurrency limit.
        pub concurrency: usize,
        /// Active lease count.
        pub active: usize,
        /// Waiting lease count.
        pub waiting: usize,
    }

    /// Parameters for [`DaemonShutdown`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ShutdownParams {
        /// When to stop.
        pub when: ShutdownWhen,
    }

    /// Timing of a daemon shutdown.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ShutdownWhen {
        /// Stop once no workers remain.
        Idle,
        /// Cancel and close all workers, then stop.
        Now,
    }

    /// Parameters for [`StreamReset`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ResetParams {
        /// Session whose partial output must be discarded.
        pub session: SessionId,
        /// New model request attempt number.
        pub attempt: u32,
    }

    /// Parameters for [`SessionExited`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ExitedParams {
        /// Session whose worker exited.
        pub session: SessionId,
        /// Process exit code, absent when unknown or killed by signal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<i32>,
        /// Explanation of the exit, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
    }

    /// Identifies a session.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SessionRef {
        /// The session.
        pub session: SessionId,
    }

    /// Parameters for [`SessionPrompt`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct PromptParams {
        /// The session.
        pub session: SessionId,
        /// The user message.
        pub content: Vec<Block>,
    }

    /// Result of [`SessionPrompt`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PromptResult {
        /// The head after the turn.
        pub head: EventId,
        /// Whether a failure or request limit ended the turn.
        #[serde(default)]
        pub failed: bool,
        /// Whether the turn was cancelled.
        #[serde(default)]
        pub cancelled: bool,
        /// Text from the final assistant response only.
        #[serde(default)]
        pub text: String,
    }

    /// Result of [`SessionCompact`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CompactResult {
        /// The `compaction` event, now the head.
        pub head: EventId,
        /// Context size before compaction.
        pub tokens_before: u64,
    }

    /// Parameters for [`SessionCheckout`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CheckoutParams {
        /// The session.
        pub session: SessionId,
        /// The new head.
        pub event: EventId,
    }

    /// Parameters for [`ApprovalResolve`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ResolveParams {
        /// The session.
        pub session: SessionId,
        /// The call awaiting approval.
        pub call: ToolCallId,
        /// Whether the call may run.
        pub approved: bool,
    }

    /// Parameters for [`EventAppended`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct EventParams {
        /// The session.
        pub session: SessionId,
        /// The persisted event.
        pub event: Event,
    }

    /// Parameters for [`StreamDelta`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct DeltaParams {
        /// The session.
        pub session: SessionId,
        /// The increment.
        pub delta: Delta,
        /// Model request attempt, starting at zero.
        #[serde(default)]
        pub attempt: u32,
    }

    /// An increment of streaming model output.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Delta {
        /// Response text.
        Text {
            /// Appended text.
            text: String,
        },
        /// Reasoning text.
        Thinking {
            /// Appended text.
            text: String,
        },
        /// Tool call arguments, as raw JSON fragments.
        ToolInput {
            /// The call being streamed.
            call: ToolCallId,
            /// Appended JSON text.
            partial_json: String,
        },
    }
}

/// Internal methods between the daemon and a session worker.
pub mod worker {
    use super::client::{AttachResult, CreateParams, OpenParams};
    use super::*;

    request!(
        /// Daemon → worker: start one session; the result signals readiness.
        WorkerStart, "worker/start", WorkerStartParams => AttachResult
    );
    request!(
        /// Worker → daemon: wait for permission to request a model endpoint.
        ModelLease, "model/lease", LeaseParams => ()
    );
    notification!(
        /// Worker → daemon: release a model endpoint lease.
        ModelRelease, "model/release", LeaseParams
    );

    /// Session startup mode and parameters.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "mode", rename_all = "snake_case")]
    pub enum WorkerStartParams {
        /// Create a new session.
        Create(CreateParams),
        /// Open an existing session.
        Open(OpenParams),
    }

    /// Identifies the endpoint whose model lease is acquired or released.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LeaseParams {
        /// Provider endpoint.
        pub endpoint: String,
    }
}

/// Methods between the kernel and an extension process.
pub mod extension {
    use super::*;
    use crate::manifest::Registration;

    request!(
        /// Kernel → extension: handshake. The extension answers with what it
        /// registers.
        Initialize, "initialize", InitializeParams => Registration
    );
    request!(
        /// Kernel → extension: run a registered tool.
        ToolInvoke, "tool/invoke", InvokeParams => ToolOutput
    );
    request!(
        /// Kernel → extension: run a hook the extension registered.
        HookRun, "hook/run", HookParams => HookOutcome
    );
    request!(
        /// Kernel → extension: run a registered slash command.
        CommandRun, "command/run", CommandParams => CommandResult
    );
    request!(
        /// Kernel → extension: prepare to exit. The kernel closes stdin after
        /// the response.
        Shutdown, "shutdown", () => ()
    );
    request!(
        /// Extension → kernel: persist extension state as a `custom` event at
        /// the session head.
        AppendCustom, "session/append_custom", AppendCustomParams => EventId
    );
    request!(
        /// Extension → kernel: run a child session to completion and return
        /// its final answer. Needs the `subagents` capability. Requests may
        /// run concurrently, including while a `tool/invoke` from the same
        /// session is pending.
        AgentRun, "agent/run", AgentRunParams => AgentRunResult
    );
    notification!(
        /// Extension → kernel: diagnostic output.
        Log, "log", LogParams
    );

    /// Parameters for [`Initialize`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct InitializeParams {
        /// The kernel's [`crate::PROTOCOL_VERSION`].
        pub protocol_version: u32,
        /// Version of the xi binary.
        pub xi_version: String,
        /// Workspace root.
        pub workspace: PathBuf,
    }

    /// Parameters for [`ToolInvoke`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct InvokeParams {
        /// The session the call belongs to.
        pub session: SessionId,
        /// The call.
        pub call: ToolCallId,
        /// Registered tool name.
        pub tool: String,
        /// Input after `pre_tool` hooks.
        pub input: Value,
    }

    /// Parameters for [`HookRun`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct HookParams {
        /// The session.
        pub session: SessionId,
        /// The hook point and its data, tagged by `point`.
        #[serde(flatten)]
        pub event: HookEvent,
    }

    /// Data for each hook point.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "point", rename_all = "snake_case")]
    pub enum HookEvent {
        /// Before a tool runs.
        PreTool {
            /// The call.
            call: ToolCallId,
            /// Tool name.
            tool: String,
            /// Input as it stands after earlier hooks.
            input: Value,
        },
        /// After a tool runs.
        PostTool {
            /// The call.
            call: ToolCallId,
            /// Tool name.
            tool: String,
            /// Input the tool ran with.
            input: Value,
            /// Output as it stands after earlier hooks.
            output: ToolOutput,
        },
        /// After a turn ends.
        TurnEnd {
            /// The head after the turn.
            head: EventId,
        },
        /// Before older context is summarised.
        Compact {
            /// Context size before compaction.
            tokens_before: u64,
            /// The previous summary on this branch, if any.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            previous_summary: Option<String>,
        },
    }

    /// A hook's answer. The kernel rejects outcomes that do not fit the hook
    /// point, such as rewriting input in `post_tool`.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "outcome", rename_all = "snake_case")]
    pub enum HookOutcome {
        /// Proceed unchanged.
        Continue,
        /// Refuse the tool call (`pre_tool` only).
        Deny {
            /// Reason shown to the model.
            reason: String,
        },
        /// Replace the tool input (`pre_tool` only). Policy is evaluated on
        /// the rewritten input.
        RewriteInput {
            /// The new input.
            input: Value,
        },
        /// Replace the tool output (`post_tool` only).
        ReplaceOutput {
            /// The new output.
            output: ToolOutput,
        },
        /// Supply the compaction summary (`compact` only). The kernel skips
        /// its own summary request.
        ReplaceSummary {
            /// The summary.
            summary: String,
        },
    }

    /// Parameters for [`CommandRun`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CommandParams {
        /// The session the command was typed in.
        pub session: SessionId,
        /// Command name without the slash.
        pub name: String,
        /// Everything after the command name.
        pub args: String,
    }

    /// Result of [`CommandRun`].
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CommandResult {
        /// Text shown to the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
    }

    /// Parameters for [`AppendCustom`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AppendCustomParams {
        /// The session.
        pub session: SessionId,
        /// Extension-defined discriminator.
        pub kind: String,
        /// Extension-defined payload.
        pub data: Value,
    }

    /// Parameters for [`AgentRun`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AgentRunParams {
        /// The parent session.
        pub session: SessionId,
        /// The child's first user message.
        pub prompt: String,
        /// Built-in tools the child may use, a subset of the parent's.
        /// Absent means all of the parent's built-in tools. Extension tools
        /// are never offered to a child, so subagents do not nest.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tools: Option<Vec<String>>,
        /// Model for the child; the parent's when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<ModelRef>,
    }

    /// Result of [`AgentRun`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct AgentRunResult {
        /// The child session.
        pub session: SessionId,
        /// The child log file.
        pub path: PathBuf,
        /// Text of the child's final assistant message.
        pub text: String,
        /// Whether a failure or request limit ended the child's turn.
        pub failed: bool,
        /// Summed token use of every child request.
        pub usage: Usage,
    }

    /// Parameters for [`Log`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LogParams {
        /// Severity.
        pub level: LogLevel,
        /// The message.
        pub message: String,
    }

    /// Log severity.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum LogLevel {
        /// Verbose detail.
        Debug,
        /// Normal operation.
        Info,
        /// Something unexpected that did not stop the extension.
        Warn,
        /// A failure.
        Error,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::client::*;
    use super::worker::*;
    use crate::event::{EventKind, ModelRef};
    use crate::rpc::{Notification, Request, RequestId};
    use crate::{Block, Event, EventId, SessionId, Timestamp};

    #[test]
    fn initialize_params_wire_shape() {
        let params = InitializeParams {
            protocol_version: crate::PROTOCOL_VERSION,
            client: ClientInfo {
                name: "xi-tui".into(),
                version: "1.2.3".into(),
            },
        };
        let wire = json!({
            "protocol_version": 0,
            "client": {"name": "xi-tui", "version": "1.2.3"},
        });
        assert_eq!(serde_json::to_value(&params).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<InitializeParams>(wire.clone()).unwrap(),
            params
        );
        assert_eq!(
            serde_json::to_value(
                Request::new::<Initialize>(RequestId::Number(1), &params).unwrap()
            )
            .unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": wire}),
        );
    }

    #[test]
    fn attach_result_wire_shape() {
        let session = SessionId::generate();
        let head = EventId::generate();
        let model = ModelRef {
            provider: "test".into(),
            model: "model".into(),
        };
        let result = AttachResult {
            info: SessionDetails {
                session,
                path: "/sessions/test.jsonl".into(),
                head,
                cwd: "/work".into(),
                trusted: true,
                model,
                model_info: None,
                parent_session: None,
            },
            events: vec![Event {
                id: head,
                parent: None,
                at: Timestamp(7),
                kind: EventKind::UserMessage {
                    content: vec![Block::Text {
                        text: "hello".into(),
                    }],
                },
            }],
            partial: vec![Block::Text {
                text: "answer".into(),
            }],
            running: true,
            pending_approvals: vec![PendingApproval {
                session,
                call: "call-1".into(),
                tool: "bash".into(),
                action: "git status".into(),
                rule: Some("user#1".into()),
            }],
        };
        let wire = json!({
            "info": {
                "session": session, "path": "/sessions/test.jsonl", "head": head,
                "cwd": "/work", "trusted": true,
                "model": {"provider": "test", "model": "model"},
            },
            "events": [{
                "id": head, "at": 7, "type": "user_message",
                "content": [{"type": "text", "text": "hello"}],
            }],
            "partial": [{"type": "text", "text": "answer"}],
            "running": true,
            "pending_approvals": [{
                "session": session, "call": "call-1", "tool": "bash",
                "action": "git status", "rule": "user#1",
            }],
        });
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<AttachResult>(wire).unwrap(),
            result
        );
        let minimal = json!({"info": result.info, "running": false});
        let empty: AttachResult = serde_json::from_value(minimal.clone()).unwrap();
        assert!(empty.events.is_empty());
        assert!(empty.partial.is_empty());
        assert!(empty.pending_approvals.is_empty());
        assert_eq!(serde_json::to_value(empty).unwrap(), minimal);
    }

    #[test]
    fn prompt_result_includes_turn_status_and_final_text() {
        let head = EventId::generate();
        let result = PromptResult {
            head,
            failed: true,
            cancelled: false,
            text: "final response".into(),
        };
        let wire = json!({
            "head": head, "failed": true, "cancelled": false, "text": "final response",
        });
        assert_eq!(serde_json::to_value(&result).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<PromptResult>(wire).unwrap(),
            result
        );
        let minimal: PromptResult = serde_json::from_value(json!({"head": head})).unwrap();
        assert!(!minimal.failed);
        assert!(!minimal.cancelled);
        assert!(minimal.text.is_empty());
    }

    #[test]
    fn session_options_default_from_empty_object() {
        let options: SessionOptions = serde_json::from_value(json!({})).unwrap();
        assert_eq!(options, SessionOptions::default());
        assert_eq!(options.approval, ApprovalPolicy::Client);
        assert_eq!(
            serde_json::to_value(options).unwrap(),
            json!({"approval": "client"})
        );
        let create: CreateParams = serde_json::from_value(json!({"cwd": "/work"})).unwrap();
        assert_eq!(create.options, SessionOptions::default());
        let open: OpenParams =
            serde_json::from_value(json!({"path": "/sessions/test.jsonl"})).unwrap();
        assert_eq!(open.options, SessionOptions::default());
    }

    #[test]
    fn worker_start_params_are_internally_tagged() {
        let create = WorkerStartParams::Create(CreateParams {
            cwd: "/work".into(),
            model: None,
            options: SessionOptions::default(),
        });
        let create_wire = json!({
            "mode": "create", "cwd": "/work", "options": {"approval": "client"},
        });
        assert_eq!(serde_json::to_value(&create).unwrap(), create_wire);
        assert_eq!(
            serde_json::from_value::<WorkerStartParams>(create_wire).unwrap(),
            create
        );
        let open = WorkerStartParams::Open(OpenParams {
            path: "/sessions/test.jsonl".into(),
            options: SessionOptions::default(),
        });
        let open_wire = json!({
            "mode": "open", "path": "/sessions/test.jsonl",
            "options": {"approval": "client"},
        });
        assert_eq!(serde_json::to_value(&open).unwrap(), open_wire);
        assert_eq!(
            serde_json::from_value::<WorkerStartParams>(open_wire).unwrap(),
            open
        );
    }

    #[test]
    fn reset_params_wire_shape() {
        let params = ResetParams {
            session: SessionId::generate(),
            attempt: 2,
        };
        let wire = json!({"session": params.session, "attempt": 2});
        assert_eq!(serde_json::to_value(&params).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ResetParams>(wire.clone()).unwrap(),
            params
        );
        assert_eq!(
            serde_json::to_value(Notification::new::<StreamReset>(&params).unwrap()).unwrap(),
            json!({"jsonrpc": "2.0", "method": "stream/reset", "params": wire}),
        );
    }

    #[test]
    fn delta_params_without_attempt_defaults_to_zero() {
        let session = SessionId::generate();
        let wire = json!({
            "session": session, "delta": {"type": "text", "text": "hello"},
        });
        let params: DeltaParams = serde_json::from_value(wire).unwrap();
        assert_eq!(params.session, session);
        assert_eq!(params.attempt, 0);
        assert_eq!(
            serde_json::to_value(params).unwrap(),
            json!({
                "session": session, "delta": {"type": "text", "text": "hello"},
                "attempt": 0,
            }),
        );
    }
}
