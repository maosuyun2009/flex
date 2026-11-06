# Rex client implementation status

## Baseline

- Client baseline: `d2e7ee9` (Flex 0.0.6).
- Upstream shrew source: `9cfc45ff91ab1d2283f7514f8a5d0096a0d68690`.
- Tested server: Rex `1b6654811346488b696ded8f3c7e6701c57242f1`, API revision 10.
- Client now negotiates revision 8–10. Older servers can reject newer methods;
  their errors must remain visible rather than silently using local-only state.
- Local SDK patches and license: `vendor/shrew/`.

## Implemented

| Area | Implementation |
| --- | --- |
| Existing correctness bugs | Server-scoped session selection/deletion, persistent client labels, split direction, authoritative window/block navigation, retained float restore geometry, per-server caches |
| Search/input | Search IME/paste routing, canceled-search late commit guard, named-key filtering, overlapping matches, bounded/deduplicated page scanning |
| Keyboard | Server default registration, logical sequences, forwarding prefix metadata, key modes, once/exclusive/blocked behavior, actual press/repeat/release, FIFO server actions |
| Mouse | Ordered mouse press/release/move/drag and wheel; Shift bypass for local selection |
| Terminal lifecycle | Visible connection/exit status, stop input/stream tasks on view release, server-confirmed ownership, explicit request/take/release |
| Session/server management | Session rename/order; saved server edit, reconnect/probe, configuration reload; host creation; Tailscale login/enable/disable/logout/rename |
| Detached blocks | Enumerate server state, recover after application restart, retain server/session identity, create an anchor when the session has no placed panes |
| Floating layers | Authoritative minimize/restore, close-layer confirmation, preserved layer IDs/state/bounds, shared ordering/zoom rules for unsupported creators |
| Server action palette | Titled Lua/creator actions; raw argument schemas preserved; JSON-object prompt for parameterized actions, no args for fixed creator actions |
| Lua scripts | File selection only stages UTF-8 content; explicit load/unload, diagnostics, persistent client identity, reconnect replay |
| Block creators | Discover flavors, create with JSON options, server schema validation, selectable unsupported-renderer placeholders, block description |
| Remote actions | Default-off consent, explicit action/target validation, per-server queue, awaited RPC result, original-connection replies; clipboard/prompts/tool launchers refused |
| Cross-server relay | Saved connected destinations only; read-only status/whoami/session list/view; other methods refused |

## Verification performed

Windows toolchain through `build.bat`:

```bat
build.bat test --workspace --offline
```

The suite covers the application, engine, and three patched shrew crates.
The two engine real-server tests are ignored unless requested explicitly.
Upstream `shrew-core/tests/live.rs` skips its external operations unless
`SHREW_TEST_SERVER` is supplied; a nominal pass without that variable does not
count as a live-server check.

Also executed separately against temporary, loopback-only Rex servers with
isolated state directories and `--restore=false`:

1. `rex_integration`: persistent client label, Lua script load/unload, action
   registration, inbound `client.perform`, same-connection reply, one-shot reply
   route and revision-9 layer-method availability.
2. `rex_layout`: temporary session creation/rename/destruction, splits, floating
   layer minimize/restore, detach/list/reattach and creator discovery.
3. Upstream `live`: terminal snapshot/output, input, splits, Ctrl-C, palette action,
   background connection recovery and terminal close.

Each server was terminated after its test. No existing server configuration or
user sessions were used. Full Windows interactive GUI operation has not been
validated by these headless checks.

## Deliberate limits / follow-up

- Non-terminal plugins have discovery, creation, inspection and layout support,
  not custom content renderers. There is no generic renderer implied by a schema.
- GPUI 0.2.2 exposes logical keys, not physical scan codes. `[KeyD]` bindings are
  unsupported; no guessed keyboard-layout mapping is used.
- Advanced glyph clusters, blinking/inverted cursor, full OSC 8 metadata, terminal
  image protocols and native OS notifications remain unimplemented renderer work.
- IME rollback outside the search field remains the existing heuristic. A real
  Windows IME/raw-mode TUI test is still required before claiming full correctness.
- Remote automation is intentionally narrower than local UI actions. Refusal is
  explicit, and read/write access to the local clipboard is never granted by the
  remote-action switch. Cross-server writes and arbitrary endpoints are refused.
- A deadline cannot undo an RPC already accepted by Rex. Timeout replies must not
  be treated as proof that no server-side effect happened.
- Some legacy convenience methods in shrew retain upstream's ephemeral fallback
  while offline. Identity-scoped raw calls, scripts and replies use only the
  persistent supervised connection.
- Server theme following, remote selection/URL event metadata and per-session
  attach/detach controls are not full parity features in this iteration. They
  must not be described as covered merely because basic terminal streaming works.
