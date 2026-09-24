# Final cleanup — remove v1.0 / v2.0 menu remnants (impl step 10)

Status: **PROPOSED** (plan only; not yet implemented)

## Goal & principles

- Delete the v1.0 menu system (descriptor-tree `Interpreter` /
  `AcceptController` / `PlanBuilder` / `MenuNode` world) and every
  remaining v2.0 phase-6/7 bridge remnant (`MenuEngine` machinery,
  traversal frames, legacy `ActiveOverlay` variants, `slash.rs`
  registry).
- The help menu was **removed on purpose**: no re-porting, no
  remnants anywhere (`ActiveOverlay::Help`, `render_help`,
  `SlashKind::Help`, `/help` registry entry, `help_view` threading).
- For every still-needed bit that currently lives in legacy code:
  **first relocate it to its v2.5 home, then delete the legacy
  module** — never keep a module alive because one method is
  still called.

## Verified current state (basis of this plan)

- The live frame derivation (`repl run` loop, ~line 2048) only ever
  produces `V25Menu | PathComplete | None`. All `/` buffers route to
  the v2.5 handle.
- `from_buffer` is `#[cfg(test)]`-only; `from_buffer_cursors` is
  reachable only from it — the whole word-dispatch world
  (`"model" | "thinking" | "set" | "tree" | …` arms) is test-only.
- `Interpreter::field_options` is **already ported**:
  `node_entry::default_complete` (v2.5 core, has its own
  `completion_*` test suite). Nothing to relocate for modifiers.
- `v25_*` nodes do not use bridge v1 machinery — only `MenuShared`
  (cells), `AsyncOp`/`CompactMode`/`SandboxOp`, `LiveState`,
  `ColumnDef`, `value::Value`.
- `slash.rs`: `registry()`, `filter_commands`, `parse_slash`,
  `SlashIntent` have **zero live consumers**; `render_help` is only
  referenced from dead `Help` arms/tests.
- `InputAction::SetModel/SetThinking/SetPersona` producers exist only
  in dying `handle_enter` arms; v2.5 writes `LiveState` directly +
  `menu_submitted`.
- Prompt-line Tab still uses the old completer
  (`complete_path_prefix_sandbox`), while `src/path_complete.rs`
  (structured entries, listing cache + invalidation) backs
  `/sandbox` and tool paths — the prompt-side switch is **scoped OUT
  of this plan**: it is standalone repl code (not menu v1/v2.0) and
  becomes its own step; design findings in
  `arg_and_modifier_autocomplete.md` §Prompt-line completion.

## Still-needed bits → new homes

| Bit (today, in legacy code) | New home | Notes |
|---|---|---|

| Dotted-modifier completion (`Interpreter::field_options`, repl `Mod` overlay) | **already relocated** → `node_entry::default_complete` | Just delete. Gate: existing `completion_*` tests + one eval-level `/model .` test in `v25_model.rs`. |
| `/stat` interstitial D4 (`stat_phase`, `Stat` overlay) | **already relocated** → `v25_stat::StatNode::header` | Panel shows on drill (v2.5 leaf pattern, same as `/replay`), Enter closes. Accepted UX alignment; `stat_phase` dies. |
| Help | **none — intentionally removed** | Delete all remnants (see inventory D/E). |
| `ColumnDef` (+ builder) in `provider.rs` | move to `crates/slashdot_menu/src/fields.rs`; lib re-export `provider::ColumnDef` → `fields::ColumnDef` | Core `view.rs`/`node_impl.rs`/`menu.rs`/`node_entry.rs` + all `v25_*` use it. This is the **only** provider.rs survivor; the rest of the file dies. |
| Host wiring: `MenuShared`, `LiveState`, `AsyncOp`/`CompactMode`/`SandboxOp` | stay in `src/slash_menu_bridge.rs` (slimmed; header rewritten) | This is v2.5 host-state, not legacy — it survives with `MenuEngine` gone. |
| `repl::OverlayAction {Continue, Action}` | stays | It is `handle_enter`'s return for the live arms (`None` → plain submit, `PathComplete` apply). Distinct from the **crate** `interpreter::OverlayAction`, which dies. |
| `engine.on_edit` invalidation call (edit arm) | **nothing needed** | v2.5 `eval_prompt` re-parses the full buffer per keypress; no traversal memory to invalidate. Delete call + method. |

## Deletion inventory

### A. `src/repl.rs` (largest)

- **`ActiveOverlay`**: remove 10 variants — `Help`, `Mod`,
  `Settings`, `SettingsVal`, `Stat`, `Menu`, `Tree`, `TreeAction`,
  `SwitchAction`, `CompactSummary`. Keep `None`, `V25Menu`,
  `PathComplete`.
- **Derivation world**: `from_buffer` (cfg-test), `from_buffer_cursors`
  + all word-dispatch arms, `bare_listing_word`, `root_list_overlay`,
  `tree_overlay_entries` + tree flatten/parse helpers used only by it
  (verify no `Repl::` calls from `v25_tree` first), `parse_settings`,
  `filtered_value_candidates`, `render_help_overlay`.
- **Overlay methods**: drop dead arms of `display_rows`, `title`,
  `overlay_disc`, `cursor_row`, `filter_for`, `handle_tab`,
  `handle_enter`, `handle_esc`; delete `engine_esc` +
  `is_engine_backed` + their call sites (engine ladder was Menu-only).
  `handle_enter` keeps: `PathComplete` apply/submit arm + `_` → plain
  text submit; prune params `help_view`, `value_error`, `ctx`,
  `engine`, `settings`, `dirty` as the compiler allows.
- **run() locals/params**: `engine` construction
  (`MenuEngine::new`), `engine.on_edit` call, `stat_phase`,
  `help_view`, `value_error` + all threading (incl. test harnesses at
  ~7787/7858/9513).
- **cursor policy**: `cursor_sel_policy` loses
  `is_tree_list`/`follows_active` (constants → false; PathComplete
  sel-tracking stays).
- **Prompt-completion island stays untouched**: `complete_path_prefix*`,
  `expand_tilde` wrapper, `token_at_cursor`, `completion_pos` /
  `path_completions`, `ActiveOverlay::PathComplete` + its arms — live
  code owned by the separate prompt-completion port step (see
  `arg_and_modifier_autocomplete.md` §Prompt-line completion).
- **Tests**: all `from_buffer`-era overlay tests die (tree / compact /
  switch / model / persona / thinking / settings / stat / mod / help
  family). Port `fuzz_path_completion_preserves_layout` (8832).

### B. `src/slash_menu_bridge.rs`

- Delete: `Frame`, `level_of`, `resolve_node`, the whole
  `MenuEngine` struct + impl (`new`, `root_descriptor`,
  `register_handlers`, `render_plan`, `step_at`, `on_edit`,
  `field_options` wrappers, …), providers (`PersonaActiveProvider`,
  `ThinkingActiveProvider`, `ReplayDetailProvider`, `ModelProvider`,
  `detail_providers` map wiring), and the 18 step_at-era tests.
- Keep: `MenuShared` (+impl), `LiveState`, `AsyncOp`,
  `CompactMode`, `SandboxOp`.
- Rewrite module doc header (no more "→ interpreter / design §10").

### C. `crates/slashdot_menu`

- Delete files: `interpreter.rs` (1497), `accept.rs` (588),
  `node.rs` (117), `provider.rs` (176 — after `ColumnDef` move;
  `FieldMeta`, `StaticProvider`, `str_field`/`num_field`,
  `provider::DataResult`, the three traits have no survivors).
- `render.rs`: delete `PlanBuilder` + interpreter-coupled tests; keep
  `RenderPlan` / `LogicalLine` / `Marker` / `paint` (v2.5 output
  type).
- Keep: `value.rs` (`fields.rs` + `v25_set/session/model/tree/sandbox`
  use `Value`), `prompt.rs` / `grammar.rs` / `filter.rs` (v2.5 core),
  all of `node_entry.rs` (core v2.5 — its `DynamicNode` hits are a
  test mock).
- `lib.rs`: drop `mod` decls, repoint `ColumnDef` re-export, rewrite
  crate doc header (link v2.5 design; drop "generic overlay
  interpreter" phrasing).

### D. `src/slash.rs`

- Delete the file: `registry()`, `SlashKind`, `SlashCommand`,
  `filter_commands`, `parse_slash`, `SlashIntent`, `render_help` +
  tests — zero live consumers; `/help` intentionally gone; command
  metadata lives in the v2.5 rows (`StaticRow::new(id, desc)`).

### E. `src/agent.rs` / `InputAction`

- Remove `InputAction::SetModel/SetThinking/SetPersona` (producers
  only in dying arms; v2.5 sets `LiveState` + `menu_submitted`) and
  their agent match arms (~4077/4100/4126) + stale comments
  (`model_overlay_entries` @2516, `InputAction::SetModel` @2665).
- Audit every remaining `InputAction` variant by producer grep; delete
  orphans, keep `Quit`/`Clear`/`New`/`Submit`/`MenuDone`-family only
  where a live producer exists.

## Execution order (each phase: green build + tests, doc touch, commit)

1. **P1 — repl.rs pruning**: dead variants/arms/helpers/params + their
   tests. Gate: `cargo test`; sentinel grep for removed variant names
   in non-test code is empty.
2. **P2 — crate surgery**: `ColumnDef` → `fields.rs`; delete
   `interpreter`/`accept`/`node`/`provider`; `PlanBuilder`; `lib.rs`.
   Gate: lib tests green (expect a large drop: interpreter 25 +
   accept 8 + render ≤6).
3. **P3 — bridge slimming**: `MenuEngine`, `Frame`, providers out;
   repl has zero engine refs after P1. Gate: workspace green.
4. **P4 — slash.rs + help remnants + `InputAction` audit + agent
   comments.** Gate: sentinels.
5. **P5 — test gap-fill + docs**: add `v25_thinking` tests (set +
   active marker + root row), `v25_persona` tests (set + root row),
   `v25_menu` root-listing test (all commands present, `help`
   **absent** — codifies the decision), `v25_model` `/model .`
   modifier eval test; update this doc's status, impl doc step 10 →
   DONE (incl. re-pointing the "in-prompt Tab switch" note to the
   prompt-completion step), design-doc cleanup note; final
   full-workspace run + sentinel block; commit.

## Test expectations

- Deleted: ~60–80 tests (repl from_buffer-era family, bridge 18,
  interpreter 25, accept 8, render PlanBuilder ≤6, slash 9).
- Added: ~8–12 targeted tests (P5 gap-fill list).
- Workspace green at **every** phase boundary (baseline: 653 + 169).

## Sentinel greps (must be empty after P5)

`MenuEngine`, `PlanBuilder`, `AcceptController`, `Interpreter`,
`root_descriptor`, `SlashKind`, `render_help`, `filter_commands`,
`parse_slash`, `from_buffer_cursors`, `ActiveOverlay::{Help, Mod,
Settings, SettingsVal, Stat, Menu, Tree, TreeAction, SwitchAction,
CompactSummary}`, `field_options`, `help_view`, `stat_phase`,
`engine_esc`, `is_engine_backed`, `on_edit` (bridge), `value_error`
(repl). NOT in these sentinels (live until the prompt-completion
step): `complete_path_prefix*`, `token_at_cursor`, `PathComplete`,
`completion_pos`.

## Risks / verify-at-implementation

- Agent `menu_submitted` LiveState sync (@~3706) must cover
  model/thinking/persona before `Set*` variants drop (compile + tests
  should catch; confirm semantics manually).
- Direct `Repl::` helper calls from `v25_tree`/`v25_set` (compile
  will flag; move anything shared instead of deleting).
- `complete_path_prefix_impl` is `pub` — confirm no external callers.
- `SettingsStore::overlay_rows` users after the Settings arm removal
  (v25_set may use it).
- `path_complete` prompt parity vs old completer: `~` round-trip,
  trailing `/` on directories, sandbox fallback, empty-prefix no-op.
- Full `InputAction` producer audit (P5).
- `overlay_disc` numbering: keep as-is or renumber cleanly; check no
  test asserts discriminants.

## Out of scope

- **Prompt-line Tab completion port** to the engine completion frame
  — its own step; design findings in `arg_and_modifier_autocomplete.md`
  §Prompt-line completion. The legacy island
  (`complete_path_prefix*`, `token_at_cursor`, `PathComplete` overlay,
  `completion_pos`) survives cleanup untouched. (Cleanup's earlier P2
  `complete_prompt` shim was dropped as throwaway.)
- Step 11 re-merge with mainline.
- §5a state/async follow-up; confirm-dialog rendering refinement
  (design §table) — v2.5 feature work, not cleanup.
- Arg/modifier design rework (flat-args reshape in
  `arg_and_modifier_autocomplete.md`) — separate track.
