use std::sync::Arc;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::runner::{
    Runner,
    state::{RunnerState, RunnerStateAtomic},
};

/// A handle for the runner
///
/// Creating a handle immediately spawns the supervising task, but the runner
/// itself may be held at the gate: [`RunnerHandle::new`] parks it until
/// [`start`](RunnerHandle::start) is called, while
/// [`new_running`](RunnerHandle::new_running) lets it go right away. The
/// paused form is what makes an orderly restart possible — the replacement can
/// exist, and be observable, before the outgoing one has finished dying.
///
/// A handle covers exactly one run: once it reaches a terminal state it stays
/// there, and restarting means building a new handle.
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
    start_notify: Option<Notify>,
    abort_token: CancellationToken,
    /// Current state, readable without awaiting anything.
    state: RunnerStateAtomic,
    /// Resolves once, with the terminal state, for every waiter.
    exit_state_receiver: tokio::sync::watch::Receiver<Option<RunnerState>>,
}

impl RunnerHandle {
    /// Create the handle from the runner
    ///
    /// The runner stays [`Waiting`](RunnerState::Waiting) until
    /// [`start`](RunnerHandle::start) — or anything else that opens the gate,
    /// such as [`abort`](RunnerHandle::abort) or
    /// [`wait`](RunnerHandle::wait) — is called.
    pub fn new(runner: impl Runner) -> Arc<Self> {
        Self::new_inner(runner, true)
    }

    /// Create the handle from the runner, already running
    pub fn new_running(runner: impl Runner) -> Arc<Self> {
        Self::new_inner(runner, false)
    }

    /// Create the handle from the runner
    ///
    /// Spawns the supervising task, which owns the runner for the rest of its
    /// life. The task holds an `Arc` back to the handle, so it keeps reporting
    /// state even if every external reference is dropped.
    fn new_inner(runner: impl Runner, paused: bool) -> Arc<Self> {
        let (exit_state_sender, exit_state_receiver) =
            tokio::sync::watch::channel::<Option<RunnerState>>(None);
        let handle = Arc::new(RunnerHandle {
            start_notify: if paused { Some(Notify::new()) } else { None },
            abort_token: CancellationToken::new(),
            state: RunnerStateAtomic::new(if paused {
                RunnerState::Waiting
            } else {
                RunnerState::Started
            }),
            exit_state_receiver,
        });

        // Block to spawn the worker task
        {
            let handle = handle.clone();
            tokio::spawn(async move {
                if let Some(start_notify) = &handle.start_notify {
                    start_notify.notified().await;
                }
                let mut runner = runner;
                // Eager check for cancelation
                if handle.abort_token.is_cancelled() {
                    let exit_state = RunnerState::Killed(None);
                    handle.state.store(exit_state);
                    _ = exit_state_sender.send(Some(exit_state));
                    return;
                }
                handle.state.store_next(RunnerState::Running);
                let runner_fut = runner.run();
                let exit_state = tokio::select! {
                    wait_result = runner_fut => {
                        wait_result.map_or(RunnerState::ExitError(None), |i| i.into())
                    }
                    _ = handle.abort_token.cancelled() => {
                        handle.state.store(RunnerState::Killing);
                        let shutdown_state = runner.shutdown().await;
                        shutdown_state.map_or(RunnerState::Killed(None), |i| i.into())
                    }
                };
                handle.state.store(exit_state);
                _ = exit_state_sender.send(Some(exit_state));
            })
        };
        handle
    }

    /// State of the current runner
    ///
    /// A plain atomic load: cheap enough to poll from a render loop.
    pub fn state(&self) -> RunnerState {
        self.state.load()
    }

    /// Start the handle
    ///
    /// Opens the start gate. Idempotent, and a no-op on a handle created with
    /// [`new_running`](RunnerHandle::new_running).
    pub fn start(&self) {
        self.state.store_next(RunnerState::Started);
        self.notify_start();
    }

    /// Abort the runner
    ///
    /// Cancels the token and opens the gate, so a runner still parked at the
    /// start also gets to observe the cancellation and finish. Returns without
    /// waiting; use [`wait`](RunnerHandle::wait) for that.
    pub fn abort(&self) {
        self.state.store_next(RunnerState::Killing);
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
        let mut receiver = self.exit_state_receiver.clone();
        receiver
            .wait_for(|s| s.is_some())
            .await
            .map_or(None, |v| *v)
    }

    /// Notify the start handle
    fn notify_start(&self) {
        if let Some(start_notify) = &self.start_notify {
            start_notify.notify_one();
        }
    }
}
