# Model aliases: several presets over one upstream model

Date: 2026-10-23. Status: proposed.

## Goal

Let one upstream model appear several times in the `/model` picker, each entry running its own
per-model settings (routing, params, metadata), by declaring an *alias* record:

```toml
[models."openrouter:claude-fast"]
alias_for = "openrouter:anthropic/claude-sonnet-4"
routing = { only = ["deepinfra/fp8/flex"], allow_fallbacks = false }
```

`openrouter:claude-fast` is a distinct picker row, a distinct session model and a distinct stored
record; only the request is remapped to the upstream. Aliases may be cross-vendor
(`openrouter:x` → `anthropic:y`) — nothing ties the pair to one provider.

## Mechanism (two halves)

1. **Catalog injection.** Every stored record carrying `alias_for` contributes a synthetic
   `ModelEntry` keyed to its own id, as if a listing had returned it. Base facts come from the
   catalog entry for the **upstream id**; the alias record's own metadata overrides then win, so a
   bare alias still shows real numbers.
2. **Wire remap.** At agent creation, `provider`/`model` map to the upstream when the selected
   record sets `alias_for`. Params, routing and persona are the *alias's* — settings are never
   inherited.

## One composition

`AppState::resolved_models()` is the single catalog+settings composition ("resolution"), and every
path that shows or describes a model reads it:

- `overlay()` gains the alias step above.
- `resolved_entry()` stops being a second composition and becomes a lookup into `resolved_models()`
  (a lock-free inner, because `update_settings` calls it while holding the lock). The record-only
  fallback moves into `overlay`, which must then also append an entry for a stored model the catalog
  does not cover — so a configured model always has a row.
- `ModelNode` always carries `Arc<AppState>`; a `show_recents` flag replaces the `live: Option` that
  existed only to suppress recents under `/set default_model`. Every menu path then reads resolved
  entries — alias rows included, which that path omits today.

So no menu calls the catalog: `v25_reasoning`, `v25_model` and `/stat` render the resolved entry
they already hold.

## What an entry carries

One new `ModelEntry` field, filled by `overlay`:

- `reasoning_map: [Option<String>; 5]` — the resolved reasoning mapping, indexed by prior rank.

Nothing else. An entry does not need to know it is an alias: the wire provider is used *inside*
`overlay` to build the map step, and the alias's display label comes from the record's description.

## Reasoning, resolved

`reasoning_map[rank]` is the **provider spelling** the preset maps to — the string the backend puts
on the wire (`med → medium` on DeepSeek). It is *total* over the five prior levels: a provider that
publishes only `["max","high","low"]` still maps `med`, because the provider's request table says
what `med` becomes. `off` is `None` (absence of reasoning is not an effort), and every level is
`None` when the provider has no vocabulary.

`overlay` fills it with one call per level to `params::resolve_effort_for(wire_provider, level, support)`
— the same function the backend calls to build its request. So the menu and the wire share one
implementation of the mapping: the entry holds the answer, and the backend recomputes it per request
because a `/reasoning` override can change the level between turns. Neither re-derives the tables.
The five calls can build one table per provider rather than scanning per level.

Everything downstream is pure formatting: `/reasoning`'s rows and header print each preset's
spelling. The provider's own request table (`deepseek maps: medium→high`) is disclosed alongside, so
"what we send" and "what it runs as" are both visible — a spelling we send can differ from the effort
the service actually runs.

`reasoning_support` stays on the backend path and keeps no menu caller.

## The wire path

`AgentConfig` gains `alias_for: Option<String>`; `create_agent` derives
`wire = alias_for.unwrap_or(identity)`. That derivation drives the provider match and client
construction, `request_additional_params`, `is_anthropic_family` and the `MessageLogger` pair.
`agent`/`model` stay the identity for the session header, `ModelChange`, resume prefill, the status
bar, `/stat` and `default_model`, while `plan_rebuild` fingerprints the wire — so changing an alias's
`alias_for` (or switching alias ↔ upstream) rebuilds exactly once.

`alias_for` is seeded at activation (like `routing`/`context_limit`) and refreshed by
`update_settings` for the active model. The `main.rs` one-shot paths build without an `AppState`, so
`main` resolves `alias_for` out of the parsed `Config.models` before assembling `agent_config`.

## Where the pieces live

- `model_settings`: `alias_for` key — `KEYS`, `key_hint`, `coerce` (quoted `provider:model`, both
  halves non-empty), `apply_one`, `value_of`, `is_empty`, the fragment renderer.
- `model_catalog`: `overlay` injects alias entries and appends record-only entries, filling
  `reasoning_map`; `resolved_entry` delegates to `resolved_models`; `ModelEntry` gains
  `reasoning_map`.
- `params`: `reasoning_map(wire_provider, support) -> [Option<String>; 5]`.
- `agent/build.rs`: `AgentConfig.alias_for`; `create_agent` derives and uses the wire pair.
- `app_state`: activation and `update_settings` seed `alias_for`; a lock-free `resolve_entry` inner.
- `v25_model`: always `AppState` + `show_recents`.
- `v25_reasoning`: renders `reasoning_map` from the active resolved entry.
- `v25_stat` / `status.rs`: `session_stats_lines` takes the resolved entry rather than `stored`.

## Validation

Loud at load and save, like every other key:

- `alias_for` must be a quoted `provider:model` with both halves non-empty.
- Self-reference (`alias_for` equal to the record's own key) is an error.
- Alias → alias is refused (one hop only): the referenced id must not itself carry `alias_for`. This
  kills cycles without a resolver loop.
- The upstream need **not** exist in the catalog (a direct provider, or a model OpenRouter does not
  publish); then the alias entry's base facts are empty unless the record supplies them.
- If an alias key collides with a real catalog id, the catalog row is kept (no duplicate row); the
  wire remap still applies.

## What does NOT change

The catalog cache, `dedupe`, `order_models`, the menu engine and row matching, session storage
(`ModelChange`, header — they record the identity), the resume prefill, `/set default_model`, and the
recipe/CLI plumbing. An alias is an ordinary catalog row plus one remap at build time.

## Display

- Picker row: description defaults to `alias for <upstream>` when the record sets none.
- `/model <alias>` header: the alias's own settings over the upstream's facts, as for any model.
- Status bar / `/stat` / session: the identity.

## Tests

- `model_settings`: `alias_for` round-trips through `parse_fragment` / `render_fragment` /
  `write_to_doc` / `read_from_doc`; malformed target, self-alias and alias-of-alias are loud;
  `is_empty` counts `alias_for`.
- `model_catalog`: `overlay` yields an alias row with the upstream's facts and the alias's
  overrides, carrying a `reasoning_map` computed from the **wire** provider; an uncatalogued upstream
  falls back to the record alone; `resolved_entry(alias)` matches the list row; a colliding alias
  does not duplicate.
- `params`: `reasoning_map` is total and matches `resolve_effort_for` for every level; `off` and a
  vocabulary-less provider give all-`None`; DeepSeek maps `med → medium`.
- Build/rebuild: activating an alias seeds `alias_for`; `create_agent` matches the **wire** provider;
  `plan_rebuild` fires once when `alias_for` changes.
- Interactive: switching to an alias leaves the identity in `AppState::model()`, the session header
  and the status bar; the request goes to the upstream; `/reasoning` discloses the upstream's
  mapping; the `/set default_model` listing contains the alias.

## Open questions

- Should aliases sort/filter specially in the picker, or is the `alias for …` description enough to
  find them by fuzzy match?
  - Decision: No special treatment. Users can name alias slugs to make them easy to search for.
- Is a cross-provider alias (`openrouter:x` → `anthropic:y`) wanted?
  - Decision: Yes — e.g. direct-vendor vs. OpenRouter access to the same model. Nothing in the design
    costs anything to support it.