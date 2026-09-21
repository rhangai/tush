use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::net::UnixListener;

use crate::error::ServerError;

/// Only the user may connect.
///
/// Set on the directory rather than relying on the socket's own mode: a
/// socket is created under the process umask and there is no way to bind one
/// atomically with a mode, so the window between the two is closed by making
/// the directory untraversable instead.
const DIRECTORY_MODE: u32 = 0o700;

/// The same again on the socket, for a directory that was already there with
/// something laxer on it.
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
    pub fn bind(path: PathBuf) -> Result<Self, ServerError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .and_then(|()| {
                    fs::set_permissions(parent, fs::Permissions::from_mode(DIRECTORY_MODE))
                })
                .map_err(|error| ServerError::Directory(parent.to_path_buf(), error))?;
        }

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
/// `$XDG_RUNTIME_DIR/tush/<config stem>.sock`: per user, wiped by the system
/// when the login session ends, and named after the config so two sessions on
/// one machine do not land on the same path. Without that variable — a
/// container, an ssh login with no user instance — `/tmp/tush-<uid>/`, which
/// is the same arrangement built by hand rather than a path every user on the
/// box can reach.
pub fn default_socket_path(config: &str) -> PathBuf {
    let stem = Path::new(config)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("tush");
    let directory = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("tush"),
        // SAFETY: `getuid` reads the calling process's own id. It cannot
        // fail, takes no pointer and is not racing anything.
        None => PathBuf::from(format!("/tmp/tush-{}", unsafe { libc::getuid() })),
    };
    directory.join(format!("{stem}.sock"))
}
