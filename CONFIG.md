# The tush config file

A tush config declares the **procs** of your project: the long lived commands
tush runs and supervises for you — dev servers, build watchers, setup scripts —
along with what each one has to wait for.

It is a YAML file.

```yaml
procs:
  api:
    run: [npm, start]

  web:
    depends: [api]
    run: [npm, run, dev]
```

---

## Structure

`procs` is the only top-level key. Under it, every proc is written as an entry
whose **key is the proc's name** — how you refer to it, and what `depends`
entries point at.

```yaml
procs:
  <name>:
    name:    <string>          # optional
    group:   [<string>, ...]   # optional
    depends: [<string>, ...]   # optional
    run:     <commands>        # optional
    modes:   [<mode>, ...]     # optional
```

| Key | Type | Meaning |
| --- | --- | --- |
| `name` | string | A label to show instead of the proc's key |
| `group` | list of strings | Groups this proc belongs to |
| `depends` | list of strings | Procs that must be up before this one starts |
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

Every mode needs a `name` and a `run`, and those are the only two keys it may
have. A mode's `run` takes the same two spellings as a proc's.

Modes stay in the order you write them.

---

## `depends`

The procs that have to be up before this one starts. Each entry is another
proc's key.

```yaml
web:
  depends: [api, database]
  run: [npm, run, dev]
```

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
| Both `run` and `modes` | Accepted, but nothing decides which one a plain start means. Pick one. |

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
