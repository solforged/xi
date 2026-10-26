//! Extension packages.
//!
//! Capabilities are static: the kernel reads them from the package manifest
//! to build the sandbox before the extension starts. What an extension
//! registers is dynamic and arrives in the `initialize` result.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::ExtensionId;

/// The manifest shipped in an extension package (`xi-extension.toml`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    /// Package name, unique among installed extensions.
    pub name: ExtensionId,
    /// Package version.
    pub version: String,
    /// Command line that starts the extension, run from the package root.
    pub run: Vec<String>,
    /// Access the sandbox grants. Anything not listed is denied.
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// Sandbox grants for an extension. Filesystem entries are literal roots.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    /// Readable paths outside the package directory.
    pub fs_read: Vec<String>,
    /// Writable paths.
    pub fs_write: Vec<String>,
    /// Reachable hosts, through the egress proxy.
    pub net: Vec<String>,
    /// Environment variables passed through from the kernel's environment.
    pub env: Vec<String>,
    /// Whether the extension may run child sessions through `agent/run`.
    pub subagents: bool,
}

/// What an extension registers, returned from `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Registration {
    /// Tools offered to the model.
    pub tools: Vec<ToolSpec>,
    /// Loop points the extension wants to hook.
    pub hooks: Vec<HookPoint>,
    /// Slash commands offered to the user.
    pub commands: Vec<CommandSpec>,
}

/// A tool definition as presented to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name, unique across all extensions.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: Value,
}

/// A slash command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Name without the leading slash.
    pub name: String,
    /// One-line description for completion menus.
    pub description: String,
}

/// Points in the agent loop an extension can hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    /// Before a tool runs. May deny the call or rewrite its input.
    PreTool,
    /// After a tool runs. May replace the output.
    PostTool,
    /// After a turn ends. Observe only.
    TurnEnd,
    /// Before the kernel summarises older context. May supply the summary.
    Compact,
}
