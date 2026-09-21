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
//!
//! Nothing below knows this module exists: it reads [`App`](crate::app::App),
//! [`log`](crate::log), [`runner`](crate::runner) and [`unit`](crate::unit),
//! and none of them read it.

mod app;
mod client;

#[allow(unused_imports)]
pub use app::ViewApp;

#[allow(unused_imports)]
pub use client::{ViewClient, ViewCommand, ViewLog, ViewUnit};
