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

1. **§2 (rewritten)**: nodes become trait implementations (`NodeImpl`) whose
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

The prompt grammar, cursor/accept semantics, rendering, and modifier
contracts (§3–§7) are unchanged from v2 and inherited verbatim. §1 (goals),
§10 (per-command shapes), §11 (traceability) and §12 (role split) are
updated where the node-model revision touches them; each changed spot says
so explicitly. Inputs referenced: `slash_menu.md` (v1 UX contract, still authoritative for
anything not changed here), `src/repl.rs` (`ActiveOverlay` dispatch loop),
`src/model_catalog.rs` (`MODEL_MODIFIERS`), `src/table.rs`, sibling docs
(`settings_menu.md`, `sandbox_menu.md`, `tree.md`).

---

## 1. Goals and invariants

1. **Node-driven.** Every slash menu is a tree of *node implementations*
   (`NodeImpl`, §2). A single generic, non-overridable traversal (the
   engine) derives all behaviour — filtering, cursor, promotion, commit,
   rendering — from the nodes' rows and submit/data members. Per-menu
   `ActiveOverlay` variants and hand-written
   `from_buffer` / `handle_tab` / `handle_enter` / `handle_esc` arms collapse
   into generic node traversal states.
2. **The prompt is the state** (revised): the buffer determines level, filter
   state, and the *committed* selection — but the view cursor and the
   one-step per-node view parameter memory (back-navigation, §5) live outside
   the buffer, alongside the cached row data (§8). Rationale: the original
   "everything in the prompt" model made cursoring rewrite the id token, which
   conflicts with cursor-at-end refinement (§4); the revised model commits ids
   explicitly via Tab/Enter and keeps refinement ephemeral (restored via Esc).
3. **Uniform per-level grammar.** The same token rules apply recursively at
   every level of every menu; no menu-specific parser.
4. **Surface changes are allowed where they buy uniformity** (D1): minor
   layout adjustments to existing commands are acceptable — notably dropping
   the `<…>` id disambiguator (D2) and single-token child commands (R8-1).

Out of scope / preserved as-is: bare-prompt path completion (`Tab` on empty
input), `!cmd` / `!!cmd` shell affordances, and general `Esc`-clears-buffer
semantics outside menus (v2 audit issue #11). These stay adjacent to, not part
of, the menu system.

---

## 2. Core model: the trait node

*(Rewritten in v2.5; v2's data-descriptor model is the predecessor — the
grammar, cursor, and key semantics it defined are unchanged.)*

A menu is a recursive tree of **nodes**, and a node is a *trait
implementation*, not a data structure. One node impl per command (or per
sub-level of a command); the root list (`/`) is itself a node whose children
are the other command nodes (the existing `slash::registry()` content,
re-expressed as node impls).

### The trait and its extension points

The design principle: **the default implementation owns everything generic;
per-node code only customizes data and submission.** A command is one
understandable implementation unit — its rows, its submit behavior, and (by
default) all of its traversal behavior live in one `impl NodeImpl`.

Overridable members (the extension points):

`data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult | DataError`
- The rows this node contributes to its parent's listing (static children: its key + description; dynamic: fetched data). Default: none.
- `PathArgs` captures IDs and parsed arguments from the path traversal, and is described in the `traverse()` implementation, see further down.
- Enable optional caching, avoid copies: Return `Rc<DataResponse>`, allow parent node to check
  cheaply whether anything changed using `Rc::ptr_eq`; if nothing changed in
  the input data, its own caches do not need to be re-generated either.
  Do not cache row data unconditionally in the parent — we want uncached
  behavior in some cases (e.g. filesystem auto complete). This should be up to
  the `data()` handler implementation.
- Called internally by the default `traverse_data()` body, which parses args from the path suffix.

`submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult`
- The leaf accept behavior. Default: none (node is a listing; Enter drills).

`prepare(&mut self, epoch: u64, path_args: &PathArgs, open_editor: &mut dyn FnMut(&str) -> io::Result<String>) -> SubmitResult`
- *(Optional confirm hook)* default-noop = `submit()`. An override runs the
  prep phase — may call `open_editor()` (blocks while the editor runs;
  the host must have exited raw mode before calling `step_at` and re-enters
  afterward), precompute, stage the payload — then returns
  `SubmitResult::Prepared` to pause with the confirm dialog. The staged
  payload lives in the node's own private state (§2), accessible to
  `submit()` on the subsequent yea resolution. The editor handoff is a
  blocking synchronous call inside `prepare()`, not a separate traverse
  result — the engine does not need to know about `$EDITOR` at all.

`header(&mut self, path_args: &PathArgs) -> Vec<String>`
- Non-selectable header lines for this level (replaces v2 DetailProvider). Default: none.
- Used for help text, dynamic information (stats, message preview, drop/token
  counts, etc.). The engine truncates based on terminal size to provide
  sufficient space for menu options (§6).

Non-overridable (the generic machinery, enforced by construction — see
"Encapsulation" below): path parsing/dispatch, selection validation (D2
demotion), fuzzy rank-and-pin, the §7.1 filter/sort query engine, the
per-node cursor cache, the Esc ladder, traversal-frame management, and the
render-plan emission.

### Core types

```rust
struct PathArgs {
    /// IDs parsed from the traversal on the way down, ending in the current ID.
    ids: Vec<String>,
    /// Remaining path suffix, byte range into the prompt (preserves trailing
    /// whitespace — matters for listing semantics).
    suffix: std::ops::Range<usize>,
    /// Parsed dot-modifier key-value pairs and positional args.
    /// Only supplied to data() and submit() on leaf nodes (no children),
    /// and only when args are present.
    args: Option<Args>,
}

struct Args {
    modifiers: Vec<(String, Value)>,
    positional: Vec<String>,
}

/// Data response from a node's `data()`, wrapped in `Rc` for zero-copy
/// sharing across the traversal and optional cheap equality checks.
struct DataResponse {
    rows: Vec<Row>,
    active_ids: Vec<String>,  // empty vec = no active row (cheap, no heap)
}

struct DataError {
    message: String,
}

type DataResult = Result<Rc<DataResponse>, DataError>;

enum SubmitResult {
    Done,                    // close the menu
    Prepared,                // confirm flow: pause with yea/nay dialog (§9)
    NewPath(String),         // return a new prompt path (e.g. /compact)
}
```

`Rc<DataResponse>` enables zero-copy sharing between the node's cache and
the parent's merge step; `Rc::ptr_eq` gives the parent a cheap "nothing
changed" signal for cursor-cache preservation. The node's `data()` impl
decides whether to return a new `Rc` or reuse a cached one.

### Per-node state and the epoch

Each node owns **private, local state** in its impl:

- a **cursor cache**: selected ID, and per-filter state index. Arrow movement
  survives buffer re-derives and typing re-targets the cursor;
- a **derived data cache**, surviving frames of one menu activation; main
  caching is in data() handlers, but when those return unchanged Rc's (pointer
  equality), then we can reuse derived state cached internally. Minimize
  data duplication.
- optionally richer per-node state (e.g. a confirm flow's staged payload,
  §9), keyed as the node sees fit.

All caches are invalidated by the **epoch**: a menu-invocation counter the
host increments on every menu open and threads down the traverse chain.
Epoch increment ⇒ fresh per-node state, matching the v2 "new engine per
menu open" reopen semantics (§12) without a shared context datatype. A
multi-stage flow that must survive *within* one epoch (compaction's
stage → Esc → re-stage → accept) keys its cache on its own decision
points (e.g. the compaction target id — sessions are append-only trees, so
target-id keying alone is sound), not on epoch alone.

The host manages the epoch through a thin wrapper API (e.g.
`get_menu(&mut self) -> &Menu`): the wrapper bumps the epoch when the
input transitions from non-slash to `/` (menu open), and holds the fixed
epoch for all subsequent navigation within that activation. When the menu
is dismissed (empty buffer, submit, or Esc at root), the epoch is released.
The wrapper owns the epoch counter; the node tree receives it as a
parameter on each `traverse()` call. There is no coordinator struct —
the wrapper is one function call on the host side.

### Encapsulation: private traversal via blanket impl

Rust has no `final` methods — a default method in the *same* trait can be
shadowed. The non-overridability is enforced with the extension-trait /
blanket-impl pattern:

- the base trait `NodeImpl` exposes only the overridable members above;
- the traversal machinery lives in a **separate extension trait** (or a
  private struct each node composes), whose `traverse()` and `traverse_data()`
  is implemented **once via a blanket impl** over `NodeImpl`;
- the host and parents call only the extension trait's entry points;
  nothing can override the generic machinery.

The traversal itself is *recursive*: a parent's default body parses its level
from path_remainder, resolves the selection, and recurses into the selected
child with the updated PathArgs and remainder. 


Listing node (children, no submit handler):
- populate / refresh the row cache: call `traverse_data()` on each with the
  empty path remainder; check pointer equality - if changed, update internal
  derived state
- Path exhausted without child ID match: 
  - no key: return listing view MenuResult::Continue(RenderPlan / newprompt)
  - MoveCursor: update cursor position, return new RenderPlan
  - tab: expand focused ID in prompt, append space, populate internal frame
    cache, directly call child's traverse()
  - enter: as for tab, but direct call to child might submit() right away if
    child is leaf 
- Child ID match:
  - Non-empty suffix: Continue with traversal by `return traverse()` on owning
    child. All keys will be handled by a deeper node.
  - Empty suffix
    - no key: abort traversal and handle similar as "path exhausted", except
      for "reset to frame state and/or focus cursor" detail - see Esc ladder
      #5.1. return listing view RenderPlan / newprompt
    - MoveCursor: as in "without child ID match after "no key" rewrite
    - Esc: Clear frame and filter state, leaving only own `selector`; update
      RenderPlan
    - Tab / Enter: as in "no child ID match"

Leaf node (no children, submit handler), no key actions:
- Display the local `header()` via RenderPlan
- no key: just return RenderPlan
- enter: run prepare() handler, which by default returns submit(). If
  `prepared` is returned, return RenderPlan for confirmation dialog, and
  handle subsequent key events (CursorMove, enter) by either calling
  `submit()` if `accept` was selected, or pop one level (as with `esc`) on
  `abort`.
- esc: pop protocol as in #5.1. Also cancels any confirmation dialog.

All RenderPlan / newprompt mentions involve a
`MenuResult::Continue(Rc<RenderPlan>, newprompt)` for the host to render. The
`RenderPlan` wraps its bulk row data (`text: Rc<String>`, table cells) in `Rc`
so that cursor movement (Tab/Enter/MoveCursor) can return a new plan that
shares unchanged row data with the previous frame — the host re-renders only
the marker positions (`selected_row`, `active_rows`) without re-allocating or
re-copying the full content. For a 1000-row `/tree` list, cursor movement
allocates only the new marker positions, not 1000 row strings.

### Private parent -> child call API

`traverse(&mut self, epoch, KeyActions, PathArgs)`
returning `MenuResult(
    Continue(Option<RenderPlan>, newpath) 
    | DoneSuccess 
    | DoneError
)`
- KeyActions are Tab, Enter, Esc, MoveCursor(n) (move cursor up or down n lines, +1
  / -1 for simple key Up / Down; more for page up / down)
  The node at the end of the traversal handles all logical key actions, and
  eiter returns updated prompt / renderplan, or submits
- PathArgs is a struct `{ vec<Str>, Option<Args>, suffix:str]` containing
  - vector of IDs parsed from the traversal on the way down
    (ids), ending in the current ID
  - a string slice of the remaining path string suffix, slicing over the full
    prompt owned by the caller. It is important to preserve trailing
    whitespace (matters for listing semantics, so pre-splitting on whitespace
    would not be ideal.
  - Option<Args>: `{ modifiers: Vec<(String, Value)>, positional: Vec<String> }` 
    dot modifier key-value and vec of positional args, parsed
    generically from suffix; only supplied to data() and submit() on leaf
    nodes (no children), and only if args are found.
- RenderPlan row content can be large, and many key interactions like
  cursoring through a long list do not actually change the content - only the
  selection state. So RenderPlan should be structured to minimize copying of
  node-cached bulk row data (`Rc<T>` for all bulk data).
- Only one traversal is active at a time, so no concerns about concurrent
  access.

Parent nodes never call `data()` directly; all access is through
`traverse_data()` for uniform parameter handling, caching, and possible future
generic caching extensions):

`traverse_data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult | DataError`
- Parses args from suffix (if any) using generic dot modifier / positional arg
  parser; calls `data()`.

### Borrow discipline (verified)

One shape rule, needed because a default trait body cannot split
`&mut self` into disjoint fields the way a concrete struct can:

- **Parent-state access happens only outside the child loop** — three
  phases: own-state decisions → descend into children → own-state
  post-processing. Interleaving (e.g. reading the parent cursor inside the
  child loop to seed each child) does not compile (E0499: the
  `children_mut()` borrow locks `self` for the whole loop).
- Cross-level data flow (e.g. cursor seeding) is **passed down as
  arguments**, never read sideways from ancestor state.
- For pure traversals this is a linear pattern; most local mutation
  happens when the node is the listing leaf of the traversal, which is
  exactly the well-phased case.

### Node sharing

A node impl could in theory be reused by two parents, but e.g. `/model` and
`/set default_model` require different "active_id" and submit handling. Share
most of the base implementation around the model catalog interaction, but
specialize active_id / submit.

### Level classification (merged-child-set restatement)

A level is simply **a set of child nodes** (§2.1), each returning zero or more
rows; the old static/dynamic dichotomy is gone. The differences are now
node-internal implementation details in data(), unobservable to the parent
node.

## 2.1 Merged child sets (new in v2.5)

*(This is the key structural change motivating v2.5 — it is what makes
`/set`, `/sandbox`, and `/set default_model` expressible without
per-command interpreter logic.)*

A level's listing is the **ordered concatenation of all its child nodes'
rows**, however each child produces them:

- a static child contributes its `id` + optional description (one row);
- an enum/value child contributes its candidate value set (one row per
  value);
- a dynamic child contributes its fetched rows (one per data item) —
  possibly many, possibly zero.

So the v2 distinctions *between kinds of children at one level* dissolve:
there is no "static OR dynamic" level anymore, only **a set of children,
each with its own row source and its own submit handler**. The rules:

- **Row uniformity** (contract, enforced loosely): all children of one
  level return *roughly* the same fields — at least `id`, optionally
  `description`; optionally more. Rows must render together in one
  listing. IDs must be unique across a level.
- **Field metadata lives on the parent** (declared once): the parent
  declares the column defs / field space the shared listing renders. 
  Children *should* return the "required" fields, but a child may add fields
  (§8.1) that other children simply leave missing (missing-value rules of §7.1
  apply uniformly). 
- **Ordering**: `addChild()` order determines the default merge/sort
  order, unless the parent re-sorts.
- **Selection & validity stay uniform**: the merged row set is the level's
  candidate set; D2 demotion (a trailing token not matching any id is a
  fuzzy term) checks against the *merged* set. Each child validates and
  owns its own ids (the sketch's "own and validate their ids").
- **Submit dispatch is per child**: the row's *owning child* handles the
  submit — a static child's key-submit, a dynamic row's id-submit. One
  listing can mix e.g. a static `add` row and dynamic binding rows, each
  submitting differently. The parent keeps track of which row is owned by
  which child by tagging each row with its owning child's index during the
  merge step. Concretely: the default `traverse` body calls each child's
  `traverse_data()` in declaration order, and as each child's rows are
  appended to the merged list, the engine stores a `child_index: u16` (or
  equivalent compact tag) alongside each row. On Tab/Enter, the engine
  reads the tag from the cursor-row's row and dispatches to the
  corresponding child's `submit()`. This is internal to the default
  traversal body — not part of the `Row` struct's public API — and
  implementations may optimize the storage (a simple offset-based scheme
  suffices when child row counts are known).
- **A dynamic child under a mixed level may itself carry static children**
  (the v2 "two grammar levels" encapsulation, unchanged).

### Reference shapes

| Command | Shape in v2.5 |
| --- | --- |
| `/set` | one mixed level: static setting children (each a simple preference node with its own submit) **+** a dynamic child reusing the model-catalog rows with its own `default_model` submit. The v1 filter helpers (`MODEL_MODIFIERS`, `parse_model_filter`, `model_selection_overlay_entries`) die as a *consequence of modeling*, not cleanup. |
| `/model` | a single dynamic child returning all catalog rows (uniform fields: id, ctx, coding, prices), field metadata declared on the level; its submit handler selects the model. |
| `/sandbox` | one listing showing the current bindings (dynamic rows) with a static `add` child row at the top (declaration order); `add` maps to the add workflow (fs value completion via its own child node, §8). |
| `/tree` | dynamic child (tree rows) + the shared static action level after an id (unchanged v2 shape). |

### Mixed-level traversal

The traversal does not change: the parent parses its level against the
merged candidate set; a pinned selection drills into the selected child's
level (static child ⇒ its subtree; dynamic row ⇒ the child's shared action
level). The only new behavior is that `data()` of *several* children feeds
one listing — the parent's default body concatenates in declaration order
and rank-filters the merged set (one uniform mechanism).

## 3. Path grammar (per level, recursive)

At every level the buffer has the shape:

```
<parent tokens> [fuzzy search terms] [dot modifiers] [selection token]
```

- **Fuzzy search terms**: free words, may contain spaces. Filter the level's
  candidates (children keys + descriptions; for dynamic levels, ids +
  descriptions + displayed fields). Terms must **not** exactly match the
  level's selection token — the last word, if it exactly matches a candidate,
  *is* the selection (this is the structural rule that replaces `<…>`).
- **Dot modifiers**: `.key:value` tokens (§7). Parsed before the selection
  token; a modifier's value consumes following words until the next
  dot-modifier, a valid selection token, or end of prompt (R8-7).
- **Selection token**: space-free. Either a child `key` (static), a row id
  (dynamic), or a value-typed child's value (§2). It is *last in its level*
  and — for keys/ids — validated by a cheap lookup against the
  current candidate set (D2 — the `<…>` disambiguator is dropped;
  model/session/tree all become uniform). Value-typed selection tokens are
  permissively valid (§2). The token enters the prompt only via Tab/Enter
  (§4, §5); up/down cursoring moves the view cursor without rewriting it.
- **Trailing space** after the selection token means: show the next level
  (drill in). Absence means the selection is merely *pinned*; its sub-menu is
  not shown yet. This is a uniform, engine-owned rule for every **listing
  level** (a node with children, a value child, or a data handler):
  `/model` (no trailing space) stays in the *root* command list with the
  `model` entry pinned/highlighted; only `/model ` shows the model list.
  The same applies to `/thinking`, `/set`, `/session`, `/tree`, `/sandbox`.
  The host must not keep a per-command "open without trailing space" list —
  the grammar decides. Leaf nodes (no children/handler) accept directly.
- Dynamic nodes encapsulate *two* node levels when static sub-actions are
  specified, so the grammar applies at two levels.

> **v2.5 note on §3 terminology.** "Static"/"dynamic" here refer to how a
> *child* produces its rows (key vs `data()`), not to a property of the
> level — a level is a merged child set (§2.1). The grammar rules above are
> otherwise unchanged: a trailing token is validated against the level's
> merged candidate set; each child's rows may or may not be there at any
> given frame.

Parsing is therefore structural, not a whitespace peek: terminators are the
next dot-modifier, a valid id for the active level, or end-of-prompt (R8-2).
Filesystem-style value completion (`trailing slash` descent) is a
*value-completion behavior* of a specific modifier kind (§7), not a level
signal, so it does not interact with the level grammar.

---

## 4. Selection & cursor semantics

> **Implementation note (Phase 6.5, learned during integration):** the
> "view cursor" lives in the *engine*, not the host. The host forwards
> Up/Down as key events and renders the engine's state verbatim each frame;
> a two-cursor model (host `sel` ↔ engine `active` reconciliation) proved
> brittle in practice. See the impl plan's Phase 6.5 for the migration.

- **Cursor lives in the view; the prompt holds the commit.** The selection is
  the trailing selection token; up/down moves the *view cursor* only and does
  NOT rewrite the prompt (revised from the original cursoring design: with
  the cursor defaulting to the end of the prompt, cursor-rewrites fight
  filter refinement).
- **Tab/Enter act on the view cursor row** (not an internally derived
  default). Positioning the cursor on a row and pressing Tab writes *that*
  row's id into the prompt.
- **Accept clears refinement state.** Tab/Enter on a selection rewrite the
  prompt to *path + committed id only* — fuzzy terms and modifiers are
  ephemeral refinement, dropped on accept. (Revised from "Tab keeps the
  filter": with the cursor-at-end model, keeping terms after an id makes
  further refinement awkward.)
- **Traversal memory backs the clearing** (see §5 Esc): the previous filter
  state and cursor are remembered per level, so "clear on accept" never
  destroys state the user cannot get back.
- Exact-token preselection and fuzzy-unique auto-jumps (today's per-menu
  special cases) become one generic rule: exact match ⇒ pinned; unique fuzzy
  match ⇒ pinned.

---

## 5. Key contract (Tab / Enter / Esc)

- **Tab** completes the current candidate into the buffer (key, id, modifier
  name, or modifier value per its completion source):
  - bare level (no terms typed at the child level) ⇒ *promote* (trailing
    space; the child/value menu shows) — same as Enter;
  - otherwise complete the cursor row's id into the selection slot
    (clearing refinement terms, §4).
- **Enter**:
  1. If the node has a `header()`, it is always displayed (non-modal — no
     Enter-to-dismiss; Esc or backspace backtracks the path / leaves the
     menu). For `/stat`, the stats text is `header()` output, not a separate
     interstitial phase.
  2. If the node has a `submit` handler, invoke it (with `prepare` semantics
     if declared).
  3. Otherwise Enter behaves like Tab-drill: append trailing space and show
     the child level.
- **Esc** = back navigation with traversal memory, resolved by the
  per-level ladder in §5.1 (trailing space → frame replay → clear
  refinement → clear selection → clear buffer at root).
- **Traversal frames** are pushed only on accept/drill (downward
  transitions) and carry `{line, cursor, level}`. Because they only ever
  contain ancestors of the current position, Esc replay walks strictly up
  the tree — never to siblings.
- **Edit invalidation** (scoped): editing the line drops the traversal
  memory *of the edited level only* — a deeply nested drill-down keeps its
  ancestors' filter state. Criterion: the edit must change the level's
  segment (the id); removing *only* a trailing space is navigation
  (returning to the parent menu), not an edit, and clears nothing.
- **`*` active marker**: always indicates the *currently active* state (the
  value in effect before any menu change), sourced from application context
  — never from the typed/pinned selection. On a fresh menu the cursor seeds
  on the active row, marked `*`.

### 5.1 **Esc** = a strict per-level ladder

Each step applies only when the next does not:
  1. When the prompt ends on a valid child ID but no trailing space, replay
     the local traversal frame, if it exists (restore the previous local
     prompt + filter state + cursor matching ID). If no frame exists, focus
     the row matching ID (if it exists), and drop the trailing ID from the
     prompt.
  2. Next Esc: Clear the local frame and current level's fuzzy terms and
     modifiers (the buffer rewrite reflects the level with refinement
     dropped).
  3. Clear the level's selection token / ID (leaving the parent level with a
     trailing space so the parent list stays visible).
  4. Strip a trailing space (drill-out one level, cursor memory preserved
     via traversal frames), return newprompt. Parent level will continue at
     1), usually in a single Esc, restoring parent view state seamlessly. At
     the root level, clear the entire buffer (v1 rule retained).

### 5.2 Active-state sourcing (`active_rows`)

The "active" row of every candidate list must come from a single, uniform
mechanism inside the engine — the host never computes or renders it per
overlay:

- **`DataResult::active_ids -> Option<vec<String>>`**: dynamic providers
  report the id(s) of the row(s) that are currently in effect **inside their
  `fetch` response** (e.g. the active model's `provider:name`, the current
  session id, the current leaf's tree entry id), set from their own live data
  source. The engine resolves the ids to row indices in the candidate set per
  frame and renders `*` there.
- **Enum / static levels**: the same selector notion — an optional
  `ActiveProvider` registered on the level's node supplies just the active
  id (e.g. `/thinking`'s currently-set level, `/persona`'s current persona)
  from live app state; the engine resolves it against the descriptor-derived
  rows exactly as `DataResult::active_ids` (no host-side per-overlay marker
  injection). A level with no selector (root command list) renders no `*`
  anywhere.
- The engine's `RenderPlan` exposes `active_rows: Option<vec<usize>>` (the
  `*` positions) separately from `selected_row` (the cursor position). The host
  renders `*` from the first and `>` from the second with no per-variant
  logic; when both land on one row, `*` wins.
- Open-time cursor seeding falls out of this: when a level is first
  displayed and no sticky cursor exists for it, the cursor seeds on the
  first active row where the selector(s) resolve; otherwise row 0.

---

## 6. Rendering

- **Panel structure** (slightly adjusted from v1/v2.0): 
  - optional header() handler rows
  - non-selectable header column row (if any child rows)
  - selectable rows with `> ` (selected) / `* ` (active-default) markers.
- **Headers** (R8-4):
  - *(i)* Optional node-supplied multi-line / preformatted `header()` - can be
    used to provide help, or display dynamic information (stats, message
    preview, drop/token counts, etc.). Truncated based on terminal size to
    provide sufficient space for menu options (if any)
  - *(ii)* when child IDs / table is present: static single-line table
    headers from the descriptor, showing table headings; fixed position when
    scrolling through long list
- The generic UI limits the header block to the available vertical space,
  accounting for the space the menu options need (as `/tree <id>` sub-menus do
  today).
- **Tables** come from the declarative columndef (D6). The existing builder
  knobs (`headers`, `aligns`, `min_widths` priority ordering, `gap`,
  `ellipsis`, wrap-vs-ellipsis) are sufficient; nothing from the hand-built
  settings/sandbox/model tables is lost.
  Table inclusion might become more dynamic in the future (e.g. always include
  modifier-specified filter / sort columns, explicit `.columns:` modifier) -
  so keep the data structures flexible for now.

---

## 7. Modifiers & value completion

One mechanism, dot-prefixed, at every level (D3). Static levels may also carry
modifiers (e.g. `/sandbox add .ro:<path> .rw:<other>`); static *children*
remain bare words. Multi-word modes become single hyphenated tokens
(`text-only`); no spaced display labels (R8-1).

A modifier is:

| Property | Notes |
| --- | --- |
| `key` | Dot token name (`.sort`, `.prompt`, `.ro`, …). |
| value kind | `string` / `number` (float or int, permissively parsed) — only these two types. |
| enum set | **Orthogonal** to the value kind: an optional candidate list constraining *completion* only. The interpreter never rejects an out-of-enum token — the backend validates at accept time (any prompt is valid; the action may error). |
| completion source | `dataset-derived` (**default**): unless an enum is set, the distinct string values of the field are *lazily* extracted on first modifier use with a pass over the cached rows (§8.1). · `dynamic handler` (query → candidates, cached until the level changes — trailing slash for fs descent, §8) · `default value` (auto-filled on Tab alongside the selector, e.g. `.prompt` next to the `llm` selection in the compaction menu). |
| front-end / back-end | **Front-end**: applied by the interpreter over cached rows (§7.1). **Back-end**: passed through to the data-fetch handler for live filtering (e.g. fs paths). The data() implementation controls caching, and determines which is used. |

Value tokens may be multi-word (consume until next dot-modifier / valid id /
end of prompt); selection tokens may not (R8-7).

### 7.1 Field filtering & sorting on dynamic levels

Every field of a dynamic row (§8.1) is addressable by a modifier token named
after the field. The generic interpreter applies filters and sorting over the
cached rows — this generalizes the hand-written `ModelFilter` /
`apply_modifier` logic into one mechanism:

| Token | Semantics |
| --- | --- |
| `.field:value` | Exact match on a string field (id, description, or string key-value field). |
| `.field:>v` / `.field:<v` | Numeric comparison (retain rows with value > / < v); `>=` / `<=` likewise. Comparison filters drop rows *missing* the field. |
| `.sort:field` | Direction defaults to the field's declared `default_dir` (§8.1), so legacy names like `.sort:coding` keep working. |
| `.sort:>field` / `.sort:<field` | Explicit descending / ascending as a prefix. The `<>` prefix is stripped during fuzzy search, so typing `.sort:>co` still matches `coding`. |

Rules:

- **Composition**: all filters AND together and apply before sorting; fuzzy
  search terms narrow further on top (structural filters first, free-text
  layer second). A later `.sort:` overrides an earlier one (single sort).
- **Compound specs are dropped**: the brainstorm's `.price:in<0.10;out<0.6`
  becomes two tokens — `.price_input:<0.1 .price_output:<0.6` — which the
  uniform per-field grammar expresses for free.
- **Legacy named sorts become computed fields**: `most-popular` is the
  handler's default row order (no token needed); `coding_price`,
  `intelligence_price`, `agentic_price` are numeric computed fields
  (index-per-price ratios), so `.sort:<coding_price` is a plain field sort —
  no special-cased sort names remain.
- **Missing values**: excluded by comparison and exact-match filters (matches
  today's `map_or(false, …)` behavior) and sort *last*, uniformly (a minor
  normalization of today's mixed `f64::MAX` / `0.0` sentinels).
- **Back-end fields**: the same token syntax is passed through to a
  live-filter handler untouched; the handler owns its interpretation (§8).

Field-name tokens are enum-completed from the row's filterable/sortable
fields; string-field values complete from distinct dataset values (the
default completion source above).

---

## 8. Data sources (dynamic levels)

Two handler kinds (R8-6) — unchanged from v2; only the *binding* changes:

1. **One-shot fetch** — called once per dynamic menu open; returns rows (id,
   description, optional typed key-value fields, default completion value for
   the selection token). The engine does all filtering/sorting over the
   cached rows. Model catalog, sessions, tree snapshots, settings-derived
   candidate sets.
2. **Live-filter fetch** — takes current modifier inputs; returns rows or a
   `not changed` signal when the candidate set would be identical (e.g. same
   fs directory level), in which case fuzzy matching keeps narrowing in-place.
   Tab expands to the currently selected entry (possibly with a trailing
   slash) and re-runs the handler to load more children, or the handler
   returns `not changed` for a leaf. Filesystem paths, remote model listings.
   Primarily needed for modifier string value completion, esp. fs paths - but
   good to keep option open to complete positional value parameters as well.

A data child may sit under a level with other children too (e.g.
`/set default_model` value = model listing; `/sandbox remove` value = binding
paths as ids). Under the merged-child-set model (§2.1) such nesting is
just one data child of the parent level.

**Binding (v2.5 change).** In v2, providers were registered on the engine
in path-keyed side tables (`with_provider(&["model"], …)`); in v2.5 the
row source **is the node**: a dynamic child's `data()` *is* its fetch
(one-shot or live-filter), and its `active_rows` (§5.2) is likewise
reported by the node in its data response. No registration step, no
stringly-keyed lookup; the accept-controller's path-based handler lookup
collapses into node identity. The engine passes only prompt-derived
inputs (`epoch`, parsed modifiers, query); application context is
captured in the node impl at construction (§2, §12).

### 8.1 Row fields

Row fields (id, description, key-value fields) carry small metadata used by
filtering, completion, and rendering:

- `type`: `string` or `number`.
- `filterable` / `sortable`: opt-out flags (default on) — only fields that
  make sense participate (e.g. a raw uid is exact-match filterable but may be
  excluded from `.sort`).
- `default_dir`: default sort direction for a direction-less `.sort:field`
  (indices `desc`, prices `asc`).
- **Computed fields**: pre-derived numeric/string values supplied by the data
  handler (combined price, index-per-price ratios, age in days, …) so filters
  and sorts never need special-cased names (§7.1).
- **Display-string override** (per field, optional): a verbatim string used
  for table display only — ignored by sorting `filtering` and completion,
  which always read the raw field value. Lets a provider refine presentation
  (an ISO date, a price glyph) in its own code with compiled performance,
  with no added engine complexity.
- `id` (string) and `description` (string) are implicit fields: exact-match
  filterable, sortable, and always in the fuzzy-search space. All fields are
  filterable + sortable by default (no special "sort" field — ``.sort:``
  completes over all fields).

For one-shot fetches, the interpreter runs a **preprocessing pass** once per
fetch over the cached rows: extraction of distinct dataset-derived string
value sets for modifiers without a pre-defined enum (D7).

---

## 9. Accept flows: leaf, confirm, editor handoff

The two "paused" flows (R8-3) are **not** unified into one concept; they are
distinct and each is framework-owned where it can be:

1. **Confirm (v2.5: two-phase submit protocol)** — a leaf whose handler
   needs a yea/nay step. The decomposition (settled in the simplification
   review) replaces v2's two-handler `ConfirmPrep` machinery:
   - The **state machine lives with the handler** (node-local, epoch/path
     keyed per §2): `prepare()` (default-noop = `submit()`) runs the prep —
     may launch the editor, precompute, stage the payload — then returns
     `SubmitResult::Prepared` = "pause with the dialog as render target".
   - The **dialog is a not-overridable engine service**: the engine renders
     the accept/cancel dialog and routes its keystrokes/cursoring, and on
     Enter resolves yea/nay back to the node (`submit_resolved(yea)`).
     Zero per-node dialog logic.
   - On yea the handler commits (e.g. runs the compaction); on nay — or on
     Esc / any buffer rewrite while paused — the engine routes the cancel
     to the node, which drops its staged payload (cache management, §2).
     Re-entering the same prompt from scratch re-runs the prep.
   - Consequence: no menu-level confirm state remains in the engine.
2. **Editor handoff** — the external `$EDITOR` review is a blocking
   synchronous call inside `prepare()` (§2, `open_editor` callback). The
   node's `prepare()` calls `open_editor(&staged_text)`, blocks while the
   editor runs, stores the result in its private state, and returns
   `SubmitResult::Prepared`. The engine then transitions to the confirm
   dialog. No special traverse result needed; the engine never knows about
   `$EDITOR`.

**Leaf handler results** (formalized from R8-8, revised): `SubmitResult`
(§2, "Core types"). The handler returns one of:
- `Done` — close the menu;
- `NewPath(String)` — buffer rewrite (e.g. `/compact` →
  `/tree <last-user-id> compact ` — pure sugar, no parallel hierarchy);
- `Prepared` — pause for confirm dialog (§9); the node has staged its
  payload in private state, and `submit()` will commit on yea.

App-data mutations (set model, write session, update recents, trigger a
background refresh) are performed **by the handlers themselves** via the
shared handles they captured at build time — they run at accept time,
after the frame's engine decisions are made, so there is no re-entrancy
into the engine's ongoing work. The consequence: app state a handler
mutates must live in shared cells (single-threaded `Rc<RefCell>`), not in
loop-owned `&mut` borrows — an explicit app-side structuring step.

Purity comes from the crate boundary + generics (the engine compiles
without knowing the host's action type), not from an intermediate result
vocabulary; the host applies only what only the host *can* apply (control
flow, terminal, transcript).

---

## 10. Per-command target shape (summary)

| Command | Shape in v2.5 |
| --- | --- |
| `/` | static root list; children = command nodes |
| `/model` | one dynamic child returning all catalog rows (§2.1); front-end field modifiers (§7.1), leaf = select model |
| `/thinking` | one `enum` positional value child (`off|low|med|high`), shared leaf = SetThinking (§2) |
| `/persona` | dynamic leaf with dataset/enum string value source |
| `/set` | **mixed level (§2.1)**: static setting children, each its own simple-preference node + submit; `default_model` = dynamic child reusing the model rows with its own submit |
| `/sandbox` | **mixed listing (§2.1)**: static `add` row (add workflow, read-only / read-write children, fs value completion, §8) at the top + dynamic binding rows (remove workflow per row) |
| `/session` | dynamic level (sessions) → static action children → confirm via two-phase submit (§9) on delete |
| `/tree` | dynamic level (tree nodes) → static action children (`switch`, `compact`); `compact` modes are single-token enum children; `prompt` leaf carries potentially-long prompt (default value) + two-phase submit confirm; might use display strings with unicode lines to render tree indent layout |
| `/compact` | leaf that returns the rewritten path `/tree <id> compact ` via
newpath; drop current parallel menu |
| `/stat` | `header()` returns stats text; no submit handler — Esc/backspace clears. |
| `/replay` | `header()` returns help text; `submit()` dispatches the replay. |

---

## 11. Traceability of resolved questions

| Question | Resolution |
| --- | --- |
| Q1 surface changes | Allowed where they buy uniformity (D1). |
| Q2 `<…>` disambiguator | Dropped; positional ids (D2). Revised: accepts clear the filter, traversal memory restores it (§4, §5). |
| Q3 modifier grammar | Dot-modifiers uniformly for values; children stay bare words (D3). |
| Q4 `/stat` `/replay` | In scope via `header()` (stats/help text) + `submit()` where applicable (§5, §10). |
| Q5 confirm | Revised (v2): two independent handlers (prep leaf + confirm accept). **v2.5:** two-phase submit protocol — handler-local state machine, not-overridable dialog service (§9). |
| Q6 tables | Generalize now, low cost (D6). |
| Q7 dataset values | Default per modifier (D7). |
| Q8 fs completion | Dynamic completion handler within modifier value completion (D8). |
| R8-1 spaced labels | Single-token; drop spaced display. |
| R8-2 space vs slash | Different contexts; structural terminators; parsing unaffected. |
| R8-3 paused accepts | Confirm via two-phase submit (§9); editor handoff via blocking `prepare()` callback; interstitial subsumed by `header()` (§5, §9). |
| R8-5 pinned id | Revised: accepts commit the id and clear refinement; prompt state is path+id, filters ephemeral with traversal-memory restore (§4, §5). |
| R8-6 handler kinds | One-shot vs live-filter (§8); front-end vs back-end modifiers (§7). |
| R8-7 spaces in tokens | Selection tokens space-free; values and fuzzy terms may contain spaces (§3, §7). |
| R8-8 `/compact` | Leaf "return new path" rewrite (§9); no parallel hierarchy. |
| Value types | Cut to `string` / `number` (permissive); enum is an orthogonal completion constraint, backend validates at accept (§7). |
| Dataset-derived completion | Default for cached values; built in the preprocessing pass unless an enum is pre-defined (§7, §8.1). |
| No per-row child nodes | No per-row child nodes (§2, §3), but each row can come from a different child node, and the parent keeps track where each row came from. |
| Positional enum / number values | `enum` / `number` value-typed child kinds; shared handler; permissive parse, backend validates (§2). |
| Field filtering / sorting | `.field:value`, `.field:>v`/`<v`, `.sort:>f`/`<f` (+ per-field default direction); computed fields replace named sorts (§7.1). |
| Compound filter specs | Dropped; express as separate per-field tokens (§7.1). |
| Cursoring & accept model | *Post-v1 design review:* up/down moves the view cursor only; Tab/Enter act on the cursor row, commit path+id, clear refinement; traversal memory + scoped edit invalidation give seamless back-navigation (§4, §5). |
| `*` marker | Always the currently-active state from application context, never the typed selection (§5). |
| Active-state sourcing | Single engine mechanism (provider `DataResult::active_rows`, §5.2) resolved to the `*` row per frame; open-time cursor seeds on it (§5.2). |
| Listing-open rule | Uniform: pinned selection shows the *parent* list with the pinned row highlighted; only the trailing space drills (§3). |
| Esc behavior | Per-level ladder: trailing space → frame replay → clear refinement → clear selection → clear buffer at root (§5.2). |
| Engine lifetime | Persistent per activation; per-activation state (cursor/frames/cache/accept-phase) lives **inside the engine**; host keeps the engine instance across keypresses; no rebuild, no take/restore, no Arcs (§2, §12). |
| Handler statefulness | v2.5: node-local state in the impl, epoch-cached; hand-written stateful closures replaced by the trait node's private per-node state (§2). Confirm = two-phase submit with handler-local state machine (§9). |
| Node model | v2.5: nodes are trait impls (`NodeImpl`); default traversal is private/non-overridable and owns epoch-cached per-node state; providers/handlers attach to nodes, not path-keyed side tables (§2, §8). |
| Merged child sets | v2.5 new: one level = ordered union of child rows (static + enum + dynamic children each with own data/submit); field metadata on the parent; per-child submit dispatch (§2.1). |
| Node sharing | Reuse a node impl by *instantiating* it per parent (e.g. `/model` and `/set default_model`); no `Rc` sharing — plain local per-node state (§2). |
| Confirm dialog | v2.5: dialog is a not-overridable engine service; the handler owns the state machine and staged payload (`prepare` default-noop = `submit`; `SubmitResult::Prepared` pauses; `submit()` on yea) (§9). |
| Coordinator | v2.5: eliminated — host manages epoch via wrapper function; no coordinator struct (§2, §12). |

---

## 12. Role split: engine vs. host (final state)

The recurring integration friction (cursor sync, marker policy, overlay
dispatch) traces to fuzzy ownership boundaries. The settled division:

**v2.5 restatement of §12's engine.** The engine is the **node tree**
(`NodeImpl` instances, §2); there is no separate "interpreter". The
generic machinery (path parsing, selection validation, rank-and-pin,
§7.1 filter/sort query, per-node cursor caches, the Esc ladder,
traversal frames, render-plan emission) lives in the default,
non-overridable traversal (blanket impl, §2). The **epoch is managed by
the host** through a thin wrapper (§2, "Per-node state and the epoch"):
the wrapper bumps the epoch on menu open and holds it for the activation;
the node tree receives it as a parameter on each `traverse()` call. There
is no centralized interpreter state machine and no menu-level confirm
state (§9 — the confirm flow is handler-local). Per frame the traversal
produces, from `(line, node-tree, epoch)`:

- the resolved level (node path) and whether it is a listing or a pinned
  level awaiting drill (the trailing-space rule, §3 — uniform, no host
  exceptions);
- the candidate rows (the merged child-set rows, §2.1, ranked, filtered,
  sorted), including the `*` active-state row resolved from the owning
  node's data response (§5.2);
- the cursor row (per-node cursor cache, §2), and Tab/Enter/Esc
  **decisions**: the new buffer text, whether to push/pop a traversal
  frame, and whether the accept is a submit (canonical line) or an
  intermediate state;
- Esc as the per-level ladder (§5.2) — pure buffer/state rewrites, done
  as a **root re-entry** with the shorter buffer (never an up-recursion),
  §2.

The engine is *pure*: no terminal I/O, no picocode types, no async. It
emits a **render plan** (level, rows with marker positions, header, title
hint) rather than strings where possible, so the host's table renderer can
format.

**Engine / node tree lifetime — persistent, with engine-owned per-activation state.**
The engine (the node tree) is built once on app start-up. Per-node caches,
cursor state, and any staged confirm payload live **inside the nodes**; a
fresh menu open ("activation") **bumps the epoch**, which invalidates them
(§2). This removes any rebuild costs. Persistence requires no `Arc`s: the
traversal reads the buffer at step entry and mutates nothing app-owned; nodes
capture what they need (§2). (The `Activation` bundle and
`take_state`/`restore_state` accessor pair were the *rebuild-era* scaffolding
for carrying per-activation state across a rebuilt engine; they are deleted
once the host holds the node tree directly.)

**The host (`repl.rs`) owns everything not derivable from the prompt:**

- terminal I/O, table rendering (columndefs from the descriptor), row
  highlight painting, scrolling;
- applying the submit result's narrow host outcome: control flow (commit /
  quit / stay), prompt update, redraw. All app-data mutations happen in node
  `submit()` (§9) via shared handles; the host loop applies only what the
  outer loop owns. Engine purity rests on the crate boundary + generics,
  not on an intermediate result vocabulary;
- non-slash affordances: bare-prompt path completion, `!shell`, general Esc
  with an empty menu.

| Layer | Owns |
| --- | --- |
| Engine (node default impl) | the node tree + its default traversal (grammar, cursor caches, Esc ladder, frames, render plan); per-node epoch-cached state; the confirm dialog service (§9); key decisions |
| Node instances (app adapters) | all application data — read MenuShared entries; supply rows (`data()`, incl. `active_rows`, §5.2) and submit behavior (`submit()`/`prepare()`, §9); perform app-data mutations directly via shared handles |
| Host | app state itself, terminal I/O, rendering the plan, translating the submit result → `InputAction` |

**Dispatch rule**: there is exactly one `ActiveOverlay::Menu` shape in the
final state (impl Phase 7). The host's key loop is uniform: `state(line)` →
render plan → read key → `step(line, key)` → apply result (buffer edit /
frames / submit). Per-command code exists only in the node impls + their
data/submit members + leaf-result translation, never as overlay arms.

**Until Phase 7 completes**, legacy overlay arms may temporarily read host
state (markers, filters), but every *new* behavior added in Phase 6.5+ must
follow this split; no new host-side per-variant policy.
