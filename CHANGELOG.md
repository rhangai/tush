# Changelog

All notable changes to this project are documented here.

## [0.1.0] - 2026-09-22

### Initial Release

First release. **One terminal for every process your project needs** — dev
servers, build watchers and setup scripts, started, supervised and tailed in
one place.

You write your project's commands down once:

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
tush run -c tush.yaml web     # starts `web` and what it depends on
```

### What's in it

- **A screen to drive it from.** Proc list on one side, the selected proc's
  output on the other. `r` starts or restarts, `Backspace` stops, `Enter` opens
  the action menu, `PgUp`/`PgDn` scrolls back through the log.
- **Dependencies, groups and modes.** `depends` orders the startup, `group:`
  brings a whole side of the stack up at once, and a proc with `modes` has more
  than one way to run — `Build` and `Watch`, say — switched from the menu
  without editing anything.
- **Procs that outlive the screen.** `tush serve` runs the session headless on
  a socket; `tush attach` draws the same screen onto it from anywhere you can
  get a shell. Quitting an attached screen takes nothing down — which is what
  makes it work in a dev container.
- **Orderly shutdown.** Each proc gets its own process group, so stopping a
  `bash -c '…'` wrapper takes its children with it: `SIGTERM` first, `SIGKILL`
  ten seconds later.
- **Restarts don't overlap.** The outgoing run is stopped and waited on before
  the new one starts, so two dev servers never fight over a port.
- **Bounded logs.** Each proc keeps a tail of its output — 1 MiB by default,
  tunable — so a watcher that prints all day costs what you said it could.
- **`--no-tui`** prints line by line instead of drawing a screen, for CI and
  pipes.
- **No shell, on purpose.** A command is an argv, so `|`, `&&`, `>` and `$VAR`
  are literal text. Want a shell? Ask for one:
  `run: [bash, -c, "npm run build | tee out"]`.

### Worth knowing

- Early days: it runs, and everything above works, but things still move.
- Linux and macOS — the shutdown path uses unix signals and process groups.
- `depends` today means _"has finished"_, which fits setup steps. A proc that
  never exits will hold its dependents.
- `-c` is taken as given; there is no searching up the tree for a config.

Bug reports and rough edges welcome.
