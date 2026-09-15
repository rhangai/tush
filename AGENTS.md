# Working on tush

Notes for an agent working in this repository. They are the corrections that
came out of real sessions, so each one is here because getting it wrong cost
something.

## Scope

**Ask before writing anything that was not asked for. Every time.**

Not "write it and flag it in the report". By the time the report is written the
code exists, and the decision it was supposed to raise has already been taken.
Asking means stopping before the edit, saying what the addition is and why, and
waiting.

The trigger is easy to check: **if you find yourself drafting a paragraph that
starts "one thing I also changed" or "this I added without being asked" — you
have already failed.** That paragraph is the question you should have asked,
arriving too late to be answered.

Things that have gone in this way, none of them asked for, all of them removed
again afterwards: a mutex, a `LogSpan` newtype, a handle type for writing log
notes from a task, a reader with a capacity of its own, three modules' worth of
UI tests. Each one was defensible on its own terms. That is exactly why the
rule is "ask", and not "use judgement".

**Do what was asked, and stop there.** The most common failure in this repo is
not a bug, it is scope: answering a question with a redesign, inventing a type
to avoid writing two words twice, or "improving" the thing next to the thing
that was asked for.

**When the ask names the mechanism, use that mechanism.** "Write the line
straight into a chunk" is not an invitation to find a tidier route through the
existing code. If the named way looks wrong, say so and wait — do not build the
other one and explain afterwards.

**When the ask is a design question, answer it. Do not implement it.** "What do
you think?", "would X work?", "what do you suggest?" want a recommendation and
the trade-offs, not a branch. Implement when told to implement.

**Propose before touching a core module.** `log`, `runner` and `unit` carry
invariants that are documented at length. Changing one of them because it makes
a caller easier is how those invariants get quietly broken — say what you want
to change and why, first.

**Do not write tests while prototyping.** Especially UI tests. The UI is being
shaped, and a pile of tests over a shape that is about to change is work thrown
away twice. Tests belong on the data structures — `log`, `util`, `unit` — where
the arithmetic is subtle and the shape is settled. Ask before adding tests
anywhere else.

## Claims

**Measure before asserting.** "This does not allocate", "this is not a hole",
"this is cheap" — if it is worth writing down, it is worth checking. Claims
made from reading the code have been wrong in this repo more than once.

**Report what happened, not what should have happened.** If a test fails, say
so with the output. If a revert lost work, say which. If an earlier statement
turned out wrong, correct it plainly and move on.

## Style

**Comments and identifiers in English.** The conversation may be in Portuguese;
the code is not.

**Doc comments say why, not what.** The signature already says what. What a
reader cannot recover is the reasoning: what was tried, what it cost, what
breaks if it changes.

**Say it once, in as few lines as it takes.** A doc that goes round the houses
is worse than no doc: it is more tiring to read than the code it sits on, so it
gets skipped, and then the one sentence that mattered goes with it. `draw_unit`
had twenty eight lines of prose over a function that writes two lines of text —
that is the failure, and it is the common one.

Concretely: one sentence per decision. No `# Heading` sections on a function
unless it genuinely has two or three separate subtleties. Do not restate the
signature, do not narrate what the next three statements do, and do not defend
a small choice across three paragraphs. If a sentence is there for rhythm
rather than because a reader would get it wrong without it, cut it.

The heavy blocks in `log`, `runner` and `util` earned their length on
invariants that are genuinely hard. That is not a licence to write at that
length everywhere — most functions rate one line.

**Name the reason in the doc when a choice looks odd.** A `Copy` type that
holds four `usize` instead of two `Range`s, a reader that must be the same size
as its log, a default that returns `None` — each of those is a decision, and
the next person will undo it unless the reason is next to it.

**Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test` before
reporting.** Clippy clean, not clippy quiet.

## The UI

**Panes are widgets.** ratatui's `Widget` / `StatefulWidget` is the composition
seam, and it is a good one: a pane that takes an area and a buffer can be drawn
and read back on its own. One struct with a pile of private `draw_this`
methods reaching into its own fields composes with nothing.

**Whatever a pane has to remember between frames is its `State`.** The pane
itself is built and thrown away every frame; the cursor, the scroll and the
measured size are not.

**Draw into the buffer. Do not build text to hand to a widget that will.**
`Line` and `Span` allocate when built and again when rendered — several
hundred times a second to say what they said last frame. `Buffer::set_stringn`
says the same thing and allocates nothing. A frame in steady state must
allocate zero times.

**A cache that exists because building allocates is treating the symptom.** If
nothing is built, nothing needs caching, and the cache, the copy it held and
the staleness check it needed all go away with it.

## Strings

**`SmallStr` for text that is shared and never changed**: unit names, keys,
modes, config values. These are read out from behind locks several times a
second, and a `String` there is an allocation and a copy per read.

**`String` where the buffer is reused**: log lines, anything filled in place
and truncated. `SmallStr` is immutable, so it cannot be refilled — swapping it
in there makes things worse.

**Short is what the saving rests on.** Up to 23 bytes live inside the
`SmallStr`'s own 24: no allocation, and a clone is a copy rather than an
atomic. Names, keys and modes are all under that; past it the allocation and
the refcount are back.

**It was `ArcStr` and is not going back.** `ArcStr` is a bare pointer with no
inline form, so every word read out of a config file is its own allocation —
measured at three allocations for `[npm, run, build]` against zero, and a
slower clone, since a refcount per word costs more than one memcpy. Its win is
`literal!()` on compile-time text, which config values are not.

**Watch the handoffs.** A `&str` accessor feeding an `impl Into<SmallStr>`
allocates a fresh copy of text that already exists as a `SmallStr` two fields
away. That is the whole saving lost at one call site.

## The log

**`LogReader` is a tail follower, not a viewport.** Its `sync` is an offset
comparison — `pushed - seen` — and that is only sound because a reader holds
exactly as many chunks as the log does. A smaller reader breaks the guarantee
the module is documented around, and the argument that it still works is three
cases long and written down nowhere.

**To show a window, copy one out.** `LogReader::copy_region` takes a
`LogRegion` — lines counted back from the end, columns of each — and fills a
`Vec<String>` the caller keeps. Bounded in both directions, so it costs the
pane and not the log.

**One rule for where a line ends.** `LogReaderIter::ends_line` is it. A second
copy of that rule is a second chance for a window and a render to disagree.

## The UI client

**`UiClient` is what the screen sees a session through**, and one of its
implementations will be a socket. So:

- **Nothing is async and nothing returns a `Result`.** Reading is from a
  snapshot the client already holds; asking is `send`, fire and forget. A
  screen cannot be made to wait on a round trip.
- **The UI declares, the client satisfies.** `set_log(key, region)` says what
  the pane wants; whether anything has to happen is the client's decision,
  because it is the one holding the region, the revision and the connection.
- **Answers carry what they actually are.** `UiLog` reports its own region and
  revision, which can differ from what was asked. A pane draws the overlap
  rather than blanking while a client catches up.
