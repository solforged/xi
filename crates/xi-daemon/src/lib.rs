//! The per-user xi daemon.
//!
//! The daemon routes client requests to one worker process per open session,
//! keeps the session index, the approvals inbox and the model-request
//! scheduler, and never calls models or runs tools itself.

pub mod activation;
pub mod runtime_dir;
pub mod units;
