//! Wire and on-disk types shared by the xi kernel, clients and extensions.
//!
//! - [`event`]: session log events, one JSON object per line on disk.
//! - [`content`]: message and tool output blocks.
//! - [`rpc`]: the JSON-RPC 2.0 envelope and typed method traits.
//! - [`methods`]: every protocol method, grouped by peer.
//! - [`manifest`]: extension package manifests and runtime registration.
//! - [`model`]: provider-neutral model metadata and reasoning settings.
//! - [`runtime`]: per-user daemon runtime paths.
//! - [`skill`]: explicit skill invocations in user messages.

pub mod content;
pub mod event;
pub mod ids;
pub mod manifest;
pub mod methods;
pub mod model;
pub mod rpc;
pub mod runtime;
pub mod skill;

use std::pin::Pin;

pub use content::{Block, OutputBlock, ProviderTag, SANDBOX_START_FAILED, Signature, ToolOutput};
pub use event::{Decision, Event, EventKind, Remember};
pub use ids::{EventId, ExtensionId, SessionId, Timestamp, ToolCallId};

/// Protocol version spoken by this crate. Version 0 makes no compatibility promises.
pub const PROTOCOL_VERSION: u32 = 0;

/// A boxed, sendable future, the return type of the object-safe async
/// traits (`Provider`, `Tool`, `Spawner`) that span crates.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
