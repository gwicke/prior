# Sandbox design & implementation

Port `/sandbox` to v2.5: a static "add" entry plus one dynamic listing of
the merged (config + session) path bindings, each with a remove/exclude/
restore action. Replaces the v2.0 `Sandbox` overlay, `InputAction::Sandbox`,
and `sandbox_overlay_entries`. The session-bind store is kept (renamed
`sandbox_state`) — it is still the data source for the agent, `bwrap`, and
`tools::sandbox_list_dir`.

## Tree structure

```text
/sandbox (StaticListingNode; emits the "sandbox" command row)
- columns: host | id(sandbox path) | mode | source
- fields:  mode (string), source (string)
- children:
  1. "add" (StaticListingNode) — added first, so it tops the default listing
     - data() row: {id: "add", desc: "Add a path mapping to the sandbox"}
       (id/desc-only; the other columns render blank — rendering tolerates
        missing fields, only the id is required)
     - child: AddSandboxMappingNode (leaf)
  2. SandboxMappingNode (dynamic; emits one row per merged binding)
     - id      = sandbox path  (unique key: dedup/remove operate on it)
     - fields  = host (string), mode ("ro"/"rw"), source
                 source ∈ {"session", "cfg", "cfg [DISABLED]"}
     - header(): mapping src/dst/mode/source info
     - child: SandboxRemoveNode
```

`SandboxMappingNode` returns *all* bindings as rows for the /sandbox listing;
drilled into (id = sandbox path) it shows `header()` and its single action
child.

## Nodes (all `SlashDotNode` impls)

| Node | data() | header() | action |
|---|---|---|---|
| `SandboxListingNode` | the `sandbox` row | — | — (built inline via `StaticListingNode`) |
| (add) `StaticListingNode` | the `add` row | — | — |
| `AddSandboxMappingNode` (leaf) | `ro` / `rw` rows | "Specify host path and optionally sandbox path" (+ mode) | `complete_parameter` (fs); `submit` → add |
| `SandboxMappingNode` | one row per binding | src/dst/mode/source | — |
| `SandboxRemoveNode` (leaf) | `remove` (or `restore` when excluded) | remove/disable note | `prepare` (confirm) → `submit` |

### `AddSandboxMappingNode`

- `data()`: `[{id:"ro",desc:"read only mapping"}, {id:"rw",desc:"read-write mapping"}]`.
  Mode is read from `ids.last()`; the paths come from `args` (arg 1 = host,
  arg 2 = optional sandbox dst).
- `complete_parameter`: arg 1 → host FS; arg 2 → sandbox-aware listing
  (`sandbox_list_dir` → host fallback). Selected via
  `PathArgs::committed(cursor)` (empty ⇒ arg 1); `≥2` committed ⇒ inert.
- `submit`: missing/nonexistent host path ⇒ `SubmitResult::Error("…")`
  (keeps the menu open — this is also the drill guard; see below). Else set
  `AsyncOp::Sandbox(Add{…})` → `Done(Some(msg))`.
- Drill arrival (Enter on `ro`/`rw`) forwards `DrillEnter`; `submit()` sees
  empty args and returns `Error("host path required")`, which renders above
  `header()` and keeps the leaf open. No `drill_rewrite` prefill needed.

### `SandboxRemoveNode`

- `data()`: look up the binding by `ids[1]` (sandbox path).
  - disabled ("excluded") config bind → `{id:"restore", desc:"re-enable this mapping"}`
  - session bind → `{id:"remove", desc:"remove this session mapping"}`
  - active config bind → `{id:"remove", desc:"exclude this config mapping"}`
- `header()`: binding detail; for a config bind add a note that the action
  only *excludes* it (reversible via `restore`).
- `prepare()`: `Prepared` (two-phase confirm).
- `submit()`: `restore` → `AsyncOp::Sandbox(Restore)`;
  `remove` → `Remove` for a session bind, `Exclude` for a config bind.
  Binding resolved by `ids[1]`, not `ids.last()` (`last()` is the action).

## Data model & state

`sandbox_bindings` → **`sandbox_state`** (data/state-only role; no UI code).
Keeps: session binds, base-config snapshot, exclusion set. Public API:
`set_base`, `add`(host, sandbox, mode) [/ `add_ro`/`add_rw` shims], `remove`,
`exclude`, `restore`, `list`, `combined_list`, `merge_into`,
`resolve_for_display`. `remove_exclude` (dead) becomes `restore`.

Mutation is asynchronous: nodes never touch the globals directly.

```rust
pub enum SandboxOp {
    Add { host_path: String, sandbox_path: String, mode: BindMode },
    Remove  { sandbox_path: String },   // session bind
    Exclude { sandbox_path: String },   // config bind
    Restore { sandbox_path: String },   // un-exclude
}
pub enum AsyncOp { /* … */ Sandbox(SandboxOp) }
```

The agent loop (already owns `config`) performs: `sandbox_state::<op>` →
`merge_into` a clone → `tools::set_sandbox(merged.clone())` →
`config.sandbox = Some(merged)` → `display_text(msg)`. This is the current
v2.0 handler, relocated.

## Prompt flows

- `/sandbox ` → root listing: bindings + `add` row
- `/sandbox add ` → mode selector: `ro` / `rw`
- `/sandbox add ro ` → leaf, `header()` shown
- `/sandbox add ro /home/gab` → host-path picker (`/home/…` filtered)
- `/sandbox add ro /home/gabriel/project` → Enter adds
- `/sandbox add ro /home/gabriel/project ~/proj` → sandbox path picker, enter adds
- `/sandbox /home/user/project` → mapping selected, `header()` shown
- `/sandbox /home/user/project remove` → confirm → removes/excludes

## Column labels (FS completion dialog)

`Host path` / `Sandbox path`; description = ISO-8601 mtime (second
resolution) + size.

---

# Implementation plan

## Engine: `SubmitResult::Error` (generic leaf guard) — DONE

Needed by any parameter leaf whose Enter may fail validation. Previously
`SubmitResult` had only `Done` / `Prepared` / `NewPath`; a failed submit could
only close the menu.

**Direct render, no error state anywhere** — the returned plan *is* the
frame; nothing is stored, so there is no clearing rule to reason about.

1. **`SubmitResult::Error(String)`** (node_impl.rs).
2. **`handle_leaf`**: `Error(msg)` arm on both `SubmitResult` matches
   (confirm-submit and `prepare`) → helper `error_continue()` returns
   `Continue(plan, pa.full_prompt())` built like the `HostKey::None` arm
   (`ensure_rows` + `view_plan` with `error: Some(msg)`) — the detail line
   `"[ERROR]: …"` renders above `header()` this frame. Same path for the
   drill arrival (which arrives via `prepare`).
3. **`view_plan` takes `error: Option<&str>`** as a parameter (this frame
   only). The old per-frame `frame_error` field is **gone**: `completion_pass`
   now returns `Result<Option<MenuResult>, String>` and its callers pass a
   completer `Err` straight through to `view_plan` — same for `Error`.
4. **Host side (repl.rs)**: an Enter/Tab `Continue` whose prompt is unchanged
   (modulo trailing whitespace — the drill's structural space) and whose plan
   has content (rows/detail/header) is painted directly for one frame via a
   one-shot `v25_enter_plan` slot (`plan_has_content` guard). Everything else
   keeps today's behavior: rewrite ⇒ buffer update ⇒ re-derive next frame.
   Minimal bridge until the v1/v2.0 repl cleanup; the semantic rule to keep
   then: *direct render when the prompt did not change*.

## Path completion extraction

Today: `complete_path_prefix_impl` (free fn, host) + a `Repl` method
`complete_path_prefix_sandbox` (sandbox `ls` → host fallback). Neither
exposes mtime/size, and both live in `repl.rs`.

New module **`src/path_complete.rs`**:

```rust
pub struct Entry {
    pub name: String,      // basename
    pub is_dir: bool,      // trailing '/' when displayed
    pub mtime: Option<std::time::SystemTime>,
    pub size: Option<u64>,
}
pub fn list_host(dir: &str) -> Vec<Entry>;      // std::fs::read_dir + metadata
pub fn list_sandbox(dir: &str) -> Option<Vec<Entry>>;  // bwrap ls, long format
pub fn complete(prefix: &str, sandbox: bool) -> Vec<Entry>; // split dir/stem, filter
```

- `complete` reproduces today's split (ends-in-`/` or is-dir ⇒ list all;
  else filter by stem), `~` round-trip, sort.
- `list_sandbox`: run `ls -lap --time-style=full-iso` in bwrap; parse
  size + mtime; keep the `-p` `/` marker for dirs. `None` (no sandbox / not
  found) ⇒ caller may fall back to host (arg 1) or return empty (arg 2).
  Baseline still works with names only if long-format parsing is skipped.
- The two `repl.rs` helpers should be dead / only used by dead 2.0 code, so do
  not spend much effort keeping them alive. If really necessary, they could
  become thin wrappers over `path_complete` (string projection of `Entry`),
  but they will be removed in the next cleanup step anyway - sandbox is the
  last item in the 2.5 migration.

## `sandbox_state` module

1. Rename file/module `sandbox_bindings` → `sandbox_state`; update all
   references (`tools.rs`, `agent.rs`, `repl.rs`).
2. Add `restore(sandbox_path)` (from `remove_exclude`); add
   `add(host, sandbox, mode)`; keep `add_ro`/`add_rw` as thin shims.
3. `combined_list` unchanged (already yields `is_session`/`is_excluded`).

## Nodes (`src/v25_sandbox.rs`)

Mirror `src/v25_tree.rs` / `src/v25_session.rs`.

1. `SandboxListingNode` (StaticListingNode): row `sandbox`, columns
   (host/id/mode/source), fields (mode, source).
2. `add` StaticListingNode row, with `AddSandboxMappingNode` child.
3. `SandboxMappingNode { }`: `data()` maps `combined_list()` →
   rows (id = sandbox path; `host`, `mode`, `source` fields); `header()`
   renders the mapping table.
4. `SandboxRemoveNode`: `data()` / `header()` / `prepare()` / `submit()` as
   above.
5. `build_sandbox_node(shared) -> SlashDotMenuNode`.
6. Register in `v25_menu::build_menu` (`root.add_child(build_sandbox_node(shared))`),
   replacing the `/sandbox` help text source in `slash.rs` (update help to
   `add ro | add rw | remove`).

## Wiring / retire v2.0

- Add `AsyncOp::Sandbox(SandboxOp)` + the agent-loop arm (relocated v2.0
  handler).
- Delete: `ActiveOverlay::Sandbox` (+ all arms), `Repl::sandbox_overlay_entries`,
  `InputAction::Sandbox` (+ its `agent.rs` arm), and the legacy `"sandbox"`
  `MenuNode` arm in `slash_menu_bridge::root_descriptor`.
- More broadly, *all* v2.0 and also the remaining v1.0 repl menu code are dead
  now, and need to be removed in the next bigger step. If Sandbox specific
  surgery is complicated, add note in impl cleanup section, so it can be
  addressed as a whole.

## Tests

- **Engine**: `Error` on `prepare`/`submit` → `Continue`, error at detail
  row 0 above `header()`; cleared when the section changes; persists across
  a passive frame.
- **`AddSandboxMappingNode`**: ro/rw rows; `submit` empty ⇒ `Error`; bad
  host ⇒ `Error`; good host ⇒ `AsyncOp::Sandbox(Add)`; arg-1 host vs arg-2
  sandbox completion routing (`committed`).
- **`SandboxMappingNode`**: rows for config+session binds (id = sandbox
  path); excluded shown as `cfg [DISABLED]`.
- **`SandboxRemoveNode`**: `restore` when excluded else `remove`; `submit`
  picks `Remove`/`Exclude`/`Restore`; confirm on `prepare`.
- **`sandbox_state`**: `restore` re-enables; existing add/remove/merge tests
  still pass after rename.
- **`path_complete`**: host listing (dirs get `/`); mtime/size description
  formatting; sandbox fallback.
- **Two-path integration**: `/sandbox add ro <host> <sandbox>` end-to-end via
  `AsyncOp`; drill arrival (`DrillEnter` on `ro`) keeps the leaf open with
  the error + header.

## Out of scope / later

- Editing existing mappings (mode flip, src/dst swap).
- Enum-set intersection for value completion (see
  `arg_and_modifier_autocomplete.md`).
- Column-label polish beyond the four columns above.
