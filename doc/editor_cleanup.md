# Editor cleanup — one wrapper, node-driven editing

Status: **draft proposal** (not started). Companion to `doc/ui_draw_v2.md` §9.

## 1. Goals

- **One** editor entry point that owns the whole terminal turnover: raw-mode off →
  `$EDITOR` → raw-mode on → DSR re-anchor. No call site keeps its own guard/DSR logic.
- Menu endpoints may **opt in** to editing per node; editor support is *not* mandatory
  for generic/library nodes. A **host listing node** (`v25_listing_node.rs`) supplies the
  default listing `edit()` (header + rows, read-only) so every listing level gets Ctrl-E;
  other host nodes override it with their own behavior (bulk edits, confirmations).
- `/tree … compact` moves generate → edit → confirm into the node's
  `prepare()` / `submit()`; the engine's existing two-phase confirm replaces
  `confirm_compaction_summary`.

## 2. The wrapper

New `src/editor.rs`:

```rust
pub(crate) enum EditorMode { Edit, View }   // View = the caller will not persist

pub(crate) fn run_editor(
    guard: &TermGuard,
    out: &dyn Output,
    current: &str,
    mode: EditorMode,
) -> io::Result<String> {
    let _ = guard.stop();
    let r = edit_in_editor(current, mode);
    let _ = guard.start();
    let _ = out.cursor_row();          // DSR re-anchor; invalidates the live frame (§4.2)
    r
}
```

- `EditorMode::View` creates the temp file **mode `0o400`** — the editor sees a read-only
  file (`vim` behaves like `:view`) — and does **not** read back; `run_editor` returns
  `current` unchanged. The permission bit is the portable signal (no per-editor `-R`/`-v`
  flags). A view-only `edit()` therefore cannot persist even if the user force-writes.
- `edit_in_editor` becomes module-private (mechanism only).
- Convenience for host nodes: `AppState::edit_text(&self, current: &str, mode) -> io::Result<String>`
  (pulls guard + output from the slot, calls `run_editor`) — mirrors `term_guard()`.
- Callers after migration: the Ctrl-E prompt arm, host node `edit()` impls.

### 2.1 Naming (output-side API)

Name the *mechanism*, not the caller:

| Today | Problem | Generic name |
|---|---|---|
| `note_editor_return` | editor-specific; Ctrl-Z resume and post-wipe re-anchor want the same op | `refresh_dsr()` — "re-query the cursor via DSR and re-anchor our screen state" |
| `painted_size` | the *name* is fine ("painted" is generic); the editor-specific part was the *fold*, not the name | keep `painted_size()` (or `screen_epoch()` if modelled explicitly) |

If `refresh_dsr()` is exactly what `cursor_row()` already does, **do not add a method** —
call `cursor_row()` (already generic; the editor is just one caller).

### 2.2 Drop the height fold (decided)

The Ctrl-E arm folded an editor-caused height change into `painted_size` (keep width stale →
wipe+replay still fires; height trusted → skip one redundant DSR). With a shared wrapper
that is a call-site-specific claim about engine state, and nodes cannot reach a loop-local
`Cell` anyway. Drop it:

- `painted_size` stays **loop-owned**; the loop sets it after each frame (a fact).
- A foreign write + resize falls into the existing loop-top branch unchanged: width →
  wipe + replay; height-only → one *redundant* DSR. Correct; negligible.
- The fold was a **micro-optimization only** (one saved DSR round-trip in the rare
  height-resize-during-edit case); no correctness path depended on it.

### 2.3 stdin ownership (general reader-gate fix)

The editor owns stdin for the child's lifetime, so the in-flight reader must be paused
across it — and the existing `paused` gate is **never released** (`ui_draw_v2.md` §9 #1).
Fix it generally with a scoped pause:

- `Output::pause_reader(&self) -> ReaderPause`; acquire = `swap(true)` remembering the
  prior value, `Drop` = restore it. Nesting-safe, no gate type change.
- A no-op guard when the output has no gate (non-interactive / no reader).
- `run_editor` holds one for the whole child lifetime; the confirm dialog holds one for its
  lifetime (replacing today's bare `store(true)`). One mechanism, both callers.

### 2.4 `draw_frame` takes the composed dims and reports them (done — §9 #3)

*As implemented.* Rather than both phases re-reading `frame_size()` (a paint-time read
that could land *after* a mid-iteration resize and record post-resize dims — skipping the
next loop-top wipe/re-anchor), the composed `dims` are **passed into** the apply: the
loop's single per-iteration read flows to compose, apply, and the record.

- `Output::frame_size() -> (usize, usize)` — shared source for callers without their own
  read (dialog init); ConsoleOutput: `term_width()`, `term_height()`.
- `draw_frame(&self, frame, anchor, policy, dims) -> (usize, usize)` — applies exactly
  `dims` (clamped) and reports them; the trait default does the same clamping.
- the `redraw` closure records the report into `painted_size`; the loop's own
  `painted_size.set((cur_w, cur_h))` (old `repl.rs:838`) is gone.
- `draw_dialog`/`resolve` pass the dialog's tracked `size`, so a Resize recompose and its
  apply agree too.

## 3. Where editing is driven from

1. **Input buffer** (overlay `None`): the Ctrl-E arm calls `run_editor` directly, then
   `continue`. No node involved.
2. **Menu endpoint** (overlay set): Ctrl-E maps to `HostKey::Edit`; the engine dispatches
   to the selected node (§5).
3. **Inside a workflow** (compaction): the node calls `run_editor` inside `prepare()` (§6).

## 4. Node editing capability — decided shape

- Trait method (library; editor support optional by construction):
  ```rust
  async fn edit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
      SubmitResult::Handled   // default: no-op edit; stay open
  }
  ```
- **Host listing node** `src/v25_listing_node.rs` — app_state-aware, used for
  *all* static-row v25 listing levels in place of the generic
  `StaticListingNode`. Its `edit()` default renders **header + rows** and
  opens them **read-only** (`EditorMode::View`): a portable "inspect this
  endpoint" utility on every listing level.
- **Per-node overrides** do the real work. `SubmitResult` gives `edit()` the full
  vocabulary — including triggering the confirmation dialog (`Prepared`):
  - a **settings listing** may treat the editor as a bulk-edit surface (parse the edited
    file → write → `Prepared` to confirm, or `Done`),
  - a **leaf** edits its parameter/draft text (compaction leaves, §6),
  - view-only / unsupported → the default.
- Host nodes self-serve via `app_state` — **no capability threading**, and the library keeps
  its no-op default. The generic `StaticListingNode` is left untouched (still used by the
  crate's own tests).
- Rejected: a decorator node wrapping `StaticListingNode` (8-method forwarding boilerplate).

## 5. Engine dispatch (`HostKey::Edit`)

- Add `HostKey::Edit` (`node_entry.rs`).
- `NodeEntry::handle_leaf` / `handle_listing`: `HostKey::Edit` bypasses completion/confirm
  (like Enter/Esc) and calls `self.inner.edit(epoch, pa).await`, mapping the result exactly
  as `prepare()`'s is mapped: `Done → DoneSuccess`, `Prepared → confirm_pending`,
  `NewPath → ContinuePrompt`, `Error → error_continue`.
- **`SubmitResult::Handled`** (new): "handled, stay open". `NodeEntry` returns
  `MenuResult::Continue(plan, prompt)` *directly* — the plan for the current level, produced
  by the passive-render path (so it is byte-identical to what a re-eval would yield) —
  instead of `ContinuePrompt(current)`. Behaviourally the same as
  `NewPath(pa.full_prompt())`, minus the host's rewrite → re-eval iteration. Map it in the
  `prepare()`, `submit()`, and `edit()` arms; it is `edit()`'s default.
- Optional companion: in the `NewPath` arm, if the new path equals `pa.full_prompt()`, take
  the same direct-`Continue` path — a general saving for no-op rewrites.
- Adding the variant surfaces every `match` on `SubmitResult` (engine + tests) as a compile
  error — that is the checklist for the engine arms.
- The input-buffer case (overlay `None`) never reaches a node — the REPL handles it directly.

## 6. Compaction via `prepare()` / `submit()`

- Split `run_compaction_at_point` (`agent.rs`) into **generate** (returns the draft;
  LLM = `agent.summarize`) and **apply** (writes the compaction entry from a supplied
  summary).
- `TreeCompact*Node::prepare(pa)`:
  1. resolve mode + target, guard the target (existing checks),
  2. generate the draft (async),
  3. `run_editor(.., EditorMode::Edit)` / `app_state.edit_text(&draft, Edit)` (blocking),
  4. store the edited text in node state,
  5. `SubmitResult::Prepared`.
- Decision via the engine's existing two-phase: `Enter → submit()` (apply from stored
  text); `Esc → confirm_pending=false` + abort. The node `header()` supplies the guidance
  ("Save and close to confirm; discard to cancel.").
- The stale `edited` draft after an abort is overwritten on the next `prepare()`
  (or add an `abort()` hook — see open questions).

## 7. Async? — No, keep it blocking

- Concurrency is what must be *forbidden*: the editor owns the tty; another task writing to
  the terminal corrupts it. Async buys nothing while the editor is up.
- Do **not** use `block_in_place` — it exists to let the worker run other tasks, i.e.
  straight into that corruption.
- The concurrent machinery is already off-runtime: `spawn_in_flight_reader` and
  `prompt_complete`'s worker are plain `std::thread`s.
- `#[tokio::main]` is multi-thread; blocking one worker for a user-driven one-off starves
  nothing.
- Keep-alives are the only casualty (reconnect) — acceptable.
- The DSR is a blocking read by design; async would only relocate it.

## 8. Deletions after migration

`review_compaction_summary`; `confirm_compaction_summary` (trait default + impl);
`CompactionSummaryReview` + `run_compaction`'s `review` param; both Ctrl-E arms' local
guard/DSR code; the `edit()`-time height fold. `edit_in_editor` → private.

## 9. Phases

- **E1** wrapper `run_editor` (+ `EditorMode`) + `AppState::edit_text`; `pause_reader` RAII +
  wire it into `run_editor` and the confirm dialog; drop the fold; port the two Ctrl-E arms.
  Behaviour-neutral except the fold's rare redundant DSR (and the gate now actually being
  released).
- **E1b** `Output::frame_size()` + `draw_frame` returning applied dims; loop records the
  fact (§2.4). Closes `ui_draw_v2.md` §9 #3.
- **E2** `SubmitResult::Handled` + `HostKey::Edit` + `edit()` dispatch; `v25_listing_node.rs`
  (read-only header+rows default); migrate `v25_menu.rs` to it.
- **E3** compaction split + `prepare()`/`submit()`; drop review/confirm plumbing.
- **E4** cleanup + doc updates.

Tests: vt100 frame suite (wrapper leaves no local DSR); `EditorMode::View` creates a
read-only file; `pause_reader` pauses/releases (including nested + no-gate no-op);
`frame_size()` == dims returned by `draw_frame`; menu tests (`edit()` dispatch, `Handled`
stays open, confirm precedence); compaction node tests (prepare → Prepared → submit applies
*edited* text; Esc aborts without writing); keep `--features test-utils` in the gate.

## 10. Decisions log

1. **`SubmitResult::Handled`** — added; `NodeEntry` returns `Continue` with a full plan
   (saves the rewrite → re-eval iteration). Optional `NewPath == full_prompt()` fast path too.
2. **Reader gate** — fixed generally with the RAII `pause_reader` (§2.3), shared by
   `run_editor` and the confirm dialog (`ui_draw_v2.md` §9 #1).
3. **Abort semantics** — no `abort()` hook: the draft is overwritten on the next
   `prepare()`, and node-level caches are invalidated by the `epoch` already passed to the
   trait methods.
4. **`draw_frame` applied dims** — done now (§2.4); closes `ui_draw_v2.md` §9 #3.
