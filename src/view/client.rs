use crate::unit::UnitKey;
use crate::util::str::SmallStr;
use crate::{
    log::{LogLine, LogRegion},
    runner::RunnerState,
    unit::{UnitChoice, UnitEvent},
};

/// One unit, as the screen needs it.
///
/// A copy and not a view: it reports what the session looked like at the last
/// [`sync`](ViewClient::sync), which is all a frame ever shows.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ViewUnit {
    /// How a [`ViewCommand`] addresses it. Not shown; see [`name`](ViewUnit::name).
    pub unit_key: UnitKey,
    /// The key the config declared it under, and what a path addresses it by:
    /// [`unit_key`](ViewUnit::unit_key) is an interned symbol that means
    /// nothing outside the process that minted it, and
    /// [`name`](ViewUnit::name) is a label a person may change.
    pub key: SmallStr,
    /// What to call it on screen.
    ///
    /// Apart from the key because the config lets a proc name itself, and
    /// folding the two would mean either showing an identifier or addressing
    /// a unit by something a person may change.
    pub name: SmallStr,
    /// A shorter name to show where [`name`](ViewUnit::name) will not fit.
    ///
    /// `None` is the config having said nothing, not "use the long one": what
    /// to do about it is the pane's, since the pane is what knows its width.
    pub name_short: Option<SmallStr>,
    /// Which mode it is on. `None` means no modes at all, which is most
    /// units — the row shows nothing rather than inventing a label.
    pub mode: Option<SmallStr>,
    /// That mode's short name, on the same terms — `None` both for a unit
    /// with no modes and for a mode that declared none.
    pub mode_short: Option<SmallStr>,
    /// Where its run was at the last sync.
    pub state: RunnerState,
}

/// Something the user asked for.
///
/// One type rather than a method per verb because this is what goes on the
/// wire: the socket client serializes a `ViewCommand` and the server plays it
/// back into an [`App`](crate::app::App). The key goes in by copy, a command
/// outliving the frame that made it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ViewCommand {
    /// Run it, restarting it if it was already running.
    Start { key: UnitKey },
    /// Stop it.
    Stop { key: UnitKey },
    /// Hand it an event and let its behavior decide.
    ///
    /// What every menu entry sends, the entries having come out of the
    /// behavior in the first place.
    Dispatch { key: UnitKey, event: UnitEvent },
}

/// What a client has for the log pane: some lines, and what they are.
pub struct ViewLog<'a> {
    /// The answer's region, not the question's. A client behind a scroll
    /// holds the older one, and reporting it lets the pane draw what came
    /// back at the offset it belongs at instead of blanking.
    pub region: LogRegion,
    /// A change token: two different values mean the log moved under the
    /// region. It does not say by how much, and it is only comparable within
    /// one unit — each log counts its own.
    pub revision: u64,
    /// The lines, oldest first.
    pub lines: &'a [LogLine],
}

/// What a view reads a session through, and sends its commands down.
///
/// Two implementations — a session in this process, and one over a socket for
/// `tush attach` — and the screen never knows which it got.
///
/// **Nothing here is async and nothing fails.** A screen has to keep drawing
/// whatever the session is doing, so it can be made to wait neither on a
/// round trip nor on "did that work?": reads come from a snapshot the client
/// already holds, and [`send`](ViewClient::send) is fire and forget. What that
/// costs is the acknowledgement — a refused command has to come back as
/// something the next [`sync`](ViewClient::sync) picks up, and that channel
/// does not exist yet.
///
/// **The set of units does not change**, coming from a config read once, so
/// [`units`](ViewClient::units) hands back a slice built at construction and
/// `sync` writes into the rows already there.
pub trait ViewClient {
    /// Take in whatever has changed since the last frame.
    ///
    /// Called once per frame before anything is read, so that one frame is
    /// not drawn out of two different moments.
    fn sync(&mut self);

    /// Every unit, as of the last [`sync`](ViewClient::sync), in the same order
    /// every time — a list that reshuffles itself cannot be pressed.
    fn units(&self) -> &[ViewUnit];

    /// Everything that can be asked of the unit under `key` right now.
    ///
    /// Called when a menu opens, not per frame. Local and instant like every
    /// read here, so a client with a connection answers from its last sync,
    /// filling nothing if that is nothing.
    fn choices(&self, key: UnitKey, out: &mut Vec<UnitChoice>);

    /// Ask for something to happen, without waiting to find out whether it did.
    ///
    /// `&self`, so asking is not a mutation of the view and something other
    /// than the render loop — a signal handler, say — could hold a client.
    fn send(&self, command: ViewCommand);

    /// Say which log the pane is showing, and which rectangle of it.
    ///
    /// Declaring rather than asking: fetching a region may be a round trip,
    /// so this only starts whatever the client has to do and
    /// [`log`](ViewClient::log) is where the result turns up, one sync or
    /// several later. `None` releases whatever was held for the last unit.
    ///
    /// Ask for more than the pane draws — the extra is what the view scrolls
    /// within without asking again, and a rectangle clipped to the pane's
    /// width is cheap whatever the log behind it is.
    ///
    /// Called every frame; whether anything has to happen is the client's
    /// call, since it holds the region, the revision and the connection.
    fn set_log(&mut self, key: Option<UnitKey>, region: LogRegion);

    /// The lines for the pane, as of the last [`sync`](ViewClient::sync).
    ///
    /// `None` only when there is nothing at all: no unit selected, or a first
    /// region that has not arrived. After that a client hands back the region
    /// it has rather than emptying the pane while it catches up.
    fn log(&self) -> Option<ViewLog<'_>>;
}
