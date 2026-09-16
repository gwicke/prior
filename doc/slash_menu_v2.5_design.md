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
   into generic traversal states.
2. **The prompt is the state** (revised): the buffer determines level, filter
   state, and the *committed* selection — but the view cursor and the
   traversal memory (back-navigation, §5) live outside the buffer, alongside
   the cached row data (§8). Rationale: the original "everything in the
   prompt" model made cursoring rewrite the id token, which conflicts with
   cursor-at-end refinement (§4); the revised model commits ids explicitly
   via Tab/Enter and keeps refinement ephemeral (restored via Esc).
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

| Member | Role |
| --- | --- |
| `data(&mut self, epoch, ctx) -> Vec<Row>` | The rows this node contributes to its parent's listing (static children: its key + description; dynamic: fetched data). Default: none. |
| `submit(&mut self, inv) -> SubmitResult` | The leaf accept behavior. Default: none (node is a listing; Enter drills). |
| `prepare(&mut self, inv) -> SubmitResult` | *(Optional confirm hook)* default-noop = `submit()`; an override adds a prep phase (§9). |
| `detail(&mut self) -> Vec<String>` | Non-selectable header lines for this level (v2 DetailProvider). Default: none. |
| `title()` / metadata | Static strings: description, child header, column defs, field metadata. |

Non-overridable (the generic machinery, enforced by construction — see
"Encapsulation" below): path parsing/dispatch, selection validation (D2
demotion), fuzzy rank-and-pin, the §7.1 filter/sort query engine, the
per-node cursor cache, the Esc ladder, traversal-frame management, and the
render-plan emission.

### Per-node state and the epoch

Each node owns **private, local state** in its impl:

- a **cursor cache** keyed on the level's filter state — the (index, pin,
  terms) triple with the uniform reset-on-change rule of v2's `CursorMap`
  (§4), so arrow movement survives buffer re-derives and typing re-targets
  the cursor;
- a **data cache** for one-shot fetches (§8), surviving frames of one menu
  activation;
- optionally richer per-node state (e.g. a confirm flow's staged payload,
  §9), keyed as the node sees fit.

All caches are invalidated by the **epoch**: a menu-invocation counter the
root increments on every menu open and threads down the traverse chain.
Epoch increment ⇒ fresh per-node state, matching the v2 "new engine per
menu open" reopen semantics (§12) without a shared context datatype. A
multi-stage flow that must survive *within* one epoch (compaction's
stage → Esc → re-stage → accept) keys its cache on its own decision
points (e.g. the compaction target id — sessions are append-only trees, so
target-id keying alone is sound), not on epoch alone.

### Encapsulation: private traversal via blanket impl

Rust has no `final` methods — a default method in the *same* trait can be
shadowed. The non-overridability is enforced with the extension-trait /
blanket-impl pattern:

- the base trait `NodeImpl` exposes only the overridable members above;
- the traversal machinery lives in a **separate extension trait** (or a
  private struct each node composes), whose `traverse()` is implemented
  **once via a blanket impl** over `NodeImpl`;
- the host and parents call only the extension trait's entry points;
  nothing can override the generic machinery.

The traversal itself is *recursive*: a parent's default body parses its
level, resolves the selection, and recurses into the selected child with
the path remainder (`traverse(keys, args, prefix, id, path_remainder)` —
returning `[Continue(Option<RenderPlan>, Option<newpath>) | DoneSuccess |
DoneError]`). Only one traversal is active at a time.

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

A node impl may be reused by two parents (e.g. `/model` and
`/set default_model` both listing the catalog rows). Decision (settled in
the simplification review): **two instances, not `Rc` sharing** — the
default-model level is rarely touched and the catalog fetch caches
independently, so instance-sharing wins are negligible. Consequence: each
node's cursor state is plain local state (no path-keying inside the node);
the parent path *is* the instance.

### Level classification (merged-child-set restatement)

A level is simply **a set of child nodes** (§2.1), each returning zero or more
rows; the old static/dynamic dichotomy is gone. The differences are now
node-internal implementation details, unobservable to the parent node. All
child rows are concatenated by the parent. Rough node types in typical
cardinality, ascending:

- **Single-key children**: contribute one row (their `key` + description);
  selection token = the child `key`. Any child may itself have children
  (a sub-level), OR a leaf handler (never both).
- **Enum children**: `enum` (candidate value set, completion aid,
  permissive parse, backend validates) or `number`; one node, one shared
  handler. Grammar impact unchanged (§2 of v2).
- **Data children**: contribute fetched rows from `data()` (§8);
  selection token = a row id. A data child with its own children shares
  them across **all its rows**: after a valid id, the next grammar level
  is that single action list (if any) — no per-row child nodes. The
  parent keeps track of which row came from which child, so submit
  dispatches to the row's owning child (§2.1).

## 2.1 Merged child sets (new in v2.5)

*(This is the key structural change motivating v2.5 — it is what makes
`/set`, `/sandbox`, and `/set default_model` expressible without
per-command interpreter logic.)*

A level's listing is the **ordered concatenation of all its child nodes'
rows**, however each child produces them:

- a static child contributes its `key` + description (one row);
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
  listing.
- **Field metadata lives on the parent** (declared once): the parent
  declares the column defs / field space the shared listing renders and
  filters against. Children conform; a child may add computed fields
  (§8.1) that other children simply leave missing (missing-value rules of
  §7.1 apply uniformly).
- **Ordering**: `addChild()` order determines the default merge/sort
  order, unless the parent re-sorts.
- **Selection & validity stay uniform**: the merged row set is the level's
  candidate set; D2 demotion (a trailing token not matching any id is a
  fuzzy term) checks against the *merged* set. Each child validates and
  owns its own ids (the sketch's "own and validate their ids").
- **Submit dispatch is per child**: the row's *owning child* handles the
  submit — a static child's key-submit, a dynamic row's id-submit. One
  listing can mix e.g. a static `add` row and dynamic binding rows, each
  submitting differently.
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
  1. If the node has an `interstitial`, show it first (second Enter dismisses
     if there is no leaf handler; otherwise the next Enter proceeds, §10).
  2. If the node has a `leaf` handler, invoke it (with confirm semantics if
     declared).
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

### 5.1 Active-state sourcing (`active_row`)

The "active" row of every candidate list must come from a single, uniform
mechanism inside the engine — the host never computes or renders it per
overlay:

- **`DataResult::active_row -> Option<String>`**: dynamic providers report
  the id of the row that is currently in effect **inside their `fetch`
  response** (e.g. the active model's `provider:name`, the current session
  id, the current leaf's tree entry id), set from their own live data
  source. The engine resolves the id to a row index in the candidate set
  per frame and renders `*` there.
- **Enum / static levels**: the same selector notion — an optional
  `ActiveProvider` registered on the level's node supplies just the active
  id (e.g. `/thinking`'s currently-set level, `/persona`'s current persona)
  from live app state; the engine resolves it against the descriptor-derived
  rows exactly as `DataResult::active_row` (no host-side per-overlay marker
  injection). A level with no selector (root command list) renders no `*`
  anywhere.
- The engine's `MenuState` exposes `active_row_marker: Option<usize>` (the
  `*` position) separately from `active` (the cursor position). The host
  renders `*` from the first and `>` from the second with no per-variant
  logic; when both land on one row, `*` wins.
- Open-time cursor seeding falls out of this: when a level is first
  displayed and no sticky cursor exists for it, the cursor seeds on the
  active row if the selector resolves; otherwise row 0.

- **Esc** = a strict per-level ladder (each step applies only when the next
  does not):
  1. Strip a trailing space (drill-out one level, cursor memory preserved
     via traversal frames).
  2. Replay a traversal frame (restore the previous prompt + filter state +
     cursor) — §5 rules.
  3. Clear the current level's fuzzy terms and modifiers (the buffer
     rewrite reflects the level with refinement dropped).
  4. Clear the level's selection token (leaving the parent level with a
     trailing space so the parent list stays visible).
  5. At the root level, clear the entire buffer (v1 rule retained).

---

## 6. Rendering

- **Panel structure** (unchanged from v1): title row, optional non-selectable
  header rows, selectable rows with `> ` (selected) / `* ` (active-default)
  markers.
- **Headers** (R8-4):
  - *(i)* static single-line `child_header` from the descriptor;
  - *(ii)* below dynamic list levels, a per-row header derived from the
    selected row's fields + table columndef — either the row's `description`
    (as normally shown in the listing) or an explicit `child_description`
    override that the data supplier may pre-format (stats, message preview,
    drop/token counts, etc.).
- The generic UI limits the header block to the available vertical space,
  accounting for the space the menu options need (as `/tree <id>` sub-menus do
  today).
- **Tables** come from the declarative columndef (D6). The existing builder
  knobs (`headers`, `aligns`, `min_widths` priority ordering, `gap`,
  `ellipsis`, wrap-vs-ellipsis) are sufficient; nothing from the hand-built
  settings/sandbox/model tables is lost.

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
| completion source | `dataset-derived` (**default**): unless an enum is set, the distinct string values of the field are extracted in a preprocessing pass over the cached rows (§8.1). · `dynamic handler` (query → candidates, cached until the level changes — trailing slash for fs descent, §8) · `default value` (auto-filled on Tab alongside the selector, e.g. `.prompt` next to the `llm` selection in the compaction menu). |
| front-end / back-end | **Front-end**: applied by the interpreter over cached rows (§7.1). **Back-end**: passed through to the data-fetch handler for live filtering (e.g. fs paths). `MODEL_MODIFIERS` metadata is the seed for this split (R8-6). |

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
| `.sort:field>` / `.sort:field<` | Explicit descending / ascending as a **suffix** (more ergonomic with type-ahead than a prefix: pick the field, then append the direction). |

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
(one-shot or live-filter), and its `active_row` (§5.1) is likewise
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

## 9. Accept flows: leaf, interstitial, confirm, editor handoff

The three "paused" flows (R8-3) are **not** unified into one concept; they are
distinct and each is framework-owned where it can be:

1. **Interstitial (D4)** — plain text display owned by the framework. Shown on
   Enter before any leaf call/drilling. With no leaf handler: second Enter
   dismisses (`/stat`). With a leaf handler: first Enter shows it, next Enter
   invokes the leaf (`/replay`).
2. **Confirm (v2.5: two-phase submit protocol)** — a leaf whose handler
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
   - Consequence: no menu-level confirm state remains — the coordinator
     (§12) shrinks to the root entry point + the epoch counter.
3. **Editor handoff** — the external `$EDITOR` review *replaces the UI
   temporarily* and is driven by the leaf handler call itself (handler returns
   "launch editor"); not an interstitial. Cancellation re-enters the prompt
   via the existing `pending_prefill` mechanism.

**Leaf handler results** (formalized from R8-8, revised): `LeafResult<T>`
is generic over the host's action type. It carries:

- **Menu-semantic outcomes** the engine drives itself: close the menu,
  **return a new path** (buffer rewrite, e.g. `/compact` →
  `/tree <last-user-id> compact ` — pure sugar, no parallel hierarchy),
  pause for confirm (§9), launch the editor, interstitial text;
- a **narrow host-outcome payload** `T` the engine treats as uninterpreted
  data — *only* the effects belonging to the REPL outer loop, not app-data
  mutations: `flow` (stay in menu / commit line / quit), an optional
  success/error message, and the redraw signal. App-data mutations
  (set model, write session, update recents, trigger a background refresh)
  are performed **by the handlers themselves** via the shared handles they
  captured at build time — they run at accept time, after the frame's
  engine decisions are made, so there is no re-entrancy into the engine's
  ongoing work. The consequence: app state a handler mutates must live in
  shared cells (single-threaded `Rc<RefCell>`), not in loop-owned
  `&mut` borrows — an explicit app-side structuring step.

Purity comes from the crate boundary + generics (the engine compiles
without knowing what `T` is), not from an intermediate result vocabulary;
the host applies only what only the host *can* apply (control flow,
terminal, transcript).

---

## 10. Per-command target shape (summary)

| Command | Shape in v2.5 |
| --- | --- |
| `/` | static root list; children = command nodes |
| `/model` | one dynamic child returning all catalog rows (§2.1); front-end field modifiers (§7.1), leaf = select model |
| `/thinking` | one `enum` positional value child (`off|low|med|high`), shared leaf = SetThinking (§2) |
| `/persona` | dynamic leaf with dataset/enum string value source |
| `/set` | **mixed level (§2.1)**: static setting children, each its own simple-preference node + submit; `default_model` = dynamic child reusing the model rows with its own submit |
| `/sandbox` | **mixed listing (§2.1)**: static `add` row (add workflow, fs value completion, §8) at the top + dynamic binding rows (remove workflow per row) |
| `/session` | dynamic level (sessions) → static action children → confirm via two-phase submit (§9) on delete |
| `/tree` | dynamic level (tree nodes) → static action children (`switch`, `compact`); `compact` modes are single-token enum children; `llm` leaf carries `.prompt` modifier (default value) + two-phase submit confirm |
| `/compact` | leaf that returns the rewritten path `/tree <id> compact ` |
| `/stat` | interstitial, no leaf |
| `/replay` | interstitial + leaf |

---

## 11. Traceability of resolved questions

| Question | Resolution |
| --- | --- |
| Q1 surface changes | Allowed where they buy uniformity (D1). |
| Q2 `<…>` disambiguator | Dropped; positional ids (D2). Revised: accepts clear the filter, traversal memory restores it (§4, §5). |
| Q3 modifier grammar | Dot-modifiers uniformly for values; children stay bare words (D3). |
| Q4 `/stat` `/replay` | In scope via interstitial (D4). |
| Q5 confirm | Revised (v2): two independent handlers (prep leaf + confirm accept). **v2.5:** two-phase submit protocol — handler-local state machine, not-overridable dialog service (§9). |
| Q6 tables | Generalize now, low cost (D6). |
| Q7 dataset values | Default per modifier (D7). |
| Q8 fs completion | Dynamic completion handler within modifier value completion (D8). |
| R8-1 spaced labels | Single-token; drop spaced display. |
| R8-2 space vs slash | Different contexts; structural terminators; parsing unaffected. |
| R8-3 paused accepts | Three distinct mechanisms (interstitial / confirm / editor handoff), §9. |
| R8-4 headers | Static `child_header` + per-row derived header (§6). |
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
| Active-state sourcing | Single engine mechanism (provider `DataResult::active_row`, §5.1) resolved to the `*` row per frame; open-time cursor seeds on it (§5.1). |
| Listing-open rule | Uniform: pinned selection shows the *parent* list with the pinned row highlighted; only the trailing space drills (§3). |
| Esc behavior | Per-level ladder: trailing space → frame replay → clear refinement → clear selection → clear buffer at root (§5.1). |
| Engine lifetime | Persistent per activation; per-activation state (cursor/frames/cache/accept-phase) lives **inside the engine**; host keeps the engine instance across keypresses; no rebuild, no take/restore, no Arcs (§2, §12). |
| Handler statefulness | v2.5: node-local state in the impl, epoch-cached; hand-written stateful closures replaced by the trait node's private per-node state (§2). Confirm = two-phase submit with handler-local state machine (§9). |
| Leaf result vocabulary | Generic `LeafResult<T>`: menu-semantic outcomes engine-typed; `T` = narrow host outcome (commit/quit/message/redraw) only. App-data mutations live in handlers; no canonical-line re-parsing (§9). |
| Node model | v2.5: nodes are trait impls (`NodeImpl`); default traversal is private/non-overridable and owns epoch-cached per-node state; providers/handlers attach to nodes, not path-keyed side tables (§2, §8). |
| Merged child sets | v2.5 new: one level = ordered union of child rows (static + enum + dynamic children each with own data/submit); field metadata on the parent; per-child submit dispatch (§2.1). |
| Node sharing | Reuse a node impl by *instantiating* it per parent (e.g. `/model` and `/set default_model`); no `Rc` sharing — plain local per-node state (§2). |
| Confirm dialog | v2.5: dialog is a not-overridable engine service; the handler owns the state machine and staged payload (`prepare` default-noop = `submit`; `SubmitResult::Prepared` pauses; `submit_resolved(yea)`) (§9). |
| Coordinator | v2.5: shrinks to the root entry point + the epoch counter; no centralized interpreter state machine, no menu-level confirm state (§12). |

---

## 12. Role split: engine vs. host (final state)

The recurring integration friction (cursor sync, marker policy, overlay
dispatch) traces to fuzzy ownership boundaries. The settled division:

**v2.5 restatement of §12's engine.** The engine is the **node tree**
(`NodeImpl` instances, §2); there is no separate "interpreter". The
generic machinery (path parsing, selection validation, rank-and-pin,
§7.1 filter/sort query, per-node cursor caches, the Esc ladder,
traversal frames, render-plan emission) lives in the default,
non-overridable traversal (blanket impl, §2). The **coordinator shrinks
to two things**: the root entry point (start a traversal from `/`) and
the **epoch counter** (incremented per menu open, threaded down the
traverse chain, invalidating per-node caches). There is no centralized
interpreter state machine and no menu-level confirm state (§9 — the
confirm flow is handler-local). Per frame the traversal produces, from
`(line, descriptor-tree, epoch, application context)`:

- the resolved level (node path) and whether it is a listing or a pinned
  level awaiting drill (the trailing-space rule, §3 — uniform, no host
  exceptions);
- the candidate rows (the merged child-set rows, §2.1, ranked, filtered,
  sorted), including the `*` active-state row resolved from the owning
  node's data response (§5.1);
- the cursor row (per-node cursor cache, §2), and Tab/Enter/Esc
  **decisions**: the new buffer text, whether to push/pop a traversal
  frame, and whether the accept is a submit (canonical line) or an
  intermediate state;
- Esc as the per-level ladder (§5.1) — pure buffer/state rewrites, done
  as a **root re-entry** with the shorter buffer (never an up-recursion),
  §2.

The engine is *pure*: no terminal I/O, no picocode types, no async. It
emits a **render plan** (level, rows with marker positions, header, title
hint) rather than strings where possible, so the host's table renderer can
format.

**Engine lifetime — persistent, with engine-owned per-activation state.**
The engine (the node tree) is *not* rebuilt per keypress. It is built once
per menu activation (menu closed → menu opened) and the host **keeps the
node tree instance** across the keypresses of that activation — there is
no serialization, no take/restore pair. Per-node caches, cursor state, and
any staged confirm payload live **inside the nodes** for the whole
activation; a fresh menu open simply **bumps the epoch**, which
invalidates them (§2). This removes the rebuild's costs: node data
snapshots never go stale (nodes read app state at call time, §2), the
data cache survives frames instead of re-fetching per keypress, and the
"forgot to carry X across the rebuild" bug class disappears. Persistence
requires no `Arc`s: the traversal reads the buffer at step entry and
mutates nothing app-owned; nodes capture what they need (§2). (The
`Activation` bundle and `take_state`/`restore_state` accessor pair were
the *rebuild-era* scaffolding for carrying per-activation state across a
rebuilt engine; they are deleted once the host holds the node tree
directly.)

**The host (`repl.rs`) owns everything not derivable from the prompt:**

- application context (ReplCtx: active model/session/tree/usage) — read
  by nodes at call time (§2), never passed through engine constructors;
- terminal I/O, table rendering (columndefs from the descriptor), row
  highlight painting, scrolling;
- applying the leaf result's narrow host outcome: control flow (commit /
  quit / stay), messages, redraw. App-data mutations happen in node
  `submit()` (§9) via shared handles; the host loop applies only what the
  outer loop owns. Engine purity rests on the crate boundary + generics,
  not on an intermediate result vocabulary;
- non-slash affordances: bare-prompt path completion, `!shell`, general Esc
  with an empty menu, editor handoff (`$EDITOR`).

| Layer | Owns |
| --- | --- |
| Engine (nodes) | the node tree + its default traversal (grammar, cursor caches, Esc ladder, frames, render plan); per-node epoch-cached state; the confirm dialog service (§9); key decisions |
| Nodes (app adapters) | all application data — read ctx at call time; supply rows (`data()`, incl. `active_row`, §5.1) and submit behavior (`submit()`/`prepare()`, §9); perform app-data mutations directly via shared handles |
| Host | app state itself, terminal I/O, rendering the plan, translating the submit result → `InputAction` |

**Dispatch rule**: there is exactly one `ActiveOverlay::Menu` shape in the
final state (impl Phase 7). The host's key loop is uniform: `state(line)` →
render plan → read key → `step(line, key)` → apply result (buffer edit /
frames / submit). Per-command code exists only in the node impls + their
data/submit members + leaf-result translation, never as overlay arms.

**Until Phase 7 completes**, legacy overlay arms may temporarily read host
state (markers, filters), but every *new* behavior added in Phase 6.5+ must
follow this split; no new host-side per-variant policy.
