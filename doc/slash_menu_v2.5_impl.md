# Slash menu v2.5 — implementation plan

Design: `slash_menu_v2.5_design.md`. Prior work: `slash_menu_v2_impl.md`
(Phases 0–7.2 steps 1–3 done).

---

## Status

**Done (v2.5 reshape):** Steps 1–5. `SlashDotNode` trait (all defaults),
`NodeEntry` (internal wrapper), `SlashDotMenu` + `SlashDotHandle` (public
API). Step 4: full traversal — section parse, data refresh (Rc::ptr_eq),
view derivation (filter → fuzzy rank → sort), cursor, Esc ladder, leaf/listing
dispatch, render plan. Step 5: merged child sets verified — `refresh_data`
builds `row_index` across children; field metadata via `SlashDotNode::fields()`;
row uniformity enforced (empty IDs warn+skip, duplicate IDs warn+last-wins);
prompt reconstruction via `full_prompt(path, suffix)` at all return points.
22 new tests covering parse, listing, drill, cursor, active markers, header,
Esc, fuzzy rank, mixed children, empty ID handling.

Design deviations from step4 doc (per user clarification):
- Modifiers are single-word (no multi-word values).
- Sort applied after fuzzy rank (filter → fuzzy rank → sort).
- `Args` and `PathArgs` own their data (borrow deferred).
- `HostKey::None` for Tab drill (no key forwarded).
- Single `MenuResult` type (no separate `NodeResult`).
- `parse_section()` in node_entry.rs (not grammar.rs).
- `full_prompt(path, suffix)` for host prompt reconstruction:
  listing drill passes `" "` as child section; matched drill passes tail.
- Empty-ID rows: warn + skip (not debug_assert).

### Deferred (not blocking, pick up as needed)

**RenderPlan / rendering:**
- No `Rc<Vec<RenderLine>>` sharing — `plan_from_cache()` rebuilds the full
  plan on every keypress. The step4 design §4.2 describes a `RenderCache`
  where `MoveCursor` reuses `lines` via Rc clone (only `selected` and
  `active_rows` recomputed). Deferred until profiling shows it matters.
- No table layout / `ColumnDef` in RenderPlan — rows render as
  `id — description`. Table columns come when commands with structured
  fields are ported (step 6+).
- No confirm dialog lines — `confirm_pending` is tracked in `EpochState`
  but no dialog text is rendered in the plan. The confirm dialog (§9)
  rendering is a step 7 concern.
- `RenderPlan.title` is unused — always `None`. Will be set when commands
  provide panel titles.

**PathArgs / Args borrowing:**
- `PathArgs` and `Args` own their data (`String`, `Vec<String>`). The
  step4 design and user preference was `Vec<(&str, &str)>` borrowed from
  the prompt — cheaper allocation but requires a lifetime on `PathArgs`.
  Deferred to avoid lifetime complexity in the trait API during early
  development; revisit when profiling shows allocation pressure.

**`parse_section` vs `grammar::parse_level`:**
- `parse_section()` (node_entry.rs) is a new single-pass left-to-right
  algorithm. The existing `grammar::parse_level()` (right-to-left,
  selection-candidate based) is unused by v2.5 traversal. Both remain
  in the crate — `parse_level` is still used by the old v2.0 interpreter
  tests. When the old interpreter is deleted, `parse_level` can be
  removed or consolidated.

**Test porting:**
- 22 new v2.5 tests written; 26 old interpreter tests remain in the old
  harness (test the old code path). The old tests will be replaced when
  commands are migrated (step 6). No test was mechanically ported —
  the new tests cover the v2.5 traversal harness independently.

**Row uniformity:**
- Only enforces non-empty `id`. The design says "at least `id` +
  `description`" but also "optionally `description`" — currently
  description is not checked. Could add a `warn!` for missing
  description in debug builds.

**Field space inference fallback:**
- When `SlashDotNode::fields()` returns empty, `infer_field_space()`
  builds a `FieldSpace` from the row data (infers number vs string from
  first non-missing value). Works but less precise than explicit metadata.
  Commands with known fields should override `fields()`.

**Cursor / view state:**
- `cursor_idx` is not re-derived from `cursor_id` on every
  `MoveCursor` — it's updated in-place and kept in sync. The step4
  design says "ID is authoritative, index is memoized" which matches,
  but the code doesn't enforce the invariant with a debug_assert.
- `EpochState` reset on epoch change clears `view_args` and
  `should_restore_view_args`. Content cache (`data_rc`, `row_index`)
  is NOT cleared — it persists across epochs (stale rows are replaced
  on the next `refresh_data`). This is intentional but undocumented.

**Next:** Step 6 (port existing commands to `SlashDotNode` — Model, Thinking,
Persona, root list).

---

## 1. Crate layout (current, pre-reshape)

```
crates/slashdot_menu/
  src/
    lib.rs
    node.rs          # MenuNode descriptor, FieldSpec, ColumnDef
    value.rs         # Value, ValueKind, ModifierSpec
    grammar.rs       # path tokenizer
    filter.rs        # field filters + sorting (§7.1)
    cursor.rs        # cursor cache, selection validation
    interpreter.rs   # state machine: state/tab/enter/esc
    accept.rs        # accept phase + confirm state
    provider.rs      # DataProvider, ActiveProvider traits
    render.rs        # render plan emission
    table.rs         # table builder
    fuzzy.rs         # fuzzy ranking
    prompt.rs        # PromptBuffer trait
    fields.rs        # FieldSpace
    wrap.rs          # text wrapping
```

---

## 2. What changes in the reshape

| v2 (current) | v2.5 (target) |
| --- | --- |
| `MenuNode` data struct + `Interpreter` central | `SlashDotNode` trait + `NodeEntry` wrapper + `SlashDotMenu`/`SlashDotHandle` |
| Providers/handlers registered by path string | Providers/handlers on the node (`data()`, `submit()`) |
| One child source per level | Merged child set (§2.1) |
| `ConfirmPrep` two-handler state machine | `prepare()` → `Prepared` → engine dialog (§9) |
| `Activation` bundle + rebuild per open | Persistent tree; epoch invalidation (§12) |
| `DataProvider::fetch()` on side table | `data()` on the child node |

**Not changed:** grammar, cursor/accept semantics, rendering contract, host
role split, modifier system, field filtering/sorting.

---

## 3. v2.5 reshape — implementation steps

Each step compiles and passes existing tests (adapted). Do them in order;
each builds on the previous.

### Step 1: Introduce `SlashDotNode` trait + public types + `SlashDotMenu`

**Done.** Pure additive — nothing ported yet.

- Public types: `PathArgs`, `Args`, `DataResponse`, `DataError`, `DataResult`,
  `SubmitResult`, `MenuResult`, `HostKey`
- `SlashDotNode` trait (all methods have defaults):
  `data()`, `submit()`, `prepare()`, `header()`, `fields()`
- `SlashDotMenu` (owns tree + epoch), `SlashDotHandle` (guard-like, one
  per invocation, `eval_prompt(prompt, key) -> MenuResult`)
- `NodeEntry` (internal wrapper): `new_root()`, `new_child()`,
  `add_child()`, `node_traverse()`, `node_data()`
- Module structure: `lib.rs`, `menu.rs`, `node_impl.rs`, `node_entry.rs`

### Step 2: Epoch wrapper + `SlashDotHandle`

**Done** as part of step 1.

### Step 3: Port the grammar + filter engine as free functions

**Done.** Grammar (`parse_level`), filter (`compile_query`, `apply_query`),
fuzzy (`rank_and_pin`) are already free functions. The v2.5 traversal
uses a new `parse_section()` (§2.1 single-pass) rather than `parse_level`,
but the filter/fuzzy/sort pipeline is reused.

### Step 4: Implement default `traverse` body

**Done.** 22 tests. The sub-steps map to implementation as follows:

4a. **Section parse + child dispatch.** Implemented as `parse_section()`
in node_entry.rs — a single left-to-right word scan that classifies each
word as dot modifier, positional (fuzzy term), or matched child ID. The
first word matching a row ID terminates the section; everything after is
the tail. Does NOT use `grammar::parse_level()` (different algorithm:
new parse is left-to-right with row_index lookup, per §2.1; `parse_level`
is right-to-left with selection candidates).

4b. **Key handling at listing level.** Done:
- `MoveCursor`: update cursor_idx/cursor_id, plan_from_cache
- `Tab`: drill — set view_args + restore flag, recurse child with `" "`
- `Enter`: drill — same but forward key
- `Esc`: handle_esc (§5.1 ladder)

4c. **Key handling at leaf level.** Done:
- `Enter`: prepare() → if Prepared, set confirm_pending; if Done, close;
  if NewPath, rewrite
- `Esc`: if confirm_pending, cancel; else handle_esc
- `None`/other: build_continue (shows header)

4d. **Cursor.** Implemented as `cursor_id: Option<String>` + `cursor_idx:
usize` per node (in EpochState). Reset on epoch increment. `cursor_id`
survives re-rank (re-resolved via `seed_or_keep_cursor`); `cursor_idx`
is the O(1) display lookup. Active marker seeds cursor on first active
row. This differs from the plan's `(epoch, terms_hash)` keying — cursor
persists across filter changes and is re-resolved, not reset.

4e. **Esc ladder.** Implemented per design §5.1 with per-node restore
(replaces old frame stack). The ladder:
1. **Restore trigger** (phase 3 of node_traverse): bare drilled ID with
   `should_restore_view_args` → seed cursor on ID's row, replace section
   with stored `view_args`, clear flag. Applies without Esc.
2. **Clear refinement**: terms, modifiers, selection all dropped.
3. **Strip trailing space** → parent becomes responsible.
4. **Close** (nothing left at root).
The "frame replay" from the old design is replaced by the per-node
`should_restore_view_args` flag + `view_args` string — no traversal
stack, the prompt IS the state.

4f. **Render plan.** Basic plan implemented: `header()` lines + ranked
rows (id — description) + `selected: Option<usize>` + `active:
Option<usize>`. No Rc-shared lines, no table layout, no `ColumnDef`
— table rendering comes when commands are ported (step 6). The host
already renders `RenderPlan` verbatim; columns/tables are added per-node
as needed.

**Tests:** 22 new tests covering parse (6), root listing, fuzzy filter,
bare ID, Tab drill, cursor wrapping, active markers, header, Esc close,
Esc clear filter, epoch reset, fuzzy rank ordering, mixed children (3),
empty ID skip. The 26 existing interpreter tests remain in the old harness
(not yet ported — they test the old code path and will be replaced when
commands are migrated in step 6).

### Step 5: Merged child sets

**Done.** 4 new tests.

5a. **Child iteration.** `refresh_data()` iterates all children in
declaration order, calls `node_data()` on each, concatenates rows. Each
row's child origin is tracked via `RowRef { child_idx, row_offset }` in
`ranked_rows`, and `row_index: HashMap<String, u16>` maps row ID →
child index.

5b. **Field metadata on the parent.** `SlashDotNode::fields()` trait
method returns `Vec<FieldSpec>`. `NodeEntry::field_space()` uses
trait-declared fields when non-empty, otherwise infers from row data.
Children conform to the parent's declared fields; missing values use
§7.1 missing-value rules.

5c. **Submit dispatch.** On Tab/Enter, the engine looks up the cursor
row's ID in `row_index` → gets child index → recurses into
`self.children[child_idx].node_traverse()`. For leaf children this
eventually reaches `prepare()`/`submit()`. Dispatch is via the recursive
traversal, not a direct `submit()` call — consistent with the §2
recursive design.

5d. **Row uniformity.** Enforced in `refresh_data()`:
- Empty ID: `eprintln!` warning + row skipped (both in `row_index`
  construction and `ensure_view` filtering). Not just a debug_assert.
- Duplicate ID: `eprintln!` warning + last child wins.
- Description: not enforced (optional, §2.1 says "at least `id`" and
  "optionally `description`").

**Tests:** Mixed children merged listing, Tab drill dispatch into correct
child, fuzzy filter across children boundary, empty ID rows skipped.

### Step 6: Port existing commands to `SlashDotNode`

Migrate the already-engine-backed commands (Model, Thinking, Persona,
root list) from descriptor+handler-registration to `SlashDotNode` impls.

For each command:
- Replace `MenuNode` + `DataProvider` + `LeafHandler` registration with an
  `impl SlashDotNode` struct.
- `data()` returns the rows (static list or `Rc<DataResponse>` from fetch).
- `submit()` handles the leaf accept.
- `header()` returns help text if any.
- Register the instance as a child of the root node.

Commands to port (in order):
1. Root `/` list (static children, trivial `data()`).
2. `/thinking` (enum value child, simplest end-to-end).
3. `/persona` (dynamic child, active marker from `data()`).
4. `/model` (dynamic child with field modifiers, computed fields).

**Tests:** Existing per-command tests pass with the new harness. The
descriptor-driven tests (`state("model")`, etc.) are replaced by
`root.node_traverse(epoch, Tab, ...)` → check `RenderPlan`.

### Step 7: Two-phase submit confirm

Port the `confirm` flow from `accept.rs` into the node model.

7a. The confirm dialog (accept/cancel) is a non-overridable engine service:
the default `traverse` body renders it when the previous step returned
`SubmitResult::Prepared`, routes keystrokes to the dialog, and calls
`submit()` on yea.

7b. The node's `prepare()` does the prep (may open an external editor via a
callback captured at construction, stage the payload in private state).
`submit()` commits using the staged payload.

7c. Delete the `ConfirmState` / `ConfirmPrep` machinery from `accept.rs`.
The node's private state replaces it.

**Tests:** Port the confirm scenarios from `accept.rs`: prep → pause →
Esc cancel → re-prep → accept.

### Step 8: Host epoch wrapper + persistent engine

Replace the per-keypress engine rebuild with a persistent node tree.

8a. The host holds one `SlashDotMenu` instance for the lifetime of the
app. Per activation: bump epoch, call `root.node_traverse(epoch, key, ...)`.

8b. Delete the `Activation` bundle and `take_state`/`restore_state`. Per-
activation state (cursor, frames, data cache) lives inside the nodes,
invalidated by epoch.

8c. The host's `ActiveOverlay::Menu` carries only the `RenderPlan` from the
last `node_traverse()` call. The host renders it verbatim.

**Verify:** `cargo build -p slashdot_menu` in isolation. Host compiles.
Existing integration tests pass.

---

## 4. Command migrations (on the reshaped engine)

Each migration replaces a bespoke `ActiveOverlay` variant + host-rendered
`display_rows` + host-cursed accept with an `impl SlashDotNode` + generic
`Menu` rendering. The host loses one variant per step.

### Step 9: Session

- Dynamic child: `data()` calls `list_sessions_sync()`, returns session
  rows (name, turns, age, dir). `active_ids` = current session id.
- Static action children per pinned session: `resume`, `fork`, `rename`,
  `delete`.
- `delete` uses two-phase submit: `prepare()` → confirm dialog (§9).
- Header: entry info panel (name/turns/age/dir) via `header()`.
- **Deletes:** `ActiveOverlay::Session`, `SessionAction`, `Confirm`,
  `session_action_overlay_entries`.

### Step 10: Tree + `/compact`

- Dynamic child: `data()` reads the tree sync mirror, returns tree rows.
  `active_ids` = current leaf id.
- Static action children: `switch`, `compact`.
- `compact` modes: single-token enum children.
- `llm` leaf: two-phase submit with `.prompt` modifier; `prepare()` may
  open editor for prompt editing.
- `/compact` itself: `submit()` returns `NewPath("/tree <id> compact ")`.
- Header: tree indent layout via display strings.
- **Deletes:** `ActiveOverlay::Tree`, `TreeAction`, `SwitchAction`,
  `CompactSummary`, `tree_overlay_entries`, `filter_tree_options`,
  `tree_entry_header`, `compact_summary_header`.

### Step 11: Settings

The showcase for merged child sets:
- Static children: one per setting (`default_model` excluded — see below).
  Each: `data()` returns one row (key + description + current value).
  `submit()` writes the value to the settings cell.
- Dynamic child: `default_model` — a separate model-node instance
  (§2 "Node sharing") returning model rows with its own `submit()`.
  `.field:` / `.sort:` modifiers are the generic engine (§7.1).
- **Deletes:** `ActiveOverlay::Settings`, `SettingsVal`,
  `filtered_value_candidates`, `parse_model_filter`,
  `model_selection_overlay_entries`, `MODEL_MODIFIERS`,
  `ReplCtx.model_filter`, recent-sync ceremony.

### Step 12: Sandbox

- Mixed listing: static `add` child at top + dynamic binding rows.
- `add` → child node with fs live-filter value completion (§8).
  `data()` returns directory entries; Tab on a directory drills deeper.
  `submit()` commits the path.
- Remove: each dynamic row has its own `submit()` to remove the binding.
- **Deletes:** `ActiveOverlay::Sandbox`, `sandbox_overlay_entries`.

### Step 13: Stat + Replay

- `/stat`: `header()` returns stats text; no `submit()` handler.
  Esc clears. **Deletes:** `ActiveOverlay::Stat`.
- `/replay`: `header()` returns help text; `submit()` dispatches the
  replay. **Deletes** the remaining replay host code.

### Step 14: Final cleanup

- `ActiveOverlay` = `None | Help | PathComplete | Menu` (4 variants).
- Delete `SlashKind` dispatch if no longer needed.
- Delete dead `engine_rows` fields, `#[allow(dead_code)]` machinery.
- Audit: `slashdot_menu` builds standalone; host renders plan verbatim;
  no per-variant marker/cursor policy; `grep ActiveOverlay` shows 4
  variants.
- Verify `slash_menu.md` acceptance checklist.

---

## 5. Test strategy

| Category | Count | Status |
| --- | --- | --- |
| Grammar / filter / fuzzy / table / render / wrap / value / prompt | 75 | Unchanged (v2.0 free functions, still passing) |
| Accept (confirm, interstitial, editor, new-path) | 8 | Unchanged (v2.0 accept.rs) |
| Interpreter (state/tab/enter/esc, dynamic, field_options) | 26 | Unchanged (v2.0, will be replaced in step 6) |
| **v2.5 node_entry (parse, listing, drill, cursor, esc, header, mixed)** | **22** | **New (steps 4–5)** |

Total: 121 lib tests. The 26 interpreter tests test the old code path
and will be replaced when commands are migrated (step 6). New v2.5 tests
cover the traversal harness, not individual commands.

---

## 6. File changes summary

| File | Reshape changes |
| --- | --- |
| `node_impl.rs` | **New** — `SlashDotNode` trait + public types (`PathArgs`, `Args`, `DataResponse`, `SubmitResult`, `MenuResult`, `HostKey`) |
| `node_entry.rs` | **New** — `NodeEntry` (internal wrapper with children, cursor cache, traversal) |
| `menu.rs` | **New** — `SlashDotMenu`, `SlashDotHandle` (public API) |
| `lib.rs` | `pub mod menu` + `pub(crate) mod node_impl` + re-exports |
| `interpreter.rs` | Logic moves into `NodeEntry::node_traverse()`; file shrinks or is deleted |
| `accept.rs` | Confirm state machine moves to node-local state; engine dialog service stays |
| `provider.rs` | `DataProvider` trait absorbed into `SlashDotNode::data()`; file shrinks |
| `node.rs` | `MenuNode` — internal, merged child set; field metadata |
| `cursor.rs` | Cursor cache moves to `NodeEntry`; file shrinks or is deleted |
| `render.rs` | `Rc`'d row data in `RenderPlan`; otherwise unchanged |
| `grammar.rs` | Unchanged (free functions) |
| `filter.rs` | Unchanged (free functions) |
| `fuzzy.rs` | Unchanged |
| `table.rs` | Unchanged |
