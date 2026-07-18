# `/settings` sub-menu — Implementation Plan

Extend the `/` menu with a `/settings` sub-menu that lists settings as a
`Setting | Value | Description` table, lets the user fuzzy-filter on the
setting key, and select / tab-complete a setting (same model as `/model`).
Selecting a setting drops its **current value** into the input box for free
editing; on `Enter` the value is validated, saved to the settings file, and the
list re-renders with the updated value and the prior filter string restored.
`Esc` cancels the edit and restores the prior input without saving.

This plan mirrors the existing `/model` + `/thinking` inline-overlay design in
`src/repl.rs` and the static `slash.rs` registry. It is split into:

- **Part A — Technical design** (what/why).
- **Part B — Implementation plan** (file changes, staged checklist + tests,
  risks, and a decisions/ open-questions log).

> **Scope split (per request).** *Phase 1* wires the menu end-to-end and
> persists the new value to the settings file, but does **not** guarantee the
> running session picks it up live. *Phase 2* live-applies whatever makes sense
> (model/persona/thinking/limits/security) to the current session.

> **Status (2026-07-17):** *Phase 1 implemented and merged-build green.
> The config format was hard-switched YAML→TOML (no YAML fallback). `/settings`
> opens a `Setting | Value | Description` table, fuzzy-filters on key +
> description, and selecting a row drops that setting's current value into the
> input box for free editing. `Enter` validates + persists via `toml_edit`
> (`DocumentMut` — comments and `[recipes.*]` preserved); `Esc` cancels; the
> prior input buffer (including the filter string) is restored and the list
> re-renders with the updated value. Phase 2 (live-apply to the running
> session) and Stage 4 (value-autocomplete pop-ups) are **not** done yet.
> `tool_call_limit` CLI flag is now `Option<usize>` so a file value takes
> effect when the flag is omitted.

---

# Part A — Technical design

## A.1 Current state (what already exists)

- **Overlay engine** (`src/repl.rs::read_input`): when the buffer starts with
  `/`, `read_input` computes a `kind` (1=command list, 2=`/model`, 3=`/thinking`,
  4=`/help`) and renders an inline pop-up **below the input line** (≤ 2/3 of
  terminal height) using `Table` + `overlay_lines`. Filter text is the token
  after the command word; selection is `sel: usize`; `Tab`/`Enter` commit.
- **Pure helpers** (unit-tested, TTY-free): `model_overlay_entries(ctx, filter)`
  and `thinking_overlay_entries(filter)` build + fuzzy-filter the rows. The
  key-dispatch wiring in `read_input` is thin and calls these.
- **`slash.rs` registry**: `SlashCommand { name, help, kind }` (`SlashKind ∈
  {Help, Thinking, Model, Quit}`). `filter_commands` drives the bare `/` list
  and `Tab` completion (`/mod` → `/model `). `/settings` must be added here so
  it appears in the `/` list and completes.
- **Settings today are NOT persisted.** `Config` (`src/config.rs`) currently
  loads `picocode.yaml`/`picocode.yml` (YAML) and holds only `agent_prompt`,
  `tool_config`, `recipes`. The five target settings live **only** in
  `AgentConfig` (`provider`, `model`, `yolo`, `tool_call_limit`, `persona_name`,
  `thinking_level`) and CLI args; there is **no `Config::save`**. **Decision
  (2026-07-17):** switch the whole config format from YAML to TOML (hard break,
  no YAML fallback — this is an experimental personal fork). The five settings
  become `#[serde(default)]` fields on `Config`, persisted to the **same**
  `picocode.toml` that holds `recipes` (split later if desired). Writes go
  through `toml_edit` so user comments + recipes survive. See §A.5.
- **Live application today** is the `SetModel`/`SetThinking` path in
  `agent.rs::run_interactive`: it clones `config`, calls `create_agent`, and
  rebuilds the `Box<dyn PicoAgent>`. Phase 2 reuses this rebuild mechanism.

## A.2 Settings model

A single, centralized, **extendable** registry so the table, filtering,
validation, and value-autocomplete all share one source of truth.

```rust
// src/settings.rs (new)
pub enum SettingKey {
    DefaultModel,        // provider:model
    SecurityMode,        // yolo | confirm
    ToolCallLimit,       // usize (>0)
    DefaultPersona,      // built-in name or file path
    DefaultThinking,     // off | low | med | high
}

pub enum ValueKind {
    Model,                       // provider:model, completed from catalog
    Enum(&'static [&'static str]), // fixed options (SecurityMode, DefaultThinking)
    Int,                         // ToolCallLimit
    Persona,                     // completed from persona::PERSONAS (+ files)
    FreeText,
}

pub struct SettingMeta {
    pub key: SettingKey,
    pub name: &'static str,      // filterable key string, e.g. "default_model"
    pub description: &'static str,
    pub kind: ValueKind,
}

pub static SETTINGS: &[SettingMeta] = &[
    SettingMeta { key: SettingKey::DefaultModel,  name: "default_model",
        description: "Default model as provider:model (used for new sessions)",
        kind: ValueKind::Model },
    SettingMeta { key: SettingKey::SecurityMode,  name: "security_mode",
        description: "Tool execution policy: yolo (no prompts) or confirm (ask)",
        kind: ValueKind::Enum(&["yolo", "confirm"]) },
    SettingMeta { key: SettingKey::ToolCallLimit, name: "tool_call_limit",
        description: "Max tool calls per prompt/turn",
        kind: ValueKind::Int },
    SettingMeta { key: SettingKey::DefaultPersona, name: "default_persona",
        description: "Persona for new sessions (built-in name or file)",
        kind: ValueKind::Persona },
    SettingMeta { key: SettingKey::DefaultThinking, name: "default_thinking_level",
        description: "Default thinking level: off | low | med | high",
        kind: ValueKind::Enum(&["off", "low", "med", "high"]) },
];
```

The in-memory store holds the current values and the file path:

```rust
pub struct SettingsStore {
    pub default_model: String,            // "provider:model"
    pub security_mode: String,            // "yolo" | "confirm"
    pub tool_call_limit: usize,
    pub default_persona: Option<String>,  // None = no persona
    pub default_thinking_level: String,   // "off" default
    pub path: PathBuf,                    // where to persist
}
```

`SettingsStore` methods (all pure/testable): `load(path)`, `save()`,
`value(key) -> String`, `set_value(key, &str) -> Result<(), String>` (parse +
validate + persist in one call — see §A.4), and `overlay_rows(filter) ->
Vec<(SettingMeta, String /*value*/)>` that fuzzy-filters `SETTINGS` by
`name + description` and returns each row's current value. This mirrors
`model_overlay_entries`.

## A.3 Overlay interaction (the core UX)

Two sub-states inside `read_input`, tracked by two new locals:

```rust
let mut edit_setting: Option<SettingKey> = None; // None = list mode, Some = editing value
let mut edit_prev_input: String = String::new(); // exact buffer (incl. filter) to restore
let mut last_kind: u8 = 0; // 0=None,1=Cmd,2=Model,3=Thinking,4=Help,5=Settings
```

### List mode (`/settings <filter>`, `edit_setting == None`)

- Buffer starts with `/settings ` (or `/setting `, `/s `). `kind = 5`.
- `cur_word = "settings"`, `cur_filter = <text after the command word>`.
- Build rows via `SettingsStore::overlay_rows(cur_filter)` → a `Table` with
  headers `["Setting", "Value", "Description"]` (reuse `table.rs`; give
  `Description` the lowest priority + ellipsis so long text truncates; `Value`
  gets a moderate fixed/min width; `Setting` gets the highest priority so keys
  stay readable). Each row shows `name | current_value | description`.
- Navigation identical to `/model`: `Up`/`Down`/`PgUp`/`PgDn` move `sel`;
  `Tab`/`Enter` **select** the highlighted setting (do **not** bubble up).
- `Esc` returns the buffer to `/settings` (mirrors `/model` Esc → `/model`).

### Edit mode (after selecting a setting, `edit_setting == Some(key)`)

- On selection: capture `edit_prev_input = editor.text()` (**exact**, e.g.
  `/settings app`), then `editor.set(store.value(key))` so the input box holds
  the current value (`anthropic:claude-3-5-sonnet-20241022`, `yolo`, `50`,
  `architect`, `off`), and set `edit_setting = Some(key)`. `kind` becomes `0`
  (no overlay) naturally because the buffer no longer starts with `/`, so the
  input box is shown cleanly for editing.
- The value is **free text** in Phase 1 (user edits directly). For `Model`,
  `Enum`, and `Persona` kinds, an optional value-autocomplete pop-up (see
  §A.6) can surface candidates to pick from — Phase 1 may ship free-edit only.
- `Enter` (no Shift): call `store.set_value(key, editor.text())`:
  - **Ok** → value persisted to file; restore `editor.set(&edit_prev_input)`,
    `edit_setting = None`; the loop re-renders the **list** from `/settings app`
    with the **updated value** shown. (Stays in `read_input` — no `InputAction`
    returned.)
  - **Err(msg)** → do **not** save, do **not** restore; show `msg` as a one-line
    error below the input (reuse the pop-up area) and keep the cursor in the
    edit box so the user can fix the value. `Esc` still cancels.
- `Esc`: restore `editor.set(&edit_prev_input)`, `edit_setting = None`,
  re-render list (no save).

This satisfies the requirement: value shown in input box → editable → on return
saved → previous input + filter restored → list re-displays with the updated
value. The "previous filter dialog state" is exactly `edit_prev_input`.

### Dispatch wiring

The `read_input` key handler already branches on `kind`. Add, **before** the
existing `if kind != 0` block, a guard:

```rust
if let Some(key) = edit_setting {
    match key.code {
        Enter (no Shift) => { /* set_value; on Ok restore+re-render; on Err show msg */ }
        Esc => { /* restore edit_prev_input; edit_setting = None */ }
        _ => { /* normal editing via edit_key */ }
    }
    continue;
}
```

This cleanly isolates edit-mode handling from list-mode handling.

## A.4 Validation (`set_value`)

| Key | Parse / validate | Error example |
|---|---|---|
| `DefaultModel` | Split on first `:` → `(provider, model)`; both non-empty; provider must be a known key in `model_catalog::required_env_vars` (unknown allowed but warns) | `"expected provider:model"` |
| `SecurityMode` | Must be `yolo` or `confirm` (also accept `ask`/`safe` aliases → normalize to `confirm`) | `"security_mode must be yolo or confirm"` |
| `ToolCallLimit` | `str::parse::<usize>()` and `> 0` | `"tool_call_limit must be a positive integer"` |
| `DefaultPersona` | Empty/None → no persona; else must be a built-in `persona::PERSONAS` name **or** an existing file path | `"unknown persona 'x' (not built-in and not a file)"` |
| `DefaultThinking` | `thinking::ThinkingLevel::from_str` | `"unknown thinking level: ..."` |

`set_value` writes through to the file immediately (§A.5) so the change is
durable even if the session later crashes.

## A.5 Persistence — the config file (TOML, hard switch)

**Decision (2026-07-17):** switch the entire config format to **TOML** (hard
break, no YAML fallback — experimental personal fork). The five settings are
added as `#[serde(default)]` fields on `Config`, so they live in the **same**
`picocode.toml` as `recipes`/`agent_prompt`/`tool_config` (can be split into a
separate file later). This is the cleanest resolution of the earlier Q1: TOML +
`toml_edit` preserves user comments + recipes on every `/settings` write, which
YAML (no round-trip comment-preserving library in Rust) cannot do.

**Reading.** `Config::load` reads `picocode.toml` (single default name; the
`--config <path>` override still works). Deserialize via `toml_edit::de::from_str`
(or `toml::from_str`) into the `Config` struct — the existing serde derives work
unchanged. The five new fields are `#[serde(default)]` so an old/minimal file
still parses.

**Writing (comment-preserving).** `SettingsStore::save()` does **not**
round-trip the whole file through serde (that would drop comments + recipes
structure). Instead it:
1. Parses the existing `picocode.toml` with `toml_edit::DocumentMut::from_str`
   (if the file is absent, starts from an empty `DocumentMut` + a header comment
   block).
2. Sets only the five managed keys (`default_model`, `security_mode`,
   `tool_call_limit`, `default_persona`, `default_thinking_level`) via
   `doc["key"] = toml_edit::value(...)`.
3. Re-emits with `doc.to_string()` — every other key (incl. `[recipes.*]` and
   their comments) is preserved byte-for-byte.

This keeps recipe editing and `/settings` writes in one file with zero comment
loss.

**Precedence (effective value resolved in `main.rs`):**
`CLI flag` > `settings file (Config)` > `built-in default` (`default_model(provider)`,
`limit:50`, `security: confirm`, `persona: None`, `thinking: off`).

**Dependencies:** drop `serde_yaml`; add `toml_edit = { version = "0.25",
features = ["serde"] }` (already present transitively at 0.25.13). `serde`
already a dep. No separate `toml` crate required (use `toml_edit::de` for
deserialize). `PicocodeError::Yaml` is replaced by a `Toml(String)` variant
(mapped from both `toml_edit`/`toml` parse errors).

## A.6 Value autocomplete (optional, mostly Phase 2 polish)

For `Model`/`Enum`/`Persona` kinds, while in **edit mode** the user could press
`Tab` to open a secondary candidate pop-up (reuse the `/model` catalog list,
`/thinking` level list, or `persona::PERSONAS` list). Selecting a candidate
replaces the edit-box text. This is a nice-to-have; Phase 1 may ship free-text
edit only and add candidate pop-ups in Phase 2. The `ValueKind` enum (§A.2) is
the hook point so this is a localized addition later.

## A.7 Live application (Phase 2)

Phase 1 persists to the file only. Phase 2 makes sense to also affect the
**running session**. The clean path reuses `run_interactive`'s existing agent
rebuild:

- Change `read_input` to also take `&mut AgentConfig` (or a small
  `&mut LiveState { config: &mut AgentConfig, dirty: &mut bool }`) alongside
  `&mut SettingsStore`.
- On a successful `set_value`, in addition to persisting, update the matching
  `AgentConfig` field(s):
  - `DefaultModel` → `config.provider`/`config.model` (split `provider:model`).
  - `SecurityMode` → `config.yolo = (mode == "yolo")`.
  - `ToolCallLimit` → `config.tool_call_limit`.
  - `DefaultPersona` → `config.persona_name` + reload `persona_prompt` via
    `persona::get_persona`.
  - `DefaultThinking` → `config.thinking_level`.
  - Set `dirty = true` for any field the running agent reads at build time.
- Because `read_input` stays in the overlay on save (does **not** return), the
  rebuild is deferred: `run_interactive`, **just before each `Submit` prompt**,
  checks `dirty` and, if set, calls `create_agent(config.clone())` (mirroring the
  `SetModel`/`SetThinking` branches) and clears `dirty`. This avoids pushing the
  generic agent-rebuild into `repl.rs` and keeps the "stay in overlay"
  requirement intact. The "applies on next prompt" latency is acceptable for
  *default* settings.
- `ReplCtx { provider, model, recent }` is rebuilt each loop from `config`, so
  the `/model` overlay and status line reflect the new model automatically.

## A.8 `slash.rs` + help wiring

- Add `SlashKind::Settings` and a registry entry
  `SlashCommand { name: "/settings", help: "View and edit settings (model, security, limits, persona, thinking)", kind: Settings }`.
- In `read_input`'s command dispatch, add a branch for
  `"settings" | "setting" | "s"` (parallel to `"model" | "models" | "m"`),
  setting `kind = 5`.
- `Tab` completion from the `/` list: when the highlighted command is
  `Settings`, `editor.set("/settings ")`.
- `render_help` and the startup hint currently reference `/menu` (a known TODO,
  see `phase_1_plan.md` §3.7). Update them to mention `/settings` (or alias
  `/menu` → `/settings`).

---

# Part B — Implementation plan

## B.1 Proposed file changes

| File | Change |
|---|---|
| `src/settings.rs` | **New.** `SettingKey`, `ValueKind`, `SettingMeta`, `SETTINGS` registry, `SettingsStore` (`load`/`save`/`value`/`set_value`/`overlay_rows`), parse + validate per key. Re-export from `lib.rs`. |
| `Cargo.toml` | Drop `serde_yaml`; add `toml_edit = { version = "0.25", features = ["serde"] }`. |
| `src/config.rs` | **YAML→TOML hard switch.** `Config::load` reads `picocode.toml` (single default; `--config` override unchanged). Add `#[serde(default)]` fields `default_model`, `security_mode`, `tool_call_limit`, `default_persona`, `default_thinking_level`. No `Config::save` (settings writes go through `SettingsStore`/`toml_edit`). |
| `src/slash.rs` | Add `SlashKind::Settings`; registry entry `/settings`; `filter_commands`/completion already picks it up; update `parse_slash` not required (handled inline in `repl.rs`). Bump registry count test (currently `4`). |
| `src/repl.rs` | `read_input` gains `&mut SettingsStore` (+ `&mut LiveState` for Phase 2). Add `kind = 5` branch, `edit_setting`/`edit_prev_input` locals, the edit-mode key guard, and `settings_overlay_entries` pure helper. Update `render_help`/hint references. |
| `src/agent.rs` | `run_interactive`: build `SettingsStore` from `Config`; pass `&mut` into `read_input`; add `dirty` flag; on `Submit`, rebuild agent if dirty (Phase 2). |
| `src/main.rs` | Switch config load to `picocode.toml`; resolve setting precedence (CLI > file > built-in) for the 5 settings; update help strings ("picocode.yaml" → "picocode.toml"); pass `Config` into `run_interactive`. |
| `src/lib.rs` | `pub mod settings;`, re-export `SettingsStore`, `SettingKey`. Replace `PicocodeError::Yaml` with `Toml(String)` (mapped from `toml_edit`/`toml` parse errors). |
| `picocode.toml.example` | New (rename `picocode.yaml.example`): TOML syntax for `agent_prompt`, `tool_config`, `recipes`, and the 5 settings keys (see §B.3). |

## B.2 Staged checklist

### Stage 1 — `settings.rs` (pure, TTY-free)
- [ ] `SettingKey`/`ValueKind`/`SettingMeta`/`SETTINGS` registry (5 entries,
      exact descriptions from §A.2).
- [ ] `SettingsStore::load(path)` / `save()` round-trip (temp-file test).
- [ ] `value(key)` returns current string.
- [ ] `set_value(key, &str) -> Result<(), String>` with the §A.4 validation;
      on success persists via `save()`.
- [ ] `overlay_rows(filter)` fuzzy-filters on `name + description`,
      returns `(meta, value)` rows.
- [ ] **Unit tests** (`settings.rs`): each key's parse/validate (valid + invalid
      cases incl. `provider:model` split, enum normalization `ask`→`confirm`,
      `tool_call_limit` zero/non-numeric, unknown persona, thinking parse);
      `overlay_rows` empty-query returns all 5 in order; subsequence filter;
      `load`/`save` round-trip preserves values; `save` via `toml_edit` preserves
      the *other* keys (e.g. a `[recipes.*]` block) and their comments in the
      same `picocode.toml`.

### Stage 2 — `config.rs` YAML→TOML switch
- [ ] `Config::load` reads `picocode.toml` (drop `.yaml`/`.yml`); deserialize via
      `toml_edit::de::from_str`. Keep `load(None)` cwd search.
- [ ] Add 5 `#[serde(default)]` fields to `Config`; `#[derive]` unchanged.
- [ ] `PicocodeError`: replace `Yaml` variant with `Toml(String)`; `From`
      impls for `toml_edit`/`toml` parse errors.
- [ ] **Unit tests**: `load` parses a TOML file with `recipes` + settings;
      missing/old file still parses via `#[serde(default)]`; built-in defaults
      via `Default`. (No `Config::save` — writes flow through `SettingsStore`.)

### Stage 3 — `slash.rs` + `repl.rs` overlay (Phase 1: persist only)
- [ ] `slash.rs`: `SlashKind::Settings` + `/settings` registry entry; update
      count assertions (`registry().len() == 5`, `empty_query_returns_all == 5`).
- [ ] `repl.rs::read_input`: add `&mut SettingsStore` param; `kind = 5` branch
      building the `Setting|Value|Description` `Table` from
      `settings_overlay_entries`; `Up/Down/PgUp/PgDn`/`Tab`/`Enter` select;
      `Esc` → `/settings`.
- [ ] New locals `edit_setting`/`edit_prev_input`; edit-mode key guard
      (commit `set_value`; on Ok restore `edit_prev_input` + re-render list; on
      Err show inline error; `Esc` cancels). Pure helper
      `settings_overlay_entries(store, filter)` (mirrors
      `model_overlay_entries`) for unit testing.
- [ ] `run_interactive`: construct `SettingsStore`, pass `&mut`; save happens
      inside `read_input` (no `InputAction` needed for Phase 1).
- [ ] Update `render_help` + startup hint to mention `/settings`.
- [ ] **Unit tests** (`repl.rs`): `settings_overlay_entries` returns 5 rows,
      filters by key, includes current values; the select→edit→commit→restore
      transition can be exercised via a small pure `SettingsOverlayState` helper
      if extracted (recommended: extract the transition logic into a pure struct
      with `select()`/`commit(value)`/`cancel()` so it is TTY-free testable,
      mirroring `model_overlay_entries`).

### Stage 4 — Value autocomplete (optional Phase 1 / Phase 2 polish)
- [ ] For `Model`/`Enum`/`Persona` kinds, `Tab` in edit mode opens a candidate
      pop-up (catalog / `/thinking` levels / `persona::PERSONAS`); selection
      replaces the edit-box text. Localized to `ValueKind` dispatch.

### Stage 5 — Phase 2 live-apply
- [ ] `read_input` takes `&mut LiveState { config, dirty }`.
- [ ] On successful `set_value`, update `AgentConfig` fields + `dirty = true`.
- [ ] `run_interactive`: before each `Submit` prompt, if `dirty` rebuild agent
      via `create_agent(config.clone())` (mirror `SetModel`/`SetThinking`) and
      clear `dirty`.
- [ ] **Manual/integration test**: change `default_model` via `/settings`, send
      a prompt, assert the new provider/model is used (status line / response
      origin).

### Stage 6 — Integration (pty) test
- [ ] Extend `tests/render_scroll.rs` pattern (portable-pty + vt100): open
      `/settings`, type `mod` to filter, select `default_model`, edit value,
      `Enter`; assert (a) the list re-renders with the new value, (b) the
      settings file on disk contains it.

## B.3 `SettingsStore` ↔ `Config` mapping (concrete)

```toml
# picocode.toml (additions; all optional, defaulted)
# Managed by the /settings menu — safe to edit by hand.
default_model = "anthropic:claude-3-5-sonnet-20241022"
security_mode = "confirm"          # yolo | confirm
tool_call_limit = 50
default_persona = "architect"      # omit for no persona
default_thinking_level = "off"     # off | low | med | high
```

`SettingsStore` reads these into its typed fields (via `Config`/`toml_edit::de`);
`set_value` writes them back through `toml_edit::DocumentMut`, preserving the
rest of the file. `default_persona` omitted ⇄ `None`. Full file also holds
`agent_prompt`, `tool_config`, and `[recipes.*]` (see `picocode.toml.example`).

## B.4 Risks & open questions

- **Q1 — Config format / file location.** **RESOLVED (2026-07-17):** hard
  switch the whole config to **TOML** (no YAML fallback); settings + `recipes`
  live in the same `picocode.toml`; writes via `toml_edit` preserve comments +
  recipes. Supersedes both the "extend YAML" and "dedicated YAML file" options.
  (Splitting settings into a separate file later remains possible.)
- **Q2 — "Stay in overlay" vs live-apply rebuild.** Resolved in §A.7: save +
  in-memory update happen inside `read_input`; the agent rebuild is deferred to
  the next `Submit` via a `dirty` flag (no new `InputAction`, no generic rebuild
  inside `repl.rs`).
- **Q3 — `default_model` vs CLI / `default_model(provider)`.** Precedence is
  CLI > file > built-in (§A.5). `/model` (session switch) currently does **not**
  persist; optionally Phase 2 could also write `default_model` — out of scope
  unless desired.
- **Q4 — Security-mode vocabulary.** Using `yolo`/`confirm` (with `ask`/`safe`
  aliases normalized to `confirm`). Confirm canonical strings with user; they map
  directly to `AgentConfig::yolo`.
- **Q5 — `default_persona` empty semantics.** `None`/empty/omitted = no persona
  (matches `main.rs` `persona_name: Option<String>`). Validate against
  `persona::PERSONAS` names or an existing file.
- **Q6 — Width budget for the table.** `Description` is long; rely on
  `Table::with_priorities` + `set_ellipsis` (already used by `/model`) so keys
  and values stay visible and the description truncates. Verify on an 80-col
  terminal in Stage 3.
- **Q7 — Error display in edit mode.** Reuse the pop-up region for a one-line
  red error; ensure it does not clobber the input row (consistent with §3.7
  invariant — only rows at/below input are touched).
- **Q8 — Test for the save path in CI.** The pty test (Stage 6) writes a real
  file; run it as an integration test, not a unit test, to avoid touching the
  user's `picocode.toml`.

## B.5 Decisions / corrections log

- **2026-07-17** — Plan drafted. Chose: centralized `SETTINGS` registry +
  `SettingsStore`; inline two-state overlay (`list` ⇄ `edit`) fully contained in
  `read_input` (no new `InputAction` for Phase 1); deferred agent rebuild to
  next-prompt via `dirty` flag for Phase 2; persistence via extended `Config` +
  `save()` (with dedicated-file alternative noted). Mirrors existing `/model`
  overlay design per request.
- **2026-07-17** — Config format decision: **hard switch YAML→TOML** (no YAML
  fallback; experimental personal fork). Settings + `recipes` live in the same
  `picocode.toml`; writes via `toml_edit` preserve comments + recipes. Dropped
  `serde_yaml`; added `toml_edit` (serde feature). `PicocodeError::Yaml` →
  `Toml(String)`. Q1 resolved. Updated §A.5, §B.1, §B.2 (Stage 2), §B.3, §B.4.
- **2026-07-17** — *Phase 1 implemented.* New `src/settings.rs` (`SettingKey`,
  `ValueKind`, `SettingMeta`, `SETTINGS`, `SettingsStore` with `load`/`save`/
  `value`/`validate`/`set_value`/`overlay_rows`); `slash.rs` gains
  `SlashKind::Settings`; `repl.rs::read_input` gains a `kind = 5` list branch +
  a `list ⇄ edit` two-state overlay (`edit_setting`/`edit_prev_input`/
  `edit_error`); `agent.rs::run_interactive` + `main.rs` wire a `&mut
  SettingsStore` through. `picocode.yaml.example` → `picocode.toml.example`.
  92 lib tests + 2 pty tests green; `cargo build` warning-free. Phase 2
  (live-apply) and Stage 4 (value autocomplete) remain.
