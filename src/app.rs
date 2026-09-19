//! The session: a config that has been checked, and what is built from it.
//!
//! [`Config`](crate::config::Config) is whatever the file said. [`App`] is
//! that config once it has been found sound — every `depends` naming a proc
//! that exists, no proc ambiguous about how it runs, nothing waiting on
//! itself. The conversion is [`App::new`], and it is the only way to get one,
//! so holding an `App` is holding the answer to those questions.
//!
//! When it is not sound, what comes back is [`AppErrors`]: every problem
//! found, not the first one, because a config with three mistakes in it is
//! about to be fixed and one mistake per run is three runs of the same
//! discovery.
//!
//! The checking is here and not in [`config`](crate::config), which reads one
//! proc at a time: every question is about the procs *together*, so this is
//! the first place any of them can even be asked.

mod app;
mod map;
mod target;

#[allow(unused_imports)]
pub use app::App;

#[allow(unused_imports)]
pub use target::{TARGET_SEPARATOR, Target};
