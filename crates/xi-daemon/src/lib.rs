//! The per-user xi daemon.
//!
//! The daemon routes client requests to one worker process per open session,
//! keeps the session index, the approvals inbox and the model-request
//! scheduler, and never calls models or runs tools itself.

pub mod activation;
mod daemon;
mod error;
mod notify;
mod private;
pub mod runtime_dir;
mod scheduler;
mod serve;
mod spawn;
pub mod units;

pub use daemon::{Daemon, DaemonOptions};
pub use error::DaemonError;
pub use private::{private, serve_private};
pub use serve::run_user_daemon;
pub use spawn::{InProcessSpawner, ProcessSpawner, Spawner, WorkerProcess};
