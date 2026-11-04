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
| `crates/app/src/workspace.rs` | The window: sidebar, tab bar, pane area |
| `crates/app/src/terminal.rs` | One block as an entity: the terminal stream, focus, input |
| `crates/app/src/paint.rs` | `shrew_core::Frame` → GPUI quads and shaped text runs |
| `crates/app/src/keymap.rs` | GPUI keystroke → Rex key |

Panes are positioned from `BlockState::rect`, which Rex sends as fractions of
the window, so server-side splits, floats and zoom need no layout code here.

## Build

You need Rust 1.90+, a C toolchain, and **Zig 0.17** on `PATH` (or in `ZIG`):
`shrew-term` builds the real Ghostty VT engine from source.

```powershell
# Windows
rustup default stable
winget install zig.zig          # or unzip a release and add it to PATH
cargo build --release
```

First build is slow: it compiles Ghostty and ~500 crates.

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

## What works

- Sidebar of servers → sessions; tab bar of windows; panes laid out from the
  server's own geometry, including floats and zoom.
- Live terminal output via the `rex.data.terminal.v1` stream: the Ghostty
  snapshot is restored, then raw output is applied in order.
- Keyboard input (`client.key`), local echo off, ctrl/alt/shift/meta passed
  through. The window tells the server its cell grid and takes ownership of a
  terminal's size.
- Reconnect after a drop: the engine resumes the stream and forces a repaint.

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

## Verification

- `cargo test -p rexshell-engine` — passes (grid arithmetic; the engine wrapper
  is type-checked against real `shrew-core`, including the Ghostty build).
- `cargo check -p rexshell --target x86_64-pc-windows-gnu` — clean, with Zig as
  the C cross-compiler.
- **Not yet run on a real Windows machine.** The next step is a real window
  against a live server; expect GPUI font-metric and DPI surprises there.
