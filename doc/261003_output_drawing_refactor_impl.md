# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping.

## Close the implementation↔design deltas (before step 5)

Three gaps, each its own commit; all three have landed.

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
  the `Main` write's bytes and hands the write on **empty**, so Draw stays capture-agnostic and the
  cursor advance + choreography still run (the row count is taken before the empty — see below).
  `Meta` is never captured.

### Landed — `ContentWrite { bytes, stream }`; Render owns the row count and width

The payload's `rows` member is gone: the `\n` split is the row structure, so a carried count was a
second source of truth. `render(Some(w))` repairs each `Main` write against the watched width — forces
the line terminator a block owes, re-wraps any row over `term_w` re-arming the clear on each
continuation (`\n\x1b[K`) — then reads the row count off the result, before a capture empties the
bytes. Content's `RowCounter` leaves the pipeline (`WrapLines → LineClear → buffer`). `Draw::draw`
takes the count as a parameter. `Meta` stays raw. (payload + enforcement `0761231`; width move
`c515a5e`)

### Deferred — the pad fallback

Padding a short row to `term_w` fully overwrites the cell row it lands on, but it is redundant while
Draw's per-row entry `EL` is trusted; it is a fallback for a stream where clearing proves unreliable,
**not** a second half of the width requirement. Add it only if such a stream appears.

### Follow-up — track the repairs back into content

Render repairs two broken conventions: a missing line terminator, and a row over the watched width.
Both are content's to keep — this repo already produces complete, pre-wrapped, `\n`-terminated lines,
so the enforcement is a net, not the plan. Find the producers that trip it and fix them in content;
leave the net.

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
  of this refactor, not a separate bug. Tackle on its own, beside or after step 5.
- **Watch item — a content row of exactly `term_w`.** With clearing trusted this is inert: the entry
  `EL` erases the old row regardless. It matters only where the **pad fallback** is in use — a padded
  row fills the last cell with no byte beyond it, so on a terminal whose deferred wrap wants a
  following byte it is the edge to confirm end-to-end. `pad_row` fills to `term_w` for the same
  reason.

## Learnings

Decisions not obvious from the code or the design.

- **Width enforcement and padding solve different problems.** Clearing is per *terminal line*, so the
  only thing that defeats it is a row wide enough to wrap — that is the leak, and width enforcement
  (never exceed `term_w`) is the fix. Padding to `term_w` is redundant while the entry `EL` is
  trusted; it is a fallback for when clearing is not effective, not a second half of the requirement.
- **Resize / caret reach who they reach.** Resize travels both the `size` watch and the `input`
  channel; caret invalidation travels the manager only, drained lazily at the prompt loop's top
  (`poll_caret`), not inside `render()`.
- **The `/session` listing memoizes on an mtime-driven, monotonic repo generation** (no time window);
  `/tree`/`/sandbox` rely on the engine's `same_content` fallback instead of a node-level memo.
