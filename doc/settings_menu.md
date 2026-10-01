# `/set` sub-menu — Implementation Plan

Extend the `/` menu with a `/set` sub-menu that lists settings as a
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
> The config format was hard-switched YAML→TOML (no YAML fallback). `/set`
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
  and `Tab` completion (`/mod` → `/model `). `/set` must be added here so
  it appears in the `/` list and completes.
- **Settings today are NOT persisted.** `Config` (`src/config.rs`) currently
  loads `picocode.yaml`/`picocode.yml` (YAML) and holds only `agent_prompt`,
  `tool_config`, `recipes`. The five target settings live **only** in
  `AgentConfig` (`provider`, `model`, `yolo`, `tool_call_limit`, `persona_name`,
  `thinking_level`) and CLI args; there is **no `Config::save`**. **Decision
  (2026-07-17):** switch the whole config format from YAML to TOML (hard break,
  no YAML fallback — this is an experimental personal fork). The five settings
  become `#[serde(default)]` fields on `Config`, persisted to the **same**
  `settings.toml` that holds `recipes` (split later if desired). Writes go
  through `toml_edit` so user comments + recipes survive. See §A.5.
- **Live application today** is the `SetModel`/`SetThinking` path in
  `agent.rs::run_interactive`: it clones `config`, calls `create_agent`, and
  rebuilds the `Arc<CodeAgent>`. Phase 2 reuses this rebuild mechanism.

## A.2 Settings model

A single, centralized, **extendable** registry so the table, filtering,
validation, and value-autocomplete all share one source of truth.

```rust
// src/set.rs (new)
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

### List mode (`/set <filter>`, `edit_setting == None`)

- Buffer starts with `/set ` (or `/setting `, `/s `). `kind = 5`.
- `cur_word = "settings"`, `cur_filter = <text after the command word>`.
- Build rows via `SettingsStore::overlay_rows(cur_filter)` → a `Table` with
  headers `["Setting", "Value", "Description"]` (reuse `table.rs`; give
  `Description` the lowest priority + ellipsis so long text truncates; `Value`
  gets a moderate fixed/min width; `Setting` gets the highest priority so keys
  stay readable). Each row shows `name | current_value | description`.
- Navigation identical to `/model`: `Up`/`Down`/`PgUp`/`PgDn` move `sel`;
  `Tab`/`Enter` **select** the highlighted setting (do **not** bubble up).
- `Esc` returns the buffer to `/set` (mirrors `/model` Esc → `/model`).

### Edit mode (after selecting a setting, `edit_setting == Some(key)`)

- On selection: capture `edit_prev_input = editor.text()` (**exact**, e.g.
  `/set app`), then `editor.set(store.value(key))` so the input box holds
  the current value (`anthropic:claude-3-5-sonnet-20241022`, `yolo`, `50`,
  `architect`, `off`), and set `edit_setting = Some(key)`. `kind` becomes `0`
  (no overlay) naturally because the buffer no longer starts with `/`, so the
  input box is shown cleanly for editing.
- The value is **free text** in Phase 1 (user edits directly). For `Model`,
  `Enum`, and `Persona` kinds, an optional value-autocomplete pop-up (see
  §A.6) can surface candidates to pick from — Phase 1 may ship free-edit only.
- `Enter` (no Shift): call `store.set_value(key, editor.text())`:
  - **Ok** → value persisted to file; restore `editor.set(&edit_prev_input)`,
    `edit_setting = None`; the loop re-renders the **list** from `/set app`
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
`settings.toml` as `recipes`/`agent_prompt`/`tool_config` (can be split into a
separate file later). This is the cleanest resolution of the earlier Q1: TOML +
`toml_edit` preserves user comments + recipes on every `/set` write, which
YAML (no round-trip comment-preserving library in Rust) cannot do.

**Reading.** `Config::load` reads `settings.toml` (single default name; the
`--config <path>` override still works). Deserialize via `toml_edit::de::from_str`
(or `toml::from_str`) into the `Config` struct — the existing serde derives work
unchanged. The five new fields are `#[serde(default)]` so an old/minimal file
still parses.

**Writing (comment-preserving).** `SettingsStore::save()` does **not**
round-trip the whole file through serde (that would drop comments + recipes
structure). Instead it:
1. Parses the existing `settings.toml` with `toml_edit::DocumentMut::from_str`
   (if the file is absent, starts from an empty `DocumentMut` + a header comment
   block).
2. Sets only the five managed keys (`default_model`, `security_mode`,
   `tool_call_limit`, `default_persona`, `default_thinking_level`) via
   `doc["key"] = toml_edit::value(...)`.
3. Re-emits with `doc.to_string()` — every other key (incl. `[recipes.*]` and
   their comments) is preserved byte-for-byte.

This keeps recipe editing and `/set` writes in one file with zero comment
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

## A.7 Live application (Phase 2) — re-evaluated 2026-07-17

Phase 1 persists to the file only. Phase 2 makes a change also affect the
**running session**. The recent `/set` commit flow (string-hierarchy,
`emit_commit` stays in the `read_input` loop instead of returning an
`InputAction`) *confirms* the original design choice: save happens in
`read_input`, and the agent rebuild is deferred to `run_interactive` via a
`dirty` flag — there is no other clean path, because `read_input` is **sync**
and cannot call the async `create_agent`.

Refinements from the recent work:

- **`read_input` must NOT mutate `AgentConfig`.** It only needs `&mut bool`
  (`dirty`). On a successful `set_value` it sets `*dirty = true`; it does
  **not** touch `config`. `run_interactive` already owns `settings: &mut
  SettingsStore` (which `set_value` already updated in memory), so at the next
  `Submit` it derives the new `AgentConfig` *from the `SettingsStore`* and
  rebuilds. This keeps `read_input` sync-only and avoids a config/agent
  divergence bug: if the rebuild fails (valid-format but non-existent model,
  missing API key), `config` and the agent stay in sync (both old) instead of
  `config` having been mutated while the agent did not.
- **Derive, don't duplicate.** Add `apply_settings_to_config(cfg: &mut
  AgentConfig, s: &SettingsStore)` — the inverse of the existing
  `SettingsStore::from_agent_config` — mapping the five settings onto
  `AgentConfig` (`default_model` → provider/model split, `security_mode` →
  `yolo`, `tool_call_limit`, `default_persona` → `persona_name` +
  `persona::get_persona`, `default_thinking_level` → `thinking_level`).
- **Rebuild site + timing.** In `run_interactive`'s `Submit` arm, *before*
  generating the response (where `create_agent` is already lazily called), if
  `dirty`: `let mut new_cfg = config.clone();
  apply_settings_to_config(&mut new_cfg, settings); match
  create_agent(new_cfg.clone()).await { Ok(a) => { agent = Some(a); config =
  new_cfg; dirty = false; }, Err(e) => { output.display_text(&format!("Cannot
  apply settings: {}", e)); /* keep `dirty` so the next Submit retries */ }
  }`. This mirrors the existing `SetModel`/`SetThinking` handlers exactly.
- **Immediate feedback without a rebuild.** The `/set` *list* re-renders
  with the new value the instant `set_value` succeeds (it updates the
  `SettingsStore` in memory). The `/model` overlay's active marker and the
  status line pick up the new model only after the deferred rebuild (next
  `Submit`) — acceptable latency, same as the original plan's note. *Verify
  whether the status line is reprinted per-loop; if printed once at startup it
  is stale until the rebuild (cosmetic only).*
- **Precedence.** Live changes write the `SettingsStore` (and the file) and,
  via the rebuild, override the initial CLI/file `AgentConfig` for the rest of
  the session. Startup precedence (CLI > file > built-in) is unaffected.
- **Optional consistency.** `/model`/`/thinking` already live-apply (they
  return `SetModel`/`SetThinking`) but do **not** persist. Consider also
  writing `default_model` when the user picks a model, so the session default
  survives restart (out of scope unless wanted).
- **`LiveState` is just `&mut dirty`.** Because `run_interactive` already
  holds `settings: &mut SettingsStore` (the source of truth for the rebuild),
  `read_input` only needs `dirty: &mut bool` added to its signature — not a
  `&mut AgentConfig`. (A `LiveState { dirty }` struct can be introduced later
  if more live state is needed.)

## A.8 `slash.rs` + help wiring

- Add `SlashKind::Settings` and a registry entry
  `SlashCommand { name: "/set", help: "View and edit settings", kind: Settings }`.
- In `read_input`'s command dispatch, add a branch for
  `"settings" | "setting" | "s"` (parallel to `"model" | "models" | "m"`),
  setting `kind = 5`.
- `Tab` completion from the `/` list: when the highlighted command is
  `Settings`, `editor.set("/set ")`.
- `render_help` and the startup hint currently reference `/menu` (a known TODO,
  see `phase_1_plan.md` §3.7). Update them to mention `/set` (or alias
  `/menu` → `/set`).

---

# Part B — Implementation plan

## B.1 Proposed file changes

| File | Change |
|---|---|
| `src/set.rs` | **New.** `SettingKey`, `ValueKind`, `SettingMeta`, `SETTINGS` registry, `SettingsStore` (`load`/`save`/`value`/`set_value`/`overlay_rows`), parse + validate per key. Re-export from `lib.rs`. |
| `Cargo.toml` | Drop `serde_yaml`; add `toml_edit = { version = "0.25", features = ["serde"] }`. |
| `src/config.rs` | **YAML→TOML hard switch.** `Config::load` reads `settings.toml` (single default; `--config` override unchanged). Add `#[serde(default)]` fields `default_model`, `security_mode`, `tool_call_limit`, `default_persona`, `default_thinking_level`. No `Config::save` (settings writes go through `SettingsStore`/`toml_edit`). |
| `src/slash.rs` | Add `SlashKind::Settings`; registry entry `/set`; `filter_commands`/completion already picks it up; update `parse_slash` not required (handled inline in `repl.rs`). Bump registry count test (currently `4`). |
| `src/repl.rs` | `read_input` already takes `&mut SettingsStore` (Phase 1). For Phase 2 live-apply, add `dirty: &mut bool` and set it on a successful `/set` `set_value`. Add `kind = 5` branch, `edit_setting`/`edit_prev_input` locals, the edit-mode key guard, and `settings_overlay_entries` pure helper. Update `render_help`/hint references. |
| `src/agent.rs` | `run_interactive`: build `SettingsStore` from `Config`; pass `&mut` into `read_input`; add `dirty` flag; on `Submit`, rebuild agent if dirty (Phase 2). |
| `src/main.rs` | Switch config load to `settings.toml`; resolve setting precedence (CLI > file > built-in) for the 5 settings; update help strings ("picocode.yaml" → "settings.toml"); pass `Config` into `run_interactive`. |
| `src/lib.rs` | `pub mod settings;`, re-export `SettingsStore`, `SettingKey`. Replace `PicocodeError::Yaml` with `Toml(String)` (mapped from `toml_edit`/`toml` parse errors). |
| `settings.toml.example` | New (rename `picocode.yaml.example`): TOML syntax for `agent_prompt`, `tool_config`, `recipes`, and the 5 settings keys (see §B.3). |

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
      same `settings.toml`.

### Stage 2 — `config.rs` YAML→TOML switch
- [ ] `Config::load` reads `settings.toml` (drop `.yaml`/`.yml`); deserialize via
      `toml_edit::de::from_str`. Keep `load(None)` cwd search.
- [ ] Add 5 `#[serde(default)]` fields to `Config`; `#[derive]` unchanged.
- [ ] `PicocodeError`: replace `Yaml` variant with `Toml(String)`; `From`
      impls for `toml_edit`/`toml` parse errors.
- [ ] **Unit tests**: `load` parses a TOML file with `recipes` + settings;
      missing/old file still parses via `#[serde(default)]`; built-in defaults
      via `Default`. (No `Config::save` — writes flow through `SettingsStore`.)

### Stage 3 — `slash.rs` + `repl.rs` overlay (Phase 1: persist only)
- [ ] `slash.rs`: `SlashKind::Settings` + `/set` registry entry; update
      count assertions (`registry().len() == 5`, `empty_query_returns_all == 5`).
- [ ] `repl.rs::read_input`: add `&mut SettingsStore` param; `kind = 5` branch
      building the `Setting|Value|Description` `Table` from
      `settings_overlay_entries`; `Up/Down/PgUp/PgDn`/`Tab`/`Enter` select;
      `Esc` → `/set`.
- [ ] New locals `edit_setting`/`edit_prev_input`; edit-mode key guard
      (commit `set_value`; on Ok restore `edit_prev_input` + re-render list; on
      Err show inline error; `Esc` cancels). Pure helper
      `settings_overlay_entries(store, filter)` (mirrors
      `model_overlay_entries`) for unit testing.
- [ ] `run_interactive`: construct `SettingsStore`, pass `&mut`; save happens
      inside `read_input` (no `InputAction` needed for Phase 1).
- [ ] Update `render_help` + startup hint to mention `/set`.
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

### Stage 5 — Phase 2 live-apply (refined 2026-07-17) — DONE
- [x] `run_interactive`: add `let mut dirty = false;`; pass `&mut dirty` into
      `read_input` (alongside the existing `&mut SettingsStore`).
- [x] `read_input` signature: add `dirty: &mut bool`. In the `/set` value
      commit `Ok` branch, after `settings.set_value(...)` succeeds, set
      `*dirty = true`. No `AgentConfig` mutation here.
- [x] Add `apply_settings_to_config(cfg, settings)` (inverse of
      `from_agent_config`); unit-test the five mappings
      (`settings::tests::apply_to_config_maps_the_five_settings`).
- [x] `run_interactive` `Submit` arm: if `dirty`, derive `new_cfg` from
      `config` + `settings`, `create_agent(new_cfg)`, mirror `SetModel`
      failure handling (display error, keep `dirty` to retry), on `Ok` update
      `agent` + `config` and clear `dirty`.
- [x] **Test:** `settings_change_triggers_deferred_rebuild` (pty) drives a
      `/set` change, asserts the committed line + the deferred rebuild
      attempt at the next `Submit` (`Unsupported provider` error) + file
      persistence. Unit test covers the five mappings.

**Confirmed decisions (2026-07-17):**
- The startup status line is *not* refreshed after live-apply; it stays as
  printed once at startup (screen-space efficiency; expose settings via a
  future slash command instead). The `/set` *list* re-renders with the
  new value immediately (it reads the in-memory `SettingsStore`).
- `/model` (session switch) intentionally does **not** persist to
  `default_model` — they are separate on purpose. `/set default_model`
  is the only path that writes the session default to `settings.toml`.

### Stage 6 — Integration (pty) test
- [x] `tests/render_scroll.rs`: `settings_string_hierarchy_saves_and_errors`
      (save + red-error + clear-on-Esc) and `settings_change_triggers_deferred_rebuild`
      (deferred rebuild wiring) cover this. See Stage 5.

## B.3 `SettingsStore` ↔ `Config` mapping (concrete)

```toml
# settings.toml (additions; all optional, defaulted)
# Managed by the /set menu — safe to edit by hand.
default_model = "anthropic:claude-3-5-sonnet-20241022"
security_mode = "confirm"          # yolo | confirm
tool_call_limit = 50
default_persona = "architect"      # omit for no persona
default_thinking_level = "off"     # off | low | med | high
show_reasoning = "content"         # disabled | indicator | content
reasoning_in_context = false       # keep reasoning in session context (true | false)
```

`SettingsStore` reads these into its typed fields (via `Config`/`toml_edit::de`);
`set_value` writes them back through `toml_edit::DocumentMut`, preserving the
rest of the file. `default_persona` omitted ⇄ `None`. Full file also holds
`agent_prompt`, `tool_config`, and `[recipes.*]` (see `settings.toml.example`).

## B.4 Risks & open questions

- **Q1 — Config format / file location.** **RESOLVED (2026-07-17):** hard
  switch the whole config to **TOML** (no YAML fallback); settings + `recipes`
  live in the same `settings.toml`; writes via `toml_edit` preserve comments +
  recipes. Supersedes both the "extend YAML" and "dedicated YAML file" options.
  (Splitting settings into a separate file later remains possible.)
- **Q2 — "Stay in overlay" vs live-apply rebuild.** Resolved in §A.7: save +
  in-memory update happen inside `read_input`; the agent rebuild is deferred to
  the next `Submit` via a `dirty` flag (no new `InputAction`, no generic rebuild
  inside `repl.rs`).
- **Q3 — `default_model` vs CLI / `default_model(provider)`.** Precedence is
  CLI > file > built-in (§A.5). **RESOLVED (2026-07-17):** `/model` (session
  switch) intentionally does **not** persist to `default_model` — they are
  separate on purpose (frequent session switch vs. durable default). Only
  `/set default_model` writes the session default to `settings.toml`.
  Live-apply of `/set default_model` still updates the running session
  (Stage 5).
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
  user's `settings.toml`.

## B.5 Decisions / corrections log

- **2026-07-17** — Plan drafted. Chose: centralized `SETTINGS` registry +
  `SettingsStore`; inline two-state overlay (`list` ⇄ `edit`) fully contained in
  `read_input` (no new `InputAction` for Phase 1); deferred agent rebuild to
  next-prompt via `dirty` flag for Phase 2; persistence via extended `Config` +
  `save()` (with dedicated-file alternative noted). Mirrors existing `/model`
  overlay design per request.
- **2026-07-17** — Config format decision: **hard switch YAML→TOML** (no YAML
  fallback; experimental personal fork). Settings + `recipes` live in the same
  `settings.toml`; writes via `toml_edit` preserve comments + recipes. Dropped
  `serde_yaml`; added `toml_edit` (serde feature). `PicocodeError::Yaml` →
  `Toml(String)`. Q1 resolved. Updated §A.5, §B.1, §B.2 (Stage 2), §B.3, §B.4.
- **2026-07-17** — *Phase 1 implemented.* New `src/set.rs` (`SettingKey`,
  `ValueKind`, `SettingMeta`, `SETTINGS`, `SettingsStore` with `load`/`save`/
  `value`/`validate`/`set_value`/`overlay_rows`); `slash.rs` gains
  `SlashKind::Settings`; `repl.rs::read_input` gains a `kind = 5` list branch +
  a `list ⇄ edit` two-state overlay (`edit_setting`/`edit_prev_input`/
  `edit_error`); `agent.rs::run_interactive` + `main.rs` wire a `&mut
  SettingsStore` through. `picocode.yaml.example` → `settings.toml.example`.
  92 lib tests + 2 pty tests green; `cargo build` warning-free. Phase 2
  (live-apply) and Stage 4 (value autocomplete) remain.

---

# Phase 2 — string-hierarchy + uniform tab-complete (DESIGN LOCKED)

## P2.1 Goal

Replace the `/set` *two-state* overlay (`list` ⇄ free-text `edit`,
threaded via `edit_setting`/`edit_prev_input`/`edit_error`) with a
**single, string-driven hierarchy** that mirrors how `/model`, `/thinking`
and the command palette already work: the input line *is* the navigation
state. Also generalize **Tab** to a uniform, all-dialogs completion and
set a hard rule for Tab vs Enter.

## P2.2 Input-string hierarchy

```
/set                     -> list mode  (all settings)
/set def                -> list mode  (fuzzy-filtered by token)
/set default_model       -> list mode  (row selected; Enter drills in)
/set default_model x:y   -> value mode for `default_model`, query "x:y"
```

Parsed from `editor.text()` every iteration (no extra state):
`parse_settings(rest) -> { setting: Option<SettingKey>, value_query: String, mode: List|Value }`.
`parts = rest.split_whitespace()`; `len >= 2` ⇒ value mode (resolve
`parts[0]` to a `SettingKey` via exact then fuzzy best match); else list mode.

## P2.3 Tab vs Enter — the uniform rule

- **Tab = expand only.** Fill the *current (last) token* with the
  highlighted candidate (`candidates[sel]` — the best fuzzy match for what
  is typed). Updates the input string ⇒ recomputes the overlay next render.
  **Never commits a lasting change** (no save / no quit / no drill-in).
  No-op when the candidate list is empty (safe).
- **Enter = activate.** Do the primary action on the highlighted item:
  - list mode → drill in: `editor.set("/set {exact_name} ")`
    (namespace completion appends a space; leaf values do **not**).
  - value mode → `set_value(key, candidates[sel].value | value_query)`
    then **revert to top `/set`** on success.
  - `/model` → `InputAction::SetModel`; `/thinking` → `SetThinking`;
    `/quit` → `Quit`. All lasting changes go through Enter only.

This retires the two ad-hoc Tab behaviors at `repl.rs` (kind==1 ⇒
command-word replace; kind==5 ⇒ `enter_setting_edit`/drill-in) in favour
of one generic `complete_token` that each mode feeds its candidate *names*.

## P2.4 Esc = pop one level

`Esc` removes the last token (walks up the tree): `/set default_model x:y`
⇒ `/set default_model` (list) ⇒ `/set` (top list) ⇒ closes the
overlay. This replaces the old `edit_setting` Esc = full-cancel, and is
consistent with the string hierarchy (backspace walks up too).

## P2.5 Value errors → red header overlay

Validation failure (`set_value` `Err`) sets a display-only `value_error:
Option<String>`. In **value mode** the overlay prepends a **red header
line** (same below-position, BG + red) *above* the regular candidate
list, e.g. `error: tool_call_limit must be greater than 0`. The `focus`
visual-line index is bumped by the header line count. Cleared on any
keystroke that edits the value token. List mode shows no error.

## P2.6 Candidate source

`setting_value_candidates(key, ctx) -> Vec<Candidate { display, value }>`:
- `Enum(values)` ⇒ the `&[..]` slice (e.g. `yolo`/`confirm`, `off`/`low`/`med`/`high`).
- `Persona` ⇒ `persona::PERSONAS` names + a `"none"` entry (value `""`).
- `Model` ⇒ reuse `model_overlay_entries(ctx, query)`; `value =
  "provider:model"` derived from the `ModelEntry` (id `openrouter/tencent/hy3:free`
  ⇒ `openrouter:tencent/hy3:free`).
- `Int` / `FreeText` ⇒ empty (free-text box; `value_query` from the input line).

## P2.7 Scope

**Uniform now (scope A).** Build the generic `complete_token` and wire it
for `/set` *and* `/model` / `/thinking` / commands in the same pass;
this deletes the ad-hoc Tab code and prevents further inconsistent menus.

## P2.8 Net code change vs Phase 1

- **Remove** `edit_setting: Option<SettingKey>`, `edit_prev_input: String`
  (state now encoded in the input string). Keep `value_error` (display-only).
- **Add** `parse_settings`, `setting_value_candidates`, generic
  `complete_token`.
- **Rework** the `kind == 5` overlay (list vs value) + Enter/Tab/Esc
  dispatch; retire `enter_setting_edit`.
- Reuses `fuzzy_filter`, `overlay_lines`, `Table::render_multiline`, and
  `model_overlay_entries` wholesale. Low risk; fewer lines than Phase 1's
  edit plumbing.

- **2026-07-17** — *Phase 2 design locked.* Replaces the `/set`
  `list ⇄ edit` two-state overlay with a **string hierarchy**
  (`/set [key] [value]`); state lives in the input line, so
  `edit_setting`/`edit_prev_input` are removed. **Uniform rule:** Tab =
  expand current token to the highlighted full match (never commits); Enter
  = activate/commit (lasting changes — save, quit — only via Enter).
  **Value mode opens only when the first `/set` token *exactly* matches a
  setting name** (case-insensitive); partial prefixes (e.g. `d`, `def`, `sec`)
  stay in the list until typed out or Tab-completed to the full name. This
  avoids `/set d` jumping straight into `default_model` while other
  `d`-prefixed settings exist.
  `complete_token` is generic across `/set`, `/model`, `/thinking`,
  commands; retires the ad-hoc Tab handlers. `Esc` = pop one token.
  Successful set reverts to top `/set`. Value errors render as a red
  header line above the regular value overlay. Candidate source
  `setting_value_candidates(key, ctx)` reuses `model_overlay_entries` for
  `Model`. **Implemented & green:** pure fns `parse_settings` /
  `resolve_setting_key` / `replace_last_token` + assoc
  `setting_value_candidates(key, ctx)`; the `kind == 5` build block
  branches list vs value mode; uniform `Tab` handler (generic
  `complete_token` behaviour wired for `/set`, `/model`,
  `/thinking`, commands) replaces the ad-hoc handlers; `Enter` commits
  (value mode → `set_value` then revert to top `/set`); `Esc` pops
  one token; value errors render as a red header line above the
  regular value overlay. Unit tests for the parsers + a pty
  integration test (`settings_string_hierarchy_saves_and_errors`)
  cover list→save→persist and invalid→red-error (no clobber).
  98 lib + 3 pty tests pass; build warning-free. (Phase 2 done;
  live-apply to the running session remains — see Phase 1 note.)

  **UI refinements (post-implementation nits, all green):**
  - A bare `/set` (or `/model`, `/thinking`) with *no* trailing space
    now opens its menu — the command dispatch opens a menu when the bare
    token exactly matches a command word (`matches!` on `after`).
  - The `default_model` value candidates (`model_candidate` pure helper)
    carry the `provider:model` form in both `value` and `display`; a
    regression test locks this.
  - The red validation-error header is tied to value mode: cleared at the
    start of every key event (the Enter handler re-sets it on a fresh
    invalid submit), so it disappears the moment the user navigates away
    (e.g. Esc) instead of lingering in the settings list. Pty test asserts
    the error line is gone from the live screen after Esc.
  - `LineEditor::backspace` now always deletes from the end of the line
    (append-only input); cursor position is ignored. Updated unit tests.
  - `/thinking <level>` no longer prints a `thinking level -> …`
    confirmation line (the `/thinking <level>` input is self-documenting);
    the live-apply still happens in `agent.rs::run_interactive`.
  - **Generic `emit_commit` helper (unified commit):** all three commits
    (`/model`, `/thinking`, `/set <key> <value>`) now go through
    `Repl::emit_commit`, which clears the input, `display_plain`s the
    committed-invocation line, syncs `input_row`, and pre-fills the next
    input (`""` for leaf commands; `"/set"` for the `/set` value
    commit that reverts to the list). The separator below the emitted line is
    drawn *conditionally* via a `sep` flag: `/set` value commits stay in
    the `read_input` loop (revert to the list, never return to `agent.rs`), so
    `emit_commit` draws the separator itself (`sep = true`); `/model` and
    `/thinking` *return* an `InputAction`, so `agent.rs` already draws a
    separator before the next `read_input` (its per-prompt framing at
    `agent.rs:519`) — passing `sep = false` there avoids a doubled separator.
    This guarantees exactly one separator *directly* below the emitted line for
    every commit. Verified by pty: a single divider sits immediately below the
    emitted line for all three. `/quit` also emits a trailing newline
    (`display_plain("")`) so the shell prompt after exit starts on a fresh
    line instead of indented under the input.
  - `/model` list shows the `provider:model` form (verified by dump) and the
    filter text now leads with `provider:model` (plus bare provider + slash
    id), so typing `openrouter:deepseek` filters to that model.
  - **Model names are fully-qualified `provider:model` (nested namespace
    preserved):** OpenRouter entries carry `provider == "openrouter"` with an
    `id` that is the *full* (namespaced) model path (e.g. `tencent/hy3:free`),
    so the qualified form is `openrouter:tencent/hy3:free` — **not**
    `openrouter:hy3:free`. Added `ModelEntry::qualified_parts()`/
    `qualified()` (model_catalog.rs) and routed both `model_candidate` (the
    `/set default_model` picker) and the `/model` Enter handler through
    it, plus the `/model` list's Model column and its fuzzy filter text. A
    regression test locks the nested form. Previously the id was split on the
    first `/` and the namespace was dropped.
  - **`/model` / `/thinking` commits now emit the *selected* line (not the raw
    editor):** previously a bare `/model` + Enter emitted the literal `/model`
    (no model) because `editor.text()` was empty, and `agent.rs` additionally
    printed a redundant `model -> provider/model` confirmation. Now the Enter
    handler emits `/model <provider:model>` / `/thinking <level>` built from
    the highlighted selection (`ModelEntry::qualified()`), and the
    `agent.rs::SetModel` success confirmation line is removed (mirroring the
    already-clean `SetThinking` path). The emitted input line is the sole
    record of the change.
  - **Enter on a highlighted `/set` value candidate no longer errors:**
    pressing Enter with an empty value editor saved the empty string →
    `expected provider:model` validation error. Enter now saves the
    highlighted candidate's value when the editor is empty (mirrors Tab); a
    typed value is still saved as-is. The emitted line is rewritten to the
    saved value so the record is accurate. `filtered_value_candidates` helper
    shared by Tab + Enter.
