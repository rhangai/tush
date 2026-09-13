use crate::base::ExitReason;

/// What a sequence of runners does when one of them does not succeed.
///
/// An enum and not a `bool` because it will not stay at two: retrying a
/// failed step, tolerating a specific exit code, telling a
/// [`Killed`](ExitReason::Killed) from an [`Error`](ExitReason::Error) — each
/// is a variant here rather than a second boolean at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunnerPolicy {
    /// Stop at the first runner that does not succeed, and leave the rest
    /// unrun. The `&&` of a shell, and the default, because a step that runs
    /// against the output of a step that failed is usually worse than a step
    /// that does not run.
    #[default]
    Abort,
    /// Run every one regardless. The `;` of a shell.
    ///
    /// The sequence still *reports* the failure — see
    /// [`RunnerSerial::run`](crate::runner::RunnerSerial) — so this means
    /// "run them all anyway", not "pretend it went fine".
    Continue,
}

impl RunnerPolicy {
    /// Whether `reason` should end the sequence it came from.
    ///
    /// Success never does. [`Killed`](ExitReason::Killed) currently weighs
    /// the same as [`Error`](ExitReason::Error); this match is where they
    /// would part ways.
    pub fn is_abort(&self, reason: ExitReason) -> bool {
        if matches!(reason, ExitReason::Success) {
            return false;
        }
        match self {
            Self::Abort => true,
            Self::Continue => false,
        }
    }
}
