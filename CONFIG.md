# The tush config file

A tush config declares the **procs** of your project: the long lived commands
tush runs and supervises for you — dev servers, build watchers, setup scripts —
along with what each one has to wait for.

It is a YAML file.

```yaml
procs:
  install:
    run: [npm, ci]

  api:
    depends: [install]
    run: [npm, start]

  web:
    depends: [install]
    run: [npm, run, dev]
```

---

## Structure

`procs` holds the procs. The keys beside it are the session's own settings:
[`log_size`](#log_size) and [`parse_ansi`](#parse_ansi) are what a proc gets
unless it says otherwise, and [`ui`](#ui) is about the screen rather than
about any proc. Under `procs`, every proc is written as an entry whose **key
is the proc's name** — how you refer to it, and what `depends` entries point
at.

```yaml
log_size:   <size>               # optional
parse_ansi: <boolean>            # optional

ui:                              # optional
  colors: <boolean>

procs:
  <name>:
    name:        <string>        # optional
    name_short:  <string>        # optional
    group:       [<string>, ...] # optional
    depends:     [<string>, ...] # optional
    working_dir: <path>          # optional
    panel:       main | minor    # optional
    log_size:    <size>          # optional
    parse_ansi:  <boolean>       # optional
    run:         <commands>      # optional
    modes:       [<mode>, ...]   # optional
```

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | A label to show instead of the proc's key |
| `name_short` | string | A shorter label, for where the full one will not fit |
| `group` | list of strings | Groups this proc belongs to |
| `depends` | list of strings | Procs that have to finish before this one starts |
| `working_dir` | [path](#working_dir) | Where its commands run |
| `panel` | `main` or `minor` | Which of the two lists on screen it is drawn in |
| `log_size` | [size](#log_size) | How much of this proc's output to keep |
| `parse_ansi` | [boolean](#parse_ansi) | Whether its escape sequences are read as escape sequences |
| `run` | [commands](#run) | The one way this proc runs |
| `modes` | [list of modes](#modes) | Several named ways to run it |

Every key is optional, and **any key not in that table is an error**. A
misspelled key gets you a message naming what was expected, not a setting that
silently does nothing.

---

## `run`

A command is written as its **argv** — the program, then each argument, as a
list of strings.

```yaml
run: [npm, run, build]
```

There is no shell involved. `|`, `>`, `&&`, `*` and `$VAR` are not special;
they are passed to the program as literal text. If you want a shell, ask for
one:

```yaml
run: [bash, -c, "npm run build | tee build.log"]
```

### One command or several

`run` takes either one command or a list of them. They are told apart by what
is inside the list — strings, or lists:

```yaml
# one command: a list of strings is a single argv
run: [npm, run, build]

# several: a list of lists is one argv each
run:
  - [npm, run, build]
  - [npm, run, test]
```

A single command is still a list even when it is one word — `run: [bash]`,
never `run: bash`.

### Quote anything that is not a word

Every part of an argv has to be a string, and YAML will hand over a number or
a boolean if you let it:

```yaml
run: [serve, --port, 8080]      # error: 8080 is a number
run: [serve, --port, "8080"]    # right
```

The same goes for `true`, `false`, `yes`, `no`, `on`, `off` and `null`. Rather
than memorise which bare words YAML treats as strings, quote anything that is
not plainly a word.

---

## `modes`

When a proc has more than one way to run — build it once, or watch it — give
it `modes` instead of `run`. A mode is a named `run`:

```yaml
web:
  modes:
    - name: Build
      run: [npm, run, build]

    - name: Watch
      run:
        - [npm, run, watch]
        - [npm, run, serve]
```

A mode may also carry a `name_short`, which the units list uses the same way
it uses a proc's:

```yaml
modes:
  - name: Watch
    name_short: W
    run: [npm, run, watch]
```

Every mode needs a `name` and a `run`; `name_short` and
[`working_dir`](#working_dir) are the only other keys it may have. A mode's
`run` takes the same two spellings as a proc's.

Modes stay in the order you write them.

---

## `depends`

The procs that have to **finish** before this one starts. Each entry is
another proc's key.

```yaml
web:
  depends: [install, migrate]
  run: [npm, run, dev]
```

Finished means the run ended, whatever it ended with: a proc that failed still
releases what waited on it. So `depends` is for the steps that end — `npm ci`,
a migration, a build.

A proc that never exits never finishes, and whatever depends on it never
starts: name a dev server here and it waits for good.

---

## `working_dir`

Where the proc's commands run. Leave it out and they run wherever `tush` was
started.

```yaml
web:
  working_dir: ./frontend
  run: [npm, run, dev]
```

A relative path is **relative to `tush`'s own working directory, not to the
config file**. A config kept in `config/tush.yaml` and started from the
project root still writes `./frontend`.

Nothing checks the directory when the config loads, because a directory an
earlier proc creates is a working config. One that is still missing when the
proc starts fails the way a missing program does — and the log line names the
directory, which is what tells the two apart.

A mode may carry its own, and a mode's wins over the proc's:

```yaml
web:
  working_dir: ./frontend
  modes:
    - name: Watch
      run: [npm, run, dev]
    - name: E2E
      working_dir: ./e2e        # this mode only
      run: [npm, test]
```

---

## `log_size`

How much output to keep, as a size. Written at the top level it is what every
proc keeps; written inside a proc it is what that one keeps instead.

```yaml
log_size: 1M            # what a proc keeps unless it says otherwise

procs:
  api:
    run: [npm, start]

  web:
    log_size: 16M       # this one prints a lot and you read it
    run: [npm, run, dev]

  db-setup:
    log_size: 64K       # four lines and done
    run: [./scripts/db.sh]
```

Bytes, plain or with a unit: `65536`, `64K`, `8M`, `1G`. Binary units, so `1K`
is 1024. The default is `1M`.

**Bytes and not lines**, because bytes is what is actually reserved: a proc
whose lines are long remembers fewer of them than a line count would suggest.
This is a tail and not a transcript — the oldest output is dropped to make
room, which is the point of a bounded size and not a failure.

The memory is taken up front and for every proc, so the figure is what a proc
costs whether or not it ever prints that much. That is the reason for the
per-proc key: a session with one noisy watcher and ten setup scripts should
not reserve the watcher's size eleven times.

---

## `parse_ansi`

Whether a proc's escape sequences are read as escape sequences. On by default.
At the top level it is what every proc gets; inside a proc it is what that one
gets instead.

```yaml
parse_ansi: true            # what a proc gets unless it says otherwise

procs:
  api:
    run: [cargo, run]

  codegen:
    parse_ansi: false       # show exactly what it writes
    run: [./scripts/gen.sh]
```

On, a sequence is consumed where the line is cut: its bytes leave the text and
what they said becomes the colour of the run that follows. Off, the sequence
stays in the line as the characters it is made of, and each of them takes a
column like any other character.

**It is not only about colour**, which is why it is not under [`ui`](#ui):
reading the sequences is what decides where a column falls, so the answer
reaches the log itself and not just the screen. A proc that redraws a progress
bar and a proc whose output you want to read exactly as written want opposite
answers — hence the per-proc key.

---

## `ui`

What the screen does, as against what a proc does. Its own section because
nothing in it is about a proc.

```yaml
ui:
  colors: true
```

| Key | Type | Meaning |
| --- | --- | --- |
| `colors` | boolean | Whether the colours found in a log are painted. On by default. |

With `colors: false` the sequences are still read — that is
[`parse_ansi`](#parse_ansi) — so what you get is the text without them, for a
terminal that would make a mess of the colours.

It belongs to the session and not to the screen reading it, so a `tush attach`
is told what this file said.

---

## `panel`

Which of the two lists on the left of the screen the proc is drawn in.

```yaml
db-setup:
  panel: minor
  run: [./scripts/db.sh]
```

`main` is the default and the list proper. `minor` is the compact list under
it, for the procs you do not sit and watch — setup steps, migrations, a
watcher you only look at when it breaks.

It changes nothing about what the proc does or when it runs. The two lists are
navigated separately: `j` and `k` stay in the list they are in and wrap at its
ends, and <kbd>Tab</kbd> moves between them, each remembering the row you left
it on. A proc in the minor list is selected like any other, so its log and its
action menu are there when you do need them.

---

## `group`

Names you can use to address several procs at once.

```yaml
api:
  group: [backend, setup]
  run: [cargo, run]
```

Note the key is `group`, singular — `groups` is not accepted — and the value is
always a list, even with one entry: `group: [setup]`, not `group: setup`.
`depends` is a list in the same way.

---

## `name`

A label, for when the proc's key is not what you want to read on screen.

```yaml
api:
  name: API server
  run: [cargo, run]
```

Leave it out and the key is used.

---

## `name_short`

A second, shorter label, for the places too narrow for the full one — today
that is the units list, which gives each proc a single line.

```yaml
web-frontend-dev-server:
  name:       Frontend
  name_short: web
  run: [npm, run, dev]
```

Leave it out and the full name is used there too. It does **not** fall back
the way `name` falls back to the key: the two are kept apart all the way to
the screen, so a view can tell a short name you chose from one it settled for.

---

## Order

Procs are loaded **sorted by name**, not in the order you wrote them. Moving
lines around in the file changes nothing about what starts first — if you care
about the order of two procs, say so with `depends`.

Modes are not affected; those stay in the order you write them.

---

## Reusing pieces with anchors

YAML anchors work, and are the way to avoid repeating yourself. Anchor a value
with `&name` and reuse it with `*name`:

```yaml
procs:
  api:
    group: &backend [backend, watch]
    run: [cargo, run]

  worker:
    group: *backend
    run: [cargo, run, --bin, worker]
```

It works for any value — a `run`, a `group`, a whole proc.

The merge key `<<`, which some YAML tools use to mix one mapping into another,
is **not** supported. This is an error:

```yaml
worker:
  <<: *api        # error: unknown field `<<`
  run: [cargo, run, --bin, worker]
```

---

## Gotchas

| Written | What happens |
| --- | --- |
| `run: []` | A command with no program — not "runs nothing". Leave `run` out instead. |
| The same proc name twice | The last one silently wins. YAML allows it; nothing warns you. |
| `group: setup` | Error — it has to be a list: `group: [setup]`. |
| `run: bash` | Error — it has to be a list: `run: [bash]`. |
| Both `run` and `modes` | Error — ``a` declares both `run` and `modes``. Pick one. |

---

## A full example

```yaml
procs:
  server-setup:
    group: [setup]
    run: [bash, scripts/setup-server.sh]

  server:
    name: API server
    group: [backend]
    depends: [server-setup]
    modes:
      - name: Build
        run: [cargo, build]
      - name: Watch
        run: [cargo, watch, -x, run]

  site-setup:
    group: [setup]
    run: [npm, ci]

  site-admin:
    group: [web]
    depends: [site-setup]
    modes:
      - name: Build
        run: [npm, run, build]
      - name: Watch
        run:
          - [npm, run, watch]
          - [npm, run, serve]
          - [npm, run, proxy]
```
