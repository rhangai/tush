use std::num::NonZeroU8;

#[derive(Clone, Copy, Debug)]
pub enum ExitReason {
    Success,
    Error(Option<NonZeroU8>),
    Killed(Option<NonZeroU8>),
}
