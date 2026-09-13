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
  - optional single-line **description string** of the node / command itself,
    shown and fuzzy searched in parent menu
  - optional single-line **child menu description** - shown as header when
    child menu is active
  - optional **data-fetch handler** (marks it dynamic; supplies row ids, desc,
    optional key-value fields, per-row default completion values),
    can optionally take modifier inputs for dynamic backend filtering (e.g.
    fs),
  - optional **leaf handler** (absent ⇒ Enter = Tab + drill to child),
  - optional **modifiers** (reusable, typed): each is a `key` + value kind
    (enum / int / model / persona / dataset-derived / filesystem) with a default
    value for Tab, matching `.sort`, `/set <key>`, `/tree <id>` , `/sandbox`
    subcommands, and compact modes under one grammar,
  - an optional **table-column definition** (which key-value fields to show, in
    what order/width) — the 1:1 replacement for the hand-built `Table`s above,
  - optional **confirm semantics** (a special leaf that stages a delete/clear).
  - optional **interstitial**, static string or a dynamic handler - activated
    / shown when pressing enter, before any handler calls or attempt at
    drilling.

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
  - It is okay to make minor adjustments in the layout to conform to a uniform
    grammar.
- **Q2 — The `<id>` disambiguator.** `/tree` currently requires angle brackets to
  separate an id from a search term. v2's "id is last-in-level, cursor-updated"
  removes the need. **May we drop `<...>` and rely on positional-id**, or must the
  parser keep accepting both for compat? This directly affects Q1 and Phase D.
  - I think dropping `<>` is fine as long as the ID validity check is cheap -
    which it should be for an in-memory / cached list. Currently, only some
    contexts use angle brackets (model and session do not, tree does) - so
    this makes this more uniform.
    Also, currently the /model dialog tab-complete *replaces* the fuzzy filter
    input with a full ID. In the new scheme, it would *add* or replace the ID,
    leaving the fuzzy filter in place.
- **Q3 — One modifier grammar across `/model`, `/set`, `/tree`, `/sandbox`?**
  `/model` modifiers are dot-prefixed (`.sort:`); `/set` *keys* are bare words
  (`/set default_model`); `/tree compact` modes are bare words; `/sandbox`
  subcommands are bare words. Should the unified "modifier" **use dotted syntax
  everywhere** (`.sort:` → `.sort:`), or stay **bare-word per command**? The doc
  is ambiguous (it shows both `.price:` and `/set <key>`). This determines the
  parser.
    - general layout per level: 
        - static menu: `command [optional fuzzy search terms not exactly matching child command][optional dot modifiers]maybeChildCommand`
        - dynamic menu: `command [optional fuzzy search terms not exactly matching ID][optional dot modifiers]maybeID`
- **Q4 — Are `/stat` and `/replay` in scope?** They're info panels / shortcuts,
  not tree menus. Should they become a new "info leaf" node kind, or be left out
  of the descriptor and kept as an escape hatch? I'd keep them out of the core
  descriptor (Q#, issue #10) unless you want uniform discoverability.
    - Good question. I added the idea of an optional "interstitial" string or
      -handler in the concept above. `/stat`+enter should show that
      interstitial, second enter dismisses (no leaf handler). `/replay` would
      define both interstitial and leaf handler doing the actual replay.
- **Q5 — Confirm dialogs.** Should delete/clear confirmation be a first-class
  "confirm" node property (recommended, one interpreter path), or stay a special
  overlay? This affects Phase D the most, since `/session … delete` is the only
  current three-level flow.
    - node property seems cleanest, with a single handler instance called for
      the initial prep work (e.g. compaction prep), as well as the final
      accept / abort decision. single instance since there is state to be
      kept. this might be the only stateful situation as well; the same prompt
      would re-start the *prep*.
- **Q6 — Table rendering in the descriptor.** Are the hand-built `Table`
  configurations (settings 3-col, sandbox remove 4-col, model) worth generalising
  into a declarative table def now, or is that polish to defer behind the core
  interpreter? Medium-size driver of Phase A scope.
    - How much effort is generalizing here? Is there much customization around
      wrapping / ellipsis handling / formatting that could be lost or would
      add complexity in a generalized version?
- **Q7 — Dataset-derived value auto-complete.** The doc says string modifier
  values may auto-complete "based on actual dataset values". Should that be
  **on by default for dynamic menus** (potentially noisy/expensive) or opt-in
  per modifier with pre-defined enums preferred? Relevant to Phase C.
    - Sure, can make it opt-in; but the general extraction of a string set
      from a static / cached list would be a single generic pass anyway,
      right?
- **Q8 — Back-compat of the `esc`/`Tab` contract.** v2's uniform "Tab expands,
  Enter activates, Esc pops one token" already *mostly* matches slash_menu.md's
  rules. Are we willing to **retire* any* residual ad-hoc Tab behaviours**
  (e.g. `/sandbox`'s on-Tab fs-walker, bare-prompt `PathComplete`) in favour of
  "values = a filesystem source" on a node? This touches non-slash affordances
  (issue #11).
    - How about making that an expansion of the dot modifier value complete.
      Support registering a dynamic completion handler that takes a query
      string, returns candidates, and leaves the fine filtering and tab
      handling within a level to the menu by returning identical cached
      responses unless the level changes (trailing slash)?
      `/sandbox add .ro:<path> .rw:<other path>`
      `/sandbox remove` -> dynamic value menu, ids=paths

---

## 7. Decisions (from the inline review)

Consolidates the annotated answers into concrete rules the design pass should
honour. Kept conceptual; where a rule touches an earlier issue, it cites it.

- **D1 — Uniform, per-level grammar.** The path grammar is recursive *per
  level*, not per menu: `command [fuzzy terms not exactly matching the level's
  selection token] [dot modifiers] <selection>` where `<selection>` is either a
  *child command* (static level) or an *id* (dynamic level). Drilling enters the
  selection's own level where the rule repeats (`/tree <id>` → action level →
  `switch` / `compact` → ...). Minor layout adjustments to existing commands are
  acceptable to conform (Q1).
- **D2 — Drop `<...>`.** Ids are positional, last-in-level, cursor-updated;
  validity is a cheap lookup against the cached/in-memory rows (Q2). `/model`,
  `/session`, `/tree` all adopt the same rule. Fuzzy terms are **not replaced**
  when an id is appended — Tab *adds* the id, leaving the search terms in the
  prompt (Q2 note, see §R8-5).
- **D3 — One modifier mechanism.** Dot-modifiers are the uniform value/argument
  carrier at every level. Statics still name their children with bare words
  (`/set default_model`, `/tree <id> compact`); modes that today contain a space
  must become single tokens (see §R8-1). This collapses the `Mod` / `SettingsVal`
  divergence (issue #1).
- **D4 — Interstitial covers info + replay.** Enter on a node may first show an
  *interstitial* (static text or a handler) before any leaf call. `/stat` = info
  interstitial with no leaf handler (second Enter dismisses); `/replay` =
  interstitial **and** a leaf handler that performs the replay (Q4). Resolves
  issue #10 without an escape hatch.
- **D5 — Confirm is a node property, and stateful.** A confirm is a special
  leaf whose **single handler instance** performs the prep work (e.g. compaction
  precompute) and then the accept/abort decision. Re-entering the same prompt
  re-runs the prep. It is expected to be the *only* stateful accept path (issue
  #3, Q5). Compare §R8-3 for the interstitial relationship.
- **D6 — Generalise the table now (low cost).** Reading `src/table.rs`, the
  builder already exposes exactly the knobs a declarative table def needs:
  `headers`, `aligns`, `min_widths` (prio), `gap`, `ellipsis`, plus a
  *wrap-vs-ellipsis* mode (`render` vs `render_multiline`). Nothing subtle is
  lost in deduplicating settings/sandbox/model tables behind a small
  `columndef` (Q6). Include it in Phase A; it is a 10-line descriptor, not a
  risk.
- **D7 — Dataset string values opt-in.** `string`-typed modifier values may
  extract their candidate set in a single generic pass over the cached rows
  (choice values = distinct field strings). Keep it **opt-in per modifier**
  (default = pre-defined enum) and reserve on-demand extraction for non-resident
  / back-end sources (Q7).
- **D8 — Completion as a registered handler.** Path/fs/on-demand value
  completion is an *expansion of dot-modifier value completion*, not a separate
  overlay (issue #8, Q8): a dynamic completion handler takes a query string,
  returns candidates; the interpreter caches identical responses until a
  level-changing input is seen (`trailing space` for menu levels, `trailing
  slash` for fs descent — see §R8-2), so fine-filtering and tab stay in the
  interpreter. `/sandbox add .ro:<path> .rw:<other>` and `/sandbox remove` →
  dynamic value menu (ids = paths) fall out of this.

## 8. Remaining issues / contradictions to resolve in the design pass

These survived the review and are not yet answered in the doc. Each is a real
consequence of a decision above, and should be settled before lower-level work.

- **R8-1 — Child-command tokens must be space-free.** The open layout is
  whitespace-separated, and D2 forbids spaces in ids. But today `/tree <id>
  compact text only` uses a *two-word* mode label (`text only`), normalized to
  `text-only` only for pre-seletion. Under a single-token-per-level grammar this  is ambiguous. Decision needed: canonicalize modes to a single hyphenated token
  (`text-only`) as the committed form and keep a spaced display label only if
  desired. Same audit applies to any other spaced option label.
- **R8-2 — Trailing-space (menu drill) vs trailing-slash (fs descent).** v2
  uses *trailing space* to promote a level; D8's fs value-complete uses
  *trailing slash* to descend a directory. These are two different
  level-changing signals, and both sit inside (or at the end of) a value
  position. Decision needed: either define the trigger precisely per value-kind
  (`/` for a filesystem value, space otherwise), or adopt v2's core rule
  wholesale — "a value consumes words until the next dot-modifier / valid id /
  end of prompt" — so the *terminator* is structural, not a whitespace peek.
- **R8-3 — Three overlapping "paused accept" flows.** The doc now has (a) the
  informational interstitial (D4), (b) the stateful confirm (D5), and (c) the
  pre-existing *review-after-commit* flow (compaction summary shown in `$EDITOR`,
  `pending_prefill` re-entry on cancel). All three pause before a final act.
  Decision needed: model them as **one** "advance/paused" `interstitial`
  concept with an optional **stateful** handler, where (a) has no handler, (b)
  is a confirm-with-accept/abort handler, and (c) is an external-editor variant  that re-enters via `pending_prefill`. Otherwise the interpreter three
  special-cases a single concept.
- **R8-4 — Header depth.** The single-line `child menu description` (new field)
  covers a static header, but the rich multi-line `BG_HEADER` rows (session
  stats, compactation drop/token counts, entry preview) are dynamic and built
  per menu today. Decision needed: split headers into (i) a static single-line
  menu header string and (ii) a per-row header derived from the same
  key-value fields + table columndef that drives the list. That merges issue #9
  with the new descriptor field.
- **R8-5 — Pinned id vs `active follows filter`.** Once a valid trailing id is
  in the prompt (D2), the `active`/cursor must be **that id**, not the first
  fuzzy match; re-editing the fuzzy terms must not silently move selection off
  the pinned id. State the precedence (pinned id wins) or the "cursor lives in
  the path" guarantee breaks the moment a user touches the terms after Tab.
- **R8-6 — Back-end vs front-end modifier filtering boundary.** D8 lets a
  data-fetch handler take modifier inputs (back-end filtering over non-resident
  data, e.g. fs). But `/model` applies its modifiers in-memory today
  (`apply_modifier`). Decision needed: mark each modifier as *back-end* (passed
  through to the fetch handler) vs *front-end* (applied in the interpreter over
  cached rows); `MODEL_MODIFIERS` metadata is the seed for this split.
- **R8-7 — Value tokens that legitimately contain words.** v2 core already
  says a modifier value "consumes following words ... up to the next dot
  modifier / valid id / end of prompt" (.prompt: for compact). D1/D3 say the
  *selection* token must be space-free, but a *value after a modifier* may be
  multi-word. Confirm this distinction (space-free selection vs word-consuming
  value) is explicit, or multi-word values collide with R8-1.
- **R8-8 — `/compact` default target vs default-on-tab.** `/compact` is a
  shortcut that synthesizes `/tree <last-user-id> compact <mode>`. With
  positional ids (D2) it stays sugar, but its silently-derived target must be
  the same "default value on tab-complete" mechanism used elsewhere, not a
  hidden resolve (issue #7). Confirm it reuses D8's default-value slot.

