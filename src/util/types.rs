//! Type aliases for the small collections that turn up all over the config.
//!
//! All of them are lists that a person typed into a YAML file, so they are
//! short, and they are read far more often than they are built. A [`SmallVec`]
//! keeps them where they were made instead of behind a pointer.
//!
//! The inline capacities are what they are for a reason, and it is not the
//! same reason twice — see each one.

use arcstr::ArcStr;
use smallvec::SmallVec;

/// A short list of names: an argv, a proc's groups, its `depends`, the keys a
/// command line resolved to.
///
/// Eight because that is past the long end of every one of those — an argv of
/// `[bash, -c, "…"]`, a proc in two groups — and 80 bytes is still cheap to
/// move.
pub type SmallVecArcStr = SmallVec<[ArcStr; 8]>;

/// The commands of one proc, each as its argv.
///
/// **One inline, not eight.** A `run:` is a single command in all but a
/// handful of procs, and the inner list is 80 bytes: eight of them inline is
/// 656 bytes carried by every behavior, mode and no-op in the session, since
/// this sits inside the enum they all are. One is 96.
pub type SmallMatrixArcStr = SmallVec<[SmallVecArcStr; 1]>;
