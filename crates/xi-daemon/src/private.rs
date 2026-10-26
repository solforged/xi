//! Listener-free daemon instances for one invocation.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use xi_kernel::config::{Paths, daemon_settings};

use crate::{Daemon, DaemonOptions, Spawner};

/// Returns the client ends of a new in-memory, listener-free daemon.
/// The last client disconnect cancels and closes its workers.
pub fn private(
    paths: Paths,
    spawner: Arc<dyn Spawner>,
) -> (
    impl AsyncRead + Unpin + Send + 'static,
    impl AsyncWrite + Unpin + Send + 'static,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    tokio::spawn(serve_private(reader, writer, paths, spawner));
    tokio::io::split(client)
}

/// Serves a private instance over the supplied client transport, such as stdio.
/// Invalid global settings close the transport and are reported on stderr.
pub async fn serve_private<R, W>(reader: R, writer: W, paths: Paths, spawner: Arc<dyn Spawner>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let settings = match daemon_settings(&paths) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("xi private daemon: {error}");
            return;
        }
    };
    let daemon = Daemon::new(DaemonOptions {
        settings,
        paths,
        spawner,
        private: true,
    });
    daemon.serve(reader, writer).await;
    daemon.finished().await;
}
