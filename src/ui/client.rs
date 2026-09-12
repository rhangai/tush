use crate::{runner::RunnerState, unit::UnitEvent};

/// One unit, as the screen needs it: what it is called, and where its run was
/// the last time anybody looked.
///
/// The state is a copy, not a view. It is already slightly out of date by the
/// time it is drawn, and that is fine — it is a report of what the session
/// looked like at the last [`sync`](UiClient::sync), which is all a frame
/// ever shows.
#[derive(Clone, Debug)]
pub struct UiUnit {
    /// The name the unit was declared under, which is also how a
    /// [`UiCommand`] addresses it.
    pub key: String,
    /// Where its run was at the last sync.
    pub state: RunnerState,
}

/// Something the user asked for.
///
/// One type rather than one method per verb, because this is what goes on the
/// wire: the remote client serializes a `UiCommand` and the server plays it
/// back into an [`App`](crate::app::App). A trait method per verb would mean
/// re-deriving that enum at the socket anyway, and the two would drift.
///
/// The key is owned for the same reason — a command has to outlive the frame
/// that produced it.
#[derive(Clone, Debug)]
pub enum UiCommand {
    /// Run it, restarting it if it was already running.
    Start { key: String },
    /// Stop it.
    Stop { key: String },
    /// Hand it an event and let its behavior decide — the mode switch, for a
    /// unit that has modes.
    Dispatch { key: String, event: UnitEvent },
}

/// What the UI reads a session through, and sends its commands down.
///
/// A client, not a source, because that is what both implementations are:
/// [`UiApp`](crate::ui::UiApp) is a client of a session in this process, and
/// the one behind `tush attach` will be a client of a session on a socket.
/// The UI never knows which it got.
///
/// # Nothing here is async, and nothing here fails
///
/// Both are the same decision. A screen has to draw thirty times a second
/// whatever the session is doing, so it cannot be made to wait on it — not on
/// a round trip, and not on an answer to "did that work?". So reading is
/// local and instant, and asking is [`send`](UiClient::send): fire and
/// forget, no `Result`, no await.
///
/// What that costs is the acknowledgement. A command that the session
/// refuses — a name it does not have, a socket that went away — is not
/// reported back through this call. It cannot be: the call is over before the
/// session has seen it. It has to come back the way every other change does,
/// as something the next [`sync`](UiClient::sync) picks up, and that channel
/// does not exist yet.
///
/// # Reading, and where the waiting went
///
/// It went inside the implementation. Whatever a client has to do to know
/// what it knows — read a `HashMap` of atomics, drain a socket, keep a task
/// alive to fill a buffer — happens on its own terms and is finished by the
/// time [`units`](UiClient::units) is called. `sync` is where a frame draws
/// the line: take in whatever has arrived, then render from a set of values
/// that does not move while it is being drawn.
///
/// This is the same split [`LogReader`](crate::log::LogReader) already makes,
/// for the same reason — `sync` to take a consistent view, then read it.
///
/// # The set of units does not change
///
/// It comes from a config that was read once, so [`units`](UiClient::units)
/// hands back a slice it built at construction rather than a `Vec` it
/// allocates per frame; `sync` writes the new states into the rows already
/// there. If a reconfigurable daemon ever makes that false, it is this
/// contract that changes, not the signature.
pub trait UiClient {
    /// Take in whatever has changed since the last frame.
    ///
    /// Called once per frame, before anything is read. What it actually does
    /// is the implementation's business — and for one that has nothing to
    /// wait for, doing it here rather than in `units` is what keeps a single
    /// frame from being drawn out of two different moments.
    fn sync(&mut self);

    /// Every unit of the session, as of the last [`sync`](UiClient::sync).
    ///
    /// In the order they should be listed, and in the same order every time:
    /// a list that reshuffles itself is a list nobody can press
    /// <kbd>Enter</kbd> on.
    fn units(&self) -> &[UiUnit];

    /// Ask for something to happen, and do not wait to find out whether it
    /// did.
    ///
    /// `&self` rather than `&mut self` so that sending is not a mutation of
    /// the view — which leaves the door open for something other than the
    /// render loop to hold a client and ask, a signal handler wanting
    /// everything stopped being the obvious one.
    fn send(&self, command: UiCommand);
}
