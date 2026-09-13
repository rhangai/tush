//! The config file, parsed.
//!
//! A [`Config`] is the parsed form of a file like `tmp/example.yaml`: the
//! procs a session is made of, each one the recipe for a
//! [`Unit`](crate::unit::Unit) — what it waits for, what groups it answers
//! to, and what it runs.
//!
//! ```text
//! Config
//!   └── procs: [ConfigProc]            key, name, groups, depends
//!                   ├── run:   Option<ConfigUnitRun>       one way to run
//!                   └── modes: Option<[ConfigUnitMode]>    several, by name
//!                                         └── run: ConfigUnitRun
//! ```
//!
//! [`ConfigUnitRun`] is the leaf either way, which is the point of the split:
//! a mode is a named run, so whatever can execute a run can execute a mode.
//!
//! # Loading, not checking
//!
//! This turns the file into these structs and stops there. It does not ask
//! whether a `depends` names a proc that exists, whether the dependencies
//! form a cycle, whether a command has a program in it, or whether a proc
//! that declared both `run` and `modes` meant to. Those are questions for the
//! layer that builds the
//! [`DependencyGraph`](crate::util::graph::DependencyGraph) and starts
//! things, where there is a whole session to answer them against.
//!
//! Which is why `run` and `modes` are two options rather than the one enum
//! they add up to. An enum decides between them here, at parse time, where
//! the only way to object is to refuse the file; two options carry both
//! answers forward and leave the deciding to whoever is in a position to
//! report it properly.
//!
//! A key nobody recognises *is* refused here, by `deny_unknown_fields`, and
//! that is the one thing this layer is strict about. It can afford to be:
//! unlike the questions above, a key that names nothing has no reading under
//! which the file was meant to work.
//!
//! # Why `figment` reads it
//!
//! [`Figment`] is a layering loader: a config is assembled from providers —
//! a file, then environment variables, then defaults — each one overriding
//! the last, and the whole stack is deserialized once at the end. Only the
//! file provider is used today, so what it buys right now is the error: a
//! `figment::Error` carries the key path it went wrong at and the source it
//! came from, which is most of what makes a config error actionable. The
//! layering is what it is there for later.
//!
//! # Where the file's shape and the struct's shape disagree
//!
//! Twice, and both are covered by an attribute rather than by code:
//!
//! - the procs are *written* as a mapping keyed by name, and *used* as a list
//!   — [`KeyValueMap`] moves the key into the struct as [`ConfigProc::key`];
//! - `run` is written either as one command or as a list of them —
//!   [`OneOrMany`] takes both and always yields the list.

use std::path::Path;

use anyhow::{Context, Result};
use arcstr::ArcStr;
use figment::{
    Figment, Provider,
    providers::{Format, Yaml},
};
use serde::Deserialize;
use serde_with::{KeyValueMap, OneOrMany, serde_as};

/// A parsed config file: every proc a session is made of.
///
/// A list, though the file writes a mapping, because nothing downstream wants
/// them by name — the units go into a [`UnitMap`](crate::unit::UnitMap),
/// which is the lookup.
///
/// # The order is by key, not as written
///
/// `figment`'s value tree is a `BTreeMap`, so by the time a provider's data
/// reaches `serde` the mapping has been sorted and the order the procs were
/// written in is gone. What comes out is sorted by key.
///
/// That order still has to be *stable*, because it is the tie breaker the
/// dependency resolution falls back on between procs nothing else separates —
/// and sorted is stable. What is lost is only the ability to influence it by
/// moving lines around in the file, which is not a control worth keeping if
/// the trade is layering and better errors. A config that cares about the
/// order of two procs should say so with a `depends`.
#[serde_as]
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    #[serde_as(as = "KeyValueMap<_>")]
    pub procs: Vec<ConfigProc>,
}

impl Config {
    /// Read and parse a config file.
    ///
    /// The path is taken as given. [`Yaml::file`] would instead walk up from
    /// the working directory looking for the name, which is a good way to
    /// *find* a config and a bad way to load one that was named: a caller who
    /// passed a path and got a file from three directories up has been
    /// answered a question it did not ask.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Self::extract(Yaml::file_exact(path))
            .with_context(|| format!("loading config `{}`", path.display()))
    }

    /// Parse a config from YAML text.
    pub fn from_yaml(source: &str) -> Result<Self> {
        Self::extract(Yaml::string(source))
    }

    /// Deserialize one stack of providers.
    ///
    /// The seam where more providers go — an `Env` layer, a defaults layer —
    /// each merged on top of the last.
    fn extract(provider: impl Provider) -> Result<Self> {
        Ok(Figment::from(provider).extract()?)
    }
}

/// One declared process: the recipe a [`Unit`](crate::unit::Unit) is built
/// from.
///
/// [`run`](ConfigProc::run) and [`modes`](ConfigProc::modes) are meant to be
/// exclusive, and nothing here enforces it — a proc that declared both, or
/// neither, loads and says so by what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigProc {
    /// The name it is addressed by, and the name the unit goes into a
    /// [`UnitMap`](crate::unit::UnitMap) with.
    ///
    /// `$key$` is not a key of the file: it is how [`KeyValueMap`] hands over
    /// the name this proc was declared under in the `procs` mapping.
    #[serde(rename = "$key$")]
    pub key: ArcStr,
    /// The name it is shown under. `None` for the procs with nothing better
    /// to say about themselves than their key, which is most of them — see
    /// [`display_name`](ConfigProc::display_name).
    #[serde(default)]
    pub name: Option<ArcStr>,
    /// The groups it belongs to, for starting several procs by one name.
    ///
    /// `group` in the file, singular, because that is how it reads at the
    /// declaration of one proc.
    #[serde(default, rename = "group")]
    pub groups: Vec<ArcStr>,
    /// What has to be up before it may start, and the edges of the
    /// dependency graph.
    #[serde(default)]
    pub depends: Vec<ArcStr>,
    /// `run:` — the one way this proc runs.
    #[serde(default)]
    pub run: Option<ConfigUnitRun>,
    /// `modes:` — several named ways to run it, as in the `Build` and `Watch`
    /// of the example config.
    #[serde(default)]
    pub modes: Option<Vec<ConfigUnitMode>>,
}

impl ConfigProc {
    /// What to call it: its name, or its key when it did not give one.
    ///
    /// By value rather than borrowed, because what it is for is being handed
    /// to a [`UnitBehavior`](crate::unit::UnitBehavior) that keeps it — and
    /// an [`ArcStr`] handed over is a refcount, while a `&str` handed to the
    /// same place has to be allocated into one. Which would be a fresh copy
    /// of text sitting right here, made at the one point in the path where
    /// everything else travels for free.
    pub fn display_name(&self) -> ArcStr {
        self.name.clone().unwrap_or_else(|| self.key.clone())
    }
}

/// One named way to run a proc, switched between without redeclaring it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigUnitMode {
    pub name: ArcStr,
    pub run: ConfigUnitRun,
}

/// What a unit executes: the commands, in the order they were written.
///
/// A command is its argv — program and arguments already split — rather than
/// a line for a shell, so [`Process`](crate::base::Process) can spawn it
/// directly with no quoting rules in between.
///
/// A newtype rather than a struct with a `commands` field because `run` in
/// the file *is* the list, in either of two spellings:
///
/// ```yaml
/// run: [bash, -c, "echo hi"]      # one command
/// run:                            # several
///   - [npm, run, build]
///   - [npm, run, test]
/// ```
///
/// [`OneOrMany`] is what takes both: a list of strings is one command's argv,
/// a list of lists is a command each, and either way this holds the list.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConfigUnitRun(#[serde_as(as = "OneOrMany<_>")] pub Vec<Vec<ArcStr>>);

impl ConfigUnitRun {
    /// The commands, each an argv.
    pub fn commands(&self) -> &[Vec<ArcStr>] {
        &self.0
    }
}
