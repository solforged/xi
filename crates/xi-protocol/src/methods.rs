//! Protocol methods, grouped by the peers that use them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{Block, ToolOutput};
use crate::event::{Event, ModelRef, Usage};
use crate::ids::{EventId, SessionId, ToolCallId};
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

/// Methods between a client (TUI, editor, SDK) and the kernel.
pub mod client {
    use super::*;

    request!(
        /// Client → kernel: start a new session.
        SessionCreate, "session/create", CreateParams => SessionInfo
    );
    request!(
        /// Client → kernel: resume a session file.
        SessionOpen, "session/open", OpenParams => SessionInfo
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
    notification!(
        /// Kernel → client: an event was persisted.
        EventAppended, "event/appended", EventParams
    );
    notification!(
        /// Kernel → client: streaming model output. Not persisted; the
        /// complete message arrives later as an event.
        StreamDelta, "stream/delta", DeltaParams
    );

    /// Parameters for [`SessionCreate`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CreateParams {
        /// Workspace root.
        pub cwd: PathBuf,
        /// Model to start with; the configured default when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<ModelRef>,
    }

    /// Parameters for [`SessionOpen`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct OpenParams {
        /// Session file.
        pub path: PathBuf,
    }

    /// An open session.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SessionInfo {
        /// Session identity.
        pub session: SessionId,
        /// Session file.
        pub path: PathBuf,
        /// Event the next prompt continues from.
        pub head: EventId,
        /// Every event in file order.
        pub events: Vec<Event>,
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
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct PromptResult {
        /// The head after the turn.
        pub head: EventId,
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
