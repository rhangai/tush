# The shape of tush

The decisions that span modules. What is settled inside one module is in that
module's `//!` — `log`, `app::schedule`, `runner::handle` and `util::graph`
carry the long arguments, and this file does not repeat them.

`README.md` is the tool. `CONFIG.md` is the file a user writes. This is the
crate.

## Three layers, one direction

```
base  ──>  runner  ──>  unit  ──>  app        the session
                                     │
                                   view  ──>  ui / server    what sees it
```

Nothing knows about the layer above it. `util` knows about nothing at all —
no process, no unit — and `base` knows about processes but not units. A change
that makes a lower layer name a higher one is the thing to object to, whatever
it buys the caller.

`view` is the one seam that points both ways by design: it reads `app`, `log`,
`runner` and `unit`, and none of them read it.

| Module | What is in it |
| --- | --- |
| `util` | Data structures with nothing to do with processes: `SmallStr`, the arenas and rings the logs are built on, the buffer pools the wire is written from, the dependency graph. |
| `base` | The OS: a child `Process` in its own group, and the `ExitReason` it finished with. |
| `log` | Capture of a proc's output into a bounded `Log`, and the readers and regions a view sees it through. |
| `runner` | Supervision of one run: the `Runner` trait, its `RunnerHandle` and the `RunnerState` it publishes. |
| `unit` | A named, restartable proc, and the `UnitBehavior` that decides what starting it means. |
| `app` | A checked `Config` and the session built from it: the unit map, the dependency graph, the schedule. |
| `config` | The file, parsed. |
| `view` | A session as something outside it reads and drives it — `ViewClient` and its two implementations. |
| `ui` | The terminal screen. |
| `server` | A session with no screen, listening for something to attach. |
| `cli` | The command line, as `clap` reads it. |
| `error` | Every error type in the crate, gathered in one file. |

## A handle is one run; a unit outlives its runs

`RunnerHandle` supervises exactly one run and stays terminal once it is.
Restarting is not an operation on a handle — it is a new handle. That is why
the `Log` belongs to the `Unit` and not to the run: scrollback survives a
restart, and no reader has to resubscribe.

Handles are created **parked** at a start gate. The replacement can therefore
exist and be observed before the outgoing run has finished dying, which is
what makes "no two dev servers fighting over the same port" structural rather
than a sequence somebody has to get right.

## A run's state only moves forward

`RunnerState`'s variants are ordered — everything from `ExitSuccess` down is
terminal — and the three `set_*` methods are the whole of the rule: each names
the states it may leave and returns the rest untouched. A report that arrives
late, a `run` closure firing after an abort, is dropped rather than rewinding
the run, and how a run finished outranks the abort that came too late.

It is published on a `watch`, so anything — a screen, the schedule, another
unit — reads the latest state without a channel of its own and without the
run knowing who is looking.

## `App` is the proof

Holding an `App` means the config was checked: every `depends` names a proc
that exists, no proc declares both `run` and `modes`, nothing waits on itself.
`App::new` is the only way to get one. Anything downstream that re-asks those
questions is either redundant or says the proof is too weak.

A bad config is an answer, not a failure. `DependencyGraph::resolve` always
returns an order plus the cycles it had to break, and `App::new` collects
every problem rather than stopping at the first — a config with three mistakes
is about to be fixed, and one per run is three runs of the same discovery.
New checks follow that or say why not.

## Names become keys once

`AppUnitMap` owns the interner and is the only place text becomes a `UnitKey`.
Everything above addresses a unit by key: a key is read out of a row, copied
into a command and compared every frame, where a name would be a string to
clone and hash. A `&str` name flowing upward is a defect.

The exception is deliberate and is the wire: a `UnitKey` means nothing outside
the process that minted it, so HTTP addresses a unit by the text it was
declared under, which `ViewUnit::key` carries. The config key and not the
display name, which is a label the config may change and two procs may share.

The client encodes that segment in one place — `key_path`, beside the routes
in `view::server` — rather than at each `format!` that builds a URL: a config
key is whatever somebody typed, and a space or a `/` in one otherwise makes a
request that does not parse or one that routes somewhere else.

## One event, coalescing

`EventDispatcher` is a `watch<()>`: "something moved, look again". Every unit
in a session shares one, so a screen watches the session and not a proc at a
time. Carrying data on it would make it a channel, with the buffering and the
lost-message question that come with that.

## Requests are recorded, not awaited

Nothing in the orchestration blocks on another unit. `AppSchedule` writes down
what is wanted; its task re-reads that set on every event and starts whatever
has become startable. A unit asked for directly is `Force` (restart it); one
pulled in as a dependency is `Schedule` (leave a running one alone).

The task holds both the session and the schedule **weakly**. Recording a
request against a session that is gone is a no-op, and a task that could keep
its own session alive is a shutdown that hangs.

Only *direct* dependencies are checked on each wake. The deeper ones need no
checking: they are pending too, and this is the loop that clears them.

## Readiness is what the proc's `type` says

`Unit::resolved` is true once a run has reached what its `UnitType` counts as
done: any terminal state for a `oneshot`, a failure included, and the moment
the process is up for a `service`. It stays true, so a later restart does not
put dependents back on hold. A service counts a terminal state too, or one
that dies before it ever runs would hold its dependents for good.

Up is `RunnerState::Running` and nothing further: the child was spawned. A
health check does not belong here — a port, a line on stdout or a grace period
are guesses about somebody else's program, and a wrong one releases a
dependent into a service that is not listening yet. The config's answer to
needing more is a `oneshot` that does the waiting.

The type lives on the `UnitBehavior` and not on the `Unit`, because a mode is
a behavior: a proc with `modes` takes it from the mode that is on, and its own
is what a mode that declared none was built with. `Unit::spawn` reads it under
the same lock as the run it builds and the handle carries it, so a run is
judged on the terms of the mode that produced it.

## The log has one owner and one rule

A `Unit` owns its `Log`; every process writes through a `LogWriterRef`, which
is weak so a reader task still draining a pipe cannot outlive the unit. The
ring is behind a mutex whose critical section is one swap — nothing is
decoded, allocated or printed while it is held, on either side.

A `LogReader` mirrors a whole log, which is what makes `sync` an offset
comparison. To show a window you copy one out: `copy_region` takes a
`LogRegion` and fills a `Vec<String>` the caller keeps, bounded in both
directions, so a view costs its own size and not the log's.

One reader per view, and one rule for where a line ends
(`LogReaderIter::ends_line`) — a second copy of that rule is a second chance
for a window and a render to disagree.

## `ViewClient` is the protocol

What a screen sees a session through, and one implementation of it is a
socket. So:

- **Nothing is async and nothing returns a `Result`.** Reads come from a
  snapshot the client already holds; `send` is fire and forget. A screen
  cannot be made to wait on a round trip, and cannot be stopped by one
  failing.
- **The UI declares, the client satisfies.** `set_log(key, region)` says what
  the pane wants; whether anything has to happen is the client's call, since
  it holds the region, the revision and the connection.
- **Answers carry what they actually are.** `ViewLog` reports its own region
  and revision, which can differ from what was asked, so a pane draws the
  overlap instead of blanking while a client catches up.

What that costs is the acknowledgement: a refused command has nowhere to come
back through. When the session grows that channel, `ViewApp::send` is where it
is written to.

`tush dispatch` is outside the trait for that reason. It waits and it fails,
because a shell wants an exit code and a command silently not delivered is the
thing to avoid there — so `ViewDispatch` talks to `ServerClient` directly,
which is the same routes both clients go through.

## Who owns the children

Four commands, differing in where the session is and where the screen is:

| | session | screen | what ends it |
| --- | --- | --- | --- |
| `run` | here | here | quitting the screen |
| `serve` | here | stdout, and a socket | a signal |
| `attach` | elsewhere | here | quitting takes nothing down |
| `dispatch` | elsewhere | none | the session answering the one command |

`run` owns its procs, so quitting has to take them with it, and
`AppUnitMap::shutdown` is awaited rather than left to `Drop`, which cannot
await. `serve` is the opposite: nothing a client does ends the session. The
order at the end is the session first and the printer second, because
`shutdown` is what writes the last note into each log.

## Strings

`SmallStr` for text that is shared and never changed — names, keys, modes,
config values — because up to 23 bytes live inline, so a clone is a copy and
not an atomic. `String` where the buffer is reused: log lines, anything
filled in place and truncated.

It was `ArcStr`, and is not going back: a bare pointer with no inline form
makes every word of a config its own allocation, measured at three for
`[npm, run, build]` against zero.

## Buffers are refilled, not rebuilt

A frame, a poll and a request are the same shapes at the same rate for as long
as a session is up, so the steady state is what they are written against: the
same allocation, cleared and filled again, rather than one per round.

- **The screen draws into the buffer** and builds no `Line` or `Span` to hand
  to a widget. A frame in steady state allocates nothing, and there is no
  cache, because nothing is built to cache.
- **What comes back over the socket is decoded over what the last answer
  left** — `deserialize_in_place` into the rows, the lines and the choices the
  screen and its poller trade back and forth.
- **What goes out is cut from a pool**: `BytesMutPool` for one client's paths,
  headers and bodies, `BytesMutSyncPool` for a server answering several
  requests at once. A piece is a view onto the pool's own allocation, and the
  slot comes back once that piece has been dropped.
- **An answer asked for far more often than it changes is kept serialized** —
  `BytesReusable`, which is what `/units` hands out a refcount of per poll.

What that costs is a field that means nothing on its own: several types here
keep a buffer past what it held, and the doc says what is in it in each case.
A shape that made the stale state unrepresentable would make the surviving
buffer unrepresentable too, which was the whole point.

## Errors

`thiserror` per layer, `anyhow` where a human reads it. Every type lives in
`error`, gathered rather than kept beside what returns it, so the set is one
file to read.

## Open

Decisions that are not settled, written down so they are not mistaken for
ones that are.

- **What `AppUnitMap` adds.** It owns the interner, the graph and the groups,
  which is its reason to exist; the five start and stop methods that look a
  key up and forward to the unit are not, since the only thing they add is an
  `AppError::NotFound` a caller mostly cannot act on.
