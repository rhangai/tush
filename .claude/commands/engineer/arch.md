---
description: Judge an architectural decision — ownership, seams, async, traits, errors — and say whether it is right, costly or a Rust antipattern. Opinion only, no edits.
argument-hint: <decision | module | seam | diff | sweep>
effort: high
---

# Architecture

Target: **$ARGUMENTS** — a decision ("should the schedule own the interner?"), a
module (`app`, `runner`), a seam (`Unit` ↔ `RunnerHandle`), a diff (`HEAD`, a
branch), or a sweep ("is there anything dumb in the orchestration?"). If it is
empty, ask which one and stop; do not pick.

The deliverable is an **opinion**. Nothing under `src` changes — not a rename,
not an extracted type, not a test. A design question gets answered, not
implemented (AGENTS.md, _Scope_). If the change is three lines, say the three
lines and let the user ask for them.

Opinions are welcome on anything: naming, module boundaries, what the config
file should look like, whether a feature is worth having at all. The one thing
this command does not do is design the UI — panes, layout, key handling and
drawing are settled elsewhere (`/ui`). `ViewClient` _is_ in scope, because it is the
protocol between a session and a screen and one day a socket.

## 1. The shape as it stands

Much of this code took the shape that was easy at the time, not the one that
was good. **What exists is evidence, not a standard**: judge it like anything
else, and a better shape is a welcome answer. What _is_ settled is the
maintainer's — AGENTS.md — and the target the skills describe
(`rust-maintainable`, `rust-hot-path`); judge against those.

Know the current shape before opining on it, so a proposal is not the thing
already there:

- **Layers.** `ARCHITECTURE.md` draws them; `rust-maintainable` has the
  import order as it stands. Nothing below `view` knows it exists; `ui` and
  `server` each hold a `ViewClient`.
- **Runs and units.** A `RunnerHandle` is one run; restart makes a new one.
  The log belongs to the unit, which outlives its runs.
- **`App` as proof.** Holding one means the config was checked; the errors are
  collected rather than stopping at the first, and `DependencyGraph::resolve`
  returns an order plus the cycles it broke.
- **Keys.** Names become `AppUnitKey`s once, in `AppUnitMap`.
- **One signal, coalescing.** `EventDispatcher` wakes listeners with no
  payload (a `Notify` plus a version counter); listeners re-read what they
  care about. Carrying data on it would make it a channel, with a buffer and
  a lost-message question.
- **Requests are recorded, not awaited.** The schedule writes down what is
  wanted and its task re-reads it on every event.
- **Shared state behind locks, and `Weak`s back to owners.** `Unit` keeps its
  state behind `parking_lot` mutexes; `AppSchedule`, `UnitHandle` and
  `UnitHandleManager` hold `Weak`s to what owns them. This is the part most
  likely to be the easy shape — `rust-maintainable`, _Ownership_, is the
  target.

## 2. Read before opining

In this order, stopping when you have it:

- The module `//!` and the item docs. `app/schedule.rs`, `runner/handle.rs`,
  `util/graph.rs` and `util/event.rs` state the reasoning that most questions
  about orchestration are already answered by.
- `AGENTS.md` — strings, the log, the UI client. Those are settled; cite, do
  not reopen. Reopening one needs a measurement, not an argument.
- History: `git log -S '<name>' --oneline -- src`, then `git show`. Several of
  these decisions were made _after_ the obvious one was tried and removed —
  the mutex, the `LogSpan`, the note handle. Proposing one of those back is
  the failure mode this section exists to prevent.
- The callers: `grep -rn '<name>' src`. An ownership question is a question
  about the set of call sites.

Prose goes stale before code does: where a doc and the source disagree, trust
the source and name the doc.

## 3. The questions to actually ask

Of any decision, in roughly this order — most of them are answered in one line
and only one or two will bite:

- **Is it the right data structure?** This is where cost is won without
  hurting the reading: a `Vec` indexed by a dense key (`AppUnitKey` is an
  interned `u32`) instead of a `HashMap`, a ring instead of a `Vec` that grows
  and shifts, one pool instead of a buffer per caller. Propose the structure,
  what it costs to switch, and what reads simpler afterwards — not a
  micro-optimisation of the one in place.
- **Who owns it, and who merely reads it?** Ownership should be a tree. A
  `Weak` back to a parent means the tree has a cycle — ask what restructuring
  would remove it before accepting the `Weak` as the answer.
- **Who writes it?** One writer per fact. Readers may hold copies — a
  snapshot, a version, a swapped buffer — as long as they say which way they
  can be stale. Two places that both write the same fact is the defect.
- **What happens when the task is cancelled here?** Every `.await` is a place
  the future is dropped. Name what is half done: a handle published but never
  waited on, a unit never moved to terminal, a scheduled request never
  cleared.
- **Does it need a lock at all, and what is the order?** `parking_lot` is not
  reentrant and a guard must not cross an `.await`. Two locks in one path is
  a lock order to write down.
- **Where does the work happen, and how often?** Work on every wake that could
  be answered once at build time is the schedule's `dependencies` map — that
  is the right fix, and the pattern to look for elsewhere.
- **What does it cost when there is a socket in the middle?** `ViewClient`
  has to survive becoming remote: nothing async, nothing returning `Result`,
  answers carrying what they actually are. A decision that only works
  in-process is a decision with a deadline.
- **Is the abstraction carrying its weight?** A trait with one implementer, a
  newtype that saves two words, a layer that only forwards. `RunnerSerial`
  earns it — it makes a list of commands indistinguishable from one process
  to everything above. Most do not.

Then run §4 over the same decision. A decision that survives this list can
still be the wrong thing to write in Rust, and that is the half a developer
walks past.

## 4. Rust antipatterns that look like good decisions

This is the half a design review usually misses. The decision reads well in
prose and is wrong in Rust — the borrow checker accepts it, `clippy` is quiet,
and the cost or the deadlock arrives later. Run the group that applies.

**Ownership and lifetimes**

- `Arc<Mutex<T>>` reached for as the default. The question is whether the data
  is shared or the _work_ is: a task owning `T` and taking messages needs no
  lock, and keeps the invariant in one loop. `Unit` shares its state behind
  locks today; whether that is right is a fair question, not a precedent.
- A spawned task holding an `Arc` to what spawned it: the session can then
  never drop. The fix is ownership — the parent owns the task and ends it,
  so the task never outlives it — rather than a `Weak` with a silent no-op on
  failed upgrade, which is what the code does today.
- `Clone` on a type that owns a lock or a channel: cloning now means "another
  handle to the same thing", which is a decision about identity, not a
  convenience. Say which one it is in the doc or do not derive it.
- A lifetime parameter on a struct to avoid a clone. It infects every holder
  and usually ends in `Arc` anyway a week later; in a config-driven program,
  `SmallStr` is the cheaper answer (AGENTS.md, _Strings_).
- `Deref` to reach a field, or `AsRef`/`Into` chains that quietly allocate. A
  `&str` accessor feeding an `impl Into<SmallStr>` is the repo's known one.
- `&mut self` vs `&self` is API design, not spelling: `&self` plus interior
  mutability lets a value be shared and called from any task, and commits you
  to a lock forever. `Unit` chose that deliberately; a new type should say why.
- `Drop` used for anything that can fail or must await. `Drop` cannot await
  and must not block a runtime worker — `Process`'s drop `SIGKILL`s as a
  safety net precisely because the orderly path is an `async fn` the caller
  has to have called.

**Async and cancellation**

- **Cancel safety is a property of the call site, not of the function.** Any
  future in a `select!` branch that loses can be dropped mid-way. `Runner`
  documents the contract this repo settled on — dropped `run`, then
  `shutdown` on the same value — and a new `select!` arm has to answer the
  same question: what is half done if this one loses.
- `tokio::spawn` with the `JoinHandle` dropped. That is a decision that the
  task's lifetime is now untracked and it will outlive its reason to exist;
  a `CancellationToken` or a handle with a defined shutdown is the alternative.
- A `parking_lot` guard alive across an `.await` — usually a compile error,
  but a non-`Send` future or a block ending after the await point gets through.
  So does a `watch::Ref` (from `borrow()`) held across an await, which blocks
  every sender instead, and compiles.
- Picking the wrong Tokio primitive. `watch` = latest value, coalescing,
  lossy; `mpsc` = every message, with back pressure; `broadcast` = every
  message to everyone, lossy under lag; `Notify` = a wakeup with no payload.
  `EventDispatcher` carries no payload on purpose; "I need to know _what_
  changed" is a request for a different primitive, with all of its costs.
- `Notify` wakeup semantics: `notify_one` before a `notified()` stores a
  permit, `notify_waiters` does not — a gate built on the wrong one loses the
  wakeup exactly once, at startup, under load. `RunnerHandle`'s start gate
  depends on this.
- `async fn` that never awaits, or that only wraps a synchronous call. It
  makes every caller async for nothing, and the UI client rule (nothing async)
  exists because that spreads.
- Blocking work on the runtime: synchronous file IO, a long `parking_lot`
  section under contention, or CPU work between awaits. `spawn_blocking` or
  do it before the runtime starts.
- Holding a lock to decide, then awaiting, then acting on the decision. The
  state can have changed; either the lock must span it (it cannot) or the
  action must be idempotent or re-checked.

**Traits and generics**

- A trait with one implementer and no test double. `Runner` earns it —
  `Process`, `()`, `RunnerSerial` — most do not.
- `Box<dyn Trait>` on a hot path where the set of implementers is closed. The
  repo's answer is `enum_dispatch` (`UnitBehavior`), and it is written down as
  such; adding a `Box<dyn>` next to it needs a reason.
- Conversely, generics where the set is open or the type only flows through:
  a generic parameter monomorphises the whole chain and infects every holder's
  signature. `RunnerHandle` takes `impl Runner` and erases it into a task —
  that is the seam.
- RPITIT (`fn f(&self) -> impl Future + Send`) makes the trait not
  dyn-compatible. That is a real constraint on where the trait can be stored,
  and `Runner`'s `Send + 'static` supertrait is load bearing for
  `tokio::spawn`. Removing a bound to "make it more general" breaks the spawn.
- `impl Trait` in argument position where a caller will want a turbofish, or
  in return position in a public API where it pins you to the concrete type
  forever.
- Blanket impls and `From` chains in error types that make two different
  failures indistinguishable at the `?`.

**Types and errors**

- A type alias standing in for a newtype: it carries no invariant and no
  `impl`s, so the check it was supposed to enforce does not exist.
- An enum matched with a `_` arm: a new variant then compiles silently. On a
  state machine like `RunnerState`, that is how a state gets forgotten.
- `#[from]` collapsing distinct causes, or two variants with the same message
  — `UnitError::Invalid` and `UnitError::Runner` both render "unit not found"
  at the time of writing, which is exactly this failure.
- `anyhow` below the edges. The repo's shape is `thiserror` per layer and
  `anyhow` where a human reads it; an `anyhow::Result` in `unit` or `runner`
  throws away the match the caller needed.
- A `Result` whose `Err` no caller can act on, or an `Option` where the `None`
  has no defined meaning (AGENTS.md wants that meaning in the doc).
- `#[derive(Default)]` on a type with an invariant: it hands out a value
  nobody validated.
- `usize` where the value crosses a wire or a config, `as` casts that narrow,
  and `u32`/`u16` packing chosen for a saving nobody measured.
- Deriving `PartialEq`/`Hash` on a type holding a key _and_ a name: two values
  that mean the same unit then compare unequal.

Whether `unsafe` can be reached into UB is `/engineer:soundness`'s job, and
whether an optimisation pays is `/engineer:perf`'s — name the claim and hand
it off rather than redoing it.

## 5. Dumb-decision sweep

When the ask is "is there anything stupid in here", this is the pass. Look for
these, in the orchestration modules (`app`, `unit`, `runner`, `config`,
`util`):

- State that exists twice, or an invariant enforced in two places.
- A name where a key exists; a `String` where the module holds `SmallStr`, or
  the reverse (AGENTS.md, _Strings_).
- An `Arc` cycle, or a `Weak` standing in for an ownership tree — `Unit` ↔
  its handle manager (`Arc::new_cyclic`), `AppSchedule` ↔ `AppUnitMap`.
- A `Result` that no caller can act on, or an error type with one variant that
  is ever constructed.
- A check repeated below `App::new`, which already proved it.
- Configuration re-parsed, re-resolved or re-interned per read.
- A cache over something that should not be built in the first place
  (AGENTS.md, _The UI_).
- A `pub` surface wider than its callers need — especially in `log` and
  `runner`, where the narrow surface is what makes the invariants checkable.
- Something in `util` that knows about processes, or something in `base` that
  knows about units.
- Anything from §4 that is load bearing: an untracked task, a lock spanning a
  decision and an await, a trait with one implementer.

Rank by what it costs to fix later: a seam in the wrong place, then state that
can disagree, then a surface too wide, then a cost, then a name. **Three real
findings beat eleven with two real ones in the pile**, and "nothing here is
dumb, here is what I checked" is a complete answer — a sweep that has to
produce findings will invent them.

## 6. Out of scope

- **UI internals.** Panes, `State`, drawing, theming, key handling. If the
  answer is "that is a UI decision", say that in one line and stop. The
  exception is anything crossing `ViewClient`, and anything the UI needs from a
  session that the session cannot give.
- **Rewrites nobody asked for.** The answer to a question about one field is
  not a new module layout. If you think the layout is wrong, say so in two
  sentences and let the user decide whether to open it.
- **UB and cost audits.** `/engineer:soundness` checks `unsafe`;
  `/engineer:perf` measures what a change costs. Hand off rather than redo
  it; if a decision rests on one of those claims, say which and that it is
  unverified.

## 7. Claims

Anything about allocation, cost or contention gets measured or traced to the
bottom (AGENTS.md, _Claims_). A decision argued from a cost that turns out not
to exist is worse than no answer, because it gets built.

When you cannot settle it, say what would: a benchmark, a `cargo test
--release` run, a look at what the callers actually do. Label an argument as
an argument.

## 8. Reporting

**Recommendation first, in a paragraph** — the answer, not the reasoning that
led to it. Then what it costs. Then what would have to be true for the
alternative to win; that is usually the useful half.

Name the invariant a proposal touches, quoted from the module doc rather than
from memory, and say plainly when it breaks one that is documented as load
bearing. A snippet is fine when the snippet _is_ the answer. A branch is not.

Short. English, whatever language the conversation is in.
