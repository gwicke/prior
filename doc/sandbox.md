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
- **No unsandboxed fallback.** `bwrap` is required at startup (see
  `check_runtime_deps`); if it is absent the process fails closed. A command is
  *never* run outside the sandbox — if the resolved sandbox config is disabled
  or unset, the command is refused rather than executed by a plain shell.

---

## Configuration

Extend `Config` so that sandbox settings live under the existing
`tool_config.<tool>` map (the same place as `auto_allow`):

```toml
[tool_config.bash]
auto_allow = ["^cargo test"]

[tool_config.bash.sandbox]
# No `enabled` switch: the sandbox is always active when `bwrap` is present
# (and `bwrap` is required at startup, so prior fails closed without it).
timeout_secs = 120        # default: 120
network = false           # default: false (no egress; netns unshared)
# network = true          # public destinations only, STILL netns-unshared
# [network]               # optional narrowing on top of `true`
# enabled = true
# allow = ["github.com:443"]
memory_mb = 1024          # default: none (unlimited)
pid_isolation = true      # default: true (--unshare-pid --pid)
drop_caps = true           # default: true (--cap-drop ALL)
# Bindings are split into two lists: `ro` (read-only) and `rw` (read-write).
# Each entry is a bare PATH (identical host/in-sandbox path) or a remap
# `{ from = "HOST", to = "SANDBOX" }`.
ro = [
  ".",
  "/usr",
  { from = "~/.cargo", to = "/opt/cargo" },
]
rw = [
  ".",
  "./target",
  { from = "./cache", to = "/var/cache/app" },
]
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
    /// Wall-clock timeout for the whole command. Default 120s. 0/None = no timeout.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Network egress policy: bare bool or `{ enabled, allow }`.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// RLIMIT_AS in MB (virtual memory cap). Default none.
    #[serde(default)]
    pub memory_mb: Option<u64>,
    /// Run in a fresh PID namespace. Default true.
    #[serde(default)]
    pub pid_isolation: Option<bool>,
    /// Drop all capabilities. Default true.
    #[serde(default)]
    pub drop_caps: Option<bool>,
    /// Read-only filesystem bindings (bare path or `{ from, to }` remap).
    #[serde(default)]
    pub ro: Vec<BindPath>,
    /// Read-write filesystem bindings (same shape as `ro`).
    #[serde(default)]
    pub rw: Vec<BindPath>,
}

/// A single filesystem binding, parsed from a bare path or a `{ from, to }`
/// remap. The read/write mode is implied by which list (`ro` vs `rw`) the
/// binding is placed in, so it is not stored here.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum BindPath {
    /// Bare path, mounted read-only or read-write depending on the list.
    Simple(String),
    /// Explicit host→sandbox remap.
    Remap { from: String, to: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum BindMode { Ro, Rw }
```

> **Note on the two-list design:** Instead of a single `paths` array where each
> entry carries its own mode, bindings are split into two lists — `ro` and `rw`
> — and the mode is implied by which list the entry lives in. Each entry is a
> plain path string or a `{ from, to }` remap table. This is simpler to parse (a
> plain `untagged` enum) and removes the per-entry `mode`/`ro`/`rw` ambiguity.

---

## The `paths` model

Each entry in the `ro`/`rw` lists declares one filesystem binding. Semantics
mirror `bwrap` (the list name selects the flag):

- **Config**: `P` (in `ro`)
  - **bwrap flag**: `--ro-bind P P`
  - **Meaning**: Host `P` mounted read-only at the **same** path in-sandbox.
- **Config**: `P` (in `rw`)
  - **bwrap flag**: `--bind P P`
  - **Meaning**: Host `P` mounted read-write at the **same** path.
- **Config**: `{from,to}` (in `ro`)
  - **bwrap flag**: `--ro-bind from to`
  - **Meaning**: Host `from` mounted read-only at in-sandbox `to`.
- **Config**: `{from,to}` (in `rw`)
  - **bwrap flag**: `--bind from to`
  - **Meaning**: Host `from` mounted read-write at in-sandbox `to`.

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
5. **Ordering / precedence.** On overlap, the **most-specific (deepest
   in-sandbox `to`) binding wins**, regardless of the order you list bindings
   in. `build_bwrap_expr` sorts bindings by `to` depth so the deepest is
   mounted last (bwrap's "last mount wins"), and `PathPolicy::check` resolves
   overlaps by `to` specificity via the remap table — so the shell sandbox and
   the non-shell file tools always agree. A `rw` entry for a subtree of a
   `ro` entry therefore upgrades just that subtree to writable; a `ro` entry
   for a subtree of a `rw` entry restricts just that subtree. Do **not** rely
   on config order to get the right result — the deepest binding is
   authoritative either way.

### Remap semantics (the interesting case)

Remapping lets the agent see a host directory at a **different** location than
on the host. Example: the host cache lives at `/home/user/.cache/foo` but the
tool inside the sandbox expects `/var/cache/foo`:

```toml
[[tool_config.bash.sandbox.paths]]
rw = { from = "~/.cache/foo", to = "/var/cache/foo" }
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
    // 1. Build the duct expression. There is no unsandboxed fallback and no
    //    enable switch: the sandbox is always active, so a present `cfg` always
    //    routes through bwrap. A missing config is refused rather than run by a
    //    plain shell.
    let expr = if cfg.is_present() {
        build_bwrap_expr(cmd, cfg)   // returns a duct::Expression wrapping bwrap
    } else {
        return Err(ToolError::Generic("sandbox not configured; refusing to run unsandboxed"));
    };

    // 2. Always apply a timeout (duct implements this with a killer thread
    //    + process-group kill, so a hung child is reaped and the blocking
    //    thread is released).
    let expr = match cfg.timeout_secs {
        Some(0) | None => expr,
        Some(s) => expr.timeout(std::time::Duration::from_secs(s)),
    };

    // 3. Capture combined output. Output is streamed in bounded chunks and
    //    capped to `cfg.max_output_bytes` (configurable via
    //    `[tool_config.bash.sandbox].max_output_bytes`; default 20 KiB). Beyond
    //    the cap the pipe is still drained but surplus bytes are discarded, so a
    //    chatty command can never balloon memory.
    tokio::task::spawn_blocking(move || {
        expr.stderr_to_stdout().unchecked().reader().read_bounded(cfg.max_output_bytes())
            .map_err(|e| ToolError::Io(e.to_string()))
    }).await??
}
```

### bwrap command construction

```text
bwrap
  --ro-bind <cwd> <cwd>            # baseline: CWD visible read-only (if no path overrides)
  [for each path in cfg.ro]   --ro-bind <from> <to>
  [for each path in cfg.rw]   --bind   <from> <to>
  --dev /dev --proc /proc               # fresh instances; skipped if bound explicitly
  --ro-bind /etc/<entry> …              # ESSENTIAL_ETC working set (not all of /etc)
  --bind <base>/prior-<session-uuid> /tmp
                                       # per-session; <base> is a `/tmp`
                                       # remap's host side, else
                                       # <data_dir>/sandbox-tmp. Replaces
                                       # the old `--tmpfs /tmp`; see "/tmp
                                       # is a per-session directory".
                                       # [--tmpfs /tmp] only for one-shot
                                       # runs with no session
  --new-session                         # ALWAYS: no controlling terminal
  --unshare-net                         # ALWAYS (R3a): egress, if any, is bridged
  [--unshare-pid --pid]                 # if pid_isolation (default true)
  --unshare-ipc --unshare-uts --hostname sandbox
  [--cap-drop ALL]                      # if drop_caps (default true)
  --die-with-parent
  --seccomp <fd>                        # R3d: deny AF_VSOCK + io_uring (fresh memfd per spawn)
  -- prlimit --as=<memory_mb*1024*1024> -- <cmd argv>   # memory cap via direct argv (see below)
```

- **Memory limit.** bwrap has no direct RLIMIT flag. We set `RLIMIT_AS` with
  `prlimit` (util-linux), which execs the target via a **direct argv list** — no
  shell, so the limit step is never interpreted by a shell. `prlimit --as` takes
  bytes, so `memory_mb` is converted to `memory_mb * 1024 * 1024`. Both the
  shell path (`sh -c <cmd>`) and the argv path (`<program> <argv…>`) are wrapped
  the same way, so the cap is enforced on both. (`RLIMIT_AS` + `--unshare-pid`
  scopes the limit to the command's process tree.)
  - **Fallback.** If `prlimit` is absent on the host (it lives under `/usr/bin`,
    bound read-only into the namespace), the shell path falls back to the shell
    builtin `ulimit -v <memory_mb*1024>; exec sh -c <cmd>` (still a cap, but via
    a shell). The argv path has no shell, so it simply runs uncapped when
    `prlimit` is missing — matching the prior behavior where that path never
    enforced a memory limit.
- **Timeout vs memory.** If memory is exceeded the process is killed by the
  kernel (OOM within the limit), not by the timeout. The timeout covers hangs;
  the memory cap covers runaway allocation.
- **`--die-with-parent`** ensures orphans are cleaned up if prior dies.
- **Exit status.** `unchecked()` keeps duct from erroring on non-zero exit; the
  command's stdout/stderr is returned and the agent sees the real exit
  behavior through output. (Optionally surface a non-zero exit as a
  `ToolError::Generic` with the captured output — TBD per UX review.)

### Environment isolation

bwrap **inherits the parent environment verbatim** by default — it applies no
env filtering of its own. prior runs with provider API keys in its process
environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `HF_TOKEN`, `AWS_*`, ...),
plus any host secrets the operator's shell holds (`SSH_AUTH_SOCK`,
`GITHUB_TOKEN`, ...) and the host's desktop session variables (`DISPLAY`,
`DBUS_SESSION_BUS_ADDRESS`, ...). Without explicit sanitization, a sandboxed
LLM command could read those secrets. The sandbox therefore applies an
**environment policy** on top of bwrap's filesystem/network/PID/cap isolation,
and spawns bwrap itself with a scrubbed environment.

The policy is configured under `sandbox.env` as a single mutually-exclusive
mode (you cannot combine a blacklist and a whitelist in one block):

```toml
# Whitelist (default when `env` is unset): keep ONLY matching names;
# everything else dropped. Glob `*`,`?` plus literal.
[tool_config.bash.sandbox.env]
mode = "only_allow"
patterns = ["PATH", "HOME", "TERM", "LANG", "CARGO_HOME", "RUSTUP_HOME"]
# -- or -- Blacklist: start from the full env, drop names matching any pattern.
# [tool_config.bash.sandbox.env]
# mode = "disallow"
# patterns = ["*_KEY", "*_SECRET", "*_TOKEN", "AWS_*", "SSH_AUTH_SOCK"]
```

- `only_allow` keeps only the matches. An **empty** `patterns` list wipes the
  environment (only `PWD` survives bwrap's `--clearenv`); commands that need
  `PATH` will fail, so whitelists must include it.
- `disallow` keeps the whole environment minus the matches. An **empty**
  `patterns` list keeps everything (a debug / allow-all escape hatch).

**Default.** When `sandbox.env` is omitted entirely, prior applies the
built-in **allowlist** (`config::DEFAULT_ENV_ALLOW`):

```
PATH, HOME, TERM, LANG, LC_*, TZ, USER, LOGNAME, SHELL,
CARGO_HOME, RUSTUP_HOME, RUSTUP_TOOLCHAIN,
XDG_CACHE_HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME,
SOURCE_DATE_EPOCH
```

A denylist cannot be the default: it only removes the secret shapes it knows
about and passes everything else through. That includes the host's *desktop*
variables — `DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`,
`XDG_RUNTIME_DIR`, `GPG_AGENT_INFO`, `SSH_AGENT_PID` — which are not secrets
but are pointers to the host's X11 and IPC endpoints, and which make those
endpoints reachable from inside the sandbox. See
`doc/261021_sandbox_env_x11_audit.md` (Finding 2) for the measurements.

The sandbox additionally sets `TMPDIR=/tmp` itself, pointing at its own
per-session directory rather than the host's `/tmp` (which is not mapped in). It
does not inherit the host's `TMPDIR`, and `TMPDIR` is deliberately not on the
allowlist.
`PWD` is likewise absent: bwrap's `--clearenv` sets it to the in-namespace
cwd, which is what the command should see.

An explicit `sandbox.env` block is authoritative in full — the built-in
allowlist and the forced `TMPDIR` are not mixed in. An operator who wants the
old denylist behaviour can ask for it explicitly:

```toml
[tool_config.bash.sandbox.env]
mode = "disallow"
patterns = ["*_KEY", "*_SECRET", "*_TOKEN", "AWS_*", "SSH_AUTH_SOCK"]
```

**Mechanism.** `build_bwrap_expr` always emits `--clearenv` (which retains
only `PWD`) followed by `--setenv NAME VALUE` for each variable the policy
permits. This makes the policy explicit and verifiable (inspect the `--setenv`
args) rather than relying on bwrap's inheritance behavior.

**bwrap's own environment is scrubbed too.** `--clearenv` only clears the
environment of bwrap's *child*. With `--unshare-pid`, bwrap is PID 1 inside
the namespace, so its own environment is readable at `/proc/1/environ` by
anything running in the sandbox — and if bwrap inherited the host
environment, the command could read every secret the policy dropped, straight
out of `/proc/1/environ`. prior therefore spawns bwrap with a scrubbed
environment of its own (`config::bwrap_scrub_env`): `PATH` and nothing else,
so the `bwrap` binary can be found. The secret never reaches bwrap, so there
is nothing for `/proc/1/environ` to leak.

> **Caveat:** the `--setenv` values the policy permits are still visible in
> `/proc/1/cmdline` (bwrap's argument vector). That is host fingerprinting,
> not secret leakage. A binding that exposes a host *socket* path would make
> that socket reachable regardless of the env policy.

### The sandbox has no terminal (R7)

`--new-session` (always emitted) and a redirected stdin (always
`Stdio::null()`) together keep a sandboxed command away from prior's terminal.
They are two different controls and neither is sufficient alone:

- **`--new-session` prevents *acquiring* a terminal.** Without it the child
  could `open("/dev/tty")` and get prior's own terminal device. It also stops
  the child from becoming a session leader or signalling prior's process group.
- **The redirected stdin prevents *inheriting* one.** `--new-session` cannot
  revoke a file descriptor that bwrap already inherited. `duct`'s default stdin
  is the *parent's* fd 0 — prior's terminal, opened read-write — and bwrap
  forwards inherited stdio verbatim, so a command would get a live, duplex pty
  even with `--new-session` set. Both bwrap builders therefore end with
  `.stdin_null()`, and the `ls`-for-completions path sets
  `Stdio::null()` explicitly (it uses `std::process::Command`, whose `output()`
  does not redirect stdin either).

Without this, a sandboxed command could read the user's keystrokes (prior runs
the terminal in raw mode, so every key typed while a tool call runs is
readable), paint the terminal with escape sequences to forge a confirmation
prompt or assistant message, resize it, or change its `termios` flags.
Measured on a live sandbox: fd 0 was `/dev/pts/5` and answered a device-
attribute query; `TIOCSWINSZ` and `tcsetattr` both succeeded. See
`doc/261021_sandbox_env_x11_audit.md` (Finding 4).

No tool needs stdin: every sandboxed call is non-interactive, `--clearenv`
removes anything that would prompt, and the egress bridge uses fd 3. Closing it
is therefore behaviour-neutral.

### The sandbox inherits no other fd (R8)

bwrap forwards every inherited non-`CLOEXEC` descriptor into the payload, and
neither `--clearenv`, `--unshare-*` nor `--cap-drop` says anything about file
descriptors. A session log, memmap or socket prior happened to hold open would
otherwise be readable and writable from inside the sandbox.

`seal_inherited_fds` (`src/tools.rs`) is installed as a `pre_exec` hook on every
bwrap spawn (both `duct` builders and the `ls`-for-completions path). It runs in
the forked child, after std has installed stdio, and marks every descriptor ≥ 3
`CLOEXEC` with `close_range(3, UINT_MAX, CLOSE_RANGE_CLOEXEC)`, then clears the
flag again on the one fd the payload is meant to keep — the egress bootstrap fd,
and only when egress is on. So the payload sees stdio plus, at most, that fd.

Two details are load-bearing:

- **Marking, not closing.** A plain `close_range(3, …, 0)` would also close
  std's own `CLOEXEC` exec-error pipe, turning a clean `ENOENT` on a failed
  `execve` into a runtime abort. `CLOSE_RANGE_CLOEXEC` leaves it open until
  `execve`, where the kernel's own `CLOEXEC` handling closes it.
- **Linux 5.11+.** `close_range` is required; on `ENOSYS` the spawn fails rather
  than leaking the table. prior already requires bwrap, so the floor is cheap.

See `doc/261021_sandbox_env_x11_audit.md` (Finding 5).

### `/tmp` is a per-session directory

Before this change `/tmp` was one of two things, and neither was right:

- **No binding targeted `/tmp`** — it was a fresh 256 MB `--tmpfs`, thrown away
  after every command. Nothing persisted between two `bash` calls in one
  session, so a build artefact could not be made and then used.
- **A binding remapped `/tmp`** — e.g. `rw = [{ from = "~/tmp/agents", to = "/tmp" }]`.
  Then every session, in every project, shared one host directory. `/tmp`
  files leaked *between unrelated sessions*, and a stray `.py`/`.rs` there
  could shadow a stdlib module for whatever ran next.

One rule replaces both: **`/tmp` is a directory named for the session, created
by prior, bound over `/tmp`.**

#### Name and location

```text
<data_dir>/sandbox-tmp/prior-<session-uuid>/
```

`<data_dir>` is the per-user directory prior already resolves for history and
settings (`platform::app_config_dir()`, i.e. `$XDG_CONFIG_HOME/prior` or
`~/.config/prior`). Reusing it keeps the location predictable and avoids
inventing a second data-root; `sandbox-tmp/` is its one subdirectory that is
disposable rather than configuration.

- **`prior-` prefix.** The directory must be identifiable and prunable amid
  unrelated files. This is the same reason a pid file carries the program name:
  an operator can `rm -rf` a stale one without knowing prior's internals, and a
  sweeper can list exactly the entries that belong to prior.
- **Session uuid only.** Not the cwd, and not the two combined. Sessions can move
  between working directories (parallel checkouts, `--resume` from elsewhere),
  and the session id is the thing that survives the move — so keying on it keeps
  `/tmp` continuous through a cwd change. Keying on the cwd would deliberately
  *break* that continuity.
- **Scoped to the session, not the cwd.** Cross-session `/tmp` sharing is not a
  real use case: durable artefacts belong in the repository or the cwd, and
  anything else is added deliberately with `/sandbox add rw <path>`. Choosing
  per-session also gives cleanup a single owner and a single lifetime — the
  directory is garbage once the session is gone, and nothing has to reason about
  which other sessions still want it.
- Mode `0700`. The parent is only traversable by the user, so the sandbox cannot
  reach a sibling session's directory by `..` (measured: the bound subdir's
  parent is the sandbox's own namespace root, which contains no other session).

#### Lifetime and cleanup

The directory belongs to exactly one session id, so it is dead as soon as that
session's record is gone. Removal happens at the two points that already destroy
a session, plus a sweeper for the rest:

- **`/clear`** deletes the outgoing session file and starts a fresh session with
  a new id. The id changes, so the old directory is unreachable and its files
  are dead; remove it at the switch.
- **`/session <id> delete`** removes another session from disk. It already
  refuses the current session (`SessionDeleteNode::prepare`), so the
  current-session directory can never be swept out from under a running
  command. Remove the corresponding directory alongside `repo.delete`.
- **mtime sweep**, for what neither point reaches: a session deleted while prior
  was not running, a crash between the file write and the directory removal, a
  `/tmp` directory whose session file was removed by hand. Once per startup,
  list `<data_dir>/sandbox-tmp/`, and remove entries older than a generous age
  (a week is the right order of magnitude — long enough that a session left open
  overnight is safe, short enough that the directory does not accumulate).

All three are **best-effort and must never fail the operation that triggered
them.** A failed `unlink` is a leaked directory, which the sweeper will catch; a
failed `rm -rf` that aborted `/clear` would be a far worse outcome. This
mirrors the existing contract for session-file deletion.

Only entries matching `prior-<uuid>` are touched, which is the point of the
prefix: the sweeper removes prior's directories and nothing else, even if
`sandbox-tmp/` ends up sharing a parent with unrelated files.

#### It replaces the tmpfs, it does not sit on top of it

The effective `/tmp` destination is resolved first, exactly as bindings are
resolved today:

1. a binding with `to == "/tmp"` whose `from` is a directory (or does not exist
   yet — `create_dir_all` makes it) — use that directory as the base;
2. no such binding — use `<data_dir>/sandbox-tmp/` as the base;
3. a binding with `to == "/tmp"` whose `from` is a **file** — no writable `/tmp`
   exists in that layout. Rejected with a clear error rather than emitting a
   namespace whose `/tmp` is a file.

Two cases fall back to the throwaway tmpfs instead of a session directory,
because there is no name to give it:

- **No session.** A one-shot `--input` / recipe run has no session id and
  persists nothing between commands, so a tmpfs loses nothing.
- **No config directory.** With neither `HOME` nor `XDG_CONFIG_HOME` there is
  nowhere durable to put `sandbox-tmp/`; a hard error would refuse to run the
  sandbox at all, which is the wrong trade. It falls back to the tmpfs rather
  than failing the command.

prior then creates `<base>/prior-<uuid>/` (in the parent, before the spawn —
bwrap hard-errors on a missing bind source: `Can't find source path`), and emits
**one** `--bind <base>/prior-<uuid> /tmp`. It does not also emit `--tmpfs /tmp`:
two mounts at `/tmp` would only leave the later one visible, and `--size` is
rejected by bwrap when followed by a bind rather than `--tmpfs`.

This replaced the old `tmp_covered` test (`src/tools.rs`), which suppressed the
tmpfs whenever *any* binding targeted `/tmp` **or a path under it**. That was too
broad: a binding at `to = "/tmp/scratch"` suppressed the `/tmp` mount and left
the rest of `/tmp` unwritable. Sub-path bindings are still emitted, just
**after** the `/tmp` bind so the deeper mount wins — load-bearing, and the trap
that bit this implementation: `Path::starts_with` compares components, so
`/tmp`.starts_with(`/tmp/`) is *true*; without an explicit `!= /tmp` in the
sub-binding test, the base binding is deferred last and shadows the session bind,
silently re-exposing the whole base directory in `/tmp`'s place.

Consequences where the old behaviour is described:

- **`tmp_size_mb` is retired** (done). With `/tmp` a host directory it had no
  meaning: the size is the host filesystem's and there is no tmpfs left to cap.
  The field, its `SandboxTmpSizeMb` deserializer, the `--size` emission and the
  example line are gone. `SandboxConfig` is **not** `deny_unknown_fields`, so a
  user config that still carries `tmp_size_mb` parses cleanly and ignores the
  key — no migration, no warning: it was advisory sizing, not a control. A
  one-shot run with no session (see below) still gets a fixed-size throwaway
  tmpfs, now hard-coded at 256 MB.
- The default config (no `/tmp` binding) previously gave each *call* a clean
  `/tmp`. It now gives each *session* one. That is the intended change, but it
  means a command can now observe what an earlier command in the same session
  left in `/tmp`.

#### The non-shell tools must see the same directory

`read`/`write`/`edit` resolve paths through `PathPolicy`, which maps
agent-visible paths to host paths via `RemapTable` (see "Path translation via
`RemapTable`", below). If `/tmp` becomes
`<base>/prior-<uuid>` while `PathPolicy` still believes `/tmp` is either
unmapped (→ `validate_path` rejects it as outside the cwd) or the old remap
target, the two worlds disagree in the worst way: `bash` creates `/tmp/f` and
the `write` tool then cannot see it, or writes it to a different place.

So the session tmp directory participates in the same resolution as any other
binding: the effective `/tmp` entry of the policy becomes
`{ from: "<base>/prior-<uuid>", to: "/tmp" }`, constructed from the *same*
resolved base the bwrap argv uses. One function resolves `/tmp`, and both the
argv builder and the policy builder call it — the consistency
`doc/path_leak_risks.md` already requires for every other remap.

The id is **read live at spawn**, not captured into the published config. This
started as a worry — `set_sandbox` publishes the config once, so a captured id
would go stale on `/clear` — but reading live is what we want: `/clear` and
`/session … resume` genuinely switch sessions, and `/tmp` must follow. The id is
published by `AppState` on every `current_id` write (`set_current_session_id`,
`activate_session`) into a process-global slot that `AppState` already owns; the
sandbox layer reads it at spawn. So the directory is stable *within* a session —
nothing writes the id between its commands — and moves exactly when the session
does. A test that publishes an id and then switches it asserts the isolation.

The `/tmp` mapping for the file tools when **no** binding list is set needs one
more piece: there is no `PathPolicy` in the permissive default, so `get_path_with`
asks `sandbox_tmp::tool_path("/tmp/…")` directly and maps into the session
directory, normalizing `..` and rejecting anything that escapes it.

#### Tests

- Two consecutive `bash` calls in one session see each other's `/tmp` files
  (`two_calls_in_one_session_share_tmp`).
- Two sessions do not (a marker written by one is absent in the other;
  `two_sessions_do_not_share_tmp`).
- A `/tmp` remap is honoured: the session directory is created *inside* the
  configured base, not beside it (`a_session_makes_a_directory_under_the_base`).
- The `write` tool writes to the same host file a subsequent `bash` `cat /tmp/f`
  returns, with the session directory under the remap base
  (`nonshell_remap_consistency`).
- A binding under `/tmp` is emitted after the `/tmp` mount and wins
  (`tmp_bind_precedes_a_sub_tmp_bind`) — the ordering that the misclassified
  base silently breaks.
- Naming (`dir_name` / `session_id_of`), the file-remap rejection, and `0700`
  creation live in `sandbox_tmp`'s own tests.

A resumed session keeps its `/tmp` across a cwd change because the id does not
change; this is a property of the key rather than a separate test.

### Path-binding special case

When either binding list (`cfg.ro` or `cfg.rw`) is **non-empty**, do **not**
add the implicit `--ro-bind <cwd> <cwd>` baseline; the operator's lists are
authoritative (deny-by-default). When both lists are empty and sandboxing is on,
fall back to a single `--ro-bind <cwd> <cwd>` plus a writable `/tmp`, so the
common case "run commands in my project, read-only, with a scratch /tmp" works
with zero config. That `/tmp` is the per-session directory described above, not
a tmpfs.

### seccomp: AF_VSOCK and io_uring (R3d)

The network namespace is the network fix, not this. The filter closes the two
ways a command can reach a socket *around* `connect()` and therefore around both
the netns and the SOCKS bridge:

- **`socket(AF_VSOCK, …)`** — VM sockets are addressed by CID, not routed
  through the netns, so the namespace does not fence them.
- **`io_uring_setup` / `io_uring_enter` / `io_uring_register`** — io_uring
  builds and connects sockets in kernel context, bypassing a `connect()`-based
  rule and the socket-path netns check.

Both return `EPERM`, which is indistinguishable from a socket family the kernel
does not have — a denied call looks like "not supported here", not like a probe.
Everything else is allowed. **`AF_UNIX` is deliberately not denied**: the netns
already removed the abstract X11 socket, and the bridge and `agent-browser`
depend on Unix sockets.

`src/seccomp.rs` assembles a 12-instruction classic-BPF program and writes it to
a **fresh memfd per spawn**, passed as `--seccomp <fd>`. Two details are
load-bearing and both are covered by tests:

- **Fresh fd per spawn.** `bwrap` reads the fd to EOF; a reused fd is at EOF on
  the second spawn and the kernel then rejects the filter with `EINVAL`.
- **It survives the R8 seal.** The fd is `CLOEXEC`-cleared and added to the
  seal's keep list, or R8 would close it before bwrap reads it.

No libseccomp dependency: `bwrap --seccomp` takes a raw program, and the jump
targets are checked by a test that walks the program per syscall and per socket
family.

The bridge shim also sets `PR_SET_PDEATHSIG(SIGKILL)` at startup, so it dies
with bwrap instead of holding an accept queue open in a namespace whose
supervisor is gone. See `doc/261021_sandbox_env_x11_audit.md` (R3d).

### Egress: one port, two proxy protocols (R3i)

`network = true` gives the sandbox exactly one route out: the `prior-egress`
bridge listening on the namespace loopback. The listener speaks **both** SOCKS5
and HTTP proxy on the same port, told apart by the first byte — `0x05` is a
SOCKS5 greeting, anything else is an HTTP request. The two cannot collide,
because a SOCKS version byte is not an ASCII letter.

Serving both is what makes a single proxy variable work for every client.
SOCKS-aware tools (`curl`, `git`, `cargo`, python `requests`) can use SOCKS5;
many others cannot, and a client that cannot parse a variable's scheme **fails
rather than falling through** (measured: `w3m` given `HTTP_PROXY=socks5h://…`
dies, it does not try `http_proxy`).

The published variables (`egress::proxy_var_specs`, each in upper and lower
case, because no client reads them all):

| variable | value | why |
|---|---|---|
| `ALL_PROXY`, `all_proxy` | `socks5h://127.0.0.1:<port>` | the only spelling that resolves the name proxy-side, so the sandbox never learns an unroutable address |
| `HTTP_PROXY`, `http_proxy` | `http://127.0.0.1:<port>` | must be parseable by clients with no SOCKS code (`w3m`, `wget`) |
| `HTTPS_PROXY`, `https_proxy` | `http://127.0.0.1:<port>` | same; HTTPS still arrives as `CONNECT` |
| `NO_PROXY`, `no_proxy` | `localhost,127.0.0.1,::1` | bypass for loopback destinations: `--unshare-net` is unconditional, so a direct loopback connect lands on the **netns's own** loopback (the sandbox's services or a refused connect), never the host's |

`NO_PROXY` bypasses for loopback destinations only, and the bypass is
fail-closed toward the host: direct means the netns loopback, so the sandbox
reaches its own services — a test's mock provider, a dev server — or nothing.
The one thing given up is the literal-destination route to *host* loopback
(`127.0.0.1:5432` allowlist entries dialled by prior on the host side); that
reach is denied by default anyway (R3j, see the security properties), and
services on the host meant to be reachable listen on a non-loopback address,
which `NO_PROXY` does not cover. Destination matching only: the variable never
affects a client reaching the bridge itself, whose address arrives in the proxy
variables rather than as a destination.

The HTTP engine handles the two request forms a proxy sees:

- **`CONNECT host:port`** (used by every client for `https://`) — reply
  `200 Connection established`, then splice bytes untouched. No TLS involvement,
  so **keep-alive inside the tunnel is preserved**. A `git https` clone is only
  two requests on one connection, so the bridge costs one RTT at setup and
  nothing after.
- **absolute-form `GET http://host/path`** (plain `http://`, the `w3m`/`wget`
  path) — dial the request-line authority, rewrite to origin-form, force
  `Connection: close`, relay to EOF. Forcing close avoids writing an HTTP/1.1
  body parser to find the end of a response; the cost is one connection per
  plain-HTTP resource, which every CLI client here already does.

For absolute-form the **request-line authority** is the allowlist input, and a
disagreeing `Host:` header is ignored — deciding on one name while dialling
another is a request-smuggling vector.

The accept loop serves conversations **concurrently** (thread per connection,
capped at 64 live). Serving inline previously serialised the bridge; HTTP
clients open parallel connections by default, so one slow upstream would stall
everything behind it.

---

## Fallback & detection

- `bwrap` is required at startup (`check_runtime_deps` fails closed if it is
  absent), so the tool layer can assume it is always present.
- There is **no unsandboxed fallback and no enable switch.** The sandbox is
  always active whenever a config is present. If no sandbox config is set,
  `run_sandboxed` refuses the command outright (returns an error) rather than
  executing it via a plain shell. Sandboxing is always the failure mode — never
  a degraded, unsandboxed run.

---

## Security properties & caveats

- **Deny-by-default root** is the key guarantee: without an explicit binding,
  host files are invisible. Never use `--ro-bind / /`.
- **The network namespace is never shared.** `--unshare-net` is emitted
  regardless of `network`, and `network = true` means "egress *through the
  bridge*", not "join the host's namespace" (R3a). This is what makes
  `network = true` a namespace guarantee: the abstract X11 socket
  (`\0/tmp/.X11-unix/X0`) is unreachable because nothing routes to it.
  - **Local and non-public destinations are denied by default (R3j).** The
    bridge reaches *host* loopback (it is the one route to a host service), so
    "empty allowlist" would otherwise mean "any host daemon": loopback, RFC 1918,
    CGNAT, link-local (cloud metadata `169.254.169.254`), IPv6 ULA, multicast.
    These are refused unless an `allow` entry names the destination literally
    (`allow = ["127.0.0.1:5432"]`), and the check runs on the *resolved* address,
    so a public name rebinding to loopback is refused too. An empty allowlist
    therefore means *public internet only*.
  - Two socket kinds, two mechanisms. *Abstract* sockets live in the network
    namespace, so unsharing is necessary and sufficient. *Pathname* sockets
    (`/run/user/*/bus`) live in the filesystem, which the netns does not
    govern — they are excluded by deny-default bindings, and R4's Landlock work
    is the belt for the case where a binding re-exposes one. An allowlist is a
    destination filter on top; it is never the safety mechanism.
- **`/etc` is a working set, not the whole directory.** `ESSENTIAL_ETC` binds
  the files a normal command needs (`/etc/alternatives`, the loader cache, NSS
  config, the CA store, timezone, shell profile). The host's `/etc/passwd`,
  `/etc/shadow` and `/etc/os-release` are not visible. Absent entries are
  skipped, so a minimal `/etc` still boots.
- **Fresh `/dev` and `/proc`** are mounted by default. Mounting `/proc` is why
  the bwrap env scrub matters: `/proc/1/environ` is bwrap's own environment.
- **Capability drop + PID namespace** limit privilege escalation and hide the
  host process tree.
- **No terminal.** `--new-session` plus a null stdin keep the sandbox away from
  prior's tty: no keystroke readback, no escape-sequence injection into the
  user's screen, no `termios`/winsize tampering. See "The sandbox has no
  terminal" above.
- **Only stdio (and the egress fd) cross into the sandbox.** Every other
  inherited descriptor is marked `CLOEXEC` in the child before `execve`, so an
  open session log, memmap or socket cannot be read or written from inside. See
  "The sandbox inherits no other fd" above.
- **One route out, two proxy protocols.** Egress is only through the bridge, and
  the bridge speaks SOCKS5 and HTTP proxy so that clients with no SOCKS support
  (`w3m`, `wget`) are not silently cut off. The allowlist governs `host:port` in
  both paths. See "Egress: one port, two proxy protocols" above.
- **`AF_VSOCK` and `io_uring` cannot reach the network.** Both route around
  `connect()` and so around the netns and the bridge; the seccomp filter denies
  them with `EPERM`. `AF_UNIX`/`AF_INET`/`AF_INET6` still work. See "seccomp:
  AF_VSOCK and io_uring" above.
- **`/tmp` is per session, not shared.** Each session gets
  `<data_dir>/sandbox-tmp/prior-<uuid>` bound over `/tmp`, mode `0700`. A
  command cannot see what another session left in `/tmp`, and no `/tmp` file is
  the host's. Within one session it persists across calls, which is the point.
  See "`/tmp` is a per-session directory" above.
- **Not a hardening boundary against root.** bwrap provides isolation, not a
  bulletproof jail; it assumes the sandbox user is unprivileged. It is a
  defense-in-depth measure appropriate for an LLM-driven shell, not a
  substitute for running prior as a non-root user.
- **`from` paths should be validated** against path traversal (reuse
  `validate_path` from `tools.rs`) so a malicious/erroneous config can't bind
  `/etc` or `/` unexpectedly. The remap `to` is created inside the namespace,
  so it cannot escape the sandbox root.

---

## Testing plan

- **Unit:** parse `BindPath` entries (bare path and `{ from, to }` remap) from
  the `ro`/`rw` lists; assert `from`/`to` resolution (relative→CWD, `~`→home)
  and that mode follows the list (ro→read-only, rw→read-write).
- **Unit:** `build_bwrap_expr` produces the expected flag sequence for a
  representative `SandboxConfig` (assert arg vector contains
  `--ro-bind`, `--bind`, `--unshare-net`, `--cap-drop ALL`, memory wrapper).
- **Integration (tempfile):** run `run_sandboxed("echo hi")` and assert output;
  run a command that exceeds `timeout_secs` (e.g. `sleep 30`) with a 1s timeout
  and assert it returns an error / is killed.
- **Integration:** with a `rw` remap binding, assert a file written inside the
  sandbox at `to` appears at host `from` (and not at `to` on the host).
- **Integration:** with `network: false`, assert a direct socket connect fails;
  with `network: true`, assert a *direct* connect still fails (the netns is
  still unshared) but a proxied request succeeds through the `prior-egress`
  bridge, and the abstract X11 socket stays unreachable in both cases. These are
  `egress_bridge_carries_traffic_out_of_the_netns`,
  `direct_tcp_egress_is_blocked_even_with_network_enabled`, and
  `egress_allowlist_denies_unlisted_destination` in `src/tools.rs`.
- **Integration:** clients with **no SOCKS support** reach the bridge over the
  HTTP-proxy front end: `http_only_clients_reach_the_bridge` fetches a
  host-loopback upstream with real `wget` and `w3m` from inside the sandbox.
  The HTTP engine's rewrite, `CONNECT` tunnel, request-line routing and
  `split_authority` are unit-tested in `src/egress.rs`.
- **Integration:** the bridge serves conversations concurrently —
  `bridge_serves_conversations_concurrently` asserts four requests against an
  origin with a 300 ms delay finish in well under the serial 4×300 ms.
- **Integration:** the seccomp filter is enforced, not just present:
  `sandbox_enforces_seccomp_filter` asserts `socket(AF_VSOCK)` fails with
  `EPERM` while `AF_INET`/`AF_UNIX` still succeed. The program's exact verdicts
  and on-wire layout are unit-tested in `src/seccomp.rs`.
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
resolved `paths` config. It proceeds in two clearly separated phases — *expand
to a sandbox path* (`PathPolicy::resolve_sandbox`), then *authorize and
translate to a host path* (`PathPolicy::resolve_host_arg`):

1. **Expand to an absolute sandbox path.** This is what the shell would compute
   for the same argument, and it is the only place argument semantics live:
   - `~` / `~/` expands to the **sandbox `HOME`** — the value the sandboxed
     shell will expand it to (`sandbox.env` policy; see below), *not* the host
     process's `HOME` unconditionally. If the sandbox has no `HOME`, `~` is left
     unexpanded, matching a shell with no `HOME`.
   - a relative path joins the **sandbox CWD** — the host CWD mapped through the
     remap table (`to` side). For an unremapped cwd this is the cwd itself; for
     a remapped cwd it is the namespace path, so a relative argument still lands
     under what the shell sees.
   - `..` / `.` are normalized (existing `ParentDir`/`CurDir` logic).
2. **Namespace check + remap rewrite.** The absolute sandbox path must match a
   binding's `to` prefix; the suffix is appended to that binding's host `from`.
   A path with no `to` match is **rejected** — it is not in the sandbox
   namespace. This is what makes the non-shell tools and the shell agree: a host
   path (`/home/me/projects/f.txt`) is not something the shell can open either,
   so the tool refuses it rather than passing it through a no-op remap and
   checking the binding's `from`. A request for `/tmp/x` becomes the host `from`
   that bwrap mounts at `/tmp`, so the non-shell tool and the shell address the
   same file.
3. **Mode check** on the covering binding:
   - Read operations (`read_file`, `glob_files`, `grep_text`, `list_dir`) are
     allowed under any `ro` or `rw` binding that covers the path.
   - Write operations (`write_file`, `edit_file`, `make_dir`, `remove`,
     `move_file`, `copy_file`, and the *destination* of move/copy) require the
     covering binding to be `rw`. A write under an `ro` binding →
     `ToolError::Generic("Access denied: path is read-only")`.

On any denial the error echoes the path the **caller supplied** (an in-sandbox
path it can act on); the host form never appears in a model-visible string. See
`doc/path_leak_risks.md`.

> **`~` is not a leak.** `HOME` is in `DEFAULT_ENV_ALLOW` and is forwarded to the
> sandbox verbatim, so the model already sees it (`echo $HOME`) and can already
> write `$HOME/...`. Expanding `~` against that same value gives it nothing new:
> the namespace + mode checks still gate access. The expansion source is the
> *effective sandbox* `HOME` (`sandbox_home`), so tools and shell cannot disagree
> about what `~` means even under a custom `sandbox.env`.

### How `paths` maps to the policy

The `paths` list is pre-resolved at config load into a `Vec<ResolvedBind>`
(`mode`, absolute `from`, absolute `to`). From that list we also build a single
**`RemapTable`** — an ordered `to → from` map (longest `to` prefix wins) used
for path translation. Both the shell wrapper and `validate_path` consult the
same `ResolvedBind` + `RemapTable`, so the two worlds cannot disagree.

### Decision: non-shell tools stay host-direct (NOT run in-namespace)

We deliberately do **not** execute the non-shell tools as child processes inside
a bwrap namespace to make them "see" the same filesystem as the shell.
Rationale:

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

`validate_path` performs, in order:

1. **Expand to an absolute sandbox path** (`~` against the sandbox `HOME`,
   relative against the sandbox CWD, normalize). This is the `to`-side form.
2. **Namespace check + rewrite.** Match a binding's `to` prefix; unbound →
   deny (not in the sandbox namespace). On a match, replace the `to` prefix with
   the binding's host `from`. A non-shell write to `/tmp/x` is thereby
   redirected to the host `from` that bwrap mounts at `/tmp` (e.g.
   `./scratch/x`); the shell command writing `/tmp/x` lands on the *same* host
   file. **This is what makes `/tmp` (and any remap) mean the same thing to both
   tool kinds** — and why no `to` match must be a denial: a host path the model
   happens to guess is not a namespace path, so the shell could not open it
   either.
3. **Mode check** (read vs write) on the covering binding.

For non-remapped bindings (`from == to`) the rewrite is a no-op and the host
path matches `from` directly. For remapped bindings, the rewrite maps the
agent-visible `to` back to the host `from`, then the normal `from`/`mode` policy
applies.

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
  for non-shell tools, matching the sandboxed shell path.
- **`~` expands to the sandbox `HOME`** (`tilde_and_relative_args_resolve_in_the_sandbox_namespace`),
  and a relative argument joins the sandbox cwd.
- **A host path is not addressable** (`a_host_path_is_not_addressable_in_the_sandbox`,
  `nonshell_host_path_alias_rejected`): a path covered by a binding's `from`
  but not its `to` is rejected — the alias hole is closed.
- **`/tmp` scratch stays writable under an `ro`-only config**
  (`nonshell_tmp_scratch_is_writable_under_ro_policy`).

---

## Implementation plan (phased)

Each phase is independently reviewable and keeps the tree buildable/testable.

### Phase 0 — Config schema & resolution (no behavior change)

- [ ] Add `SandboxConfig` (with `ro`/`rw: Vec<BindPath>`), `BindPath`,
      `BindMode`, `ToolSettings.sandbox` to `config.rs`.
- [ ] Define `BindPath` as an untagged enum (`Simple(String)` /
      `Remap { from, to }`); mode is implied by which list (`ro`/`rw`) it is in.
- [ ] Add `ResolvedBind { mode, from: PathBuf, to: PathBuf }` and a
      `resolve_paths(cwd, home, &[BindPath], &[BindPath]) -> Vec<ResolvedBind>`
      helper that expands `~`, resolves relative paths, and validates `from`
      via `validate_path`-style traversal.
- [ ] Add `RemapTable` (ordered `to -> from` map, longest-`to` prefix wins)
      built from the `ResolvedBind` list; this is the single source of truth for
      path translation shared by the shell wrapper and `validate_path`.
- [ ] Add `Config::bash_sandbox() -> SandboxConfig` accessor (merges defaults).
- [ ] Unit tests: parse `BindPath` bare-path and remap forms from the `ro`/`rw`
      lists; resolve relative/`~`/absolute; assert mode follows the list.
- [ ] Update `settings.toml.example` with the full `sandbox:` block (commented).

### Phase 1 — Timeout + `run_sandboxed` refactor (shell tools)

- [ ] Add `run_sandboxed(cmd, &SandboxConfig) -> Result<String, ToolError>` in
      `tools.rs`.
- [ ] Route `Bash::call` and `agent_browser` through `run_sandboxed`.
- [ ] Apply `duct` `.timeout(Duration::from_secs(timeout_secs))` inside the
      helper (skip when `timeout_secs == 0/None`).
- [ ] The sandbox is always active when a config is present (bwrap required at
      startup); the helper refuses only when no sandbox config is set.
- [ ] Unit/integration tests: `echo` works; `sleep 30` with 1s timeout is
      killed/errors; an unset sandbox config is refused.
- [ ] Verify existing `Bash`/`agent_browser` behavior unchanged when sandboxed.

### Phase 2 — bwrap sandboxing (shell tools)

- [ ] Implement `build_bwrap_expr(cmd, &SandboxConfig, &[ResolvedBind])`.
- [ ] Deny-by-default root: when `paths` non-empty, bind only declared entries;
      when empty, fall back to `--ro-bind <cwd> <cwd>` + writable `/tmp`.
- [x] Emit flags: per-path `--ro-bind`/`--bind`; `--dev /dev --proc /proc`;
      `--ro-bind` per `ESSENTIAL_ETC` entry; `--tmpfs /tmp --size`;
      `--unshare-net` (transitional: still conditional until the R3a bridge
      exists, because unsharing without a bridge would remove all egress);
      `--unshare-pid --pid`
      (if `pid_isolation`); `--unshare-ipc --unshare-uts --hostname sandbox`;
      `--cap-drop ALL` (if `drop_caps`); `--die-with-parent`.
- [x] Memory wrapper: `prlimit --as=<MB*1024*1024> -- <argv>` (direct argv, no
      shell); `ulimit -v` shell-builtin fallback when `prlimit` absent.
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
- [ ] Reject paths outside all bindings when `paths` non-empty
      (deny-by-default); keep CWD-visible read-only fallback when empty.
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
- [ ] Final integration pass: run the example `settings.toml` recipes under
      sandbox on/off.
- [ ] CHANGELOG entry; confirm `cargo test` + `cargo build` green on a host
      with and without bwrap.
