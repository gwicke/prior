# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping. Do not refer to this document from code comments.

## Follow-ups

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
- **Non-tty width (CLI)** is independent geometry — the `term_w` hard requirement exists only so a
  wrapped row cannot defeat the per-row entry `EL`, and a pipe has no screen. Accepted for now: the
  80 fallback (`TermSize::default` via `probe`'s `Err` arm); make it configurable later if wanted.

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

- **Terminal teardown is owned by `run_interactive`'s exit, not `TermGuard`'s `Drop`.** The drop
  backstop is unreachable at process exit: the persistent stdin reader holds an `Arc` clone for the
  process lifetime, and `main` returning kills the thread without unwinding it. The prompt loop's
  exit therefore calls the front door (`suspend`) explicitly; the panic hook reaches the same guard
  through `termio::emergency_stop` (a process-global registered by `TermIO::tty`), and
  `main`'s `process::exit` path calls it too since exit skips destructors. Ctrl-C never needed this:
  the reader stops the guard inline before exiting. `tests/quit_restores_cooked_mode.rs` pins it on
  a real pty.
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
- **Degradation keys on caps; a pinned double makes it deterministic.** The strip and the frame drop
  read caps, so a recording double's **caps** decide whether it degrades: `recording_with(screen
  caps)` keeps it inert (the byte-edge suite sees composed bytes regardless of the runner's
  terminal), `recording_with(pipe caps)` exercises the degradation. The one non-real double left is
  `null` (a sinkhole), which does not degrade.
