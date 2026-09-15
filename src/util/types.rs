//! Type aliases for the small collections that turn up all over the config.
//!
//! All of them are lists that a person typed into a YAML file, so they are
//! short, and they are read far more often than they are built. A [`SmallVec`]
//! keeps them where they were made instead of behind a pointer.
//!
//! The inline capacities are what they are for a reason, and it is not the
//! same reason twice — see each one.

use smallvec::SmallVec;

use crate::util::vec::JaggedVec;

use super::str::SmallStr;

/// A short list of names: an argv, a proc's groups, its `depends`, the keys a
/// command line resolved to.
///
/// Eight because that is past the long end of every one of those — an argv of
/// `[bash, -c, "…"]`, a proc in two groups. At 208 bytes it is no longer free
/// to move, but a [`SmallStr`] that fits inline never allocates, so eight of
/// them inline is eight allocations that do not happen.
pub type SmallVecStr = SmallVec<[SmallStr; 8]>;

/// The commands of one proc, each as its argv.
///
/// **One run of items, not a vec per row.** A `SmallVec` of `SmallVec`s pays
/// its inline capacity in every row, so two commands of three words cost two
/// 208 byte inner vecs on the heap; a [`JaggedVec`] spends one budget of `N`
/// across all its rows and holds the same two without allocating at all.
///
/// **Eight words is where the allocations stop.** The `Arc` around this always
/// allocates; what `N` buys is whether the rows allocate *again* on top of it.
/// Measured over the example config, spills go 3, 1, 0 at `N` of 2, 4, 6 and
/// stay at 0 — so past six, more `N` is 24 bytes a word for nothing. Eight is
/// six with two words of headroom, and at 208 bytes it is what one argv of the
/// `SmallVecStr` above already cost.
///
/// **Six rows, because rows are free here.** `R` is a `[u8; R]` that fits in
/// the alignment slack of the items, so 2 and 6 are both 208 bytes and only 7
/// starts costing. A `run:` is one command in all but a handful of procs; six
/// is simply as many as the padding will hold.
pub type SmallMultiVecStr = JaggedVec<SmallStr, 8, 6>;
