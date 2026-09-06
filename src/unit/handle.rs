use tokio::task::JoinSet;

use super::base::{UnitRunner, UnitState};

/// A handle for the item
pub struct UnitHandle {
    kill_sender: tokio::sync::oneshot::Sender<()>,
    state_receiver: tokio::sync::watch::Receiver<UnitState>,
}

impl UnitHandle {
    pub fn kill(self) {
        _ = self.kill_sender.send(());
    }

    pub fn from_runner<R>(runner: R) -> Self
    where
        R: UnitRunner,
    {
        let (handle, fut) = Self::from_runner_impl(runner);
        tokio::spawn(fut);
        handle
    }

    pub fn from_runner_set<R>(join_set: &mut JoinSet<()>, runner: R) -> Self
    where
        R: UnitRunner,
    {
        let (handle, fut) = Self::from_runner_impl(runner);
        join_set.spawn(fut);
        handle
    }

    fn from_runner_impl<R>(runner: R) -> (Self, impl Future<Output = ()>)
    where
        R: UnitRunner,
    {
        let (kill_sender, kill_receiver) = tokio::sync::oneshot::channel::<()>();
        let (state_sender, state_receiver) = tokio::sync::watch::channel(UnitState::Started);

        let future = async move {
            let mut runner = runner;
            _ = state_sender.send(UnitState::Running);
            tokio::select! {
                ok = runner.wait() => {
                    _ = state_sender.send(if ok { UnitState::Finished } else { UnitState::Failed });
                }
                result = kill_receiver => {
                    match result {
                        Ok(_) => {
                            _ = state_sender.send(UnitState::Killing);
                            runner.kill().await;
                            _ = state_sender.send(UnitState::Killed);
                        }
                        Err(_) => {
                            let ok = runner.wait().await;
                            _ = state_sender.send(if ok { UnitState::Finished } else { UnitState::Failed });
                        }
                    }
                }
            }
        };

        let item_handle = UnitHandle {
            kill_sender,
            state_receiver,
        };
        (item_handle, future)
    }
}
