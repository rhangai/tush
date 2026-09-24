//! A session as something outside it sees and drives it.
//!
//! [`ViewClient`] is the seam: what may be read, and what may be asked for.
//! Separate from [`ui`](crate::ui) because the screen is only one of the
//! things that holds one — `tush server` serves this same vocabulary over a
//! socket, and it has no screen to link.
//!
//! - [`ViewClient`] — the seam itself.
//! - [`ViewUnit`], [`ViewCommand`], [`ViewLog`] — what goes across it.
//! - [`ViewApp`] — the client for a session in this process.
//! - `ServerClient` — the routes a serving session exposes, spelled once, for
//!   the two clients below to share.
//! - [`ViewDispatch`] — the client that says one thing and exits, for
//!   `tush dispatch`.
//! - [`ViewPrinter`] — the view that is a stream rather than a screen, for
//!   `tush serve` and for a `run` with no terminal to draw on.
//!
//! Nothing below knows this module exists: it reads [`App`](crate::app::App),
//! [`log`](crate::log), [`runner`](crate::runner) and [`unit`](crate::unit),
//! and none of them read it.

mod app;
mod client;
mod dispatch;
mod print;
mod server;
mod socket;

#[allow(unused_imports)]
pub use app::ViewApp;

#[allow(unused_imports)]
pub use print::ViewPrinter;

#[allow(unused_imports)]
pub use socket::ViewSocket;

#[allow(unused_imports)]
pub use dispatch::ViewDispatch;

#[allow(unused_imports)]
pub use client::{ViewClient, ViewCommand, ViewLog, ViewSettings, ViewUnit};
