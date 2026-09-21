//! A session with no screen, listening for something to attach to it.
//!
//! `tush run` is a session and a screen in one process, and quitting the
//! screen takes the children with it. Here the session is the process: it
//! outlives every connection, nothing a client does can end it, and the only
//! thing that stops it is a signal.
//!
//! What a person watching a server reads is
//! [`ViewPrinter`](crate::view::ViewPrinter), which is not here: `tush run
//! --no-tui` wants the same stream with no socket under it.
//!
//! The frames a connection will exchange are not here yet. What is here is
//! everything around them: where the socket lives, what happens to the one a
//! crashed server left behind, and how a connection is ended when the session
//! is.

mod server;
mod socket;

#[allow(unused_imports)]
pub use server::Server;

#[allow(unused_imports)]
pub use socket::default_socket_path;
