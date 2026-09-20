---
name: soundness
description: Audit a Rust item, module or diff in this repo for correctness, soundness and whether an optimisation actually pays — or answer an architectural question about one, with the invariants it touches. Use when asked whether something is sound, correct, UB-free, racy, actually cheaper, or "would X work here".
---

# Soundness

`/soundness <target>` — a path (`src/util/vec.rs`), an item (`Arena::alloc`,
`LogReader::sync`), a module (`log`), a diff (`HEAD`, a branch), or a question
("would a smaller reader work?", "is `Relaxed` enough on `pushed_hint`?").
With no target, ask which one and stop; do not pick.

The deliverable is an **answer**. Nothing under `src` changes — not a fix, not
an `#[allow]`, not a test. A design question gets answered, not implemented
(AGENTS.md, *Scope*); if the fix is one line, say the line and let the user
ask for it.

## Two modes

- **Audit** — the target names code. Every claim gets a verdict, evidence,
  and a failure scenario when the verdict is "broken".
- **Consult** — the target is a question. Recommendation, what it costs, what
  would have to be true for the alternative. Still no branch.

## 0. The three axes

This repo wants all three, and they fail differently:

- **Correctness** — does it do what its doc says, for every input reachable
  from safe code.
- **Soundness** — can safe code, using only the public surface, reach UB. A
  private function with a precondition is not unsound; a `pub` one that can
  be called into UB is, whatever its doc says.
- **Cost** — does the optimisation pay. Half this codebase exists to avoid an
  allocation or a lock; an arena that is slower than `Box` is a bug here even
  though it computes the right answer.

## 1. Read before judging

In this order, stopping as soon as the answer is in hand:

- The item's doc and the module's `//!`. `log`, `runner`, `unit`, `util` state
  their invariants at length — an audit that contradicts one must say *which*
  one it breaks, not re-derive it.
- `AGENTS.md` — strings, the log, the UI client. Those decisions are settled.
- History: `git log -S '<name>' --oneline -- src`, then `git show`. The reason
  is in the commit more often than in the code.
- The callers: `grep -rn '<name>' src`. Soundness is a property of the whole
  set of call sites, never of the function alone.

`README.md` has gone stale at least once (it describes `RunnerState` as a
packed atomic with a CAS loop; it is a `tokio::sync::watch` and a `&mut self`
state machine). Trust the source over the prose.

## 2. Where the sharp edges actually are

The `unsafe` lives in `util/vec.rs`, `util/arena.rs`, `log/buffer.rs`,
`log/chunk.rs`, `log/line.rs` and `base/process.rs`. Run the checklist that
applies; do not run all of it everywhere.

**Unsafe and aliasing**

- A `SAFETY` comment must name the invariant and who upholds it. One that
  restates the operation ("the pointer is valid") is not a `SAFETY` comment.
- `from_utf8_unchecked` (`LogBufferLine::as_str`, `LogChunk::get_data`,
  `LogChunk::get_str`) is sound only while *every* path that writes those
  bytes writes whole UTF-8. The entry point is rarely the bug: check the
  truncation and the carry across a read. A cut inside a multi-byte character
  is UB, not a mojibake.
- `unsafe impl Send`/`Sync` (`ArenaBlock`, `ArenaInner`, `StorageHeap`) is a
  promise about the fields *as they are today*. A new field the compiler would
  have rejected passes silently. When the target adds one, re-derive the
  promise from scratch.
- Raw pointers in `util/vec.rs`: provenance (every pointer derived from the
  one allocation it addresses), `add` to one-past-end is allowed and
  dereferencing it is not, `assume_init_ref` only over the prefix actually
  written, the `Layout` at `dealloc` identical to the one at `alloc`, and the
  `ends` array non-decreasing and bounded by `data_len`.
- `ManuallyDrop::take` exactly once, and nothing touches the field after.
- Exception safety: if `T::drop` or a caller's closure panics mid-move, what
  is left — a leak or a double drop? A leak is acceptable. A double drop is
  not. `replace_with_or_abort` is the repo's answer where it is used; check it
  is used where it is needed.
- `Arena`: the invariant is *two handles never overlap*. Any new route to a
  block has to preserve it, and blocks are never returned — a caller that
  allocates in a loop drains the arena, which is a design error and not a
  soundness one.

**Concurrency**

- Name the pair. Every `Acquire` must have the `Release` it reads, and the
  answer to "what does this publish" must be a specific write. `LogReader::sync`
  is the pattern: `version` Acquire against the writer's `fetch_add(Release)`,
  `pushed_hint` allowed to be stale, the truth read under the `history` lock.
  A proposal to weaken an ordering has to say which pair it keeps.
- CAS loops (`ArenaInner::claim`): the failed branch must re-read, and the counter
  must be monotonic — the arena's offset is, which is what makes ABA a
  non-question there.
- Lock order. `Unit` holds `behavior` and `handle_manager`; `Schedule` holds
  `scheduled`. Two of them taken in one path in two orders is a deadlock, and
  `parking_lot` is not reentrant: re-locking on the same thread hangs, it does
  not panic.
- A `parking_lot` guard alive across an `.await` is the bug to look for in
  `runner` and `app` — it is not `Send`, so it usually fails to compile, but a
  guard in a non-`Send` future or a block that ends after the await point is
  how it gets through.
- `Weak` upgrades (`LogWriterRef`, `UnitHandle::manager_weak`) are `Weak` on
  purpose. Every one needs a defined behaviour when the upgrade fails, not an
  `unwrap`.
- Cancellation: a Tokio task can be dropped at any `.await`. Ask what is half
  updated at each one — a chunk handed out but not pushed back, a state never
  moved to terminal, a child never reaped.
- `base/process.rs` kills a process *group* by `kill(-pid, ...)`. After the
  child is reaped the pid can be reused, and the signal then goes to somebody
  else's group. The audit is the ordering of wait against kill, not the
  `unsafe` block.

**Arithmetic and encoding**

- Ring offsets: `pushed - seen` is sound only while a reader holds as many
  chunks as the log does (AGENTS.md, *The log*). Check the clamp when a reader
  is further behind than the ring is long, and that the `u64` counters cannot
  wrap into a subtraction underflow.
- Columns are not bytes are not chars. `unicode_width` gives display width;
  slicing by it is wrong unless the code maps back to a byte boundary.
- Narrowing casts (`as u32`, `as u16`, `as usize`) in `util/vec.rs`: what
  happens at the boundary, and is the boundary reachable from a config file.
- One rule for where a line ends: `LogReaderIter::ends_line`. A second copy is
  a second chance to disagree.

**Cost**

- "Does not allocate", "one atomic", "cheap" — measured or traced to the
  bottom, never read off the page (AGENTS.md, *Claims*). If neither is
  possible, say what is known instead.
- A steady state frame allocates zero times. In a render path, a `Line` or a
  `Span` built per frame is the finding.
- Watch the handoffs: a `&str` accessor feeding an `impl Into<SmallStr>`
  re-allocates text that already exists as a `SmallStr`.

## 3. What each tool settles

```
cargo clippy --all-targets      # clean, not quiet
cargo test
cargo miri test <filter>        # miri is installed; slow, so filter
MIRIFLAGS=-Zmiri-strict-provenance cargo miri test <filter>
cargo test --release            # different codegen; UB shows differently
```

Miri is the only thing here that *settles* UB in `util::vec` and
`util::arena`, and it settles it only for the paths the tests walk — say which
paths it did not reach. There is no `loom` in the tree, so a claim about an
ordering is an argument with the pairs named, not a result; label it as such.

Report what the tool said. "Miri clean over `util::vec::tests`" is a fact;
"the pointer arithmetic looks fine" is not.

## 4. Verdicts

- **Sound** — with the invariant it rests on and the place that upholds it.
- **Broken** — with a scenario: concrete input or interleaving, then the wrong
  value, the UB or the hang. A finding with no scenario is a guess; drop it or
  demote it.
- **Unverified** — and what would settle it. Use this one; it is often the
  honest answer for concurrency.

Rank by consequence: UB, then silently wrong output, then panic or hang, then
a cost claim that does not hold, then style. Three real findings beat eleven
with two real ones in the pile.

## 5. Consulting

Recommendation first, in a paragraph. Then what it costs. Then what would have
to be true for the alternative to win — that is usually the useful half.

Name the invariant the proposal touches, quoted from the module doc rather
than from memory, and say plainly when the proposal breaks one that is
documented as load bearing. A snippet in the reply is fine when the snippet
*is* the answer. An edit is not.

## 6. Reporting

Per finding: `file:line`, what breaks, the scenario, the verdict, what settled
it. Then one line each for what was checked and found sound — an audit that
lists only problems does not tell the reader what was covered. Then what was
not covered, and why.

Short. English, whatever language the conversation is in.
