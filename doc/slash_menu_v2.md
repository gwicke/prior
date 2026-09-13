# Slash menu v2 design doc
Goals
- Regular and data driven: Replace custom key bindings with data structure
  defining the structure, leaving low-level key handling to the generic menu
  tooling
- Syntax driven, robust: Prompt fully represents menu state

V1 design & principles: slash_menu.md - v2 refines and generalizes these UX
principles

##  represent filter and selection list cursor state in path
Currently: Hidden, need special logic to return to previous list / selection
(e.g. in tree view).

Idea: 
- `/tree [search terms] id` - make `<id>` reflect the selection state
- `/tree [search terms] id ` - show sub-menu (trailing space)
- `/tree [search terms] id switch` - switch action menu selected but not shown
- `/tree [search terms] id switch ` - switch action menu shown (trailing
  space)
- `/tree [search terms] id compact llm .prompt:some long prompt` - compaction menu
  shown, llm entry selected; enter calls llm leaf handler
- `/tree [search terms] id switch direct`
- `/tree [search terms] id switch manual`
- `/tree [search terms] id switch llm .prompt:args`
Trickier:
- `/set [search terms] <key> /value` - plain value or / menu for model
  selection

## mixing of filtering and formal identifiers
- last in hierarchy level
- no spaces in ids (usually uids)

## extended filter syntax
Currently
- `path:.` -> `.path:.`

Future: model selection
- `.price:free` - sorted by popularity, or mix of pop + relevance to query
  strings
- `.price:in<0.10;out<0.6` - sorted by popularity
- `.sort:new` - new models
- `.sort:new,pop`


# Menu kinds
## Common
- tree structure: each entry can have children - implies syntactical nesting
- key, description for each entry - fuzzy search across both
- Optional modifiers
    - string key, string or number values (typed)
    - nominate some as default on tab completion (including value): llm
      compaction .prompt, session .path filter etc
    - have their own auto complete on keys (`.*`), string values on enum /
      default values
    - parsing consumes following words into value up to next dot modifier,
      valid id (positional), or end of prompt
- handlers registered on leafs

## Static / data driven only
- Top level `/`, `/set` etc - purely driven by data
- Only need handlers on leafs
  - args: positional ids from path, modifier key-values

## Dynamic data
- `/tree`, `/model`, `/set default_model`
- data fetch handler: 
    - designates dynamic id for each row (string, e.g. uuid)
        - cursoring through a listing updates id in prompt (last position)
        - when the last word in prompt is a valid ID in current listing view &
          a trailing space is present / added, then move to next level of
          hierarchy / display sub-tree menu
    - optional description string field for each entry
    - optional additional key-value fields (string key, values [string|number])
        - configure subset for display in table (list of keys)
        - pervasive filtering / sorting via `.` modifiers, corresponding to
          all fields
            - auto-complete / filter `string` values based on actual dataset
              values; optional manual override to pre-defined enum set
- fuzzy search defaults across ids, description fields
- optional leaf handler for final accept (enter with valid ID present, e.g.
  /model dialog); when the leaf handler is absent, enter behaves like tab
  (adds trailing space, moves to child menu)

---

# Audit & high-level implementation plan (conceptual)

> Scope: this section audits the *current* menu structure against the data-driven
> v2 goals above and maps every menu onto the proposed layout. It is deliberately
> **conceptual** — no concrete types/signatures yet. Findings are phrased to feed
> a lower-level design pass, not to prescribe code.
>
> Sources used: `src/slash.rs` (registry), `src/repl.rs` (`ActiveOverlay`,
> `from_buffer`, `handle_*`, overlay-entry helpers), `src/model_catalog.rs`
> (`MODEL_MODIFIERS`), and the sibling docs `slash_menu.md`, `tree.md`,
> `settings_menu.md`, `sandbox_menu.md`.

## 1. What exists today (the real shape)

The current overlay engine already manifests most of the v2 principles
(`slash_menu.md`): the **prompt is the state**, one `ActiveOverlay` variant per
menu, a uniform dispatch loop, and data-driven bits where the data was
centralized (`MODEL_MODIFIERS`, `SETTINGS`). What v2 asks for is not a rewrite
of that loop but a **generalization of the data descriptions** so the bulk of
`from_buffer` / `handle_tab` / `handle_enter` / `handle_esc` match arms stop
being hand-written per menu.

Current surface, by command:

| Command | Level chain (tokens) | Data source today | Handler-style |
| --- | --- | --- | --- |
| `/` | list | `slash::registry()` (static) | leaf per command word |
| `/model` | list → `.mod` modifiers → value / leaf | catalog + `MODEL_MODIFIERS` | Enter=leaf (SetModel); Tab=complete token |
| `/thinking` | list | hardcoded 4 levels | Enter=leaf (SetThinking) |
| `/persona` | list | `persona` + `setting_value_candidates` | Enter=leaf (SetPersona) |
| `/set` | list → value | `SETTINGS` registry + `SettingsStore` | Enter=commit then revert to `/set` |
| `/sandbox` | subcommand → list/path | hardcoded 3 subcommands + bindings + fs | Tab/Enter path completion |
| `/session` | list → action → confirm | repo `SessionMeta` | two-level accept → detail → confirm |
| `/tree` | list → action (switch/compact) → sub-menu | tree node snapshot | two-level accept → detail → sub-menu |
| `/compact` | shortcut to `/tree … compact` | derived from tree | leaf |
| `/stat`, `/replay` | info panels | derived stats / help | non-selectable / submit |
| `/quit`, `/clear`, `/new` | leaf | n/a | leaf |

## 2. Mapping onto the v2 layout

Below, each current menu is interpreted in v2 terms ("node = data entry",
"modifier = `.key:value` token", "leaf = a handler-bearing node"). The goal is a
single *menu descriptor* per command word from which every overlay behaviour
(filter, cursor, promotion, commit) is derived.

**Static / data-driven only (no per-row data fetch) — map to a static descriptor:**

- `/` command list — a flat node list (`key`, `help`); each child maps to another
  descriptor. Already close to v2's "top level purely driven by data"; only the
  *rendering* has to move off `slash::registry()`'s bespoke filtering.
- `/thinking` — enum-valued leaf (`off|low|med|high`). In v2 this is a node with
  an enum modifier/value auto-complete (exactly like `.sort:` today).
- `/persona` — a node whose value candidates come from `persona::PERSONAS` + files;
  that's the "string values from actual dataset / optional pre-defined enum"
  case in v2's dynamic-data section.
- `/set` — two-level: list of setting nodes, each node carrying a `ValueKind`
  (enum / model / persona / int / free) that *is* the modifier auto-complete
  machinery. Maps cleanly onto "string key, default value on tab-complete,
  enum value auto-complete".
- `/sandbox` — a node with enum subcommands (`add_ro|add_rw|remove`), one of which
  (`remove`) lists row-data (bindings) and one of which (`add_ro/add_rw`) needs a
  free `path` argument with fs auto-complete. The path auto-complete is a
  v2 "free-form value" with a custom completion source.

**Dynamic data (per-row fetch handler) — map to a dynamic descriptor:**

- `/model` — a dynamic listing (catalog). Already has the `.`-modifier machinery
  (`MODEL_MODIFIERS`) and an `active` marker. Its leaf handler = "select model".
  Closest to fully v2-shaped today; the residual work is making its *two*
  bespoke layers (Mod overlay + value completion) fall out of one descriptor.
- `/session` — a dynamic listing (sessions) with a **two-level accept**: the first
  Enter only completes a token (no leaf handler), the second opens the detail
  node. v2 expresses this via "no leaf handler → Enter behaves like Tab (appends
  space, moves to child menu)".
- `/session <name>` action menu — a child node whose options (resume/fork/rename/
  delete) are *static* actions applied to the selected session, plus a third-level
  `/clear`/delete confirm. In v2 the confirm is a special tree leaf whose handler
  takes positional `<id>` args.
- `/tree` — a dynamic listing (tree nodes) with a **cursoring→id-in-prompt**
  update: the selected entry's id is the trailing token (`/tree <id>`), exactly
  v2's "cursor updates id in prompt (last position)". Sub-menus switch/compact
  are static child nodes; compact's `manual|text only|llm` are enum children.
- `/compact` — a pure shortcut that resolves a target id automatically and lands
  on `/tree <id> compact <mode>`; unchanged in v2, it is just sugar over `/tree`.

## 3. Issues / gaps found in the current implementation vs v2

These are the concrete problems the v2 refactor should address. Several are
already half-there but are encoded ad hoc per menu rather than derived.

1. **Two bespoke, non-uniform "modifier" mechanisms.**
   - `/model` has `MODEL_MODIFIERS` (token + optional enum choices) driving a
     dedicated `Mod` overlay.
   - `/set` reimplements the same concept as `SettingMeta.name` + `ValueKind`
     (enum/model/persona/int/free) with its own `SettingsVal` overlay.
   Both are "`.`/key with a value and an auto-complete source", but there is no
   shared descriptor. **v2 unifies them as one `modifier` node type.**
2. **Filtering + cursor state is per-menu, not path-derived.** Several places
   force a jump when the buffer "fuzzy-matches exactly one row" or when a token
   "exactly names" an option (`/session`, `/tree`, `/sandbox`, `/set` value
   mode). These are separate special-cases, each with its own pre-selection /
   "active follows filter" logic. v2 wants a single rule: *the prompted id /
   positional token IS the selection; trailing space = drill in.*
3. **The three deep-dive exceptions (session/tree) don't fit the flat shape.**
   `Session`, `SessionAction`, `Confirm`, `Tree`, `TreeAction`, `SwitchAction`,
   `CompactSummary` are six hand-written variants doing promotion by string
   prefix matching inside `from_buffer`. v2's tree-with-children semantics
   ("each entry can have children → syntactical nesting") is exactly what these
   encode, but today the tree shape is hard-baked into six match arms.
4. **Leaf vs non-leaf isn't declared; it's inferred by listing.**
   `/model` and `/thinking` commit on Enter; `/session`/`/tree` lists only promote.
   The distinction is implicit in the `handle_enter` arm, not expressed in the
   data. v2 says: declare a `leaf handler` on a node; absent ⇒ Enter = Tab + drill.
5. **`value` tokens are positional-only today; v2 wants typed modifier args.**
   `/tree compact llm <payload>` is parsed as space-separated *positional*
   tokens, and `/set <key> <value>` too. v2's "parsing consumes following words
   into value up to next dot-modifier / valid id / end" is a more general
   grammar none of the current parsers implement uniformly (each has its own
   hand-rolled token walk).
6. **Formal ids vs search terms are disambiguated ad hoc.**
   `/tree` uses `<angle-bracketed-id>` to distinguish a target from a search
   term; `/session` uses fuzzy-uniqueness. v2 proposes ids are *last in a
   hierarchy level*, *no spaces*, and cursor-updated — which removes the need
   for the `<...>` disambiguator (`/tree <id> switch` becomes `/tree <id> switch`
   where `<id>` is positional). Flag: dropping `<...>` is a UX/back-compat
   question (see Q4).
7. **`/compact`'s auto-derived target is a hidden promotion.** It silently
   resolves `target_id` to the last user message. In v2 terms this is a node
   with a *default positional arg* filled by the handler — fine, but it should be
   a first-class "default value on tab-complete" concern, not a special overlay.
8. **Path completion is a separate overlay type (`PathComplete`) with its own
   fs-walking Tab logic.** v2's value auto-complete ("string values from actual
   dataset") covers a subset, but free filesystem paths (with `~` round-tripping)
   are a distinct completion *source*. The source model must be open so a node
   can say "values = filesystem", "values = enum", "values = catalog", "values =
   dataset-derived". The current `ValueCandidate {display,value,desc}` is a
   reasonable seed for this.
9. **Rendering special-cases (headers, table format) are not part of the data.**
   Several menus render `Table` columns (settings, sandbox remove, model) and
   informational `BG_HEADER`s (session action, tree switch/compact). v2 lists
   "optional key-value fields + configure subset for display in table" as a
   dynamic-data feature; today every table is assembled by hand.
10. **`/replay` and `/stat` mix "info panel" and "command".** They are not really
    tree menus. v2 should either model them as a node kind "info leaf" or keep
    them outside the descriptor (flag, see Q6).
11. **Non-slash affordances leak into the slash menu.** Bare-prompt path
    completion (`Tab` on empty input → `PathComplete`) and the `!cmd`/`!!cmd`,
    `Esc` semantics are adjacent, not the same system. The refactor should keep
    the v2 data-driven *slash* menus from entangling with these.

## 4. Conceptual target shape (the descriptor)

Synthesising the above, the v2 system is one recursive descriptor:

- A **menu** = a named node (the command word) with:
  - **children** (recursive; at the base level these are the command words
    themselves, which is what the existing `slash::registry()` already lists),
  - optional **data-fetch handler** (marks it dynamic; supplies row ids, desc,
    optional key-value fields, per-row default completion values),
  - optional **leaf handler** (absent ⇒ Enter = Tab + drill to child),
  - optional **modifiers** (reusable, typed): each is a `key` + value kind
    (enum / int / model / persona / dataset-derived / filesystem) with a default
    value for Tab, matching `.sort`, `/set <key>`, `/tree <id>` , `/sandbox`
    subcommands, and compact modes under one grammar,
  - an optional **table-column definition** (which key-value fields to show, in
    what order/width) — the 1:1 replacement for the hand-built `Table`s above,
  - optional **confirm semantics** (a special leaf that stages a delete/clear).

From that single descriptor, a generic interpreter derives:
- the **path/state grammar** (which comma-free tokens are ids vs positional args
  vs modifiers vs values), so filtering + cursor live in the path like v1 wants;
- the **filter rules** (search space = id + description + displayed fields;
  first match = active; exact-token = preselected);
- the **Tab / Enter / Esc contract** (Tab expands token or drills, Enter
  activates leaf or drills, Esc pops one token);
- the **header/table rendering**.

The implementation consequence (for the later design pass) is that the current
~17 `ActiveOverlay` variants and the per-menu arms in `from_buffer` /
`handle_tab` / `handle_enter` / `handle_esc` collapse into a small set of generic
interpreter states driven by descriptors — while the *edge data* (catalog,
sessions, tree, personas, settings, bindings, fs) is provided by handlers
behind the descriptor. This preserves the proven "prompt is the state" core.

## 5. Staged conceptual roadmap

1. **Phase A — descriptor core.** Define the node descriptor (`children`, fetch
   handler, leaf handler, modifiers, table def, confirm). Land the generic
   interpreter state machine (state grammar, Tab/Enter/Esc) behind the existing
   loop so nothing user-facing changes yet.
2. **Phase B — migrate static menus.** Port `/`, `/thinking`, `/persona`, `/set`
   (and the sub-menus) onto descriptors. Deletes the `Mod`/`SettingsVal`
   divergence in favour of one modifier type.
3. **Phase C — migrate dynamic-but-flat menus.** Port `/model` (reusing
   `MODEL_MODIFIERS` as modifier metadata) and `/sandbox` (adding a "filesystem
   value source").
4. **Phase D — migrate the deep trees.** Port `/session` and `/tree` (their full
   six-variant chain incl. confirm) onto the recursive child model; replace the
   `<angle-bracket-id>` disambiguator with positional-id-in-path.
5. **Phase E — collapse the diff.** As each menu migrates, delete its bespoke
   overlay variants and `from_buffer`/handler arms; keep edge-data handlers and
   the headless/render helpers. Then flatten the remaining ad hoc promotion
   logic (fuzzy-unique jumps, exact-token preselection) into the generic
   interpreter.

## 6. Clarifying questions before the design pass

I want these resolved because each materially changes the descriptor shape:

- **Q1 — Scope of the grammar.** The v2 doc's "trailing space = show sub-menu"
  and "`<id> switch direct`" imply a fairly strict, well-defined token grammar.
  Should the new path grammar be **retro-fitted to existing commands** (accept
  today's aliases: bare `/tree`, `/sandbox add_ro ...`, `--summarize`), or are we
  free to *redefine* the surface and migrate? (I lean: keep existing surface as
  the compatibility baseline, extend the grammar underneath.)
- **Q2 — The `<id>` disambiguator.** `/tree` currently requires angle brackets to
  separate an id from a search term. v2's "id is last-in-level, cursor-updated"
  removes the need. **May we drop `<...>` and rely on positional-id**, or must the
  parser keep accepting both for compat? This directly affects Q1 and Phase D.
- **Q3 — One modifier grammar across `/model`, `/set`, `/tree`, `/sandbox`?**
  `/model` modifiers are dot-prefixed (`.sort:`); `/set` *keys* are bare words
  (`/set default_model`); `/tree compact` modes are bare words; `/sandbox`
  subcommands are bare words. Should the unified "modifier" **use dotted syntax
  everywhere** (`.sort:` → `.sort:`), or stay **bare-word per command**? The doc
  is ambiguous (it shows both `.price:` and `/set <key>`). This determines the
  parser.
- **Q4 — Are `/stat` and `/replay` in scope?** They're info panels / shortcuts,
  not tree menus. Should they become a new "info leaf" node kind, or be left out
  of the descriptor and kept as an escape hatch? I'd keep them out of the core
  descriptor (Q#, issue #10) unless you want uniform discoverability.
- **Q5 — Confirm dialogs.** Should delete/clear confirmation be a first-class
  "confirm" node property (recommended, one interpreter path), or stay a special
  overlay? This affects Phase D the most, since `/session … delete` is the only
  current three-level flow.
- **Q6 — Table rendering in the descriptor.** Are the hand-built `Table`
  configurations (settings 3-col, sandbox remove 4-col, model) worth generalising
  into a declarative table def now, or is that polish to defer behind the core
  interpreter? Medium-size driver of Phase A scope.
- **Q7 — Dataset-derived value auto-complete.** The doc says string modifier
  values may auto-complete "based on actual dataset values". Should that be
  **on by default for dynamic menus** (potentially noisy/expensive) or opt-in
  per modifier with pre-defined enums preferred? Relevant to Phase C.
- **Q8 — Back-compat of the `esc`/`Tab` contract.** v2's uniform "Tab expands,
  Enter activates, Esc pops one token" already *mostly* matches slash_menu.md's
  rules. Are we willing to **retire* any* residual ad-hoc Tab behaviours**
  (e.g. `/sandbox`'s on-Tab fs-walker, bare-prompt `PathComplete`) in favour of
  "values = a filesystem source" on a node? This touches non-slash affordances
  (issue #11).

