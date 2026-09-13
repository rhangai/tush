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
//! This turns the file into these structs and stops. Whether a `depends`
//! names a proc that exists, whether they form a cycle, whether a proc that
//! declared both `run` and `modes` meant to — all of that belongs to
//! [`app`](crate::app), which has a whole session to answer it against.
//!
//! Which is why `run` and `modes` are two options rather than the one enum
//! they add up to: an enum would decide here, where the only way to object is
//! to refuse the file.
//!
//! A key nobody recognises *is* refused here, by `deny_unknown_fields`. This
//! layer can afford to be strict about that one thing, since a key that names
//! nothing has no reading under which the file was meant to work.
//!
//! # Why `figment` reads it
//!
//! It is a layering loader — file, then environment, then defaults, each
//! overriding the last — and only the file provider is used today. What it
//! buys right now is the error, which carries the key path it went wrong at
//! and the source it came from. The layering is for later.
//!
//! # Where the file's shape and the struct's disagree
//!
//! Twice, both covered by an attribute: the procs are written as a mapping
//! and used as a list ([`KeyValueMap`] moves the key in as
//! [`ConfigProc::key`]), and `run` is written as one command or a list of
//! them ([`OneOrMany`] takes both and always yields the list).

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
/// A list though the file writes a mapping, because nothing downstream wants
/// them by name — [`UnitMap`](crate::unit::UnitMap) is the lookup.
///
/// **The order is by key, not as written.** `figment`'s value tree is a
/// `BTreeMap`, so the mapping is already sorted by the time `serde` sees it.
/// Sorted is still *stable*, which is what dependency resolution needs as its
/// tie breaker; what is lost is influencing the order by moving lines around,
/// and a config that cares should say so with a `depends`.
#[serde_as]
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Every proc the file declared, sorted by key.
    #[serde(default)]
    #[serde_as(as = "KeyValueMap<_>")]
    pub procs: Vec<ConfigProc>,
}

impl Config {
    /// Read and parse a config file.
    ///
    /// The path is taken as given: [`Yaml::file`] would walk up looking for
    /// the name, and a caller who passed a path and got a file from three
    /// directories up has been answered a question it did not ask.
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
    /// By value, because it is handed to a
    /// [`UnitBehavior`](crate::unit::UnitBehavior) that keeps it — an
    /// [`ArcStr`] travels as a refcount, while a `&str` would have to be
    /// allocated into one at the far end.
    pub fn display_name(&self) -> ArcStr {
        self.name.clone().unwrap_or_else(|| self.key.clone())
    }
}

/// One named way to run a proc, switched between without redeclaring it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigUnitMode {
    /// What the mode is called, and what the menu lists it as.
    pub name: ArcStr,
    /// What it runs.
    pub run: ConfigUnitRun,
}

/// What a unit executes: the commands, in the order they were written.
///
/// A command is its argv — already split — rather than a line for a shell, so
/// [`Process`](crate::base::Process) spawns it with no quoting rules between.
///
/// A newtype and not a struct with a `commands` field because `run` in the
/// file *is* the list, in either of two spellings:
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
