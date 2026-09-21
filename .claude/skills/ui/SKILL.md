---
name: ui
description: Draw the terminal UI in this repo — add or change a pane, a row, a popup, a key, a status mark, the footer, the theme, or how the screen behaves at a size it does not fit in. Carries the TUI's UX rules (what a developer reads a screen like this for, what may move, what a key costs) and the drawing rules under `src/ui` (panes are widgets, text goes straight into the buffer, every glyph comes from the theme). Use when asked to design or implement something on screen, to judge a layout, or when the screen is wrong at some terminal size.
---

# Drawing the screen

`/ui <target>` — a pane (`log`, `units`, the menu), a thing to add (a filter
row, a second popup, a count in the title), a binding, a theme entry, or a
complaint ("the list looks cramped", "it breaks at 80 columns"). With no
target, ask which one and stop; do not pick.

The deliverable is the change under `src/ui`. If the pane needs something the
session does not already hand over, that is a `ViewClient` change — say what
you need and why, and wait (AGENTS.md, *The UI client*: it has to survive
becoming a socket).

**A design question gets an answer, not a branch.** "Would a third pane work?",
"what should the footer say?" want the recommendation and what it costs.

## 1. What the screen is for

Somebody runs `tush` on a second monitor and looks over at it every few
minutes, between doing something else. Two questions, in this order:

1. **Which one is broken?** — the units pane, read down the gutter in one
   glance.
2. **What did it say?** — the log pane, which is why it has the rest of the
   screen.

Every decision is judged against those two. A thing that makes the screen
prettier and the gutter slower to read has lost.

## 2. Settled — cite it, do not re-derive it

`src/ui/render.rs`'s `//!` and AGENTS.md (*The UI*) say most of this. Re-read
the pane you are changing before designing anything.

- **A pane is a `Widget`, and what it remembers between frames is its
  `StatefulWidget::State`.** The pane is built and dropped every frame; the
  cursor, the scroll and the measured size are not. `UiRender` owns the
  states, the layout and the theme, and only places the panes.
- **Text goes into the buffer.** `Buffer::set_stringn`, or `set_clipped` when
  it may not fit. No `Line`, `Span`, `Paragraph`, `List` or `Block` title —
  each allocates in the building and again in the rendering, ten times a
  second, to say what it said last frame. A frame in steady state allocates
  zero times.
- **Therefore no cache.** A cache over something that should not be built is
  the symptom, and it brings a staleness check with it.
- **Every fixed glyph, word and colour comes from `UiTheme`** — three lists,
  read one at a time, which is what makes `ascii()` a change to one of them.
- **The panes share one wall.** `join_borders` tees the log pane's corners
  into the units pane's border; a double rule down the middle is the first
  thing the eye catches and it means nothing.
- **The units slice is ordered by panel and then by name**, so the two lists
  are one slice and the cursor is one index into it (`minor_start`). The log
  pane declares the rectangle it wants (`set_log`) and draws whatever came
  back, at the offset it came back at.

Numbers already chosen, and what they are: `UNITS_WIDTH` 30 (a name does not
grow when the window does), `LOG_MARGIN` 100 (a page of scrolling costs no
round trip), `MINOR_SHARE` 2, `NAME_MIN` 8, menu `MIN_WIDTH` 28, `WHEEL_LINES`
3. Changing one is a decision to write down, not a tweak.

## 3. The UX rules

**Nothing moves under the hand.** Rows keep their position whatever the
processes do — the order is the config's, not the state's. A list that sorts
running units to the top cannot be pressed, because the row you aimed at left.
Same rule for the menu: its entries are read once at open and held, or a
process exiting renumbers them under the cursor.

**A key that is not in the footer does not exist.** The footer is the only
documentation this program has. The eight hints already come to over 90
columns, so at 80 the last of them is cut — a ninth binding is a decision
about which one loses its place, not an addition.

**Two ways to press everything.** Arrows and `j`/`k`, `Enter` and the
accelerator, `Backspace` and `Delete`. A developer's hands are already in one
of two habits and the screen does not get to pick which. `Ctrl-C` quits from
anywhere, menu included — raw mode means nothing else turns it into a signal.

**Modes announce themselves.** The menu is the only one: it sits over
everything, takes every key, and `q` closes it instead of quitting. A mode you
cannot see is a mode you are stuck in.

**A press never does nothing.** The menu cursor opens on a row that can be
chosen and steps over the disabled ones; `Tab` into an empty list is a no-op,
not a cursor pointing at a row that is not there. If a key must be refused,
the screen has to show why — and there is no way to say "refused" yet
(`ViewClient::send` is fire and forget), so prefer a key that cannot be.

**Never blank a pane.** Late data draws the overlap, the last thing known, or
a word — `no output yet`, dim, so an empty pane does not read as a pane that
failed to draw. Blank is what broken looks like.

**Say when the screen is not the present.** Scrolled back, the log goes on
without you and the pane stops changing, which looks exactly like a process
that died: `↓ 42` in the bottom border is that rule paying for itself. Anything
else that can fall behind the session owes the same mark.

**Colour is the second channel, never the only one.** Each run state carries
its meaning in its shape (`●`, `✓`, `✗`, `◌`), because the palette is the
user's and some of them are on a light background, a colourblind palette, or
`NO_COLOR`. Use the sixteen named `Color`s, never RGB.

**One accent per row, dim for everything secondary.** A row says one thing
loudly — the status mark — and the rest quietly. A line of evenly bright text
is a line nobody picks anything out of.

**Alignment is what makes it look designed.** The comfortable row anchors both
lines to the same column whatever they say; the menu's rows line up with the
units list's cursor. Ragged text is the thing that reads as unfinished, not a
missing border.

**Elision is honest.** Clipped text ends in the theme's ellipsis, and widths
are measured with `unicode_width`, never `len()`. A name that simply stops
looks like a name spelled that way.

**The wheel is the only mouse.** Capturing it already costs the terminal's own
selection (`Shift` to override); taking clicks as well would cost more than a
pointer is worth on this screen.

**Ten frames a second is the budget.** Whatever you add is drawn 10×/s
forever; if it cannot be written into the buffer straight from what is already
in memory, it is the wrong design, not a slow one.

## 4. The three shapes you will be asked for

**A new pane.** A struct borrowing what it draws (`&[ViewUnit]`, the theme,
the shared `Block`), a `State` for what survives the frame,
`StatefulWidget::render`, and a place in `UiRender::draw` and `UiRender::areas`
— `areas` is cached against the frame `Rect` because `Layout::areas`
allocates. The keys that reach it go in `Ui::handle`, in `src/ui/ui.rs`, which
is the only place that knows about keys.

**A new key.** `Ui::handle` (or `handle_menu`), a `statusbar_*` pair in
`UiThemeSymbols` *and* `UiThemeTexts` *and* `UiThemeSymbols::ascii`, and an
entry in `hints()` — whose array is `[(…); 8]`, so adding one is a decision
about which eight the footer shows.

**A new theme entry.** All three lists stay in step, and `ascii()` writes every
field out rather than falling through to `default()` — that is deliberate, so
the compiler is what remembers a mark added later. Never a glyph inline in a
pane.

## 5. Sizes — the sweep before reporting

The screen has to hold at the sizes people actually have. Walk these; most
take one look at the code:

- **80×24**, the floor. Footer readable, both panes usable, menu inside the
  frame (`centered` clamps, so check the *content* fits).
- **Very narrow.** Under `UNITS_WIDTH + border`, the log pane has no columns
  left; under `NAME_MIN`, the right hand column has to be dropped rather than
  the name.
- **Very short.** A comfortable row wants three lines and falls back to
  compact; the minor list wants a rule and a row above it or it is not drawn
  at all — and then focus must come back to the main list.
- **Resize while the menu is open**, and while scrolled back.
- **Empty and huge.** No units; no minor list; one hundred units; a name
  longer than the pane; a log line wider than the pane.
- **`UiTheme::ascii()`** — the layout must still line up when every glyph is
  one column.

Every arithmetic on a `Rect` saturates or `checked_sub`s. A `u16` underflow
here is a panic that takes the terminal down with raw mode still on.

## 6. Scope

**Ask before writing what was not asked for** (AGENTS.md, *Scope*) — a helper
type, a second pane "while I was here", a theme field nobody wanted.

**No UI tests.** The screen is being shaped and tests over a shape about to
change are work thrown away twice. To *look* at a layout without a terminal,
render into `ratatui::backend::TestBackend` from a throwaway `#[test]`, print
the buffer, and delete it before reporting — say that you did. Keeping it is a
separate ask.

**Do not touch `log`, `runner` or `unit`** to make a pane easier. Propose it.

**Do not widen `ViewClient`** without asking: nothing async, nothing returning
`Result`, and the UI declares while the client satisfies.

## 7. Before reporting

```
cargo fmt
cargo clippy --all-targets      # clean, not quiet
cargo test
```

Then look at it. The agent cannot read a TUI out of a pipe, so ask the user to
run it — `! cargo run -- run --config tmp/example.yaml`, which has two panels,
modes, a dependency and a process that fails — and say what to look at.

Report short: what changed on screen, which of §5 you actually checked and
which you reasoned about, and anything you found wrong and left alone. Claims
about allocation get measured or dropped (AGENTS.md, *Claims*). English,
whatever language the conversation is in.
