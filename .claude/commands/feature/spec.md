---
description: Write the feature's spec (requirements.md) by talking it through with you — why, what changes for whoever runs tush, what is out, and how to know it is done
argument-hint: "[idea in free text | slug]"
---

# /feature:spec

Writes the feature's spec to `tmp/sessions/<slug>/requirements.md`, by
talking it through. The first step of the flow:

```
/feature:spec → /feature:plan → /feature:work → /feature:pr

tmp/sessions/<slug>/          (tmp/ is gitignored: nothing here is committed)
├── requirements.md   the spec (this command)
├── plan.md           sprints, "Done when", status, decisions (/feature:plan)
└── evaluation.md     the evaluator's review, when one was asked for (/feature:work)
```

<arguments>
$ARGUMENTS
</arguments>

## Find the session

- The argument is a slug of an existing session, or free text describing the
  idea. A loose idea is enough to start; it does not have to be thought
  through.
- No argument: look in `tmp/sessions/*/requirements.md` for one whose
  `Branch:` is the current branch. None → ask what is to be done.
- `requirements.md` already exists? Then this is a refinement: read it and
  iterate on it, do not rewrite it from scratch.

## Flow

1. Read `AGENTS.md`, `ARCHITECTURE.md`, the `//!` of the modules the feature
   touches, and `README.md` / `CONFIG.md` if it changes what a user sees or
   writes. Research before asking: a question the code answers does not go to
   the user.
2. Collect, by talking, what is missing for it to be clear **why** the feature
   exists, **what** changes for the person running tush, and what is **out**.
   For a bug: the config, the terminal size, the command, and the steps that
   reproduce it.

    A few questions at a time, each with your hypothesis ("I think it is X
    because I saw Y; right?"). Do not assume: ask. Questions this project
    always has to answer:
    - **Config:** does every existing `tush.yaml` still load and mean the same
      thing? A new key needs a default and a line in `CONFIG.md`.
    - **The view client:** does the screen need something the session does not
      hand over yet? That widens `ViewClient`, which has to survive being a
      socket — `tush attach` against a `tush serve` of the same build.
    - **The screen:** where it goes, what key, what it looks like at 80×24, and
      whether the footer has room (`/ui`).
    - **Cost:** does it run per frame, per poll or per log line? Then it is on
      a hot path and allocates nothing in steady state.
    - **Core modules:** does it need `log`, `runner` or `unit` to change? That
      is a decision of its own (AGENTS.md, _Scope_).
    - **Done:** how will we know it works?

3. If the request contradicts `AGENTS.md`, `ARCHITECTURE.md` or a module's
   documented invariant, say so before going on.
4. Present the summary and iterate until the user approves.
5. Still on `main`? Ask, then create the branch: `git switch -c feat/<slug>`
   (`fix/<slug>` for a bug, `chore/<slug>` for an internal change).
6. Write `requirements.md`:

```markdown
# <Feature name>

> Branch: `<branch>`

## Summary

<two to four short paragraphs, in plain words: the problem, why it is worth
solving, and what changes when using tush. No file or type names: this is
what someone reads to understand the feature without opening anything else>

## What

<what changes for the person running tush: on the screen, in the keys, in the
config file, on the command line, over the socket; what exists already and is
affected>

### Out of scope

- <item>: <why / where it will be handled>

## Acceptance criteria

- [ ] <behaviour that can be checked on the screen, from a command, or by a
      test: "restarting a unit from the menu keeps the first run's lines
      above the second's in its log", not "restart works">

## Risks and assumptions

- <what can go wrong, what was assumed without confirmation>

## How

<high-level technical plan: which layers it touches, the types and modules,
invariants it leans on, whether `ViewClient` or a core module changes, and
the traps found in the code>
```

Who reads it: the user, and an AI developer with your context. Be concise
without losing a rule. Skip a section that does not apply.

The **acceptance criteria** are the contract: `/feature:plan` splits them
among the sprints' "Done when". Each one has to be answerable with yes or no.
Cover the happy path, the edge cases the conversation raised (an empty log,
no units, a narrow terminal, a config without the new key) and what must
**not** change.

The **How** is a direction, not a decision. If it changes a core module,
widens `ViewClient` or adds a dependency, mark it as needing the user's
go-ahead, and offer `/engineer:arch` on it.

## Next step

> "Spec written to `<path>`. When you want, run `/feature:plan`."

A small change does not need a spec: `/feature:plan <idea>` plans straight
from a brief.
