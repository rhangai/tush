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
//! Twice, and both are a hand written [`Deserialize`] rather than an attribute:
//! the procs are written as a mapping and used as a list, and `run` is written
//! as one command or a list of them.
//!
//! `serde_with`'s `KeyValueMap` and `OneOrMany` covered both until the commands
//! moved into a [`JaggedVec`](crate::util::vec::JaggedVec), which is not the
//! `Vec<Vec<_>>` those adapters build. Writing it out is what the move bought:
//! the words go from the parser straight into the one run of items, with no
//! intermediate list made and dropped.

use std::path::Path;

use anyhow::{Context, Result};
use figment::{
    Figment, Provider,
    providers::{Format, Yaml},
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeSeed, Visitor},
};

use crate::util::str::SmallStr;
use crate::util::types::{SmallMultiVecStr, SmallVecStr};

/// How much output a log keeps when the file does not say.
///
/// A mebibyte, which is what a log has always held here. At most eight
/// thousand short lines, fewer as they get longer — enough to scroll back
/// through what a proc just did, which is what this is for.
const DEFAULT_LOG_SIZE: usize = 1024 * 1024;

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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How much output to keep per proc, in bytes.
    ///
    /// Bytes rather than lines because bytes is what the ring promises: a log
    /// of long lines remembers fewer of them than a line count would suggest.
    /// This is a tail and not a transcript, so the default is small on
    /// purpose.
    ///
    /// One for every proc rather than one each, because the knob a person
    /// reaches for is how far back the pane scrolls, not how far back one
    /// proc scrolls.
    #[serde(default)]
    log_size: Option<ConfigSize>,
    /// Every proc the file declared, sorted by key.
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_procs")]
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

    /// Get the log size for the units
    pub fn log_size(&self) -> usize {
        self.log_size.map_or(DEFAULT_LOG_SIZE, |s| s.0)
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
    /// Skipped rather than read, because it is not written inside the proc: it
    /// is the key the proc was declared under, and [`deserialize_procs`] writes
    /// it in once the proc itself is built.
    #[serde(skip)]
    pub key: SmallStr,
    /// The name it is shown under. `None` for the procs with nothing better
    /// to say about themselves than their key, which is most of them, and
    /// which is what is shown instead.
    #[serde(default)]
    pub name: Option<SmallStr>,
    /// A shorter name, for where the long one will not fit.
    ///
    /// Handed on as the `Option` it is, and not folded into
    /// [`name`](ConfigProc::name): only a view knows whether it has the room,
    /// and a fallback taken here would reach it as a short name somebody
    /// chose.
    #[serde(default)]
    pub name_short: Option<SmallStr>,
    /// The groups it belongs to, for starting several procs by one name.
    ///
    /// `group` in the file, singular, because that is how it reads at the
    /// declaration of one proc.
    #[serde(default, rename = "group")]
    #[serde(deserialize_with = "deserialize_vecstr")]
    pub groups: SmallVecStr,
    /// What has to be up before it may start, and the edges of the
    /// dependency graph.
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_vecstr")]
    pub depends: SmallVecStr,
    /// Where its commands run, or `None` to run where `tush` was started.
    ///
    /// A relative path is relative to `tush`'s own working directory and not
    /// to the config file, which is what `Command::current_dir` does with one
    /// and the only reading that needs nothing carried from the loader.
    ///
    /// Not checked here or by [`App`](crate::app::App): a directory an
    /// earlier proc creates is a working config, and refusing it at startup
    /// would be this layer deciding what the session can be.
    #[serde(default)]
    pub working_dir: Option<SmallStr>,
    /// How much output this proc's log keeps, in bytes, or `None` to take the
    /// session's.
    ///
    /// Per proc as well as per session because how far back you want to
    /// scroll is a property of the proc and not of the config: a watcher
    /// printing a page a second and a setup step printing four lines want
    /// wildly different tails, and one figure for both either forgets the
    /// first or reserves for the second what it will never write.
    #[serde(default)]
    log_size: Option<ConfigSize>,
    /// Which of the two lists on screen it is drawn in.
    ///
    /// Presentation and nothing else: it moves a row, and never what the proc
    /// does or when it runs. An enum and not a `minor: true`, for the reason
    /// [`RunnerPolicy`](crate::runner::RunnerPolicy) is one — the set will not
    /// stay at two, and a proc that should not appear at all is a variant
    /// here rather than a second flag that can contradict this one.
    #[serde(default)]
    pub panel: ConfigPanel,
    /// `run:` — the one way this proc runs.
    #[serde(default)]
    pub run: Option<ConfigUnitRun>,
    /// `modes:` — several named ways to run it, as in the `Build` and `Watch`
    /// of the example config.
    #[serde(default)]
    pub modes: Option<Vec<ConfigUnitMode>>,
}

/// Which list a proc is drawn in.
///
/// Named for the two places on screen and not for the procs that go in them,
/// because that is all it decides: a setup step and a watcher you only look
/// at when it breaks want the same row, and they have nothing else in common.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigPanel {
    /// The list proper, which is where a proc goes unless it says otherwise.
    ///
    /// First in the `Ord`, which is what sorts the rows: a view orders by
    /// panel and then by name, so the two lists fall out of one sort.
    #[default]
    Main,
    /// The compact list under it, for what you do not sit and watch.
    Minor,
}

impl ConfigProc {
    /// How much output this proc keeps, given what the session keeps.
    ///
    /// The fallback is taken here rather than left to the caller so that
    /// there is one place the two figures meet.
    pub fn log_size(&self, session: usize) -> usize {
        self.log_size.map_or(session, |size| size.0)
    }
}

/// One named way to run a proc, switched between without redeclaring it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigUnitMode {
    /// What the mode is called, and what the menu lists it as.
    pub name: SmallStr,
    /// A shorter name for it, on the same terms as a proc's.
    #[serde(default)]
    pub name_short: Option<SmallStr>,
    /// Where this mode runs, overriding the proc's.
    ///
    /// `None` is "wherever the proc says", not "where `tush` was started" —
    /// a mode that wanted the latter has to say `.` — which is what makes the
    /// two levels read as one setting with an exception rather than as two.
    #[serde(default)]
    pub working_dir: Option<SmallStr>,
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
/// The two are told apart by the first element alone: a string means the whole
/// sequence is one command's argv, a list means one command each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigUnitRun {
    /// Each command as its argv, in the order they were written.
    pub commands: SmallMultiVecStr,
}

/// Reads the `procs` mapping as a list, moving each key into the proc it opened.
///
/// A mapping is how a person writes it — the key names the proc and cannot
/// repeat — and a list is how the rest of the crate wants it, ordered and
/// indexable.
fn deserialize_procs<'rde, D>(deserializer: D) -> Result<Vec<ConfigProc>, D::Error>
where
    D: Deserializer<'rde>,
{
    /// Reads the mapping, one proc per key.
    struct ConfigProcVisitor;
    impl<'de> Visitor<'de> for ConfigProcVisitor {
        type Value = Vec<ConfigProc>;

        /// What the error says the file should have held.
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("proc map using proc-key: {}")
        }

        /// Each entry in turn, with the key written into the proc it opened.
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut vec: Vec<ConfigProc> = if let Some(hint) = map.size_hint() {
                Vec::with_capacity(hint)
            } else {
                Vec::new()
            };
            while let Some(key) = map.next_key::<SmallStr>()? {
                let mut value = map.next_value::<ConfigProc>()?;
                value.key = key;
                vec.push(value);
            }
            Ok(vec)
        }
    }
    deserializer.deserialize_map(ConfigProcVisitor)
}

/// Reads the `procs` mapping as a list, moving each key into the proc it opened.
///
/// A mapping is how a person writes it — the key names the proc and cannot
/// repeat — and a list is how the rest of the crate wants it, ordered and
/// indexable.
fn deserialize_vecstr<'rde, D>(deserializer: D) -> Result<SmallVecStr, D::Error>
where
    D: Deserializer<'rde>,
{
    /// Reads the mapping, one proc per key.
    struct SmallStrVisitor;
    impl<'de> Visitor<'de> for SmallStrVisitor {
        type Value = SmallVecStr;

        /// What the error says the file should have held.
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("string or list of strings")
        }

        fn visit_str<E>(self, str: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let mut v = SmallVecStr::with_capacity(1);
            v.push(SmallStr::new(str));
            Ok(v)
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut v = if let Some(hint) = seq.size_hint() {
                SmallVecStr::with_capacity(hint)
            } else {
                SmallVecStr::new()
            };
            while let Some(str) = seq.next_element::<SmallStr>()? {
                v.push(str)
            }
            Ok(v)
        }
    }
    deserializer.deserialize_any(SmallStrVisitor)
}

/// Streams the words straight into the [`JaggedVec`](crate::util::vec::JaggedVec).
///
/// That is what the seeds are for: every visitor below is handed the vec
/// itself, so a word is pushed where it will live instead of into a list that
/// exists only to be copied out of.
///
/// Which of the two spellings it is cannot be known before the first element,
/// so that one is read with `deserialize_any` and every element after it with
/// the typed seed its answer picks.
impl<'rde> Deserialize<'rde> for ConfigUnitRun {
    /// Always a sequence: the two spellings differ inside it, not at it.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'rde>,
    {
        /// What the first element turned out to be, and so what the rest are.
        #[derive(Debug)]
        enum Mode {
            /// Strings: the whole sequence is one command's argv.
            Flat,
            /// Lists: one command each.
            Multi,
        }
        /// The first element, whose type settles the [`Mode`].
        ///
        /// It is consumed into the vec as it is read rather than looked at and
        /// put back, because a `deserialize_any` answer cannot be rewound.
        struct ElemUnknown<'a> {
            vec: &'a mut SmallMultiVecStr,
        }
        impl<'de, 'a> DeserializeSeed<'de> for ElemUnknown<'a> {
            type Value = Mode;
            /// `deserialize_any`, since the shape is exactly what is being asked.
            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Mode, D::Error> {
                d.deserialize_any(self)
            }
        }
        impl<'de, 'a> Visitor<'de> for ElemUnknown<'a> {
            type Value = Mode;

            /// What the error says the first element should have been.
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an string or a list of commands")
            }

            /// A word, and no commit: a flat `run` is one row for the whole sequence.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Mode, E> {
                self.vec.push_data(SmallStr::new(v));
                Ok(Mode::Flat)
            }

            /// A command, closed here because in this spelling each element is one.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Mode, A::Error> {
                while seq.next_element_seed(ElemStr { vec: self.vec })?.is_some() {}
                if self.vec.is_row_open() {
                    self.vec.commit_row();
                }
                Ok(Mode::Multi)
            }
        }

        /// One word of an argv, once the spelling is known.
        struct ElemStr<'a> {
            vec: &'a mut SmallMultiVecStr,
        }
        impl<'de, 'a> DeserializeSeed<'de> for ElemStr<'a> {
            type Value = ();
            /// `deserialize_str`, which is cheaper than asking what it is again.
            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
                d.deserialize_str(self)
            }
        }
        impl<'de, 'a> Visitor<'de> for ElemStr<'a> {
            type Value = ();

            /// What the error says an argv element should have been.
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("the command arguments")
            }

            /// The word, pushed into whichever row is open.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<(), E> {
                self.vec.push_data(SmallStr::new(v));
                Ok(())
            }
        }

        /// One command: its words, and then the row they make.
        struct ElemList<'a> {
            vec: &'a mut SmallMultiVecStr,
        }
        impl<'de, 'a> DeserializeSeed<'de> for ElemList<'a> {
            type Value = ();
            /// `deserialize_seq`, which is cheaper than asking what it is again.
            fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
                d.deserialize_seq(self)
            }
        }
        impl<'de, 'a> Visitor<'de> for ElemList<'a> {
            type Value = ();

            /// What the error says a command should have been.
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of commands")
            }

            /// The words, then the row — skipping the commit for an empty command,
            /// which has no program to run and would only fail later.
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                while seq.next_element_seed(ElemStr { vec: self.vec })?.is_some() {}
                if self.vec.is_row_open() {
                    self.vec.commit_row();
                }
                Ok(())
            }
        }

        /// The `run:` sequence itself.
        struct RunVisitor;
        impl<'de> Visitor<'de> for RunVisitor {
            type Value = ConfigUnitRun;

            /// What the error says `run:` should have been.
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a list of command arguments or a list of list of commands")
            }

            /// Probes the first element, then reads the rest the way it says to.
            ///
            /// An empty `run:` never reaches the probe and yields no commands, which
            /// is what a proc that declares one and lists nothing asked for.
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut vec = SmallMultiVecStr::new();
                let Some(mode) = seq.next_element_seed(ElemUnknown { vec: &mut vec })? else {
                    return Ok(ConfigUnitRun { commands: vec });
                };
                match mode {
                    Mode::Flat => {
                        while seq.next_element_seed(ElemStr { vec: &mut vec })?.is_some() {}
                        if vec.is_row_open() {
                            vec.commit_row();
                        }
                    }
                    Mode::Multi => {
                        while seq.next_element_seed(ElemList { vec: &mut vec })?.is_some() {}
                    }
                };
                Ok(ConfigUnitRun { commands: vec })
            }
        }

        deserializer.deserialize_seq(RunVisitor)
    }
}

/// A size
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
struct ConfigSize(pub usize);

impl<'de> Deserialize<'de> for ConfigSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Size {
            Str(SmallStr),
            U64(u64),
        }
        let res = match Size::deserialize(deserializer)? {
            Size::U64(num) => Ok(num),
            Size::Str(s) => parse_size::Config::new()
                .with_binary()
                .parse_size(s.as_bytes())
                .map_err(serde::de::Error::custom),
        }?;
        Ok(Self(res as usize))
    }
}
