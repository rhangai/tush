use std::{
    num::NonZeroU8,
    sync::atomic::{AtomicU16, Ordering},
};

use crate::base::ExitReason;

#[derive(Clone, Copy, Debug)]
pub enum RunnerState {
    Stopped,
    Waiting,
    Started,
    Running,
    Killing,
    ExitSuccess,
    ExitError(Option<NonZeroU8>),
    Killed(Option<NonZeroU8>),
}

impl RunnerState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            RunnerState::ExitSuccess | RunnerState::ExitError(..) | RunnerState::Killed(..)
        )
    }

    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            RunnerState::Stopped
                | RunnerState::ExitSuccess
                | RunnerState::ExitError(..)
                | RunnerState::Killed(..)
        )
    }
}

impl From<ExitReason> for RunnerState {
    fn from(value: ExitReason) -> Self {
        match value {
            ExitReason::Success => RunnerState::ExitSuccess,
            ExitReason::Error(code) => RunnerState::ExitError(code),
            ExitReason::Killed(code) => RunnerState::Killed(code),
        }
    }
}

impl TryFrom<RunnerState> for ExitReason {
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

// Atomic RunnerState
//
// RunnerState intended to be set atomically between tasks, it is Sync so it can
// be send across threads in an Arc
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
    pub fn store(&self, value: RunnerState) {
        self.inner
            .store(Self::state_to_u16(value), Ordering::Release);
    }

    /// Load the state
    pub fn load(&self) -> RunnerState {
        Self::u16_to_state(self.inner.load(Ordering::Acquire))
    }

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

    const fn state_to_u16_code(value: Option<NonZeroU8>) -> u16 {
        match value {
            Some(v) => (v.get() as u16) << 8,
            None => 0,
        }
    }

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
