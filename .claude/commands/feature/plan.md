---
description: Write or adjust plan.md — sprints with Pending/Done status and a "Done when" that a test, a command or the screen can check
argument-hint: "[slug | idea in free text] [requested change]"
---

# /feature:plan

Writes, or adjusts, the development plan in `tmp/sessions/<slug>/plan.md`,
from `requirements.md` or from a brief. `plan.md` is the only place the
feature's state lives.

<arguments>
$ARGUMENTS
</arguments>

## Find the session

- An argument with a slug → `tmp/sessions/<slug>/`. No argument: look in
  `tmp/sessions/*/requirements.md` and `tmp/sessions/*/plan.md` for one whose
  `Branch:` is the current branch.
- `plan.md` already exists? Then this is an adjustment: apply it with a
  targeted Edit and record it under `## Decisions`. Do not rewrite the file.
- No `requirements.md`, and the argument is an idea in free text: **the short
  path.** For a small change — one sprint or two, no core module, no new
  config key — plan straight from the brief: ask what is unclear, then write
  the plan with a `## Brief` section in place of the requirements link. Still
  on `main`? Ask, then create the branch, as `/feature:spec` does. Anything
  bigger: suggest `/feature:spec` first.

## Flow

1. Read `requirements.md` (or the brief), `ARCHITECTURE.md` and the code it
   names. Every **acceptance criterion** has to land in some sprint's "Done
   when"; if one fits nowhere, tell the user.
2. Write `plan.md` in the format below.
3. A sprint that changes `log`, `runner` or `unit`, widens `ViewClient` or
   adds a dependency: run `/engineer:arch` on what it proposes, and put the
   verdict in one paragraph in that sprint's notes. The user decides on that
   sprint with the review in front of them.
4. Show the user a summary — the sprints and each one's "Done when" — and
   iterate until they approve.

```markdown
# Plan — <Feature name>

> Branch: `<branch>` · Requirements: [requirements.md](requirements.md)

<or, on the short path, in place of the link above:>

## Brief

<the idea in two or three sentences, and what was settled while asking>

## Sprint 1 — <what it delivers, in the user's words>

**Status:** Pending

**Why:** <1-3 sentences: why this sprint exists and why in this order>

**Done when**

- [ ] <observable behaviour>
      → test: `<module>` › `<test_name>` — fails if <the bug it catches>
- [ ] <observable behaviour>
      → command: `<command>` <what it has to print or do>
- [ ] <observable behaviour>
      → screen: <config, terminal size, keys to press, what has to appear>

**Out:** <what is left for another sprint or another feature>

**Notes for implementing**
<files, layers, invariants to keep, traps. A guide for whoever implements,
not a criterion.>

## Sprint 2 — ...

## Decisions

- <YYYY-MM-DD> — <what changed in the plan> — <why>

## Found, not fixed

- `<file>:<line>` — <what is wrong> — <found during Sprint N>
```

`**Status:**` takes two values only: `Pending` or `Done`. `/feature:work`
changes it to `Done`, once every item is checked and the user has confirmed.

`## Found, not fixed` collects what `/feature:work` notices on the way and
leaves alone. Each entry is a candidate for a sprint or a feature of its own;
nothing in it is done unless the user turns it into one.

## Writing "Done when"

This is what the user reads to say whether it is finished. Each item answers,
on its own, **"how do I know it is done?"**, without opening code.

- **Behaviour, not implementation.** "A config written before this feature
  loads and runs exactly as it did" ✓. "`ConfigProc` has a new field" ✗:
  that is the how, and it goes in the notes.
- **Every check is one of three kinds:**
    - `test:` a named test, existing or written in the sprint, and the bug
      it catches: "fails if the reader recounts lines it already saw". No
      plausible bug, no test; reach for the edges (empty, full, wrap, a line
      split across chunks) before the happy path. Tests belong on
      the data structures — `log`, `util`, `unit` — and nowhere in `ui`
      (AGENTS.md, _Scope_). A test anywhere else is proposed to the user, not
      planned silently.
    - `command:` a command and its expected output: `cargo run -- dispatch
start <key>` against a running `tush serve`, `cargo run -- run --no-tui`,
      a config that must fail to load with a given message.
    - `screen:` concrete steps for the user to follow on `cargo run -- run
--config <file>`: the config, the terminal size, the keys, and what has to
      appear. The agent cannot see the screen: it says what it expects from
      reading the code — and which sizes from `ui.md` §5 it reasoned about —
      then asks the user to confirm. Name the config: `tmp/demo/tush.yaml` has
      panels, groups, modes and dependencies, or write the one the step needs
      into the session folder.

    "Read the file and confirm that…" is not a check. If only reading code can
    validate it, the item describes implementation: rewrite it as an effect or
    move it to the notes.

- **No allocation counts yet.** The code is moving too fast for a number to
  stay meaningful; hot-path rules are kept by `rust-hot-path`, and a number is
  asked for with `/engineer:perf` when it matters.
- **3 to 6 items per sprint.** More is almost always a sprint too big: split
  it.
- **Not included:** `cargo fmt`, `cargo clippy --all-targets`, `cargo test`
  passing. `/feature:work` runs those every sprint.
- An item has no number or ID. Whoever refers to one quotes its text.

## Phasing

- A sprint is a delivery that can be checked on its own.
- **A core-module change is its own sprint.** If `log`, `runner` or `unit`
  has to change, or `ViewClient` widens, that sprint comes first, says what
  invariant it touches, carries the `/engineer:arch` verdict, and needs the
  user's go-ahead before `/feature:work` starts it.
- **Do not mix a behaviour change with an optimisation.** If behaviour
  changes, it has to be possible to say why.
- **Do not mix a refactor of what is there with the feature.** Code that
  diverges from the skills goes under `## Found, not fixed`; fixing it is a
  sprint the user asks for.
- An order the user imposed ("first only the config") wins.
- **User docs go with the change.** A sprint that changes what a user sees,
  types or writes in the config updates `README.md` or `CONFIG.md` in the
  same sprint, by `.claude/commands/doc.md`'s rules — say which in its notes.
- A small sprint is better than a "complete" one.
- A plan change after starting: edit the sprint or create a new one, and
  record it under `## Decisions`. No addenda, no "1b" sprint with items
  hanging off it.

## Next step

> "Plan approved. Run `/feature:work` to start Sprint 1."
