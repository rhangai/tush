use crate::base::ExitReason;

/// What a sequence of runners does when one of them does not succeed.
///
/// An enum with two variants today, and an enum rather than a `bool` because
/// it will not stay at two: retrying a failed step, tolerating a specific
/// exit code, treating a [`Killed`](ExitReason::Killed) differently from an
/// [`Error`](ExitReason::Error) — each of those is a variant here, and none
/// of them is a second boolean parameter at every call site.
///
/// A success never ends a sequence, whatever the policy says; the policy only
/// gets asked about the reasons that are not.
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
    /// Success never does, so the policy is only consulted about the rest.
    /// Note that a [`Killed`](ExitReason::Killed) is currently weighed the
    /// same as an [`Error`](ExitReason::Error): if the two ever need to part
    /// ways, this match is where it happens.
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
