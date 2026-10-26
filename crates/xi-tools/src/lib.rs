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
//!
//! `read` and `search` return `LINE:HH|text` anchors, where `HH` is the
//! leading byte of xxh3-64. `edit` validates all anchors against one snapshot
//! before writing. Offsets for `read` are one-based. Shell output is merged
//! at the pipe, bounded in memory, and spilled before it can grow unbounded.

mod bash;
mod files;
mod output;
mod paths;
mod search;

#[cfg(test)]
mod tests;

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
    fn run<'a>(
        &'a self,
        call: &'a ToolCallId,
        input: Value,
        ctx: &'a Context,
    ) -> BoxFuture<'a, Ran>;
}

/// The default toolset: `read`, `edit`, `write`, `bash` and `search`.
#[must_use]
pub fn builtin() -> Vec<Arc<dyn Tool>> {
    ["read", "edit", "write", "bash", "search"]
        .into_iter()
        .map(|name| by_name(name).expect("built-in tool"))
        .collect()
}

/// A built-in tool by name, including ones not in the default set.
#[must_use]
pub fn by_name(name: &str) -> Option<Arc<dyn Tool>> {
    let kind = match name {
        "read" => Kind::Read,
        "edit" => Kind::Edit,
        "write" => Kind::Write,
        "bash" => Kind::Bash,
        "search" => Kind::Search,
        "replace" => Kind::Replace,
        _ => return None,
    };
    Some(Arc::new(kind))
}

/// Truncates `text` to `ctx.output_limit` bytes for context. When it is
/// longer, the full text is written to `ctx.spill_dir` under the call id and
/// the returned text says where. Used by built-in tools and by the kernel
/// for extension tool output.
#[must_use]
pub fn truncate(call: &ToolCallId, text: String, ctx: &Context) -> (String, Option<PathBuf>) {
    output::truncate(call, text, ctx)
}

#[derive(Clone, Copy)]
enum Kind {
    Read,
    Edit,
    Write,
    Bash,
    Search,
    Replace,
}

impl Tool for Kind {
    fn spec(&self) -> ToolSpec {
        use serde_json::json;
        let (name, description, properties, required) = match self {
            Self::Read => (
                "read",
                "Read text as LINE:HH|content anchors, list directories, or view images. offset is a 1-based line (default 1), limit defaults to 2000. Long lines are clipped.",
                json!({"path":{"type":"string"}, "offset":{"type":"integer","minimum":1}, "limit":{"type":"integer","minimum":1}}),
                vec!["path"],
            ),
            Self::Edit => (
                "edit",
                "Edit using read/search anchors from the current file. Replace inclusive start..end (end defaults to start), or insert after an anchor (\"0\" inserts at top). lines is an array of new lines; [] deletes. Anchors must match, edits cannot overlap. Preserves line endings.",
                json!({"path":{"type":"string"}, "edits":{"type":"array","minItems":1,"items":{"oneOf":[
                    {"type":"object","properties":{"start":{"type":"string"},"end":{"type":"string"},"lines":{"type":"array","items":{"type":"string"}}},"required":["start","lines"],"additionalProperties":false},
                    {"type":"object","properties":{"after":{"type":"string"},"lines":{"type":"array","items":{"type":"string"}}},"required":["after","lines"],"additionalProperties":false}
                ]}}}),
                vec!["path", "edits"],
            ),
            Self::Write => (
                "write",
                "Create or overwrite a UTF-8 text file. Creates missing parent directories. Returns byte and line counts. Prefer edit for existing files.",
                json!({"path":{"type":"string"}, "content":{"type":"string"}}),
                vec!["path", "content"],
            ),
            Self::Bash => (
                "bash",
                "Run a shell command in the workspace sandbox with no stdin. timeout is seconds (default 120, maximum 600). Returns merged stdout/stderr and exit status. Timeout or cancellation kills the process group. Large output is spilled to a readable file.",
                json!({"command":{"type":"string"}, "timeout":{"type":"integer","minimum":1,"maximum":600}}),
                vec!["command"],
            ),
            Self::Search => (
                "search",
                "Search a regex and return file paths with LINE:HH|text anchors. Without pattern, glob is required and matching files are listed. Respects ignore files and skips hidden files. path defaults to the workspace; limit defaults to 200. Paths are workspace-relative.",
                json!({"pattern":{"type":"string"}, "path":{"type":"string"}, "glob":{"type":"string"}, "ignore_case":{"type":"boolean"}, "limit":{"type":"integer","minimum":1}}),
                vec![],
            ),
            Self::Replace => (
                "replace",
                "Replace an exact string in a text file. old must occur exactly once unless all is true. Empty old is invalid.",
                json!({"path":{"type":"string"}, "old":{"type":"string"}, "new":{"type":"string"}, "all":{"type":"boolean"}}),
                vec!["path", "old", "new"],
            ),
        };
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        }
    }

    fn crossings(&self, input: &Value, ctx: &Context) -> Result<Vec<Crossing>, String> {
        let (path, write) = match self {
            Self::Read => (files::read_input(input)?.path, false),
            Self::Edit => (files::edit_input(input)?.path, true),
            Self::Write => (files::write_input(input)?.path, true),
            Self::Replace => (files::replace_input(input)?.path, true),
            Self::Search => (search::input(input)?.path, false),
            Self::Bash => return Ok(vec![Crossing::Exec(bash::input(input)?.command)]),
        };
        paths::crossings(&path, write, ctx)
    }

    fn run<'a>(
        &'a self,
        call: &'a ToolCallId,
        input: Value,
        ctx: &'a Context,
    ) -> BoxFuture<'a, Ran> {
        Box::pin(async move {
            if let Err(error) = self.crossings(&input, ctx) {
                return output::text(call, error, true, ctx);
            }
            if matches!(self, Self::Bash) {
                return match bash::run(call, &input, ctx).await {
                    Ok(ran) => ran,
                    Err(error) => output::text(call, error, true, ctx),
                };
            }
            let kind = *self;
            let call_owned = call.clone();
            let context = ctx.clone();
            let result = tokio::task::spawn_blocking(move || match kind {
                Self::Read => files::read(&call_owned, &input, &context),
                Self::Edit => files::edit(&call_owned, &input, &context),
                Self::Write => files::write(&call_owned, &input, &context),
                Self::Search => search::run(&call_owned, &input, &context),
                Self::Replace => files::replace(&call_owned, &input, &context),
                Self::Bash => unreachable!(),
            })
            .await;
            match result {
                Ok(Ok(ran)) => ran,
                Ok(Err(error)) => output::text(call, error, true, ctx),
                Err(error) => output::text(call, format!("Tool worker failed: {error}"), true, ctx),
            }
        })
    }
}

fn parse<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, String> {
    T::deserialize(input).map_err(|error| format!("Invalid input: {error}"))
}
