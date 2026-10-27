//! Connections to the xi daemon.
//!
//! [`connect`] finds or starts the per-user daemon. [`client::Client`]
//! negotiates the protocol and provides typed daemon requests and
//! notifications over an [`xi_rpc::Peer`].

pub mod client;
pub mod connect;
