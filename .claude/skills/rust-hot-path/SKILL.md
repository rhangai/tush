---
name: rust-hot-path
description: How to write Rust that runs every frame, every poll or every log line in this repo without wasting allocations or waiting — doing nothing when nothing changed, keeping locks short, refilling and swapping buffers, indexing by dense keys. Use when writing or changing code on such a path.
paths:
    [
        "src/ui/**",
        "src/view/**",
        "src/server/**",
        "src/log/**",
        "src/log2/**",
        "src/util/**",
    ]
---

# Hot paths

A frame or a poll in steady state allocates **zero** times. The patterns below
are how to get there.

**The target is not wasteful, not as fast as possible.** What this skill exists
to stop is the dumb cost: a `Vec` allocated every frame instead of refilled, a
`String` built for each log line and freed instead of taken from a pool. A
readable version that gets 98% of the way beats an unreadable one that gets
100%. Off the hot path none of this applies — write the plain version. Each stands on its own reasoning; where the code already
does it, that is cited as an example, not as a template — the code around it
may still be the easy shape rather than the good one (`rust-maintainable`).

Whether a change actually pays is measured, not read (AGENTS.md, _Claims_) —
that is `/engineer:perf`. This skill is for writing it right the first time.

## Settled here

AGENTS.md already settles, and is always in context: reuse rather than share
(_Cleverness_), no cache over something that should not be built and text
straight into the buffer (_The UI_), `SmallStr` against `String` and the
handoffs between them (_Strings_). Not repeated here. One addition:

- **No `Arc` clone per iteration.** Building and sharing an `Arc` at setup
  is fine; cloning or dropping one per frame, poll or line is an atomic each
  time, and a contended one when another thread holds the same `Arc`. Borrow
  it, or hold a clone made once.

## The patterns

**The cheapest frame does nothing.** Most frames and polls find nothing
changed, so the first thing a hot path does is find that out with one
comparison, before any lock or copy: `LogReader` compares one atomic version
and returns, `ViewApp` skips the copy when the revision and the region both
match, `ViewSocket::sync` swaps only a buffer flagged fresh. A path that does
its work and then discovers nothing changed has paid for a frame of output
nobody needed.

**Fill outside the lock, swap inside it.** A lock held while a buffer is
filled makes every other side wait for the fill. Fill your own buffer, take
the lock to swap it in, drop the guard: `view/socket.rs`'s task decodes into
its own scratch and trades it in under the lock.

**Do it once when building, not on every wake.** What can be answered at
build time is answered there and kept: names become `AppUnitKey`s once, in
`AppUnitMap`'s interner, and each entry keeps its `dependencies()` so the
schedule's loop reads a list instead of resolving the graph again.

**Refill, do not rebuild.** `clear()`, `truncate()` and `drain(..)` keep the
capacity; `= Vec::new()` throws it away. When a function produces a
collection, take `out: &mut` from a caller that keeps it, and clear it first.
Here: `UnitBehavior::choices(state, out)`, `open_line` in `log/region.rs`
(clears the `String` already at that index instead of pushing a new one),
`LogReader::copy_region`.

**Trade full buffers, do not copy them.** When one side fills and the other
reads, `mem::swap` the two and both keep their capacity: `ViewSocket::sync`
trades with its task in at most two swaps, and none when nothing changed.
`mem::take` lends a buffer by value — take it, fill it, hand it back — when a
`&mut` cannot cross the call (`take_items` in `ui/render/menu.rs`). It leaves
an empty `Default` behind, so a buffer taken and never returned takes its
capacity with it.

**Fill into, not over.** `a = b.clone()` drops `a`'s allocation and makes a
new one; `a.clone_from(&b)` reuses it. The same for decoding: serde's
`deserialize_in_place` refills the value it is given, and `Cargo.toml` keeps
`serde_derive` only for that feature — 317 allocations per poll without it,
143 with.

**Write into a buffer, not a `format!`.** `format!` allocates a `String`
every call. Use `write!` into a `String` that is kept and cleared. On screen,
not even that: text goes straight into the frame with `Buffer::set_stringn`
(AGENTS.md, _The UI_).

**A piece still out stops the buffer being reused.** A `BytesMut` cannot
reclaim its allocation while a `Bytes` cut from it is alive, which is how a
buffer "cleared before each piece" measured at two allocations per poll.
A small pool sized one more than the pieces alive at once fixes it
(`util/bytes.rs`).

**Do not collect to iterate.** A `collect()` that is only iterated again is an
allocation for nothing; chain the iterators, or `extend` into a buffer that is
kept. `Iterator::collect_into` is nightly-only and not available here.

**A dense key indexes a `Vec`.** A key that counts up from zero and never has
holes is an index: a `Vec` looked up by it does no hashing, keeps the values
side by side, and reads as plainly as the `HashMap`. `AppUnitKey` is one — an
interned `u32` (`to_usize()`), and the set of units is fixed once `App` is
built. Two conditions: the key space stays dense (nothing is removed and
re-added under new keys), and a key that came from outside — over the socket,
from a command — is looked up with `get`, never `[]`, since it may not be one
this session made.

**Keep small collections inline**, where the plain `Vec` would allocate on
every frame or poll. `smallvec` is already a dependency:
`UnitChoices` (`SmallVec<[UnitChoice; 4]>`), `UnitKeyVec` (16), `SmallVecStr`
(8). Pick `N` from the real maximum and say where it comes from in the doc;
past `N` it is a heap `Vec` again, plus a branch.

**Watch the size of what is copied around.** An enum is as big as its largest
variant, so one large variant makes every value pay — `Box` that variant when
the values are copied on a hot path; elsewhere, leave it.

**Pre-size when the size is known.** `with_capacity` once, at build time, for
a buffer whose size is set by config or the screen; then refill it. Not a
guess per call.

## When a pattern is not enough

If keeping a path cheap takes something clever — packed fields, manual
indexing, a hand-rolled structure — the data structure is usually the wrong
one. Say so and suggest the alternative for `/engineer:arch` (a ring instead
of a growing `Vec`, one pool instead of many small buffers, an index instead
of a search) rather than micro-optimising around it.

## Not without asking

Common advice for hot paths that this repo does not take by default; each
one needs permission, and a number from `/engineer:perf` to ask with:

- `Arc`/`Rc` for shared ownership to avoid a clone (_Cleverness_).
- `#[inline]`, `#[cold]`, release-profile, LTO, PGO and `target-cpu` tuning.
- A new dependency for speed: `ahash`/`FxHashMap`, `ThinVec`, `ArrayVec`,
  compact string crates. `SmallStr` and `smallvec` are the choices made.
- `Box<[T]>` for a buffer: it has no spare capacity, so it cannot be refilled.
