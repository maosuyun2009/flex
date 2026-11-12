# Local shrew compatibility patch

Source: [teddytennant/shrew](https://github.com/teddytennant/shrew), commit
`9cfc45ff91ab1d2283f7514f8a5d0096a0d68690` (MIT; see `LICENSE`).

Only `shrew-core`, `shrew-client`, `shrew-proto`, and the small `shrew-term`
patch are copied here. The root
Cargo workspace patches these packages, so Windows builds and CI use the same
sources. `shrew-term` and Ghostty remain pinned upstream; no Cargo cache edits
are required. Vendored package metadata is explicit rather than inherited from
upstream's workspace.

Local changes:

- Raw RPC calls use the supervised, persistent connection, preserving client
  identity, scripts, action registration, and reply routing.
- Successful client label/script/keymap setup is replayed on reconnect. Scripts
  are not run twice when the server resumes the previous connection identity.
- Client perform/cross-server request events retain their original connection
  for replies. The application decides whether to execute them; the engine
  never automatically connects to a requested address.
- Snapshots preserve non-terminal creators and layer identifiers/state/bounds.
  Terminal metadata is applied only to terminal blocks.
- Terminal input and control requests are ordered. Closing a terminal stops its
  input and stream tasks rather than leaving an Arc/task retention cycle.
- `shrew-term` keeps the caret cell in `Frame::cursor_position` even when a
  program hides the cursor, so an IME can place its preedit and candidates.
- Synchronized output (DEC 2026) captures the last complete screen through
  Ghostty's render-hold callback. Partial redraw packets are not presented.
  A one-second deadline wakes the renderer even if output stops; reconnect,
  stream termination, resize and snapshot restoration release the old hold.
  Synthetic pi scrolling traces are replayed in `shrew-term/tests/pi_scroll.rs`.
- Ownership claims are not treated as successful before the server replies.

The control handshake advertises revision 10 with minimum revision 8. An
isolated live-server test first reproduced `revision_too_low` for layer-state
operations at revision 8, then passed after the revision upgrade. Live checks
cover persistent labels, scripts, action registration/replies, session and
block lifecycles, detached blocks and minimized layers. This does not certify
every revision-10 feature; test against the intended server version.

## Verification

From a Windows developer shell:

```bat
build.bat test --workspace --offline
```

The root engine has an opt-in real-server test:

```bat
set FLEX_TEST_SERVER=http://127.0.0.1:17979
build.bat test -p flex-engine --test rex_integration --test rex_layout --offline -- --ignored
```

Use only a disposable server for integration tests. Ordinary unit tests do not
start a server. Upstream's `shrew-core/tests/live.rs` only runs its external
operations when `SHREW_TEST_SERVER` is explicitly set.

When upgrading upstream, compare these directories against this recorded commit
and replay only patches still needed. Do not replace them with the latest source
without running the workspace and live protocol tests.
