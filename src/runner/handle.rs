use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::runner::{Runner, state::RunnerState};

/// One run, supervised.
///
/// Creating a handle spawns the supervising task at once, but the runner is
/// held at a gate until [`start`](RunnerHandle::start). That is what makes an
/// orderly restart possible: the replacement can exist, and be observed,
/// before the outgoing run has finished dying.
///
/// A handle covers exactly one run: once terminal it stays terminal, and
/// restarting means a new handle.
///
/// # Lifecycle
///
/// ```text
///          new()                start()            run()
///  ────> Waiting ──────────────> Started ─────────> Running ──┬──> ExitSuccess / ExitError
///           │                       │                         │
///           └────── abort() ────────┴───> Killing ────────────┴──> Killed
/// ```
///
/// Aborting before the start gate opens is honoured: the task wakes up, sees
/// the cancelled token and finishes as `Killed` without ever running.
pub struct RunnerHandle {
    /// The start gate the supervising task parks on, shared with it.
    start_notify: Arc<Notify>,
    /// Cancels the run wherever it has got to, the gate included.
    abort_token: CancellationToken,
    /// The read side of the state: a `watch`, so reading it is a borrow of
    /// the last value and never waits on the task that writes it.
    state_receiver: tokio::sync::watch::Receiver<RunnerState>,
}

impl RunnerHandle {
    /// A handle parked at the start gate.
    ///
    /// It stays [`Waiting`](RunnerState::Waiting) until
    /// [`start`](RunnerHandle::start), or anything else that opens the gate —
    /// [`abort`](RunnerHandle::abort), [`wait`](RunnerHandle::wait).
    pub fn new(runner: impl Runner) -> Self {
        Self::new_inner(runner)
    }
    /// Spawn the supervising task, which owns the runner for the rest of its
    /// life.
    ///
    /// The task holds an `Arc` back to the handle, so it keeps reporting state
    /// even once every external reference is dropped.
    fn new_inner(runner: impl Runner) -> Self {
        let (state_sender, state_receiver) =
            tokio::sync::watch::channel::<RunnerState>(RunnerState::Waiting);

        let start_notify = Arc::new(Notify::new());
        let abort_token = CancellationToken::new();

        // Block to spawn the worker task
        {
            let notify = start_notify.clone();
            let abort_token = abort_token.clone();
            _ = tokio::spawn(async move {
                notify.notified().await;
                state_sender.send_modify(RunnerState::set_started);
                let mut runner = runner;
                // Eager check for cancelation
                if abort_token.is_cancelled() {
                    state_sender.send_replace(RunnerState::Killed(None));
                    return;
                }
                let runner_fut = {
                    let state_sender = state_sender.clone();
                    runner.run(move || state_sender.send_modify(RunnerState::set_running))
                };
                let exit_state = tokio::select! {
                    wait_result = runner_fut => {
                        wait_result.map_or(RunnerState::ExitError(None), |i| i.into())
                    }
                    _ = abort_token.cancelled_owned() => {
                        state_sender.send_modify(RunnerState::set_killing);
                        let shutdown_state = runner.shutdown().await;
                        shutdown_state.map_or(RunnerState::Killed(None), |i| i.into())
                    }
                };
                state_sender.send_replace(exit_state);
            });
        };

        Self {
            start_notify,
            abort_token,
            state_receiver,
        }
    }

    /// State of the current runner
    ///
    /// A plain atomic load: cheap enough to poll from a render loop.
    pub fn state(&self) -> RunnerState {
        *self.state_receiver.borrow()
    }

    /// Open the start gate. Idempotent, and a no-op on a handle that was
    /// created already released.
    pub fn start(&self) {
        self.notify_start();
    }

    /// Abort the runner
    ///
    /// Cancels the token and opens the gate, so a runner still parked at the
    /// start also gets to observe the cancellation and finish. Returns without
    /// waiting; use [`wait`](RunnerHandle::wait) for that.
    pub fn abort(&self) {
        self.abort_token.cancel();
        self.notify_start();
    }

    /// Wait for the runner
    ///
    /// Note this also opens the start gate: waiting on a parked runner runs
    /// it, instead of deadlocking on something that would never begin.
    /// Returns `None` only if the supervising task went away without
    /// publishing a terminal state.
    pub async fn wait(&self) -> Option<RunnerState> {
        self.notify_start();
        let mut receiver = self.state_receiver.clone();
        let result = receiver.wait_for(|s| s.is_finished()).await;
        result.ok().map(|v| *v)
    }

    /// Wait for the state
    #[must_use]
    pub async fn wait_for(&self, f: impl Fn(&RunnerState) -> bool) -> bool {
        self.start_notify.notify_one();
        let mut receiver = self.state_receiver.clone();
        receiver.wait_for(f).await.is_ok()
    }

    /// Notify the start handle
    fn notify_start(&self) {
        self.start_notify.notify_one();
    }
}

/// Dropping the handle aborts the task
impl Drop for RunnerHandle {
    fn drop(&mut self) {
        self.abort();
    }
}
