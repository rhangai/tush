use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::net::UnixListener;

use crate::error::ServerError;

/// Only the user may connect.
///
/// Set after the bind and not with it — a socket is created under the process
/// umask and there is no way to bind one atomically with a mode — so this
/// narrows a window rather than closing it. What closes it is the directory
/// the socket sits in, and that belongs to whoever chose the path:
/// `XDG_RUNTIME_DIR` is the user's own and already unreadable to everyone
/// else.
const SOCKET_MODE: u32 = 0o600;

/// The socket, and the path it has to be unlinked from afterwards.
///
/// A bound Unix socket is a file that outlives the process that made it, so
/// this owns the path as much as the listener: dropping it is what stops the
/// next server from finding a socket nobody is listening on.
pub struct ServerSocket {
    /// Taken once, by whatever serves on it. `None` afterwards: the listener
    /// is owned by value from then on, and this half stays only to unlink.
    listener: Option<UnixListener>,
    path: PathBuf,
}

impl ServerSocket {
    /// Listen on `path`, taking it over from a server that is not there any
    /// more.
    ///
    /// A socket file whose server died looks exactly like a live one, so this
    /// asks instead: a connection that is refused means nobody is accepting
    /// and the file is debris, and one that succeeds means a real server and
    /// this one refuses to start. Without the question, a single crash makes
    /// the path unusable until somebody deletes it by hand.
    ///
    /// The directory has to be there already. Whoever names a path owns the
    /// directory it is in — the default one is the system's, and a path given
    /// on the command line is the caller's.
    pub fn bind(path: PathBuf) -> Result<Self, ServerError> {
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                    return Err(ServerError::AlreadyRunning(path));
                }
                fs::remove_file(&path).map_err(|error| ServerError::Bind(path.clone(), error))?;
                UnixListener::bind(&path).map_err(|error| ServerError::Bind(path.clone(), error))?
            }
            Err(error) => return Err(ServerError::Bind(path, error)),
        };

        // Best effort: a socket nobody can reach through the directory is
        // already closed off, so failing here is not worth refusing to start.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(SOCKET_MODE));

        Ok(Self {
            listener: Some(listener),
            path,
        })
    }

    /// Hand the listener over, once.
    ///
    /// `None` on a second call, which is a server asked to run twice — there
    /// is nothing to serve on and nothing to report, since the first call
    /// still holds it.
    pub fn take_listener(&mut self) -> Option<UnixListener> {
        self.listener.take()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ServerSocket {
    /// Unlink, so the next server binds rather than having to take it over.
    ///
    /// Blocking work in a `Drop`, which is only acceptable because it is one
    /// `unlink` on a path the kernel has cached. The takeover in
    /// [`bind`](ServerSocket::bind) is what covers the times this does not
    /// run at all — a `SIGKILL`, a panic in a worker.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Where a server listens when the command line did not say.
///
/// `$XDG_RUNTIME_DIR/tush.sock`: a directory the system already made, already
/// the user's alone, and wiped when the login session ends. Nothing here
/// creates it — see [`bind`](ServerSocket::bind).
///
/// One name and not one per config, so that the path is something a person
/// can say from memory rather than work out. What it costs is that a second
/// session on the same machine lands on it too, and finds a server already
/// listening — which is an error naming the path, and the answer to it is
/// `--socket` or `TUSH_SOCKET`.
///
/// Without that variable — a container, an ssh login with no user instance —
/// `/tmp/tush-<uid>.sock`: the uid is in the name because `/tmp` is shared,
/// and there is no directory of one's own to put it in without making one.
pub fn default_socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("tush.sock"),
        // SAFETY: `getuid` reads the calling process's own id. It cannot
        // fail, takes no pointer and is not racing anything.
        None => PathBuf::from("/var/run/tush.sock"),
    }
}
