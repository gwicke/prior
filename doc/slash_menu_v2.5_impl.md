# Slash menu v2.5 — implementation plan

Design: `slash_menu_v2.5_design.md`. Prior work: `slash_menu_v2_impl.md`
(Phases 0–7.2 steps 1–3 done).

---

## Status

**Done (v2):** Phases 0–7.2.1–3. Uniform key loop; Model/Thinking/Persona/
CommandList collapsed to single `Menu` overlay. Engine owns cursor, active
marker, Esc ladder, field filtering. Host has ~8 overlay variants remaining.

**Next:** v2.5 reshape (§3 below), then command migrations on the reshaped
engine (§4).

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
| `MenuNode` data struct + `Interpreter` central | `NodeImpl` trait impls + blanket `traverse()` |
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

### Step 1: Introduce `NodeImpl` trait + types

Define the core types from §2 of the design. This is pure additive —
nothing is ported yet.

```rust
// crates/slashdot_menu/src/node_impl.rs

pub struct PathArgs {
    pub ids: Vec<String>,
    pub suffix: std::ops::Range<usize>,
    pub args: Option<Args>,
}

pub struct Args {
    pub modifiers: Vec<(String, Value)>,
    pub positional: Vec<String>,
}

pub struct DataResponse {
    pub rows: Vec<Row>,
    pub active_ids: Vec<String>,
}

pub struct DataError { pub message: String }
pub type DataResult = Result<Rc<DataResponse>, DataError>;

pub enum SubmitResult {
    Done,
    Prepared,
    NewPath(String),
}

pub enum MenuResult {
    Continue(Rc<RenderPlan>, String),  // render plan + new prompt
    DoneSuccess,
    DoneError(String),
}

pub enum HostKey {
    Tab, Enter, Esc,
    MoveCursor(i32),   // +1/-1 for arrows, ±N for pgup/pgdn
    Seed(usize),       // open-time cursor seeding
}
```

Define the base trait (overridable members only):

```rust
pub trait NodeImpl {
    fn data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult;
    fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult;
    fn prepare(&mut self, epoch: u64, path_args: &PathArgs)
        -> SubmitResult;
    fn header(&mut self, path_args: &PathArgs) -> Vec<String>;
}
```

Defaults: `data()` → empty, `submit()` → `Done`, `prepare()` →
`self.submit(epoch, path_args)`, `header()` → empty.

**Verify:** `cargo build -p slashdot_menu`. No tests yet — just the types
compile.

### Step 2: Blanket impl skeleton + epoch

Define the extension trait with `traverse()` and `traverse_data()` as the
public API. Implement via blanket impl over `NodeImpl`. The default body
for `traverse()` starts as a stub that returns `DoneError("not yet
ported")`.

```rust
pub trait NodeTraversal {
    fn traverse(&mut self, epoch: u64, key: HostKey, path_args: &PathArgs)
        -> MenuResult;
    fn traverse_data(&mut self, epoch: u64, path_args: &PathArgs)
        -> DataResult;
}

impl<T: NodeImpl> NodeTraversal for T {
    fn traverse(&mut self, epoch: u64, key: HostKey, path_args: &PathArgs)
        -> MenuResult {
        // Stub — filled in step 4
        MenuResult::DoneError("not yet".into())
    }
    fn traverse_data(&mut self, epoch: u64, path_args: &PathArgs)
        -> DataResult {
        self.data(epoch, path_args)
    }
}
```

The epoch wrapper on the host side: a simple counter bumped when the buffer
transitions to `/`, held for the activation. This replaces the
`Activation` bundle.

**Verify:** `cargo build`. No test changes needed.

### Step 3: Port the grammar + filter engine as free functions

The grammar tokenizer (`grammar.rs`) and filter/sort engine (`filter.rs`)
are already free functions. They stay as-is — the default `traverse` body
will call them. No code changes here; just verify the boundary:

- `grammar::parse_level(suffix: &str) -> (terms, modifiers, selection)`
- `filter::apply_query(rows, query, sort) -> Vec<usize>` (ranked indices)
- `fuzzy::rank_and_pin(candidates, terms) -> Vec<usize>` (ranked, pinned)

These are called by the default `traverse` body (step 4) and by `data()`
implementations (the node's own preprocessing). Nothing changes in their
signatures.

### Step 4: Implement default `traverse` body

This is the core step. Port the interpreter's logic into the blanket impl's
default `traverse()` body, following the algorithm in §2 of the design
("Traversal algorithm").

**Sub-steps:**

4a. **Path parsing + child dispatch.** The default body calls
`grammar::parse_level(suffix)` to get terms/modifiers/selection. For listing
nodes (those with children), it resolves the selection against the merged
child set (step 5) and recurses into the matching child with updated
`PathArgs`. For leaf nodes (no children, has `submit`), it handles the key
directly.

4b. **Key handling at the listing level.** Following the design's traversal
algorithm:
- `no key` / `MoveCursor`: return `Continue(RenderPlan, prompt)`.
- `Tab`: expand focused ID, append space, push frame, call child's
  `traverse()`.
- `Enter`: as Tab, but if child is leaf, call `prepare()` → handle
  `SubmitResult`.
- `Esc`: per §5.1 ladder.

4c. **Key handling at the leaf level.**
- `no key`: return `Continue(RenderPlan, prompt)`.
- `Enter`: call `prepare()` (which defaults to `submit()`). If
  `Prepared`, render confirm dialog; subsequent Enter calls
  `submit()`, Esc cancels.
- `Esc`: pop per §5.1.

4d. **Cursor cache.** Each node stores `{ selected_id, filter_index }`.
Keyed on `(epoch, terms_hash)`. Reset on epoch increment or terms change.
Cursor seeding: first active row on fresh open.

4e. **Esc ladder.** Port the current interpreter's Esc logic: trailing
space → frame replay → clear terms → clear selection → root clear.
Always a root re-entry with the shortened buffer (never up-recursion).

4f. **RenderPlan emission.** Port from `render.rs`. The plan carries:
- `rows: Vec<Rc<RenderLine>>` (Rc'd for zero-copy sharing)
- `selected_row: usize`
- `active_rows: Option<Vec<usize>>`
- `header: Vec<String>` (from `header()` + table column headers)
- `table: Option<ColumnDef>`

**Tests:** Port the ~50 prompt/key-level scenario tests from
`interpreter.rs` to the new `traverse()` harness. Same scenario tables,
new entry point: `node.traverse(epoch, key, path_args) → MenuResult`
instead of `interpreter.tab_at(line, sel)`. Cursor-map internals tests
become per-node cursor tests.

### Step 5: Merged child sets

Extend the default `traverse` body to concatenate rows from multiple
children per level.

5a. **Child iteration.** The default body calls `traverse_data()` on each
child in declaration order, concatenating `DataResponse::rows`. Each row
is tagged with its child index (internal `child_index: u16`, not in `Row`
API).

5b. **Field metadata on the parent.** Add a `fields: Vec<FieldSpec>` to the
parent node's trait (or a shared metadata struct). Children conform to
these fields; missing values use §7.1 missing-value rules.

5c. **Submit dispatch.** On Tab/Enter, the engine reads the child index
from the cursor row's tag and dispatches to the corresponding child's
`submit()`.

5d. **Row uniformity.** Enforce at least `id` + `description` across all
children of a level. This is a debug assert, not a compile-time check.

**Tests:** Write a mixed-level test: static `add` child + dynamic data
child in one level (the `/sandbox` shape). Verify merged listing, correct
submit dispatch, and cursor continuity across the boundary.

### Step 6: Port existing commands to `NodeImpl`

Migrate the already-engine-backed commands (Model, Thinking, Persona,
root list) from descriptor+handler-registration to `NodeImpl` impls.

For each command:
- Replace `MenuNode` + `DataProvider` + `LeafHandler` registration with an
  `impl NodeImpl` struct.
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
`root.traverse(epoch, Seed(0), ...)` → check `RenderPlan`.

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

8a. The host holds one root `NodeImpl` instance for the lifetime of the
app. Per activation: bump epoch, call `root.traverse(epoch, key, ...)`.

8b. Delete the `Activation` bundle and `take_state`/`restore_state`. Per-
activation state (cursor, frames, data cache) lives inside the nodes,
invalidated by epoch.

8c. The host's `ActiveOverlay::Menu` carries only the `RenderPlan` from the
last `traverse()` call. The host renders it verbatim.

**Verify:** `cargo build -p slashdot_menu` in isolation. Host compiles.
Existing integration tests pass.

---

## 4. Command migrations (on the reshaped engine)

Each migration replaces a bespoke `ActiveOverlay` variant + host-rendered
`display_rows` + host-cursed accept with an `impl NodeImpl` + generic
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

| Category | Count (approx) | Status |
| --- | --- | --- |
| Grammar / filter / fuzzy / table | ~45 | Unchanged (free functions) |
| Prompt/key scenario (state/tab/enter/esc) | ~50 | Port to `traverse()` harness (step 4) |
| Per-node cursor | ~10 | New (step 4d) |
| Mixed-level submit dispatch | ~5 | New (step 5) |
| Confirm two-phase | ~8 | Port from `accept.rs` (step 7) |
| Per-command integration | ~20 | Port per migration (steps 9–13) |
| Epoch invalidation | ~3 | New (step 8) |

Total: ~140 tests. ~45 unchanged, ~50 mechanical port, ~45 new/ported.

---

## 6. File changes summary

| File | Reshape changes |
| --- | --- |
| `node.rs` | `MenuNode` → `NodeImpl` trait; merged child set; field metadata |
| `interpreter.rs` | Logic moves into blanket `traverse()` default body; file shrinks or is deleted |
| `accept.rs` | Confirm state machine moves to node-local state; engine dialog service stays |
| `provider.rs` | `DataProvider` trait absorbed into `NodeImpl::data()`; file shrinks |
| `render.rs` | `Rc`'d row data in `RenderPlan`; otherwise unchanged |
| `cursor.rs` | Cursor cache moves to per-node state; file shrinks or is deleted |
| `grammar.rs` | Unchanged (free functions) |
| `filter.rs` | Unchanged (free functions) |
| `fuzzy.rs` | Unchanged |
| `table.rs` | Unchanged |
| `lib.rs` | New re-exports for `NodeImpl`, `NodeTraversal`, `HostKey`, etc. |
| `node_impl.rs` | **New** — core types (`PathArgs`, `Args`, `DataResponse`, `SubmitResult`, `MenuResult`) |
