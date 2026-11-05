# rexshell

A Windows shell for [Rex](https://superlogical.com/rex) servers, built on
[shrew-core](https://github.com/teddytennant/shrew) (the MIT engine behind
Shrew's iOS/Android/Linux clients) and [GPUI](https://crates.io/crates/gpui).

Rex ships a macOS app. This is the missing frontend: a native Windows window
that connects to a Rex server running anywhere on your tailnet and shows its
sessions, tabs and panes — not a terminal emulator plus SSH, but a real client
of the multiplexer.

```
┌──────────────┬──────────────────────────────────────┐
│ This box  ·v │ zsh │ server │ tests                  │  ← windows (tabs)
│  project (3) ├──────────────────────────────────────┤
│  logs (1)    │ ┌────────────────┐                   │
│ work      ·v │ │ $ cargo test   │                   │  ← blocks (panes),
│  api (2)     │ │   running 12…  │                   │    placed from the
│  docs (1)    │ └────────────────┘                   │    server's fractions
└──────────────┴──────────────────────────────────────┘
```

## Layout

| Path | What |
|---|---|
| `crates/engine` | Toolkit-free glue over `shrew_core::Shrew`: connects, reconnects, opens terminals, and the pixel→cell arithmetic. **This crate compiles and tests without a GPU.** |
| `crates/app/src/workspace.rs` | The chrome: title bar, sidebar, tabs, pane area |
| `crates/app/src/theme.rs` | Palette, metrics and the two chrome finishes, after [Pebrel](https://github.com/Kuddev/pebrel) |
| `crates/app/src/fonts.rs` | Picks the terminal font from what is installed |
| `crates/app/src/icons.rs` | Icons drawn from primitives, and the square icon button |
| `crates/app/src/shortcuts.rs` | The key table that both the matcher and the settings page read |
| `crates/app/src/settings.rs` | Font size, sidebar, glass; persisted next to the saved servers |
| `crates/app/src/win32.rs` | The four Win32 calls GPUI has no API for: drag, maximize/restore, minimize, round corners |
| `crates/app/src/terminal.rs` | One block as an entity: the terminal stream, focus, input |
| `crates/app/src/paint.rs` | `shrew_core::Frame` → GPUI quads and shaped text runs |
| `crates/app/src/keymap.rs` | GPUI keystroke → Rex key |

Panes are positioned from `BlockState::rect`, which Rex sends as fractions of
the window, so server-side splits, floats and zoom need no layout code here.

## Build

You need Rust 1.90+, a C toolchain, and **Zig 0.17** on `PATH` (or in `ZIG`):
`shrew-term` builds the real Ghostty VT engine from source.

```powershell
# Windows: rustup toolchain + Zig, both installed off C:
rustup-init.exe -y --no-modify-path --profile minimal --default-toolchain stable
# then unzip zig 0.17 next to it and add it to PATH

build.bat build --release
```

`build.bat` points `RUSTUP_HOME` / `CARGO_HOME` at D:, adds Zig to `PATH`, and
calls MSVC's `vcvars64.bat` before handing its arguments to `cargo`. First
build compiles Ghostty and ~460 crates: **18m 37s** here, 17.7 MB exe.

On Linux, `cargo check` currently trips over `xattr v0.2.3` vs `libc v0.2.191`
(`ENOATTR` was dropped), a known-upstream mismatch pulled in by
`gpui_http_client`. It does not affect the Windows build.

## Run

```powershell
rexshell my-box:7297             # or 100.64.0.2:7297, box.tailnet.ts.net
$env:REXSHELL_SERVER = "my-box"; rexshell
rexshell                         # attaches to a local Rex server if one runs
```

The server list is saved under `%APPDATA%\rexshell`.

### The server side

A Rex server listens only on local Unix sockets by default, and it has no
password auth yet — it trusts the network. Pick one:

```sh
# Recommended: the server joins your tailnet, so Rex knows who is calling.
rex server start --tailscale

# Or a plain listener on a network you fully control. Every connection gets
# full access; the Rex docs call this "super dangerous".
rex server start --extra-listen :7297 --allow-anonymous
```

## Terminal font

Maple Mono NF, as asked for, with a fallback chain: `fonts.rs` checks
`TextSystem::all_font_names` and takes the first of *Maple Mono Normal NF CN*,
*Maple Mono NF CN*, *Maple Mono Normal NF*, *Maple Mono NF*, *Maple Mono*,
*Cascadia Mono*, *Consolas*, `monospace`. Naming a family blindly is not enough:
GPUI resolves through DirectWrite and substitutes *something* when the family is
missing, possibly a proportional face, which would wreck the grid.

The installed Nerd Font build registers as **Maple Mono Normal NF CN** (from
`MapleMonoNormal-NF-CN-Regular.ttf` in the per-user font folder), so that is the
first name tried. The `CN` variant carries CJK glyphs in the same family, which
also fixes double-width alignment for Chinese text. Only the Regular weight is
installed here, so bold is synthesised by the platform.

Settings shows which family was actually picked. The shell's own chrome keeps the
system UI font — Maple Mono is for the terminals.

## Chrome

GPUI 0.2.2 draws no system title bar on Windows (`dwstyle` gets `WS_SYSMENU`,
`WS_THICKFRAME`, `WS_MAXIMIZEBOX`, `WS_MINIMIZEBOX` — but never `WS_CAPTION`), so
the shell draws its own and asks Windows for the missing verbs:

| Part | Behaviour |
| --- | --- |
| Title bar | drags the window (`WM_NCLBUTTONDOWN` + `HTCAPTION`), shows the server and session |
| `—` `□` `×` | minimize, maximize/restore, close — drawn as shapes, not glyphs |
| Corners | `DWMWA_WINDOW_CORNER_PREFERENCE` rounds them, as on Pebrel |
| Initial size | a fraction of the real display, because a fixed logical size overflows a scaled screen |

Window controls are drawn with divs on purpose: `─`, `□` and `✕` are not in
GPUI's Windows fallback chain, and rendering them as text produced nothing at
all (found by screenshotting the title bar).

## Shortcuts

`Ctrl-h/j/k/l` move focus between panes (vim/tmux-navigator style, geometrically:
no move when nothing lies that way) · `Ctrl-Shift-d` split right · `Ctrl-Shift-e` split down · `Ctrl-Shift-w` close pane
· `Ctrl-Shift-f` float the pane or tile it again · `Ctrl-Shift-t` new tab ·
`Ctrl-Shift-Enter` zoom · `Ctrl-b` sidebar · `Ctrl-,` settings. One table in `shortcuts.rs` feeds both the
matcher and the settings page, so the list you read is the list that works.

Pane focus moves geometrically — Rex picks the nearest block in that direction,
so pressing left at the left edge does nothing rather than jumping somewhere
surprising. The client then follows: a click publishes the pane it landed on with
`session.focus_block`, and a direction move starts from that pane and adopts the
server's answer, so client and server never disagree about where the keyboard is.

Zoom buys the whole pane area for one block: Rex toggles it, and the shell draws a
zoomed *floating* pane at full size because the server keeps its layer bounds and
only reports `zoomed_block_id` (for a tiled pane the reported layout already
collapses to that one block).

Floating a pane remembers the rect it had, so tiling it again puts it back on
the same side with roughly its old share (`split_hint`): a pane from the right of
a 30/70 split returns as `after` with a ratio of 0.3, not a flat 50/50. Rex takes
only a side and a ratio, not a saved layout, so a nested split comes back one
level deep.

### Prefix tools

`Ctrl-Shift-Space` arms a prefix; the next bare key opens a tool as a **floating
terminal** over the window (`session.new_layer`, bounds 64% × 72%, centred), so
the layout underneath is untouched and closing it leaves no hole. herdr-style:

| Key | Opens |
| --- | --- |
| `g` | `lazygit` in the pane's directory; quit it and the pane closes, focus returns |
| `f` | `fzf` over `rg --files`, and the choice is typed into the pane it came from |
| `d` | `fzf` over directories, typed in with a trailing `/` |
| `z` | both files and directories |

The picker prints its choice as a marked line (`@@pick@@<path>`) instead of
relying on fzf's screen, so the client can find it: when the picker's process
exits, the shell reads that line out of the pane's text and sends it to the
origin block with the same path that a paste uses. No file on the server, no
extra round trip, and nothing to poll — the pane's own stream is the channel.

Splits and floats are Rex's own: the server holds the layout, this shell only
sends `session.new_split` / `session.float_block` / `session.move_block` and
draws the blocks at the fractions it gets back. Another client sees the same
split immediately.

A floating pane carries its own title bar and a corner grip: drag the bar to
move it, drag the corner to resize. The drag is drawn from a local copy of the
rect and committed once on release, because Rex resizes a *layer* and the layer
id is not part of this shell's state — the commit resolves it with a
`session.view` call first (`session.resize_layer`). A drag therefore costs two
round trips in total, and none while the pointer is moving.

## Icons and translucency

Icons are painted from divs, not shipped as SVG: GPUI would need an
`AssetSource` and a bundled file per icon, and every icon here is a line or a
box. The `×` character is used where a diagonal is needed — GPUI 0.2.2 has no
rotation — which does exist in the Windows fallback chain, unlike `─`, `□` and
`✕`.

The window is translucent, not see-through, and that is a measured limit rather
than a preference:

| Attempt | Result on Windows 11, 150% scaling |
| --- | --- |
| `WindowBackgroundAppearance::Blurred` (the real acrylic) | every screen capture on the desktop went black |
| `Transparent` + `DWMWA_SYSTEMBACKDROP_TYPE` (the supported Win11 backdrop) | window composited over black; backdrop never drawn |
| `GPUI_DISABLE_DIRECT_COMPOSITION=true` | capture fine, but the renderer still writes opaque pixels |
| GDI capture of the desktop behind the window, painted as the background | worked, and GPUI panicked (`RefCell already borrowed`) as soon as a task delivered the frames — the task is polled from the Win32 message pump while the app is borrowed |

GPUI always paints opaque pixels on Windows, so nothing behind the window can
reach the screen without a framework change. What the shell does instead: one
vertical gradient behind everything (slate at the top, near-black at the
bottom), chrome surfaces at 72% alpha over it, and the terminal at 72% over the
same. The window then reads as a single translucent sheet, and text stays
readable because the gradient is the darkest thing in the stack.

Measured across the window: `(25,31,37)` at the top, `(17,20,24)` at the bottom,
identical in the sidebar and the terminal — one sheet, not two panels. The
Settings switch labelled **Translucent** turns the gradient off and leaves flat
surfaces.

## Input methods

Chinese, Japanese and Korean text never arrives as a keystroke: the IME hands the
composed string to the focused element's *input handler*. So `TerminalView`
implements `EntityInputHandler` and registers it during paint with
`window.handle_input`; `replace_text_in_range` types the committed text into the
pty with `session.send_text`.

Three things had to line up:

| Symptom | Cause | Fix |
| --- | --- | --- |
| Composed text never appeared | nothing registered an input handler, so the IME had nowhere to deliver | `EntityInputHandler` + `handle_input` in the paint callback |
| Every ASCII character arrived twice | with a handler registered, GPUI delivers characters *both* as keys and as input-handler text | only accept text with a non-ASCII character in it |
| pinyin letters leaked before the commit | the first letters arrive before the composition starts | swallow keys while composing, and retract the ones already sent when the composition starts |

`REXSHELL_DEBUG=1` logs each commit, and F12 exercises the commit path without
an IME, which is how this was verified.

## Agent status

Programs that speak the Program Status Protocol (OSC 7501) — codex, claude, pi,
terraform and anything else that adopts it — report what they are doing, and the
server keeps the record. The shell reads it from `BlockState::agent`:

| State | Dot |
| --- | --- |
| idle | white |
| working | green |
| blocked | red |
| error | amber |
| done | blue |

The sidebar opens with an **AGENTS** section, ordered most urgent first by the
engine (`AppState::attention`), listing every pane that reports a status with its
app name, state word, and — when blocked — what it is waiting for:

```
AGENTS
  ● claude   blocked · permission
  ● codex    working · Demo
SESSIONS
  ● Demo  3
```

Clicking a row jumps to that pane: it selects the pane's server, session and
window, and hands it focus. Below it, sessions aggregate with the most urgent
state among their panes (GPUI's `max()` over the state order), so a session with
one blocked agent shows red even while three others are idle, and each tab chip
carries the same dot so a split still says which pane needs you.

Nothing reports by itself: an agent has to emit the sequence. `rex skill setup`
on the server installs that for the harnesses Rex knows (claude, codex, opencode,
pi); anything else can print it, e.g.
`printf '\033]7501;state=working:app=codex\033\\'`.

## Background picture

Settings → **Background** → *Choose…* picks an image (png, jpeg, webp, gif) that
is drawn **once, across the whole terminal area** — not once per pane — under the
text, cover-fitted and clipped to the terminal so the title bar and its buttons
stay visible.
**Picture opacity** decides how much of it survives: the terminal paints its own
colour over the picture at `1 - opacity`, so 35% leaves a clear watermark and 80%
is nearly the picture with text on top.

Two details that were wrong at first and are worth keeping:

- The renderer takes **BGRA, premultiplied**, not RGBA. Feeding it RGBA swaps red
  and blue, which turned a blue picture brown — that was the "colour difference".
- `paint_image` escapes the element bounds unless it is wrapped in
  `paint_layer(bounds, ..)`. Without the clip it painted over the title bar and
  hid the window buttons.

Decoded once into a `RenderImage` (`background.rs`). Clearing the row removes it
and returns the flat finish.

## Terminal theme## Input methods

Chinese, Japanese and Korean text never arrives as a keystroke: the IME hands the
composed string to the focused element's *input handler*. So `TerminalView`
implements `EntityInputHandler` and registers it during paint with
`window.handle_input`; `replace_text_in_range` types the committed text into the
pty with `session.send_text`.

Three things had to line up:

| Symptom | Cause | Fix |
| --- | --- | --- |
| Composed text never appeared | nothing registered an input handler, so the IME had nowhere to deliver | `EntityInputHandler` + `handle_input` in the paint callback |
| Every ASCII character arrived twice | with a handler registered, GPUI delivers characters *both* as keys and as input-handler text | only accept text with a non-ASCII character in it |
| pinyin letters leaked before the commit | the first letters arrive before the composition starts | swallow keys while composing, and retract the ones already sent when the composition starts |

`REXSHELL_DEBUG=1` logs each commit, and F12 exercises the commit path without
an IME, which is how this was verified.

## Agent status

Programs that speak the Program Status Protocol (OSC 7501) — codex, claude, pi,
terraform and anything else that adopts it — report what they are doing, and the
server keeps the record. The shell reads it from `BlockState::agent`:

| State | Dot |
| --- | --- |
| idle | white |
| working | green |
| blocked | red |
| error | amber |
| done | blue |

The sidebar opens with an **AGENTS** section, ordered most urgent first by the
engine (`AppState::attention`), listing every pane that reports a status with its
app name, state word, and — when blocked — what it is waiting for:

```
AGENTS
  ● claude   blocked · permission
  ● codex    working · Demo
SESSIONS
  ● Demo  3
```

Clicking a row jumps to that pane: it selects the pane's server, session and
window, and hands it focus. Below it, sessions aggregate with the most urgent
state among their panes (GPUI's `max()` over the state order), so a session with
one blocked agent shows red even while three others are idle, and each tab chip
carries the same dot so a split still says which pane needs you.

Nothing reports by itself: an agent has to emit the sequence. `rex skill setup`
on the server installs that for the harnesses Rex knows (claude, codex, opencode,
pi); anything else can print it, e.g.
`printf '\033]7501;state=working:app=codex\033\\'`.

## Background picture

Settings → **Background** → *Choose…* picks an image (png, jpeg, webp, gif) that
is drawn **once, across the whole terminal area** — not once per pane — under the
text, cover-fitted and clipped to the terminal so the title bar and its buttons
stay visible.
**Picture opacity** decides how much of it survives: the terminal paints its own
colour over the picture at `1 - opacity`, so 35% leaves a clear watermark and 80%
is nearly the picture with text on top.

Two details that were wrong at first and are worth keeping:

- The renderer takes **BGRA, premultiplied**, not RGBA. Feeding it RGBA swaps red
  and blue, which turned a blue picture brown — that was the "colour difference".
- `paint_image` escapes the element bounds unless it is wrapped in
  `paint_layer(bounds, ..)`. Without the clip it painted over the title bar and
  hid the window buttons.

Decoded once into a `RenderImage` (`background.rs`). Clearing the row removes it
and returns the flat finish.

## Terminal theme## Input methods

Chinese, Japanese and Korean text never arrives as a keystroke: the IME hands the
composed string to the focused element's *input handler*. So `TerminalView`
implements `EntityInputHandler` and registers it during paint with
`window.handle_input`; `replace_text_in_range` types the committed text into the
pty with `session.send_text`.

Three things had to line up:

| Symptom | Cause | Fix |
| --- | --- | --- |
| Composed text never appeared | nothing registered an input handler, so the IME had nowhere to deliver | `EntityInputHandler` + `handle_input` in the paint callback |
| Every ASCII character arrived twice | with a handler registered, GPUI delivers characters *both* as keys and as input-handler text | only accept text with a non-ASCII character in it |
| pinyin letters leaked before the commit | the first letters arrive before the composition starts | swallow keys while composing, and retract the ones already sent when the composition starts |

`REXSHELL_DEBUG=1` logs each commit, and F12 exercises the commit path without
an IME, which is how this was verified.

## Agent status

Programs that speak the Program Status Protocol (OSC 7501) — codex, claude, pi,
terraform and anything else that adopts it — report what they are doing, and the
server keeps the record. The shell reads it from `BlockState::agent`:

| State | Dot |
| --- | --- |
| idle | white |
| working | green |
| blocked | red |
| error | amber |
| done | blue |

The sidebar opens with an **AGENTS** section, ordered most urgent first by the
engine (`AppState::attention`), listing every pane that reports a status with its
app name, state word, and — when blocked — what it is waiting for:

```
AGENTS
  ● claude   blocked · permission
  ● codex    working · Demo
SESSIONS
  ● Demo  3
```

Clicking a row jumps to that pane: it selects the pane's server, session and
window, and hands it focus. Below it, sessions aggregate with the most urgent
state among their panes (GPUI's `max()` over the state order), so a session with
one blocked agent shows red even while three others are idle, and each tab chip
carries the same dot so a split still says which pane needs you.

Nothing reports by itself: an agent has to emit the sequence. `rex skill setup`
on the server installs that for the harnesses Rex knows (claude, codex, opencode,
pi); anything else can print it, e.g.
`printf '\033]7501;state=working:app=codex\033\\'`.

## Background picture

Settings → **Background** → *Choose…* picks an image (png, jpeg, webp, gif) that
is drawn **once, across the whole terminal area** — not once per pane — under the
text, cover-fitted and clipped to the terminal so the title bar and its buttons
stay visible.
**Picture opacity** decides how much of it survives: the terminal paints its own
colour over the picture at `1 - opacity`, so 35% leaves a clear watermark and 80%
is nearly the picture with text on top.

Two details that were wrong at first and are worth keeping:

- The renderer takes **BGRA, premultiplied**, not RGBA. Feeding it RGBA swaps red
  and blue, which turned a blue picture brown — that was the "colour difference".
- `paint_image` escapes the element bounds unless it is wrapped in
  `paint_layer(bounds, ..)`. Without the clip it painted over the title bar and
  hid the window buttons.

Decoded once into a `RenderImage` (`background.rs`). Clearing the row removes it
and returns the flat finish.

## Terminal theme

The terminal is themed to the same palette as the chrome rather than keeping the
server's Ghostty defaults: `THEME` in `crates/app/src/terminal.rs` sets the
foreground, background, cursor (the amber accent) and all sixteen ANSI colours
from the shell's hues, and is pushed with `TerminalSession::set_theme`. The pane
background is painted translucent, so the terminal sits on the same sheet as
everything else.

## What works

- Sidebar of servers → sessions; tab bar of windows; panes laid out from the
  server's own geometry, including floats and zoom.
- Live terminal output via the `rex.data.terminal.v1` stream: the Ghostty
  snapshot is restored, then raw output is applied in order.
- Keyboard input (`client.key`), local echo off, ctrl/alt/shift/meta passed
  through. The window tells the server its cell grid and takes ownership of a
  terminal's size.
- Reconnect after a drop: the engine resumes the stream and forces a repaint.
- Window drag, minimize, maximize/restore and close — verified by moving the
  window over synthetic mouse input and reading its rect back.
- Splits and floats — verified end to end: `ctrl-shift-d` added a second block to
  the window, `ctrl-shift-f` moved it into a `floating` layer at the bounds the
  shell asked for.
- Floating pane drag and resize — verified against the server: dragging the
  title bar moved the layer from `[0.14, 0.12, …]` to `[0.28, 0.30, …]`, and
  dragging the corner grip resized it to `[0.148, 0.112, 0.48, 0.447]` with the
  position unchanged.
- Settings page: font size (live on every open terminal), sidebar, finish; all
  saved and reloaded from the engine's store.

## What is not there yet

Deliberately, so the first version stays readable:

- **Blinking cursor and inverted cursor cells.** The cursor is a quad painted
  behind the text, so the glyph stays visible instead of inverting.
- **Mouse selection, scroll wheel, scrollback gestures.** Scrollback exists in
  the engine; the shell paints only the viewport.
- **Cell-accurate wide glyphs.** Each run is shaped as one line, so a long line
  mixing CJK/emoji with ASCII can drift off the grid. `Run::widths` carries the
  per-grapheme cell widths when that matters.
- **Add/remove server UI.** Set an address on the command line or let it save
  from the last run.
- Notifications, agent "needs you" inbox, file viewer, themes, font settings.
- The cursor is a solid block behind the glyph and does not blink, so on the
  last row it can look clipped at the pane edge.
- Very fast synthetic input (a `SendKeys` burst) can interleave; typing by hand
  is clean.

Set `REXSHELL_DEBUG=1` to log one line per rendered frame — the quickest way to
tell a dead stream from a failed snapshot restore.

## Verification

- `cargo test -p rexshell-engine` — passes (grid arithmetic; the engine wrapper
  is type-checked against real `shrew-core`, including the Ghostty build).
- `cargo check -p rexshell --target x86_64-pc-windows-gnu` — clean, with Zig as
  the C cross-compiler.
- Built and run on Windows 11 / MSVC 14.41 / Rust 1.99.1: the process opens a
  window and registers as a Rex client (`rex client ls` shows
  `rexshell 0.1.0  app  anonymous over tcp`).
- **Not eyeballed yet**: the desktop was locked during the first run, so
  font metrics, DPI scaling and the wide-glyph drift are still unconfirmed.
