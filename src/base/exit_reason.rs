#[derive(Clone, Copy, Debug)]
pub enum ExitReason {
    Success,
    Error(Option<i32>),
    Killed(Option<i32>),
}
