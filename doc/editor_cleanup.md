# Editor cleanup — one wrapper, node-driven editing

Status: **E1b + E1 + E2 + E3 + E4 + E5 done** — all phases complete. Rebased onto
`origin/main` @ `c705add` (which fixed `ui_draw_v2.md` §9 #1/#2 — see §2.3).
Companion to `doc/ui_draw_v2.md` §9.

| step | scope | status |
|---|---|---|
| E1b | `draw_frame` takes the composed dims; `frame_size()`; loop records the applied pair | ✅ `3cdcbd4` |
| E1 | `run_editor` wrapper + `EditorMode` + `pause_reader` + fold drop + Ctrl-E port | ✅ this change |
| E2 | `SubmitResult::Handled` + `HostKey::Edit` + `edit()` dispatch + `v25_listing_node.rs` | ✅ this change |
| E3 | compaction split; `prepare()`/`submit()`; drop review/confirm plumbing | ✅ this change |
| E4 | cleanup + doc updates | ✅ this change |
| E5 | typed non-REPL slash dispatch → v2.5 batch eval | ✅ this change |

## 1. Goals

- **One** editor entry point that owns the whole terminal turnover: reader gate →
  raw-mode off → `$EDITOR` → raw-mode on → DSR re-anchor. No call site keeps its own
  guard/DSR logic.
- Menu endpoints may **opt in** to editing per node; editor support is *not* mandatory
  for generic/library nodes. A **host listing decorator** (`v25_listing_node.rs`) wraps
  `StaticListingNode` — used *exactly at the sites that use `StaticListingNode` today*
  (static rows, no `submit`; dynamic listing nodes are unaffected) — and supplies the
  default `edit()`: open `header()` read-only (`EditorMode::View`). Other host nodes
  override `edit()` with their own behavior (bulk edits, confirmations).
- `/tree … compact` moves generate → edit → confirm into the node's
  `prepare()` / `submit()`; the engine's existing two-phase confirm replaces
  `confirm_compaction_summary`.

## 2. The wrapper — `src/editor.rs` (done)

```rust
pub(crate) fn run_editor(
    guard: &TermGuard,
    out: &dyn Output,
    current: &str,
    mode: EditorMode,          // Edit: tempfile 0600 + read-back; View: 0400, no read-back
) -> io::Result<String>;
```

What it owns, in order: `ReaderHold` (raise the in-flight reader's stdin gate) →
`guard.stop()` → `edit_in_editor(current, mode)` → `guard.start()` → `out.cursor_row()`
(DSR re-anchor) → drop the hold (gate down only *after* the DSR has consumed its reply —
the reply arrives on stdin). Blocking by design (§7).

- `edit_in_editor` + `resolve_editor` are **module-private** here (mechanism only), with
  their tests moved from `repl.rs`.
- `EditorMode::View` creates the temp file **mode `0o400`** — the editor sees a file it
  cannot save (`vim` behaves like `:view`) — and does **not** read back; `run_editor`
  returns `current` unchanged. The permission bit is the portable signal (no per-editor
  `-R`/`-v` flags), and even a force-write cannot persist.
- Convenience for host nodes: `AppState::edit_text(&self, current, mode)` — pulls guard +
  output from the slot, reports `Unsupported` without an interactive guard.

### 2.1 Naming (output-side API) — settled

No new output method was needed: `refresh_dsr()` collapsed into the existing generic
`cursor_row()` (the editor is just one foreign-writer caller), and `painted_size` stayed
loop-owned — E1b made it the record of `draw_frame`'s returned dims, so there is no
editor-specific name anywhere in `Output`.

### 2.2 Drop the height fold — done (E1)

Removed with the Ctrl-E arm port: the arms no longer touch `painted_size` at all (the
closure's record at paint time is the only writer besides the loop-top reader). The fold
was a micro-optimization only — one saved DSR round-trip in the rare
height-resize-during-edit case; no correctness path depended on it.

### 2.3 stdin ownership — dialog side fixed upstream; editor side in E1

- **Upstream `87caa3b` fixed §9 #1**: `ui::resolve()` now stores `false` into the gate it
  raised (pinned by `confirm_releases_reader_gate_on_resolve`, and
  `ui_draw_v2.md` §9 #2's DSR-fallback fix — `DrawCache::reanchor(Option)` clears `live`
  on timeout — is what `run_editor`'s re-anchor now leans on).
- **E1 added the editor side**: a module-local `ReaderHold` RAII in `editor.rs` — swap-in
  the gate, restore the *previous* value on drop (nesting-safe against an outer pause).
  Held across the whole child lifetime *and* the DSR, since the reply arrives on stdin.
- **Decision: no `Output::pause_reader()` trait method.** The confirm path keeps
  upstream's shipped store/release pattern (working, tested); a trait method used by
  exactly one caller isn't worth the API.

## 3. Where editing is driven from

1. **Input buffer** (overlay `None`): Ctrl-E → `run_editor(.., EditorMode::Edit)`; result
   replaces the buffer.
2. **Menu endpoint** (overlay set): Ctrl-E → `run_editor(.., EditorMode::View)` on the
   header today; becomes `HostKey::Edit` → node `edit()` dispatch in E2.
   (The header arm is now honestly view-only: it used to open an editable buffer whose
   edits were silently discarded.)
3. **Inside a workflow** (compaction): the node calls `run_editor` inside `prepare()` (E3).

## 4. Node editing capability — decided shape (E2)

- Trait method (library; editor support optional by construction):
  ```rust
  async fn edit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
      SubmitResult::Handled   // default: no-op edit; stay open
  }
  ```
- **Host listing decorator** `src/v25_listing_node.rs` — a *concrete* wrapper holding
  `StaticListingNode` + `Arc<AppState>`, used **exactly at the sites that use
  `StaticListingNode` today** (static rows, no `submit`). Dynamic listing nodes are *not*
  wrapped — they get their own `edit()` where applicable (below). Its `edit()` opens
  **`header()` only**, read-only (`EditorMode::View`); rows in the view can be added later
  once specced.
- **Sweep (E2):** every *other* production node that **defines `header()`** got the same
  read-only `edit()` through the shared `view_header()` helper (20 nodes: clear/new/quit/
  replay/stat, set leaf, sandbox mapping/remove/add, session
  list/resume/fork/rename/delete, tree list/switch/compact + the three compact modes).
  Nodes without `header()` get nothing — there is nothing to view. (`SandboxMappingNode`
  was a unit struct; it gained an `app_state` field for this.)
- **Decorator, not a look-alike**: every trait method *forwards* to the inner
  `StaticListingNode` — no logic/builder duplication, no drift — and because the inner type
  is concrete the forwarding is static: **zero runtime overhead** beyond the one `dyn` layer
  the menu already has. (The earlier "8-method boilerplate" objection was about wrapping
  `dyn`; wrapping the concrete type makes it mechanical.) Future `StaticListingNode`
  overrides must be forwarded too — the forward-all-methods impl plus a comment is the
  guard.
- **Per-node overrides** do the real work; `SubmitResult` gives `edit()` the full
  vocabulary — including the confirmation dialog (`Prepared`): settings bulk-edit
  (parse → write → `Prepared`/`Done`), leaves editing parameter/draft text.
- Host nodes self-serve via `app_state` — no capability threading; the library keeps its
  no-op default.
- `edit()` carries the **epoch** for cache invalidation: a node stashing state across
  calls (e.g. the compaction draft, keyed by the prepared message ID *and* the epoch it
  was prepared at) reuses the stash only while both still match, re-generating otherwise.

## 5. Engine dispatch (`HostKey::Edit`) — E2

- Add `HostKey::Edit` (`node_entry.rs`).
- `NodeEntry::handle_leaf` / `handle_listing`: bypass completion/confirm (an `Edit` while
  a confirm is pending is ignored, like other non-confirm keys), call `edit(epoch, pa)`,
  map like `prepare()`: `Done → DoneSuccess`, `Prepared → confirm_pending`,
  `NewPath → ContinuePrompt`, `Error → error_continue`.
- **`SubmitResult::Handled`** (new): "handled, stay open" — `NodeEntry` returns
  `MenuResult::Continue(plan, prompt)` *directly* via the passive-render path (byte-identical
  to what a re-eval yields), saving the host's rewrite → re-eval iteration. Behaviourally
  `NewPath(pa.full_prompt())` minus the iteration. Optional companion: fast-path
  `NewPath == full_prompt()` into the same direct-`Continue`. Adding the variant surfaces
  every `match` on `SubmitResult` — that is the engine-arm checklist.
- Input-buffer editing (overlay `None`) never reaches a node — the REPL handles it.

## 6. Compaction via `prepare()` / `submit()` — E3 (done)

**Data layer (`agent.rs`) — one code path for every mode:**

- `CompactionDraft { summary, tokens_before }` — what generation produces; callers replace
  `summary` with the reviewed (edited) text before applying.
- `generate_compaction(app_state, target_id, mode, llm_payload)` = validate (session,
  known target, one "nothing before" guard for `Llm`, lazy agent) → snapshot
  `tokens_before` at the original leaf → **one pivot dance for all modes**: move the leaf
  to the predecessor, build the scoped context (an active previous compaction re-enters
  as its `<summary>` wrapper — lossless iterative compaction), build the per-mode draft
  (`Llm` summarizes *while pivoted*, so retries re-send the identical scoped prefix) →
  **restore the original leaf on every return path**. No special case for target == leaf
  or a missing predecessor — those are plain moves. No session writes, no display.
- `apply_compaction(app_state, target_id, draft)` = `append_compaction` only. No
  `display_separator`/`display_markdown`: the user just reviewed the summary, and the
  committed input line documents the write (re-render, if ever wanted, is a UI decision).

**UI layer (`v25_tree.rs`):**

- Shared body for the three mode nodes: `compact_prepare` (generate →
  `app_state.edit_text(&draft, Edit)` → stash → `SubmitResult::Prepared`),
  `edit_draft` (no tty → accept as-is), `compact_submit` (validate stash →
  `apply_compaction` → clear stash).
- `CompactStash { epoch, target_id, draft }` — keyed by (target, epoch) per §4:
  `prepare()` always regenerates (overwrites the stash, §10.3); `edit()` (Ctrl-E)
  re-opens a fresh stash without regenerating, else regenerates; `submit()` rejects a
  stale key with `Error` (stay open, Enter regenerates).
- **The confirm is the engine's native workflow** — `prepare()` returns `Prepared`,
  `NodeEntry` arms `confirm_pending`, Enter runs `submit()`, Esc aborts. No
  confirmation dialog exists anywhere in the compaction path anymore.
- `header()` carries the guidance ("Enter opens $EDITOR; then Enter writes, Esc aborts").
- Non-REPL slash input has no typed arms anymore (E5): `Repl::batch_slash` runs
  a complete line through the v2.5 engine as a batch — passive derive +
  virtual `Enter` presses (`Prepared` confirms take a second press; an Enter
  that *navigates* stops the sequence so blind presses never act on an
  unchosen row) — dropping `Continue` plans (no overlay paints without a
  REPL) and echoing commits like `emit_commit`. One dispatch for interactive
  and scripted input; the four `/tree`/`/session`/`/clear`/`/new` arms and
  their private helpers are deleted (§9 E5).

## 7. Async? — No, keep it blocking (decided)

Concurrency is what must be *forbidden*: the editor owns the tty, so any other task
writing to it corrupts it — async buys nothing while the editor is up. Do **not** use
`block_in_place` (it exists to let other tasks run). The concurrent machinery is already
off-runtime (`spawn_in_flight_reader`, `prompt_complete`'s worker are `std::thread`s);
`#[tokio::main]` is multi-thread; keep-alives are the only casualty (reconnect); the DSR
is a blocking read by design.

## 8. Deletions

- **Done (E1):** both Ctrl-E arms' local guard/DSR/fold code; `resolve_editor` /
  `edit_in_editor` moved out of `repl.rs` (now module-private in `editor.rs`);
  `review_compaction_summary`'s local stop/edit/start/DSR (now calls `run_editor`).
- **Done (E3):** `confirm_compaction_summary` (trait default + impl),
  `CompactionSummaryReview` + `run_compaction`'s `review` param — plus the
  now-callerless `review_compaction_summary`, `run_compaction` /
  `run_compaction_at_point` / `CompactionResult` / `CompactionTarget` /
  `compact_inline`, and `find_predecessor_on_active_branch` (only served the
  removed dead guard, see §10.7).
- **Stale docs naming the deleted review plumbing — fixed (E4):** `tree_compaction.md`
  (Status backend paragraph, §6.7b review UX, supersession banners on §6.8 and the
  Phase-7 record), `ui_draw_v2.md` ($EDITOR wrapper row), plus the sweep:
  `todo.md` (Phase-7 backend + `Abandoned` note), `slash_menu.md` (the
  `CompactionResult` cancel-re-entry bullet), `nested_compactions.md`
  (`run_compaction_at_point` name), `slashdot/tree_design.md` (the
  `AsyncOp::TreeCompact` section + the deferred-editor follow-up),
  `v25_cleanup_pass_2.md` (overlay_lines row; Phase 4 marked moot — the
  historical phase records elsewhere were left as written).

## 9. Phases

- **E1b** ✅ `3cdcbd4`: `Output::frame_size()`; `draw_frame(frame, anchor, policy, dims) ->
  applied`; `redraw` threads the loop's single read through compose → apply and records the
  report into `painted_size` (loop's `set((cur_w, cur_h))` gone); `draw_dialog`/`resolve`
  pass the dialog's tracked `size`; 3 ui tests.
- **E1** ✅ this change: new `src/editor.rs` (`run_editor`, `EditorMode`, `ReaderHold`,
  module-private mechanism + 15 moved/new tests); Ctrl-E arms ported (Edit for the
  buffer, View for the header); folds removed; `review_compaction_summary` routed through
  the wrapper; `AppState::edit_text` + test (guard-less = `Unsupported`).
- **E2** ✅: `SubmitResult::Handled` + `SlashDotNode::edit(epoch, pa)` (default `Handled`)
  + `HostKey::Edit` with pre-gate dispatch in `handle_leaf`/`handle_listing` (pending
  confirm swallows; `Handled` degrades to `key = None`, i.e. the exact passive re-derive;
  no recursion → no boxing); `v25_listing_node.rs` decorator + shared `header_ctx()`/
  `view_header()` helpers; all 13 production `StaticListingNode` sites migrated; the 19
  header-bearing nodes wired to `edit()` (§4); Ctrl-E on a `V25Menu` overlay routes
  through `HostKey::Edit`. Tests: 5 engine dispatch + 4 decorator.
- **E3** ✅ this change: `generate_compaction`/`apply_compaction`/`CompactionDraft` in
  `agent.rs` (one pivot dance for all modes; Llm summarizes while pivoted — see §10.6);
  shared `compact_prepare`/`edit_draft`/`compact_submit` + `CompactStash` in
  `v25_tree.rs` wired to the native `Prepared` → `confirm_pending` → `submit()` flow;
  the typed arm drives the same node pair; review plumbing deleted (§8).
  **Duplication found and collapsed** (flagged per review): (a) the validation
  preamble existed twice — `compact_inline` vs the typed arm — now one place in
  `generate_compaction`; (b) two conflicting "nothing before" guards — the
  `pred == leaf` variant was provably unfireable and was removed (§10.7); (c) the
  append+render block repeated in all three mode arms — now one `apply_compaction`
  (display dropped); (d) the seed pivot ran separately from the Llm summarize, whose
  comment claimed a leaf move that had already been restored — now one dance.
  Tests: 10 generate/apply (incl. leaf-restore, edited-draft, no-write-without-apply,
  the two Llm guards), 7 node two-phase (incl. no-stash and stale-epoch rejects) with
  the full traversal driving Enter#1/Enter#2 through `eval_prompt_resolved`.
- **E4** ✅ this change: **reachability verification — the delete suggestion is
  refuted.** Interactive `Submit` provably never carries `/` text (a `/`-buffer
  is owned by the v2.5 engine from the first keystroke; Enter queues
  `HostKey::Enter` into the engine, whose `DoneSuccess`/`DoneError` returns
  `MenuDone` — `edit_key`'s `Submit` arm can never see a `/` buffer, and the
  `Done*` non-Enter arms clear the buffer before dismissing). The typed
  `/tree … compact` arm **is** reached through `use_repl = stdin().is_terminal()
  && !config.quiet` being false (raw `get_user_input` lines) — the quiet/scripted
  command path shared with `/session` / `/clear` / `/new`; scripted compaction
  is functional (no tty → `edit_draft` accepts the draft as-is) and covered by
  the node prepare/submit + `parse_tree_args` tests. Kept; documented with an
  inline reachability note on the arm.
  **Doc sweep:** stale present-tense references to the deleted review plumbing
  fixed in `tree_compaction.md`, `ui_draw_v2.md`, `todo.md`, `slash_menu.md`,
  `nested_compactions.md`, `slashdot/tree_design.md`, `v25_cleanup_pass_2.md`
  (§8); historical phase records left as written; supersession banners added
  where a section would otherwise read as current (`tree_compaction.md` §6.8 /
  Phase 7). No new pty test: `e2e_session_state.rs` already drives interactive
  slash commands through `read_input`'s engine path, and the node-level tests
  cover the arm's mechanics.
- **E5** ✅ this change: **typed non-REPL dispatch removed — scripted slash
  input goes through the v2.5 engine.** Deleted the four Submit-arm slash arms
  plus `parse_tree_args`/`TreeAction`/`handle_session_command`/
  `SessionCommandResult`/`build_summary_prompt`/
  `collect_entries_for_branch_summary`/`parse_session_name_after` and the
  parser tests (~730 lines). New `Repl::batch_slash` (`repl.rs`): evaluated in
  the **input position** (before the loop's convergence step, so control and
  target writes land the same cycle as an interactive `MenuDone`); key
  mapping = passive `HostKey::None` + virtual `Enter`s with the navigation
  guard (returned prompt moved → a listing drill → stop); `Continue` plans are
  dropped (drawing is host-side) and commits echo through `batch_commit`
  (history + `display_plain` + message — the `emit_commit` equivalent without
  a frame). `/q` and `exit` keep their loop arm. **Casualty:**
  `switch --summarize`'s only implementation left with the typed arm (the
  engine's switch node never had it) — `doc/todo.md` is now its sole tracker.
  Tests: 4 batch tests (one-Enter `/thinking high` commit, `/quit` latches the
  control target, non-slash//`/q` fall through to `Submit`, unknown input
  returns instead of hanging) plus 1 in-process `run_interactive` test
  (scripted `Output` feeding `/thinking high` + `/quit`: the commit echo
  proves `batch_slash` ran, the consumed queue proves `quit` landed on its
  own cycle — input-position eval → `MenuDone` → same-cycle convergence).
  Host reality found while verifying: the binary never yields non-REPL
  input (`QuietOutput::get_user_input` is an always-empty stub; piped
  stdin was never a command source) — the supported hosts are
  in-process/scripted ones, so a spawned pipe e2e was removed as a wrong
  premise, and the pre-existing empty-input spin is logged in
  `doc/todo.md`.

Gate: `cargo test --features test-utils` (includes `confirm_raw_mode`), `cargo fmt --check`,
clippy clean in touched regions.

## 10. Decisions log

1. **`SubmitResult::Handled`** — add it; `NodeEntry` returns `Continue` with a full plan
   (saves the rewrite → re-eval iteration). Optional `NewPath == full_prompt()` fast path too.
2. **Reader gate** — dialog side fixed upstream (`87caa3b`, `resolve()` releases);
   editor side = `ReaderHold` inside `run_editor`, holding across the DSR with
   prior-value restore. No `pause_reader` trait method (§2.3).
3. **Abort semantics** — no `abort()` hook: the draft is overwritten on the next
   `prepare()`, node caches invalidated by the `epoch` on the trait methods.
4. **`draw_frame` dims** — passed in and reported (E1b); `painted_size` loop-owned.
5. **Header Ctrl-E** — becomes `EditorMode::View` (was: editable buffer with discarded
   edits).
6. **One pivot dance** — every compaction mode does move → draft → restore, restore on
   all paths; `Llm` summarizes while pivoted (the old step-7 comment claimed the leaf
   was "already moved by step 3", but the seed pivot had already restored it — the
   summarize ran unpivoted, i.e. against the whole context when target ≠ leaf; fixed).
   Target == leaf / no predecessor need no special case — they're plain moves.
7. **Single "nothing before" guard** — the two guards collapsed to
   `predecessor.is_none()` for `Llm`. The `find_predecessor_on_active_branch(…) == leaf`
   variant could never fire (a predecessor always precedes its target on the root→leaf
   path, whose last entry is the leaf); fn + tests removed.
8. **Typed-arm confirm** — the `/tree … compact` arm calls the nodes' `prepare()` then
   `submit()` directly (typed Enter = confirmation); it never gets its own dialog.
9. **Typed-arm reachability (E4)** — refuted the "unreachable → delete" flag:
   interactive `/` input routes through the engine and commits as `MenuDone`
   (repl.rs `read_input` + the `emit_commit` history note), so `Submit` never
   carries slash text — but the arm is the **non-REPL command path** (`repl == None`
   when stdin is not a tty or `--quiet`): raw lines reach `parse_tree_args` and the
   node pair exactly as designed (quiet/pipe compaction accepted without an
   editor). The arm survives with a reachability comment; only the stale claim in
   §6/the E3 record was corrected.
  **Non-priority (follow-up decision):** the non-REPL path itself is *not a
  priority* — the four inline arms (`/tree`, `/session`, `/clear`, `/new`) are
  tolerated as-is, not grown. If scripted slash input ever becomes important,
  it should go through the **v2.5 menu eval** too — the same dispatch as
  interactive — with the drawing output ignored (frames are irrelevant with
  no overlay); one implementation instead of per-command arms.
  **Executed (E5):** the arms are deleted and `Repl::batch_slash` implements
  exactly that — the v2.5 eval with drawing ignored (§9 E5). E4's "scripted
  input reaches the arm" overstated the *binary* case: the binary never
  reads command lines from a pipe (`QuietOutput::get_user_input` is an
  always-empty stub — pre-existing, see `doc/todo.md`), so the real
  non-REPL consumers are in-process/embedded hosts with a reading `Output` —
  which makes the deletion even more clearly correct.
