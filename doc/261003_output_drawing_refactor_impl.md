# Output drawing refactor: remaining work and notes

Ephemeral companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md)
(the authoritative design). `git log` and `doc/todo.md` record what is done; this file tracks only
what remains and the decisions worth keeping. Do not refer to this document from code comments.

## Remaining: Step 5 — delete `output.rs`

The end state is a `Content` built directly (`Content::new` in production, `Content::quiet` for
`--quiet`, `Content::with_tap` in tests) that the agent and its guards hold; `output.rs` and its
trait dissolve. Landed as commits, each compiling and green on its own.

### 5a — the drawing half of `Output` retires
`Output::draw_frame` (default + `ConsoleOutput`'s forward), `begin_batch`/`end_batch` and
`BatchGuard` are dead: the manager already owns all of them (`RenderManager::draw_frame`,
`begin_batch`/`end_batch`, `ContentBatchGuard`). Remove the three verbs and the guard; the few test
callers that used the `Output` forward reach the hub directly (`content.manager.lock().draw_frame`,
`ContentBatchGuard::open`). No behavior change. Also drop `display_tool_error` (a default impl with
no production caller, an `agent.rs` test only). The trait is left with content verbs +
`confirm`/`confirm_default`.

### 5b — `Confirmation` out of `output.rs`; the no-port policy stands alone
`Output`'s confirm pair is the *no-pump* policy (a REPL confirmation is the port's now), reached only
by `Guard` when `confirm_port` is `None`. Move `Confirmation` beside `ConfirmDecision`; give the
Guard a `ConfirmPolicy` value rather than an `Output`. See the open question below on what that
policy resolves to. `Output` then carries content verbs only.

### 5c — the tap narrows to a content-only seam
`Content::tap` is `Option<Arc<dyn Output>>` but is used only for content verbs; give it a
content-only trait (in `content/`) and implement it for the quiet / record / test sinks. `Content`
stops importing `crate::output`. `NoOutput`/`LogOutput` (dead) die here.

### 5d — `AgentConfig.output` drops; the agent + guards hold `Content`
`content_for` dies; every caller builds its `Content` directly (production `Content::new`, quiet
`Content::quiet`, tests `Content::with_tap`). `ConsoleOutput` dissolves into `Content` plus a public
`hub()`; `confirm_probe`, the examples, `main.rs` and the test harnesses (`for_test_with_output`, the
`ScriptOutput` doubles, `repl`'s `commit_harness`) migrate. Confirm capture stays at the
`ContentWrite` hand-off — content bytes only, never frames — as the sinks die.

### 5e — delete `output.rs`; prune `lib.rs`
Remove the module and re-export the survivors (`Confirmation`/`ConfirmPolicy` from `confirm`, the
sink types from `content`).

### 5f (independent) — the non-tty `StripAnsi` edge strip
Design §TermIO: a `StripAnsi` writer wrapper (per stream from caps) strips CSI/OSC-8 at the very
edge, zero-width, so geometry is untouched; non-tty primaries additionally drop frames wholesale at
Draw (done). Not implemented, claimed by no earlier step. Carve out here or defer.

### Open questions
- **the no-port confirm policy.** Today it is per-consumer: `QuietOutput` asks on stdin,
  `ConsoleOutput` denies, the test doubles answer a fixed value. Design §UI says batch / one-shot
  gated tools *require yolo*, which argues for a flat **deny**. That drops quiet's stdin ask — a
  behavior change to confirm before landing.
- **the seam's shape.** A trait (minimal edits to the test doubles) vs a closed enum
  (`Live | Quiet | Record(..)`, matching the TermIO/Draw style). The `ScriptOutput` double wants a
  queued `get_user_input` *and* a recorded buffer, which an enum models awkwardly — leaning trait.

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
