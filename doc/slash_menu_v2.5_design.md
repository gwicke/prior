# Slash menu v2.5 — design

Derived from the brainstorm/audit in `slash_menu_v2.md` (which contains the full
audit of the current implementation, the issue list, and the inline review that
produced decisions D1–D8 and resolutions R8-1…R8-8). This document states the
resolved design. Implementation staging will go into a separate plan document.

**v2.5 revision (this document).** Forked from `slash_menu_v2_design.md` as a
verbatim copy; the node model is revised to the trait-node / merged-child-set
design discussed in `slash_menu_v2_design_simplification.md` (the round 1–3
review, including verified borrow-shape analysis, is recorded there; this
document is the distilled end state). Phases 0–7.1 of the implementation
(`slash_menu_v2_impl.md`) already run on the v2 descriptor engine; v2.5 changes
*where rows and handlers live*, not the prompt grammar, cursor/accept
semantics, rendering contract, or host role split — those sections are
inherited verbatim. Changed sections:

1. **§2 (rewritten)**: nodes become trait implementations (`SlashDotNode`) whose
   default traversal is private and non-overridable and owns per-node state
   (epoch-cached), replacing the data descriptor + centralized interpreter
   side tables (providers/handlers registered by path string).
2. **§2.1 (new)**: merged child sets — one level mixes static, enum/value,
   and dynamic child nodes; each row-producing child owns its data source and
   submit handler; the parent declares the field metadata all children
   conform to.
3. **§9 (confirm part)**: the confirm flow is a two-phase submit protocol —
   the yea/nay dialog is a not-overridable engine service, the state machine
   is handler-local — replacing v2's two-handler ConfirmPrep machinery.
4. **§8 (binding half)**: the one-shot/live-filter split stays; providers
   attach to nodes instead of path-keyed side tables.

---

## 1. Goals and invariants

1. **Node-driven.** Every slash menu is a tree of *node implementations*
   (`SlashDotNode`, §2). A single generic, non-overridable traversal (the
   engine) derives all behaviour from the nodes' rows and submit/data
   members.
2. **The prompt is the state** (revised): the buffer determines level, filter
   state, and the *committed* selection — but the view cursor and traversal
   memory live outside the buffer, alongside cached row data (§8). The revised
   model commits ids via Tab/Enter and keeps refinement ephemeral (restored via
   Esc).
3. **Uniform per-level grammar.** The same token rules apply recursively at
   every level; no menu-specific parser.
4. **Surface changes are allowed where they buy uniformity** (D1): dropping
   the `<…>` id disambiguator (D2) and single-token child commands (R8-1).

Out of scope: bare-prompt path completion, `!cmd`/`!!cmd` shell affordances,
and general `Esc`-clears-buffer semantics outside menus. These stay adjacent
to, not part of, the menu system.

---

## 2. Core model: the trait node

**Architecture: thin client.** The engine owns all menu data, cursor state,
and tree structure. The host is a display + input forwarding layer — it
renders the `RenderPlan` the engine produces and forwards key events back.
Think of a web browser rendering server-generated HTML: the host never
computes menu state, never holds cursor positions, never rebuilds the tree.

A menu is a recursive tree of **nodes**, and a node is a *trait
implementation*, not a data structure. One node impl per command (or per
sub-level); the root list (`/`) is itself a node whose children are the other
command nodes.

### Extension points

**The engine owns everything generic; per-node code only
customizes data and submission.** A command is one `impl SlashDotNode`.

`data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult | DataError`
- The rows this node contributes to its parent's listing. Default: none.
- Return `Rc<DataResponse>` so the parent can check `Rc::ptr_eq` for
  cheap "nothing changed" signals. Caching decisions are the handler's.

`submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult`
- The leaf accept behavior. Default: none (node is a listing; Enter drills).

`prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult`
- Optional confirm hook; default = `submit()`. An override runs the prep
  phase — may open an external editor (via a callback captured at
  construction), precompute, stage the payload — then returns
  `SubmitResult::Prepared`. Staged payload lives in the node's private state
  (§2), accessible to `submit()` on yea.

`header(&mut self, path_args: &PathArgs) -> Vec<String>`
- Non-selectable header lines (replaces v2 DetailProvider). Default: none.
  The engine truncates based on terminal size (§6).

### Core types

```rust
struct PathArgs {
    ids: Vec<String>,                          // traversal path IDs
    suffix: std::ops::Range<usize>,            // byte range into prompt
    args: Option<Args>,                        // parsed modifiers + positional args
}

struct Args {
    modifiers: Vec<(String, Value)>,
    positional: Vec<String>,
}

struct DataResponse {
    rows: Vec<Row>,
    active_ids: Vec<String>,  // empty = no active row
}

struct DataError { message: String }
type DataResult = Result<Rc<DataResponse>, DataError>;

enum SubmitResult {
    Done,                    // close the menu
    Prepared,                // confirm flow: pause with yea/nay dialog (§9)
    NewPath(String),         // return a new prompt path (e.g. /compact)
}
```

### Encapsulation: internal `NodeEntry` wrapper

Non-overridable machinery: path parsing, selection validation (D2 demotion),
fuzzy rank-and-pin, §7.1 filter/sort query, per-node cursor cache, Esc
restore ladder, render-plan emission. Encapsulated by the
`NodeEntry` newtype:

- the host implements `SlashDotNode` (data, submit, prepare, header);
- `NodeEntry` wraps a `dyn SlashDotNode` and adds children, cursor cache, data cache;
- `NodeEntry::node_traverse()` and `NodeEntry::node_data()` are internal methods on the struct;
- the host never sees `NodeEntry` — it interacts only through `SlashDotMenu` / `SlashDotHandle`.

### Per-node state and the epoch

Each node owns private local state:

- a **cursor cache**: selected row ID (+ memoized index);
- **view state**: the level's `view_args` — the last rendered prompt
  section, also the view cache key — and `should_restore_view_args`
  (set on drill, consumed on restore, §5.1);
- a **derived data cache**: one activation; reuses unchanged `Rc` signals
  from `data()` (§8);
- optionally richer state (e.g. confirm payload, §9).

All caches invalidate on **epoch** increment: a menu-invocation counter the
host increments on menu open. Multi-stage flows (compaction) key on their own
decision points, not epoch alone.

The host gets a `SlashDotHandle` from `SlashDotMenu::get_handle()` (bumps epoch).
The handle exposes `eval_prompt(prompt, key) -> MenuResult` — the only way for
the host to interact with the menu. The epoch is passed to `SlashDotNode` methods
so nodes can invalidate their own caches.

### Traversal algorithm

The traversal is recursive: the parent parses its level (dot modifiers +
words checked against child IDs, building `Args`), recurses into the
matched child with its unconsumed tail, or — if no child ID matched —
renders its own listing from the parsed view args. A descending parent
returns the child's result unmodified (pass-through, §2 "Parent→child
API"); leaves own the newprompt.

**Listing node (children, no submit):**
- Refresh row cache via `node_data()` on each child; check `Rc::ptr_eq`.
- No child ID match (or child ID with trailing space):
  - `no key` / `MoveCursor`: render listing `MenuResult::Continue(RenderPlan, prompt)`.
    Rendering a listing that is not *exactly* one valid ID clears
    `should_restore_view_args` (§5.1).
  - `Tab`: replace the level's view args with the cursor row's ID and a
    trailing space (drill), set this node's `should_restore_view_args`,
    append the ID to `PathArgs.ids`, recurse into that child with the ID
    as its prompt.
  - `Enter`: as Tab, but forward key; child may `submit()` immediately if it
    is a leaf.
- Child ID match, no trailing space:
  - `should_restore_view_args` set (this node drilled earlier): undo the
    drill per §5.1 — seed the cursor on the ID's row and replace the ID
    in the prompt with this node's stored view args.
  - Otherwise: render this node's own listing (cursor on the ID's row);
    the child level's listing is hidden. No recursion.

**Leaf node (no children, submit handler):**
- Display `header()` via RenderPlan.
- `Enter`: run `prepare()` → default returns `submit()`. If `Prepared`,
  render confirm dialog; subsequent `Enter` calls `submit()` (yea) or Esc
  (nay, pop level). Confirm dialog is a non-overridable engine service.
- `Esc`: restore ladder per §5.1; also cancels any confirm dialog.

`node_traverse()` returns `MenuResult::{ Continue(Rc<RenderPlan>, newprompt),
DoneSuccess, DoneError }`. `RenderPlan` wraps bulk row data in `Rc` so cursor
movement shares unchanged content — only marker positions (`selected_row`,
`active_rows`) re-allocate.

### Parent→child API (implementation-internal, private)

`NodeEntry::node_traverse(&mut self, epoch, HostKey, PathArgs) -> MenuResult`
- `HostKey`: `Tab`, `Enter`, `Esc`, `MoveCursor(n)`. The node at the end
  of the traversal handles all logical key actions.
- `PathArgs.ids`: traversal path — the IDs drilled so far, one entry per
  level, consumed by leaf `prepare()`/`submit()`.
- `PathArgs.suffix`: byte range into the prompt (preserves trailing
  whitespace). `PathArgs.args`: modifiers + positional args, only on leaf
  nodes.
- Return is **pass-through on the way up**: a parent that descended
  returns the child's `MenuResult` unmodified. Leaves own the newprompt;
  `NewPath` rewrites the buffer and traversal restarts at root.

`NodeEntry::node_data(&mut self, epoch, PathArgs) -> DataResult | DataError`
- Parses args from suffix, calls `SlashDotNode::data()`.

### Borrow discipline

Parent-state access happens only outside the child loop (three phases:
own-state → descend → post-processing). Cross-level data flow is passed down
as arguments, never read sideways.

### Node sharing

Reuse a node impl by *instantiating* it per parent (e.g. `/model` and
`/set default_model`); no `Rc` sharing — plain local per-node state.

---

## 2.1 Merged child sets (new in v2.5)

A level's listing is the **ordered concatenation of all child nodes' rows**,
however each child produces them (static, enum, or dynamic). The v2
static/dynamic level distinction dissolves — only a set of children, each with
its own row source and submit handler.

- **Row uniformity**: at least `id`, optionally `description` and more. IDs
  unique across a level.
- **Field metadata on the parent**: declared once; children conform. Missing
  fields use §7.1 missing-value rules.
- **Ordering**: `addChild()` order, unless parent re-sorts.
- **Selection**: merged set is the candidate set; D2 demotion checks merged.
- **Submit dispatch**: parent tags each row with its owning child index during
  merge (internal `child_index: u16`); Tab/Enter reads the tag and dispatches.
  Internal to the traversal body, not in `Row` API.
- **Dynamic children may carry static children** (unchanged v2 encapsulation).

### Reference shapes

| Command | Shape |
| --- | --- |
| `/set` | Mixed: static setting children + dynamic child reusing model rows with own submit. |
| `/model` | Single dynamic child (catalog rows), field metadata on level, leaf = select model. |
| `/sandbox` | Mixed: static `add` row + dynamic binding rows. |
| `/tree` | Dynamic child (tree rows) + static action level after id. |

---

## 3. Path grammar (per level, recursive)

```
[parent tokens] [fuzzy search terms] [dot modifiers] [selection token]
```

- **Fuzzy terms**: free words filtering candidates. Last word that exactly
  matches a candidate *is* the selection (replaces `<…>`).
- **Dot modifiers** (§7): `.key:value`; value consumes until next modifier /
  valid id / end of prompt.
- **Selection token**: space-free; validated against merged candidate set (D2).
  Enters only via Tab/Enter (§4, §5).
- **Trailing space**: drill in. No trailing space = sub-menu hidden (the
  level renders its own listing, cursor on the ID; the child level below
  is not shown). Uniform for all listing levels. Host must not keep
  per-command "open without trailing space" lists.
- Dynamic nodes encapsulate two grammar levels when static sub-actions exist.

Parsing is structural (terminators: next dot-modifier, valid id, or end-of-
prompt); fs-style completion is a modifier behavior (§7), not a level signal.

---

## 4. Selection & cursor semantics

- **Cursor lives in the view; prompt holds the commit.** Up/down moves the
  view cursor only.
- **Tab/Enter act on the cursor row.** Writes that row's id into the prompt
  (drill, §5).
- **Drill clears refinement.** Terms and modifiers are ephemeral; replaced
  by the cursored ID. The pre-drill view args are remembered per node and
  restored via §5.1 on the way back up.
- **Exact match ⇒ selected; unique fuzzy match ⇒ selected** (one generic
  rule).

---

## 5. Key contract (Tab / Enter / Esc)

- **Tab**: bare level → promote (trailing space, same as Enter); otherwise
  complete cursor row's id.
- **Enter**: if `submit` handler, invoke it (with `prepare` if declared);
  otherwise Tab-drill.
- **Esc**: back navigation via §5.1 restore ladder.
- **Drill**: Tab/Enter replaces the level's view args with the cursored ID
  + trailing space and recurses into that child; the previous view args
  are remembered in per-node state for the §5.1 restore.
- **Edit invalidation**: scoped to the edited level; trailing-space removal
  is navigation, not edit.
- **`*` active marker**: always from application context, never typed selection.

### 5.1 Restore ladder

Each step applies only when the next does not. There is **no traversal
stack** (that "frame" language is a hold-over from the previous
non-recursive design): the prompt is the state; each node remembers its last
rendered view args in `view_args` (per-node, updated whenever a listing
of this node is rendered — drilled or not) and a `should_restore_view_args`
boolean (set when this node drills into a child, consumed once).

1. **Restore (no key needed).** A level whose prompt section is exactly
   one valid child ID, no trailing space, and `should_restore_view_args`
   set, means the user drilled in (Tab/Enter) and navigated back out.
   Undo the drill in one step:
   - Seed the cursor on the row named by the ID (the ID becomes the
     focused row, so the user can immediately add filter terms or
     re-drill).
   - Replace the ID in the prompt with this node's stored `view_args`
     (e.g. `/model claude-4 ` → Esc → `/model sonnet`).
   - Clear `should_restore_view_args`.
   Without the boolean (the ID was typed by hand, e.g. `/model` letter
   by letter), nothing is rewritten — the level renders its own listing
   with the cursor on the ID's row; the child level's listing is hidden.
2. **Clear refinement.** Fuzzy terms, modifiers, and selection all
   dropped (e.g. `/model sonnet` → Esc → `/model `). Cursor position is
   kept.
3. **Strip trailing space** → the parent level becomes responsible for
   the prompt again (its restore step 1 then applies on the next Esc).
   Root: clear entire buffer.

### 5.2 Active-state sourcing

The `*` row comes from a uniform mechanism — host never computes it:

- Dynamic levels: `data()` returns `active_ids: Vec<String>`; engine resolves
  to row indices at render time.
- Static/enum levels: node's `data()` supplies the active id from live app
  state; engine resolves identically.
- `RenderPlan.active_rows: Option<Vec<usize>>` vs `selected_row` (cursor).
  Host renders `*` and `>` with no per-variant logic.
- Open-time cursor seeds on first active row; otherwise row 0.

---

## 6. Rendering

- **Panel structure**: optional `header()` rows, table header row (if child
  rows), selectable rows with `>` (cursor) / `*` (active) markers.
- **Headers** (R8-4): *(i)* `header()` — help text, stats, previews;
  truncated to fit terminal. *(ii)* Static table headings from columndefs;
  fixed position when scrolling.
- **Tables**: declarative columndefs (D6). Existing knobs sufficient.

---

## 7. Modifiers & value completion

One mechanism, dot-prefixed, at every level (D3). Static children remain bare
words. Multi-word modes → hyphenated tokens (R8-1).

| Property | Notes |
| --- | --- |
| `key` | Dot token name (`.sort`, `.prompt`, `.ro`, …). |
| value kind | `string` / `number` only. |
| enum set | Orthogonal to kind; constrains completion only; backend validates at accept. |
| completion source | `dataset-derived` (default, lazy extraction), `dynamic handler` (fs paths), `default value`. |
| front-end / back-end | Front-end: engine-applied over cached rows (§7.1). Back-end: passed to live-filter handler. `data()` controls which. |

### 7.1 Field filtering & sorting

| Token | Semantics |
| --- | --- |
| `.field:value` | Exact match (string field). |
| `.field:>v` / `.field:<v` | Numeric comparison; `>=`/`<=` likewise. Drops missing. |
| `.sort:field` | Direction defaults to field's `default_dir`. |
| `.sort:>field` / `.sort:<field` | Explicit desc/asc prefix; `<>` stripped during fuzzy. |

Rules: filters AND, apply before sort; fuzzy narrows on top. Single sort
(later `.sort:` overrides). Missing values: excluded by filters, sort last.

---

## 8. Data sources

Two handler kinds (R8-6):

1. **One-shot**: called once per menu open; rows cached; engine filters/sorts.
2. **Live-filter**: takes modifier inputs; returns rows or "not changed";
   primarily for fs paths.

Binding (v2.5 change): the row source **is the node** — `data()` is its fetch,
`active_rows` reported in data response. No registration, no string-keyed
lookup; application context captured at construction.

### 8.1 Row fields

- `type`: `string` / `number`.
- `filterable` / `sortable`: opt-out flags (default on).
- `default_dir`: default sort direction.
- **Computed fields**: pre-derived values (ratios, age) so no special-cased
  sort names (§7.1).
- **Display-string override**: table display only; ignored by filter/sort.
- `id` and `description`: implicit fields, always filterable/sortable/fuzzy.

Preprocessing: one pass per one-shot fetch to extract distinct dataset string
values for modifiers without a pre-defined enum (D7).

---

## 9. Accept flows

1. **Confirm (two-phase submit)**: `prepare()` → `Prepared` → engine renders
   confirm dialog (non-overridable service) → `submit()` on yea. State
   machine is handler-local (node-private, §2). Cancel (Esc / buffer rewrite)
   drops staged payload.
2. **Editor handoff**: the node's `prepare()` opens an external editor
   via a callback captured at construction (the engine crate is
   generic; the picocode-side node impl captures `open_editor` as
   private state). Blocks, stores result, returns `Prepared`. Engine
   never knows about `$EDITOR`.

**SubmitResult**: `Done` (close), `Prepared` (pause for confirm),
`NewPath(String)` (buffer rewrite, e.g. `/compact`).

App-data mutations happen in handlers via shared handles (`Rc<RefCell>`);
the host applies only control flow / terminal / transcript (§12).

---

## 10. Per-command target shape

| Command | Shape |
| --- | --- |
| `/` | Static root list; children = command nodes. |
| `/model` | Dynamic child (catalog); field modifiers (§7.1); leaf = select model. |
| `/thinking` | Enum value child (`off\|low\|med\|high`); shared leaf = SetThinking. |
| `/persona` | Dynamic child with dataset/enum string values. |
| `/set` | Mixed: static setting children + dynamic `default_model` child (§2.1). |
| `/sandbox` | Mixed: static `add` + dynamic bindings (§2.1). |
| `/session` | Dynamic → static action children → confirm on delete (§9). |
| `/tree` | Dynamic → static actions (`switch`/`compact`); `llm` = confirm (§9). |
| `/compact` | Leaf: `NewPath` → `/tree <id> compact `. |
| `/stat` | `header()` = stats; no submit. |
| `/replay` | `header()` = help; `submit()` dispatches replay. |

---

## 11. Traceability of resolved questions

| Question | Resolution |
| --- | --- |
| Q1–Q8 | Decisions D1–D8: surface changes for uniformity; `<…>` dropped (D2); dot-modifiers (D3); stat/replay in scope (D4); tables generalized (D6); dataset completion default (D7); fs completion via modifier (D8). |
| R8-1–R8-8 | Single-token labels; structural terminators; confirm two-phase submit (R8-3 → §9); headers from node (R8-4); accept clears filter + traversal memory (R8-5); one-shot vs live-filter + front/back-end modifiers (R8-6); selection space-free, values may contain spaces (R8-7); compact as leaf-returns-path (R8-8). |
| Node model | `SlashDotNode` trait + internal `NodeEntry` wrapper; non-overridable traversal on `NodeEntry`; epoch-cached per-node state; providers on nodes, not side tables (§2, §8). |
| Merged child sets | Ordered union of child rows; field metadata on parent; per-child submit dispatch (§2.1). |
| Cursoring | Up/down moves cursor only; Tab/Enter drill (ID + space into prompt); per-node view_args memory + §5.1 restore on the way up. |
| `*` marker | Always from application context, never typed selection (§5.2). |
| Esc | Per-level restore ladder: restore (ID→cursor + view_args if drilled) → refinement → space → root clear (§5.1). |
| Confirm | Handler-local state machine; engine dialog service (§9). |
| Coordinator | Eliminated — host epoch wrapper (§2, §12). |

---

## 12. Role split: engine vs. host

Per activation the traversal produces, from `(prompt, node-tree, epoch)`:

- resolved level (node path) + drilled/listing state (§3);
- merged candidate rows, ranked/filtered/sorted, with `*` active-row (§5.2);
- cursor row (per-node cache) + key decisions (buffer rewrite, drill,
  submit);
- Esc: restore ladder at the current level — ID→cursor conversion +
  view_args restore if drilled (§5.1).

**Engine / node tree**: built once at startup. Per-node caches + staged
payload inside nodes; epoch bump invalidates on menu open. No `Arc`s needed.

**Host**: terminal I/O, table rendering, plan painting, submit-result
translation → `InputAction`. Non-slash affordances (`!shell`, path completion).

| Layer | Owns |
| --- | --- |
| Engine (default impl) | Node tree + traversal (grammar, cursor caches, Esc restore ladder, view_args state, render plan); confirm dialog service; key decisions |
| Node instances | Application data — `data()` / `submit()` / `prepare()`; mutations via shared handles |
| Host | App state, terminal I/O, rendering plan, `SubmitResult` → `InputAction` |

**Dispatch**: one `ActiveOverlay::Menu`. Host loop: `node_traverse(epoch, buffer)` →
apply `MenuResult` (buffer / submit). Per-command code lives only in node
impls, never as overlay arms.

---

> **Cleanup note (impl step 10 / Step 15 —
> `slashdot/final_cleanup.md` P1–P5, DONE)**: the v1.0/v2.0 machinery
> this design was written against — `Interpreter` /
> `AcceptController` / `PlanBuilder` / `MenuEngine` /
> `from_buffer_cursors`, the legacy `ActiveOverlay` variants
> (`Menu`, `Mod`, `Help`, `Settings`, `Stat`, …), the `slash.rs`
> registry — has been deleted. The host overlay set is now
> `ActiveOverlay::{None, V25Menu, PathComplete}` and `InputAction` is
> `{Submit, MenuDone}` (v2.5 writes `LiveState` + `menu_submitted`
> directly; async goes through `AsyncOp`). §12's layer split still
> holds: "Engine" = the `slashdot_menu` crate default impl, "Host" =
> repl/agent; only the type names moved. The prompt-completion island
> (`PathComplete` + `complete_path_prefix*`) is deliberately scoped
> out to the prompt-completion step.
