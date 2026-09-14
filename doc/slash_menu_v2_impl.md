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
  no editing operations leak into the crate.

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
  contract (design §5), pinned-id/view rule (§4), exact/unique-match
  preselection, value-typed children (§2).
- **Tests**: static-level scenarios only — `/thinking`-shaped enum child,
  drill/promote, Esc pop, pinned-id precedence.

### Phase 4 — dynamic levels + rendering

- Wire `DataProvider` into the interpreter: row candidate sets, cursor↔id
  rewriting, preprocessing pass (distinct string values per field, D7
  dataset-derived completion).
- `render.rs`: render plan — title, `child_header`, per-row derived header
  (R8-4), table from `ColumnDef`, markers, vertical budget (frame.rs).
- **Tests**: dynamic scenarios (fake provider), header composition, table
  output snapshot vs. current menus' shape.

### Phase 5 — accept flows

- Leaf invocation, interstitial, confirm (stateful instance), editor handoff
  signal, "return new path" rewrites (design §9).
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
   roadmap — do it incrementally per step, not at the end).

- **Verify** per step: existing UX behaviours from `slash_menu.md` keep
  passing (the v1 doc doubles as an acceptance checklist), plus new v2
  behaviors (Tab-preserves-filter, positional ids without `<…>`).

### Phase 7 — cleanup

- Remove `ActiveOverlay` variants that no longer have consumers; collapse
  `from_buffer`/`handle_tab`/`handle_enter`/`handle_esc` to interpreter
  delegation; drop `slash::registry()` bespoke filtering (root list renders
  from the root descriptor).
- Final audit: no picocode types leak into `slashdot_menu` (compile-time
  check: the crate must build without the parent in the workspace).

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
- The pinned-id/view rule (§4) is the subtlest behavior; Phase 3 tests should
  cover edit-terms-after-Tab sequences explicitly.
- `<…>` removal is a user-visible change; call it out in release notes.
- Editor `pending_prefill` / `$EDITOR` handoff: Phase 5 only defines the
  signal; wiring into the existing editor flow is Phase 6 (tree step) work.
