//! The terminal UI, and the seam it sees the session through.
//!
//! # Why there is a seam at all
//!
//! The UI is meant to be able to run somewhere the session is not. The plan
//! is three entry points over the same screen:
//!
//! ```text
//! tush server --config x.yaml   the session, with a plain log
//! tush attach tui               the screen, over a socket; quitting leaves it running
//! tush ui --config x.yaml       both, in one process; quitting takes it down
//! ```
//!
//! Only the last one exists today. But the difference between them is
//! entirely *where the answers come from*, so if the UI is written against an
//! `App` now, the first two are a rewrite of the UI later. Written against
//! [`UiClient`] instead, they are a second implementation of three methods.
//!
//! # The parts
//!
//! - [`UiClient`] — what the screen may read, and what it may ask for.
//!   Neither half waits: reading is from a snapshot the client already has,
//!   and asking is a [`UiCommand`] sent without an answer.
//! - [`UiApp`] — the client for a session in this process.
//! - [`Ui`] — the screen: the redraw loop, the key bindings, the list.
//!
//! Today the list is the whole screen: one line per unit, its state in front
//! of its name, <kbd>Enter</kbd> to start it or move it to its next mode, and
//! <kbd>Backspace</kbd> to stop it. The logs the units are already capturing
//! are not shown yet.

mod app;
mod client;
mod render;
mod ui;

#[allow(unused_imports)]
pub use app::UiApp;

#[allow(unused_imports)]
pub use client::{UiClient, UiCommand, UiUnit};

#[allow(unused_imports)]
pub use ui::Ui;
