# Synthetic pi scrolling trace

Captured from the installed `@earendil-works/pi-tui` `TuiAltScreen` renderer.
No user conversation, account settings, credentials, model call or network was
used. Terminal size is 60 columns × 12 rows.

Sequence:

1. Render 60 document lines. Lines 0–24 have a green background and contain
   `OLD TOOL OUTPUT <n>`; lines 25–59 contain `Latest reply line <n>`.
2. Call `scrollBy(-40)` and render.
3. Append `NEW OUTPUT WHILE SCROLLED UP` to the document and render without
   changing its scroll position.
4. Call `scrollToBottom()` and render.

Each `.vt` file contains the actual bytes emitted by that render. Each `.txt`
file is the renderer's expected screen (`getScreenLines()`, terminal escapes
removed and line-end whitespace trimmed). The regression test breaks each VT
frame into 17-byte packets and requests a client frame after every packet.

The protocol uses `CSI ? 2026 h/l` to wrap each redraw. Rendering intermediate
packets must not expose partially updated rows; the expected complete image
must appear once the hold ends.
