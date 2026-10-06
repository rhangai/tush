---
description: Measure what code costs — allocations per frame or per poll, copies, locks — and whether an optimisation pays. The number first; no edits.
argument-hint: <path | item | hot path | diff | claim>
---

# Performance

Target: **$ARGUMENTS** — a hot path ("a frame of the log pane", "the poller
in `view/socket.rs`"), an item, a diff (`HEAD`, a branch), or a claim ("this
does not allocate", "`SmallStr` beats `String` here"). If it is empty, ask
which one and stop; do not pick.

The deliverable is a **number**, and what it means. Nothing under `src`
changes. If the fix is three lines, say the three lines and let the user ask
for them.

## 0. Name the target, in one sentence

"Fewer allocator calls in the poll loop", "the same buffer refilled instead of
rebuilt" and "fewer bytes on the wire" are three jobs with three answers
(AGENTS.md, _Altitude_). If the ask does not say which, say the one you read
it as and wait.

A request for a number gets the number. A second scenario, a breakdown per
call site, a baseline worktree — offer them in one line, do not produce them.

## 1. The budgets already set

Judge against these; they are settled.

- **A frame in steady state allocates zero times.** In a render path, a
  `Line`, `Span`, `Paragraph` or `format!` per frame is the finding — text
  goes into the buffer with `set_stringn` (AGENTS.md, _The UI_).
- **A cache over something that allocates is the symptom.** The fix is not
  building it.
- **Reuse, not sharing.** An allocation question is answered by the same
  buffer cleared and refilled. `Arc` removes a copy and leaves the
  allocation where it was; an answer that starts "wrap it in" has not
  answered (AGENTS.md, _Cleverness_).
- **`SmallStr` up to 23 bytes is inline**: no allocation, and a clone is a
  copy. Past that it is a refcount again. `String` stays where a buffer is
  refilled (AGENTS.md, _Strings_).
- **Watch the handoffs.** A `&str` accessor feeding an `impl Into<SmallStr>`
  re-allocates text that already exists as a `SmallStr`.

The paths these were paid for in, and so the ones to look at first: a frame
(`ui/render`), the poller (`view/socket.rs`), the server's answers
(`server/`), and the log write and read (`log/`).

## 2. Measure, do not read

Reading the source has been wrong here: one buffer "cleared and reserved
before each piece" was recommended as allocation-free and measured at two
allocations per poll. Trace to the bottom or measure; prefer measure.

**Allocations** — a counting allocator in a throwaway `#[test]`:

```rust
struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) } }
}
#[global_allocator]
static A: Counting = Counting;
```

Warm the path up first, then count over N iterations of the steady state.
The counter is global, so run with `--test-threads=1` and a filter. The
default `realloc` goes through `alloc`, so every growth is counted.

**Time** — `Instant` over many iterations in `cargo test --release`, labelled
as rough. There is no `criterion` in the tree; a timing difference under
noise is not a result.

Delete the probe before reporting and say that you did. Keeping it is a
separate ask (AGENTS.md, _Do not write tests while prototyping_).

**Before against after** is the same probe run on both. Two numbers.

## 3. Verdicts

- **Pays** — the number, before and after, and on which path.
- **Does not pay** — the number, and what the change cost to get it:
  a lock, a type, an `unsafe`, a wider API. Readability is a cost: a gain of a
  few percent that makes the code hard to read does not pay, and neither does
  any micro-optimisation off the hot path, whatever its number.
- **Unmeasured** — and what would measure it. Better than a guess.

A claim made from reading the code is labelled as an argument.

What this command hunts is waste — a buffer rebuilt per frame, a `String`
built and freed per log line — not the last percent. When the fix for a cost
is a different data structure rather than a tweak, say so and hand it to
`/engineer:arch`.

## 4. Reporting

The number first, in one line. Then what it means against §1. Then, if
asked, what would bring it down — one proposal, with what it costs.

Short. English, whatever language the conversation is in.
