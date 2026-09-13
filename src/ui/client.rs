use arcstr::ArcStr;

use crate::{
    log::LogRegion,
    runner::RunnerState,
    unit::{UnitChoice, UnitEvent},
};

/// One unit, as the screen needs it: what it is called, and where its run was
/// the last time anybody looked.
///
/// The state is a copy, not a view. It is already slightly out of date by the
/// time it is drawn, and that is fine — it is a report of what the session
/// looked like at the last [`sync`](UiClient::sync), which is all a frame
/// ever shows.
#[derive(Clone, Debug)]
pub struct UiUnit {
    /// The name the unit was declared under, which is how a [`UiCommand`]
    /// addresses it — and nothing else. It is not shown; see
    /// [`name`](UiUnit::name).
    pub key: ArcStr,
    /// What to call it on screen.
    ///
    /// Separate from the key because the config lets a proc give itself one
    /// (`name:`), falling back to the key when it does not. Folding the two
    /// together would mean either showing an identifier where a title was
    /// meant, or addressing a unit by something a person was free to change.
    ///
    /// An [`ArcStr`], like everything that comes out of a unit's behavior:
    /// the text never changes, and refreshing this several times a second
    /// should not mean copying it several times a second.
    pub name: ArcStr,
    /// Which of its modes it is on, for a unit that has modes.
    ///
    /// `None` is not "no mode" but "no modes at all", which is most units —
    /// and it has to stay tellable from a mode, because a row shows nothing
    /// here rather than inventing a label for a proc that runs one way.
    pub mode: Option<ArcStr>,
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
    Start { key: ArcStr },
    /// Stop it.
    Stop { key: ArcStr },
    /// Hand it an event and let its behavior decide.
    ///
    /// What every entry of the action menu sends, because every entry came
    /// out of the behavior in the first place — including the ones that move
    /// a unit onto another mode, which nothing outside the behavior could do.
    Dispatch { key: ArcStr, event: UnitEvent },
}

/// What a client has for the pane: some lines, and what they are.
///
/// The lines are [`String`]s because that is what
/// [`copy_region`](crate::log::LogReader::copy_region) produces. A struct per
/// line, with room for which stream it came from, would be a shape this
/// cannot fill — when stderr wants its own colour it is the copy underneath
/// that has to learn about it first, not this.
pub struct UiLog<'a> {
    /// Which region these lines actually are.
    ///
    /// The answer's region, not the question's, and the two can differ: a
    /// client that has not caught up with a scroll holds the region from
    /// before it. Reporting it is what lets the pane draw what came back, at
    /// the offset it belongs at, rather than blanking for as long as a round
    /// trip takes.
    pub region: LogRegion,
    /// What the log these came from had written when they were taken.
    ///
    /// A change token and nothing more: compare two, and different means the
    /// log moved under the region. It says *that* it moved, not by how many
    /// lines — the log counts chunks, and one chunk is one or two lines or
    /// part of a long one.
    ///
    /// So it is what a client polls to know whether to fetch the region
    /// again, and what a pane scrolled up uses to know its view has drifted —
    /// which it can report to the reader, but not correct.
    ///
    /// Only comparable within one unit. Two logs count their own, so a
    /// revision from before the selection moved means nothing after it.
    pub revision: u64,
    /// The lines, oldest first.
    pub lines: &'a [String],
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

    /// Everything that can be asked of the unit under `key` right now, for
    /// the menu to list.
    ///
    /// Called when a menu opens and not per frame, which is why it may look
    /// like work: a list of every unit's choices, rebuilt thirty times a
    /// second to serve the one popup that might open, is work proportional to
    /// the units to answer a question about one of them.
    ///
    /// Filled into a `Vec` the caller owns and reuses, so that opening a menu
    /// again costs the refcounts and not an allocation.
    ///
    /// Local and instant like every other read here, which for a client with
    /// a connection means answering from what it last synced — and filling
    /// nothing if that is nothing, the way [`log`](UiClient::log) hands back
    /// what it has rather than waiting for what was asked.
    fn choices(&self, key: &str, out: &mut Vec<UnitChoice>);

    /// Ask for something to happen, and do not wait to find out whether it
    /// did.
    ///
    /// `&self` rather than `&mut self` so that sending is not a mutation of
    /// the view — which leaves the door open for something other than the
    /// render loop to hold a client and ask, a signal handler wanting
    /// everything stopped being the obvious one.
    fn send(&self, command: UiCommand);

    /// Say which log the pane is showing, and which rectangle of it.
    ///
    /// Declaring rather than asking, for the same reason
    /// [`send`](UiClient::send) does not wait: fetching a region may be a
    /// round trip, and a screen cannot be made to hold still for one. This
    /// starts whatever the client has to do; [`log`](UiClient::log) is where
    /// the result turns up, one sync or several later.
    ///
    /// `key` of `None` is a pane with nothing selected, and releases whatever
    /// the client was holding for the last one.
    ///
    /// Ask for more than the pane draws. The extra is what the view scrolls
    /// within without asking again — the region is a buffer as much as a
    /// request, and a rectangle is cheap: a few hundred lines clipped to the
    /// pane's width is tens of kibibytes whatever the log behind it is.
    ///
    /// Called every frame with the current region. Deciding whether anything
    /// has to happen is the client's: it is the one holding the region, the
    /// revision it was taken at, and the connection it would have to use.
    fn set_log(&mut self, key: Option<ArcStr>, region: LogRegion);

    /// The lines for the pane, as of the last [`sync`](UiClient::sync).
    ///
    /// `None` while there is nothing to draw at all: no unit selected, or a
    /// first region that has not arrived. Once there is something there is
    /// something — a client hands back the region it has even when a newer
    /// one was asked for, rather than emptying the pane while it catches up.
    fn log(&self) -> Option<UiLog<'_>>;
}
