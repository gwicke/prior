# Slash menu v2.5 — implementation plan

Design: `slash_menu_v2.5_design.md`. Prior work: `slash_menu_v2_impl.md`
(Phases 0–7.2 steps 1–3 done).

---

## Status

**Done (v2.5 reshape):** Steps 1–6h + tree phases 1–2 complete. 606 lib tests pass. Tree indent fix: assistant +2 via role, invisible entries don't break turn state.

Steps 1–5: Core engine. `SlashDotNode` trait (all defaults), `NodeEntry`
(internal wrapper), `SlashDotMenu` + `SlashDotHandle` (public API). Full
traversal: section parse, data refresh (Rc::ptr_eq), view derivation
(filter → fuzzy rank → sort), cursor, Esc ladder, leaf/listing dispatch,
render plan. Merged child sets, field metadata, row uniformity.

Step 6 (command porting): `/thinking`, `/persona`, `/model` fully ported.
`/set` fully ported with value-forwarding wrappers, EnumSettingNode,
SettingLeafNode. Root `/` list done — all `/` prefixes route through
v2.5 engine.

**Key additions:**
- **`SlashDotMenuNode`** (menu.rs): thin public wrapper around `NodeEntry`.
  `root()` for the implicit root, `new(impl SlashDotNode)` for leaf nodes,
  `add_child(SlashDotMenuNode)` to build the tree. `From<T: SlashDotNode>`
  for implicit wrapping via `.into()`. `SlashDotMenu::from_root_node(node)`
  replaces the old `SlashDotMenu::new()` + `add_root_child()` pattern.
- `StaticListingNode` (convenience impl in `node_impl.rs`): takes static
  row config (id, description) in constructor. Builder methods:
  `.with_header()`, `.with_active()`, `.with_fields()`, `.with_columns()`.
  Eliminates boilerplate impls for listing-level commands.
- `StaticRow` helper struct for `StaticListingNode` row definitions.
- `ColumnDef` re-exported from `slashdot_menu` for host convenience.
  `ColumnDef::new(field, header)` — use `"id"` for row id,
  `"description"` for description, or any custom field name.
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
- **Closure-parametrized nodes**: `ThinkingEnumNode`, `PersonaEnumNode`,
  `ModelNode` take boxed closures for active/submit, parameterized by
  `Rc<RefCell<SettingsStore>>` or `Rc<RefCell<LiveState>>`. Enables
  `/set` to reuse the same node types with different state sources.
- **`/set` tree**: `build_set_node()` constructs the settings subtree.
  Value-forwarding wrappers for listing/enum settings (model, persona,
  thinking, security_mode, show_reasoning, reasoning_in_context).
  Direct `SettingLeafNode` children for int/freetext settings
  (tool_call_limit, session_resume_replay_turns, llm_compact_prompt).
  `EnumSettingNode` for enum option lists. `SettingLeafNode` returns
  `NewPath` on empty suffix to keep menu open and show header.
- **REPL wiring**: persistent `SlashDotMenu` + `Option<SlashDotHandle>` in
  the input loop. **All `/` prefixes route through v2.5** — the old
  `from_buffer_cursors` dispatcher is no longer on the live path (only
  used in v2.0 tests). `ActiveOverlay::V25Menu { plan }` variant for v2.5
  engine overlay.
- **Fuzzy ranking prefix bonus**: `rank_row_refs` gives +10_000 bonus to
  rows whose ID starts with the query (matching v1.0's
  `fuzzy_filter_bonus`). Ensures exact prefix matches rank above
  subsequence-only matches.
- **`HostKey::MaybeSubmit`**: listing Enter drills and forwards MaybeSubmit
  to child; leaf submits on MaybeSubmit; listing ignores MaybeSubmit.
  Prevents Enter from drilling through multiple listing levels.
- **Cursor follows filter rank**: `cursor_user_moved` flag tracks explicit
  arrow-key navigation. When false (menu entry, typing), cursor re-seeds
  from ranked_rows top. When true, cursor persists through filtering.
  Resets when cursor row drops out of filtered set.
- Catalog generation counter (`model_catalog::catalog_generation()`):
  atomic `u64` bumped on each cache refresh. `ModelNode` compares it to
  its cached `Rc<DataResponse>` to skip rebuild when data is unchanged
  (enables engine's `Rc::ptr_eq` freshness check).
- **RenderPlan with LogicalLine**: three-section structure (`table_header`,
  `detail`, `rows` as `Rc<Vec<LogicalLine>>`). `LogicalLine` is `String`
  with embedded `\n` for wrapped rows. No more per-visual-line
  `RenderLine` entries. V25Menu overlay renders directly with markers and
  backgrounds at render time — O(visible) per keystroke.
- **Cursor clamping**: `MoveCursor` uses `clamp` (no wrap-around). Cursor
  stops at first/last row.
- **PageUp/PageDown**: visual-line-aware page scrolling via `page_delta()`
  helper that counts wrapped sub-lines to determine how many logical rows
  fit in the available screen height.
- **Banner**: `picocode - type / for details` (removed `/help` mention).

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

**Double separator on AsyncOp submit:**
`DoneSuccess` calls `emit_commit` with `sep: true`, then the next
loop iteration calls `display_separator()` again — two blank lines
between the committed command and the next prompt. Common to all
`DoneSuccess` paths (thinking, model, switch, etc.). Fix: either
suppress the agent loop separator after `MenuDone`, or pass `sep: false`
in the `DoneSuccess` handler.

**RenderPlan / rendering:**
- ~~No `Rc<Vec<RenderLine>>` sharing~~ **Done.**
- ~~No table layout / `ColumnDef` in RenderPlan~~ **Done.** Table headings,
  custom columns via `.with_columns()`, sticky headers.
- ~~`RenderPlan.title` is unused — always `None`~~ **Removed.** v2.5 design
  (§6) has no titles. See overlay rendering design below.
- ~~Overlay rendering: LogicalLine, direct render path, PageUp/PageDown, cursor clamping~~ **Done.** See §7.
- No confirm dialog lines — `confirm_pending` is tracked in `EpochState`
  but no dialog text is rendered in the plan.
- Scrolling / cursoring with wrapped multi-line logical lines is broken -
  cursor drifts once scrolling starts
- Vertical space separation between header() and listing menu is not handled
  well when vertical space is tight. Should enforce ratio and and minimum
  number of listing rows visible (say 3).

**PathArgs / Args borrowing:**
- Owned data. Deferred to avoid lifetime complexity in the trait API.

**Row uniformity:**
- Only enforces non-empty `id`. Description optional.

**Field space inference fallback:**
- When `SlashDotNode::fields()` returns empty, `infer_field_space()`
  builds from row data. Commands with known fields should override
  `fields()`.

**Cursor / view state:**
- `cursor_idx` updated in-place and kept in sync. No debug_assert
  enforcing the ID-authoritative invariant.

**State and async review**
- Consider better designs for app state access and async execution

**Prompt rewrite support**
- WIP design doc: slashdot/prompt_rewrite_design.md

---

## Next steps (revised)

1. ~~`/thinking`~~ — Done.
2. ~~`/persona`~~ — Done.
3. ~~**`/model`**~~ — Done.
4. ~~**Root `/` list**~~ — Done. All `/` prefixes route through v2.5.
   `SlashDotMenuNode` API for bottom-up tree construction.
5. **Settings** (`/set`) — Next command to port. Sub-steps:

   0. **Fix `node_data()` deviation.** Design §3.2 specifies
      `NodeEntry::node_data()` as the only way parent nodes access child
      data. Implementation calls `child.inner.data()` directly in
      `refresh_data()`. Fix: add `node_data()` on `NodeEntry`, route
      `refresh_data()` through it. Add protective comments on `inner`
      and in `refresh_data()`.

   1. **Value forwarding in `node_data()`.** Add `value_forwarding: bool`
      flag on `NodeEntry`, exposed via `SlashDotMenuNode::with_value_forwarding()`.
      When true, `node_data()` calls own `inner.data()` for rows, then
      queries children's `node_data()` for `active_ids`, injects them as
      `"value"` field. Generic — no closures needed.

   2. **`LiveState` bundle.** Replace three separate `Rc<RefCell<...>>`
      fields in `MenuShared` with `Rc<RefCell<LiveState>>` holding model,
      persona, thinking_level. `/model`, `/persona`, `/thinking` read/write
      LiveState (transient). `/set` reads/writes `SettingsStore` (persistent).

   3. **Closure-parametrized nodes.** ModelNode, PersonaEnumNode,
      ThinkingEnumNode take `active_fn: Box<dyn Fn()->String>` and
      `submit_fn: Box<dyn FnMut(&str)->Result<(),String>>`. Two
      instantiations: live (→ LiveState) and persistent (→ SettingsStore).
      Shared helpers (`model_row`, `order_models`, columns, fields) reused.

   4. **`/set` tree.** `set` node is `StaticListingNode` with
      Setting|Value|Description columns. Children:
      - Listing settings (default_model, default_persona, etc.):
        `SlashDotMenuNode` with value_forwarding; child is the
        parameterized command node (active from SettingsStore, persist via
        set_value).
      - Enum settings (security_mode, show_reasoning, reasoning_in_context):
        value_forwarding + EnumSettingNode child.
      - Leaf settings (tool_call_limit, etc.): `SettingLeafNode` —
        `data()` returns row with value directly; `submit()` persists;
        `header()` shows current value on drill.

   5. **Move root menu.** `build_menu()` from `v25_thinking.rs` → new
      `src/v25_menu.rs`. Exports `build_root_menu(live_state, settings, shared)`.

   6. **REPL sync.** After DoneSuccess: LiveState → agent rebuild if
      changed; dirty flag for `/set` (existing mechanism). Engine stays
      generic — no `/set`-specific dispatch.

6. **Stat + Replay** — Simple leaf/interstitial commands. **Done.**
6f. **Quit** — Leaf node via `AsyncOp::Quit`. **Done.** 3 tests.
6g. **New** — Leaf node via `AsyncOp::New { name }`. **Done.** 3 tests.
6h. **Clear** — Leaf node via `AsyncOp::Clear { name }`. **Done.** 3 tests.
8. **Session** — Dynamic listing with action children:
   slashdot/session_design.md. **Done** (steps 0–5). Full session
   lifecycle: listing, resume, fork, rename, delete with confirm.
   v2.0 session overlay code removed. 19 unit tests. Rendering
   refinements: header() no background, compact timestamps,
   Table wrap option for single-line listings.
7. **Sandbox** — Mixed static+dynamic children. WIP design:
   slashdot/sandbox_design.md
9. **Tree + compact** — Most complex: dynamic listing with nested
   actions, confirm dialogs, `NewPath` rewrites.
10. **Final cleanup** — Remove `MenuEngine`, `PlanBuilder`,
    `Interpreter`, `AcceptController`, `from_buffer_cursors`, and all
    dead v2.0 code.

### Known gaps (follow-ups)

**Dot modifier auto-complete:** The v2.5 engine applies `.field:value`
filter/sort syntax correctly, but the auto-complete *discovery UI* is
missing. In v2.0, typing `/model .` showed a popup of available field
names (via `Interpreter::field_options()` + `ActiveOverlay::Mod`).
Typing `/model .sort:` showed distinct string values for that field.
This popup is now unreachable — all `/` routes through v2.5, which has
no equivalent.

Primary user: `/model` (already ported, has `fields()` metadata). Can
be tackled as a standalone step, or as part of `/session` or `/tree`
porting if those commands also need modifier completion.

Implementation options:
- A `HostKey` variant (e.g. `ModifierComplete`) that returns candidates.
- A dedicated overlay mode in the engine (e.g. `MenuResult::ModComplete`).
- Host-side detection + engine query (like v2.0's `field_options()`).

---

## 8. Remaining v2.0 code and retirement path

### Current v2.0 overlay variants

| Variant | Used by | v2.5 replacement |
|---|---|---|
| `Menu` | Root list, model, tree list, replay | `V25Menu` (when commands are ported) |
| `Settings` | `/set` settings list | — (not yet ported) |
| `SettingsVal` | `/set <key>` value picker | — (not yet ported) |
| `Stat` | `/stat` interstitial | — (not yet ported) |
| `Sandbox` | `/sandbox` path bindings | — (not yet ported) |
| ~~`Session`~~ | removed | **Done.** Removed (`48b17df`). |
| ~~`SessionAction`~~ | removed | **Done.** Removed (`48b17df`). |
| ~~`Confirm`~~ | removed | **Done.** Removed (`48b17df`). |
| ~~`TreeAction`~~ | `/tree <id>` action menu | **Done.** Ported to v2.5 `TreeListNode` + action children. |
| ~~`SwitchAction`~~ | `/tree <id> switch` | **Done.** `TreeSwitchNode` → `AsyncOp::TreeSwitch`. |
| ~~`CompactSummary`~~ | `/tree <id> compact` summary | **Done.** Three compact nodes → `AsyncOp::TreeCompact`. |
| `Mod` | Dotted modifier completion | — (not yet ported) |
| `PathComplete` | Tab path completion | — (not yet ported) |

### Retirement path

The v2.0 code lives primarily in:
- `slash_menu_bridge.rs` — `MenuEngine`, `MenuShared`, `PlanBuilder`,
  provider implementations (`ModelProvider`, `SessionProvider`, `TreeProvider`)
- `repl.rs` — `ActiveOverlay::from_buffer_cursors()` (the v2.0 dispatcher),
  per-variant rendering/key handling
- `interpreter.rs` — v2.0 `Interpreter` (state machine)

**Status:** Step 1 (root `/` list) is done. All `/` now routes through v2.5;
the `from_buffer_cursors` dispatcher is only reached by v2.0 tests.

**Recommended order for remaining v2.0 retirement:**

1. ~~**Root `/` list**~~ **Done.** REPL routes all `/` through v2.5.

2. **Settings** (step 5): Self-contained, exercises mixed static+enum
   children. Retires `Settings`, `SettingsVal`, and the settings provider.

3. **Stat + Replay** (step 6): Simple leaf/interstitial commands.
   Retires `Stat` overlay, `ReplayDetailProvider`.

4. **Sandbox** (step 7): Mixed static+dynamic children. Retires
   `Sandbox` overlay and `sandbox_bindings` module.

5. **Session** (step 8): Dynamic listing with action children. Retires
   `Session`, `SessionAction`, `Confirm` overlays and `SessionProvider`.

6. **Tree + compact** (step 9): Most complex — dynamic listing with
   nested actions, confirm dialogs, `NewPath` rewrites. Retires
   `TreeAction`, `SwitchAction`, `CompactSummary`, `TreeProvider`.

7. **Final cleanup** (step 10): Remove `MenuEngine`, `PlanBuilder`,
   `Interpreter`, `AcceptController`, `from_buffer_cursors`, and all
   dead v2.0 code.

Each step is independently shippable — the old and new paths coexist via
the `V25Menu` vs `Menu` overlay variants until all commands are ported.

---

## 1. Crate layout (current, post-reshape)

```
crates/slashdot_menu/
  src/
    lib.rs
    node.rs          # MenuNode descriptor, FieldSpec (v2.0 compat)
    node_impl.rs     # SlashDotNode trait, StaticListingNode, public types
    node_entry.rs    # NodeEntry (internal wrapper, traversal)
    menu.rs          # SlashDotMenuNode, SlashDotMenu, SlashDotHandle (public API)
    value.rs         # Value, ValueKind, ModifierSpec
    grammar.rs       # path tokenizer
    filter.rs        # field filters + sorting (§7.1)
    cursor.rs        # cursor cache, selection validation
    interpreter.rs   # v2.0 state machine (dead code on live path)
    accept.rs        # v2.0 accept phase + confirm state
    provider.rs      # ColumnDef (DataProvider absorbed into SlashDotNode)
    render.rs        # LogicalLine, RenderPlan, paint() (v2.0 compat)
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

6d. ~~**Root `/` list**~~ **Done.** All `/` prefixes route through v2.5.
   `SlashDotMenuNode` API for bottom-up tree construction (`root()`,
   `new(impl)`, `add_child(node)`, `From<T>` for `.into()`). Banner
   updated. 2 integration tests ignored (unported `/set`).

6e. **`/stat` and `/replay`** — **Done.** Both are leaf nodes directly
   under root (no listing wrapper). `data()` returns the row for the root
   listing. `header()` renders info when drilled. `/stat` uses `NewPath("")`
   to close on submit. `/replay` sets `AsyncOp::Replay` on `MenuShared`;
   the agent loop dispatches `handle_replay()` in its async context.
   `handle_replay` extracted from the agent's match arm into a standalone
   `pub(crate) async fn` reused by both paths. `session_usage` added to
   `MenuShared`. `v25_menu.rs` module for the root menu builder (moved
   from `v25_thinking.rs`). `prepare()` doc comment clarified: two-phase
   commit only, not for display.

**Tests:** 12 thinking + 8 persona + 7 model + 4 stat + 3 replay + 3 quit + 3 new + 3 clear = 43 command tests.

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

### Step 13: Session

### Step 14: Tree + `/compact`

### Step 15: Final cleanup

(See previous versions for detailed per-step specs.)

---

## 5a. Follow-up: global app state and async (post-migration)

The v2.5 migration exposed two architectural debt items that should be
addressed after all commands are ported:

### Shared state: `MenuShared` ad-hoc syncing

`MenuShared` is a grab-bag of `Rc<RefCell<…>>` cells that the REPL
populates on every `read_input` call (session_usage, current_session_id,
tree, leaf_id, pending_op).  Each new command adds another field and
another sync line.  Longer term, there should be a single app-global
state object — `AppState` or similar — that both the REPL and the menu
nodes reference directly, eliminating the per-field syncing.

### Async operations from sync `submit()`

`submit()` is `&mut self` (sync, single-threaded).  Commands that need
async context (e.g. `/replay` which calls `session.get_branch().await`)
cannot call async code from `submit()`.  Current workaround:
`AsyncOp` enum on `MenuShared` that the agent loop dispatches after
`DoneSuccess`.  This works but adds indirection.

Options for a cleaner design:
- **`AppState` holds the session directly** (Session's internals are
  already `Arc`-wrapped, so it can be shared via `Rc<RefCell>`).
  `submit()` borrows the session reference; the engine can expose a
  `block_on` helper for sync-to-async bridging.
- **`submit()` returns a richer type** (e.g. `SubmitResult::Run(AsyncOp)`)
  that the engine propagates to the host.  The host decides how to
  execute — currently impossible because `MenuResult` carries
  `DonePayload` which is not generic (trait object constraint).
- **Move async logic into the host entirely** — nodes return plain
  `Done(None)` and the host detects which command ran (by prompt
  prefix) and dispatches the async work itself.  Simple but loses
  the "node owns its behavior" property.

---

## 5. Test strategy

| Category | Count | Status |
| --- | --- | --- |
| Grammar / filter / fuzzy / table / render / wrap / value / prompt | 75 | Unchanged (v2.0 free functions) |
| Accept (confirm, interstitial, editor, new-path) | 8 | Unchanged (v2.0 accept.rs) |
| Interpreter (state/tab/enter/esc, dynamic, field_options) | 26 | Unchanged (v2.0, will be replaced) |
| **v2.5 node_entry (parse, listing, drill, cursor, esc, header, mixed)** | **23** | **New (steps 4–5)** |
| **v2.5 `/thinking` command (drill, submit, cursor, active, filter, esc)** | **12** | **New (step 6a)** |
| **v2.5 `/persona` command (drill, submit, cursor, active, filter, esc, none-clear)** | **8** | **New (step 6b)** |
| **v2.5 `/model` command (rows, cache, columns, fields, submit, traversal, filter)** | **7** | **New (step 6c)** |

Total: 571 lib tests + 4 integration tests (2 ignored for unported `/set`).
The 26 interpreter tests test the old code path and will be removed
when v2.0 is fully retired.

---

## 6. File changes summary

| File | Reshape changes |
| --- | --- |
| `node_impl.rs` | `SlashDotNode` trait + public types + `StaticListingNode` (with `with_columns()`) |
| `node_entry.rs` | `NodeEntry` (traversal, table rendering with aligned headers) |
| `menu.rs` | `SlashDotMenuNode`, `SlashDotMenu`, `SlashDotHandle` (public API) |
| `lib.rs` | Re-exports: `SlashDotNode`, `ColumnDef`, `StaticListingNode`, etc. |
| `interpreter.rs` | Logic moves into `NodeEntry::node_traverse()`; file shrinks or is deleted |
| `accept.rs` | Confirm state machine moves to node-local state; engine dialog service stays |
| `provider.rs` | `DataProvider` trait absorbed; `ColumnDef` remains |
| `render.rs` | `LogicalLine`, `RenderPlan` (table_header/detail/rows), `paint()` for v2.0 compat |
| `table.rs` | `display_width()` made `pub(crate)`; `render_header()` added for aligned headers |
| `cursor.rs` | `RankRefResult` / `rank_row_refs` made `pub(crate)` |
| `grammar.rs` | Unchanged (free functions) |
| `filter.rs` | Unchanged (free functions) |
| `fuzzy.rs` | Unchanged |

| Host file | Changes |
| --- | --- |
| `src/v25_thinking.rs` | `ThinkingEnumNode` + `build_menu()` — constructs root `SlashDotMenuNode` tree with thinking, persona, model |
| `src/v25_persona.rs` | `PersonaEnumNode` node impl + `add_persona_child()` |
| `src/v25_model.rs` | `ModelNode` node impl + `build_model_node()` — dynamic catalog rows, Rc caching, columns, fields |
| `src/v25_commands.rs` | Test-only node impls for engine harness testing |
| `src/repl.rs` | All `/` routes through v2.5 (`ActiveOverlay::V25Menu { plan }`). `SlashDotMenu` + handle in input loop. `eval_prompt` key dispatch. V25Menu overlay rendering (LogicalLine direct path, PageUp/PageDown). `DoneSuccess` dispatch by prefix. Cursor clamping. |
| `src/agent.rs` | Banner: `picocode - type / for details` |

---

## 7. Overlay rendering design

This section specifies how the host renders the engine's `RenderPlan` into
the terminal overlay. It replaces the current `paint()` + `overlay_window`
approach with a two-layer design: the engine produces pure data (`RenderPlan` +
`LogicalLine`), and the host applies styling (cursor markers, backgrounds)
and windowing at render time.

### 7.1 Current problems

1. **`paint()` is O(n) per keystroke.** Every `RenderLine` gets formatted
   with ANSI colors, marker characters, and full-width padding — even when
   only 20 lines are visible. A 300-row model list pays 300 format+pad
   operations on every cursor move.
2. **Cursor styling is baked into `paint()`.** The `>` marker and
   `SELECTED.bg` background depend on `plan.selected`, which changes on
   every cursor move. This defeats any attempt to cache the painted output.
3. **Visual vs. logical line mismatch.** `build_render_plan` splits wrapped
   rows into multiple `RenderLine` entries (one per visual sub-line, sharing
   `option` index). But `overlay_window` treats each entry as one visual
   row. When rows wrap, the cursor highlight lands on the wrong line and the
   scroll window drifts.
4. **PageUp/PageDown broken.** They mutate `sel` (a visual-line index),
   but `plan.selected` (a logical-row index) overrides it on the next frame.

### 7.2 Redesigned `RenderPlan`

```rust
/// A single logical row: may contain embedded newlines from table wrapping.
struct LogicalLine(String);

struct RenderPlan {
    /// Table column header (e.g. "Model   Ctx   Coding   Price in").
    /// Single line guaranteed. Changes only on terminal resize.
    table_header: Option<LogicalLine>,
    /// `header()` output: help text, stats, focused-row details.
    /// Changes on cursor move (detail updates). Truncatable vertically.
    detail: Vec<LogicalLine>,
    /// Scrollable data rows. Rc-shared — same pointer on cursor moves.
    rows: Rc<Vec<LogicalLine>>,
    /// Index into `rows` for the focused row.
    selected: Option<usize>,
    /// Indices into `rows` for active rows (`*` marker).
    active_rows: Vec<usize>,
}
```

Key properties:

- **`rows` is `Rc`-shared.** On cursor moves, `rows` is the same pointer
  (`Rc::ptr_eq` is true). The overlay clones the Rc (cheap) and renders
  only the visible window.
- **`detail` changes on cursor move** (e.g. focused-row metadata), but
  `rows` is untouched. The detail section is small (typically 1–5 lines)
  and cheap to re-render.
- **`table_header` is static** (resize-only). Always shown, never
  truncated.
- **`LogicalLine` is just `String`** with embedded `\n` for wrapped
  sub-lines. No per-visual-line entries, no `option` index, no
  `is_marker_row` flag. The `selected` index *is* the logical row index.
- **No `paint()` on the hot path.** The engine produces data; the host
  renders it.

### 7.3 `LogicalLine` format

Each `LogicalLine`'s `.0` string contains the row text with embedded `\n`
for wrapped sub-lines. All sub-lines start with two spaces (the marker
slot: `"> "` or `"* "` or `"  "`). The overlay replaces the first two
spaces of the first sub-line at render time:

```text
"  gpt-4o          $2.50\n  128k context  coding:80  reasoning:no"
 │                          │
 └── marker slot (2 spaces) └── wrap continues, no marker
```

For wrapped rows: the first sub-line gets the marker (`>` or `*` or `  `),
continuation sub-lines always get `"  "` regardless of selection.
`build_render_plan` produces marker-free text; the overlay applies markers
at render time.

### 7.4 Host overlay algorithm

```text
V25Menu overlay rendering:

  // 1. Render table header (always shown, single line).
  if let Some(ref th) = plan.table_header {
      render_line(th, HEADER.bg);
  }

  // 2. Render detail section (truncatable from bottom).
  let detail_budget = max_detail_lines(terminal_height);
  for line in plan.detail.iter().take(detail_budget) {
      render_line(line, HEADER.bg);
  }

  // 3. Render visible data rows, windowed around `selected`.
  let data_budget = remaining_height;
  let visible_rows = window(plan.rows, plan.selected, data_budget);
  for row in visible_rows {
      let is_selected = row.index == plan.selected;
      let is_active = plan.active_rows.contains(&row.index);

      // Split logical line into sub-lines.
      for (j, sub) in row.text.split('\n').enumerate() {
          if j == 0 {
              // First sub-line: apply marker.
              let marker = if is_active { "* " }
                          else if is_selected { "> " }
                          else { "  " };
              render_line(&marker[2..] + sub, bg_for(is_selected));
          } else {
              // Continuation: no marker, same bg if selected.
              render_line(sub, bg_for(is_selected));
          }
      }
  }
```

### 7.5 Scrolling / windowing with logical addressing

```text
fn window(rows: &[LogicalLine], selected: Option<usize>, budget: usize) -> Window {
    let total = rows.len();
    if total <= budget || budget == 0 {
        return Window { start: 0, count: total };
    }
    // Center the window on the selected row.
    let sel = selected.unwrap_or(0);
    let start = sel.saturating_sub(budget / 2).min(total - budget);
    Window { start, count: budget }
}
```

The `start` and `count` index into `rows` directly (logical rows).
The overlay then renders each logical row's sub-lines, tracking the visual
line count to stay within `budget`. If a logical row's sub-lines overflow
the budget, it is truncated (last sub-line replaced with `…`).

### 7.6 Cursor move = O(visible)

On `MoveCursor`, the engine returns a `RenderPlan` where `rows` is the
same `Rc` (no rebuild), `selected` changed, and `detail` may have changed.
The overlay:

1. Re-renders `table_header` (1 line, O(1)).
2. Re-renders `detail` (typically 1–5 lines, O(1)).
3. Re-renders `budget` visible rows from `rows[start..start+count]`
   (O(budget), typically ~20 lines).

Total: ~25 format operations per cursor move, regardless of list size.
Compare to current: 300+ operations per cursor move.

### 7.7 PageUp / PageDown

With logical addressing, PageUp/PageDown move `selected` by a page size
and clamp:

```text
selected = (selected ± page_size).clamp(0, rows.len() - 1);
```

The overlay re-renders with the new `selected`. No separate page-scroll
path — it is the same cursor move with a larger delta.

### 7.8 Terminal resize

On resize, `term_width` changes, which triggers a full `build_render_plan`
rebuild (the engine's `RenderCache` invalidates on `term_width` change).
The overlay re-renders everything — table column widths, wrapping, detail
truncation all depend on width. This is correct and unavoidable: column
widths change, so every line's text changes.

### 7.9 Migration path

1. **Add `LogicalLine`, new `RenderPlan` shape.** Keep old `RenderLine` and
   `paint()` for non-V25Menu overlays (Settings, Tree, etc.).
2. **Implement new overlay render path** in `repl.rs` for `V25Menu`. This
   is host-side only — no engine changes beyond the `RenderPlan` shape.
3. **Port `build_render_plan`** to emit `LogicalLine` (merge wrapped
   sub-lines with `\n`).
4. **Remove `paint()` from V25Menu path.** Old overlays still use it.
5. **Port remaining overlays** (Settings, Tree, etc.) to `LogicalLine`
   over time.

### 7.10 Test strategy

| Test | What it validates |
|---|---|
| Logical line wrapping | Multi-line row renders as single `LogicalLine` with `\n` |
| Window centers on selected | `selected=50, budget=20` → window around rows 40–59 |
| Marker on first sub-line only | Wrapped row: `>` only on first sub-line, continuation gets `"  "` |
| Active + selected same row | `*` wins over `>` per §5.2 precedence |
| Detail updates on cursor move | `rows` Rc unchanged, `detail` changed |
| PageUp/PageDown | `selected ± page_size`, clamp, re-render |
| Resize triggers rebuild | `term_width` change → new `Rc` for `rows` |

