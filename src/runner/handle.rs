use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::runner::{
    Runner,
    state::{RunnerState, RunnerStateAtomic},
};

/// One run, supervised.
///
/// Creating a handle spawns the supervising task at once, but the runner is
/// held at a gate until [`start`](RunnerHandle::start). That is what makes an
/// orderly restart possible: the replacement can exist, and be observed,
/// before the outgoing run has finished dying.
///
/// `new_inner` takes a flag for a handle that starts released, and nothing
/// passes `false` yet — there is no constructor for it.
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
    /// Start gate. `None` when the handle was created already running.
    start_notify: Option<Arc<Notify>>,
    /// Token to abort the handle
    abort_token: CancellationToken,
    /// The state
    state_receiver: tokio::sync::watch::Receiver<RunnerState>,
}

impl RunnerHandle {
    /// A handle parked at the start gate.
    ///
    /// It stays [`Waiting`](RunnerState::Waiting) until
    /// [`start`](RunnerHandle::start), or anything else that opens the gate —
    /// [`abort`](RunnerHandle::abort), [`wait`](RunnerHandle::wait).
    pub fn new(runner: impl Runner) -> Self {
        Self::new_inner(runner, true, |_| ())
    }

    /// The same, with `on_exit` run once the state is terminal.
    pub fn new_with_callback<F>(runner: impl Runner, on_exit: F) -> Self
    where
        F: FnOnce(RunnerState) + Send + 'static,
    {
        Self::new_inner(runner, true, on_exit)
    }

    /// Spawn the supervising task, which owns the runner for the rest of its
    /// life.
    ///
    /// The task holds an `Arc` back to the handle, so it keeps reporting state
    /// even once every external reference is dropped.
    fn new_inner<F>(runner: impl Runner, paused: bool, on_exit: F) -> Self
    where
        F: FnOnce(RunnerState) + Send + 'static,
    {
        let (state_sender, state_receiver) =
            tokio::sync::watch::channel::<RunnerState>(if paused {
                RunnerState::Waiting
            } else {
                RunnerState::Started
            });

        let start_notify = if paused {
            Some(Arc::new(Notify::new()))
        } else {
            None
        };
        let abort_token = CancellationToken::new();

        // Block to spawn the worker task
        {
            let notify = start_notify.clone();
            let abort_token = abort_token.clone();
            let state_sender = state_sender;
            _ = tokio::spawn(async move {
                let mut exit = RunnerExit::new(state_sender, on_exit);
                if let Some(notify) = notify {
                    notify.notified().await;
                    exit.sender.send_modify(RunnerState::set_started);
                }
                let mut runner = runner;
                // Eager check for cancelation
                if abort_token.is_cancelled() {
                    exit.finish(RunnerState::Killed(None));
                    return;
                }
                exit.sender.send_modify(RunnerState::set_running);
                let runner_fut = runner.run();
                let exit_state = tokio::select! {
                    wait_result = runner_fut => {
                        wait_result.map_or(RunnerState::ExitError(None), |i| i.into())
                    }
                    _ = abort_token.cancelled_owned() => {
                        exit.sender.send_modify(RunnerState::set_killing);
                        let shutdown_state = runner.shutdown().await;
                        shutdown_state.map_or(RunnerState::Killed(None), |i| i.into())
                    }
                };
                exit.finish(exit_state);
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

    /// Notify the start handle
    fn notify_start(&self) {
        if let Some(start_notify) = &self.start_notify {
            start_notify.notify_one();
        }
    }
}

/// Dropping the handle aborts the task
impl Drop for RunnerHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

/// The supervising task's only way out, terminal state included.
///
/// A panic in `run` or `shutdown` used to unwind past the send, dropping the
/// sender with the state still at `Running`: `wait` then returned `None`, but
/// every reader that polls — `Unit::state`, and the menu built from it — saw a
/// run that never ends, with no way back short of a restart. Publishing from
/// `Drop` means the state is terminal however the task leaves, unwinding and
/// runtime teardown included.
struct RunnerExit<F: FnOnce(RunnerState)> {
    /// Also the channel for the non-terminal transitions, which is why the
    /// task reaches in rather than being handed a copy — `Sender` is not
    /// `Clone`.
    sender: tokio::sync::watch::Sender<RunnerState>,
    /// Taken by whichever of `finish` and `drop` gets there first, so the
    /// callback runs exactly once even if `finish` itself panicked.
    on_exit: Option<F>,
}

impl<F: FnOnce(RunnerState)> RunnerExit<F> {
    fn new(sender: tokio::sync::watch::Sender<RunnerState>, on_exit: F) -> Self {
        Self {
            sender,
            on_exit: Some(on_exit),
        }
    }

    /// Publish `state` and run the callback, the first time only.
    fn finish(&mut self, state: RunnerState) {
        if let Some(on_exit) = self.on_exit.take() {
            self.sender.send_replace(state);
            on_exit(state);
        }
    }
}

impl<F: FnOnce(RunnerState)> Drop for RunnerExit<F> {
    /// `Killed(None)` because a task that left without saying how it ended was
    /// not the run finishing on its own.
    fn drop(&mut self) {
        self.finish(RunnerState::Killed(None));
    }
}
