---
description: Write and open the feature's PR on GitHub, with a summary and "How to test" taken from requirements.md and plan.md
argument-hint: "[slug]"
---

# /feature:pr

Writes the feature's PR and opens it on GitHub, if needed.

<arguments>
$ARGUMENTS
</arguments>

## Find the session

- An argument with a slug → `tmp/sessions/<slug>/`. No argument: look in
  `tmp/sessions/*/plan.md` for one whose `Branch:` is the current branch.
- The session lives in `tmp/`, which is not committed: the PR body is the only
  place a reviewer will see the why and the how to test. Write it to stand on
  its own.

## Pre-checks

1. Any sprint still `Pending` in `plan.md`? List them and ask whether to open
   the PR anyway (a partial PR). Do not block: the user decides.
2. `git status`: if anything is uncommitted, list it and suggest the commit
   message. The user commits: do not run `git add` or `git commit`.
3. Offer to run, before the PR, `cargo fmt --check`,
   `cargo clippy --all-targets` and `cargo test`. Run them only if the user
   says so.

## Flow

1. Write the title and the body and **show them to the user before
   publishing**:
    - Title: at most 70 characters, in the commit pattern
      (`<type>(<scope>): <Description>`).
    - Body: each paragraph and each list item on one line. In a PR body
      GitHub renders a newline as a line break, so a hard wrap at column 80
      shows as a ragged paragraph.

```markdown
## Summary

- <1-3 items in the user's words, from the Summary / What of requirements.md, or the plan's Brief>

## How to test

- [ ] <every `screen:` item of every sprint, with the config and the steps>
- [ ] <`command:` items a reviewer can run themselves>

## Decisions

- <only those from `## Decisions` in plan.md that change what the reviewer
  expects to see; omit the section if there are none>
```

2. Approved → ask before pushing: `git push -u origin <branch>`.
3. Open it against `main`:
    - `gh` installed and logged in → `gh pr create --base main --title
"<title>" --body-file <file in the scratchpad>`.
    - Otherwise → give the user the compare link,
      `https://github.com/rhangai/tush/compare/main...<branch>?expand=1`, with
      the title and body to paste.
4. Give back the PR link.

## Rules

- Never push or open a PR without showing the content and asking.
