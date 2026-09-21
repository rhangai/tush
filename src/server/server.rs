use std::{path::PathBuf, sync::Arc};

use tokio::{net::UnixStream, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{app::App, error::ServerError, server::socket::ServerSocket};

/// A session listening for clients, with no screen of its own.
///
/// The session is the process here: [`run`](Server::run) returning is the
/// socket closing and nothing else, so what stops the procs is the caller
/// awaiting [`App::shutdown`] afterwards — the same arrangement `tush run`
/// has, with a signal where the screen used to be.
pub struct Server {
    /// Handed to every connection, which is what a connection reads the
    /// session through once it has frames to answer with.
    app: Arc<App>,
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
            app,
            socket: ServerSocket::bind(path)?,
        })
    }

    /// Where it is listening, for whoever has to tell the user.
    pub fn path(&self) -> &std::path::Path {
        self.socket.path()
    }

    /// Accept until `cancel`, then close every connection still open.
    ///
    /// The connections are a [`JoinSet`] and not loose spawns, so the set is
    /// the list of what is still attached: closing the socket does not reach
    /// a task already blocked on a client, and a task holding an
    /// [`Arc<App>`](App) that nothing can end is a session that never
    /// finishes shutting down.
    ///
    /// A connection that fails is dropped and the loop goes on; only the
    /// listener itself failing ends a server, because that is the one failure
    /// no future client can get past.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), ServerError> {
        let mut connections = JoinSet::new();
        let result = loop {
            tokio::select! {
                _ = cancel.cancelled() => break Ok(()),
                // Disabled while the set is empty, `join_next` answering
                // `None` at once — which is why this cannot spin.
                Some(_finished) = connections.join_next() => {}
                accepted = self.socket.listener().accept() => match accepted {
                    Ok((stream, _address)) => {
                        connections.spawn(connection(self.app.clone(), stream));
                    }
                    Err(error) => break Err(ServerError::Accept(error)),
                },
            }
        };
        connections.shutdown().await;
        result
    }
}

/// One client, for as long as it is there.
///
/// It reads and discards, which is the whole of the protocol so far: the
/// frame loop goes here, and until it does this is what proves a client can
/// reach the socket and be let go of when the session ends.
///
/// The [`App`] is held rather than borrowed because this outlives the call
/// that spawned it; what makes that safe is the [`JoinSet`] in
/// [`run`](Server::run), which ends every one of these before the session is
/// shut down.
async fn connection(_app: Arc<App>, mut stream: UnixStream) {
    let mut buffer = [0u8; 1024];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}
