//! Connections to the xi daemon.
//!
//! [`peer::Peer`] is a full-duplex NDJSON JSON-RPC 2.0 connection used by
//! clients, the daemon and workers. [`connect`] finds or starts the per-user
//! daemon. [`client::Client`] negotiates the protocol and provides typed daemon
//! requests and notifications.

pub mod client;
pub mod connect;
pub mod peer;
