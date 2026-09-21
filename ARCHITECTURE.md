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

## A handle is one run; a unit outlives its runs

`RunnerHandle` supervises exactly one run and stays terminal once it is.
Restarting is not an operation on a handle — it is a new handle. That is why
the `Log` belongs to the `Unit` and not to the run: scrollback survives a
restart, and no reader has to resubscribe.

Handles are created **parked** at a start gate. The replacement can therefore
exist and be observed before the outgoing run has finished dying, which is
what makes "no two dev servers fighting over the same port" structural rather
than a sequence somebody has to get right.

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

The exception is deliberate and is the wire: a key means nothing outside the
process that minted it, so HTTP addresses units by a name a person can type.
See *Open* below — that translation is not right yet.

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

## Readiness is "a run finished"

`Unit::resolved` is true once a run has reached a terminal state — any
terminal state, a failure included. It stays true, so a later restart does not
put dependents back on hold. That is right for setup steps, which is what
`depends` was drawn for. It is the open question below for anything that never
exits.

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

## Who owns the children

Three commands, differing only in where the session is and where the screen
is:

| | session | screen | what ends it |
| --- | --- | --- | --- |
| `run` | here | here | quitting the screen |
| `serve` | here | stdout, and a socket | a signal |
| `attach` | elsewhere | here | quitting takes nothing down |

`run` owns its procs, so quitting has to take them with it, and
`UnitMap::shutdown` is awaited rather than left to `Drop`, which cannot await.
`serve` is the opposite: nothing a client does ends the session. The order at
the end is the session first and the printer second, because `shutdown` is
what writes the last note into each log.

## Strings

`SmallStr` for text that is shared and never changed — names, keys, modes,
config values — because up to 23 bytes live inline, so a clone is a copy and
not an atomic. `String` where the buffer is reused: log lines, anything
filled in place and truncated.

It was `ArcStr`, and is not going back: a bare pointer with no inline form
makes every word of a config its own allocation, measured at three for
`[npm, run, build]` against zero.

## Errors

`thiserror` per layer, `anyhow` where a human reads it. Every type lives in
`error`, gathered rather than kept beside what returns it, so the set is one
file to read.

## Open

Decisions that are not settled, written down so they are not mistaken for
ones that are.

- **What `depends` means.** The implementation is "has finished"; `CONFIG.md`
  says "has to be up". A proc that never exits therefore blocks its dependents
  forever, and `CONFIG.md`'s own `web depends: [api]` example does not run.
  Either the doc follows the code, or readiness needs a second predicate —
  running-and-stayed-running, a port, a line on stdout — and `Unit::resolved`
  becomes the one for `run:` steps only.
- **How a unit's key is escaped on the wire.** Which string crosses is
  settled: the config key, carried as `ViewUnit::key`, because a `UnitKey` is
  meaningless across processes and a display name is a label two procs may
  share. How it is escaped is not: both ends build the path segment with
  `format!`, so a key holding a space or a `/` still breaks the request.
  Either percent-encode it, or restrict the key charset at `App::new` the way
  `:` already is.
- **What `AppUnitMap` adds.** It owns the interner, the graph and the groups,
  which is its reason to exist; the fifteen methods that forward to `UnitMap`
  and rewrap `UnitMapError` as `AppError` are not, since `AppError` has one
  variant a caller ever sees.
