---
name: rust-maintainable
description: The target shape for Rust in this repo — not the code as it stands — so the person maintaining it reads it right the first time: which layer code belongs in, who owns state, how tasks, locks and signals are shaped, where input is checked, when an optimisation earns its complexity, what needs permission, and the house shape of names, errors, matches and modules. Use whenever writing or changing Rust in this repo.
paths: ["src/**/*.rs"]
---

# Maintainable

A person takes every decision in this repo and reads every line. Code that is
fast but has to be explained has cost more than it saved. This is the
writing-time half; `/engineer:arch` is the review.

**Sound, then readable, then fast** — in that order, every time they pull
apart. Undefined behaviour is never traded for anything (`/engineer:soundness`
for any `unsafe`). Readability beats speed except where the cost is plainly
wasteful on a hot path (`rust-hot-path`).

**This describes the target, not the code as it stands.** Much of the code
took the shape that was easy at the time, not the one that was good, so
"it is done this way elsewhere" is not a reason. Two sources carry weight:
AGENTS.md, which is the maintainer's decisions, and the reasoning below. A
code example is cited where it gets the point right; where today's code does
the opposite, it is named as such. Code that diverges is not refactored in
passing — name it in one line and carry on (AGENTS.md, _A finding is not a
mandate_). New code follows the target.

## Where it goes

`ARCHITECTURE.md` describes the layers and what each is for. The import order
as it stands, each importing only from those before it:

```
util → error → log → base → runner → unit → config → app → view → ui, server
```

`cli` and `main` sit on top.

- **An import never points up.** If lower code needs something from above,
  the design is asking for it to be passed in or moved down. Ask; do not
  import.
- **Above `view`, only values come up from below** — keys, regions, states,
  choices. Never `App`, a `Unit` or a handle. The screen sees a session
  through `ViewClient`, which has to survive becoming a socket (AGENTS.md,
  _The view client_).
- **`util` knows nothing of processes, `base` nothing of units.** A helper
  that needs a domain word belongs in the layer that owns that word.

## Ownership

- **Ownership is a tree.** Every piece of state has one owner, and every task
  is owned by whoever started it and ends when that owner drops. When
  ownership is a tree, nothing needs a pointer back to its parent.
- **A task that owns its state beats state shared behind a lock.** With
  `Arc<Mutex<T>>` every holder is a writer, every read takes the lock, and the
  invariant is spread over every call site that locks it. A task that owns
  `T`, with a handle that sends it requests, keeps the invariant in one loop
  and the type in one file. A shared lock is for small data read far more
  often than written, and its doc says why it is not a task.
- **One writer per fact.** Readers may hold copies — a snapshot, a value
  behind a version number, a buffer swapped out — as long as exactly one
  place writes the fact and the copies say which way they can be stale. Two
  places that both write the same fact is the defect.
- **`Arc` is fine where it is built and handed out; it is the clone in a
  loop that costs.** Each clone and drop is an atomic, contended when other threads hold it,
  so on a hot path borrow the `Arc` or keep one clone made at setup — never
  clone per frame, poll or line (`rust-hot-path`).
- **A `Weak` back to a parent means the tree has a cycle.** The fix is in the
  structure: the parent passes the child what it needs, or the child sends
  messages up, or the parent owns and ends the child so the child never
  outlives it. Today `AppSchedule`, `UnitHandle` and `UnitHandleManager`
  (built with `Arc::new_cyclic`) hold `Weak`s to what owns them — the easy
  shape, not one to copy.
- **A run is a value; restart makes a new one.** A `RunnerHandle` is one run
  and is never reset; what lives as long as the unit — the log — belongs to
  the unit.
- **A field that means something only given another field is fine** — the
  reason is in AGENTS.md, _Cleverness_.

## Tasks, locks and signals

- **Every spawned task has someone who ends it.** Keep the `JoinHandle` with
  whoever owns the work and end it on `Drop` or with a `CancellationToken` —
  `ViewSocket` cancels and aborts its task when it drops. A task left
  detached says why in its doc, as `cancel_on_interrupt` in `main.rs` does.
- **When a lock is the right call, it is short and never spans an
  `.await`.** Do the work outside; inside, swap or copy out, then release the
  guard before taking the next one. In `async` code release it by closing a
  `{ }` block, not with `drop`: a guard whose scope reaches an `.await` makes
  the whole future non-`Send` even if it was dropped first (checked on rustc
  1.98). `parking_lot` is not reentrant — locking twice on one thread hangs —
  and two locks in one path is a lock order that goes in the doc.
- **A signal is not a message.** "Something moved, look again" is a signal:
  wakes fold together and the listener re-reads what it cares about. Data
  that must not be lost goes on a channel. Putting data on a signal turns it
  into a channel, with a buffer and a lost-message question.
- **Reconcile, do not wait.** Code that has to act once something else is
  ready writes down what it wants and re-checks on each change, rather than
  blocking on the other side. Re-checking everything on every change costs
  O(everything); say so in the doc and narrow it when it shows up in a
  measurement.
- **Every `.await` is a place the future can be dropped.** In a `select!` arm
  or a cancellable task, name what is half done at each one — a handle
  published but never awaited, a state never moved to terminal — and make it
  safe or say why it is.
- **No `async` without an `.await`**, and nothing async under the view
  client: async spreads to every caller.
- **`Drop` cannot await.** Orderly shutdown is an `async fn` the owner calls;
  `Drop` is the safety net for when it was not called, and does only what
  cannot fail or block — as `Process` does by killing its group.
- **Nothing blocks the runtime.** `std::fs`, a long lock under contention, or
  CPU work between awaits holds a worker thread; do it before the runtime
  starts or in `spawn_blocking`.
- **A decision made under a lock is stale after an `.await`.** Lock, decide,
  release, await, act is a race: re-check after the await, or make the action
  safe to repeat.

## The boundary

- **Check input once, where it enters, into a type that proves it.** Holding
  an `App` means the config was checked: keys exist, dependencies resolve,
  cycles were named. Below it, nothing re-checks; a re-check means the proof
  is too weak, so strengthen `App::new` instead.
- **Collect every error, not the first.** `AppUnitMap::new` gathers each
  `AppConfigError` and reports them together; a person fixing a config wants
  the whole list. A bad dependency graph is an answer, not a failure:
  `DependencyGraph::resolve` returns an order plus the cycles it broke.
- **Names become keys once**, at the boundary. Everything after it addresses
  a unit by `AppUnitKey`; a `&str` name travelling inward is a lookup someone
  will repeat.
- **Every way in keeps the same rule.** When a structure has several entry
  points, a rule kept on one is kept on all: if a read flushes, so do `end`
  and the notes. The one left out is where the bug is.

## Speed against readability

1. **Readable by default.** An optimisation comes in with a number from
   `/engineer:perf`; one with no number loses to the plain version. A readable
   version within a few percent of the clever one wins, and a micro-optimisation
   off the hot path does not come in at all.
2. **A measured win that costs readability comes in with its reason beside
   it**: what it does, the number, what breaks if it is undone. The
   `serde_derive` comment in `Cargo.toml` is the model — five lines, and the
   next person will not delete it.
3. **The hot-path idioms are not optimisations.** Clearing and refilling,
   `write!` into a kept buffer, swapping two buffers: they read as plainly as
   the slow version, so just write them (`rust-hot-path`).
4. **If explaining it needs "generic", "blanket", "phantom" or "zero cost",
   ask first** (AGENTS.md, _Cleverness_).
5. **Prefer a better data structure to a cleverer loop.** When code needs
   tricks to be fast enough, the structure underneath is usually the problem;
   propose the alternative (`/engineer:arch`) instead of tuning around it.
6. **Let what bounds the problem shape the algorithm.** Before writing a
   loop, find what limits the answer — a maximum, the end where the decision
   is made — and start there, stopping where the limit says. `cut_at` looks
   for an escape left open at a cut: an escape is at most `LOG_ESCAPE_MAX`
   bytes, so the answer lies within that many bytes back from the cut, and
   one backward scan settles it. A version that searched the line from its
   start for an `ESC` and walked on for the final byte also worked, read
   badly, and touched the whole chunk in the worst case. Done this way the
   code is faster and plainer at once, and that is the target — not a trade
   between the two.
7. **The code reads as the sentence that says it.** Say what a non-trivial
   body does in one sentence; if the code does not read as that sentence,
   restate the problem rather than polish the loop. `cut_at`'s is "back from
   the cut, a final byte first means every escape is closed, an `ESC` first
   means one is open" — and that is the loop, and its comment.
8. **Named state over a clever chain.** An iterator that carries state
   between items is a struct with plain fields and a `next` that loops; a
   chain of `zip`/`chain`/`once`, or a closure holding `mut` state, is quick
   to write once and has to be pieced together by every reader after.
   `TextLine::clip` went through three such versions, each read as worse;
   `TextClipIter`, with `current` and `end` and a `while`, is the one that
   reads.

## Needs permission, every time

AGENTS.md, _Cleverness_, has the list. The same goes for the patterns that
make a reader learn a mechanism before the code: typestate, builders, extension or sealed traits, blanket impls,
`macro_rules!` where a function would do, `Deref` to reach a field, operator
overloading, and a newtype that only saves two words.

## Names

- **A type carries its module**: `AppUnitMap`, `LogReader`, `ViewClient`,
  `UiRenderUnitsState` (`ui/render/units`, its `State`). This is the
  convention in force and new types follow it; it is the maintainer's to
  change, not a pass's.
- **Use the words already here** — unit, runner, handle, panel, mode, group,
  key, region. A synonym for one of them is a second concept to a reader.
- `as_` is free, `to_` costs, `into_` consumes; no `get_`; booleans read as
  `is_`/`has_`. Acronyms are words: `Http`, not `HTTP`.

## Control flow

- **The unhappy path leaves early** — `let … else`, `?`, an early `return` —
  so the path that matters stays at the left margin.
- **Match owned enums exhaustively.** A `_` arm makes a new variant compile
  silently; on a state machine like `RunnerState` that is how a state gets
  forgotten. `matches!` for a yes-or-no test.
- **Extract a function when it has a name a caller would use**, not to make
  the parent shorter. A helper called once, from one place, is a jump the
  reader makes for nothing.

## Errors

- **Error types live in `src/error.rs`**, one `thiserror` enum per layer; a
  new error goes there too. Moving them beside their layers is a decision
  for later, not one to make in passing.
- **`thiserror` everywhere; `anyhow` only where a person reads the message
  and nobody matches on it**: `main` and `cli`. `config` and `app` still
  return `anyhow` from early on and are moving to `thiserror`; new code there
  returns a typed error.
- **Each variant says something different.** Two variants with the same text
  are one failure to the reader. Messages lowercase, no trailing period —
  `"empty process"`, `"unit already started"`.
- **A `Result` nobody can act on is noise; an `Option` whose `None` means
  something specific says what in the doc.**
- **`expect` only for a broken invariant, and the message names it**:
  `"capacity overflow"` does, `"should never happen"` does not. Anything a
  config file or a user can cause is a `Result`.

## Types

- **Derive only what is true.** `Default` on a type with an invariant hands
  out a value nobody checked. `PartialEq`/`Hash` on a type holding a key and a
  name lets two values for the same unit compare unequal.
- **`Clone` on a type holding a lock or a channel is a decision about
  identity** — a second handle to the same thing, not a copy. Say which in
  the doc, or do not derive it.
- **Closed set of kinds: an enum.** A trait object is for a set that is
  open; for one that is closed, `match` on an enum is what a reader can
  follow.
- **Bounds in a `where` clause**, and `impl Trait` in argument position only
  where nobody will want to name the type.
- **Borrow in signatures**: `&[T]` and `&str`, not `&Vec<T>` and `&String`.
- **A constructor hands back a value whose invariant already holds.** A type
  that is only valid after a second call has that call inside its
  constructor, not in every caller.
- **An alias guarantees nothing.** If the aliased type can be built in a way
  that breaks what the name promises, it is a struct with one constructor.
- **What refuses its input hands it back**: `Result<T, Input>`, not
  `Option<T>`, above all for what cannot be rebuilt, such as an arena block.

## Modules

- **Submodules are private; the parent `pub use`s what is meant to be used**
  (`view.rs`, `log.rs`, `ui.rs`). Something not re-exported is internal, and
  a reader knows it from the parent file alone. Reach in through a
  re-export, or add one deliberately.
- **A narrow surface is what keeps an invariant checkable**, above all in
  `log` and `runner`. Widening one is a core-module change: propose it
  (AGENTS.md, _Scope_).

## Comments

A doc says why, in one sentence per decision; `/doc` carries the rest.
Comments and identifiers in English.

## Before reporting

`cargo fmt`, `cargo clippy --all-targets` (clean, not quiet), `cargo test`.
`clippy.toml` bans naming `smol_str::SmolStr` outside `util/str.rs`: use
`SmallStr`.
