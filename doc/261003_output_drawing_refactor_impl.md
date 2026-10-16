# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping.

## Close the implementation↔design deltas (before step 5)

Three gaps, each its own commit. Two have landed; the third is the plan.

### Landed — the frame ratio is `f32`

`configure` and `FRAME_MAX_ROW_RATIO` were `f64`; the design's `f32` matches the in-frame
`RowSpec::RatioAndNum`. The `f64` rested on a rounding worry that does not hold — the `f32` product
`ceil`s identically to the exact `f64` for every height in 1..=500, so the pinned `ceil(24·2/3)=16`
stands. (commit `c3a2c13`)

### Landed — `Draw` merges content + frame; the manager prepares the frame

Design §Draw: *Render composes a frame; Draw consumes the live frame, writes content, and applies the
composed frame below it* — a purely downward call, no callback. (commit `da6025d`)

- `RenderManager::render(content)` composes the configured stack (`compose`) into a fresh `Frame` and
  hands it, with the optional `ContentWrite`, to `Draw::draw`. `render(None)` applies the frame alone;
  `render(Some(w))` runs the content choreography.
- Draw's content half: consume the live frame, write the block at the fold, advance the cursor, put the
  composed frame back below. The batch deferral stays in `DrawCache`, emitted once by `end_batch`.
- `put_content`/`reposition_area` are gone. The only thing that had pushed the merge *up* was
  re-composing at reposition; composing the frame *before* the write (the design's shape) needs no
  stack below, so the merge sits at the drawing edge.
- **The capture tap stays in `RenderManager`**, at the content-emit boundary, *not* in Draw: it records
  the `Main` write's bytes and hands the write on **empty** (row count intact), so Draw stays
  capture-agnostic and the cursor advance + choreography still run. `Meta` is never captured.

### Remaining — `ContentWrite` payload + width enforcement in Render

The last delta. Design §Content: *Render re-checks the width and clamps / re-wraps if it disagrees.*
Today content's `WrapLines` is the only guard, at content's width (`max(20)`); Render is to own the
final guarantee. Normally a no-op.

**Payload: `bytes`, newline-split — no `Vec<Range>`.** `ContentWrite` stays `{ bytes, rows, stream }`;
the row structure is the `\n` split.

- *For*: a single source of truth — the bytes **are** the rows. Rows are newline-delimited by
  construction (content ends at a line boundary; `LineClear` arms on `\n`), so a split recovers them
  exactly, in the same O(n) as carrying ranges and with less total work (content builds nothing). And
  the tap needs a count that survives its bytes being emptied — a `rows: usize` supplies it, a derived
  count would not.
- *Against*: the final-unterminated-row case needs a sentence of care; a `Vec<Range>` would make the
  row count unambiguous without it.
- *Net*: `bytes` + `rows`, with `rows` becoming Render-owned (recomputed below).

**Order in `render(Some(w))`** (tap before enforcement):

1. `capture_tap(&w)` — record the `Main` bytes as content emitted them (pre-repair); return swallow.
2. **enforce** — re-check each row against the watched width, repair if needed (below).
3. if swallowing, `w.bytes.clear()` (rows stand) — the screen keeps the content out.
4. `Draw::draw(&frame, Some(&w))`.

**Enforcement**:

- Split `bytes` on `\n`; measure each row. All fit → leave the bytes untouched (the common case).
- Otherwise re-wrap the over-wide rows at the cell edge, re-arming the line clear on every continuation
  (`\n` + `\x1b[K`) so each row is entered by a byte (the overwrite rule, ui_draw_261001). Escapes ride
  whole; SGR survives the break.
- Recompute `rows` from the repaired bytes (`#\n`) — Render owns the count now, so it cannot disagree.
- `Meta` is exempt (raw, never re-wrapped).

**Decisions to settle before writing it:**

- *The width measure.* Content rows carry OSC 8 links, and the two current measures disagree:
  `slashdot_tui::ansi::display_width` is CSI-only (it counts an OSC URL as text — it returns 39 where
  the visible width is 9), while the OSC-aware scanner `row_count::display_width_ansi` lives in
  picocode. The design puts the width utilities in `slashdot_tui`. Proposal: move `display_width_ansi`
  and `wrap_to_width` (with the private `escape_end`/`split_at_width` they need) into
  `slashdot_tui::ansi` as the shared measure, and have `row_count` re-export/delegate — one
  implementation, and `slashdot_tui` gains the OSC-aware width it lacks. This touches imports in
  `row_count`, `ui/bar`, `agent/status`, `content`; confirm before doing it.
- *The leak it is claimed to fix.* The "frame leaks a row when a content row fills the last cell": the
  enforcement closes it by itself if the cause is a row wider than the watched terminal — content's
  `max(20)` floor against a narrower terminal, or a width that raced a resize between content's format
  and this call. Reproduce first (a padded row of exactly `term_w`; a terminal narrower than 20), then
  confirm the re-wrap clears it. If a *fitting* row still leaks, the cause is the exact-fill /
  deferred-wrap margin question — the independent follow-up below, not this enforcement.

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
- **Verify capture** stays at the `ContentWrite` hand-off: content bytes only, never frames. Nothing to
  migrate; confirm as the sinks die.
- **Degradation's output half** (design §TermIO): non-tty primaries drop frames wholesale at Draw
  (done); the `StripAnsi` edge strip (CSI/OSC-8, zero-width) is not implemented and is claimed by no
  step. Natural home: step 5, beside the sink dissolution.

## Follow-ups (beyond step 5)

- **A content row that fills the final terminal cell → the frame leaks a row.** Space-padded rows
  (`content::panel::panel_row`) reach *exactly* `width` columns; at the terminal's own width that fills
  the last cell, and the deferred wrap advances a physical row no byte entered (`row_count.rs`
  documents the same remnant). The enforcement above closes the *over-wide* case; the *exact-fill*
  case is independent — reproduce with a padded row of exactly `term_w`, then decide between extending
  the background with `EL` (BCE) instead of trailing spaces and padding to `width - 1`.
- **Design doc to sync** (authoritative, so edit there, not here):
  - §Render's `ContentWrite { lines, stream }` → `{ bytes, rows, stream }` (payload decision above);
    drop the `type Lines = Vec<String>` alias if it has no other user.
  - §Render's `RenderManager` sketch: add the `captured` tap field, and note `render` composes.
  - §Draw's `Draw::draw(frame, content)` already matches what landed.

## Learnings

Decisions not obvious from the code or the design.

- **The `ContentWrite` payload is `{ bytes, rows, stream }`.** `bytes` exists because it is the single
  source of truth and the tap hands writes on empty; the `rows` count is the piece that must survive
  that, which is why a *derived* count (newline-split alone) is not enough.
- **Resize / caret reach who they reach.** Resize travels both the `size` watch and the `input`
  channel; caret invalidation travels the manager only, drained lazily at the prompt loop's top
  (`poll_caret`), not inside `render()`.
- **The `/session` listing memoizes on an mtime-driven, monotonic repo generation** (no time window);
  `/tree`/`/sandbox` rely on the engine's `same_content` fallback instead of a node-level memo.
