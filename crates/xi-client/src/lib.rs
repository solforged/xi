//! Connections to the xi daemon.
//!
//! [`peer::Peer`] is a full-duplex NDJSON JSON-RPC 2.0 connection used by
//! clients, the daemon and workers. [`connect`] finds or starts the per-user
//! daemon.

pub mod connect;
pub mod peer;
