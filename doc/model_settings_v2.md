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
recognized however the provider spells it:

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

## A.6 Runtime state

`State` (`app_state.rs:138`) gains two fields, alongside the existing runtime
target `config: AgentConfig`:

```rust
/// Live params of the ACTIVE model (mirrors `config.params`; the target
/// the menu writes, the loop converges `config` onto it).
pub(crate) params: ModelParams,
/// Per-model live memory, session-scoped: id -> last live params. Makes
/// a → b → a restore a's settings.
pub(crate) model_live: HashMap<String, ModelParams>,
```

with the app-state lock rules honored (one acquisition per public method, no
file IO under the guard — clone, drop, write):

| method | role |
|---|---|
| `model_params() -> ModelParams` | read the live params |
| `set_param(k, v)` / `set_reasoning(l)` | one live field (menu writes) |
| `model_live(id) -> Option<ModelParams>` | has this model been visited? |
| `activate_model_params(id)` | first-visit seeding (§A.7) |
| `model_settings(id) -> Option<ModelSettings>` | stored record (editor, header) |
| `put_model_settings(id, &ModelSettings) -> Result<(), String>` | validate + apply in-lock, save outside |

**Seeding rule** (`activate_model_params`, called by the loop on `model_changed`
*before* the rebuild comparison) — **per field**, never per record:

```
model_live[id] exists?          → use it, done (a → b → a)
else for each field f in {reasoning, temperature, top_p}:
    stored[id].f                → use it
    else global /set default    → reasoning only: default_reasoning_level
    else built-in               → reasoning: off;  temperature/top_p: None
    and insert the result into model_live[id]
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

1. `model_changed` → `app_state.activate_model_params(&target.qualified_model_id())`
   (seeding, §A.6) **before** the comparison, so the rebuild sees final values.
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
  meaningless.
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

**P0 — rename (no behavior change).** `thinking.rs` → `params.rs` with
`ReasoningLevel`; `/thinking` → `/reasoning`; `default_thinking_level` →
`default_reasoning_level`; `record_thinking_change` →
`record_reasoning_change`. `AgentConfig.thinking_level` keeps its name in this
phase (only its type is `Option<ReasoningLevel>`) so the diff is a pure
rename; P1 folds it into `ModelParams`.
*Tests:* the existing `thinking_*` suites renamed; one new test pinning that an
old `settings.toml` with `default_thinking_level` now errors (hard break, but
it must be a *clear* error, not silence).

**P1 — `ModelParams` + provider mapping.** `ModelParams` on `AgentConfig`;
`params::additional_params`; `AgentBuilder::temperature`; `top_p` map with the
Gemini branch; effort remapping driven by a `ReasoningSupport` value; the
`params_changed` arm in the rebuild gate. *Tests:* mapping table tests per
provider (mirroring `thinking.rs:117-203`); remap cases from the real payloads
(`["max","high","low"]`, `["high","low","none"]`, `xhigh`); rebuild-gate tests
that a `temperature` change rebuilds once and re-arms correctly.

**P2 — `/temperature` + `/top_p` menu.** `NumberParamNode` with prefill, header
hints, range validation; `/reasoning` rewired to it. *Tests:* submit/range/error
paths through `eval_prompt`; `drill_rewrite` prefill for a set value, and
*empty* for an unset one; empty submit clears back to `None`.

**P3 — catalog enrichment.** `ModelEntry` fields; parse `description`,
`supported_parameters` → two booleans, `reasoning.supported_efforts`, and
`default_parameters` → the two display-only defaults; extend `model_row`
fields/columns if useful. *Tests:* `parse_openrouter` with a trimmed
fixture asserting the derived booleans, the level list, and that a
`default_parameters` of `{"temperature": null}` yields `default_temperature:
None` (not `Some(0)`); the direct-entry path in `extrapolate` copies the new
fields onto the derived `ModelEntry`; dedupe keeps them.

**P4 — per-model storage + the editor.** `[models.*]` in `Config` and
`SettingsStore`; `put_model_settings`; `src/settings_editor.rs` with comments,
unknown-key suggestions, and collected errors. *Tests:* round-trip a
hand-written `settings.toml` with comments + `[recipes.*]` and assert both
survive; delete-a-key clears a stored value; error messages for typo / type /
range; the fragment prefill contains the catalog suggestions.

**P5 — `/model` action listing.** `ModelNode` children (`activate`, `save`);
`header()` with details + current settings; `edit()` wired to A.9 for active
*and* inactive models; `save` hidden under `/set default_model`. *Tests:*
traversal test that `/model <id>` + Enter still activates; `save` writes
`reasoning` always and `temperature`/`top_p` only when set; a full-menu
traversal for each action; header content.

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
15. **Provider defaults are shown, never sent.** The catalog's
    `default_parameters` feeds the `header()` only. No hardcoded "typical"
    1.0 / 0.95 — the measured spread is {0.3, 0.7, 0.9, 1.0} / {0.95, 1.0}
    and only a third of models publish one. Sending a known default would put
    an explicit `temperature` on the wire for every model and break the
    reasoning models that reject it.
