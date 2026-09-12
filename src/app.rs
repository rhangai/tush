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
//! about to be fixed and three runs of the same discovery is a waste of
//! somebody's afternoon.
//!
//! # Where the checking belongs
//!
//! Not in [`config`](crate::config), which reads one proc at a time and can
//! only refuse the file. Every question here is about the procs *together* —
//! a name means something because another proc is declared under it, a cycle
//! is a property of the whole graph — so this is the first place any of them
//! can be asked, and the first place a useful answer exists.

mod app;
mod error;

#[allow(unused_imports)]
pub use app::App;

#[allow(unused_imports)]
pub use error::{AppError, AppErrors};
