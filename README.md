# tush

A process manager built around log tailing — think `tail -f`, but it also knows
how to start, stop and restart the things it is tailing.

You describe a set of long lived commands (dev servers, build watchers, setup
scripts), `tush` supervises them and keeps the recent output of each one
available for display.

> **Status: early work in progress.** The runtime core — process supervision,
> log capture, state machine — works and is covered by tests. There is no CLI,
> no TUI and no config parser yet: `main` is a scratch program that exercises
> the restart handshake by hand.

## Running

```sh
cargo run     # runs the scratch main
cargo test    # 19 tests, including ring buffer concurrency stress
cargo doc --no-deps --document-private-items --open
```

Requires a recent Rust toolchain (edition 2024). The signal handling is
unix-only; other platforms fall back to a plain kill.

## Architecture

Three layers, each one unaware of the one above it:

```
Unit  ──owns──>  RunnerHandle  ──drives──>  Runner (Process, ...)
  │                                              │
  └──owns──>  Log  <───────writes lines──────────┘
```

| Module | Role |
| --- | --- |
| `base` | Raw primitives: `Process` (child in its own process group), `Log` (line ring buffer), `ExitReason`. |
| `runner` | Supervision of a single run: the `Runner` trait, `RunnerHandle` (one Tokio task per run) and the `RunnerState` machine. |
| `unit` | The user facing concept: a `Unit` is a named, restartable entry; a `UnitDescription` is its recipe. |
| `util` | Shared data structures — currently `RingStr`, the string ring behind the logs. |

A `RunnerHandle` supervises exactly **one** run and, once terminal, stays
terminal. Restarting is not an operation on a handle: it is a new handle. The
`Unit` is what persists across runs, which is also why it — not the handle —
owns the log.

### Runner lifecycle

```
        new()                start()            run()
────> Waiting ──────────────> Started ─────────> Running ──┬──> ExitSuccess / ExitError
         │                       │                         │
         └────── abort() ────────┴───> Killing ────────────┴──> Killed
```

Handles are created **paused**. That is what makes an orderly restart possible:
the incoming run can exist, and be observable, before the outgoing one has
finished dying.

## Design notes

A few decisions that are not obvious from the type signatures:

- **Restarts never overlap.** `Unit::set_handle` publishes the new handle
  immediately so callers see the incoming run, but only releases it after the
  outgoing one has been aborted *and* waited on. No two dev servers fighting
  over the same port.
- **Writing a log line never takes a lock.** The stdout pump pushes into a lock
  free `ThingBuf` queue; the lines are folded into the shared `VecDeque` later,
  by whoever reads next. A slow reader cannot back-pressure a child process.
- **Logs are bounded and lossy by design.** This is a tail, not a transcript.
  Both the ring and the staging queue drop the oldest lines when full, and
  `String` allocations are recycled rather than freed.
- **Readers get private snapshots.** A `LogBuffer` is a copy that only moves
  forward when explicitly refreshed, so a render loop can iterate freely
  without holding anything. A buffer belongs to the log that issued it —
  cursors carry no identity, so refreshing one against a different log is a
  precondition violation, not something the ring detects.
- **Reading is what advances the ring**, since the pending queue is folded in
  under the reader's lock. With nothing pending that lock is shared, so idle
  readers do not serialise against each other.
- **Signals go to the whole process group.** Children are spawned with their
  own group and killed with `kill(-pid, ...)`, so shutting down a
  `bash -c '...'` wrapper takes its children with it. Dropping a running
  `Process` `SIGKILL`s the group as a safety net.
- **Shutdown escalates.** `SIGTERM`, then `SIGKILL` after 10s.
- **State is a single atomic.** `RunnerState` packs into a `u16` (variant tag in
  the low byte, exit code in the high byte), so any task can poll a runner
  without a lock or a channel. Its variants are ordered, and `store_next` uses a
  CAS loop to keep progress monotonic — a late `Running` cannot undo a `Killing`
  another task already published.

## Planned configuration

Not implemented yet; `tmp/example.yaml` is the sketch the design is aiming at:

```yaml
procs:
  server-setup:
    group: [setup]
    run: [bash]

  server:
    group: [server]
    pre-condition: [server-setup]
    modes:
      - name: Build
        run: [bash]
      - name: Watch
        run: [bash]
```

- `group` — labels for grouping units in the interface.
- `pre-condition` — units that must finish before this one may start.
- `modes` — alternative descriptions for the same unit, switched at runtime.
  `Unit::start_with_description` is the mechanism this will use.

## Roadmap

- [x] Process supervision with graceful shutdown
- [x] Bounded, non-blocking log capture
- [x] Restart under a different description
- [ ] `UnitPool` — owning many units, resolving them by name
- [ ] `UnitContext` — dependencies and `pre-condition` ordering
- [ ] Config file parsing
- [ ] TUI for tailing and controlling units
- [ ] stderr capture (only stdout is captured today)
