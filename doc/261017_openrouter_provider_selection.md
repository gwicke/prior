# OpenRouter provider selection via per-model `routing` setting

Date: 2026-10-17. Status: implemented (2026-10-17). Supersedes the `@only:` model-id suffix design
sketched in `doc/todo.md` (kept as an evaluated alternative, §9).

## Goal

Let the user pin OpenRouter's upstream provider by editing the model's per-model settings (Ctrl-E in
the `/model` detail view), as an optional `routing` key mirroring OpenRouter's `ProviderPreferences`
structure:

```toml
[models."openrouter:anthropic/claude-sonnet-4"]
routing = { only = ["deepinfra/fp8/flex"], allow_fallbacks = false }
```

which reaches the wire as part of `additional_params`:

```json
"provider": {
  "only": ["deepinfra/fp8/flex"],
  "allow_fallbacks": false
}
```

## Why this shape (vs the `@only:` suffix)

The suffix design required: a general parse of `@…` threaded through ~10 model-id split sites, a
full-id-vs-base-id identity split (storage keeps the suffix, lookups strip it), a menu-engine change
so typed suffixed words match rows, and dual forms in session storage. The settings approach needs
none of that: model ids, the catalog, the menu engine, session storage and the resume prefill are
all untouched. The per-model settings layer already owns "facts about this model" and already has
the seeding, live-edit and round-trip machinery this needs.

Trade-off accepted: routing is not settable from the model *listing* (no quick per-row pick, no
ad-hoc per-turn selection). It is durable per-model configuration, edited where all the other
per-model facts are edited. If ad-hoc selection ever becomes a goal, the suffix design (§9) remains
the better shape for that and can be layered on later.

## Facts established (investigation)

- rig fork (`gwicke/rig @ 1c2f4405`) already supports everything needed:
  `openrouter::ProviderPreferences` has `only`, `order`, `ignore`, `allow_fallbacks`, `sort`, … and
  `to_json()` returns `{"provider": {…}}`. No rig change required. We mirror a *subset* in our own
  typed struct (§1) rather than depending on rig types in the settings layer.
- picocode merges OpenRouter preferences in `agent/build.rs::merge_openrouter_params`, which runs on
  both the build path and the per-turn override path (`request_additional_params`) — routing
  carried in `AgentConfig` therefore reaches both paths automatically, preserving the baked ==
  per-turn parity invariant that `build.rs` tests assert. It used to pin
  `order: ["anthropic"], allow_fallbacks: true` unconditionally; that built-in pin is **removed**
  by this design (§2): nothing is sent unless the model's record asks.
- Per-model records already flow: stored in `[models."…"]` (quoted keys, `deny_unknown_fields`, loud
  per-key validation), seeded at activation and startup (`activate_model_params`,
  interactive.rs:306), and live-applied to the running agent when the *active* model's record is
  edited (`AppState::update_settings` already syncs `config.context_limit` from the record just
  written; app_state.rs:501-510).
- The rebuild gate (`plan_rebuild` fingerprint) compares `qualified_model_id` / params / persona /
  context_limit; a new compared field makes a routing edit trigger exactly one rebuild.
- `ModelSettings` records are flat scalars/lists rendered and re-parsed by hand (`parse_fragment` /
  `render_fragment` / `value_of`). An inline-table value flows through all of them unchanged (it is
  a `Value`, not a sub-table — `coerce` only rejects real tables today, and the fragment rebuild in
  `read_from_doc` copies values verbatim). Multi-line indented inline tables are additionally fine:
  toml_edit is TOML 1.1 (see §1).

## 1. The `routing` setting

`model_settings.rs`:

```rust
/// OpenRouter `ProviderPreferences`, mirrored. Field names match OpenRouter's
/// API so its docs translate directly. Only `openrouter` consumes this today;
/// other providers ignore it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    pub only: Option<Vec<String>>,
    pub order: Option<Vec<String>>,
    pub ignore: Option<Vec<String>>,
    pub allow_fallbacks: Option<bool>,
    pub require_parameters: Option<bool>,
}
```

- Stored as an **inline table**, rendered multi-line indented for readability:

  ```toml
  routing = {
    only = ["deepinfra/fp8/flex"],
    allow_fallbacks = false,
  }
  ```

  Multi-line inline tables are legal TOML 1.1 (ratified; toml.io lists v1.1.0) and our
  `toml_edit 0.25.15+spec-1.1.0` handles them — verified empirically: the fragment parser accepts
  and round-trips the indented form, `line_of_key` finds the `routing = {` line, and
  `write_to_doc`'s value re-parse accepts the rendered block. Cost over the single-line form: the
  `coerce` case walks the inline table's key/value pairs and the renderer emits the indented block
  (~30-50 lines total). Comments above `routing = {` survive via the existing key-decor machinery;
  comments *inside* the table are regenerated away on save, same class as value-suffix comments
  today. A nested `[models."id".routing]` header would be the TOML-1.0-legal alternative for
  external-tool compatibility, but needs table-vs-value branching in
  `parse_fragment`/`apply_one`, header line scanning, and an `Item::Table` build path in
  `write_to_doc` — not worth it while picocode is the file's only consumer.
- `deny_unknown_fields` + the existing key-vocabulary machinery gives loud typo errors inside the
  table for free (a nested suggestion needs a small extension to `suggest_key` — `"routing.only"`
  style keys, or a dedicated message naming the accepted routing keys).
- Wire vocabulary (v1): `only`, `order`, `ignore`, `allow_fallbacks`, `require_parameters`. The rest
  of OpenRouter's preferences (sort, zdr, quantizations, max_price, …) are deliberately out — they
  are expressible later without breaking anything, and each needs its own coercion case.
- `ModelSettings` gains `routing: Option<Routing>`; `KEYS`, `key_hint`, `coerce`, `apply_one`,
  `value_of`, `is_empty` and the fragment renderer all grow the one case. `coerce` accepts an inline
  table whose keys are all known routing keys, values being string arrays / bools.
- **Not** in `apply_to_entry` — routing is a params-half fact (it shapes requests), not catalog
  metadata. Accessor: `ModelSettings::routing()`.

## 2. Transport to the wire

- `AgentConfig` gains `routing: Option<crate::model_settings::Routing>` (typed, not JSON — testable
  and the merge step serializes once).
- **Seeding**: `activate_model_params` reads the stored record already for params; it now also
  copies `record.routing` into the config. Startup goes through the same path (interactive.rs:306).
  Routing has no per-turn override and no memory map — it is stored settings, so unlike params it is
  read fresh from the record at every activation. (`model_param_map` exists only because
  `/reasoning` mutates live params mid-session; routing never does.)
- **Live edit**: `AppState::update_settings`, after writing the record, sets `st.config.routing`
  when the edited id is the active model — the same precedent as `context_limit` there today.
  `plan_rebuild`'s fingerprint gains routing, so the edit triggers exactly one rebuild. Editing an
  inactive model's routing just sits in the record until activation.
- **Wire**: `request_additional_params(provider, model, params, routing)` gains the routing
  argument; both call sites (bake in `create_agent`, recompute on override turns) pass
  `config.routing`, so parity holds unchanged. `merge_openrouter_params` becomes:

  - `routing` present → serialize it under the `provider` key, verbatim.
  - `routing` absent (or an empty table) → **`provider` is not sent at all**. The old built-in
    anthropic pin is dropped entirely (decision after review): a hidden default that narrows where
    requests may go is worse than leaving routing to OpenRouter. A model that wants the old
    behavior says so per model, e.g.
    `routing = { only = ["anthropic"], allow_fallbacks = true }`.
  - Non-openrouter providers: routing is carried but unused (documented in the field and the key
    hint).

## 3. What does NOT change

Model ids and every id-split site; the catalog and `ModelEntry`; the menu engine and row matching;
session storage (`ModelChange`, header); the resume prefill; `default_model`; the CLI/recipe model
plumbing. Routing follows the model id through the settings record — resume restores it without
carrying anything extra in the session.

## 4. Display

- Ctrl-E fragment: the `routing` line with a key hint ("OpenRouter provider preferences: only /
  order / ignore / allow_fallbacks / require_parameters").
- Detail view summary (`settings_editor::summarize`): routing is named, e.g.
  `routing only=deepinfra/fp8/flex` — it is a request-shaping fact, so it ranks with
  reasoning/temperature, not with the counted metadata overrides.
- Status bar: unchanged (routing is not bar material).

## 5. Validation

- Structure: unknown routing keys and wrong-typed values are loud errors at load and at save
  (existing per-key machinery, nested-vocabulary note in §1).
- Semantics: `only` with an empty array is an error; `allow_fallbacks` alone is fine (a blanket "no
  fallbacks" is meaningful). An empty `routing = {}` serializes nothing and `is_empty()` keeps
  dropping the table.
- Provider fit: `parse_fragment` does not know the provider, so cross-provider checks happen at the
  editor/summary layer (a hint line, not an error — routing on a non-openrouter record is inert, not
  invalid).

## 6. Tests

- `model_settings`: routing inline-table round-trips through `parse_fragment` / `render_fragment` /
  `write_to_doc` / `read_from_doc`; unknown routing key is loud; wrong-typed value is loud; empty
  table drops.
- `merge_openrouter_params`: record routing produces `{"provider": {…}}` verbatim; absent routing
  sends no `provider` key at all (and the old pin cannot reappear); parity test that bake ==
  per-turn recomputation with routing present.
- Seeding/live-edit: activating a model with routing sets `config.routing`; `update_settings` on the
  active model updates it and `plan_rebuild` fires once; editing an inactive model's record does not
  touch the live config.
- `is_empty` / record-drop: a record with only routing writes a table; an emptied one removes it.

## 7. Suggested steps

1. `Routing` struct + `ModelSettings` key: coerce/apply/render/round-trip + tests (no behavior
   change anywhere else).
2. `AgentConfig.routing` + seeding in `activate_model_params` + rebuild fingerprint +
   `update_settings` live-apply.
3. Wire merge in the openrouter arm + parity tests.
4. Summarize/fragment hint polish.
5. `cargo fmt`; update `doc/todo.md` to link this doc (done).

## 8. Open questions

1. Should a routing-bearing record on a non-openrouter model be an editor warning rather than a
   doc-comment note? (Leaning: warning in the fragment, since a silent no-op setting is a footgun.)
   - Answer: In the editor might be difficult to pull off, but maybe in the model detail view?
     Implemented there: the detail view prints `note: routing only applies to openrouter models —
     ignored here`; the fragment stays provider-agnostic.
2. More routing keys later (sort/quantizations/zdr) — add per demand, each with a coercion case and
   hint.
   - Answer: Yes, likely. This also suggests that the single-line format might be hard to read.

## 9. Evaluated alternative: `@only:` model-id suffix (rejected for now)

The original todo sketch: append `@only:deepinfra/fp8/flex` to the model id in the dialog; a general
parser splits `@…` early; storage keeps the full id; the OpenRouter backend adds the JSON. Fully
worked out (rig support exists, the parity path works, session storage round-trips), but it costs:

- a suffix strip at ~10 model-id split sites (catalog lookups, reasoning support, status bar, detail
  view, params memory keys),
- a full-id (identity/storage) vs base-id (lookup) split with per-site discipline — one missed strip
  is a subtle bug,
- a menu-engine change so a typed suffixed word matches its row (safe only as a
  fallback-after-exact-miss),
- a resume prefill that must carry the suffix, and a settings-keying question (base vs full) for
  per-model records.

It wins only if routing becomes an *ad-hoc, per-activation* choice from the listing rather than
durable per-model configuration. Keep as a reference; layer it later without disturbing this design.
