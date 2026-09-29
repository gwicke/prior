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
- `submit`: 0 args ⇒ `Error("host path required")` (the drill guard, see
  below); ≥3 args ⇒ `Error("expected: <host path> [sandbox path]")`
  (likely a sign of other trouble); 1–2 args ⇒ validate
  `path_util::resolve_canonical(host)` — failure (nonexistent) ⇒
  `Error(…)` stay-open. The sandbox dst is **not** validated — bwrap
  creates it lazily even when parent dirs are missing. Success ⇒
  `AsyncOp::Sandbox(Add{…})` → `Done(Some(msg))`.
- Drill arrival (Enter on `ro`/`rw`) forwards `DrillEnter`; `submit()` sees
  empty args and returns `Error("host path required")`, which renders above
  `header()` and keeps the leaf open. No `drill_rewrite` prefill needed.

#### `complete_parameter` contract (settled)

- **Gate** (checked before any IO): the word contains `/`, or starts with
  `~` or `.`. Ordinary bare words are inert — v1 never completed them
  either. A dot-word that matches nothing shows no dialog (at worst a
  harmless non-event; no typing disruption).
- **Routing** via `PathArgs::committed(cursor)`:
  - 0 committed ⇒ arg 1 (host path): `complete(word, false)` — always the
    host FS; the host path must exist on the host.
  - 1 committed ⇒ arg 2 (sandbox dst): `complete(word, true)` —
    sandbox-aware (sandbox listing wins; configured-but-invisible ⇒ empty,
    never host paths; no sandbox ⇒ host fallback).
  - ≥2 ⇒ inert (empty).
- **Candidates = the dir's children only — no directory-itself row.** The
  prompt already holds the fully-expanded directory after a Tab expand:
  accept it with **space** (dismisses the dialog + finalizes the arg; the
  next word routes to arg 2) or **Enter** (submits the raw word). A self
  row would only add a no-op first Tab. Empty dir ⇒ empty vec ⇒ the
  engine's trailing-space convenience finalizes the arg.
- **Ordering** — `fuzzy::rank_candidates` with **query = the stem after
  the last `/`**, prefix/text keys = basename, fed the path-sorted
  children:
  - dir level (word ends in `/`, empty stem) ⇒ empty query ⇒ helper
    returns input order (confirmed in `fuzzy.rs`) ⇒ **alphabetical**;
  - typed stem ⇒ prefix tier first (`dir/abc` ⇒ `dir/abcde`), then fuzzy
    (`dir/someabc`); non-matches dropped by the helper. This replaces
    v1's strict `starts_with` pre-filter — the whole dir listing is
    ranked.
- **Exactness** (engine contract, handler-side): every candidate
  byte-equal to the word ⇒ return empty ⇒ engine appends the trailing
  space (arg finalized). Only reachable via file completion — children
  never byte-equal the dir word. ≥1 non-exact ⇒ return the **full** set,
  exact matches included (no highlighting).
- **Rows**: `id = entry.path` (full display path; dirs carry `/`, so a
  Tab-expand lands directly in the dir-listing branch), `desc =
  entry.description()` (ISO-8601 secs + raw bytes), `labels = (("Host
  path" | "Sandbox path" by arg), "Description")`.
- **Caching** (`complete_parameter` runs every passive frame):
  - **L1 dir listing** (path_complete): single slot keyed by the resolved
    dir. Host: revalidate per call with one dir-`mtime` stat; stat
    unavailable ⇒ TTL 5s. Sandbox: TTL 5s — a revalidation probe would
    itself be a bwrap spawn. Within-dir typing ⇒ zero IO; crossing `/` ⇒
    at most one `read_dir`/bwrap per 5s.
  - **L2 rows** (node): `Rc<CompletionRows>` keyed by (arg position,
    word) — an unchanged word (MoveCursor/None frames) returns the same
    `Rc` ⇒ `ptr_eq` skips the view re-derive. L1 expiry forces a rebuild
    even on an unchanged key.

### `SandboxRemoveNode`

- `data()`: look up the binding by `ids[1]` (sandbox path).
  - disabled ("excluded") config bind → `{id:"restore", desc:"re-enable this mapping"}`
  - session bind → `{id:"remove", desc:"remove this session mapping"}`
  - active config bind → `{id:"remove", desc:"exclude this config mapping"}`
- `header()`: binding detail; for a config bind add a note that the action
  only *excludes* it (reversible via `restore`).
- `prepare()`: `Prepared(Some(arms))` (two-phase confirm) — the labels
  are resolved from the same binding lookup `submit()` does, so the
  dialog names the effect that will actually have ("Remove this session
  mapping (deletes it)" vs "Exclude this config mapping for this
  session" vs "Re-enable this mapping").
- `submit()`: `restore` → `AsyncOp::Sandbox(Restore)`;
  `remove` → `Remove` for a session bind, `Exclude` for a config bind.
  Binding resolved by `ids[1]`, not `ids.last()` (`last()` is the action).

## Data model & state — DONE

`sandbox_bindings` → **`sandbox_state`** (renamed; data/state-only role, no
UI code). Public API as planned: `set_base`, **new** `add`(host, sandbox,
mode) with `add_ro`/`add_rw` as shims (still called by the v2.0 handler
until retirement), `remove`, `exclude`, **new** `restore` (was the dead
`remove_exclude`; `#[allow(dead_code)]` until `AsyncOp::Sandbox(Restore)`
wires it), `list`, `combined_list`, `merge_into`, `resolve_for_display`.
All references updated (`tools`, `agent`, `repl`, `lib`, `path_util`).
+2 tests (explicit mode, restore re-enables); all pre-existing tests
green after the rename.

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

## Path completion extraction — DONE

Landed module **`src/path_complete.rs`** (registered in `lib.rs`):

```rust
pub struct Entry {
    pub path: String,      // FULL display path (dir-joined, '~/…' round-trip,
                           // trailing '/' when is_dir) — the row id
    pub is_dir: bool,
    pub mtime: Option<std::time::SystemTime>,   // second resolution
    pub size: Option<u64>,                       // raw bytes
}
impl Entry { pub fn description(&self) -> String; }  // "ISO-TS  size" ('-' gaps)
pub fn complete(prefix: &str, sandbox: bool) -> Vec<Entry>; // split dir/stem, filter, sort
pub fn list_host(dir: &str) -> Vec<Entry>;            // read_dir, no symlink follow
pub fn list_sandbox(dir: &str) -> Option<Vec<Entry>>; // bwrap ls → parse
```

- `complete`: v1 split preserved (ends-in-`/` or is-dir ⇒ list that dir;
  else list the parent — **empty parent ⇒ `.`**, fixing the v1 hole that
  made bare dot-words inert — stem = basename), `~` round-trip inside the
  module (consumer never joins). Candidates = the dir's children,
  **path-sorted as the ranking input**, ordered by
  `fuzzy::rank_candidates`(query = stem, keys = basename): empty stem ⇒
  input order (alphabetical); typed stem ⇒ prefix tier then fuzzy,
  non-matches dropped — **replaces** the v1 strict `starts_with`
  pre-filter. No directory-itself row (the prompt already holds the
  expanded dir — see `AddSandboxMappingNode`). Caching: L1 single-slot
  dir listing (host: dir-mtime revalidate, stat-fail ⇒ TTL 5s;
  sandbox: TTL 5s). `sandbox=false` ⇒ host; `sandbox=true` ⇒ v1
  semantics: sandbox listing wins; sandbox configured but dir invisible
  ⇒ **empty** (never leak host paths); no sandbox at all ⇒ host
  fallback.
- `list_sandbox`: `tools::sandbox_list_lines` (new `pub(crate)` raw-IO fn)
  runs `ls -lap --time-style=+%Y-%m-%dT%H:%M:%S` in bwrap — custom format
  = ISO-8601 seconds directly (no ns/offset parsing); parser strips the
  `ls -l` symlink `name -> target` suffix, skips `total`/`.`/`..`, derives
  `is_dir` from the perms column (symlink-to-dir ≠ dir, matching host
  `file_type()`), and round-trips the timestamp through local time.
- Dialog labels (confirmed): id column `Host path` (arg 1) / `Sandbox
  path` (arg 2); description column keeps the default `Description`.
- Legacy `repl.rs` helpers (`complete_path_prefix_impl`,
  `complete_path_prefix_sandbox`) left untouched — only the in-prompt Tab
  complete still uses them; switching it to the engine completion frame
  and deleting both is a dedicated follow-up step (findings in
  arg_and_modifier_autocomplete.md §Prompt-line completion; scoped out
  of the v1/v2.0 cleanup).

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

## Wiring / retire v2.0 — DONE

- `AsyncOp::Sandbox(SandboxOp)` + the agent-loop arm (relocated v2.0
  handler: mutate `sandbox_state` → `set_sandbox(merged)` +
  `config.sandbox` → success/error display).
- Deleted: `ActiveOverlay::Sandbox` (+ all arms), the
  `from_buffer` `"sandbox" | "sb"` arm now defers to the v2.5 engine
  like session/tree, `Repl::sandbox_overlay_entries`,
  `InputAction::Sandbox` (+ its `agent.rs` arm), and the legacy
  `"sandbox"` `MenuNode` arm in `slash_menu_bridge::root_descriptor`.
  Surgery was pure deletion — no cleanup note needed.
- More broadly, *all* remaining v2.0 and v1.0 repl menu code is dead
  now and goes in the next bigger step (impl step 10 final cleanup).

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
