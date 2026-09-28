# Editor cleanup — one wrapper, node-driven editing

Status: **E1b + E1 + E2 done**; E3–E4 pending. Rebased onto `origin/main` @ `c705add`
(which fixed `ui_draw_v2.md` §9 #1/#2 — see §2.3). Companion to `doc/ui_draw_v2.md` §9.

| step | scope | status |
|---|---|---|
| E1b | `draw_frame` takes the composed dims; `frame_size()`; loop records the applied pair | ✅ `3cdcbd4` |
| E1 | `run_editor` wrapper + `EditorMode` + `pause_reader` + fold drop + Ctrl-E port | ✅ this change |
| E2 | `SubmitResult::Handled` + `HostKey::Edit` + `edit()` dispatch + `v25_listing_node.rs` | ✅ this change |
| E3 | compaction split; `prepare()`/`submit()`; drop review/confirm plumbing | pending |
| E4 | cleanup + doc updates | pending |

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

## 6. Compaction via `prepare()` / `submit()` — E3

- Split `run_compaction_at_point` into **generate** (draft; LLM = `agent.summarize`) and
  **apply** (writes the entry from a supplied summary).
- `TreeCompact*Node::prepare(pa)`: resolve/guard → generate → `app_state.edit_text(&draft,
  Edit)` → store the edited text → `SubmitResult::Prepared`.
- Decision via the engine's two-phase confirm: `Enter → submit()` (apply from stored text);
  `Esc` → abort. `header()` carries the guidance text.

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
- **Pending (E3):** `confirm_compaction_summary` (trait default + impl),
  `CompactionSummaryReview` + `run_compaction`'s `review` param.

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
- **E3** pending: compaction split + `prepare()`/`submit()`; drop review/confirm plumbing.
- **E4** pending: cleanup + doc updates.

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
