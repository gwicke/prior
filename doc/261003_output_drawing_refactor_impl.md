# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping. Do not refer to this document from code comments.

## Remaining: Step 5 — delete `output.rs`

Remove the sinks, the dead taps (`LogOutput`), and the `Output` trait; prune `lib.rs` re-exports.

- **Drop `AgentConfig.output`** → the design's `AgentConfig { content, confirm_port }`.
  `AgentConfig::content_for` and `ConsoleOutput::content()` exist only for the dual field and go
  with it, as does `ConsoleOutput` itself — ending the `Arc<Content>` sharing.
- **Re-shape the tap.** `Content::tap` is `Option<Arc<dyn Output>>`; make it a verb recorder
  (`FnMut`/sink) or fold it into capture. `content_for` then reduces to a quiet-vs-record switch;
  `RecordingOutput`/`ScriptOutput` lose their content duties.
- **Retire the `Output`-shaped sinks.** `QuietOutput`/`NoOutput`/`RecordingOutput`/`ScriptOutput`
  remain only for `content_for` + the frame handle; with the tap re-shaped content is covered and
  the frame is gone. `LogOutput` (dead, zero users) dies here regardless.
- **Verify capture** stays at the `ContentWrite` hand-off: content bytes only, never frames. Nothing
  to migrate; confirm as the sinks die.
- **Degradation's output half** (design §TermIO): non-tty primaries drop frames wholesale at Draw
  (done); the `StripAnsi` edge strip (CSI/OSC-8, zero-width) is not implemented and is claimed by no
  step. Natural home: step 5, beside the sink dissolution.

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
