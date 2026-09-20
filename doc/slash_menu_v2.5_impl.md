# Slash menu v2.5 — implementation plan

Design: `slash_menu_v2.5_design.md`. Prior work: `slash_menu_v2_impl.md`
(Phases 0–7.2 steps 1–3 done).

---

## Status

**Done (v2.5 reshape):** Steps 1–6 partially complete. 538 tests, 0 warnings.

Steps 1–5: Core engine. `SlashDotNode` trait (all defaults), `NodeEntry`
(internal wrapper), `SlashDotMenu` + `SlashDotHandle` (public API). Full
traversal: section parse, data refresh (Rc::ptr_eq), view derivation
(filter → fuzzy rank → sort), cursor, Esc ladder, leaf/listing dispatch,
render plan. Merged child sets, field metadata, row uniformity.

Step 6 (command porting): `/thinking`, `/persona`, `/model` fully ported.
Root list via `StaticListingNode`. Remaining commands deferred.

**Key additions:**
- `StaticListingNode` (convenience impl in `node_impl.rs`): takes static
  row config (id, description) in constructor. Builder methods:
  `.with_header()`, `.with_active()`, `.with_fields()`, `.with_columns()`.
  Eliminates boilerplate impls for listing-level commands.
- `StaticRow` helper struct for `StaticListingNode` row definitions.
- `ColumnDef` re-exported from `slashdot_menu` for host convenience.
  `ColumnDef::new(field, header)` — use `"id"` for row id,
  `"description"` for description, or any custom field name.
- `ChildBuilder` (menu.rs): opaque handle returned by
  `SlashDotMenu::add_root_child()`. Exposes `.add_child_node()` for
  chaining — builds the tree without exposing `NodeEntry` internals or
  hardcoded index paths. `NodeEntry` stays `pub(crate)`.
- Default table listing: all rows render via the table builder with
  `id | description` as default columns (implicit fields via
  `row_field_value`). No header row when `columns()` is empty.
- Custom table headings via `SlashDotNode::columns()` / `.with_columns()`.
  Header renders through the Table builder for correct column alignment.
- Empty-description fix: `Some("")` treated as no description (no dash
  separator) in both render paths.
- Shared state pattern: nodes write to the same `Rc<RefCell<...>>` the
  REPL owns (`MenuShared`). After `DoneSuccess`, the REPL reads back from
  shared state. `DoneSuccess` dispatches by command prefix
  (`thinking` → `SetThinking`, `persona` → `SetPersona`,
  `model`/`models` → `SetModel`).
- REPL wiring: persistent `SlashDotMenu` + `Option<SlashDotHandle>` in the
   input loop. Handle lifecycle: created when prompt starts with `/thinking`,
   `/persona`, `/model`, or other ported commands; dropped when it leaves.
   `ActiveOverlay::V25Menu { plan, sticky_count }` variant for v2.5 engine
   overlay.
- Catalog generation counter (`model_catalog::catalog_generation()`):
  atomic `u64` bumped on each cache refresh. `ModelNode` compares it to
  its cached `Rc<DataResponse>` to skip rebuild when data is unchanged
  (enables engine's `Rc::ptr_eq` freshness check).
- Sticky headers: header() lines and table header row stay fixed at the
  top when the list scrolls. `RenderPlan.sticky_count` computed by the
  engine; the host's overlay computation splits paint output into sticky
  prefix + windowed scrollable body.
- Rendering fixes: table headings aligned with data columns (rendered
  through `Table::render_header()`). Background extends to full terminal
  width (uses `display_width()` to compute visible length, avoiding ANSI
  escape code inflation in padding).

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
- ~~No `Rc<Vec<RenderLine>>` sharing~~ **Done.**
- ~~No table layout / `ColumnDef` in RenderPlan~~ **Done.** Table headings,
  custom columns via `.with_columns()`, sticky headers.
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

1. ~~`/thinking`~~ — Done.
2. ~~`/persona`~~ — Done.
3. ~~**`/model`**~~ — Done. Dynamic catalog rows, `Rc<DataResponse` caching via generation counter, `ChildBuilder` API for index-free tree construction.
4. **Root `/` list** — `StaticListingNode` children for all commands.
   Extends the REPL handle lifecycle to all `/` prefixes.
5. **Step 7** — two-phase submit confirm.
6. **Step 8** — persistent engine cleanup.
7. **Steps 9–14** — Session, Tree, Settings, Sandbox, Stat, Final cleanup.

---

## 1. Crate layout (current, pre-reshape)

```
crates/slashdot_menu/
  src/
    lib.rs
    node.rs          # MenuNode descriptor, FieldSpec
    node_impl.rs     # SlashDotNode trait, StaticListingNode, public types
    node_entry.rs    # NodeEntry (internal wrapper, traversal)
    menu.rs          # SlashDotMenu, SlashDotHandle (public API)
    value.rs         # Value, ValueKind, ModifierSpec
    grammar.rs       # path tokenizer
    filter.rs        # field filters + sorting (§7.1)
    cursor.rs        # cursor cache, selection validation
    interpreter.rs   # state machine (v2.0, being replaced)
    accept.rs        # accept phase + confirm state
    provider.rs      # ColumnDef (DataProvider absorbed into SlashDotNode)
    render.rs        # RenderPlan, paint()
    table.rs         # Table builder (render_multiline, render_header)
    fuzzy.rs         # fuzzy ranking
    prompt.rs        # PromptBuffer trait
    fields.rs        # FieldSpace, row_field_value()
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

### Step 1: Introduce `SlashDotNode` trait + public types + `SlashDotMenu`

**Done.** Pure additive — nothing ported yet.

### Step 2: Epoch wrapper + `SlashDotHandle`

**Done** as part of step 1.

### Step 3: Port the grammar + filter engine as free functions

**Done.** Grammar, filter, fuzzy are already free functions.

### Step 4: Implement default `traverse` body

**Done.** 22 tests. Section parse, key handling, cursor, Esc ladder,
render plan.

### Step 5: Merged child sets

**Done.** 4 tests. Child iteration, field metadata, submit dispatch,
row uniformity.

### Step 6: Port existing commands to `SlashDotNode`

**In progress.**

6a. ~~`/thinking`~~ **Done.** `StaticListingNode` parent + single
`ThinkingEnumNode` (returns all levels from `data()`). Shared state via
`Rc<RefCell<Option<String>>>`. Custom table heading: "Thinking Level".
12 tests.

6b. ~~`/persona`~~ **Done.** `StaticListingNode` parent + single
`PersonaEnumNode` (returns all personas + "none" from `data()`). Shared
state via `Rc<RefCell<SettingsStore>>`. Custom table heading:
"Persona | Description". 8 tests.

6c. ~~`/model`~~ **Done.** `StaticListingNode` parent (columns + field
metadata) + `ModelNode` child (dynamic catalog rows). Rc-cached
`DataResponse` keyed by `catalog_generation()` atomic counter. Shared
state via `Rc<MenuShared>`. Active model marker from settings. 7 tests.

6d. **Root `/` list** — `StaticListingNode` children for all commands.
Extends REPL handle lifecycle to all `/` prefixes. Generalize
`DoneSuccess` dispatch as more commands are added.

**Tests:** 12 thinking + 8 persona + 7 model = 27 command tests.

### Step 7: Two-phase submit confirm

Port the `confirm` flow from `accept.rs` into the node model.

### Step 8: Host epoch wrapper + persistent engine

Replace the per-keypress engine rebuild with a persistent node tree.

---

## 4. Command migrations (on the reshaped engine)

### Step 9: Session

### Step 10: Tree + `/compact`

### Step 11: Settings

### Step 12: Sandbox

### Step 13: Stat + Replay

### Step 14: Final cleanup

(See previous versions for detailed per-step specs.)

---

## 5. Test strategy

| Category | Count | Status |
| --- | --- | --- |
| Grammar / filter / fuzzy / table / render / wrap / value / prompt | 75 | Unchanged (v2.0 free functions) |
| Accept (confirm, interstitial, editor, new-path) | 8 | Unchanged (v2.0 accept.rs) |
| Interpreter (state/tab/enter/esc, dynamic, field_options) | 26 | Unchanged (v2.0, will be replaced) |
| **v2.5 node_entry (parse, listing, drill, cursor, esc, header, mixed)** | **22** | **New (steps 4–5)** |
| **v2.5 `/thinking` command (drill, submit, cursor, active, filter, esc)** | **12** | **New (step 6a)** |
| **v2.5 `/persona` command (drill, submit, cursor, active, filter, esc, none-clear)** | **8** | **New (step 6b)** |
| **v2.5 `/model` command (rows, cache, columns, fields, submit, traversal, filter)** | **7** | **New (step 6c)** |

Total: 538 lib tests (including non-menu tests). The 26 interpreter tests
test the old code path and will be replaced when commands are migrated.

---

## 6. File changes summary

| File | Reshape changes |
| --- | --- |
| `node_impl.rs` | `SlashDotNode` trait + public types + `StaticListingNode` (with `with_columns()`) |
| `node_entry.rs` | `NodeEntry` (traversal, table rendering with aligned headers) |
| `menu.rs` | `SlashDotMenu`, `SlashDotHandle` (public API) |
| `lib.rs` | Re-exports: `SlashDotNode`, `ColumnDef`, `StaticListingNode`, etc. |
| `interpreter.rs` | Logic moves into `NodeEntry::node_traverse()`; file shrinks or is deleted |
| `accept.rs` | Confirm state machine moves to node-local state; engine dialog service stays |
| `provider.rs` | `DataProvider` trait absorbed; `ColumnDef` remains |
| `render.rs` | `RenderPlan` with `sticky_count`; `paint()` uses `display_width()` for correct padding |
| `table.rs` | `display_width()` made `pub(crate)`; `render_header()` added for aligned headers |
| `cursor.rs` | `RankRefResult` / `rank_row_refs` made `pub(crate)` |
| `grammar.rs` | Unchanged (free functions) |
| `filter.rs` | Unchanged (free functions) |
| `fuzzy.rs` | Unchanged |

| Host file | Changes |
| --- | --- |
| `src/v25_thinking.rs` | `ThinkingValue` + `build_menu()` for `/thinking` and `/persona` |
| `src/v25_persona.rs` | `PersonaValue` node impl (writes to `SettingsStore.default_persona`) |
| `src/v25_commands.rs` | Test-only node impls for engine harness testing |
| `src/repl.rs` | `ActiveOverlay::V25Menu { plan, sticky_count }`, `SlashDotMenu` + handle in input loop, `eval_prompt` key dispatch, sticky header rendering, `DoneSuccess` dispatch by prefix |
