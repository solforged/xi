//! Errors from starting or serving the user daemon.

/// A daemon startup, configuration or listener error.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Operating-system or transport failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Global daemon settings could not be loaded.
    #[error(transparent)]
    Config(#[from] xi_kernel::Error),
}
