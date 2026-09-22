<h1 align="center">tush</h1>

<p align="center">
  One terminal for every process your project needs.<br>
  Dev servers, build watchers, setup scripts — started, supervised, and
  tailed in one place.
</p>

---

You write down your project's commands once. `tush` starts them, keeps them
running, remembers the tail of each one's output, and gives you a screen to
read and drive it from — start, stop, restart, switch a proc between `build`
and `watch`, all without leaving the keyboard.

```yaml
# tush.yaml
procs:
  api:
    run: [npm, start]

  web:
    depends: [api]
    run: [npm, run, dev]
```

```sh
tush run -c tush.yaml
```

> **Status: early days.** It runs, and the pieces below all work, but there
> are no released binaries yet and things still move. Build it from source.

## Install

```sh
cargo install --git https://github.com/rhangai/tush
```

Needs a Rust toolchain on edition 2024. Linux and macOS: the shutdown path
uses unix signals and process groups.

## Getting started

**1. Write a `tush.yaml`** next to your project. Each entry under `procs` is
one command, written as its argv — the program, then each argument:

```yaml
procs:
  api:
    name: API server
    run: [cargo, run]

  db-setup:
    panel: minor            # the compact list: setup steps, migrations
    run: [./scripts/db.sh]

  web:
    depends: [db-setup]     # waits for it to finish first
    modes:                  # two ways to run it; pick one on screen
      - name: Build
        run: [npm, run, build]
      - name: Watch
        run: [npm, run, dev]
```

**2. Run it**, naming what should come up straight away:

```sh
tush run -c tush.yaml web              # start `web` and what it depends on
tush run -c tush.yaml group:backend    # start every proc in a group
tush run -c tush.yaml                  # start nothing; drive it from the screen
```

**3. Drive it from the screen.** Procs you did not name on the command line
are started from there, and so is everything else.

Every key is in [CONFIG.md](CONFIG.md).

## The screen

```
┌────────────────────────────┬────────────────────────────────────────────┐
│ > ● API server             │ API server · running · Watch               │
│     running          Watch │ Listening on http://localhost:3000         │
│                            │ GET /api/health 200 1ms                    │
│   ○ web                    │ GET /api/users 200 14ms                    │
│     stopped                │                                            │
│ ────────────────────────── │                                            │
│   ✓ db-setup               │                                            │
│     done                   │                                            │
│   ✗ migrate                │                                            │
│     exit 1                 │                                            │
└────────────────────────────┴────────────────────────────────────────────┘
 ↑↓ move  ⇥ panel  ⏎ actions  r (re)start  ⌫ stop  pgup/dn scroll  q quit
```

The list on the left, the selected proc's output on the right, and the keys
along the bottom. The list is split in two: the procs you sit and watch on
top, and the ones you only look at when they break (`panel: minor`) under the
rule.

| Mark | Meaning | | Mark | Meaning |
| --- | --- | --- | --- | --- |
| `○` | stopped | | `●` | running |
| `◌` | waiting on a dependency | | `✓` | finished, cleanly |
| `◐` | starting | | `✗` | exited with an error, or was killed |
| `◑` | stopping | | | |

Each mark carries its meaning in its shape and not only in its colour, so the
list still reads at a glance in a terminal with a palette of its own.

### Keys

| Key | Does |
| --- | --- |
| <kbd>↑</kbd> <kbd>↓</kbd>, <kbd>j</kbd> <kbd>k</kbd> | Move through the list |
| <kbd>Tab</kbd> | Move between the two lists, each keeping its place |
| <kbd>Enter</kbd> | Open the action menu for the selected proc |
| <kbd>r</kbd> | Start it — or restart it, in the mode it is already in |
| <kbd>Backspace</kbd> / <kbd>Delete</kbd> | Stop it |
| <kbd>PgUp</kbd> <kbd>PgDn</kbd>, wheel | Scroll the log |
| <kbd>Shift</kbd> + <kbd>↑</kbd> <kbd>↓</kbd> | Scroll the log a line at a time |
| <kbd>End</kbd>, <kbd>G</kbd> | Back to following the tail |
| <kbd>q</kbd>, <kbd>Esc</kbd> | Quit — which stops everything `tush run` started |
| <kbd>Ctrl</kbd>+<kbd>C</kbd> | Quit, from anywhere |

The action menu is where a proc with `modes` is switched between them, and it
says what each entry will actually do: `Restart` on the mode that is up,
`Start` on the others.

## Commands

Three ways to run, differing only in where the session lives and where the
screen is:

| | What it does |
| --- | --- |
| `tush run -c tush.yaml [targets]` | Session and screen in one process. Quitting takes the procs down with it. |
| `tush serve -c tush.yaml [targets]` | The session with no screen: prints each line as `[proc] line` and listens on a socket. Ends on <kbd>Ctrl</kbd>+<kbd>C</kbd> or `SIGTERM`. |
| `tush attach` | A screen onto a session started by `serve`. Quitting takes nothing down. |

A *target* is a proc's name, or `group:name` for every proc in a group. Name
none and nothing starts.

| Flag | On | Means |
| --- | --- | --- |
| `-c`, `--config <PATH>` | `run`, `serve` | The config file. Required, and taken as given — no searching up the tree. |
| `--no-tui` | `run` | Print the output line by line instead of drawing a screen, for a CI log or a pipe. |
| `--socket <PATH>` | `serve`, `attach` | Which socket to listen on or attach to. Also read from `TUSH_SOCKET`. Defaults to one path under the runtime directory. |
| `--fps <N>` | `run`, `attach` | How often the screen redraws. Default 10. |
| `--refresh-rate <DURATION>` | `run`, `attach` | The same number the other way round: `100ms`, `2s`. |

### `serve` and `attach`

`tush run` is one process: the session and the screen together, and closing
the screen is closing the session. `serve` and `attach` are that same thing
cut in two, for when the procs should outlive the screen — or live somewhere
you cannot draw one, like a container.

**`tush serve` is the session with no screen.** It starts and supervises the
procs exactly as `run` does, prints every line it captures to stdout tagged
with the proc that wrote it (`[api] listening on :3000`), and listens on a
Unix socket for a screen to turn up. Nothing a client does ends it: it runs
until <kbd>Ctrl</kbd>+<kbd>C</kbd> or a `SIGTERM`, and then stops the procs in
order.

**`tush attach` is the screen with no session.** It reads a running session
over that socket and draws the same panes with the same keys — select, scroll,
start, stop, restart, switch mode. Quitting takes nothing down, and you can
attach, quit and attach again as often as you like; the logs belong to the
session, so what it kept is what you get.

About the socket:

- With no `--socket` and no `TUSH_SOCKET`, both ends use
  `$XDG_RUNTIME_DIR/tush.sock`, or `/var/run/tush.sock` where that variable is
  not set. One name both halves can say from memory.
- Two sessions on one machine want the same path, and the second is refused
  with an error naming it — give one of them a `--socket` of its own.
- A socket file left behind by a server that crashed is taken over
  automatically, so a crash never leaves a path you have to delete by hand.
- It is created `0600`, and the directory it goes in has to exist already.

### In a dev container

This is the pattern the split was drawn for: the container runs the session,
and you attach a screen to it whenever you want to look.

```dockerfile
FROM rust:1 AS tush
RUN cargo install --git https://github.com/rhangai/tush

FROM node:22
RUN apt-get update \
    && apt-get install -y --no-install-recommends tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=tush /usr/local/cargo/bin/tush /usr/local/bin/tush
WORKDIR /app
# So neither command has to spell it out, and neither depends on the
# container having an XDG_RUNTIME_DIR.
ENV TUSH_SOCKET=/tmp/tush.sock
# An init as PID 1 — see below.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["tush", "serve", "-c", "tush.yaml"]
```

```sh
docker compose up -d
docker compose logs -f                      # every proc's output, line by line
docker compose exec dev tush attach         # the screen, whenever you want it
```

<kbd>q</kbd> closes that screen and leaves the container running. `docker
stop` sends `SIGTERM` to `tush serve`, which is the orderly shutdown — bear in
mind that it gives a proc ten seconds before `SIGKILL`, which is also Docker's
own default grace period, so a session with slow procs wants a longer
`stop_grace_period`.

**Give the container an init.** A session is a lot of processes, and as PID 1
`tush` inherits every orphan among them — a `bash -c` wrapper that exits
before what it started, anything a proc daemonises. It reaps the children it
spawned itself and nothing else, so the rest pile up as zombies. That costs
more at shutdown than in memory: a run is over when its process group is
empty, and a zombie still answers a signal, so a proc that is really gone can
sit there until the ten seconds run out. `tini` above does the reaping;
`docker run --init` and `init: true` on a compose service are the same thing
without touching the image.

The same works wherever you can get a shell on the machine the session is on:
`ssh -t host tush attach`.

## Good to know

- **No shell.** A command is an argv, so `|`, `&&`, `>` and `$VAR` are literal
  text. Want a shell? Ask for one: `run: [bash, -c, "npm run build | tee out"]`.
- **Logs are a tail, not a transcript.** Each proc keeps a bounded number of
  bytes (1 MiB by default, `log_size` to change it) and drops the oldest
  output to make room, so a watcher that prints all day costs what you said
  it could and no more.
- **Restarts do not overlap.** The outgoing run is stopped and waited on
  before the new one is released — no two dev servers fighting over a port.
- **Shutdown is orderly.** Each proc runs in its own process group, so
  stopping a `bash -c '…'` wrapper takes its children with it: `SIGTERM`
  first, `SIGKILL` ten seconds later.
- **`depends` means "has finished"** today, which fits setup steps. A proc
  that never exits will hold its dependents — see the open question in
  [ARCHITECTURE.md](ARCHITECTURE.md#open).

## Docs

- **[CONFIG.md](CONFIG.md)** — every key of the config file, with examples.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — how the crate is put together and
  why, for anyone reading or changing the code.
- **[AGENTS.md](AGENTS.md)** — house rules for working in this repository.

## Building

```sh
cargo run -- run -c tush.yaml      # from a checkout
cargo test                          # 175 tests
cargo doc --no-deps --document-private-items --open
```
