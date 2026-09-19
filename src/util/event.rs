//! "Something moved" — the one thing a session tells the screen without
//! being asked.
//!
//! A [`watch`] of `()` rather than a channel of messages, because the only
//! question a listener has is whether to look again: the events coalesce, a
//! listener that was busy misses nothing it would have acted on twice, and no
//! state has to be copied out of the session to travel.

use tokio::sync::watch;

/// The sending half, cloned into whatever can change something.
///
/// Cheap to hold and never fails, so the things that change state can carry
/// one without caring whether anybody is listening — see
/// [`trigger`](EventDispatcher::trigger).
#[derive(Clone)]
pub struct EventDispatcher {
    sender: watch::Sender<()>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        let sender = watch::Sender::new(());
        Self { sender }
    }

    /// A listener that wakes on changes from here on.
    ///
    /// It starts up to date: a trigger from before this call is not one it
    /// will be woken for.
    pub fn create_listener(&self) -> EventListener {
        EventListener {
            receiver: self.sender.subscribe(),
        }
    }

    /// Say that something moved.
    ///
    /// `send_replace` and not `send`, which reports having had no receivers
    /// as an error: nobody attached is the normal case here — `tush serve`
    /// with no screen on it — and not something a caller should have to
    /// handle.
    pub fn trigger(&self) {
        self.sender.send_replace(())
    }
}

/// The receiving half, one per thing that redraws.
pub struct EventListener {
    receiver: tokio::sync::watch::Receiver<()>,
}

impl EventListener {
    /// Wait for the next change; `false` once every dispatcher is gone.
    ///
    /// A `bool` and not a `Result` because there is one way to fail and it is
    /// not an error: the session it was listening to has ended, and the
    /// caller's answer to that is to stop, not to handle it.
    pub async fn changed(&mut self) -> bool {
        let result = self.receiver.changed().await;
        result.is_ok()
    }
}
