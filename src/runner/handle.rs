use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    runner::{Runner, state::RunnerState},
    util::event::EventDispatcher,
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
    start_notify: Arc<Notify>,
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
    pub fn new(runner: impl Runner, event_dispatcher: Option<EventDispatcher>) -> Self {
        Self::new_inner(runner, event_dispatcher)
    }
    /// Spawn the supervising task, which owns the runner for the rest of its
    /// life.
    ///
    /// The task holds an `Arc` back to the handle, so it keeps reporting state
    /// even once every external reference is dropped.
    fn new_inner(runner: impl Runner, event_dispatcher: Option<EventDispatcher>) -> Self {
        let (state_sender, state_receiver) =
            tokio::sync::watch::channel::<RunnerState>(RunnerState::Waiting);

        let start_notify = Arc::new(Notify::new());
        let abort_token = CancellationToken::new();

        // Block to spawn the worker task
        {
            let notify = start_notify.clone();
            let abort_token = abort_token.clone();
            let state = RunnerHandleState {
                state_sender,
                event_dispatcher,
            };
            _ = tokio::spawn(async move {
                notify.notified().await;
                state.modify(RunnerState::set_started);
                let mut runner = runner;
                // Eager check for cancelation
                if abort_token.is_cancelled() {
                    state.set(RunnerState::Killed(None));
                    return;
                }
                let runner_fut = {
                    let state = state.clone();
                    runner.run(move || state.modify(RunnerState::set_running))
                };
                let exit_state = tokio::select! {
                    wait_result = runner_fut => {
                        wait_result.map_or(RunnerState::ExitError(None), |i| i.into())
                    }
                    _ = abort_token.cancelled_owned() => {
                        state.modify(RunnerState::set_killing);
                        let shutdown_state = runner.shutdown().await;
                        shutdown_state.map_or(RunnerState::Killed(None), |i| i.into())
                    }
                };
                state.set(exit_state);
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
        self.start_notify.notify_one();
    }
}

/// Dropping the handle aborts the task
impl Drop for RunnerHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone)]
struct RunnerHandleState {
    /// The state
    state_sender: tokio::sync::watch::Sender<RunnerState>,
    /// Event
    event_dispatcher: Option<EventDispatcher>,
}

impl RunnerHandleState {
    fn set(&self, state: RunnerState) {
        self.state_sender.send_replace(state);
        self.notify();
    }

    fn modify(&self, f: impl FnOnce(&mut RunnerState)) {
        self.state_sender.send_modify(f);
        self.notify();
    }

    fn notify(&self) {
        if let Some(event_dispatcher) = self.event_dispatcher.as_ref() {
            event_dispatcher.trigger();
        }
    }
}
