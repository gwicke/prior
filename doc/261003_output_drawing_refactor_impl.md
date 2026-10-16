# Output drawing refactor: remaining work and notes

Ephemeral implementation companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). Steps 1–4 and 4i have landed; `git log` and `doc/todo.md` are the record
of what is done. This file tracks only what remains and a few decisions worth keeping.

## Remaining: Step 5 — delete `output.rs`

Remove the sinks, the dead taps (`LogOutput`), and the `Output` trait; prune `lib.rs` re-exports.

- **Drop `AgentConfig.output`** → the design's `AgentConfig { content, confirm_port }`.
  `AgentConfig::content_for` and `ConsoleOutput::content()` exist only for the dual field and go with
  it, as does `ConsoleOutput` itself — ending the `Arc<Content>` sharing.
- **Re-shape the tap.** `Content::tap` is `Option<Arc<dyn Output>>`; make it a verb recorder
  (`FnMut`/sink) or fold it into capture. `content_for` then reduces to a quiet-vs-record switch;
  `RecordingOutput`/`ScriptOutput` lose their content duties.
- **Retire the `Output`-shaped sinks.** `QuietOutput`/`NoOutput`/`RecordingOutput`/`ScriptOutput`
  remain only for `content_for` + the frame handle; with the tap re-shaped content is covered and the
  frame is gone. `LogOutput` (dead, zero users) dies here regardless.
- **Verify capture** stays at the `ContentWrite` hand-off (re-tapped in 4d): content bytes only,
  never frames. Nothing to migrate; confirm as the sinks die.
- **Degradation's output half** (design §TermIO): non-tty primaries drop frames wholesale at Draw
  (done); the `StripAnsi` edge strip (CSI/OSC-8, zero-width) is not implemented and is claimed by no
  step. Natural home: step 5, beside the sink dissolution.

## Follow-ups (beyond step 5)

- **Content rows that fill the final terminal cell → the frame leaks.** Every *tool-panel* row
  (`content::panel::panel_row`, `tool_row_contents`) is space-padded to *exactly* `self.width()`
  visible columns, and content rows are emitted with a trailing `\n` into scrollback. On a terminal
  whose width equals the watched width that fills the last cell, leaving the cursor in the
  auto-margin; the terminal's deferred wrap then advances a *physical* row that no byte entered, so
  the line-clear (`EL`) never arms on it and the row counter — which counts a `\n` as one row, and
  leaves a line of exactly `width` unwrapped (`row_count::wrap_to_width` breaks on `> width`, not
  `>=`) — disagrees with the screen. Result: the row below is not cleared and the previous frame
  shows through (`row_count.rs` documents the same remnant). Diff rows trigger it *frequently*
  because the full-width grey background puts nearly every row exactly at the margin. Contrast the
  frame side: a short frame row ends with `EL` (`el_if_short`) and never relies on filling the last
  cell. Fix directions: extend the panel background with `EL` (BCE) instead of trailing spaces, so
  the cursor never enters the margin, and/or pad to `width - 1`; assert content width == terminal
  width at the edge. Reproduce with a padded row of exactly `term_w` first.
- **`ContentWrite`'s payload: `bytes` vs `Lines` vs `bytes + Vec<Range>`.** Step 5 keeps `bytes`
  as-is; the question is what the end state should carry. Content *is* line-based, but the line
  structure is resolved *above* the hand-off (`wrap_with_diff_markers`, `panel_row`), and below it
  the only consumer is the byte sink, which needs just the total row count (`rows: usize`) to
  advance the cursor — nothing reads line boundaries. So `Vec<Range>` would carry structure with no
  consumer *today*; it becomes the right shape only once width enforcement moves below the hand-off
  (centralising padding/wrapping in Render, per the design's "Render re-checks the width"
  gesture). Decide where width enforcement lives first; then pick the payload.

## Learnings

Decisions not obvious from the code or the design doc; fold into the design doc where noted.

- **The content + frame merge in the hub is not forced — the design's split is coherent.** Design
  §Draw: *Render composes a frame; Draw consumes the live frame, writes content, and applies the
  composed frame below it* — a purely downward call, no callback. The landed code instead keeps the
  merge steps in `RenderManager::put_content` (Draw exposing `write_bytes`/`write_meta`/`draw_frame`/
  `note_lines`). The only thing that pushes the hero step *up* is the landed choice to **re-compose
  the frame at reposition** (to pick up status/dot changes since the last compose) — re-composition
  needs the stack, which Render owns. The design avoids that by composing the fresh frame *before*
  the write and passing it down. So this is a sequencing/API choice, not a layering constraint;
  reconcile one way or the other (fold consume→write→apply into `Draw::draw(frame, content)` with
  Render composing first, or keep the hub and record why re-compose-at-reposition wins).
- **The `f64` ratio is incidental; the *invariant* is the point.** `2/3` is an arbitrary eyeballed
  constant, so `f32` vs `f64` only matters if a test pins an exact product — precision is beside the
  point here. What matters is the design's single-source rule: one place (`RenderManager::budget`)
  computes the frame row count, so a caller that needs it asks rather than re-deriving. Keep the
  invariant; treat `configure`'s float width as immaterial (sync to `f64` only to match the field).
- **`ContentWrite` is `{ bytes, rows, stream }`.** The `bytes` reason is the byte-level wrap chain,
  *not* non-UTF8 content: every content verb takes `&str`, and tool output is `from_utf8_lossy`-
  decoded upstream (tools.rs), so no non-UTF8 reaches content. Drop that rationale. *(Design
  §Content/§Render still say `{ lines, stream }` — payload shape is the open Follow-up above.)*
- **Resize / caret reach who they reach.** Resize travels both the `size` watch and the `input`
  channel; caret invalidation travels the manager only, drained lazily at the prompt loop's top
  (`poll_caret`), not inside `render()`.
- **The `/session` listing memoizes on an mtime-driven, monotonic repo generation** (no time window);
  `/tree`/`/sandbox` rely on the engine's `same_content` fallback instead of a node-level memo.
