use anyhow::Result;

use crate::{
    base::ExitReason,
    runner::{Runner, policy::RunnerPolicy},
};

/// Several runners, one after the other.
///
/// A [`Runner`] itself, so anything that takes one takes this: a
/// [`RunnerHandle`](crate::runner::RunnerHandle) supervising a serial cannot
/// tell it from a single process, and neither can a
/// [`Unit`](crate::unit::Unit). That is what the config's list of commands
/// becomes — `[[npm, run, build], [npm, run, test]]` is one unit, with one
/// log and one state, that happens to be two processes in a row.
///
/// # What a failure does
///
/// Up to the [`RunnerPolicy`]: [`Abort`](RunnerPolicy::Abort) makes the chain
/// an `&&`, [`Continue`](RunnerPolicy::Continue) a `;`. Either way the
/// sequence succeeds only if every runner did, and the reason reported is the
/// *first* failure — a later one is usually a consequence of it.
///
/// # Aborting
///
/// The index of the runner in flight is kept in a field and not on the stack,
/// which is the whole trick: a cancelled [`run`](RunnerSerial::run) is
/// *dropped* along with its locals, and
/// [`shutdown`](RunnerSerial::shutdown) is then called on the same value. The
/// field is all that survives to say which runner to shut down.
pub struct RunnerSerial<R: Runner> {
    /// The runners, in the order they were added.
    runners: Vec<R>,
    /// What to do when one of them does not succeed.
    policy: RunnerPolicy,
    /// Which one is running, by index.
    ///
    /// `None` before the first and after the last, and after any runner that
    /// ended the sequence — which is exactly the times there is nothing for
    /// [`shutdown`](RunnerSerial::shutdown) to reach.
    running: Option<usize>,
}

impl<R: Runner> RunnerSerial<R> {
    /// A sequence with nothing in it, which succeeds immediately.
    ///
    /// Stops at the first failure; see
    /// [`with_policy`](RunnerSerial::with_policy) for the alternative.
    pub fn new() -> Self {
        Self::with_policy(RunnerPolicy::default())
    }

    /// The same, deciding for itself what a failure means.
    pub fn with_policy(policy: RunnerPolicy) -> Self {
        Self {
            runners: Vec::new(),
            policy,
            running: None,
        }
    }

    /// Put a runner at the end of the sequence.
    pub fn add(&mut self, runner: R) {
        self.runners.push(runner);
    }
}

impl<R: Runner> Default for RunnerSerial<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Runner> Runner for RunnerSerial<R> {
    /// Run each one in turn, as far as the [`RunnerPolicy`] allows.
    ///
    /// A runner that returns `Err` — one that could not be started at all,
    /// rather than one that ran and failed — ends the sequence whatever the
    /// policy says. The policy weighs exit reasons, and that is not one.
    async fn run(&mut self, on_run: impl FnOnce() + Send + 'static) -> Result<ExitReason> {
        let mut failure = None;
        let mut first_on_run = Some(on_run);
        for index in 0..self.runners.len() {
            // Published before the await, because the await is the only place
            // this can be cancelled, and after it there is nothing left to
            // tell `shutdown` where we were.
            self.running = Some(index);
            let reason = if let Some(on_run) = first_on_run.take() {
                self.runners[index].run(on_run).await
            } else {
                self.runners[index].run(|| {}).await
            };
            // Cleared after it, for the same reason read the other way: this
            // runner is done, so it is not the one an abort should reach.
            // Nothing awaits between here and the next assignment, so there
            // is no cancellation point where the field could be wrong.
            self.running = None;

            let reason = reason?;
            if matches!(reason, ExitReason::Success) {
                continue;
            }
            // Kept whether or not the sequence stops here, so that a
            // `Continue` run still comes back as the failure it was.
            failure.get_or_insert(reason);
            if self.policy.is_abort(reason) {
                return Ok(reason);
            }
        }
        Ok(failure.unwrap_or(ExitReason::Success))
    }

    /// Shut down whichever runner was in flight, and abandon the rest.
    ///
    /// Nothing in flight — aborted before the first one started, or the
    /// sequence is empty — still reports [`Killed`](ExitReason::Killed)
    /// rather than [`Success`](ExitReason::Success). `shutdown` is only ever
    /// called because somebody asked for this to stop, and answering that
    /// with a success would have the handle publish
    /// [`ExitSuccess`](crate::runner::RunnerState::ExitSuccess) for a
    /// sequence that never ran.
    async fn shutdown(&mut self) -> Result<ExitReason> {
        let Some(index) = self.running.take() else {
            return Ok(ExitReason::Killed(None));
        };
        self.runners[index].shutdown().await
    }
}
