---
name: doc
description: Write or repair the docs of this repo — doc comments on a named Rust item, file or module, or a Markdown file like README.md or CONFIG.md — in the house style: why and not what, terse, English. Use when asked to document a type, trait, function, field, const or module, to write or improve one of the Markdown docs, or when a doc has gone stale.
---

# Documenting

`/doc <target>` — a path (`src/log/chunk.rs`, `README.md`), a type or function
name (`LogReader`, `copy_region`), or a module (`log`). With no target, ask
which one and stop; do not pick.

The deliverable is the docs on that target and nothing else. Code never
changes — see §8.

## Who is reading

Two kinds of doc live here, and they are written for two different people.

**Development docs** — doc comments, `ARCHITECTURE.md`, `AGENTS.md`. The
reader is about to change the code. Everything below is about these.

**User docs** — `README.md`, `CONFIG.md`. The reader downloaded a binary and
wants it running; they will never open `src`. Three rules carry over — find
the answer in the code and the history rather than guessing (§1), check every
claim (§6), change no code (§8) — and §U replaces the rest.

## U. User docs

- **Write for someone running the thing, not building it.** What it does, how
  to install it, how to run it, what the screen means, every flag and key.
  Contributor material — building from a checkout, the crate's layout, the
  house rules — stays out: it lives in `ARCHITECTURE.md` and `AGENTS.md`, and
  a user doc that hands it to the reader reads as an invitation to send
  patches. This repo does not want one.
- **Say why only where it changes what the reader does.** "A `Copy` type
  holding loose parts" is for the code; a user wants the consequence — what it
  costs them, what they have to do about it. A design justification nobody
  asked for is the first thing to cut.
- **Sound like a person.** Short sentences, second person, contractions where
  they fall naturally. Read a paragraph aloud: if nobody would say it, rewrite
  it.
- **Show, then explain.** A config block and a command line carry more than a
  paragraph about them. Cut the paragraph, not the block.
- **Terse is about prose, not information.** The tables of keys, flags and
  defaults stay complete; it is the sentences around them that shrink.
- **Everything still has to be true.** A README says what the binary does
  today: check flags against `--help`, keys and defaults against the source,
  and paths against the code that builds them. §6 is not relaxed because the
  reader is not a developer.

## What a code doc is for here

The signature already says what the thing is. A doc earns its place by saying
what a reader cannot recover by reading: what was tried, what it cost, what
breaks if it changes. If none of those three has an answer, the item probably
does not need a doc.

## 0. The observable comes first

A reader arrives to *use* the thing. They read about its insides only if they
are changing it, and most of them never are. So the order is fixed:

1. **What it is for**, in one line, in the caller's words.
2. **How the pieces are used together** — who holds which half, what each one
   calls. Where a module has two halves used against each other, a short
   worked example is shorter and truer than the paragraph it replaces.
3. **The behaviour a caller cannot read off the signatures** — what coalesces,
   what blocks, what a sentinel return means.
4. **`# Implementation`**, with everything internal under it.

That heading is the whole trick. Orderings, atomics, why a field exists, which
line has to stay above which — all of it is worth writing and none of it
belongs above the fold. Under the heading, a reader deciding how to call the
thing knows where to stop; above it, they have to read the lot to find out
none of it was for them.

**History only with the number or the constraint attached.** "It was a
`watch` of `()` before" on its own is not documentation: nobody re-decides
that from a doc — they do the cost analysis at the time — and until someone
does, it is a paragraph every reader pays for. What earns its place is the
result that stops the work being redone: `log.rs` pricing the queue it
replaced at 1.8 to 3.2 times slower, `arena.rs` pricing a `Weak` upgrade at
35x on one thread and 731x on eight, `config.rs` naming the `serde_with`
adapters that cannot build a `JaggedVec`. That is the "what was tried, what it
cost" this repo runs on. A bare before-and-after carrying neither is not.

**Do not document what the thing is not.** `util/event.rs` opened with a
paragraph on why the signal carries no payload and must not become a channel.
Naming the wrong frame at the top is how the reader ends up in it: they arrive
looking for a channel and read the rest through that. If a misuse genuinely
has to be guarded, guard it where the change would be made, not in the
module's first paragraph.

That module is the failure this repo has actually had, twice in one pass — a
`//!` rewritten once and still opening with internals, then rewritten again
and still leading with a payload the type never had.

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

- **Module (`//!`)** — what it is for and how its pieces are used together,
  in the order §0 fixes, and then the seam. Only when it is one; a two-type
  file rarely is.
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
almost nothing else has. `# Implementation` is the exception and is not
rationed: anything internal earns it, however short (§0).

Never: restate the signature, narrate the body, list the parameters, open with
"This function…", add an example to an item whose one use is obvious — §0
wants one where two halves are used against each other, and nowhere else — or
defend a small choice across three paragraphs. If a sentence is there for rhythm rather
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

A Markdown pass changes no code, so these are not what it is checked by:
verify the claims instead — `--help` for the flags, the constants for the
defaults, the source for the keys — and check the links resolve.

Then report short: what was documented, what was left undocumented for want of
a reason, and anything found broken and left alone. Do not paste the docs back
— they are in the diff.
