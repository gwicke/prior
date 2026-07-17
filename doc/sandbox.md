# Sandbox Design (`bwrap`)

This document specifies the shell-execution sandbox for the `bash` (and
`agent-browser`) tools. It fixes two problems in the current implementation:

1. **Blocking on the tokio runtime.** Commands currently run via
   `tokio::task::spawn_blocking` + duct's synchronous `.read()`. A hung command
   blocks a shared blocking thread forever and cannot be cancelled.
2. **No isolation or resource limits.** `sh_dangerous(cmd)` runs with the
   agent's full privileges in the host namespace.

Both tools are refactored to route through a single `run_sandboxed(cmd)`
helper. The helper applies a `duct` `.timeout(...)` (so commands always
terminate and the blocking thread is released) and, when `bwrap` is available,
wraps the command in a Bubblewrap (`bwrap`) namespace.

---

## Goals

- Kill runaway commands via a default timeout, even without sandboxing.
- Run shell commands in an unprivileged, namespaced sandbox by default.
- Give the operator precise control over **which paths are visible, and with
  what access** (`ro-bind` / `bind`), including **path remapping** where the
  in-sandbox path differs from the host path.
- Allow memory, network, PID, and device isolation.
- Degrade gracefully: if `bwrap` is missing, fall back to unsandboxed
  execution (timeout still enforced) and warn.

---

## Configuration

Extend `Config` so that sandbox settings live under the existing
`tool_config.<tool>` map (the same place as `auto_allow`):

```yaml
tool_config:
  bash:
    auto_allow:
      - "^cargo test"
    sandbox:
      enabled: true            # default: true if bwrap present, else false
      timeout_secs: 120        # default: 120
      network: false           # default: false (unshare network)
      memory_mb: 1024          # default: none (unlimited)
      pid_isolation: true      # default: true (--unshare-pid --pid)
      drop_caps: true          # default: true (--cap-drop ALL)
      tmp_size_mb: 256         # default: 256 (size of sandbox tmpfs at /tmp)
      paths:
        - ro:  "."             # read-only, identical from/to (common case)
        - rw:  "."             # read-write, identical from/to
        - ro:  "/usr"
        - rw:  "./target"      # built artifacts can be written
        # Remap: host path -> different in-sandbox path
        - ro:  { from: "~/.cargo", to: "/opt/cargo" }
        - rw:  { from: "./cache", to: "/var/cache/app" }
```

### Schema (Rust)

```rust
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ToolSettings {
    #[serde(default)]
    pub auto_allow: Vec<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    /// Master switch. Default: true if `bwrap` is on PATH, else false.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Wall-clock timeout for the whole command. Default 120s. 0/None = no timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Give the sandbox network access. Default false (--unshare-net).
    #[serde(default)]
    pub network: Option<bool>,
    /// RLIMIT_AS in MB (virtual memory cap). Default none.
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Run in a fresh PID namespace. Default true.
    #[serde(default)]
    pub pid_isolation: Option<bool>,
    /// Drop all capabilities. Default true.
    #[serde(default)]
    pub drop_caps: Option<bool>,
    /// Size of the tmpfs mounted at /tmp inside the sandbox. Default 256 MB.
    #[serde(default)]
    pub tmp_size_mb: Option<u64>,
    /// Filesystem bindings (see below).
    #[serde(default)]
    pub paths: Vec<BindSpec>,
}

/// A single filesystem binding for the sandbox.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum BindSpec {
    /// Short form: identical from/to. `ro: "."` or `rw: "./target"`.
    Simple {
        /// Host (and in-sandbox) path. Resolved relative to the agent CWD.
        #[serde(rename = "ro")]
        ro: String,
    },
    SimpleRw {
        #[serde(rename = "rw")]
        rw: String,
    },
    /// Remap form: different host path and in-sandbox path.
    Remap {
        mode: BindMode,            // "ro" | "rw"
        from: String,              // host path (resolved relative to CWD if relative)
        to: String,                // in-sandbox path (absolute or relative to sandbox root)
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum BindMode { Ro, Rw }
```

> **Note on `untagged` + `ro`/`rw` keys:** Because `ro` and `rw` are mutually
> exclusive and both render as a single string, the `Simple`/`SimpleRw` shapes
> above are only safe if we parse `paths` as a custom `Visitor` or as a
> `HashMap<String, String>` per entry. The recommended implementation parses
> each entry with an explicit deserializer that accepts either
> `{ "ro": "path" }` / `{ "rw": "path" }` or
> `{ "mode": "ro", "from": "...", "to": "..." }`. See "Parsing" below.

---

## The `paths` model

Each entry declares one filesystem binding. Semantics mirror `bwrap`:

| Config              | bwrap flag             | Meaning                                                        |
| ------------------- | ---------------------- | -------------------------------------------------------------- |
| `ro: P`            | `--ro-bind P P`        | Host `P` mounted read-only at the **same** path in-sandbox.    |
| `rw: P`            | `--bind P P`           | Host `P` mounted read-write at the **same** path.              |
| `ro:  {from,to}`   | `--ro-bind from to`    | Host `from` mounted read-only at in-sandbox `to`.              |
| `rw:  {from,to}`   | `--bind from to`       | Host `from` mounted read-write at in-sandbox `to`.             |

### Resolution rules

1. **Relative paths are resolved against the agent's CWD** *before* being
   passed to bwrap. `~` is expanded via `dirs::home_dir()`. Absolute paths are
   used verbatim.
2. **`from` is always a host path.** It must exist (or, for `rw`, its parent
   must exist so bwrap can create it) or the binding is skipped with a warning.
3. **`to` is the in-sandbox mount point.** For the simple form, `to == from`.
   For the remap form, `to` is created inside the namespace; if relative, it is
   interpreted relative to the sandbox root (the agent CWD).
4. **Implicit deny-by-default.** The sandbox starts from an empty root
   (`bwrap --ro-bind / /` is *not* used). Only declared `paths` are visible.
   This is what makes the sandbox meaningful: the host FS is invisible unless
   explicitly bound.
5. **Ordering / precedence.** Bindings are applied in config order. Later
   bindings win on overlap (bwrap semantics: the last mount on a path shadows
   earlier ones). A `rw` entry for a subtree of an earlier `ro` entry therefore
   upgrades just that subtree to writable — the usual pattern.

### Remap semantics (the interesting case)

Remapping lets the agent see a host directory at a **different** location than
on the host. Example: the host cache lives at `/home/user/.cache/foo` but the
tool inside the sandbox expects `/var/cache/foo`:

```yaml
paths:
  - rw: { from: "~/.cache/foo", to: "/var/cache/foo" }
```

At runtime this becomes `--bind <expanded ~/.cache/foo> /var/cache/foo`. The
sandbox root is the agent CWD, so `to: "/var/cache/foo"` is created relative to
it unless it is an absolute path that bwrap creates under the new root. Because
bwrap builds a fresh root, `to` paths that don't exist are created (bwrap
creates intermediate directories for bind targets).

Typical remap uses:

- Expose toolchains/caches at canonical locations the command expects
  (`~/.cargo` → `/usr/local/cargo`).
- Hide the real path of a secret store by mounting a dummy at the expected
  location while excluding the real one (deny-by-default means the real one is
  simply not bound).
- Co-locate a build output dir under a path the build script hard-codes.

---

## `run_sandboxed(cmd)` — the execution helper

Single function used by `Bash` and `agent_browser`. Pseudocode:

```rust
fn run_sandboxed(cmd: &str, cfg: &SandboxConfig) -> Result<String, ToolError> {
    // 1. Build the duct expression.
    let expr = if cfg.enabled_and_bwrap_present() {
        build_bwrap_expr(cmd, cfg)   // returns a duct::Expression wrapping bwrap
    } else {
        sh_dangerous(cmd)            // plain, still timeouted below
    };

    // 2. Always apply a timeout (duct implements this with a killer thread
    //    + process-group kill, so a hung child is reaped and the blocking
    //    thread is released).
    let expr = match cfg.timeout_secs {
        Some(0) | None => expr,
        Some(s) => expr.timeout(std::time::Duration::from_secs(s)),
    };

    // 3. Capture combined output.
    tokio::task::spawn_blocking(move || {
        expr.stderr_to_stdout().unchecked().read()
            .map_err(|e| ToolError::Io(e.to_string()))
    }).await??
}
```

### bwrap command construction

```
bwrap
  --ro-bind <cwd> <cwd>            # baseline: CWD visible read-only (if no path overrides)
  [for each path in cfg.paths]
      --ro-bind <from> <to>   | --bind <from> <to>
  --tmpfs /tmp  (size from tmp_size_mb, via --size)
  [--unshare-net]                       # unless network: true
  [--unshare-pid --pid]                 # if pid_isolation (default true)
  [--cap-drop ALL]                      # if drop_caps (default true)
  --die-with-parent
  -- sh -c '<memory ulimit wrapper> exec <cmd>'
```

- **Memory limit.** bwrap has no direct RLIMIT flag, so the in-namespace payload
  is wrapped: `sh -c 'ulimit -v <memory_mb*1024>; exec "$0" "$@"' sh -c <cmd>`.
  (`ulimit -v` sets `RLIMIT_AS`; combined with `--unshare-pid` the limit is
  scoped to the command's process tree.)
- **Timeout vs memory.** If memory is exceeded the process is killed by the
  kernel (OOM within the limit), not by the timeout. The timeout covers hangs;
  the memory cap covers runaway allocation.
- **`--die-with-parent`** ensures orphans are cleaned up if picocode dies.
- **Exit status.** `unchecked()` keeps duct from erroring on non-zero exit; the
  command's stdout/stderr is returned and the agent sees the real exit
  behavior through output. (Optionally surface a non-zero exit as a
  `ToolError::Generic` with the captured output — TBD per UX review.)

### Path-binding special case

When `cfg.paths` is **non-empty**, do **not** add the implicit
`--ro-bind <cwd> <cwd>` baseline; the operator's list is authoritative
(deny-by-default). When `paths` is empty and sandboxing is on, fall back to a
single `--ro-bind <cwd> <cwd>` plus a writable tmpfs, so the common case
"run commands in my project, read-only, with a scratch /tmp" works with zero
config.

---

## Fallback & detection

- At startup (or first bash call) probe `is_tool_available("bwrap")` once and
  cache the result.
- If `sandbox.enabled == true` but bwrap is absent, log a warning
  (`output`) and degrade to unsandboxed-but-timeouted execution rather than
  failing. This keeps CI/container images without bwrap functional.
- If `sandbox.enabled == false` explicitly, never invoke bwrap (useful for
  local debugging where the agent must reach the host freely).

---

## Security properties & caveats

- **Deny-by-default root** is the key guarantee: without an explicit binding,
  host files are invisible. Never use `--ro-bind / /`.
- **`--unshare-net`** blocks network egress by default; opt-in via
  `network: true`.
- **Capability drop + PID namespace** limit privilege escalation and hide the
  host process tree.
- **Not a hardening boundary against root.** bwrap provides isolation, not a
  bulletproof jail; it assumes the sandbox user is unprivileged. It is a
  defense-in-depth measure appropriate for an LLM-driven shell, not a
  substitute for running picocode as a non-root user.
- **`from` paths should be validated** against path traversal (reuse
  `validate_path` from `tools.rs`) so a malicious/erroneous config can't bind
  `/etc` or `/` unexpectedly. The remap `to` is created inside the namespace,
  so it cannot escape the sandbox root.

---

## Testing plan

- **Unit:** parse `BindSpec` for all four forms (simple ro/rw, remap ro/rw);
  assert `from`/`to` resolution (relative→CWD, `~`→home).
- **Unit:** `build_bwrap_expr` produces the expected flag sequence for a
  representative `SandboxConfig` (assert arg vector contains
  `--ro-bind`, `--bind`, `--unshare-net`, `--cap-drop ALL`, memory wrapper).
- **Integration (tempfile):** run `run_sandboxed("echo hi")` and assert output;
  run a command that exceeds `timeout_secs` (e.g. `sleep 30`) with a 1s timeout
  and assert it returns an error / is killed.
- **Integration:** with a `rw` remap binding, assert a file written inside the
  sandbox at `to` appears at host `from` (and not at `to` on the host).
- **Integration:** with `network: false`, assert a command attempting a socket
  connect fails; with `network: true`, assert it succeeds.
- **Fallback:** force bwrap-absent and assert unsandboxed execution still
  enforces the timeout.

---

## Path enforcement for non-shell tools

The `paths` policy must apply to **every** filesystem-accessing tool, not just
the shell. The non-shell tools (`read_file`, `write_file`, `edit_file`,
`glob_files`, `grep_text`, `list_dir`, `make_dir`, `remove`, `move_file`,
`copy_file`) all funnel through `get_path` → `validate_path` in `tools.rs`. That
function is the single chokepoint, so path policy is enforced **once** there
rather than per-tool.

### What `validate_path` enforces today

Only "path is within the agent CWD" (no `..` escape). This is necessary but not
sufficient for the sandbox model: a read-only binding must reject *writes*, and
a path that is not bound at all must be rejected (deny-by-default).

### New contract for `validate_path`

`validate_path(base, path, policy)` gains a `policy` argument derived from the
resolved `paths` config. It performs, in order:

0. **Remap rewrite.** Rewrite the requested path through `policy.remap`
   (`to → from` table); see "Path translation via `RemapTable`" below. A request
   for `/tmp/x` becomes the host `from` that bwrap mounts at `/tmp`, so the
   non-shell tool and the shell address the same file.
1. **Resolve** the (rewritten) path against `base` (CWD), expanding `~`, and
   normalize components (existing `ParentDir`/`CurDir` logic).
2. **Existence/binding check.** The path (or its nearest bound ancestor) must
   fall under a declared `paths` entry. If no entry covers it →
   `ToolError::Generic("Access denied: path is not within a permitted binding")`.
   (This realizes deny-by-default for the non-shell tools, matching the sandbox
   root.)
3. **Mode check** based on the operation:
   - Read operations (`read_file`, `glob_files`, `grep_text`, `list_dir`) are
     allowed under any `ro` or `rw` binding that covers the path.
   - Write operations (`write_file`, `edit_file`, `make_dir`, `remove`,
     `move_file`, `copy_file`, and the *destination* of move/copy) require the
     covering binding to be `rw`. A write under an `ro` binding →
     `ToolError::Generic("Access denied: path is read-only")`.
4. **Traversal guard** (keep existing): reject escapes above `base`.

### How `paths` maps to the policy

The `paths` list is pre-resolved at config load into a `Vec<ResolvedBind>`
(`mode`, absolute `from`, absolute `to`). From that list we also build a single
**`RemapTable`** — an ordered `to → from` map (longest `to` prefix wins) used
for path translation. Both the shell wrapper and `validate_path` consult the
same `ResolvedBind` + `RemapTable`, so the two worlds cannot disagree.

### Decision: non-shell tools stay host-direct (NOT run in-namespace)

We deliberately do **not** execute the non-shell tools as child processes inside
a bwrap namespace to make them "see" the same filesystem as the shell. Rationale:

- **Per-call process + serialization cost.** Each read/write would become
  `bwrap … -- helper <op> <path>` with bytes round-tripped over stdout. The
  tools would cease to be in-process Rust `tokio::fs` calls, losing type-safe
  `Result`s and adding a spawn per call.
- **In-process enforcement is strictly stronger.** `validate_path` rejects an
  unbound path *before* any syscall. Inside bwrap the kernel enforces it — but
  only after paying the spawn cost.
- **No cross-call namespace state to share.** Bindings are static config; each
  tool call is independent. There is no runtime namespace mutation the shell
  made that later file tools must observe, so true in-namespace execution buys
  nothing here.

Instead, consistency (including remaps like `/tmp`) is achieved by **translating
requested paths through the shared `RemapTable` before any syscall**, as below.
This keeps the tools fast, in-process, and exactly consistent with the shell's
view — no extra processes.

### Path translation via `RemapTable` (the consistency mechanism)

`validate_path` performs, in order (steps 1–4 above, plus a rewrite step):

0. **Remap rewrite.** Before coverage/mode checks, rewrite the requested path
   through `RemapTable`: if it (or an ancestor) matches a `to` prefix, replace
   that prefix with the corresponding host `from`. So a non-shell write to
   `/tmp/x` is transparently redirected to the host `from` that bwrap mounts at
   `/tmp` (e.g. `./scratch/x`). The shell command writing `/tmp/x` lands on the
   *same* host file. **This is what makes `/tmp` (and any remap) mean the same
   thing to both tool kinds.**
1. **Resolve** against `base` (CWD), expand `~`, normalize components.
2. **Coverage check** against the *resolved host* path (post-rewrite) using
   `from`/`mode` of each binding; unbound → deny.
3. **Mode check** (read vs write) on the covering binding.
4. **Traversal guard** (keep existing).

For non-remapped bindings (`from == to`) the rewrite is a no-op and the host
path matches `from` directly. For remapped bindings, the rewrite maps the
agent-visible `to` back to the host `from`, then the normal `from`/`mode` policy
applies. The doc string on `ResolvedBind`/`RemapTable` must state that
non-shell enforcement keys off the *rewritten host* path.

> **Consequence:** remapping is consistent across *both* tool kinds, not a
> shell-only convenience. A non-shell tool addressing `/tmp` touches the exact
> same bytes the shell's `/tmp` resolves to. The non-shell tools can't see
> anything the shell can't, and can't write anywhere the shell couldn't — and
> vice versa. The mental model stays simple: one `RemapTable`, one policy.

### Avoiding double policy / drift

- The shell sandbox and the non-shell policy are derived from the **same**
  `paths` config, so they cannot disagree.
- `validate_path` is the only place that interprets `paths` for filesystem
  access. The bwrap wrapper is the only place that interprets `paths` for
  process execution. Both consume `ResolvedBind`, generated once.

### Tests

- A read under an `ro` binding succeeds; the same path written to fails with
  "read-only".
- A path outside all bindings (even inside CWD but not bound) fails with
  "not within a permitted binding" when `paths` is non-empty.
- `move_file` src under `rw` but dst under `ro` fails on the destination check.
- An absolute escape (`/etc/passwd`) is rejected by the traversal guard.
- **Remap consistency:** with `rw: { from: ./scratch, to: /tmp }`, a non-shell
  `write_file("/tmp/x")` and a shell `echo > /tmp/x` resolve to the *same* host
  file (`./scratch/x`); a read of `/tmp/x` via either tool sees the same bytes.
- Empty `paths` + sandbox on keeps the CWD-visible default (read-only) behavior
  for non-shell tools, matching the shell fallback.

---

## Implementation plan (phased)

Each phase is independently reviewable and keeps the tree buildable/testable.

### Phase 0 — Config schema & resolution (no behavior change)

- [ ] Add `SandboxConfig`, `BindSpec`, `BindMode`, `ToolSettings.sandbox` to
      `config.rs`.
- [ ] Implement the `BindSpec` deserializer (explicit `Visitor` for
      `{ro|rw: path}` vs `{mode, from, to}`).
- [ ] Add `ResolvedBind { mode, from: PathBuf, to: PathBuf }` and a
      `resolve_paths(cwd, home, &[BindSpec]) -> Vec<ResolvedBind>` helper that
      expands `~`, resolves relative paths, and validates `from` via
      `validate_path`-style traversal.
- [ ] Add `RemapTable` (ordered `to -> from` map, longest-`to` prefix wins)
      built from the `ResolvedBind` list; this is the single source of truth for
      path translation shared by the shell wrapper and `validate_path`.
- [ ] Add `Config::bash_sandbox() -> SandboxConfig` accessor (merges defaults).
- [ ] Unit tests: parse all four `BindSpec` forms; resolve relative/`~`/absolute.
- [ ] Update `picocode.yaml.example` with the full `sandbox:` block (commented).

### Phase 1 — Timeout + `run_sandboxed` refactor (shell tools)

- [ ] Add `run_sandboxed(cmd, &SandboxConfig) -> Result<String, ToolError>` in
      `tools.rs`.
- [ ] Route `Bash::call` and `agent_browser` through `run_sandboxed`.
- [ ] Apply `duct` `.timeout(Duration::from_secs(timeout_secs))` inside the
      helper (skip when `timeout_secs == 0/None`).
- [ ] Cache `is_tool_available("bwrap")` once; when absent, helper uses plain
      `sh_dangerous` (timeout still applied).
- [ ] Unit/integration tests: `echo` works; `sleep 30` with 1s timeout is
      killed/errors; fallback path executes without bwrap.
- [ ] Verify existing `Bash`/`agent_browser` behavior unchanged when unsandboxed.

### Phase 2 — bwrap sandboxing (shell tools)

- [ ] Implement `build_bwrap_expr(cmd, &SandboxConfig, &[ResolvedBind])`.
- [ ] Deny-by-default root: when `paths` non-empty, bind only declared entries;
      when empty, fall back to `--ro-bind <cwd> <cwd>` + writable tmpfs.
- [ ] Emit flags: per-path `--ro-bind`/`--bind`; `--tmpfs /tmp --size`;
      `--unshare-net` (unless `network`); `--unshare-pid --pid`
      (if `pid_isolation`); `--cap-drop ALL` (if `drop_caps`);
      `--die-with-parent`.
- [ ] Memory wrapper: `sh -c 'ulimit -v <MB>; exec …'` inside the namespace.
- [ ] Default `enabled` to `true` only when bwrap detected; warn on
      enabled-but-absent.
- [ ] Integration tests (tempfile): ro/rw remap round-trip; `network:false`
      blocks connect, `network:true` allows; OOM under `memory_mb`.
- [ ] Wire `tool_config.bash.sandbox` through `build_rig_agent` →
      `guard(Bash, …)` → `run_sandboxed`.

### Phase 3 — Path policy for non-shell tools

- [ ] Change `get_path`/`validate_path` signature to take a resolved policy
      (`&[ResolvedBind]` + `&RemapTable` + root rule). Non-shell tools remain
      **host-direct** (in-process `tokio::fs`); do NOT spawn them inside bwrap.
- [ ] **Remap rewrite (step 0):** before coverage/mode checks, translate the
      requested path through `RemapTable` (`to -> from`) so agent-visible paths
      like `/tmp` resolve to the same host `from` the shell mounts. Longest-`to`
      prefix wins.
- [ ] Implement binding coverage + `ro`/`rw` mode checks (read vs write) on the
      *rewritten host* path.
- [ ] Reject paths outside all bindings when `paths` non-empty (deny-by-default);
      keep CWD-visible read-only fallback when empty.
- [ ] Thread the resolved policy + `RemapTable` from `Config` into every file
      tool (read, write, edit, glob, grep, list, make_dir, remove, move, copy);
      for move/copy check both src and dst (each rewritten + checked).
- [ ] Unit tests: ro-write rejected; unbound path rejected; move dst under ro
      rejected; absolute escape rejected; **remap consistency** (non-shell
      `/tmp/x` == shell `/tmp/x` on the same host file).
- [ ] Ensure the shell and non-shell policies are both generated from the same
      `ResolvedBind` + `RemapTable` (no divergence).

### Phase 4 — Docs, polish, release

- [ ] Document `sandbox:` in README (security properties + caveats).
- [ ] Add a `doc/sandbox.md` link / summary to README.
- [ ] Final integration pass: run the example `picocode.yaml` recipes under
      sandbox on/off.
- [ ] CHANGELOG entry; confirm `cargo test` + `cargo build` green on a host
      with and without bwrap.
