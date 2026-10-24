# Model aliases: several presets over one upstream model

Date: 2026-10-23. Status: proposed.

## Goal

Let one upstream model appear several times in the `/model` picker, each entry running its own
per-model settings (routing, params, metadata), by declaring an *alias* record:

```toml
[models."openrouter:claude-fast"]
alias_for = "openrouter:anthropic/claude-sonnet-4"
routing = { only = ["deepinfra/fp8/flex"], allow_fallbacks = false }

[models."openrouter:claude-cheap"]
alias_for = "openrouter:anthropic/claude-sonnet-4"
routing = { only = ["deepinfra/fp8/cheap"], allow_fallbacks = true }
```

`openrouter:claude-fast` and `openrouter:claude-cheap` are distinct picker rows, distinct session
models, and distinct stored records. Only the request that goes upstream is remapped to
`openrouter:anthropic/claude-sonnet-4`.

## Mechanism (two halves)

1. **Catalog injection.** Every stored record carrying `alias_for` contributes a synthetic
   `ModelEntry` keyed to its own id, as if a provider listing had returned it. Base facts
   (`context_length`, prices, indices, description, effort list) are copied from the **catalog entry
   for the upstream id**; the alias record's own metadata overrides then win. So a bare alias still
   shows real numbers, and a picker row exists at all.
2. **Wire remap.** At agent creation, `provider`/`model` are mapped to the upstream id if the
   record for the selected id sets `alias_for`. Nothing else about the request changes: params,
   routing, persona are the *alias's* — settings are not inherited from the upstream record.

Alias settings are the alias's alone; the upstream's own `[models."…"]` record (if any) is used
only when the upstream id is itself selected. There is no inheritance in either direction.

## Identity vs. wire

The one hard part is that two ids now exist per request: the **identity** (what the user picked,
what the session and status bar name — the alias) and the **wire** (what rig's client is built
with — the upstream). They must be kept apart by *purpose*, not by accident:

- **identity** — session header + `ModelChange`, resume prefill, status bar, `/stat` header,
  `default_model`, `qualified_model_id()` and the rebuild fingerprint's `model`.
- **wire** — `create_agent`'s provider match and client construction, `request_additional_params`
  (reasoning spelling + `top_p` + OpenRouter routing), the `reasoning_support` lookup,
  `is_anthropic_family` (thinking-replay contract), and the `MessageLogger` provider/model pair.

`AgentConfig` gains `wire_provider: String` / `wire_model: String`; `model`/`provider` stay the
identity. `CodeAgent` carries both pairs. `plan_rebuild` fingerprints the wire pair, so changing an
alias's `alias_for` (or switching alias ↔ upstream) rebuilds exactly once.

## Resolution points

One pure helper is the only place the mapping is decided:

```rust
/// The id a request for `id` must actually be sent to: the record's `alias_for`
/// when set, else `id` itself.
pub fn wire_id(models: &HashMap<String, ModelSettings>, id: &str) -> String;
```

It is called from three places:

- **Activation** (`AppState::activate_model_params`): alongside the params/routing seeding it
  already does, set `config.wire_provider`/`wire_model`. Startup copies them onto the applied
  config like `config.params` / `context_limit`, so the first agent is built once.
- **`update_settings`**: an edit of the *active* model's record refreshes the target's wire pair,
  the same precedent as `routing`/`context_limit`. The rebuild gate then fires the one rebuild.
- **`main.rs` one-shot paths** (`Recipe` / `Input` / positional prompt): these build the agent
  without any `AppState`, so `main` resolves the alias out of the parsed `Config.models` before
  assembling `agent_config`, setting `provider`/`model` to the wire pair (a one-shot run has no
  session or status bar to name the alias in).

## Where the pieces live

- `model_settings`: new `alias_for: Option<String>` key. `KEYS`, `key_hint`, `coerce` (quoted
  string, form-checked), `apply_one`, `value_of`, `is_empty`, and the fragment renderer each gain
  the one case; `wire_id` joins the module.
- `model_catalog`: `overlay` appends the alias entries (it already holds the whole `stored` map).
  `resolved_entry` becomes alias-aware so the *single-entry* view (the `/model <id>` header,
  `/stat`, the editor prefill, `AppState::update_settings`'s window read) agrees with the list: it
  looks up the upstream entry as the base, re-keys `provider`/`id`/`display` to the alias id, tags
  `alias_for`, then applies the alias record's overrides. `ModelEntry` gains
  `alias_for: Option<String>` (display marker only).
- `agent/build.rs`: `create_agent` reads `wire_provider`/`wire_model` for the provider match, the
  client, and `request_additional_params`; `CodeAgent::new` takes and stores both pairs.
- `agent/turn.rs` / `agent/transcript.rs` / `agent/build.rs::LoggingHook`: the reasoning strip and
  the per-request params recompute switch from `self.provider`/`self.model` to the wire pair.
- `v25_reasoning`: the remap disclosure must describe the *wire* model, or an alias would read as
  "no data". Add `AppState::active_reasoning_support()` (identity → wire via the record → raw
  catalog lookup) and use it, keeping disclosure and backend remap in lockstep exactly as they are
  today.
- `v25_model`: the `/model <alias>` header and row show `alias for: <upstream>`; the alias's
  `routing` inert-note keys off the **wire** provider.

## Validation

Loud at load and save, like every other key:

- `alias_for` must be a quoted `provider:model` with both halves non-empty.
- Self-reference (`alias_for` equal to the record's own key) is an error.
- Alias → alias is refused (one hop only): the referenced id must not itself carry `alias_for`.
  This kills cycles without a resolver loop.
- The upstream need **not** exist in the catalog (a direct provider, or a model OpenRouter does
  not publish); then the alias entry's base facts are empty unless the record supplies them.
- If an alias key collides with a real catalog id, the catalog row is kept (no duplicate row); the
  wire remap still applies.

## What does NOT change

The catalog cache, `dedupe`, `order_models`, the menu engine and row matching, session storage
(`ModelChange`, header — they record the identity), the resume prefill, `/set default_model`, and
the recipe/CLI plumbing. An alias is an ordinary catalog row plus one remap at build time.

## Display

- Picker row: description defaults to `alias for <upstream>` when the record sets none.
- `/model <alias>` header: an `alias for: <upstream>` line, plus the target's facts (from the
  upstream entry) and the alias's own settings, exactly as today.
- Status bar / `/stat` / session: the identity.

## Tests

- `model_settings`: `alias_for` round-trips through `parse_fragment` / `render_fragment` /
  `write_to_doc` / `read_from_doc`; malformed target, self-alias, and alias-of-alias are loud;
  `is_empty` counts `alias_for`.
- `model_catalog`: `overlay` yields an alias row with the upstream's facts and the alias's
  overrides; an uncatalogued upstream falls back to the record alone; `resolved_entry(alias)`
  matches the list row; a colliding alias does not duplicate.
- `wire_id`: target when set, identity otherwise.
- Build/rebuild: activating an alias seeds the wire pair; `create_agent` matches the **wire**
  provider; `plan_rebuild` fires once when `alias_for` changes.
- Interactive: switching to an alias leaves the identity in `AppState::model()`, the session
  header, and the status bar; the request goes to the upstream.

## Open questions

- Should aliases sort/filter specially in the picker (an `.alias` modifier, or hoisted near the
  top), or is the `alias for …` description enough to find them by fuzzy match?
- Is a cross-provider alias (`openrouter:x` → `anthropic:y`) wanted, or should the alias and its
  upstream share a provider? Nothing in the design prevents it; the wire pair is already
  provider-independent.