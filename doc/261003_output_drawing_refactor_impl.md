# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping. Do not refer to this document from code comments.

## Follow-ups

- **`TermGuard`'s mode sequences bypass the degradation edge.** It writes the bracketed-paste /
  kitty-keyboard / cursor CSI straight to `io::stdout()`, not through [`TermIO::write`], so a tty
  *stdin* with a piped primary (`picocode > out.txt`) still leaks those escapes into the file. Raw
  mode only matters for an interactive stdin, and the leak is a handful of bytes at entry/exit, so
  it was left out of 5f — but it is the one remaining path that reaches a non-terminal primary
  unstripped.
- **`tests/confirm_raw_mode.rs::confirm_dialog_paints_then_clears_with_decision_line` fails.**
  Fallout of this refactor, not a separate bug. On resolve the dialog's divider row survives: the
  assertion sees the header/options cleared but a full-width rule still on screen (the decision line
  lands above it). Tackle on its own.
- **`--quiet` with no positional prompt spins** (`doc/todo.md`): `Content::quiet`'s empty input
  source returns `""`, paired with `if input.is_empty() { continue }`.
- **Watch item — a content row of exactly `term_w`.** With clearing trusted this is inert: the entry
  `EL` erases the old row regardless. It matters only where the **pad fallback** is in use — a
  padded row fills the last cell with no byte beyond it, so on a terminal whose deferred wrap wants
  a following byte it is the edge to confirm end-to-end. `pad_row` fills to `term_w` for the same
  reason.

### Next — step 6: output kinds, backend-owned routing

The output vocabulary is a *sink* vocabulary today (`Stream::{Main, Meta}`), so Content decides
routing (`reasoning_on_stderr = !caps.primary`) and Render/Draw special-case `Stream::Meta` (skip
the frame merge; skip width repair). Turn it into a *kind* vocabulary and move the sink decision to
the layer that owns the capability. When it lands, §TermIO/§Render/§Draw of the design doc get the
new vocabulary as end state.

The shape:

- **Content-facing kinds, not sinks.** `Stream { Content, Reasoning, Error }`: Content says *what* a
  write is. `Reasoning` replaces today's "divert to Meta"; `Error` names what is now `SystemMessage`
  on `Main`. `Stream::Meta` goes. `Frame` never crosses the content API.
- **TermIO owns kind → sink, from caps.** `Content`/`Error` → primary; `Reasoning` → primary when
  the primary is a terminal, else the meta stream; `Frame` → primary, dropped when the primary is
  not a terminal. The strip stays at the edge (5f), per *target* cap.
- **`Frame` is a Draw → TermIO distinction**, not a `Stream` variant: `TermIO::write_frame` (or a
  two-variant `TermTarget { Content(Stream), Frame }`). Draw's frame-side writes — `plan_consume`'s
  walk, `apply`/`draw_frame`/`reposition`, the caret show/hide, the re-anchor — carry it; the
  content write keeps its kind. Content bytes are never rewritten by the consume, so the drop
  leaves them intact.
- **No screen query is needed for the merge.** `Reasoning` is emitted exactly when the primary is
  not a terminal — i.e. exactly when frames are dropped — so the walk/reposition around a
  `Reasoning` write are `Frame` bytes TermIO discards. Draw can always merge (its cache advances
  against dropped bytes, the fiction 5f already accepts).
- **One screen predicate, one consumer left.** Render's `repair_main` applies the width net only to
  a write bound for the primary screen; put the rule on `Caps` (`caps.kind_sink(kind)`), which
  TermIO owns and probes, and let Render read it — a query, not a private table.

Open before landing:

- **`Error`'s sink is a decision, not a rename.** Errors are on `Main` today (stdout, so a piped
  consumer sees them); only `--quiet` sends them to stderr, via `eprintln!`. Naming the kind forces
  the choice — keep stdout to avoid a regression, or make it cap-driven.
- **`--quiet` is unaffected** — it drops the hub hand-off entirely; keep it that way.
- **Recording doubles:** record per *kind* or per *sink*? They key on `Main`/`Meta` now, and the
  real-vs-double split (`recording`/`recording_degraded`) must survive.
- **`Reasoning` vs a generic meta:** the only non-primary stream today *is* diverted reasoning, so
  the semantic kind is a clean rename — but `show_reasoning` (whether it renders at all) stays
  Content's, and only the sink moves.
- **Churn:** `Stream`'s variants are a `termio` re-export, so `ContentWrite.stream`, `repair_main`,
  `Draw::draw`, `write_stdio`, and a large block of `Stream::Main`/`Meta` test assertions move.
- **`TermGuard`** (the first bullet above) is the same "TermIO owns output" finding and folds in
  here.
- **Non-tty width (CLI)** is independent geometry — the `term_w` hard requirement exists only so a
  wrapped row cannot defeat the per-row entry `EL`, and a pipe has no screen, so a non-tty width can
  be a configured value (default 80) or unlimited. Do it separately or accept the 80 fallback.

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
- **Tests move with the code they test.** The render test modules lived in `output.rs` only because
  the trait they shared was there; once the trait goes, the suite belongs beside the `Content` it
  drives (`content/handle.rs`) and beside the preview helpers it exercises (`content/preview.rs`).
- **Degradation is a property of a real stream, not of a cap alone.** The strip and the frame drop
  key on caps, but only a *real* backend acts on them: a test double observes the composed bytes, so
  `recording()`/`null()` neither strip nor drop frames. That is what keeps the byte-edge suite
  (which asserts raw ANSI at the recording edge) deterministic regardless of the runner's terminal;
  `recording_degraded(caps)` supplies the real-stream behaviour with pinned caps when a test needs
  to exercise the degradation itself.
