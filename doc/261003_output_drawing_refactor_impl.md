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
  on `Main`. `Stream::Meta` goes. `Frame` never crosses the content API. Finer kinds buy future
  flexibility even where two route alike today (`Reasoning` and `Error` do: primary-on-tty,
  meta-on-pipe).
- **TermIO owns kind → sink, from caps.** `Content` → primary; `Reasoning`/`Error` → primary when
  the primary is a terminal, else the meta stream; `Frame` → primary, dropped when the primary is
  not a terminal. The strip stays at the edge (5f), per *target* cap. `Error` on a tty stays on
  stdout (so a piped consumer still sees it), which is today's `Main` behavior — the kind only makes
  the sink a TermIO decision.
- **`Frame` is a Draw → TermIO distinction**, not a `Stream` variant: `TermIO::write_frame` (or a
  two-variant `TermTarget { Content(Stream), Frame }`). Draw's frame-side writes — `plan_consume`'s
  walk, `apply`/`draw_frame`/`reposition`, the caret show/hide, the re-anchor — carry it; the
  content write keeps its kind. Content bytes are never rewritten by the consume, so the drop
  leaves them intact. This also lets the 5f `paints_frames` gate leave `Draw`: TermIO drops `Frame`.
- **Nothing above TermIO needs the mapping.** `repair_main` needs the *width*, not the sink: a
  non-primary (stderr) write occurs only when the primary is not a terminal — exactly when frames
  are dropped and the live-screen cache is already fiction — so the net and the returned row count
  can apply uniformly (the count only matters on a screen, where every write is a screen write). So
  no `Caps::kind_sink` surfaces above TermIO; the mapping is TermIO's alone, and Draw always merges
  (its cache advances against dropped bytes, the fiction 5f accepts).

Open before landing:

- **Recording doubles model real backends.** They should replicate what a tty or a plain backend
  would emit — so a recording backend takes caps and always behaves real (the 5f `degrade` flag
  becomes "always", `recording()` pins tty caps), the byte-edge suite pinning tty caps and the
  degradation tests pinning pipe caps. `Content`'s caps must match for colour and routing, so build
  it over the recording backend rather than swapping the sink after construction.
- **`--quiet` is unaffected** — it drops the hub hand-off entirely; keep it that way.
- **Churn:** `Stream`'s variants are a `termio` re-export, so `ContentWrite.stream`, `repair_main`,
  `Draw::draw`, `write_stdio`, and a large block of `Stream::Main`/`Meta` test assertions move.
  Accepted: a one-off cost, not a reason to keep the sink vocabulary.
- **`TermGuard`** (the first bullet above) folds in here: it is a TermIO concern already (`TtyIo`
  owns it), but the fix keys on the **target** cap, not the backend kind — an interactive stdin can
  have a piped primary (`picocode > out.txt`), which is exactly the leak. Route the guard's CSI
  (bracketed paste, kitty keyboard, cursor) through the same degradation edge, gated on
  `caps.primary`.
- **Non-tty width (CLI)** is independent geometry — the `term_w` hard requirement exists only so a
  wrapped row cannot defeat the per-row entry `EL`, and a pipe has no screen. Accepted: keep the
  80 fallback (`TermSize::default` via `probe`'s `Err` arm) for now; make it configurable later if
  wanted.

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
