use std::{
    num::NonZeroU8,
    sync::atomic::{AtomicU16, Ordering},
};

use crate::base::ExitReason;

/// Where a run currently is in its lifecycle.
///
/// The variants are ordered: each one is "later" than the one above it, and
/// [`RunnerStateAtomic::store_next`] relies on that to make progress
/// monotonic. Everything from [`ExitSuccess`](RunnerState::ExitSuccess) down
/// is terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunnerState {
    /// Nothing was ever started (the state a unit reports with no handle).
    Stopped,
    /// The handle exists but is parked at the start gate.
    Waiting,
    /// Released to run; the task may not have been scheduled yet.
    Started,
    /// The runner's `run` future is in flight.
    Running,
    /// Aborted, waiting for the shutdown to complete.
    Killing,
    /// Finished on its own, successfully.
    ExitSuccess,
    /// Finished on its own, with a failure code.
    ExitError(Option<NonZeroU8>),
    /// Terminated by us.
    Killed(Option<NonZeroU8>),
}

impl RunnerState {
    /// Whether the run reached a terminal state.
    pub fn is_started(&self) -> bool {
        matches!(
            self,
            RunnerState::Started
                | RunnerState::Running
                | RunnerState::Killing
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }

    /// Whether the run reached a terminal state.
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            RunnerState::ExitSuccess | RunnerState::ExitError(..) | RunnerState::Killed(..)
        )
    }

    /// Whether nothing is running — terminal, or never started at all.
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            RunnerState::Stopped
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }

    /// Set started
    pub fn set_started(&mut self) {
        *self = match self {
            RunnerState::Stopped | RunnerState::Waiting => RunnerState::Started,
            _ => *self,
        };
    }

    /// Set running
    pub fn set_running(&mut self) {
        *self = match self {
            RunnerState::Stopped | RunnerState::Waiting | RunnerState::Started => {
                RunnerState::Running
            }
            _ => *self,
        };
    }

    /// Set killing
    pub fn set_killing(&mut self) {
        *self = match self {
            RunnerState::Stopped
            | RunnerState::Waiting
            | RunnerState::Started
            | RunnerState::Running => RunnerState::Killing,
            _ => *self,
        };
    }
}

impl From<ExitReason> for RunnerState {
    /// Lift a finished process into the matching terminal state.
    fn from(value: ExitReason) -> Self {
        match value {
            ExitReason::Success => RunnerState::ExitSuccess,
            ExitReason::Error(code) => RunnerState::ExitError(code),
            ExitReason::Killed(code) => RunnerState::Killed(code),
        }
    }
}

impl TryFrom<RunnerState> for ExitReason {
    /// The state itself, when it was not a terminal one.
    type Error = RunnerState;
    fn try_from(value: RunnerState) -> Result<Self, RunnerState> {
        match value {
            RunnerState::ExitSuccess => Ok(ExitReason::Success),
            RunnerState::ExitError(code) => Ok(ExitReason::Error(code)),
            RunnerState::Killed(code) => Ok(ExitReason::Killed(code)),
            reason => Err(reason),
        }
    }
}

/// A [`RunnerState`] readable and writable from any task without a lock.
///
/// The whole state fits in a `u16`: the low byte is the variant tag and the
/// high byte the exit code, with `0` meaning "no code" — which is why the
/// code is a [`NonZeroU8`]. That keeps a read to one atomic load, so the
/// render loop can poll a run's state without a lock or a channel.
pub struct RunnerStateAtomic {
    inner: AtomicU16,
}

impl RunnerStateAtomic {
    /// Create the Atomic RunnerState
    pub const fn new(value: RunnerState) -> Self {
        Self {
            inner: AtomicU16::new(Self::state_to_u16(value)),
        }
    }

    /// Store the state, only if the new_state is after the current state
    ///
    /// Intended to be used with
    /// Stopped => Started => Running => Killing
    ///
    /// Only the variant tag is compared, so the exit code carried in the high
    /// byte never interferes with the ordering. The CAS loop makes this safe
    /// against a racing writer: a late `Running` cannot undo a `Killing` that
    /// another task already published.
    pub fn store_next(&self, value: RunnerState) {
        let value = Self::state_to_u16(value);
        let value_low = value & 0xff;

        let mut current = self.inner.load(Ordering::Acquire);
        loop {
            let current_low = current & 0xff;
            if current_low >= value_low {
                break;
            }
            match self.inner.compare_exchange_weak(
                current,
                value,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Just store the state
    ///
    /// Unconditional, for the transitions that must land no matter what — the
    /// terminal state published by the supervising task.
    pub fn store(&self, value: RunnerState) {
        self.inner
            .store(Self::state_to_u16(value), Ordering::Release);
    }

    /// Load the state
    pub fn load(&self) -> RunnerState {
        Self::u16_to_state(self.inner.load(Ordering::Acquire))
    }

    /// Pack a state: variant tag in the low byte, exit code in the high byte.
    ///
    /// The tags are ordered on purpose; see
    /// [`store_next`](RunnerStateAtomic::store_next).
    const fn state_to_u16(value: RunnerState) -> u16 {
        match value {
            RunnerState::Stopped => 0,
            RunnerState::Waiting => 1,
            RunnerState::Started => 2,
            RunnerState::Running => 3,
            RunnerState::Killing => 4,
            RunnerState::ExitSuccess => 5,
            RunnerState::ExitError(code) => 6 | Self::state_to_u16_code(code),
            RunnerState::Killed(code) => 7 | Self::state_to_u16_code(code),
        }
    }

    /// Shift an exit code into the high byte, `None` becoming zero.
    const fn state_to_u16_code(value: Option<NonZeroU8>) -> u16 {
        match value {
            Some(v) => (v.get() as u16) << 8,
            None => 0,
        }
    }

    /// Unpack a state. An unknown tag is reported as `Killed(None)`.
    const fn u16_to_state(value: u16) -> RunnerState {
        let low = value & 0xff;
        let high = ((value & 0xff00) >> 8) as u8;
        match low {
            0 => RunnerState::Stopped,
            1 => RunnerState::Waiting,
            2 => RunnerState::Started,
            3 => RunnerState::Running,
            4 => RunnerState::Killing,
            5 => RunnerState::ExitSuccess,
            6 => RunnerState::ExitError(NonZeroU8::new(high)),
            7 => RunnerState::Killed(NonZeroU8::new(high)),
            _ => RunnerState::Killed(None),
        }
    }
}
