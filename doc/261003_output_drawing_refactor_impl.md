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
final guarantee.

**Primary: no row may exceed `term_w`, so none wraps.** Draw clears per *terminal line* — an `EL` on
each row's entry (the overwrite rule, ui_draw_261001) — so the `EL` erases the old frame's row no
matter how short the new content is. Terminal wrapping defeats that: a wrapped continuation is a
terminal line no byte entered, so it never gets its entry `EL` and keeps what the frame left there —
the leak. Fix the wrapping and clear-then-write covers the rest.

**Padding is the fallback, and only where clearing is not effective.** Without clearing, a row padded
to `term_w` is what fully overwrites the old frame content; with reliable clearing, padding is
redundant (the entry `EL` already erased the row). So padding to `term_w` is required *only* if
draw's clearing cannot be trusted for a row / stream — it is **not** a second half of the width
requirement. Width enforcement (re-wrap, never exceed) is the concern; under-full padding rides
behind the clearing guarantee.

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

**Enforcement** (over the watched width `term_w`):

- Split `bytes` on `\n`, keeping the split's trailing empty piece (`a\nb` → `["a","b"]`, `a\n` →
  `["a",""]`): that final empty piece is the row the cursor rests *on*, not a row to repair.
- **Re-wrap (primary):** a non-empty row over `term_w` is re-wrapped at the cell edge
  (`wrap_to_width`), re-arming the line clear on every continuation (`\n` + `\x1b[K`) so each row is
  entered by a byte (the overwrite rule, ui_draw_261001). Escapes ride whole; SGR survives the break.
  Each repaired row now clears on entry, so it needs no padding.
- **Pad (fallback):** only where draw's clearing is not effective, a row under `term_w` is padded to
  exactly `term_w` display columns so it fully overwrites the cell row it lands on (an empty row to a
  full blank row too). Pad *before* a trailing reset, as `pad_row` does; a re-wrapped continuation is
  padded on its own account, only a row's true tail is exempt. Redundant — and skipped — behind a
  trusted entry `EL`.
- Recompute `rows` from the repaired bytes as the newline count — the count `rows_of` would report —
  so Render owns it and it cannot disagree with what it writes; the tap's emptied write still needs it.
- All-fit walks the rows once: a no-op when clearing is trusted, a pad pass when it is not. `Meta` is
  exempt (raw, straight to the meta sink, never touched).

**Settled — the width measure moves into `slashdot_tui`.** Content rows carry OSC 8 links, and the two
measures today disagree: `slashdot_tui::ansi::display_width` is CSI-only (it counts an OSC URL as text
— 39 where the visible width is 9), while the OSC-aware scanner is `row_count::display_width_ansi` in
picocode. The design puts the width utilities in `slashdot_tui`, so the move is the fix, not a
workaround: `display_width_ansi` + `wrap_to_width` (with the private `escape_end` / `split_at_width`
they need) move into `slashdot_tui::ansi`; `row_count` re-exports / delegates so its callers
(`ui/bar`, `agent/status`, `content`) are unchanged — one OSC-aware measure below both layers.

**Settled — the leak is the over-wide wrap defeating the entry clear.** With no row past `term_w`, no
content write can cross onto a row no byte entered, so each row clears on entry and the frame's old
row is erased without padding. Reproduce after the fact with a terminal narrower than content's
`max(20)` floor — the over-wide case the re-wrap closes.

**Build order.** Land the payload change **and the re-wrap** first — that is the fix for the concrete
leak. Add the **pad fallback** only if draw's clearing turns out unreliable for a stream; it is not
part of the width fix and can trail.

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

- **`tests/confirm_raw_mode.rs::confirm_dialog_paints_then_clears_with_decision_line` fails.** Fallout
  of this refactor, not a separate bug. Tackle **after** the cleanup deltas (the Render enforcement
  above, then step 5), on its own.
- **Watch item — a content row of exactly `term_w`.** With clearing trusted this is inert: the entry
  `EL` erases the old row regardless. It matters only where the **pad fallback** is in use — a padded
  row fills the last cell with no byte beyond it, so on a terminal whose deferred wrap wants a
  following byte it is the edge to confirm end-to-end. `pad_row` fills to `term_w` for the same
  reason.
- **Design doc to sync** (authoritative, so edit there, not here):
  - §Content's `ContentWrite { lines, stream }` → `{ bytes, rows, stream }` (payload decision above);
    drop the `type Lines = Vec<String>` alias if it has no other user. Its "clamps / re-wraps"
    sentence gains the *pad to fill* half.
  - §Render's `RenderManager` sketch: add the `captured` tap field, and note `render` composes.
  - §Draw's `Draw::draw(frame, content)` already matches what landed.
  - §End state layering's "the width utilities … live in their own crate" gains the OSC-aware
    `display_width_ansi` beside `display_width`.

## Learnings

Decisions not obvious from the code or the design.

- **The `ContentWrite` payload is `{ bytes, rows, stream }`.** `bytes` exists because it is the single
  source of truth and the tap hands writes on empty; the `rows` count is the piece that must survive
  that, which is why a *derived* count (newline-split alone) is not enough.
- **Width enforcement and padding solve different problems.** Clearing is per *terminal line*, so the
  only thing that defeats it is a row wide enough to wrap — that is the leak, and width enforcement
  (never exceed `term_w`) is the fix. Padding to `term_w` is redundant while the entry `EL` is
  trusted; it is a fallback for when clearing is not effective, not a second half of the requirement.
- **Resize / caret reach who they reach.** Resize travels both the `size` watch and the `input`
  channel; caret invalidation travels the manager only, drained lazily at the prompt loop's top
  (`poll_caret`), not inside `render()`.
- **The `/session` listing memoizes on an mtime-driven, monotonic repo generation** (no time window);
  `/tree`/`/sandbox` rely on the engine's `same_content` fallback instead of a node-level memo.
