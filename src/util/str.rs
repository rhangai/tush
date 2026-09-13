//! The one string type the config, the units and the screen share.

// The one module that may name what backs it — see `clippy.toml`.
#![allow(clippy::disallowed_types)]

use std::{borrow::Borrow, fmt, ops::Deref};

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Text that is read far more often than it is made: proc keys, names, modes,
/// the fixed marks a theme is drawn with.
///
/// Named for why it is here rather than for what it wraps: these are short
/// strings — `server-build`, `Watch`, ` · ` — and short is what makes the
/// representation worth having over a [`String`], since one that fits inline
/// is never allocated and costs a copy to clone.
///
/// A newtype over [`SmolStr`] rather than a re-export of it, so that what
/// backs it is one line to change. That is the whole of what it is for — the
/// crate has already been through one such swap, and the last one reached
/// every module that holds a name.
///
/// **It derefs to [`str`] and is never unwrapped.** A wrapper that had to be
/// opened at each use would put `.as_str()` on every comparison, every width
/// and every draw, which is a permanent cost paid for an occasional swap. The
/// deref is what makes it transparent: no caller names what is inside, and no
/// caller has to.
///
/// Everything that can be derived is: [`SmolStr`]'s own [`Hash`](std::hash::Hash),
/// [`Ord`] and [`PartialEq`] all go through its `str`, so a derive on the
/// newtype behaves exactly as a hand written forward would — which is what
/// makes the [`Borrow`] below sound. The four that are written out are the
/// four a derive would get wrong or not reach at all.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SmallStr(SmolStr);

impl SmallStr {
    /// Text known at compile time.
    ///
    /// The only `const` constructor, and the one the themes are written with.
    /// [`SmolStr::new_static`] keeps the pointer it was handed, so this
    /// neither allocates nor copies and has no length to stay under.
    pub const fn literal(text: &'static str) -> Self {
        Self(SmolStr::new_static(text))
    }

    /// Text found at runtime: a config value, a name read out of a file.
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(SmolStr::new(text))
    }

    /// The text itself, for the places a deref will not reach.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl Deref for SmallStr {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// So that a `HashMap<SmallStr, _>` can be looked up by `&str`, which is how every
/// key reaches the maps in this crate.
impl Borrow<str> for SmallStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for SmallStr {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

/// As the text and not as a tuple struct around it, which is the derive's
/// `Str("api")`.
///
/// Quoted, though, where [`Display`](fmt::Display) is not: `{:?}` is what is
/// reached for when a value looks wrong, and the theme holds marks like
/// `" · "` whose spaces are the whole of what there is to see.
impl fmt::Debug for SmallStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

/// Not reachable through the deref: `{}` does not follow one, and a proc's
/// name is formatted into every message [`AppError`](crate::app::AppError)
/// has.
impl fmt::Display for SmallStr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), formatter)
    }
}
