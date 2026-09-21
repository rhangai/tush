use std::{path::PathBuf, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::{
    app::App,
    error::ServerError,
    server::{http, socket::ServerSocket, state::ServerState},
};

/// A session listening for clients, with no screen of its own.
///
/// The session is the process here: [`run`](Server::run) returning is the
/// socket closing and nothing else, so what stops the procs is the caller
/// awaiting [`App::shutdown`] afterwards — the same arrangement `tush run`
/// has, with a signal where the screen used to be.
pub struct Server {
    /// What every request reads the session through.
    state: Arc<ServerState>,
    socket: ServerSocket,
}

impl Server {
    /// Take the socket, before anything is accepted on it.
    ///
    /// Separate from [`run`](Server::run) so that a path already in use is a
    /// failure at startup, next to the other startup failures, rather than
    /// something a session discovers after it has spawned its procs.
    pub fn bind(app: Arc<App>, path: PathBuf) -> Result<Self, ServerError> {
        Ok(Self {
            state: Arc::new(ServerState::new(app)),
            socket: ServerSocket::bind(path)?,
        })
    }

    /// Where it is listening, for whoever has to tell the user.
    pub fn path(&self) -> &std::path::Path {
        self.socket.path()
    }

    /// Serve until `cancel`, then let the requests in flight finish.
    ///
    /// The accept loop is `axum`'s, which is most of why it is here: graceful
    /// shutdown, one connection per client with requests pipelined on it, and
    /// a request that panics taken as that request's failure rather than the
    /// server's.
    ///
    /// The socket outlives the serving and is dropped with this, which is
    /// what unlinks the path — see [`ServerSocket`].
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), ServerError> {
        let Some(listener) = self.socket.take_listener() else {
            return Ok(());
        };
        axum::serve(listener, http::router(self.state.clone()))
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await
            .map_err(ServerError::Accept)
    }
}
