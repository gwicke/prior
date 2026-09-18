# Slash menu v2.5 — implementation plan

Design: `slash_menu_v2.5_design.md`. Prior work: `slash_menu_v2_impl.md`
(Phases 0–7.2 steps 1–3 done).

---

## Status

**Done (v2.5 reshape):** Steps 1–6 partially complete.

Steps 1–5: Core engine. `SlashDotNode` trait (all defaults), `NodeEntry`
(internal wrapper), `SlashDotMenu` + `SlashDotHandle` (public API). Full
traversal: section parse, data refresh (Rc::ptr_eq), view derivation
(filter → fuzzy rank → sort), cursor, Esc ladder, leaf/listing dispatch,
render plan. Merged child sets, field metadata, row uniformity.

Step 6 (command porting): `/thinking` fully ported end-to-end. `/persona`
next. Root list via `StaticListingNode`. Remaining commands deferred.

**Key additions since last update:**
- `StaticListingNode` (convenience impl in `node_impl.rs`): takes static
  row config (id, description) in constructor. Optional header, active
  markers (`Rc<RefCell<HashSet>>>`), field metadata. Eliminates boilerplate
  impls for listing-level commands. Used for the root command list and the
  `/thinking` parent node.
- `StaticRow` helper struct for `StaticListingNode` row definitions.
- Default table listing: all rows render via the table builder with
  `id | description` as default columns (implicit fields via
  `row_field_value`). Plain-text `id — description` format removed.
- Empty-description fix: `Some("")` treated as no description (no dash
  separator) in both `node_entry.rs` and `render.rs` render paths.
- Shared state pattern: `ThinkingValue` writes to the same
  `Rc<RefCell<Option<String>>>` the REPL owns (`MenuShared`), no parallel
  state. After `DoneSuccess`, the REPL reads back from the shared cell.
  This is the pattern for all future command nodes.
- REPL wiring: persistent `SlashDotMenu` + `Option<SlashDotHandle>` in the
  input loop. Handle lifecycle: created when prompt starts with `/`, dropped
  when it leaves. `ActiveOverlay::V25Menu { plan }` variant for v2.5
  engine overlay. All key dispatch routes through `eval_prompt`.

**Design deviations from step4 doc (per user clarification):**
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
- ~~No `Rc<Vec<RenderLine>>` sharing~~ **Done.** `RenderPlan.lines` is now
  `Rc<Vec<RenderLine>>`.
- ~~No table layout / `ColumnDef` in RenderPlan~~ **Done.** Default table
  with `id | description` columns. `SlashDotNode::columns()` for explicit
  column definitions.
- No confirm dialog lines — `confirm_pending` is tracked in `EpochState`
  but no dialog text is rendered in the plan.
- `RenderPlan.title` is unused — always `None`. Will be set when commands
  provide panel titles.

**PathArgs / Args borrowing:**
- Owned data. Deferred to avoid lifetime complexity in the trait API.

**`parse_section` vs `grammar::parse_level`:**
- `parse_section()` (node_entry.rs) is the v2.5 algorithm.
  `parse_level` remains for old v2.0 interpreter tests.

**Row uniformity:**
- Only enforces non-empty `id`. Description optional.

**Field space inference fallback:**
- When `SlashDotNode::fields()` returns empty, `infer_field_space()`
  builds from row data. Commands with known fields should override
  `fields()`.

**Cursor / view state:**
- `cursor_idx` updated in-place and kept in sync. No debug_assert
  enforcing the ID-authoritative invariant.

---

## Next steps (revised)

1. **`/persona`** — enum values like `/thinking`, active marker from
   `MenuShared.current_persona` / settings store. Same shared-state pattern.
2. **Root `/` list** — `StaticListingNode` children for all commands.
   Extends the REPL handle lifecycle to all `/` prefixes.
3. **`/model`** — dynamic rows from catalog, field modifiers, computed
   fields. The showcase for `fields()` + merged child sets.
4. **Step 7** — two-phase submit confirm.
5. **Step 8** — persistent engine cleanup.
6. **Steps 9–14** — Session, Tree, Settings, Sandbox, Stat, Final cleanup.

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
the tail.

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

4d. **Cursor.** `cursor_id: Option<String>` + `cursor_idx: usize` per node
(in EpochState). Active marker seeds cursor on first active row.

4e. **Esc ladder.** Per-node restore with `should_restore_view_args` +
`view_args` string — no traversal stack, the prompt IS the state.

4f. **Render plan.** `header()` lines + ranked rows (table with
`id | description` default columns) + `selected: Option<usize>` +
`active_rows: Vec<usize>`.

### Step 5: Merged child sets

**Done.** 4 new tests.

5a–5d. Child iteration, field metadata, submit dispatch, row uniformity.

### Step 6: Port existing commands to `SlashDotNode`

**In progress.**

6a. ~~`/thinking`~~ **Done.** `StaticListingNode` parent + `ThinkingValue`
leaf nodes. Shared state via `Rc<RefCell<Option<String>>>` — same cell
`MenuShared` owns. 12 tests.

6b. **`/persona`** — **Next.** Enum values (persona names + "none"),
active marker from settings store. Same pattern as thinking.

6c. **Root `/` list** — `StaticListingNode` children for all commands.
Extends REPL handle lifecycle to all `/` prefixes.

6d. **`/model`** — dynamic rows from catalog, field modifiers.

**Tests:** 12 new per-command tests (thinking). Additional tests per
command as they are ported.

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

### Step 8: Host epoch wrapper + persistent engine

Replace the per-keypress engine rebuild with a persistent node tree.

8a. The host holds one `SlashDotMenu` instance for the lifetime of the
app. Per activation: bump epoch, call `root.node_traverse(epoch, key, ...)`.

8b. Delete the `Activation` bundle and `take_state`/`restore_state`. Per-
activation state (cursor, frames, data cache) lives inside the nodes,
invalidated by epoch.

8c. The host's `ActiveOverlay::Menu` carries only the `RenderPlan` from the
last `node_traverse()` call. The host renders it verbatim.

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
| Grammar / filter / fuzzy / table / render / wrap / value / prompt | 75 | Unchanged (v2.0 free functions) |
| Accept (confirm, interstitial, editor, new-path) | 8 | Unchanged (v2.0 accept.rs) |
| Interpreter (state/tab/enter/esc, dynamic, field_options) | 26 | Unchanged (v2.0, will be replaced) |
| **v2.5 node_entry (parse, listing, drill, cursor, esc, header, mixed)** | **22** | **New (steps 4–5)** |
| **v2.5 `/thinking` command (drill, submit, cursor, active, filter, esc)** | **12** | **New (step 6a)** |

Total: 523 lib tests (including non-menu tests). The 26 interpreter tests
test the old code path and will be replaced when commands are migrated.
New v2.5 tests cover the traversal harness and per-command behavior.

---

## 6. File changes summary

| File | Reshape changes |
| --- | --- |
| `node_impl.rs` | **New** — `SlashDotNode` trait + public types + `StaticListingNode` |
| `node_entry.rs` | **New** — `NodeEntry` (internal wrapper with children, cursor, traversal) |
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

| Host file | Changes |
| --- | --- |
| `src/v25_thinking.rs` | **New** — `ThinkingValue` + `build_menu()` for `/thinking` |
| `src/v25_commands.rs` | **New** — test-only node impls for engine harness testing |
| `src/repl.rs` | `ActiveOverlay::V25Menu` variant, `SlashDotMenu` + handle in input loop, `eval_prompt` key dispatch |
