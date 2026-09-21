//! A session with no screen, listening for something to attach to it.
//!
//! `tush run` is a session and a screen in one process, and quitting the
//! screen takes the children with it. Here the session is the process: it
//! outlives every connection, nothing a client does can end it, and the only
//! thing that stops it is a signal.
//!
//! - [`Server`] — the socket, the accept loop, and the connections on it.
//! - [`ServerPrinter`] — the session's output, onto stdout.
//!
//! The frames a connection will exchange are not here yet. What is here is
//! everything around them: where the socket lives, what happens to the one a
//! crashed server left behind, and how a connection is ended when the session
//! is.

mod print;
mod server;
mod socket;

#[allow(unused_imports)]
pub use print::ServerPrinter;

#[allow(unused_imports)]
pub use server::Server;

#[allow(unused_imports)]
pub use socket::default_socket_path;
