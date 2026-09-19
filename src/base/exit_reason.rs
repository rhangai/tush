use std::num::NonZeroU8;

/// How a process finished.
///
/// The exit code is kept as an [`Option<NonZeroU8>`] because a zero code is
/// already represented by [`ExitReason::Success`], which leaves `None` free
/// to mean the process died without reporting one at all — killed by a
/// signal, or the wait itself failed.
#[derive(Clone, Copy, Debug)]
pub enum ExitReason {
    /// The process exited on its own with a zero status.
    Success,
    /// The process exited on its own with a non zero status.
    Error(Option<NonZeroU8>),
    /// The process was terminated by us (`shutdown` or `kill`).
    Killed(Option<NonZeroU8>),
}
