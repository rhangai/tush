---
description: Implement the next Pending sprint of plan.md, check its "Done when" with you, and mark it Done
argument-hint: "[slug] [sprint]"
---

# /feature:work

Works on the feature sprint by sprint, following
`tmp/sessions/<slug>/plan.md`: implements, checks the sprint's "Done when",
and marks it `Done` once the user confirms.

<arguments>
$ARGUMENTS
</arguments>

## Find the session and the sprint

- An argument with a slug → `tmp/sessions/<slug>/`. No argument: look in
  `tmp/sessions/*/plan.md` for one whose `Branch:` is the current branch.
- No `plan.md`: suggest `/feature:plan`.
- The sprint is the first with `**Status:** Pending`, unless the user names
  another.
- Say in one line which sprint you are taking and where it stands: no code
  yet, or implemented and waiting for the checks.
- A sprint that changes a core module or widens `ViewClient` needs the user's
  go-ahead in this conversation before any edit. The plan saying so is not
  the go-ahead.

## 1. Implement

1. Re-read the sprint in `plan.md`, `requirements.md` (or the `## Brief`),
   `AGENTS.md`, and the `//!` of every module you will touch.
   `rust-maintainable` and `rust-hot-path` apply to the code you write.
   Drawing on screen follows `.claude/commands/ui.md`; docs follow
   `.claude/commands/doc.md` — read the one that applies.
2. Implement **what the sprint says, and stop there** (AGENTS.md, _Scope_).
   The tests named in "Done when" are part of the sprint, but you do not
   write them: step 5 does. Anything else you would add — a helper type, a
   test elsewhere, a fix next door — is a question first, not a paragraph in
   the summary.
3. An item is wrong, impossible or ambiguous? **Stop and ask**, quoting the
   item's text and saying what you found. Do not reinterpret it silently. If
   the user decides to change it, edit the sprint and record it under
   `## Decisions`.
4. Code that diverges from the skills, found on the way: add it to
   `## Found, not fixed` in `plan.md` — `file:line`, what is wrong, which
   sprint — and leave it (AGENTS.md, _A finding is not a mandate_).
5. With the code in place, hand the sprint's `test:` items to `tester`: the
   context that wrote the code tests what it wrote. The plan already
   approved the list, so it goes straight to the write phase, with nothing
   from this conversation:

    ```
    Agent(
      subagent_type="tester",
      prompt="phase: write. Target: <files the sprint touched>. Approved
              list, from Sprint <N> of tmp/sessions/<slug>/plan.md:
              <each test: item, verbatim, with the bug it names>.
              <Run | Skip> the cargo-mutants proof."
    )
    ```

    Mutation testing can take as long as the sprint, so ask in one line
    before spawning whether to run it. A test it left `#[ignore]` is a bug in
    the sprint's code: fix the code, not the test.

## 2. Check

1. `cargo fmt`, `cargo clippy --all-targets` (clean, not quiet), `cargo test`.
2. Each `test:` item: run it by name and confirm it tests what the item says,
   not something near it. Each `command:` item: run it and compare.
3. Each `screen:` item: you cannot see the screen. Say what you expect it to
   show from reading the code, and which sizes from `ui.md` §5 you reasoned
   about; then give the user the steps and ask.
4. Show the result **quoting each item's text**, in three groups:
    - checked (the evidence in one line)
    - failed (what failed and the likely cause)
    - for the user: the `screen:` items, with the steps and what you expect
      Then the files touched (path + one line) and anything added to `## Found,
not fixed`.
5. Something failed → fix only what failed and check again.
6. Everything checked and the user confirmed the screen items → tick the
   boxes and change the sprint's `**Status:**` to `Done`.

## 3. Evaluator (optional)

The architecture and the interfaces are still moving, so the `evaluator` is
offered, not run: once the checks pass, ask in one line whether to have it
review the sprint. If yes:

```
Agent(
  subagent_type="evaluator",
  prompt="Session: tmp/sessions/<slug>/. Evaluate Sprint <N> of plan.md.
          Diff base: <commit or branch the sprint started from>."
)
```

It reviews the working tree, before the commit. Write what it returns to
`tmp/sessions/<slug>/evaluation.md`, show the findings, and fix only what the
user agrees to.

## 4. Commit

**Never commit.** Once the sprint is `Done`, suggest the message in the
repo's pattern — `<type>(<scope>): <Description>`, e.g. `feat(ui): Added a
filter row to the units pane` — without naming the item or the sprint, with
the files that go in. The user commits.

The type is what `CHANGELOG.md` is built from (`cliff.toml`): `feat`, `fix`,
`perf`, `refactor`, `doc` and `test` each get a section, and `chore` is
skipped. A change a user would notice is not a `chore`.

## 5. Next step

Pause and ask before starting the next sprint. If none is left `Pending`:

> "Every sprint is done. Run `/feature:pr` to open the PR."

## Rules

- Never commit; only suggest the message.
- Never ask "did item X pass?" with an ID or by sending the user to open a
  file. The question carries the item's text, what was found, and your
  hypothesis.
- Do not change a status to `Done` without the checks passing and the user
  confirming the screen.
- Edit `plan.md` with targeted Edits, keeping what the user adjusted by hand.
- A short summary in chat: path + one line per file, no code pasted whole.
