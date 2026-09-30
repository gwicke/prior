# Model settings, v2

Per-model model parameters: a general `ModelParams` set (reasoning, temperature,
top_p), per-model persisted defaults, a `/model <id>` action listing with a
TOML editor, and ephemeral per-turn overrides at the tail of a prompt.

> **Status (2026-09-29):** design agreed, not started. Supersedes the original
> sketch; the shape below is what the Q&A settled. Renames are hard breaks —
> this is an experimental personal fork (same call as the YAML→TOML switch in
> `doc/settings_menu.md`).
>
> **Scope:** three tracks, staged in Part B. Track 1 (params) is the
> foundation; nothing else is worth building before it lands.

---

# Part A — Technical design

## A.1 Current state (what already exists)

**The params funnel is narrow — one place to extend.** There is exactly one
`additional_params` producer:

- `src/thinking.rs:53` `mapping(provider, level) -> Option<Value>` — per-provider
  JSON for the five internal levels.
- `src/agent.rs:1466` computes it once in `create_agent`;
  `src/agent.rs:1499` folds OpenRouter's `ProviderPreferences` into it
  (`merge_openrouter_params`, `agent.rs:1434`);
  `src/agent.rs:2040` hands it to `AgentBuilder::additional_params`.
- `AgentConfig.thinking_level: Option<String>` (`agent.rs:1364`) is the only
  model param today.
- The rebuild gate (`agent.rs:3646-3706`) compares
  `model_changed || thinking_changed || persona_changed` against the *running*
  config and calls `create_agent` once per distinct target.

**Settings persistence** (`src/settings.rs`): `SettingsStore` is the one in-memory
copy; `save()` (`settings.rs:544`) round-trips `settings.toml` through
`toml_edit::DocumentMut`, preserving comments and `[recipes.*]`. `Config`
(`config.rs:878`) has no `deny_unknown_fields`, so new top-level tables are
backward compatible. `SettingsStore` is built from the resolved `AgentConfig`
at startup (`main.rs:323`), *not* from the file — new file-backed data must be
threaded explicitly. `SettingsStore { … }` is constructed literally in 33
places; adding a field is wide but purely mechanical.

**The menu engine** (`crates/slashdot_menu`): a `NodeEntry` with children is a
listing, without children a leaf (`node_entry.rs:359`). A node's own `data()`
feeds the *parent's* listing; children receive the parent path in
`path_args.ids`, so a dynamic child can act on the drilled row. `edit()`
(Ctrl-E) dispatches from **both** leaf (`node_entry.rs:419`) and listing
(`node_entry.rs:549`) nodes, and can return `Prepared` to arm the confirm
dialog. The `/model` subtree is built at `v25_model.rs:318`; `ModelNode` is
reused by `/set default_model` (`v25_set.rs:160`).

**The editor** is one wrapper: `crate::editor::run_editor` via
`AppState::edit_text` (`app_state.rs:465`). `EditorMode::Edit` reads the result
back, `View` is read-only. The two-phase prepare → editor → confirm → submit
pattern is established by compaction (`v25_tree.rs:1385-1482`).

**The catalog** (`src/model_catalog.rs:49` `ModelEntry`) is built from
OpenRouter's public `/api/v1/models` (`parse_openrouter`, `model_catalog.rs:253`)
plus curated `DIRECT_PRICING`. It currently keeps only
`supported_parameters.contains("reasoning")` and **drops** `description`,
`temperature`/`top_p` support, and the `reasoning.supported_efforts` list.

**rig capabilities** (checked against the pinned rev `1c2f440`):
- `AgentBuilder::temperature(f64)` sets the typed `CompletionRequest.temperature`
  field, which each provider maps to its own spelling (OpenAI/Anthropic flat,
  Gemini → `generationConfig.temperature`, `gemini/completion.rs:362`).
  **Provider-correct for free.**
- `top_p` has no typed field anywhere. It must go through `additional_params`,
  which is `#[serde(flatten)]`-ed into the request body for OpenAI/Anthropic/
  OpenRouter — **but Gemini parses it into a typed
  `AdditionalParameters { generation_config, #[serde(flatten)] additional_params }`**
  (`gemini/completion.rs:845`): a flat `top_p` is forwarded verbatim to the wire
  and rejected. Gemini needs `{"generationConfig": {"topP": …}}`.
- `StreamingPromptRequest` (per-request, from `Agent::stream_prompt`) exposes
  `.temperature()`, `.merge_additional_params(map)` and
  `.without_additional_params()` — **per-turn overrides need no agent rebuild**.

## A.2 Data model

One struct for "the params of a model", used by every layer (live, stored,
per-turn):

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelParams {
    /// `None` = off / not set. Never sent to the provider.
    pub reasoning: Option<ReasoningLevel>,
    /// `None` = unspecified, leave the provider default (current behaviour).
    pub temperature: Option<f64>,
    /// `None` = unspecified, leave the provider default.
    pub top_p: Option<f64>,
}
```

`ReasoningLevel` is the hard rename of `ThinkingLevel` (`Off | Low | Med | High |
Max`); the internal ladder and its `FromStr`/`as_str` are unchanged, so
`/reasoning med` keeps working verbatim.

`AgentConfig.thinking_level: Option<String>` is replaced by
`AgentConfig.params: ModelParams`. `AgentConfig` derives `Clone`, and
`PartialEq` on `ModelParams` gives the rebuild gate its comparison for free.

**Stored per-model settings** are `ModelParams` plus optional *metadata
overrides* (for models OpenRouter does not cover, or to correct it):

```rust
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    // --- params (same spelling as ModelParams) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    // --- catalog metadata overrides; used only where set ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// True when the model exposes any reasoning effort at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// The efforts this model accepts, e.g. `["low", "high", "max"]`.
    /// Drives backend effort remapping (§A.4) without per-model code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,

    /// Leading comments per key, carried across saves (§A.9). Never
    /// serialized — rebuilt from the file on every load/write.
    #[serde(skip)]
    pub comments: BTreeMap<String, String>,
}
```

`deny_unknown_fields` is deliberate: a typo in `[models.*]` is a **load-time
error naming the key**, not a silently ignored line. (It is scoped to this
struct only — the top-level `Config` stays permissive.)

Two accessors keep the layers apart:

```rust
impl ModelSettings {
    /// The params half — what the agent actually sends.
    pub fn params(&self) -> ModelParams;
    /// Overlay the metadata half onto a catalog entry. `None` fields keep the
    /// catalog's value, so partial tables are normal.
    pub fn apply_to_entry(&self, e: &mut ModelEntry);
}
```

## A.3 Catalog enrichment

`ModelEntry` (`model_catalog.rs:49`) gains four fields, all parsed from the
OpenRouter payload:

```rust
pub description: Option<String>,       // data[].description (~200 chars)
pub supports_temperature: bool,         // derived, array not retained
pub supports_top_p: bool,               // derived
pub reasoning_levels: Vec<String>,      // data[].reasoning.supported_efforts
/// The provider's own default, when OpenRouter publishes one. **Display only** —
/// never sent, and deliberately not part of the "None means unspecified" contract.
pub default_temperature: Option<f64>,  // data[].default_parameters.temperature
pub default_top_p: Option<f64>,         // data[].default_parameters.top_p
```

`supported_parameters` is **not** retained: it is a ~20-element array and the
catalog is 300–460 entries held in a process-wide cache. Two booleans capture
all we need, derived once at parse time. `reasoning_levels` is kept verbatim
(at most ~6 short strings) because §A.4 needs it. `default_parameters` also
carries `top_k` / `frequency_penalty` / `presence_penalty` / `repetition_penalty`
and explicit `null`s; we read only the two keys we model and treat `null` as
"no default published".

*Measured against the live API* (60 most-popular models, 2026-09-29): only
**19 of 60** publish a `temperature`/`top_p` default, and the values are **not**
uniform — temperature ∈ {0.3, 0.7, 0.9, 1.0} and `top_p` ∈ {0.95, 1.0}
(`tencent/hy3` is `temperature 0.9`; `openai/gpt-6-luna` publishes explicit
nulls for everything). So the header must **not** hardcode a "typical" 1.0 /
0.95 — it shows the model's real default where known and degrades to
"provider default" where not, which is the two-thirds case.

*Verified against the live API* (`?sort=most-popular&limit=6`, 2026-09-29):
`description` is a ~200-char blurb ending in `…`; `openai/gpt-5.6-luna` has
**neither** `temperature` nor `top_p` in `supported_parameters`;
`deepseek/v4.1-flash` supports efforts `["max","high","low"]` (no `medium`),
`tencent/hy4-preview` is `["high","low","none"]`.

Consequences:
- `/temperature` and `/top_p` can say "this model does not expose temperature"
  in the header, and can warn on set instead of letting the provider 400.
- `/reasoning med` on `deepseek/v4.1-flash` no longer sends an effort the model
  rejects — §A.4 remaps it.

**Precedence:** `ModelSettings.apply_to_entry` wins where set; the catalog
fills every field it leaves `None`. The resolved view is built in one place
(`model_catalog::resolved_entry(id) -> Option<ModelEntry>`), used by the header,
`/stat`, and the editor prefill.

## A.4 Provider mapping

`src/thinking.rs` becomes `src/params.rs` (the module's doc comment already
describes itself as the `additional_params` bridge). One entry point:

```rust
/// Provider-specific request params for a model. `support` is the model's
/// known reasoning-effort list (empty = unknown, fall back to the static map).
pub fn additional_params(
    provider: &str,
    params: &ModelParams,
    support: &ReasoningSupport,
) -> Option<Value>;
```

Composition, in order:

1. **reasoning** — the existing per-provider `mapping()` (`thinking.rs:53`),
   then the effort string is **remapped** through `support.levels` (§below).
   This is additive: no provider arm changes, only the effort it produces.
2. **top_p** — `{"top_p": v}`, except `gemini`/`google`:
   `{"generationConfig": {"topP": v}}`.
3. **temperature** — *not* in this JSON. Handed to
   `AgentBuilder::temperature(v)` (`agent.rs:2040` area), because rig already
   maps the typed field per provider.
4. OpenRouter's provider prefs still merge last, unchanged
   (`merge_openrouter_params`, `agent.rs:1434`).

**Effort remapping** (data-driven, no per-model code). Two tables: a rank over
the *internal* levels, and an alias set per rank so a backend spelling is
recognized however the provider spells it. A **third** source sits in front of
the ladder: a provider's own documented request table (§A.4.1).

```
internal:      off=0  low=1  med=2  high=3  max=4
aliases:       {off,none,disabled} {low,minimal} {med,medium} {high} {max,xhigh,x-high}
```

- `Off` → the provider's own off spelling (`None` for anthropic/openai,
  `effort:"none"` for OpenRouter-style, `thinking.type:"disabled"` for
  deepseek/xiaomi — unchanged from today). An empty or `none`-only level list
  also means off.
- Otherwise, when `support.levels` is non-empty: keep the effort if the list
  already contains it; else take the supported level with the **highest rank
  that does not exceed the target**. The user never gets *more* reasoning than
  they asked for — under-reasoning is visible and recoverable, over-reasoning
  silently costs latency and tokens. If every supported level outranks the
  target, take the lowest.
- If `support.levels` is empty (non-OpenRouter models, offline): the static
  per-provider spelling is sent as-is — today's behavior.

So `/reasoning med` on `["max","high","low"]` sends `low`, and on
`["max","medium","low"]` sends `medium`; `/reasoning max` on
`["high","low","none"]` sends `high`. The frontend never learns about it: the
menu keeps the static five-level list, and the *model header* is the only
place the backend's view surfaces (`supported efforts: low, high, max`).

### A.4.1 The provider's own request table

The step-down ladder is wrong for a provider that *accepts* an effort it does
not advertise. DeepSeek documents exactly that
(`api-docs.deepseek.com/guides/thinking_mode/`):

```
requested   served
minimal  →  low
low      →  low
medium   →  high      ← the ladder would have sent `low` here
high     →  high
xhigh    →  high
max      →  max
ultra    →  max
```

`deepseek/v4.1-flash` advertises `["max","high","low"]` — no `medium` — so the
ladder alone would downgrade a `med` request to `low` and take away reasoning
the user asked for *and* the provider was willing to give.

**Resolution: a static per-provider table, consulted first, and disclosed.**
`PROVIDER_EFFORT_MAPPINGS` in `params.rs` holds `(request, served)` pairs read
from the provider's own docs. Two jobs, both data rather than code:

1. **Never downgrade a documented request.** If the provider's table covers the
   level's rank, the request goes out as `mapping()` spelled it and the service
   applies its own mapping. The ladder is skipped entirely.
2. **Tell the user, so the choice stays theirs.** `/reasoning`'s `header()`
   shows the provider's table, the model's published list, and where each of
   our five presets actually lands — so someone who wants DeepSeek's `high`
   reads `med → high` and picks `med` deliberately rather than discovering the
   substitution. Working backwards from the desired effort is the point.

Rank-matched, not string-matched: our `med` finds their `medium`, and their
`xhigh` is understood as our `max`. Only providers whose docs we have read
appear in the table — `xiaomimimo` shares DeepSeek's wire shape and is
deliberately **absent**, since assuming parity is the same guess that produced
the wrong idea about temperature defaults, and an unmapped provider degrades
to the ladder rather than to a wrong effort.

The header reads its answer through `params::resolve_effort_for`, which derives
the on-the-wire spelling from `mapping()` itself rather than re-deriving it, so
the disclosed mapping cannot drift from the request that is sent.

## A.5 Settings storage

New top-level table in `settings.toml`, one entry per `provider:model`:

```toml
[models."openrouter:tencent/hy3:free"]
# Per-model defaults, applied when this model is first activated.
reasoning = "high"
temperature = 0.8
# top_p = 0.95

# Metadata overrides — used where set, OpenRouter catalog data otherwise.
# description = "Tencent HY 4 …"
# context_length = 1048576
# prompt_cost = 0.43
# completion_cost = 0.87
# coding_index = 61.0
# supports_reasoning = true
# reasoning_levels = ["low", "medium", "high", "max"]
```

- **Quoted keys** are required — model ids contain `/` and `:`. `toml_edit`
  round-trips them as bare keys; the doc comment lives on the table's decor.
- The table must be appended **after** existing tables (`[tool_config.*]`,
  `[recipes.*]`): the managed root keys in `settings.toml.example` must stay
  above the first `[table]` header, and `toml_edit` appends new tables at the
  end, so this is satisfied by construction.
- `SettingsStore` gains `models: HashMap<String, ModelSettings>`; `Config`
  (`config.rs:878`) gains the same field so it is read at load. `main.rs:323`
  must thread `config.models` into the store (`from_agent_config` has no access
  to the `Config` — add a `with_models` step there, keeping the existing
  constructor signature used by 33 test sites).
- **Writers** (all via `DocumentMut`, so comments, `[recipes.*]` and
  `tool_config` survive):
  - `SettingsStore::put_model_settings(id, &ModelSettings)` — replace one table
    (used by `save` and by the editor).
  - Existing `save()` is left alone; both funnel through one private
    `write_doc(path, f)`.
- **What `save` writes** (decided): the keys that differ from their built-in
  default, where the default is "the value the seeding rule would have given a
  model with nothing on file".
  - `reasoning` — **always written**. Live reasoning is never `None`: it is
    seeded from `/set default_reasoning_level` (or the built-in `off`), so
    saving pins the currently effective level into the model's record.
  - `temperature`, `top_p` — written **only when live `Some`**. A model the user
    never touched keeps no keys for them, so the provider default keeps
    applying.
  - Absent keys are *removed* on save — that is how a previously saved value is
    cleared back to the provider default.
  - *Consequence, intended:* because stored wins over the global in §A.6,
    saving model A freezes today's `/set default_reasoning_level` into A. A
    later global change no longer reaches A, only never-saved models. This is
    the price of "the model file is authoritative once it exists".
  - A **per-turn override never counts as "modified"** — it is stripped and
    reverted at end of turn, so it can neither be saved nor survive a switch.
  - **Which models `save` accepts:** any model *used* this session, not only
    the one running now. The source is `model_param_map[id]` — the A.6
    per-model memory — rather than `config.params`, so a → b → a's runtime
    values can be made durable without re-activating a first. For the current
    model the two are the same value by the invariant, so this widens the set
    without changing any existing behavior. A model never used this session has
    no live params to save and says so.
  - *Consequence, intended:* `save` on a model that was visited but never
    changed materializes the global's current level into that model's record
    (the `reasoning`-always-written rule above). Pinning today's global is what
    `save` means, but it is worth knowing a no-op-looking `save` is not a
    byte-level no-op.

## A.6 Runtime state

`State` (`app_state.rs:138`) gains two fields, alongside the existing runtime
target `config: AgentConfig`:

```rust
/// Per-model live memory, session-scoped: id -> last live params. Makes
/// a → b → a restore a's settings.
pub(crate) model_param_map: HashMap<String, ModelParams>,
```

There is deliberately **no** separate `params` field. The live params of the
current model stay in `config.params`, where they already were and where the
menu and the loop both already read them; the map is the parallel structure that
remembers them per model. Two copies, one of which pre-existed, rather than
three. The invariant is `model_param_map[current_id] == config.params`, held by
construction because every write goes through one private helper
(`AppState::write_live_params`) that sets both halves — the only two live
writers are `set_param` (which `/temperature` and `/top_p` both funnel through)
and `set_reasoning`. A drift here is exactly what made a first cut lose a→b→a.

`model_param_map` starts **empty** even though `config.params` is seeded at
construction: the caller's first `activate_model_params` is what consults the
record, and pre-seeding would make it find its own entry and never get there.

with the app-state lock rules honored (one acquisition per public method, no
file IO under the guard — clone, drop, write):

| method | role |
|---|---|
| `params() -> ModelParams` | read the live params |
| `set_param(k, v)` / `set_reasoning(l)` | one live field (menu writes) |
| `activate_model_params(id)` | load-or-seed on activation (§A.7) |
| `model_settings(id) -> Option<ModelSettings>` | stored record (editor, header) |
| `apply_model_settings(id, &ModelSettings)` | record in memory, no write |
| `update_settings(f)` | mutate the store and persist, write outside the lock |

**Seeding rule** (`activate_model_params`) — **per field**, never per record:

```
model_param_map[id] exists?          → use it, done (a → b → a)
else for each field f in {reasoning, temperature, top_p}:
    stored[id].f                → use it
    else global /set default    → reasoning only: default_reasoning_level
    else built-in               → reasoning: off;  temperature/top_p: None
    and insert the result into model_param_map[id]
```

`temperature`/`top_p` have **no `/set` global** (decided: no new global keys)
and their built-in is `None` — leave the provider default, today's behavior.
That built-in is applied to **every newly loaded model**: switching from a model
the user set `temperature = 0.2` on to one with nothing on file gives the
second model `None`, not an inherited `0.2`. Only the per-model record can set
them.

**Editing an inactive model** falls out of this: `put_model_settings` updates
the store only. If the model has not been activated this session, its live
entry does not exist yet, so the next activation seeds from the freshly stored
values. If it *has* been activated, the live values win until the user changes
them (or runs `save`, which copies live → stored).

## A.7 Convergence

In `run_interactive`'s convergence step (`agent.rs:3635`):

1. Seeding happens **on activation, not here**: the `activate` action node
   calls `app_state.activate_model_params(&model)` right after `set_model`, and
   `run_interactive` does the same for the startup model before
   `ensure_session`. Not in the loop, because the loop snapshots its target at
   `agent.rs:3800` and compares at `3817` — a seeding call would have to land
   before the snapshot, where the model is not yet known, or after it, where it
   is stale, and the switch would cost two rebuilds with the outgoing model's
   params in between. Not in `set_model` either: `/set default_model` shares
   that writer but is not an activation.
2. Rebuild gate: `params_changed = config.params != target.params` joins
   `model_changed || persona_changed` in the same condition; the `target_fp`
   tuple (`agent.rs:3657`) gains `params` so a failed build is not retried every
   cycle.
3. `new_cfg.params = target.params.clone()` whenever any of the three fires.
4. `record_thinking_change` → `record_reasoning_change` (same journal entry; see
   A.11 renames).

## A.8 Menu

### `/model <id>` → an action listing

`ModelNode` keeps `data()` (the catalog rows, which feed the `/model` listing)
and **gains children**, making the drilled level a listing:

```
/model                       (listing: the static "model" row)
└── ModelNode                (data() = catalog rows; now a listing)
    ├── activate             (set this model active — journal ModelChange)
    └── save                 (live params → [models."<id>"])
```

There is deliberately **no per-model settings sub-listing** and **no new global
`/set` key** (decided): the actions are `activate`, `save`, and Ctrl-E. Live
edits of a model's params are the global `/reasoning`, `/temperature`,
`/top_p` — they always act on the *active* model.

- Each action child's `data()` returns its rows only when
  `path_args.ids.len() >= 2` (a model is drilled); at the `/model` level it
  returns nothing, so the catalog listing is unchanged.
- The cursor starts on `activate`, so `/model <id>` + `Enter` still activates
  in one keystroke — the current behavior is preserved. `↓` then `Enter` picks
  `save`.
- `save` is only offered when the node is the **live** one (`live.is_some()`);
  under `/set default_model` the same `ModelNode` is reused (`v25_set.rs:160`),
  where "activate" means "set as default model" and saving live params is
  meaningless. On the live node it accepts any model *used* this session, not
  only the current one (see A.5).
- The model's `header()` renders: display name, OpenRouter description, ctx /
  $/M in / $/M out / coding / intelligence / agentic, `supported efforts: …`
  when known, whether temperature/top_p are supported, and the **current**
  settings (`reasoning: high · temperature: 0.8 · top_p: —`), ending with
  `ctrl-e to edit`.
- `ModelNode::edit()` (Ctrl-E) runs the TOML editor of A.9 for the drilled id —
  it works for **inactive** models too.

### `/reasoning`, `/temperature`, `/top_p`

- `/reasoning` — the renamed `ThinkingEnumNode` (`v25_thinking.rs:20`), same
  static five-level list, now writing `set_param(Reasoning)`.
- `/temperature`, `/top_p` — a new `NumberParamNode` leaf:
  - `data()` — one row carrying the current value (and the model's support).
  - `drill_rewrite` — pre-fills the current value when set, and **pre-fills
    empty when unset**; same shape as `SettingLeafNode` (`v25_set.rs:106`).
    A literal placeholder number would be a lie: the built-in default is
    `None` (provider's own), not `1`, and pre-filling `1` would both misreport
    the state and — on Enter — *set* `1` where the user wanted it unset. The
    header carries "default: provider" instead, as text.
  - `submit()` — parse + range-check + `set_param`; `Error` on a bad value
    (stays open, engine-rendered). Submitting an **empty** value clears the
    setting back to `None` (i.e. back to the provider default) — the inverse
    of setting it, and the only way to undo one without Ctrl-E.
  - `header()` — current value (or "provider default" when unset), the
    provider's real default where the catalog publishes one (`default 0.9` for
    `tencent/hy3`, not a hardcoded 1.0), what higher/lower does ("lower = more
    deterministic"), and the supported range when the catalog knows it.
  - **No `complete_parameter`** for the numbers (decided). Discovery comes from
    the header and the prefill.

**The known default is display-only, never sent.** Knowing that
`tencent/hy3` defaults to `temperature 0.9` must not turn into sending `0.9`:
`None` stays "unspecified, leave the provider alone" (§A.2), and A.5's
"changed from the built-in default" test is against the built-in `None`, not
against the published default. Otherwise every model would get an explicit
`temperature` on the wire, changing sampling on models that were fine and
breaking the reasoning models that reject it. The catalog default is a fact
*about* the provider, which is why it is also excluded from the
`ModelSettings` override set — a user editing a non-OpenRouter model's table
is configuring picocode, not annotating the provider.

Validation: `temperature` ∈ [0.0, 2.0], `top_p` ∈ [0.0, 1.0], errors naming the
key, the value and the range. A value inside our range but outside a known
provider's (Anthropic caps temperature at 1.0) is accepted with a warning in the
header rather than rejected — see Risks.

## A.9 The TOML editor routine (reusable)

New module `src/settings_editor.rs` — deliberately **not** a method on the model
node, since "edit settings via toml" is meant to be reused (a future `/set`
bulk edit, a future per-session table).

```rust
/// Render `current` as a commented TOML fragment, hand it to `$EDITOR`
/// (`EditorMode::Edit`), parse the result, and return the typed settings.
/// `Ok(None)` = the user aborted or the text was unchanged.
pub fn edit_fragment(
    app_state: &AppState,
    title: &str,
    current: &ModelSettings,
    catalog_hints: &dyn Fn() -> ModelSettings,
) -> Result<Option<ModelSettings>, String>;
```

- **Pre-fill** = the stored record, with comments preserved (§ below), and
  unknown catalog values rendered as commented-out suggestions
  (`# context_length = 1048576`) so the user can accept one by uncommenting.
- **Comment preservation**: `ModelSettings.comments: BTreeMap<String, String>`
  holds each key's leading comment block, captured when the table is read and
  refreshed on every write. A key the user deletes loses its comment; a key the
  user edits keeps its comment. Tables round-trip through `DocumentMut`, so
  comments *outside* the model table are untouched by construction.
- **Validation and errors** (the "do it well" requirement):
  - unparsable → `toml_edit`'s error with its span, re-anchored to the fragment
    (`line 4: expected `.``, got `= 3`);
  - unknown key → `unknown key 'temprature' (did you mean 'temperature'?)` —
    `deny_unknown_fields` plus a Levenshtein suggestion;
  - wrong type / out of range → the key, the found value, the expected range;
  - all errors are collected and shown together, not one at a time.
- **Outcome**: the node's `edit()` returns `Done(msg)` on commit. The editor
  round-trip *is* the confirmation — unlike compaction, nothing is destroyed
  and nothing is written before the user quits the editor. (A
  `Prepared` → confirm variant stays available if we later want it.)

## A.10 Per-turn overrides

At the tail of a submitted (non-menu) prompt:

```
explain this function
/temperature 0.2
/top_p 0.9
```

**Grammar.** From the end of the input, walk lines backwards. A line is an
override iff it matches `<cmd> <value>` — exactly two whitespace-separated
tokens, `cmd` ∈ {`/reasoning`, `/temperature`, `/top_p`}, `value` a single
token. Stop at the first non-matching line; everything from there down is
prompt text. Trailing blank lines are trimmed first. If nothing matched, the
input is returned untouched.

**Pure function, fully unit-testable:**

```rust
pub struct Split {
    pub text: String,                    // the prompt as committed
    pub overrides: TurnOverrides,        // last-wins per key
    pub warnings: Vec<String>,           // shown once, then the turn proceeds
}
pub fn split(input: &str) -> Split;
```

- A line that is **syntactically valid but semantically wrong** (`/temperature
  3.0`, `/temperature abc`) is still stripped, and produces a warning:
  `warning: /temperature 3.0 ignored (must be 0.0–2.0)`. The turn continues
  (decided). This keeps the "no mystery text reached the model" invariant.
- `/reasoning <level>` never fails: an unknown level remaps through the same
  §A.4 machinery, warning whenever it had to move at all
  (`note: reasoning 'med' → 'low' (model does not support medium)`).
- Duplicate keys: last wins, earlier ones warn.

**Application** — per request, no rebuild:

- `PicoAgent::prompt` (`agent.rs:79`) gains a `&TurnOverrides` parameter;
  `CodeAgent` chains `.temperature(v)` / `.merge_additional_params(top_p_map)`
  onto the `stream_prompt` request at `agent.rs:267-289` before
  `.history(prior).await`.
- Overrides are held by the REPL turn (the same scope as `TurnPersistence`), so
  **retries re-apply them automatically** (`next_input` is already stripped).
  Synthetic steering messages ("continue", abort feedback) dispatch **without**
  them — the override belongs to the user's dispatch only.

**Wiring** in the `Submit` arm (`agent.rs:3790`):

```
line_history.add(&input);              // RAW — the user recalls what they typed
let split = turn_overrides::split(&input);
for w in &split.warnings { output.display_text(w) }
if split.text.trim().is_empty() { warn; continue; }   // overrides-only prompt
… ensure_session …                                      // uses split.text
agent.prompt(&split.text, …, &split.overrides)          // session persists split.text
```

**The empty-prompt guard — checked, and it does not cover this case.** There is
already a guard at `agent.rs:3791`:

```rust
if input.is_empty() { continue; }
```

but it runs on the **raw** input, *before* any extraction, so an
overrides-only prompt sails past it. Two further notes from the check:

- The guard is `is_empty()`, not `trim().is_empty()`, so a whitespace-only
  prompt already slips through today. It is not caught downstream either:
  rig's `validate_message_content` enforces *block count*, not content
  (`request.rs:812` — "a single empty-string block still passes"), so a
  `Message::user("   ")` is a well-formed request and goes to the provider.
  The new guard uses `trim()` and closes that gap as a side effect.
- Order matters: the guard must sit **after** `split()` and **before**
  `ensure_session` (or it creates a session for a turn that never runs) and
  before `history.add` of the stripped text (which never happens — history gets
  the raw line, above).

History/session, per the decision: **prompt history keeps the raw text**
(Up/Down re-runs the exact line, including its overrides); the **session
persists the stripped text** (the model never sees the directive lines). The
same split runs in the quiet/pipe path — both funnel through this arm.

## A.11 Renames (all hard breaks)

| Old | New | Notes |
|---|---|---|
| `/thinking` | `/reasoning` | menu row, root listing test (`v25_menu.rs:190`) |
| `ThinkingLevel` | `ReasoningLevel` | `thinking.rs`, `settings.rs`, `agent.rs` |
| `AgentConfig.thinking_level` | `AgentConfig.params: ModelParams` | + `AppState::thinking()` → `model_params()` |
| `settings.toml` `default_thinking_level` | `default_reasoning_level` | `/set` writes the new key |
| `SettingsStore.default_thinking_level` | `.default_reasoning_level` | 33 test constructors |
| `record_thinking_change` | `record_reasoning_change` | fn name only |
| **journal `thinking_level_change` / `thinkingLevel`** | **unchanged** | on-disk session format is a compatibility surface — old sessions must still replay; the value vocabulary is identical, so no data migration is needed |
| `thinking.rs` | `params.rs` | the `additional_params` bridge |

---

# Part B — Implementation plan

Each phase is independently green; P0–P1 are the prerequisite for the rest.

**P0 — rename (no behavior change).** **Done (2026-09-29).** `thinking.rs` →
`params.rs` with `ReasoningLevel`; `/thinking` → `/reasoning`;
`default_thinking_level` → `default_reasoning_level`; `record_thinking_change`
→ `record_reasoning_change`. `AgentConfig.thinking_level` keeps its name in this
phase (only its type is `Option<ReasoningLevel>`) so the diff is a pure
rename; P1 folds it into `ModelParams`.
*Tests:* the existing `thinking_*` suites renamed; a root-listing test pinning
that `thinking` no longer answers; migration + optional-key tests (§A.5, below).

**P0 addendum — the setting became optional, and the old key auto-migrates.**
Two decisions taken during implementation, both superseding the original P0
note that an old key should error:

- `default_reasoning_level` is `Option<String>` in `SettingsStore` and is
  written **only when a level is recorded** — `/set … off` removes the key
  rather than pinning a literal that means the same thing. An untouched
  settings file therefore carries no reasoning key at all, and `value()` still
  reports `off` so the menu shows the right state. Same shape as the existing
  `default_persona` / `llm_compact_prompt` handling in the same `save()`.
- A pre-rename `default_thinking_level` is **folded into the new key** on load
  (`Config::parse` → `Config::migrate`, the single read path shared by
  `Config::load` and `SettingsStore::load`), and `save()` then drops the old
  spelling. The new key wins when both are present, so a file edited since the
  rename keeps its explicit value. Upgrading never silently resets the level.
- The session journal keeps `thinking_level_change` / `thinkingLevel`: a
  compatibility surface, not part of this rename.

**P1 — `ModelParams` + provider mapping.** **Done (2026-09-29).** `ModelParams`
on `AgentConfig` (replacing `reasoning_level: Option<String>`);
`params::additional_params`; `AgentBuilder::temperature`; `top_p` map with the
Gemini branch; effort remapping driven by a `ReasoningSupport` value; the
`params_changed` arm in the rebuild gate. *Tests:* the existing per-provider
mapping tables (unchanged — those payloads are provider contracts); remap cases
from the real payloads; rebuild-gate tests that a param change rebuilds once
and re-arms correctly.

**P1 addendum — three shapes the implementation settled.**
- **The rebuild gate is now a pure function.** `plan_rebuild(applied, target,
  last_attempt) -> Option<RebuildFingerprint>` was extracted from the
  convergence step, because both of its invariants ("only a real change
  rebuilds", "one attempt per distinct target") were untestable inline in the
  400-line loop. `model_changed` / `params_changed` / `persona_changed` stay as
  locals — the rebuild body and the error message still branch on them.
- **`mapping_opt` is gone.** `additional_params` takes the typed `ModelParams`;
  a string-typed entry point would be a way to bypass the composed producer and
  silently lose the `top_p` and remap handling. Two `ReasoningSupport` tie-breaks
  are pinned by tests: an exact rank match keeps the model's own spelling, and
  when a model publishes two spellings for one rank (`["max","xhigh"]`) the
  first published wins.
- **Temperature never appears in `additional_params`** — there is a test for it.
  It rides `AgentBuilder::temperature`, so rig spells it per provider and only
  one spelling of a given knob is ever in flight.
- `model_catalog::reasoning_support()` returns `unknown()` until P3, so every
  level still goes out verbatim: P1 is behavior-preserving for existing
  configs, and P3 is the single function that turns remapping on.

**P2 — `/temperature` + `/top_p` menu.** **Done (2026-09-29).** One
`NumberParamNode` serves both; what differs (name, range, effect wording) is
data on `NumberParam`, not two near-identical nodes. Leaf nodes directly under
the root, so each supplies its own root row from `data()`. *Tests:* parse and
range boundaries, write/clear, field isolation, prefill for set vs unset, the
header contract, and two engine-level walks through the real `build_menu`.

**P2 addendum — what the node does *not* do yet.** The `header()` has a marked
seam for the two catalog-derived facts (whether the model exposes this knob, and
the provider's published default) and deliberately renders neither until P3
supplies them. A guessed number is worse than silence, and guessing was already
shown to be wrong: the published defaults are {0.3, 0.7, 0.9, 1.0} / {0.95, 1.0},
not the 1.0/0.95 anyone would type from memory.

**P3 — catalog enrichment.** **Done (2026-09-29).** `ModelEntry` gains
`description`, `supports_temperature`, `supports_top_p`, `reasoning_levels`,
`default_temperature`, `default_top_p`; all parsed from the OpenRouter
payload. `model_catalog::reasoning_support()` now reads the cached catalog, so
the P1 remapper is live. *Tests:* a trimmed payload covering all six fields
(including that a blank blurb and a `null` default both degrade to "absent"),
the direct-entry path carrying the facts over, the composition from parsed
entry to remapper input, and `unknown` for a model the catalog cannot see.

**P3 addendum — three implementation decisions.**
- **`ModelEntry` derives `Default`** and the literals that genuinely have
  nothing to say (`static_fallback`, `liveness_prune`, test fixtures) end in
  `..Default::default()`. Six new fields on thirteen literals would have been
  a wall of `None`, and a wall of `None` hides the sites that *do* carry data.
- **`support_for_entry` is split out of `reasoning_support`** so the parse →
  support composition is testable without seeding the process-wide catalog
  cache, which every other catalog-reading test shares.
- **`/reasoning` row descriptions carry the landing** (`balanced reasoning →
  deepseek high`), which is where the disclosure is most useful — visible while
  scanning the rows, not only after opening the header. `header()` keeps the
  fuller table, since the rows have no room for it.
- Live-API check (40 most-popular models, 2026-09-29): `description` 40/40,
  `temperature` listed 32/40, effort lists 31/40, `default_temperature` 11/40.
  So the last one is genuinely sparse, and the header says "not published"
  rather than guessing in the two-thirds of cases.

**P4 — per-model storage + the editor.** `[models.*]` in `Config` and
`SettingsStore`; `put_model_settings`; `src/settings_editor.rs` with comments,
unknown-key suggestions, and collected errors. *Tests:* round-trip a
hand-written `settings.toml` with comments + `[recipes.*]` and assert both
survive; delete-a-key clears a stored value; error messages for typo / type /
range; the fragment prefill contains the catalog suggestions.

**P5 — `/model` action listing.** **Done (2026-09-29).** `ModelNode` gains
`activate` and `save` children; `header()` with details + current settings;
`edit()` wired to A.9 for active *and* inactive models; `save` hidden under
`/set default_model`. *Tests:* 13 new — the one-keystroke activate path, `save`
writing live params, `save` preserving stored metadata, the inactive-model
message, the `/set` row set, both header states, the no-tty Ctrl-E path, and
both halves of the two-phase commit.

**P5 addendum — three things the implementation settled.**
- **Ctrl-E and `save` both confirm, per the review decision.** §A.9's "the
  editor round-trip *is* the confirmation" was written when the confirm was a
  bare `bool`; the engine's own shipped precedent (compaction: editor, stash,
  *then* `Prepared`) guards irreversible writes, and both of these write
  `settings.toml`. So `edit()` stashes the record and arms
  `Prepared(Some((accept, cancel)))`; the engine calls `submit()` on the node
  that armed it, which for a listing is `ModelNode` itself — the stash is what
  tells `submit()` "write settings" rather than its other job, activating.
- **`save` owns the three params, not the whole table.** It starts from the
  stored record and overwrites only reasoning/temperature/top_p, so a metadata
  override set through Ctrl-E survives it.
- **Depth, not the path tail, is the "is a model drilled?" signal.** The engine
  threads ids rather than structure, and the last id at the *undrilled* level
  is the command's own name (`"model"`, `"default_model"`) — indistinguishable
  from a model id by looking at the tail. A first cut used that, and every
  catalog row grew the same two actions; a test caught it. Each action node now
  carries the number of segments above it (1 live, 2 under `/set`).

A real `edited settings` dialog is reachable for the first time: `ModelNode` is
the first *listing* node to use Ctrl-E, and the engine only added that routing
in the same upstream commit that introduced the dialog.

**P5.5 — per-model live-param seeding.** **Done (2026-09-30).** Found by
report: `save` wrote `[models.*].reasoning` correctly, but a new session did not
start from it. The record had **no reader outside the menu layer** — the header,
the Ctrl-E prefill and `save` were the only three things that read it. The
seeding that consumes it was specified in A.6/A.7 but never assigned a phase;
P1 delivered the `ModelParams` type and the rebuild gate without it.

`State.model_param_map: HashMap<String, ModelParams>`, load-or-seed on
activation, per field: `model_param_map[id]` → `stored[id].f` → global (reasoning
only) → built-in. *Tests:* 9 — the reported case, the global fallback, per-field
seeding, a→b→a restoring runtime values, temperature not leaking across a
switch, a record edited after a visit not displacing live values, the
map/live invariant, an empty id, and a save-then-restart round trip through the
menu.

**P5.5 addendum — the a→b→a hole, and a second defect it uncovered.** The
seeding rule has two clauses and the obvious implementation only has one: seed
from the record, and the map is a *cache that goes stale*. `model_param_map`
is only a correct memory of "what this model is running with" if it is written
through on **every** live param change, not just at activation — otherwise a
`/reasoning high` after the seed is forgotten by the next a→b→a. Hence the
single-writer helper.

Same missing code also meant **params carried across model switches**: set
`temperature = 0.2` on one model, switch, and the next model ran at `0.2`
instead of `None`. A.6 calls that out explicitly; there was a test for neither.

Two placement decisions worth recording. Seeding runs on **activation** (the
action node, and startup) rather than in the loop, because the loop snapshots
its target at `agent.rs:3800` and compares at `3817` — in the loop, seeding
lands either before the snapshot (model not yet known) or after it (stale),
costing two rebuilds with the outgoing model's params in between. And it is not
in `set_model`, because `/set default_model` shares that writer but is not an
activation. `settings.rs:401` (`apply_to_config`) would also be a bug if ever
wired into the live path — it sets `cfg.params.reasoning` from the global,
stomping the per-model value. It is test-only today; noted in a comment.

**P5.6 — the records reached the store at all.** **Done (2026-09-30).** Found
by report, and it made the whole per-model feature dead at startup. P5.5's
seeding read the record correctly; there was simply never a record in memory.
`main.rs` built the startup store with `SettingsStore::from_agent_config`, which
hardcoded `models: HashMap::new()` under a comment claiming `main` would thread
the records in — it never did, and `AgentConfig` has no field for them, so the
constructor could not have recovered them on its own. One symptom, three
consequences:

1. `activate_model_params` found nothing and fell back to the global, so
   `/reasoning` reported `low` while the file said `max`.
2. The Ctrl-E prefill was handed a default record, and `render_fragment`
   renders an unset key as a commented suggestion — so the editor showed a
   fully-commented example instead of `reasoning = "max"`.
3. The damaging one: `apply_model_settings` *replaces* the table, so a Ctrl-E
   commit against an empty record dropped the keys the file already had.

`from_agent_config` now takes the records as a parameter, and `main` passes
`config.models`. *Tests:* 5, all deliberately going through the **startup
constructor** or a **real settings file** — a test that hand-builds a store with
`models` populated passes happily and proves nothing, which is exactly how this
hid. One test covers the whole chain from a written `settings.toml` through
`Config::load` → `from_agent_config` → `AppState::new` → seeding.

**P6 — per-turn overrides.** `turn_overrides::split`; `PicoAgent::prompt`
signature change; the `Submit`-arm wiring; warnings. *Tests:* parser table
(strip / no-strip / partial block / invalid value / duplicate / only-overrides);
history raw vs session stripped, end-to-end through `run_interactive`; a retry
re-applying the override; a steering dispatch not re-applying it.

**Risks**

- *Catalog cache growth*: `description` is ~200 bytes × 460 entries (~90 KB) —
  acceptable, but it is the first string payload in a cache that was
  deliberately numeric. If it shows up in a profile, keep it behind a
  `full` flag rather than trimming the model list.
- *Provider quirks*: Anthropic caps `temperature` at 1.0 and discourages setting
  it alongside thinking; some reasoning models reject `temperature != 1`
  entirely. We accept-and-warn rather than reject — provider-level validation
  would need a per-model capability table we do not have.
- *Two live stores* (`config.params` applied vs `AppState.params` target) can
  drift on a failed rebuild; the rebuild gate's `target_fp` re-arm is the only
  guard. Same shape as the existing model/thinking gate — no new failure mode.
- *A per-model table in a hand-edited `settings.toml`* makes
  `deny_unknown_fields` a startup-failure surface. That is the intent (a typo
  should be loud), but it is a behavior change for users who hand-edit.

**Open question (small, non-blocking)**

1. `/temperature 1.5` on Anthropic — accept with a header warning, or reject?
   Currently: accept + warn.

---

## Decisions log

Settled in design review (2026-09-29), in the order they were taken:

1. **Frontend stays simple.** `/reasoning` keeps a static five-level list;
   effort remapping to what a model accepts is a backend concern, surfaced only
   in the model `header()`. No per-model code, no per-model menu.
2. **Hard rename** `/thinking` → `/reasoning` throughout, including
   `default_thinking_level` → `default_reasoning_level`. The session journal's
   on-disk names are exempt — they are a compatibility surface.
3. **`supports_reasoning` / `reasoning_levels`.** The level list is stored
   verbatim so remapping is data-driven.
4. **Precedence, per field**: session live memory → stored per-model → global
   `/set` (reasoning only) → built-in. A → b → A restores A's live values.
5. **Ctrl-E edits stored settings only** — never live. Editing a model that has
   not been activated this session is supported; the values are picked up on
   first activation.
6. **Live changes never touch disk** (no auto-save); only `save` and Ctrl-E
   write.
7. **Per-turn overrides:** strip anything syntactically valid, warn on a
   semantically invalid value and continue. Reasoning always remaps, never
   fails.
8. **History/session:** raw text in prompt history, stripped text in the
   session.
9. **A prompt that is only overrides is refused** (warn, no turn) — the stripped
   prompt must be non-empty.
10. **Metadata overrides win over the catalog where set**, fill the gaps
    otherwise.
11. **`save` writes what differs from the built-in default**: `reasoning`
    always (it is seeded from `/set`), `temperature`/`top_p` only when set.
12. **No per-model settings sub-listing and no new global `/set` keys.** The
    model menu is `activate` / `save` plus Ctrl-E; live edits go through the
    global `/reasoning`, `/temperature`, `/top_p` and act on the active model.
13. **No number completion.** Prefill + a descriptive `header()` carry the
    discovery instead; the prefill is *empty* when the value is unset (the
    default is `None`, not a number), and an empty submit clears the setting.
14. **Track shape A for all three tracks** (single `ModelParams` + one
    `additional_params` producer; a `ModelSettings` record plus a reusable TOML
    editor; a pure `split()` applied per request).
15. **Reasoning effort mappings are shown, not silently applied.** A static
    per-provider table (A.4.1) carries what each service actually does with a
    request, it is consulted before the step-down ladder so a documented
    request is never downgraded, and `/reasoning`'s header discloses where
    every preset lands. The user picks working backwards from the effort they
    want; the frontend never learns provider vocabularies.
16. **Provider defaults are shown, never sent.** The catalog's
    `default_parameters` feeds the `header()` only. No hardcoded "typical"
    1.0 / 0.95 — the measured spread is {0.3, 0.7, 0.9, 1.0} / {0.95, 1.0}
    and only a third of models publish one. Sending a known default would put
    an explicit `temperature` on the wire for every model and break the
    reasoning models that reject it.
17. **`default_reasoning_level` is optional**, and the pre-rename
    `default_thinking_level` auto-migrates into it on load (P0 addendum).
    Chosen over failing on the old key: a rename that makes the tool refuse to
    start is a different class of breakage from a command that changed name.
18. **The reasoning disclosure lives in the row descriptions, not only the
    header.** `balanced reasoning → deepseek high` sits on the row the user is
    looking at. The header keeps the full table, which does not fit in a row.
19. **Absent catalog data is shown as absent.** "provider default: not
    published" and "this model does not list top_p in supported_parameters" are
    real answers — the second for 8 of the 40 most-popular models, the first
    for 29 of 40.
