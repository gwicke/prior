# Slash menu v2 — implementation plan

Implements the design in `slash_menu_v2_design.md` as a new sub-crate,
**`slashdot_menu`**, independent of picocode itself (design rationale:
crate independence was reviewed inline; summary below). No new code-level
detail beyond what is needed to stage and verify the work; types/signatures
are sketched where the boundary matters.

---

## 1. Crate layout & independence

```
crates/slashdot_menu/
  Cargo.toml            # no picocode dependency
  src/
    lib.rs
    node.rs             # MenuNode descriptor, FieldSpec, ColumnDef
    value.rs            # Value (String | Number), ValueKind, ModifierSpec
    grammar.rs          # path tokenizer: terms / modifiers / selection
    filter.rs           # field filters + sorting engine (§7.1)
    cursor.rs           # selection/pinned-id/view rules (§4)
    interpreter.rs      # state machine: from_buffer, tab, enter, esc
    render.rs           # render plan emission (rows/markers/headers)
    frame.rs            # minimal frame abstraction (ratatui-like)
    table.rs            # table builder (moved/copied from src/table.rs)
    fuzzy.rs            # fuzzy ranking (moved/copied from src/fuzzy.rs)
  tests/                # headless, table-driven tests
```

### Independence boundaries (agreed)

- **Editor buffer**: a narrow trait, no editor internals:

  ```rust
  pub trait PromptBuffer {
      fn text(&self) -> String;
      fn set_text(&mut self, s: &str);
      // cursor position is only needed for read-side token extraction;
      // the interpreter always rewrites whole lines via set_text.
  }
  ```

  Implemented in picocode over the existing editor (`set`/`set_text` is
  already the primitive the v1 overlays use). No cursor movement, no spans,
  no editing operations leak into the crate. The *view cursor* (highlighted
  row) is separate: the host passes it into `tab_at`/`enter_at` (§4).

- **Rendering**: the crate emits a **render plan**, not a ratatui frame. The
  plan is plain data (rows of styled segments, marker chars, alignment), so
  picocode paints it with ratatui today and any replacement later. The crate
  depends only on ANSI-style primitives it owns (`Style { fg, bg, bold }`,
  no ratatui types).

- **Frame abstraction**: `frame.rs` defines the panel geometry contract the
  interpreter needs (available width/height, vertical budgeting for header
  block vs option rows). Close in shape to ratatui's `Frame`/`Rect` but
  defined locally; picocode's paint step maps plan + real frame.

- **Table & fuzzy**: `src/table.rs` and `src/fuzzy.rs` move into the crate
  (picocode re-exports or depend on the crate — decide at code time; moving
  and re-exporting avoids duplication, they have no picocode-internal deps).

Everything picocode-specific — catalog, sessions, tree, settings, bindings,
fs, editor — stays behind handler traits defined in `node.rs` (§2).

---

## 2. Descriptor & handler traits

Per design §2. Core types (sketch):

```rust
pub enum Value { Str(String), Num(f64) }

pub struct MenuNode {
    pub key: String,
    pub description: Option<String>,
    pub child_header: Option<String>,
    pub children: Children,          // static / dynamic / value-typed
    pub modifiers: Vec<ModifierSpec>,
    pub accept: Option<AcceptHandler>,
    pub interstitial: Option<Interstitial>,
    pub confirm: bool,
    pub table: Option<ColumnDef>,
}

pub enum Children {
    Static(Vec<MenuNode>),
    Dynamic(DynamicSpec),            // one-shot or live-filter fetch
    Value(ValueChild),               // enum / number positional value
}

pub enum ValueChild {
    Enum { values: Vec<String>, },   // completion set only
    Number,
}
```

Handler traits (object-safe, stateless unless `Confirm`):

```rust
pub trait DataProvider {
    // One-shot: called once per menu open.
    fn fetch(&mut self, modifiers: &ModifierMap) -> Vec<Row>;
    // Live-filter: called per level change; Ok(None) = "not changed".
    fn fetch_live(&mut self, query: &str, modifiers: &ModifierMap)
        -> Result<Option<Vec<Row>>, Error>;
}

pub struct Row {
    pub id: String,
    pub description: Option<String>,
    pub fields: Vec<(String, Value)>,     // typed key-value fields
    pub computed: Vec<(String, Value)>,   // computed fields (§8.1)
    pub default_completion: Option<String>,
}

pub trait LeafHandler {
    fn invoke(&mut self, args: &MenuArgs) -> AcceptOutcome; // close | new_path
}
```

Note on `Children::Value`: even a permissive `Number`/free `Enum` child needs
candidate rows for rendering/cursor; for non-dataset values the candidate set
is derived from the enum list itself (numbers: none, free-typed). Rendering
and cursoring work identically to dynamic levels once candidates exist.

---

## 3. Phased implementation

Each phase ends compiling and green. Tests are named per phase below; the
headless test harness (fake `PromptBuffer`, canned providers) is built in
Phase 0 and reused everywhere.

### Phase 0 — crate scaffolding + moved primitives

- Create `crates/slashdot_menu`, workspace entry, no external deps.
- Move `table.rs` + `fuzzy.rs` in; picocode `use`s the crate (re-export in
  `lib.rs` to keep internal paths stable).
- Port the ~existing table/fuzzy unit tests.
- **Verify**: `cargo test -p slashdot_menu` and full workspace build.

### Phase 1 — value & grammar

- `value.rs`: `Value`, permissive parse (int/float/str), comparison.
- `grammar.rs`: tokenizer implementing design §3 — split a buffer into
  `[terms][modifiers][selection?]` per level, structural terminators
  (dot-modifier / valid id / end), multi-word values, multi-word fuzzy terms.
- **Tests**: token classification tables (selection vs term vs modifier),
  multi-word value consumption, terminator precedence. This phase is where
  R8-7 (space-free selection vs word-consuming values) is proven.

### Phase 2 — filter/sort engine

- `filter.rs`: field filters (`.field:value`, `>`, `<`, `>=`, `<=`),
  sorting (`.sort:>f` / `<f`, per-field `default_dir`), composition (AND,
  single sort, missing-value ordering), computed fields, enum-as-completion
  semantics.
- **Tests**: port the `ModelFilter` behaviors as table tests — the existing
  `/model` filter cases become the first consumer of the generic engine;
  missing-value sentinel normalization (design §7.1) gets explicit cases.

### Phase 3 — interpreter core (static levels)

- `cursor.rs` + `interpreter.rs`: state derivation from buffer, Tab/Enter/Esc
  contract (design §5), exact/unique-match preselection, value-typed
  children (§2). *Revised per design review:* accepts commit path+id and
  clear refinement (§4); Tab/Enter act on the host cursor row
  (`tab_at`/`enter_at`); Tab on a bare level name promotes (trailing space).
- **Tests**: static-level scenarios only — `/thinking`-shaped enum child,
  drill/promote, Esc pop, accept-clears-terms, cursor-row Tab.

### Phase 4 — dynamic levels + rendering

- Wire `DataProvider` into the interpreter: row candidate sets with
  candidate-set validation of selection tokens, preprocessing pass
  (distinct string values per field, D7 dataset-derived completion).
- `render.rs`: render plan — title, `child_header`, per-row derived header
  (R8-4), table from `ColumnDef`, markers, vertical budget (frame.rs).
- **Tests**: dynamic scenarios (fake provider), header composition, table
  output snapshot vs. current menus' shape.

### Phase 5 — accept flows

- Leaf invocation, interstitial, confirm (stateful instance), editor handoff
  signal, "return new path" rewrites (design §9).

### Phase 5.5 — traversal memory (post-design-review)

> **Follow-up (cursor ownership, learned in Phase 6):** the host-cursor
> contract above ("Tab/Enter act on the host cursor") was implemented as a
> reconciliation loop — host `sel` overrides the engine's `active` on each
> key, and the host re-derives `sel` from `active` next frame via
> `cursor_sel_policy` with per-variant flags (`seed_on_open`,
> `follows_active`, `last_active`). This dual-cursor split is a recurring
> bug source (root-list pins ignored, cursor/marker conflation). **Phase 6.5
> (below) replaces it: the engine owns the cursor.**

- `MenuEngine::step_at(line, cursor, key, frames)`: Tab/Enter act on the
  host cursor; frames `{line, cursor, level}` pushed on accept/drill; Esc
  replays newest-first (restore filter state + cursor) before structural
  pops; strict up-walk guaranteed by push-only-on-descend.
- Scoped edit invalidation: `on_edit(prev, now)` clears the edited level's
  frames only; pure trailing-space removal = navigation (clears nothing).
- `*` marker from application context (`ReplCtx`), never the typed
  selection.
- Host loop carries `menu_frames`; slash lines route edits through the
  engine.
- **Tests**: frame push on accept AND drill, Esc replay chain (accept →
  replay → structural pop), scoped invalidation (deep edit keeps
  ancestors), trailing-space-removal is not an edit.
- **Tests**: `/stat`-like (interstitial no-leaf), confirm prep→accept state
  survival, leaf-returns-path (the `/compact` case).

### Phase 6 — picocode integration (per command, design §10 order)

Migrate commands one by one, each step replacing bespoke overlay variants and
`from_buffer`/`handle_*` arms:

1. `/thinking` (value-typed child, simplest end-to-end proof)
2. `/` root list
3. `/persona`, `/set` (+ `default_model` nested dynamic)
4. `/model` (port `MODEL_MODIFIERS` to `ModifierSpec` + computed fields;
   `get_filtered_catalog` logic becomes generic filter tests)
5. `/session` (+ delete confirm)
6. `/tree` (+ `switch`/`compact` shared action level, `.prompt` modifier,
   confirm), `/compact` (leaf returns path)
7. `/sandbox` (live-filter fs provider, `.ro:`/`.rw:` modifiers, remove
   dynamic value menu)
8. `/stat`, `/replay` (interstitial)
9. Delete dead overlay variants + arms as each lands (Phase E of the design
   roadmap — do it incrementally per step, not at the end). *(Revised by
   Phase 7.1: the remaining bespoke variants collapse in one switch
   instead; steps 1–8 land descriptors/handlers per command, the host
   arms survive until the switch.)*

- **Verify** per step: existing UX behaviours from `slash_menu.md` keep
  passing (the v1 doc doubles as an acceptance checklist, superseded
  row-by-row where the v2 design revised cursoring/accept — see §4–§5),
  plus new v2 behaviors: cursor-row Tab/Enter, accept-clears-filter with
  Esc traversal-memory restore, bare-level Tab promote, positional ids
  without `<…>`.

### Phase 6.5 — engine-owned cursor (consolidation)

The Phase 6 integration left cursor sync split between the host (`sel`,
reconciled per frame via `cursor_sel_policy` with per-variant flags) and the
engine (`active`, derived from the typed pin). That feedback loop is brittle
and produced several regressions (root-list pins ignored, `*`/`>` marker
conflation). This phase makes the **engine the single owner of the cursor**:

- `MenuEngine::step_at` accepts `HostKey::Up`/`Down` (and `Home`/`End` /
  PgUp/PgDn) and mutates the engine's active row. The host no longer
  tracks `sel` for engine-driven overlays; it passes key events only.
- Each frame the host queries `state(line)` and renders it verbatim:
  one marker rule (`>` = cursor row; `*` = current-state row, only where a
  persistent "current" exists — e.g. active model/thinking level/session —
  never on the root command list), no `cursor_sel_policy`, no
  `follows_active`/`last_active` tracking, no per-variant marker decisions
  in `display_rows`.
- One-shot behaviors become explicit engine API instead of host policy
  flags: `set_cursor(idx)` for open-time seeding (e.g. `/tree` starts on
  the current leaf row).
- Non-engine overlays (path completion, legacy arms pending migration)
  keep the v1 host cursor until their step lands.
- **Tests**: Up/Down mutate engine state and survive a buffer re-derive;
  rendered marker matches engine state for every migrated overlay;
  open-time seeding via `set_cursor`; Esc replay restores the cursor.

### Phase 6.6 — uniform listing rule, active-selector, Esc ladder

Follow-ups surfaced while manually exercising `/model` after Phase 6.5
(design §3 listing rule, §5.1 active-selector, §5.1 Esc ladder, §12 role
split). The engine owns all of it; the host must not keep per-command
exceptions.

1. **Listing-open rule (engine)**: a *pinned* selection on a listing node
   (children or handler) shows the **parent** list with the pinned row
   highlighted; only the trailing space shows the child menu. Today
   `/model` (no space) opens the model dialog via the host's
   "open without trailing space" match list — remove that list and let
   `state("model")` route to the root `CommandList` (the interpreter
   already returns `path=[]`, `sel=Some("model")`, the root rows with the
   pinned row as cursor). Applies to `/thinking`, `/set`, `/persona`,
   `/session`, `/tree`, `/sandbox`, `/compact` uniformly.
2. **`active_row` (engine)**: providers report the currently-in-effect id
   in their `fetch` response (`DataResult::active_row`); the engine
   resolves it to the `*` row per frame. Migrate the host's
   ctx-driven `active` computations (Model, Thinking, Persona, Session,
   Tree) into providers; `MenuState` has `active_row_marker: Option<usize>`
   distinct from the cursor `active`; open-time cursor seeds on the marker
   row when no sticky cursor exists. Fixes "`/model ` doesn't focus the
   active model on entry" (the cursor currently seeds at the pinned/recent
   top, not the selector).
3. **Esc ladder (engine)**: replace the host's per-variant `handle_esc`
   buffer rewrites with the §5.1 ladder, decided in the engine
   (trailing space → frame replay → clear level terms/modifiers → clear
   level selection → clear buffer at root). Repairs "Esc from a filtered
   view clears the entire prompt".
4. **Cursor consistency `/model` vs `/model `**: the pinned root level and
   the drilled listing must share cursor provenance (same sticky-map
   seeding; entering the dialog seeds on the active-selector row, backing
   out restores the root cursor).
5. **Tab-into-prompt mismatch**: Tab completing from a filtered view must
   write the *cursor row's* id (verify `tab_at` uses the sticky cursor,
   not the pre-filter pin; regression-guard with a rank-reorder test).

### Phase 6.7 — persistent engine + handler-owned state (end-state plumbing)

Mechanical migration to the §12 settled ownership model. The engine keeps
no per-frame rebuild; handlers become the sole carriers of app state.

**Shape of the end state** (drives every step below):

- The **descriptor tree is long-lived**: built once (per REPL, refreshed
  only when the static metadata itself changes), with stateful handler
  closures bound at build time. Handlers capture shared handles into
  application state and read/write it **at call time**; closures never go
  stale because the engine is persistent (§12).
- Shared application state lives in **shared cells** — `Rc<RefCell<…>>`
  (single-threaded app) — owned by the REPL; `Arc` only where a background
  thread participates (e.g. catalog refresh). Handlers mutate app data
  directly through the cells; the host loop applies only loop-owned
  effects (control flow, terminal, transcript, redraw).
- The **model provider is one-shot**: `fetch` runs once per menu
  activation into the data cache; **all** filtering/sorting stays
  engine-side over the cached rows (§7.1). The active row is **data**, not
  a separate trait call: the provider sets `DataResult::active_row`
  (the id of the current state — active model / current session / current
  leaf) inside `fetch`, from its own live data source. The engine resolves
  it to the `*`-marked row per frame; a fresh menu seeds its cursor on it.

1. **App-state structuring (host, prerequisite)**: introduce a
   `MenuShared`-style handle bundle holding exactly the mutable slices
   menu handlers touch. Handles are **live stores**, not snapshots:
   handlers read and write the same underlying data the rest of the app
   uses. All `Rc<RefCell<…>>` (single-threaded app); `Arc` only where a
   background thread participates (the catalog's global cache):
   - settings store (all `/set` writes; the **active model id lives here**,
     so the model provider reads it in `fetch`);
   - session repository (live `JsonlSessionRepo`; the `/session` listing
     provider calls `list_sessions_sync` on it) + current-session id;
   - tree + leaf read mirror (the async `Session` only mutates between
     turns, so the REPL refreshes this sync mirror once per loop
     iteration; the `/tree` provider reads it per `fetch`);
   - catalog: the **global cache** `catalog_cached()` IS the live store —
     the provider reads it directly, no snapshot cell.
   The REPL keeps building `ReplCtx` snapshots from the same underlying
   storage, so the rest of the REPL is untouched. Explicitly *not* in
   scope: restructuring REPL internals beyond these slices.
2. **Long-lived descriptors; constructors lose ctx (bridge)**:
   `MenuEngine::new(&MenuShared)` — no `ReplCtx`, no `for_model`.
   Descriptors built once; leaf/confirm handlers registered at build time
   as closures capturing the shared handles. **Providers attach at
   construction** (the host's `menu_cursors`/`menu_frames` live for the
   whole `read_input` loop, so a once-attached provider survives every
   frame — no per-variant `with_provider` wiring remains).
   Providers are the sole owners of their data source:
   - `ModelProvider`: `fetch` reads the **global catalog cache directly**
     and the settings cell (for `active_row`); **no filter capture** — the
     engine's query compiler applies the prompt's `.sort:`/field modifiers
     over the cached rows (§7.1), so `for_model`'s merged-`ModelFilter`
     ladder disappears. The provider also emits rows in its **default
     sort order** (`fetch_recent_and_popular`: active model + 5 most
     recent in LRU order, then repository popularity) — the engine's
     `apply_query` passes un-`.sort:`ed provider order through, so no sort
     = provider order. The v1 host-side LRU/recent boost, its five-row
     window, and `model_overlay_entries`' fuzzy+rank blend die: the boost
     folds into the provider's default ordering (data-handler concern),
     fuzzy narrowing is the engine's generic `rank_and_pin`. Any explicit
     `.sort:`/field filter is applied generically like for any table.
   - `SessionProvider` / `TreeProvider`: rows read from the live repo /
     sync tree mirror at `fetch` time (session provider keeps its
     current-first + recency order in its own ordering); `active_row` is
     the current session id / leaf id likewise.
   Bridge constructors collapse to the single `new`; **all host
   per-command provider wiring** (`engine_for`'s Session/Tree arms,
   `for_model`, `model_engine_args`) is deleted in the same step.
   Rendering is generic: the host's model table shows plain provider
   fields (numbers as-is, no currency prefix/glyphs); the `/set
   default_model` + modifier-completion paths still use the v1
   `parse_model_filter`/`model_selection_overlay_entries` temporarily
   (they migrate to the same provider later — only the leaf/handlers
   differ).
   **Revised (step-2 findings)**: `DataProvider::active_selector()` is
   removed entirely — the active row is part of the provider *response*
   (`DataResult::active_row`), set inside `fetch`. No mid-menu settings
   mutation is possible (any leaf handler both terminates the menu
   invocation), so one `active_row` per activation is always correct.
3. **Host holds one persistent engine (host + bridge; replaces the
   rebuild ceremony)**: `read_input` constructs `MenuEngine::new(&MenuShared)`
   **once per prompt** and keeps the instance for the whole activation —
   no rebuild per keypress, no `Activation` serialization, no
   `take_activation`/`restore_activation`/`take_cursors`/`restore_cursors`.
   Cursor map, traversal frames, data cache, and accept phase all live
   *inside* the engine; the display path calls `engine.interpreter.state(&line)`
   on the same instance and the key path calls `engine.step_at(...)` on it.
   - `from_buffer_cursors` stops constructing engines (root list + each
     per-command overlay derive from the persistent engine's `state()`);
   - `handle_tab`/`handle_enter`/`engine_esc`/`on_edit` stop constructing
     engines — they step the shared instance; frames stay engine-owned;
   - the host's `menu_frames`/`menu_cursors` locals, `engine_seed`/
     `engine_arrow`/`engine_esc` restore dance, and the `Frame` bridge type
     are all deleted (frames/cursors never leave the engine);
   - `/stat`'s legacy `stat_phase` stays a host field only because that
     overlay is not yet engine-backed; it disappears at Phase 7.1.
   The `Activation` bundle + accessor pair (Phase 6.7 step 1) was the
   rebuild-era scaffolding; it is deleted here, not extended.
4. **Generic leaf result + confirm two-handler (engine + bridge)**:
   `LeafResult<T>` (design §9): menu-semantic variants stay engine-typed
   (close, new path/R8-8, editor handoff, interstitial pause); `T` is the
   **narrow host outcome** `{ flow: Stay | Commit(line) | Quit,
   message: Option<String>, redraw }` (design §9's redraw signal). Node
   gains `confirm_handler` alongside
   `leaf_handler` (boolean `confirm` deprecated): the prep step is the
   `leaf_handler` returning a payload the engine stashes in
   `ConfirmPrep`; the accept step invokes `confirm_handler(payload)`.
   Drop `ConfirmState` instance threading and the host's translation
   table. Migrate `/tree <id> compact llm` and `/session … delete` to the
   pair as the reference cases; all other leaves may temporarily return
   `T = Commit(line)` so the host's canonical-line parse keeps working
   until their handler migration (Phase 7.3).
5. **RenderPlan emission (bridge)**: the bridge maps `MenuState` →
   `RenderPlan` verbatim (§6): title from the descriptor path, static
   child header, per-row derived header (R8-4: row fields /
   `child_description`), candidate rows via the table builder +
   declared columndefs, `>` cursor row, `*` active-selector row (from
   `active_row_marker`; `*` wins per §5.1), interstitial/confirm panels
   as plans. No host-side per-variant titles, headers, marker decisions,
   or column layouts remain for menu overlays.
   **Implemented**: `DataProvider` gains a `DataResult::columns`
   (`Vec<ColumnDef>` — field name, header label, alignment, min width,
   shrink priority), threaded through `MenuState::columns`; the model
   provider declares its 6 columns; `PlanBuilder::build` renders columns
   when declared and paints the `*` active row (design §5.1, `*` wins on
   the shared cursor/active row). The bridge adds
   `MenuEngine::render_plan(line, width)` producing the plan. Host-side
   per-variant render arms are now the remaining Phase 7.1 cleanup.
6. **Tests (persistent-engine contract)**:
   - persistence: a single engine instance stepped through a key sequence
     (drill, arrows, edit, Esc replay, Tab, Enter) maintains cursor map,
     frames, and data cache across all steps — no host sewing required;
   - activation lifecycle: a *new* `MenuEngine::new(&MenuShared)` per menu
     open gives a fresh data cache + accept phase (the reopen clears
     per-activation state like the old rebuild did);
   - handler closure reads live shared state: a settings change made by
     one handler is visible to the next frame's provider/selector without
     any rebuild;
   - model provider: `fetch` called exactly once per activation despite
     keypresses/filter edits; `active_row` reflects the current settings
     (no mid-menu mutation is possible);
   - confirm two-handler flow: prep → pause → Esc cancel → re-prep →
     accept applies the payload;
   - engine purity: `cargo build -p slashdot_menu` in isolation (the
     crate must compile without the parent in the workspace).

#### Follow-ups from the step-2 (6.7-1/2) implementation

Recorded so they are not lost; each is a later-phase cleanup, not part of
step 2.

- **Model title/modifier summary**: `ActiveOverlay::Model` now carries a
  stub `ModelFilter::default_for_coding()` only for `title()`; the
  modifier summary (`sort:coding`, `price≤0.5`, …) the v1 table showed in
  the header no longer renders. Recover it generically once the engine
  exposes the parsed per-level modifiers (the `Query`/`FieldSpace` already
  have them) or drop the title entirely at the Phase 7 overlay collapse.
- **Rendering consolidation**: the host's `Model` `display_rows` builds a
  plain column table (`Model/Ctx/Coding/Price in/Price out/Reason`) from
  row fields — the same job the engine's `render.rs` does for dynamic
  levels. Delete the host arm and paint the engine's `RenderPlan` verbatim
  (Phase 6.7-5 / Phase 7.1). The `.field:`/`.sort:` completion overlay and
  `/set default_model` still route through v1 `parse_model_filter` /
  `model_selection_overlay_entries`; migrate them to the same provider
  (only the leaf/handlers differ) and delete the v1 helpers.
- **Number-cell formatting stopgap** (resolved in 7.1 slice-1): the
  `Model` overlay hard-coded a 2-decimal round + trailing-zero trim
  (`format_num_cell` / `trim_trailing_zeros` in `repl.rs`) because the
  model table was host-rendered. 7.1 slice-1 moved rendering into the
  crate: `Value::fmt_num` rounds to 2 + trims; the host helpers are
  deleted. The *configurable* decimals/padding (`num_decimals` /
  `zero_pad` on `FieldSpec`) remains a future refinement (noted as
  TODO on `fmt_num`) but is no longer a correctness stopgap.
- **`active_row` staleness semantics**: providers set `active_row` once
  per `fetch` (= once per activation, since the engine fetches provider
  data once when a level first opens). Handlers can't change settings
  mid-menu (any leaf terminates the invocation), so one `active_row` per
  activation is always correct; re-polling live cells per frame is not
  needed.
- **`model_catalog::provider_available` dimming**: the v1 table dimmed
  rows whose provider wasn't available (ANSI `\x1b[2m`). Dropped with the
  custom arm, since it was a per-row presentational computation in host
  code. If wanted, the provider can surface it as a row field (boolean =>
  number) and the generic render can style it; otherwise the behavior is
  intentionally gone.
- **`recent`/`model_filter` on `ReplCtx`**: still populated and used by
  the temporary `/set default_model` + completion paths, and `recent` seeds
  `MenuShared.recent_models`. Once those migrate, `ReplCtx.model_filter`
  and the `run_interactive` <-> `MenuShared` recent-sync ceremony can be
  deleted.
- **`max_age_days` modifier**: has no row field (the row carries
  `created`); the generic engine drops `.max_age_days:N` as unknown field.
  v1 mapped it to a created-age cutoff. Either add a computed `age_days`
  field in the model provider or drop the modifier from `MODEL_MODIFIERS`.

### Phase 7 — the switch (host) + cleanup

The host conversion happens **once, at a single switch point** — not per
command. Before the switch, descriptors/handlers exist; after it, the
host has no per-command menu logic at all.

7.1. **Overlay collapse (host)**: `ActiveOverlay` shrinks to
   `None | Help | PathComplete | Menu`, where `Menu` carries only the
   engine's `RenderPlan` (+ a handle to the `MenuState` the plan was
   built from). `from_buffer_cursors`' per-command arms collapse to:
   buffer starts with `/` → menu (engine `state(line)` + plan); Help
   view; bare-prompt path completion; `None`. The "open without trailing
   space" word list and `bare_listing_word` special cases die (uniform
   listing rule, §3/6.6-1). The REPL owns **one persistent
   `MenuEngine`** per prompt (from 6.7-3); a *new* engine per menu open
   drops per-activation state (data cache, accept phase) and the sticky
   cursor map / traversal frames reset — matching the reopen semantics
   the old rebuild gave.
    **Slice-1 (implemented)**: the fully-engine-backed `Model` overlay now
    renders the engine's `RenderPlan` verbatim — the host `display_rows`
    model arm, model `Table`/columns, and the `format_num_cell`/
    `trim_trailing_zeros` stopgap are deleted; `ActiveOverlay::Model`
    carries `plan: RenderPlan` and the loop paints `plan.paint()` directly.
    Modeling number/decimals/wrapping moved into the crate (`Value::fmt_num`,
    `PlanBuilder::build` → `render_multiline` + `fmt_num`). The model's
    title now comes from the descriptor `child_header` ("Models") via
    `render_plan`/`level_title` (design §12). Remaining overlay collapse is
    incremental (per fully-engine-backed command): thinking/persona/session/
    tree list still render via host `display_rows` because their accept
    (`cands→SetPersona`, `names`, host `targets`, …) is host-side — porting
    only their render would be throwaway. They follow in 7.3 with their
    accept migration.
    **Slice-2 (implemented)**: `Persona` migrated to the engine end-to-end
    like `Model` — accept (Tab/Enter) routes through `engine.step_at`
    (canonical `persona <id>` → host maps to `SetPersona`, "none" → empty
    name), and `ActiveOverlay::Persona` carries `plan: RenderPlan`,
    rendering `plan.paint()` verbatim. The `*` marker is resolved by the
    engine from a new `PersonaActiveProvider` (design §5.1): the static
    value-child level registers an [`crate::slashdot_menu::provider::ActiveProvider`]
    supplying just the active id (from `settings.default_persona`); the
    engine resolves it against the descriptor rows per frame. This removes
    the host-side `plan.active = ctx.persona…` injection — one uniform
    active-state mechanism (same as `DataResult::active_row` for dynamic
    levels).
    **Slice-3 (implemented)**: `Thinking` migrated end-to-end, identical to
    persona. `MenuShared` gains `current_thinking_level` (a mirror of the
    runtime `config.thinking_level`, synced by the REPL each loop); a
    `ThinkingActiveProvider` reads it for the `*` marker; `ActiveOverlay::
    Thinking` carries `plan: RenderPlan` and paints verbatim. Also fixed the
    fresh-level cursor seeding in `state()`: an explicit typed/pinned
    selection wins over the active-row marker for the seed (previously the
    active marker overrode a pin, breaking `thinking_enter_submits`).
    **Slice-4 (implemented)**: the host's dotted-modifier completion for
    `/model` (`Mod` overlay + `modifier_overlay_entries`/`modifier_value_rows`
    + `MODEL_MODIFIERS`) moved into the generic engine whose §7.1 field
    logic it now exercises:
    - `Interpreter::modifier_options(line)` — the generic completion for a
      trailing dotted token: a bare `.key` lists all field names (all
      fields filterable+sortable per user guidance — no special "sort"
      field, but `.sort:` also completes over all field names);
      `.field:value` on a string field lists distinct dataset values
      (fuzzy); number fields return no candidates (free-form → hand back
      to the list).
    - `Row::display_field` (design §8.1 display-string override): a
      per-field verbatim display string used for table cells but ignored by
      sort/filter/completion (which read raw `field_value`). Lets a
      provider refine display (ISO date, price glyph) with no engine
      complexity.
    - `ModelProvider::field_meta()` + computed fields: the model level
      declares its field specs (types + default sort dirs); `created` is
      exposed as an ISO date *string* (sortable/filterable, no engine "N
      days ago"); `price`, `coding_price`, `intelligence_price`,
      `agentic_price` are plain provider-supplied computed fields.
    - Removed the host `Mod`-overlay entry path; the model arm calls
      `engine.interpreter.modifier_options`. `MODEL_MODIFIERS`/
      `ModifierSpec` remain only for the still-v1 `/set default_model`
      completion path.
    - **Exit-on-trailing-space** (design §5.1 listing exit): a trailing
      space after an *exact* dotted completion (e.g. `.sort:coding `)
      dismisses the overlay and returns to the list — an obvious path to
      apply the sort. Enter/Tab on a completed `.name:value` appends the
      trailing space (exit + apply); a value-in-progress `.name:` does not.
    - **Value nodes are listing nodes**: completing a value command's word
      (`/think` → `thinking `) now drills with a trailing space exactly like
      a static/dynamic listing, instead of pinning and requiring a second
      Tab. (A completion *inside* an already-drilled value level, e.g.
      `thinking ` → `med`, still pins the value token.)
    - **`.sort` injected into the `.` auto-complete**: `sort` is really meta
      (it names a field) but from the user's perspective it is a dot-modifier
      like any other, so it appears in the bare-`.` field list. The `.sort:`
      value completion still completes over the *real* fields only.
7.2. **Uniform key loop (host)**: `handle_tab` / `handle_enter` /
   `handle_esc` / arrows / paging collapse to one arm:
   `engine.step_at(line, HostKey, …)` → apply the `StepResult` (buffer
   rewrite + restore-cursor to the editor) and the `HostOutcome` to the
   loop (`Stay` → redraw, `Commit(line)` → submit, `Quit` → exit);
   messages printed; interstitial/editor/confirm arrive as
   `AcceptOutcome`. `cursor_sel_policy`, `engine_for`, `engine_seed`,
   `engine_arrow`, `engine_esc` disappear (already removed in 6.7-3).
   **Unmigrated commands ride a temporary
   shim**: their descriptor's leaf handler returns `T = Commit(line)` and
   the legacy `SlashKind` parse in the submit path stays until each
   command's handler migration (7.3).
7.3. **Per-command handler migration (host, incremental)**: for each
   command in §10 order: write the real leaf/confirm handler (mutating
   `MenuShared` directly), then delete its `SlashKind` parse arm and
   bespoke helpers (`session_overlay_entries`, `tree_overlay_entries`,
   `sandbox_overlay_entries`, `filtered_value_candidates`,
   `modifier_overlay_entries`, the `parse_model_filter` merge ladder,
   `model_overlay_entries`, …). The per-command overlay arms are already
   gone with 7.1; only the parse arms and helpers remain per command.
7.4. **Final cleanup/audit**:
   - delete `SlashKind` + `slash::registry()` once no parse arms remain —
     the root list renders from the root descriptor only;
   - remove dead `engine_rows` fields, `#[allow(dead_code)]` overlay
     machinery, and the (already removed in 6.7-3) `Activation`/take-
     restore scaffolding;
   - audit: `slashdot_menu` builds without the parent crate; the host
     renders the engine's plan verbatim (§12) — no per-variant marker or
     cursor policy survives; `grep ActiveOverlay` shows only the four
     variants;
   - verify the `slash_menu.md` acceptance checklist (v1 behaviors,
     superseded row-by-row by the v2 design where they conflict) plus the
     v2 additions: cursor-row Tab/Enter, accept-clears-filter with Esc
     traversal-memory restore, bare-level Tab promote, `*`/`>` marker
     split, positional ids without `<…>`.

---

## 4. Integration approach inside picocode

- `repl.rs` keeps the *dispatch* (key events → overlay) but its per-menu
  logic is replaced by: parse buffer → interpreter call → apply result
  (`set_text` and/or render plan → paint).
- Overlay state that is currently pre-computed in `ActiveOverlay` variants
  (rows, active marker, header) moves into the interpreter's own state; the
  variants collapse to a single `ActiveOverlay::Menu(InterpreterState)`.
- Non-slash affordances untouched (bare-prompt path completion, `!cmd`,
  general Esc) — they stay in `repl.rs`.

## 5. Risks / notes

- `repl.rs` is ~9.4k lines; Phase 6 steps must stay small and individually
  shippable (one command per PR).
- The accept/clear + traversal-memory model (design §4–§5) is the subtlest
  behavior; tests must cover edit-terms-after-accept sequences, Esc replay
  chains, and scoped edit invalidation explicitly (Phase 5.5).
- `<…>` removal is a user-visible change; call it out in release notes.
- Behavior change vs. v1 contract: cursoring no longer rewrites the prompt;
  Tab/Enter commit explicitly (design §4). `slash_menu.md` (v1) rows on
  cursoring are superseded by the v2 design where they conflict.
- Editor `pending_prefill` / `$EDITOR` handoff: Phase 5 only defines the
  signal; wiring into the existing editor flow is Phase 6 (tree step) work.
