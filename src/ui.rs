//! The terminal screen.
//!
//! It is written against [`ViewClient`](crate::view::ViewClient) rather than
//! against `App`, because `tush attach` will run it over a socket with the
//! session in another process. That way it is a second implementation of the
//! trait and not a rewrite of the screen.
//!
//! - [`Ui`] — the redraw loop, the keys, and the two panes.
//! - [`UiTheme`] — every fixed character and colour the panes draw with.

mod render;
mod theme;
mod ui;

#[allow(unused_imports)]
pub use theme::{UiTheme, UiThemeColors, UiThemeMenuLayout, UiThemeSymbols, UiThemeTexts};

#[allow(unused_imports)]
pub use ui::Ui;
