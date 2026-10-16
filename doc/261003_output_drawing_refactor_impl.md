# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping. Do not refer to this document from code comments.

## Remaining: Step 5 — delete `output.rs`

The end state is a `Content` built directly (`Content::new` in production, `Content::quiet` for
`--quiet`, a live `Content` + capture in tests) that the agent and its guards hold; `output.rs` and
its trait dissolve. Landed as commits, each compiling and green on its own.

### 5d — the tap seam dissolves; `AgentConfig.output` drops
Capture is Content's now, so the `tap: Option<Arc<dyn Output>>` seam is all that stands between
Content and a direct build. Fold its two remaining jobs into Content and delete the seam:

- **quiet** — a `Content` flag: suppress the hub hand-off, route `display_error` to stderr
  (`Content::quiet`); and
- **the non-REPL input source** — production reads stdin, tests inject a script.

That deletes `Content::with_tap`, `RecordingOutput`, `NoOutput`/`LogOutput` (dead), and
`content_for`; every caller builds its `Content` directly (production `Content::new`, quiet
`Content::quiet`, tests a live `Content` read through `begin_capture`). `ConsoleOutput` dissolves
into `Content` plus a public `hub()`; `confirm_probe`, the examples, `main.rs` and the test harnesses
(`for_test_with_output`, the `ScriptOutput` doubles, `repl`'s `commit_harness`) migrate;
`AgentConfig.output` drops.

### 5e — delete `output.rs`; prune `lib.rs`
Remove the module; `Confirmation` is already gone and the sinks are content's (5c), so `lib.rs` loses
the whole `output` re-export line.

### 5f (independent) — the non-tty `StripAnsi` edge strip
Design §TermIO: a `StripAnsi` writer wrapper (per stream from caps) strips CSI/OSC-8 at the very
edge, zero-width, so geometry is untouched; non-tty primaries additionally drop frames wholesale at
Draw (done). Not implemented, claimed by no earlier step. Carve out here or defer.

## Follow-ups (beyond step 5)

- **`tests/confirm_raw_mode.rs::confirm_dialog_paints_then_clears_with_decision_line` fails.**
  Fallout of this refactor, not a separate bug. Tackle on its own, beside or after step 5.
- **Watch item — a content row of exactly `term_w`.** With clearing trusted this is inert: the entry
  `EL` erases the old row regardless. It matters only where the **pad fallback** is in use — a
  padded row fills the last cell with no byte beyond it, so on a terminal whose deferred wrap wants
  a following byte it is the edge to confirm end-to-end. `pad_row` fills to `term_w` for the same
  reason.

### Follow-up — track the repairs back into content

Render repairs two broken conventions: a missing line terminator, and a row over the watched width.
Both are content's to keep — this repo already produces complete, pre-wrapped, `\n`-terminated
lines, so the enforcement is a net, not the plan. Find the producers that trip it and fix them in
content; leave the net.

### Deferred — the pad fallback

Padding a short row to `term_w` fully overwrites the cell row it lands on, but it is redundant while
Draw's per-row entry `EL` is trusted; it is a fallback for a stream where clearing proves
unreliable, **not** a second half of the width requirement. Add it only if such a stream appears.


## Learnings

Decisions not obvious from the code or the design.

- **Width enforcement and padding solve different problems.** Clearing is per *terminal line*, so
  the only thing that defeats it is a row wide enough to wrap — that is the leak, and width
  enforcement (never exceed `term_w`) is the fix. Padding to `term_w` is redundant while the entry
  `EL` is trusted; it is a fallback for when clearing is not effective, not a second half of the
  requirement.
- **Resize / caret reach who they reach.** Resize travels both the `size` watch and the `input`
  channel; caret invalidation travels the manager only, drained lazily at the prompt loop's top
  (`poll_caret`), not inside `render()`.
- **The `/session` listing memoizes on an mtime-driven, monotonic repo generation** (no time
  window); `/tree`/`/sandbox` rely on the engine's `same_content` fallback instead of a node-level
  memo.
