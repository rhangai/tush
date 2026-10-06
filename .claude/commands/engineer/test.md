---
description: Write tests that catch bugs, not tests that restate the code — proposed with the bug each one catches, approved, written, then proved able to fail with cargo-mutants.
argument-hint: <path | item | module>
---

# Test

Target: **$ARGUMENTS** — a path (`src/util/localring.rs`), an item
(`LogReader::sync`), or a module (`util`). If it is empty, ask which one and
stop; do not pick.

You do not write these tests yourself. The `tester` agent does, from a fresh
context, because the context that wrote the code tests what it wrote. Your
job is to run the two phases and to carry the user's answers between them.

## 0. Before starting

Say it in one line and wait, if either holds:

- The target is UI (drawing, panes, layout, keys). AGENTS.md: no UI tests
  unless the user asks for them by name.
- The target is still being shaped (a rewrite on this branch, a module
  touched in most recent commits). Tests over a moving design are thrown away
  twice; the user may want to wait.

## 1. Propose

Spawn `tester` with: the target, `phase: propose`, and nothing from this
conversation — no summary of the code, no explanation of how it works, no
list of what to test. Its value is that it does not know.

Show the user its proposal as it came back. Do not trim it, rank it or add to
it.

## 2. Approve

Wait. The user approves, removes, adds or edits rows, and answers each item
under "Proposed, outside the tests". Nothing is written until then.

## 3. Write

Send the approved list, exactly as approved, to the same agent (SendMessage,
so it keeps what it read) with `phase: write`. If it cannot be continued,
spawn a new `tester` with the target, `phase: write` and the list.

## 4. Report

Relay its report. If it lists missed mutants with a test that would kill
them, that is a new proposal: back to step 2 with those rows only. A test
left `#[ignore]` is a bug it found; say so first, and leave the fix to the
user (AGENTS.md, _A finding is not a mandate_).
