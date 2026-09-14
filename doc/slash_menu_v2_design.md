# Slash menu v2 — design

Derived from the brainstorm/audit in `slash_menu_v2.md` (which contains the full
audit of the current implementation, the issue list, and the inline review that
produced decisions D1–D8 and resolutions R8-1…R8-8). This document states the
resolved design. Implementation staging will go into a separate plan document.

Inputs referenced: `slash_menu.md` (v1 UX contract, still authoritative for
anything not changed here), `src/repl.rs` (`ActiveOverlay` dispatch loop),
`src/model_catalog.rs` (`MODEL_MODIFIERS`), `src/table.rs`, sibling docs
(`settings_menu.md`, `sandbox_menu.md`, `tree.md`).

---

## 1. Goals and invariants

1. **Data-driven.** Every slash menu is described by a *menu descriptor* (node
   tree + handler attachments). The interpreter (generic overlay engine) derives
   all behaviour — filtering, cursor, promotion, commit, rendering — from the
   descriptor. Per-menu `ActiveOverlay` variants and hand-written
   `from_buffer` / `handle_tab` / `handle_enter` / `handle_esc` arms collapse
   into generic interpreter states.
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

## 2. Core model: the menu descriptor

A menu is a recursive node. One descriptor per command word; the root list
(`/`) is itself a descriptor whose children are the other command descriptors
(the existing `slash::registry()` content, re-expressed as nodes).

Each node has:

| Field | Meaning |
| --- | --- |
| `key` | The token that selects this node at its parent level. Space-free (R8-1). |
| `description` | Optional single-line description; fuzzy-searched together with `key` in the parent menu. |
| `child_header` | Optional single-line header shown when this node's child menu is active. |
| `children` | Nested nodes (static subtree). A node is a *list level* if it has either static children, or a data handler (below). |
| `modifiers` | Optional typed value/argument slots (§7): `key` + value kind + default + completion source + front-end/back-end flag. |
| `data_handler` | Optional data-fetch handler (marks the level dynamic; §8). Absent ⇒ static level. |
| `accept_handler` | Optional accept handler (§10). Absent ⇒ Enter behaves like Tab: add trailing space, drill into the child level. |
| `interstitial_handler` | Optional handler returning a static string, shown on Enter *before* any leaf call or drilling (§10). |
| `confirm` | Optional stateful confirm property on a leaf (§10). |
| `table` | Optional declarative column definition for list rendering (D6): headers, order, alignment, min-widths (priority), ellipsis vs wrap. |

Handlers attached to nodes receive as arguments: positional ids collected from
the path, and modifier key-values (typed).

### Level classification

- **Static level**: children are a fixed node list. Selection token = a child
  `key` (bare word).
- **Dynamic level**: rows come from a data handler. The selection token is a
  row id (opaque string, no spaces, typically uid-like). A dynamic node's
  `children` are **shared across all rows**: after a valid id, the next
  grammar level is that single static action list (if any) — e.g. `/tree`
  row-ids → `switch` / `compact` action level. So a dynamic node with static
  sub-actions encapsulates *two* grammar levels; there are no per-row child
  nodes.

### Value-typed children (positional enum / number values)

Besides plain static children (bare-word keys, each with its own handler), a
level may carry a single **value-typed child** — a node that matches *any* of
a defined set of values in the selection position:

- **`enum` child**: declares the candidate value set (e.g. `off|low|med|high`
  for `/thinking`). One node with one shared leaf handler — instead of N
  static children with identical handlers.
- **`number` child**: accepts any permissively-parsed number (int or float).

Design rule: use plain static children when values need distinct behaviour or
descriptions; use a value-typed child when values share one handler and
differ only in payload. This reuses the modifier machinery (completion
sources, enum sets, permissive parsing) in the selection position.

Grammar impact is minimal: a value-typed child occupies one selection
position (space-free token). Parse-level validity is *permissive* — any token
is a candidate selection; the enum set is a completion aid, not a parse
constraint. The backend validates at accept time (any prompt is valid, the
action may error — §7). Cursoring over the candidate set rewrites the token,
exactly like ids on dynamic levels.

---

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

### 5.1 Active-state sourcing (`active_selector`)

The "active" row of every candidate list must come from a single, uniform
mechanism inside the engine — the host never computes or renders it per
overlay:

- **`DataProvider::active_selector() -> Option<String>`**: dynamic providers
  report the id of the row that is currently in effect (e.g. the active
  model's `provider:name`, the current session id, the current leaf's tree
  entry id). The engine resolves the selector to a row index in the
  candidate set per frame and renders `*` there.
- **Enum / static levels**: the same selector notion, from the node
  descriptor (`active_selector` on the level or value child, e.g.
  `/thinking`'s currently-set level, `/persona`'s current persona) or from
  the provider supplying the enum rows. A level with no selector (root
  command list) renders no `*` anywhere.
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
| `.sort:<field` | Sort ascending by field (small first — e.g. cheapest price first). |
| `.sort:>field` | Sort descending by field (large first — e.g. highest index first). |
| `.sort:field` | Direction defaults to the field's declared `default_dir` (§8.1), so legacy names like `.sort:coding` keep working. |

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

Two handler kinds (R8-6):

1. **One-shot fetch** — called once per dynamic menu open; returns rows (id,
   description, optional typed key-value fields, default completion value for
   the selection token). The interpreter does all filtering/sorting over the
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

A dynamic level may also be nested under a static level (e.g.
`/set default_model` value = model listing; `/sandbox remove` value = binding
paths as ids).

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
- `id` (string) and `description` (string) are implicit fields: exact-match
  filterable, sortable, and always in the fuzzy-search space.

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
2. **Confirm (D5)** — a node property on a leaf. A *single handler instance*
   is used for both the prep call (e.g. compaction precompute) and the final
   accept/abort decision, because state must survive between them.
   Re-entering the same prompt from scratch would re-run the prep. This is
   expected to be the only stateful accept path.
3. **Editor handoff** — the external `$EDITOR` review *replaces the UI
   temporarily* and is driven by the leaf handler call itself (handler returns
   "launch editor"); not an interstitial. Cancellation re-enters the prompt
   via the existing `pending_prefill` mechanism.

**Leaf handler results** (formalized from R8-8): a leaf may

- perform its action and close the menu (success / failure with optional
  messages), or
- **return a new path** — rewriting the buffer to another menu state, e.g.
  `/compact` becomes a leaf that returns `/tree <last-user-id> compact ` (with
  trailing space so the compact menu shows immediately). No parallel
  `/compact` hierarchy exists any more; it is pure sugar over `/tree`.

---

## 10. Per-command target shape (summary)

| Command | Shape in v2 |
| --- | --- |
| `/` | static root list; children = command descriptors |
| `/model` | dynamic level (catalog, one-shot) + front-end field modifiers (§7.1; `MODEL_MODIFIERS` metadata seeds field/computed-field definitions), leaf = select model |
| `/thinking` | static level with one `enum` positional value child (`off|low|med|high`), shared leaf = SetThinking (§2) |
| `/persona` | dynamic leaf with dataset/enum string value source |
| `/set` | static level of setting children, each with typed modifier-like value machinery; `default_model` nests a dynamic model level |
| `/sandbox` | static subcommand children (`add`/`remove`); `add` carries back-end fs modifiers (`.ro:` (default tab complete), `.rw:`); `remove` is a dynamic value menu with ids = paths |
| `/session` | dynamic level (sessions) → static action children → confirm leaf on delete |
| `/tree` | dynamic level (tree nodes) → static action children (`switch`, `compact`); `compact` modes are single-token enum children; `llm` leaf carries `.prompt` modifier (default value) + confirm property |
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
| Q5 confirm | Node property, stateful single handler instance (D5). |
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
| Static children on dynamic nodes | One static action level for *all* rows of the dynamic level; no per-row child nodes (§2, §3). |
| Positional enum / number values | `enum` / `number` value-typed child kinds; shared handler; permissive parse, backend validates (§2). |
| Field filtering / sorting | `.field:value`, `.field:>v`/`<v`, `.sort:>f`/`<f` (+ per-field default direction); computed fields replace named sorts (§7.1). |
| Compound filter specs | Dropped; express as separate per-field tokens (§7.1). |
| Cursoring & accept model | *Post-v1 design review:* up/down moves the view cursor only; Tab/Enter act on the cursor row, commit path+id, clear refinement; traversal memory + scoped edit invalidation give seamless back-navigation (§4, §5). |
| `*` marker | Always the currently-active state from application context, never the typed selection (§5). |
| Active-state sourcing | Single engine mechanism (`active_selector` on providers/descriptors) resolved to the `*` row per frame; open-time cursor seeds on it (§5.1). |
| Listing-open rule | Uniform: pinned selection shows the *parent* list with the pinned row highlighted; only the trailing space drills (§3). |
| Esc behavior | Per-level ladder: trailing space → frame replay → clear refinement → clear selection → clear buffer at root (§5.1). |

---

## 12. Role split: engine vs. host (final state)

The recurring integration friction (cursor sync, marker policy, overlay
dispatch) traces to fuzzy ownership boundaries. The settled division:

**The engine (`slashdot_menu`) owns everything derived from the prompt
state.** Per frame it produces, from `(line, cursor map, providers,
descriptor)`:

- the resolved level (node path) and whether it is a listing or a pinned
  level awaiting drill (the trailing-space rule, §3 — uniform, no host
  exceptions);
- the candidate rows (ranked, filtered, sorted), including the `*`
  active-state row resolved from `active_selector` (§5.1);
- the cursor row (sticky cursor map), and Tab/Enter/Esc **decisions**:
  the new buffer text, whether to push/pop a traversal frame, and whether
  the accept is a submit (canonical line) or an intermediate state;
- Esc as the per-level ladder (§5.1) — pure buffer/state rewrites.

The engine is *pure*: no terminal I/O, no picocode types, no async. It
emits a **render plan** (level, rows with marker positions, header, title
hint) rather than strings where possible, so the host's table renderer can
format.

**The host (`repl.rs`) owns everything not derivable from the prompt:**

- application context (ReplCtx: active model/session/tree/usage) and
  feeding it to providers as `active_selector` data;
- terminal I/O, table rendering (columndefs from the descriptor), row
  highlight painting, scrolling;
- dispatching the engine's *submit* result to `InputAction` (SetModel,
  SetThinking, Quit, …) — the engine never interprets picocode actions;
- non-slash affordances: bare-prompt path completion, `!shell`, general Esc
  with an empty menu, editor handoff (`$EDITOR`).

**Dispatch rule**: there is exactly one `ActiveOverlay::Menu` shape in the
final state (impl Phase 7). The host's key loop is uniform: build engine →
`state(line)` → render plan → read key → `step(line, key)` → apply result
(buffer edit / frames / submit). Per-command code exists only in
descriptors + providers + leaf-result translation, never as overlay arms.

**Until Phase 7 completes**, legacy overlay arms may temporarily read host
state (markers, filters), but every *new* behavior added in Phase 6.5+ must
follow this split; no new host-side per-variant policy.
