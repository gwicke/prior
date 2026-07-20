# `/sandbox` sub-menu — Session Path Bindings

Extend the `/` menu with a `/sandbox` sub-menu that lets the user temporarily
add/remove read-only and read-write path bindings for the **current session**
only. These changes are **never persisted** to `settings.toml` — they are held
in a global `Arc<Mutex<Vec<SessionBind>>>` and merged into the live
`SandboxConfig` before each `bwrap` invocation.

---

## Motivation

During a session, the user may need to temporarily expose additional
directories to the sandbox (e.g. a build output directory, a config subtree,
a log directory) without editing and re-reading `settings.toml`. The
`/sandbox` menu provides this without leaving the REPL.

---

## Design

### Data model

A new module `src/sandbox_bindings.rs` holds a process-wide, session-scoped
list of temporary bindings:

```rust
/// A single temporary binding added during the session.
#[derive(Debug, Clone)]
pub struct SessionBind {
    pub host_path: PathBuf,       // resolved absolute host path
    pub mode: BindMode,           // Ro | Rw
}

/// Global session-scoped additional bindings.
static SESSION_BINDS: LazyLock<Mutex<Vec<SessionBind>>> = …;

pub fn add_ro(path: &str) -> Result<(), String>;
pub fn add_rw(path: &str) -> Result<(), String>;
pub fn remove(path: &str) -> Result<(), String>;
pub fn list() -> Vec<SessionBind>;
pub fn merge_into(cfg: &mut SandboxConfig);
```

### SandboxConfig merge

`merge_into` appends the temporary bindings to the appropriate `ro`/`rw` list
of a `SandboxConfig` **clone** before it is published to the `SANDBOX` static
in `tools.rs`. The original `SandboxConfig` (from `settings.toml`) is never
mutated.

### Places that publish the sandbox config

1. `agent.rs::run_interactive` — calls `set_sandbox(config.sandbox.clone())`
   once at the start.
2. `agent.rs::build_rig_agent` — calls `set_sandbox(sandbox.clone())` at agent
   creation.

After each `/sandbox add_ro` / `add_rw` / `remove` we call
`set_sandbox(merge_session(sandbox_config()))` so the change takes effect
immediately for the next `!`/`!!` command or LLM tool call.

### Slash-command integration

- **`SlashKind::Sandbox`** — new variant in `src/slash.rs`.
- **`InputAction::Sandbox(...)`** — new variant in `src/repl.rs`.

The overlay flow:

```
/          → command list (kind 1) includes "/sandbox"
/sandbox   → sub-menu overlay (kind 8) with three items:
               add_ro   — Make path available read-only
               add_rw   — Make path available read-write
               remove   — Remove a path from the bindings
/sandbox add_ro <path>  → file-path tab-completion overlay
/sandbox add_rw <path>  → file-path tab-completion overlay
/sandbox remove <path>  → (optional) file-path tab-completion for existing binds
```

### File-path tab-completion

A generic `complete_path_prefix(prefix: &str) -> Vec<String>` helper resolves
`prefix` against the filesystem (`std::fs::read_dir` on the parent directory,
fuzzy-matching the prefix) and returns completions. It is used by the
`/sandbox add_ro` / `add_rw` sub-commands and can be wired into the regular
prompt input (e.g. when the user types `/tmp/foo<tab>` in the prompt).

The completion overlay is a new overlay type (kind 9) that shows matching
paths in a scrollable list, reusing the same `overlay_lines` + `sel` machinery.

### Tab-complete overlay

When the user presses Tab in the `/sandbox add_ro <partial>` state, the editor
replaces the last token with the completed path. If multiple completions match,
an overlay (kind 9) shows the candidates and the user can select with Up/Down
and Tab/Enter to complete (same pattern as `/model` / `/set` value overlays).

---

## Implementation plan

### Files changed

| File | Change |
|------|--------|
| `src/sandbox_bindings.rs` | **New** — session bindings registry + merge |
| `src/slash.rs` | Add `Sandbox` variant to `SlashKind`, add `/sandbox` to registry |
| `src/repl.rs` | Add `Sandbox` variant to `InputAction`, handle kind 8/9, file-path completion |
| `src/agent.rs` | Handle `InputAction::Sandbox` in `run_interactive` |
| `src/tools.rs` | Minor: expose `set_sandbox` and `sandbox_config` (already `pub(crate)`) |
| `src/lib.rs` | Add `pub mod sandbox_bindings` |

### Stages

1. **New module `sandbox_bindings.rs`** — session-scoped bindings, `merge_into`.
2. **`slash.rs`** — register `/sandbox` command.
3. **`repl.rs`** — overlay for sub-commands, file-path completion, Tab/Enter handling.
4. **`agent.rs`** — handle `InputAction::Sandbox`, re-publish sandbox config.
5. **`lib.rs`** — register the new module.

### Risks

- **Thread safety**: `SANDBOX` / `SESSION_BINDS` are both `Mutex`-protected;
  lock ordering is always `SESSION_BINDS → SANDBOX` (never the reverse) to
  avoid deadlock.
- **Path resolution**: bindings are resolved against the agent's CWD at the
  time of the `/sandbox` command, not at bwrap invocation time. This matches
  how `settings.toml` bindings are resolved and is safe because the CWD does
  not change mid-session.
- **Performance**: `read_dir` on the parents of every typed prefix could be
  slow on deep trees. The completion is only triggered on Tab (not on every
  keystroke), so it is acceptable.

---

## Testing

- **Unit tests** for `sandbox_bindings.rs` (add, remove, list, merge, dedup).
- **Unit tests** for path-completion prefix matching (no filesystem — use a
  mock / temp dir).
- **Integration tests** via `!``cat /proc/mounts`` after `/sandbox add_ro
  /tmp/foo` to verify the binding is visible inside the sandbox.