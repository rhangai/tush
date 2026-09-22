---
name: doc
description: Write or repair doc comments on a named Rust item, file or module in this repo, in the house style — why and not what, terse, English. Use when asked to document a type, trait, function, field, const or module, or when a doc has gone stale.
---

# Documenting

`/doc <target>` — a path (`src/log/chunk.rs`), a type or function name
(`LogReader`, `copy_region`), or a module (`log`). With no target, ask which
one and stop; do not pick.

The deliverable is doc comments on that target and the items inside it.
Nothing else in the file changes.

## What a doc is for here

The signature already says what the thing is. A doc earns its place by saying
what a reader cannot recover by reading: what was tried, what it cost, what
breaks if it changes. If none of those three has an answer, the item probably
does not need a doc.

## 1. Recover the why, before writing a line

In this order, and stop as soon as you have it:

- The item and what it touches — its fields, its callers (`grep -rn '<name>'
  src`), the module's `//!` if it has one.
- The history: `git log -S '<name>' --oneline -- src`, then `git show` the
  commit that introduced or last changed it. Commit messages in this repo
  carry the reason more often than the code does.
- `AGENTS.md` — the sections on strings, the log and the UI client are already
  the reason behind much of what you will meet. Cite that decision; do not
  re-derive it.
- Neighbours, for the shape: `src/util/str.rs`, `src/log/chunk.rs` and
  `src/ui/client.rs` are the style at full strength; `src/runner/state.rs` is
  it at the length most items rate.

## 2. What gets a doc

- **Module (`//!`)** — why the module is a seam: what belongs in it and what
  does not. Only when it is one; a two-type file rarely is.
- **Type** — why it exists at all and the invariant it carries. Not a tour of
  its fields.
- **Field or variant** — only when something is not plain: why it is separate
  from the field beside it, what `None` means, who may change it.
- **Method** — one line, in the caller's terms.
- **Const** — what breaks if the number changes, and what it must agree with.

Skip `new`, plain getters, `Display`, and trait impls that do the obvious
thing. A doc there is noise, and noise is what gets the ones that matter
skipped.

## 3. Read the block, not the item

A struct's fields, an enum's variants and a family of sibling methods are read
together, so they are judged together. Two failures come out of that and
neither is visible one item at a time:

**A gap reads as an omission.** `AppUnitMap` had six fields carrying two to
six lines each and `dependency_graph` and `groups` carrying none — obvious
enough not to need one, but next to the others the blank looks like unfinished
work rather than a decision. Same for a start method documented while the
three beside it are not. Give the obvious one a single line and the block
reads level.

**The outlier is the one to cut, not the others to grow.** If one field runs
six lines and its neighbours run one, the six is the mistake. Where the long
version is genuinely needed — a public method a caller reads — say it there
once, and leave the field a line that points at it.

The result to aim for is a block where every item has a doc and none of them
is more than about three lines. A field whose reason cannot fit that is
usually a reason about the type, and belongs in the type's doc.

## 4. The budget

One sentence is the default. Two or three when there is a trade-off to name.

`# Heading` sections only when the item genuinely holds two or three separate
subtleties — `log.rs`, `util/arena.rs` and `base/process.rs` have earned them;
almost nothing else has.

Never: restate the signature, narrate the body, list the parameters, open with
"This function…", add an `# Examples` block nobody asked for, or defend a
small choice across three paragraphs. If a sentence is there for rhythm rather
than because a reader would get it wrong without it, cut it.

Twenty-eight lines of prose over a function that writes two lines of text is
the failure this repo has actually had.

**Say the obvious thing once or not at all.** The common waste is not a wrong
sentence, it is a true one a reader already had: a second paragraph restating
the first in bolder words, a clause explaining why an atomic load is fast, a
"which is to say" that says it again. On a rewrite pass, go looking for those
first — a doc that shrank and lost nothing was prolix, and most of them are.

## 5. Name the reason when the choice looks odd

If the item is one of these, the reason *is* the doc:

- a newtype over a crate's type;
- a `Copy` type holding loose parts instead of the obvious aggregate;
- a `None` that does not mean "use the default";
- something that must be the same size or count as something else;
- a method deliberately not `async`, or returning no `Result`;
- a `String` where the module otherwise holds `SmallStr`, or the reverse.

Each of those gets undone by the next reader unless the reason sits next to
it.

## 6. Claims

Do not write "does not allocate", "cheap", "O(1)" or "no copy" from reading
the code. Follow the call to the bottom, or measure it. If you cannot, write
what you do know — "a copy of the region the pane asked for" — or leave it
out. A wrong claim in a doc outlives the session.

A doc that was true and is not any more is a bug: at the time of writing,
`RunnerState::is_started` and `is_finished` carry the same sentence. Repair
stale docs on the target you were given; report the ones you notice elsewhere
and leave them alone.

Link the first mention of a type or method a reader would go and look at —
``[`SmolStr`]``, ``[`sync`](UiClient::sync)``, ``[`Hash`](std::hash::Hash)`` —
not every mention.

## 7. When the why is not recoverable

Do not invent one. An invented reason is worse than no doc: the next reader
believes it and designs around it.

Leave that item alone, finish the pass, and list it at the end — what you
could not tell, and your best guess, so the answer can be one word. No
stopping to ask mid-pass, and nothing waits on the answer.

## 8. What this pass does not do

Writing docs is not an architectural decision: write them, do not ask first.
The guard is elsewhere — **the code itself does not change.** Not a rename,
not a reorder, not an extracted helper, not an `#[allow]`. If the code is
wrong, say so in the report and leave it.

No new tests, no new types, nothing "while I was here". Comments and
identifiers in English, whatever language the conversation is in.

## 9. Before reporting

```
cargo fmt
cargo clippy --all-targets      # clean, not quiet
cargo test
cargo doc --no-deps             # broken intra-doc links; this style invites them
```

Then report short: what was documented, what was left undocumented for want of
a reason, and anything found broken and left alone. Do not paste the docs back
— they are in the diff.
