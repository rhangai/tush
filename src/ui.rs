//! The terminal UI, and the seam it sees the session through.
//!
//! The screen is written against [`UiClient`] rather than against `App`,
//! because `tush attach` will run it over a socket with the session in
//! another process. That way it is a second implementation of the trait and
//! not a rewrite of the screen.
//!
//! - [`UiClient`] — what the screen may read and what it may ask for.
//! - [`UiApp`] — the client for a session in this process.
//! - [`Ui`] — the screen: the redraw loop, the keys, and the two panes.
//! - [`UiTheme`] — every fixed character and colour the panes draw with.

mod app;
mod client;
mod render;
mod theme;
mod ui;

#[allow(unused_imports)]
pub use app::UiApp;

#[allow(unused_imports)]
pub use client::{UiClient, UiCommand, UiLog, UiUnit};

#[allow(unused_imports)]
pub use theme::{
    UiTheme, UiThemeColors, UiThemeStatus, UiThemeStatuses, UiThemeSymbols, UiThemeUnits,
};

#[allow(unused_imports)]
pub use ui::Ui;
