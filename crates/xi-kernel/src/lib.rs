//! The xi agent loop, stable context, layered configuration and client server.
//!
//! Configuration uses `default_model = "provider/model"` for selection and
//! `[[model]]` for registry metadata. Global and explicit files have full
//! authority. Project policy only tightens; project settings and local
//! authority require an exact canonical root in `$XDG_CONFIG_HOME/xi/trust.toml`.
//!
//! Each open session owns one durable tree log and one cached system prompt.
//! Concurrent prompts steer the current turn only at model boundaries, after
//! all tool results have been persisted. Cancellation retains unconsumed
//! steering for the next prompt. Transport closure explicitly rejects it.

pub mod config;
pub mod context;
mod egress;
pub mod extensions;
pub mod sandbox;
pub mod settings;
pub mod worker;

mod session;

pub use config::Approvals;
pub(crate) use session::approvals::PendingAsk;
pub use session::{
    ApprovalMode, Backend, Error, EventBus, Leaser, Notification, Result, Session, SessionSnapshot,
    TurnOptions, TurnOutcome, toolset,
};
pub(crate) use session::{State, error_output, is_tool_name};
pub use session::{compaction, title};
