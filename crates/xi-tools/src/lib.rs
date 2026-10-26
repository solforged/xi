//! Built-in tools.
//!
//! Every tool reports the boundary crossings a call would make
//! ([`Tool::crossings`]) before it runs, so the kernel can ask policy first.
//! Work inside the sandbox crosses nothing and runs without asking:
//!
//! - `read` and `search` read in-process; paths under the sandbox's readable
//!   or writable roots are inside, anything else is a [`Crossing::Read`].
//! - `write` and `edit` write in-process; paths under a writable root and
//!   not under a protected path are inside, anything else is a
//!   [`Crossing::Write`]. Paths are resolved through symlinks first, so a
//!   link cannot smuggle a write out of the workspace.
//! - `bash` runs through `xi-sandbox-exec`, and every command is a
//!   [`Crossing::Exec`] so policy can deny specific command lines.

use std::path::PathBuf;
use std::sync::Arc;

pub use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use xi_protocol::manifest::ToolSpec;
use xi_protocol::{ToolCallId, ToolOutput};
use xi_sandbox::Spec;

/// What a tool call needs from the kernel.
#[derive(Clone, Debug)]
pub struct Context {
    /// Workspace root, canonicalized.
    pub workspace: PathBuf,
    /// The session's sandbox. Its roots define what counts as inside, and
    /// `bash` runs under it.
    pub sandbox: Spec,
    /// Path of the `xi-sandbox-exec` helper.
    pub helper: PathBuf,
    /// Directory for full outputs that were truncated in context. Inside
    /// the sandbox's readable roots, so the model can `read` them.
    pub spill_dir: PathBuf,
    /// Bytes of output a call may put in context before truncating.
    pub output_limit: usize,
    /// Fires when the user cancels the turn.
    pub cancel: CancellationToken,
}

/// A boundary crossing a tool call would make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Crossing {
    /// Reading an absolute path outside the sandbox.
    Read(PathBuf),
    /// Writing an absolute path outside the sandbox or under a protected
    /// path.
    Write(PathBuf),
    /// Running a command line.
    Exec(String),
    /// Connecting to a host.
    Net(String),
}

/// The result of running a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// Output for the model, truncated to [`Context::output_limit`].
    pub output: ToolOutput,
    /// File holding the full output when it was truncated.
    pub spill: Option<PathBuf>,
}

/// A tool the model can call.
pub trait Tool: Send + Sync {
    /// Name, description and input schema shown to the model.
    fn spec(&self) -> ToolSpec;

    /// The crossings `input` would make. The kernel evaluates each against
    /// policy and only calls [`Tool::run`] when all are allowed or approved.
    ///
    /// # Errors
    /// Returns a message for the model when the input is invalid.
    fn crossings(&self, input: &Value, ctx: &Context) -> Result<Vec<Crossing>, String>;

    /// Runs the call. Failures are reported as output with `is_error` set,
    /// so the model sees them.
    fn run<'a>(&'a self, call: &'a ToolCallId, input: Value, ctx: &'a Context)
    -> BoxFuture<'a, Ran>;
}

/// The default toolset: `read`, `edit`, `write`, `bash` and `search`.
#[must_use]
pub fn builtin() -> Vec<Arc<dyn Tool>> {
    todo!("tools lane")
}

/// A built-in tool by name, including ones not in the default set.
#[must_use]
pub fn by_name(name: &str) -> Option<Arc<dyn Tool>> {
    let _ = name;
    todo!("tools lane")
}

/// Truncates `text` to `ctx.output_limit` bytes for context. When it is
/// longer, the full text is written to `ctx.spill_dir` under the call id and
/// the returned text says where. Used by built-in tools and by the kernel
/// for extension tool output.
#[must_use]
pub fn truncate(call: &ToolCallId, text: String, ctx: &Context) -> (String, Option<PathBuf>) {
    let _ = (call, text, ctx);
    todo!("tools lane")
}
