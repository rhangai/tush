//! Telling whoever is watching that something in a session changed.
//!
//! The side that changes things holds an [`EventDispatcher`] and calls
//! [`trigger`](EventDispatcher::trigger). The side that re-reads them holds an
//! [`EventListener`] and waits on [`triggered`](EventListener::triggered),
//! which wakes once something has moved and gives back `false` once the
//! session is over.
//!
//! ```rust
//! let dispatcher = EventDispatcher::new();
//! let mut listener = dispatcher.create_listener();
//!
//! // wherever something changes
//! dispatcher.trigger();
//!
//! // wherever it has to be looked at again
//! while listener.triggered().await {
//!     // something moved; go and read the session
//! }
//! ```
//!
//! Triggers that land while a listener is busy fold into one wake, so wakes
//! never match triggers one for one: a listener re-reads whatever it cares
//! about on every wake rather than counting them.
//!
//! # Implementation
//!
//! Three parts. [`Notify`] parks a listener, the version is how one that was
//! busy finds out it missed a trigger, and the dispatcher count is how it
//! learns there will not be another. The order they have to be touched in is
//! on [`trigger`](EventDispatcher::trigger) and
//! [`triggered`](EventListener::triggered).

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use tokio::sync::Notify;

/// Clone one into anything that changes state, and call
/// [`trigger`](EventDispatcher::trigger) when it does.
///
/// Holding one commits to nothing: it never fails, and it does not care
/// whether anybody is listening.
pub struct EventDispatcher {
    inner: Arc<EventInner>,
}

impl EventDispatcher {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EventInner::new()),
        }
    }

    /// A listener that wakes on changes from here on.
    ///
    /// It starts up to date: a trigger from before this call is not one it
    /// will be woken for.
    pub fn create_listener(&self) -> EventListener {
        EventListener {
            inner: self.inner.clone(),
            version: self.inner.version.load(Ordering::Relaxed),
        }
    }

    /// Say that something moved.
    ///
    /// Never fails and never reports having had no listeners: nobody attached
    /// is the normal case — `tush serve` with no screen on it — and not
    /// something a caller should have to handle.
    ///
    /// # Implementation
    ///
    /// The version moves before the wake goes out, and has to: a listener
    /// that builds its `notified` after `notify_waiters` has returned is not
    /// covered by that wake, and the bump is the only thing left for it to
    /// see.
    pub fn trigger(&self) {
        self.inner.version.fetch_add(1, Ordering::Relaxed);
        self.inner.notify.notify_waiters();
    }
}

/// Manual, because cloning one is what the count is counting.
impl Clone for EventDispatcher {
    fn clone(&self) -> Self {
        let inner = self.inner.clone();
        self.inner.dispatcher_count.fetch_add(1, Ordering::Relaxed);
        Self { inner }
    }
}

/// The last one out wakes whoever is parked, which is how a listener learns
/// there will be no more triggers.
impl Drop for EventDispatcher {
    fn drop(&mut self) {
        let count = self.inner.dispatcher_count.fetch_sub(1, Ordering::Relaxed);
        if count == 1 {
            self.inner.notify.notify_waiters();
        }
    }
}

/// The receiving half, one per thing that waits on a session.
///
/// `Clone` because [`triggered`](EventListener::triggered) needs `&mut`: a
/// holder that keeps a listener in a field waits on a clone of it, and a clone
/// comes up to date as of the moment it was taken.
#[derive(Clone)]
pub struct EventListener {
    /// Shared with every dispatcher and every other listener.
    inner: Arc<EventInner>,
    /// The version last handed back: what makes a trigger that arrived while
    /// the caller was busy still be there the next time it asks. Compared
    /// with `!=` and never `<`, so the counter is free to wrap — aliasing
    /// would need a listener exactly `u32::MAX` triggers behind.
    version: u32,
}

impl EventListener {
    /// Wait until something moves; `false` once every dispatcher is gone.
    ///
    /// A `bool` and not a `Result` because there is one way to fail and it is
    /// not an error: the session it was listening to has ended, and the
    /// caller's answer to that is to stop, not to handle it.
    ///
    /// # Implementation
    ///
    /// The count is read again after the wake, since the last dispatcher
    /// dropping sends one too and that is not a change.
    ///
    /// `notified` is built before the version is read and has to stay there:
    /// it records [`Notify`]'s wake count as it stands, so a trigger landing
    /// after the version load still completes the await, and one that landed
    /// before it is ordered into view by that count's own `SeqCst`. Swap the
    /// two lines and the first case is a lost wakeup.
    #[must_use = "`false` means the session has ended; a loop that ignores it never stops"]
    pub async fn triggered(&mut self) -> bool {
        let notified = self.inner.notify.notified();
        let count = self.inner.dispatcher_count.load(Ordering::Relaxed);
        if count == 0 {
            return false;
        }
        let version = self.inner.version.load(Ordering::Relaxed);
        if self.version != version {
            self.version = version;
            return true;
        }
        notified.await;
        self.version = self.inner.version.load(Ordering::Relaxed);
        self.inner.dispatcher_count.load(Ordering::Relaxed) != 0
    }
}

/// What both halves hold, and all they share.
struct EventInner {
    /// `notify_waiters` and never `notify_one`: every listener wants the
    /// wake, and a permit stored for one of them is a wake the rest never
    /// get.
    notify: Notify,
    /// Live [`EventDispatcher`]s, counted here rather than read off the
    /// `Arc`, whose strong count the listeners are in too and so would never
    /// reach zero while one of them is still waiting.
    dispatcher_count: AtomicU32,
    /// Bumped by every trigger, and before the wake goes out — see
    /// [`trigger`](EventDispatcher::trigger).
    version: AtomicU32,
}

impl EventInner {
    fn new() -> Self {
        Self {
            notify: Notify::new(),
            dispatcher_count: AtomicU32::new(1),
            version: AtomicU32::new(0),
        }
    }
}
