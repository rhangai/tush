---
description: Audit unsafe code for undefined behaviour — can safe code, through the public surface, reach UB. Answer only, no edits.
argument-hint: <path | item | module | diff>
effort: high
context: fork
agent: auditor
background: false
---

# Soundness

Target: **$ARGUMENTS** — a path (`src/util/jagged_vec.rs`), an item
(`Arena::alloc`, `SmallStr::as_str`), a module (`util`), or a diff (`HEAD`, a
branch). If it is empty, ask which one and stop; do not pick.

Soundness comes before readability and speed in this repo: nothing is worth
UB.

The question is one: **can safe code, using only the public surface, reach
UB?** A private function with a precondition is not unsound; a `pub` one that
can be called into UB is, whatever its doc says. Wrong output, a panic, a
deadlock or a cost that does not pay are not UB — name them in one line if
you trip over one, and leave design to `/engineer:arch` and cost to
`/engineer:perf` (AGENTS.md, _A finding is not a mandate_).

The deliverable is an **answer**. Nothing under `src` changes — not a fix, not
an `#[allow]`, not a test. If the fix is one line, say the line.

## 1. Read before judging

In this order, stopping as soon as the answer is in hand:

- Every `unsafe` block in the target, its `SAFETY` comment, and the item and
  module docs that state the invariant it rests on. An audit that contradicts
  one must say _which_ one it breaks.
- The callers: `grep -rn '<name>' src`. Soundness is a property of the whole
  set of call sites and of every path that writes what the `unsafe` reads,
  never of the block alone.
- History: `git log -S '<name>' --oneline -- src`, then `git show`.

## 2. Where the `unsafe` is

`util/jagged_vec.rs`, `util/arena.rs`, `util/arena2.rs`, `util/chunk.rs`,
`util/str.rs`, `log/buffer.rs`, `log/chunk.rs`, `log/line.rs`,
`base/process.rs`. Re-run `grep -rlw unsafe src` — this list goes stale. Run
the checklist that applies; not all of it everywhere.

**Every block**

- A `SAFETY` comment names the invariant and who upholds it. One that restates
  the operation ("the pointer is valid") is not a `SAFETY` comment.
- The invariant has to hold on _every_ path into the block, including the
  ones a later commit added.

**UTF-8**

- `from_utf8_unchecked` (`SmallStr::as_str`, `util/chunk.rs`,
  `LogBufferLine::as_str`, `LogChunk::get_data`, `LogChunk::get_str`) is sound
  only while every path that writes those bytes writes whole UTF-8. The entry
  point is rarely the bug: check truncation, the carry across a read, and any
  cut made by display width rather than a char boundary. A cut inside a
  multi-byte character is UB, not mojibake.

**Pointers and initialisation**

- Provenance: every pointer derived from the one allocation it addresses.
  `add` to one-past-end is allowed; dereferencing it is not.
- **Aliasing.** A `&mut` to the whole of a buffer, made while a raw pointer
  into part of it is still in use, invalidates that pointer (Stacked and Tree
  Borrows alike): derive every pointer from one raw base and stay raw until
  the access, rather than going back through `&mut self.buf` in between. A
  `&` and a write through a raw pointer to the same bytes at once is UB too.
- `assume_init_ref` only over the prefix actually written; `Vec::set_len`
  only after the items up to the new length are initialised.
- `ptr::copy` when the ranges may overlap, `copy_nonoverlapping` only when
  they cannot.
- The `Layout` at `dealloc` identical to the one at `alloc`, and a zero-sized
  `Layout` never reaching either: `jagged_vec.rs` hands back a dangling
  pointer for it on both sides, and a change there has to keep that.
- In an `unsafe fn`, each unsafe operation still sits in its own `unsafe {}`
  with its own `SAFETY` (edition 2024 warns on `unsafe_op_in_unsafe_fn`).
- Arithmetic that feeds an unchecked access is part of the `unsafe`: in
  `util/jagged_vec.rs`, the row ends non-decreasing and bounded by
  `data_len`, and every narrowing cast (`as u32`, `as u16`) checked at the
  boundary — and whether that boundary is reachable from a config file.

**Drops and panics**

- `ManuallyDrop::take` exactly once, and nothing touches the field after.
- If `T::drop` or a caller's closure panics mid-move, what is left? A leak is
  acceptable; a double drop or a read of moved-from memory is not.
  `replace_with_or_abort` is the repo's answer; check it is used wherever a
  panic could land between the move out and the move back.

**Threads**

- `unsafe impl Send`/`Sync` (`ArenaBlock`, `ArenaInner`, `StorageHeap`) is a
  promise about the fields _as they are today_. A new field the compiler would
  have rejected passes silently; when the target adds one, re-derive the
  promise from scratch.
- `Arena`: the invariant is _two handles never overlap_. Every route to a
  block has to preserve it. `ArenaInner::claim`'s CAS must re-read on failure,
  and the offset must be monotonic — that is what makes ABA a non-question.
- An atomic that publishes data later read through `unsafe` needs its pair:
  every `Acquire` names the `Release` it reads and the write that `Release`
  publishes. `Relaxed` there is a data race, and a data race is UB.

## 3. What each tool settles

```
cargo test
cargo test --release            # different codegen; UB shows differently

# Miri is not on the default toolchain: it needs a nightly with the component.
cargo +nightly miri test <filter>
MIRIFLAGS=-Zmiri-strict-provenance cargo +nightly miri test <filter>
MIRIFLAGS=-Zmiri-tree-borrows      cargo +nightly miri test <filter>
```

If the component is missing, `rustup +nightly component add miri` brings it;
do not install it yourself, say so. Miri builds into `target/miri`, apart from the normal build. Filter
it to the module under audit (`util::jagged_vec`, `util::arena`): it is slow.

Miri is the only thing that _settles_ UB, and only for the paths the tests
walk — say which paths it did not reach. Run both borrow models when the
target hands out pointers into a buffer: Stacked Borrows is stricter, Tree
Borrows is what Rust is moving to, and a disagreement between them is worth
reporting. If Miri cannot be run, say why and the verdict is
**Unverified**. There is no `loom`, so an ordering claim is an argument with
the pairs named, not a result.

Report what the tool said. "Miri clean over `util::jagged_vec::tests`" is a
fact; "the pointer arithmetic looks fine" is not.

## 4. Verdicts

- **Sound** — with the invariant it rests on and the place that upholds it.
- **Unsound** — with the scenario: the safe calls, in order, that reach the
  UB, and which UB it is. A finding with no scenario is a guess; drop it or
  demote it.
- **Unverified** — and what would settle it. Often the honest answer.

Three real findings beat eleven with two real ones in the pile.

## 5. Reporting

Per finding: `file:line`, the scenario, the verdict, what settled it. Then
one line per `unsafe` block checked and found sound — an audit that lists only
problems does not say what it covered. Then what was not covered, and why.

Short. English, whatever language the conversation is in.
