use duct_sh::sh_dangerous;
use rig_derive::rig_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use rig_core::tool::ToolContext;

use crate::config::{resolve_path_policy, resolve_paths, env_vars_to_keep, SandboxConfig};

#[derive(Debug, thiserror::Error, Serialize, Deserialize, JsonSchema)]
pub enum ToolError {
    #[error("IO error: {0}")]
    Io(String),
    #[error("Error: {0}")]
    Generic(String),
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
impl From<tokio::task::JoinError> for ToolError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Generic(e.to_string())
    }
}

fn get_path(path: &str) -> Result<PathBuf, ToolError> {
    get_path_with(path, false)
}

/// Resolve `path` for a filesystem tool, enforcing the sandbox path policy when
/// active. `is_write` marks mutating operations (write/edit/make_dir/remove/
/// move-dest/copy-dest), which require an `rw` binding under the policy.
fn get_path_with(path: &str, is_write: bool) -> Result<PathBuf, ToolError> {
    let base = std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?;

    // When a path policy is active (sandbox enabled with explicit bindings),
    // rewrite via the remap table, then enforce coverage + mode.
    if let Some(policy) = current_path_policy() {
        let host = policy.resolve(path);
        return policy
            .check(&host, is_write)
            .map(|_| host)
            .map_err(ToolError::Generic);
    }

    validate_path(&base, path)
}

fn validate_path(base: &std::path::Path, path: &str) -> Result<PathBuf, ToolError> {
    let p = std::path::Path::new(path);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };

    let mut result = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            c => result.push(c),
        }
    }

    if result.starts_with(base) {
        Ok(result)
    } else {
        Err(ToolError::Generic(
            "Access denied: path must be within the current directory".into(),
        ))
    }
}

fn walk_files(base: &std::path::Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .build()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
}

#[rig_tool(
    description = "Read file with line numbers",
    required(path, offset, limit)
)]
pub async fn read_file(path: String, offset: u64, limit: u64) -> Result<String, ToolError> {
    let content = fs::read_to_string(get_path(&path)?).await?;
    let lines: Vec<_> = content
        .lines()
        .enumerate()
        .skip(offset as usize)
        .take(if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        })
        .map(|(i, l)| format!("{:4}| {}\n", i + 1, l))
        .collect();
    Ok(lines.concat())
}

#[rig_tool(description = "Write content to file", required(path, content))]
pub async fn write_file(path: String, content: String) -> Result<String, ToolError> {
    fs::write(get_path_with(&path, true)?, content).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Replace old with new in file (old must be unique unless all=true)",
    required(path, old, new, all)
)]
pub async fn edit_file(
    path: String,
    old: String,
    new: String,
    all: bool,
) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    let text = fs::read_to_string(&p).await?;
    if !text.contains(&old) {
        return Ok("error: old_string not found".into());
    }
    let count = text.matches(&old).count();
    if !all && count > 1 {
        return Ok(format!(
            "error: old_string appears {count} times, must be unique (use all=true)"
        ));
    }
    fs::write(
        p,
        if all {
            text.replace(&old, &new)
        } else {
            text.replacen(&old, &new, 1)
        },
    )
    .await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Find files by pattern, sorted by mtime",
    required(pat, path)
)]
pub async fn glob_files(pat: String, path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;
    let matcher = globset::Glob::new(&pat)
        .map_err(|e| ToolError::Generic(e.to_string()))?
        .compile_matcher();
    let entries = tokio::task::spawn_blocking(move || {
        walk_files(&base)
            .filter(|e| matcher.is_match(e.path().strip_prefix(&base).unwrap_or(e.path())))
            .map(|e| e.into_path())
            .collect::<Vec<_>>()
    })
    .await?;

    let mut files = Vec::new();
    for e in entries {
        let mtime = fs::metadata(&e).await.and_then(|m| m.modified()).ok();
        files.push((e, mtime));
    }
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    let res = files
        .iter()
        .map(|(f, _)| f.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if res.is_empty() { "none".into() } else { res })
}

#[rig_tool(description = "Search files for a regex pattern (ripgrep)", required(pat, path))]
pub async fn grep_text(pat: String, path: String) -> Result<String, ToolError> {
    // Validate the search root against the active path policy (or cwd bounds);
    // ripgrep itself runs confined inside a read-only sandbox.
    let _ = get_path(&path)?;

    let cfg = sandbox_config();
    let cfg = cfg.as_ref().as_ref().cloned().unwrap_or_default();
    let timeout = cfg.timeout_secs.or(Some(60));

    // Constrained ripgrep, passed as a discrete argv list (no shell, no string
    // construction). The pattern and path are argv elements, so regex
    // metacharacters and shell metacharacters are never interpreted by a shell.
    let argv: Vec<String> = vec![
        "rg".into(),
        "--line-number".into(),
        "--with-filename".into(),
        "--no-column".into(),
        "--color".into(),
        "never".into(),
        "--hidden".into(),
        "--max-depth".into(),
        "25".into(),
        "--max-filesize".into(),
        "5M".into(),
        "--max-count".into(),
        "500".into(),
        "-e".into(),
        pat,
        path,
    ];
    let raw = run_sandboxed_ro(&argv, &cfg, timeout).await?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Ok("none".into())
    } else {
        Ok(cap_lines(trimmed, 50))
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct BashArgs {
    /// The shell command to execute.
    pub cmd: String,
    /// Optional timeout in seconds for this command. Overrides the configured
    /// default (`tool_config.bash.sandbox.timeout_secs`). Must be a positive
    /// finite number; values above the maximum are rejected. Leave unset to use
    /// the configured default.
    #[serde(default)]
    pub timeout: Option<u64>,
}

pub struct Bash;

impl rig_core::tool::Tool for Bash {
    type Args = BashArgs;
    type Output = String;
    type Error = ToolError;

    const NAME: &'static str = "bash";

    fn description(&self) -> String {
        "Run shell command".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BashArgs)).unwrap()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> std::result::Result<Self::Output, Self::Error> {
        run_sandboxed(&args.cmd, args.timeout).await
    }
}

/// Process-wide resolved sandbox config. picocode runs a single agent per
/// process and publishes it once at agent creation via [`set_sandbox`]; a
/// `Mutex` (rather than `OnceLock`) keeps the helper test-friendly while still
/// being written exactly once in normal operation.
static SANDBOX: std::sync::LazyLock<std::sync::Mutex<Arc<Option<SandboxConfig>>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Arc::new(None)));

pub(crate) fn set_sandbox(cfg: Option<SandboxConfig>) {
    *SANDBOX.lock().expect("sandbox lock poisoned") = Arc::new(cfg);
}

fn sandbox_config() -> Arc<Option<SandboxConfig>> {
    SANDBOX.lock().expect("sandbox lock poisoned").clone()
}

/// Resolve the active path policy for the non-shell file tools. Returns `None`
/// when the sandbox is disabled, bwrap is absent, or no explicit bindings are
/// configured (permissive default). Mirrors the shell-sandboxing activation
/// condition so the two worlds stay consistent.
fn current_path_policy() -> Option<crate::config::PathPolicy> {
    let cfg = sandbox_config();
    let c = cfg.as_ref().as_ref()?;
    if !c.enabled.unwrap_or(false) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_path_policy(c, &cwd, home.as_deref())
}

/// Maximum per-command timeout (seconds) an LLM may request via the bash
/// tool's `timeout` argument. Mirrors pi's ceiling of `2_147_483_647` ms
/// (i32::MAX), expressed in whole seconds.
const MAX_TIMEOUT_SECS: u64 = 2_147_483;

/// Resolve the effective wall-clock timeout for a command.
///
/// `override_timeout` is the LLM-supplied per-call value (seconds); when set it
/// overrides the `configured` default. The returned value is `None` when there
/// is no timeout (neither override nor default, or an explicitly-zero default).
/// Invalid overrides (non-positive, or above [`MAX_TIMEOUT_SECS`]) are rejected
/// with an error rather than silently clamped.
fn resolve_timeout(
    override_timeout: Option<u64>,
    configured: Option<u64>,
) -> Result<Option<u64>, ToolError> {
    let requested = match override_timeout {
        Some(t) => {
            if t == 0 {
                return Err(ToolError::Generic(
                    "invalid timeout: must be a positive number of seconds".into(),
                ));
            }
            if t > MAX_TIMEOUT_SECS {
                return Err(ToolError::Generic(format!(
                    "invalid timeout: maximum is {MAX_TIMEOUT_SECS} seconds"
                )));
            }
            Some(t)
        }
        None => configured,
    };
    Ok(requested.filter(|s| *s > 0))
}

/// Run a shell command through the sandbox helper. Applies a wall-clock
/// timeout (so a hung command reaps and releases the blocking thread) and,
/// when enabled, wraps the command in a Bubblewrap namespace. `bwrap` is
/// required at startup (see `check_runtime_deps`), so its absence is handled
/// before any command runs; when disabled, the command runs unsandboxed.
///
/// `override_timeout` is the LLM-supplied per-call timeout (seconds) from the
/// bash tool's `timeout` argument; when `None`, the configured sandbox default
/// (`tool_config.bash.sandbox.timeout_secs`) applies. A `None`/0 result means
/// no timeout is enforced.
pub(crate) async fn run_sandboxed(
    cmd: &str,
    override_timeout: Option<u64>,
) -> Result<String, ToolError> {
    let cfg = sandbox_config();

    // The LLM may override the configured default timeout per command. The
    // timeout is independent of whether sandboxing is active, so a disabled
    // (or bwrap-absent) config still enforces it.
    let configured = cfg.as_ref().as_ref().and_then(|c| c.timeout_secs);
    let timeout = resolve_timeout(override_timeout, configured)?;

    // Move owned copies into the blocking task (`cmd` cloned to a String, the
    // `Arc` config cheaply cloned) so the closure is `'static`. Everything runs
    // in one task so the non-`Clone` `Handle` stays owned where it can be killed.
    let cmd = cmd.to_string();
    let cfg = cfg.clone();
    let output = tokio::task::spawn_blocking(move || {
        // Decide sandboxing here: `bwrap` is required at startup, so its
        // absence is handled before any command runs; we only gate on the
        // configured `enabled` flag.
        let effective = match cfg.as_ref().as_ref() {
            Some(c) if c.enabled.unwrap_or(false) => Some(c),
            _ => None,
        };
        match effective {
            Some(c) => {
                // Sandboxed: redirect output to a host temp file bound into the
                // namespace (bwrap bypasses duct's stdout pipe).
                let out_path = make_out_path()?;
                let expr = build_bwrap_expr(&cmd, c, &out_path, false)?;
                let handle = expr.unchecked().start()?;
                let result = match timeout {
                    None => handle
                        .wait()
                        .map(|_| ())
                        .map_err(|e| ToolError::Io(e.to_string())),
                    Some(secs) => match handle.wait_timeout(std::time::Duration::from_secs(secs)) {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => {
                            let _ = handle.kill();
                            Err(ToolError::Generic(format!(
                                "command timed out after {secs}s"
                            )))
                        }
                        Err(e) => Err(ToolError::Io(e.to_string())),
                    },
                };
                // Read whatever the command wrote, regardless of exit/timeout,
                // then clean up the temp file.
                let captured = std::fs::read(&out_path).unwrap_or_default();
                let _ = std::fs::remove_file(&out_path);
                result.map(|_| captured)
            }
            None => {
                // Unsandboxed: capture combined output directly via duct.
                let expr = sh_dangerous(&cmd).stderr_to_stdout().stdout_capture();
                let handle = expr.unchecked().start()?;
                match timeout {
                    None => handle
                        .wait()
                        .map(|o| o.stdout.clone())
                        .map_err(|e| ToolError::Io(e.to_string())),
                    Some(secs) => match handle.wait_timeout(std::time::Duration::from_secs(secs)) {
                        Ok(Some(o)) => Ok(o.stdout.clone()),
                        Ok(None) => {
                            let _ = handle.kill();
                            Err(ToolError::Generic(format!(
                                "command timed out after {secs}s"
                            )))
                        }
                        Err(e) => Err(ToolError::Io(e.to_string())),
                    },
                }
            }
        }
    })
    .await
    .map_err(|e| ToolError::Generic(e.to_string()))??;

    Ok(trim_output(output))
}

/// Run a program (given as an argv list) inside a **read-only** Bubblewrap
/// namespace. Unlike [`run_sandboxed`] it never goes through a shell, so each
/// argument is passed verbatim — no quoting or escaping, and no shell
/// metacharacters are ever interpreted. Used by read-only tools such as grep.
/// Every user binding is mounted read-only regardless of its configured mode,
/// and `bwrap` is required at startup (see `check_runtime_deps`).
pub(crate) async fn run_sandboxed_ro(
    argv: &[String],
    cfg: &SandboxConfig,
    timeout: Option<u64>,
) -> Result<String, ToolError> {
    let argv = argv.to_vec();
    let cfg = cfg.clone();
    let output = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ToolError> {
        let expr = build_bwrap_expr_argv(&argv, &cfg, true)?;
        let handle = expr
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .start()
            .map_err(|e| ToolError::Io(e.to_string()))?;
        // Capture stdout + stderr directly (bwrap forwards the child's stdio to
        // the inherited fds). Combine them the way the shell path's `2>&1` does.
        let captured = match timeout {
            None => handle
                .wait()
                .map_err(|e| ToolError::Io(e.to_string())),
            Some(secs) => match handle.wait_timeout(std::time::Duration::from_secs(secs)) {
                Ok(Some(o)) => Ok(o),
                Ok(None) => {
                    let _ = handle.kill();
                    Err(ToolError::Generic(format!(
                        "command timed out after {secs}s"
                    )))
                }
                Err(e) => Err(ToolError::Io(e.to_string())),
            },
        }?;
        let mut v = captured.stdout.clone();
        if !captured.stderr.is_empty() {
            v.push(b'\n');
            v.extend_from_slice(&captured.stderr);
        }
        Ok(v)
    })
    .await
    .map_err(|e| ToolError::Generic(e.to_string()))??;
    Ok(String::from_utf8_lossy(&output).to_string())
}

/// Create a unique host temp file for capturing sandboxed command output.
fn make_out_path() -> Result<std::path::PathBuf, ToolError> {
    let name = format!(
        "picocode-out-{}-{}.txt",
        std::process::id(),
        // Nanosecond-ish uniqueness without extra deps.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let path = std::env::temp_dir().join(name);
    // Ensure the file exists so bwrap can bind it.
    std::fs::write(&path, b"").map_err(|e| ToolError::Io(e.to_string()))?;
    Ok(path)
}

/// Trim combined output and normalize empty results.
fn trim_output(out: Vec<u8>) -> String {
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        "(empty)".into()
    } else {
        s
    }
}

/// Cap output to the first `n` lines (token economy for large greps).
fn cap_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Read-only system directories bound into every sandbox so the shell and
/// common tools can execute. They are read-only, so the host OS stays hidden
/// from writes; only explicitly declared bindings add writable/project paths.
const ESSENTIAL_RO_DIRS: &[&str] = &["/usr", "/bin", "/lib", "/lib64", "/etc"];

/// Single-quote a string for safe embedding as a single argument inside a
/// `sh -c` payload. The only shell metacharacter not inert inside single
/// quotes is the single quote itself; we escape it as `''\''` (close, escaped
/// quote, reopen). This lets the outer `sh -c` pass the user command through
/// verbatim instead of expanding `$VAR` / `$(...)` before the inner shell runs
/// it.
fn sh_single_quote(s: &str) -> String {
    let mut out = String::from('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push('\'');
            out.push('\\');
            out.push('\'');
            out.push('\'');
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the common bwrap argument vector shared by both execution paths:
/// the read-only system dirs, the CWD/bindings, the scratch tmpfs, network/PID/
/// capability isolation, and the sanitized environment. Callers append either
/// the `sh -c` payload (shell path) or the program argv (direct path) after
/// `--`.
fn build_bwrap_args(
    cfg: &SandboxConfig,
    read_only: bool,
) -> Result<Vec<String>, ToolError> {
    let cwd = std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?;
    let home = dirs_home();

    // Resolve the configured bindings (empty list => CWD-visible default).
    let mut binds = resolve_paths(&cwd, home.as_deref(), &cfg.paths)
        .map_err(|e| ToolError::Generic(format!("sandbox config error: {e}")))?;
    // Emit bindings most-specific-first: sort by the in-sandbox `to` path depth
    // so the deepest (most specific) binding is mounted LAST. bwrap resolves
    // overlapping mounts by "last mount wins", so this makes the deeper
    // binding authoritative on overlap. This matches the non-shell
    // `PathPolicy::check` (which resolves overlaps by `to` specificity via the
    // remap table), keeping the two worlds order-independent: a nested `ro`
    // child is read-only, and a nested `rw` child upgrades its subtree, no
    // matter what order the operator lists the bindings in.
    binds.sort_by_key(|b| b.to.components().count());

    let mut args: Vec<String> = Vec::new();

    // Essential read-only system directories so the shell and common tools
    // (sh, ls, compilers, ...) can execute. These are read-only, so the host
    // OS is hidden from writes and from anything not explicitly bound.
    for dir in ESSENTIAL_RO_DIRS {
        if std::path::Path::new(dir).is_dir() {
            args.push("--ro-bind".into());
            args.push(dir.to_string());
            args.push(dir.to_string());
        }
    }

    if binds.is_empty() {
        // Default: read-only CWD + a writable tmpfs scratch area.
        args.push("--ro-bind".into());
        args.push(cwd.to_string_lossy().to_string());
        args.push(cwd.to_string_lossy().to_string());
    } else {
        // Deny-by-default: only declared bindings are visible (on top of the
        // essential read-only system dirs above). User bindings come BEFORE the
        // scratch tmpfs so a user binding at /tmp wins over the tmpfs.
        for b in &binds {
            args.push(
                if !read_only && b.mode.is_writable() {
                    "--bind"
                } else {
                    "--ro-bind"
                }
                .into(),
            );
            args.push(b.from.to_string_lossy().to_string());
            args.push(b.to.to_string_lossy().to_string());
        }
    }

    // Scratch tmpfs at /tmp, but only when no user binding already provides
    // /tmp (a later tmpfs would otherwise shadow a user's /tmp bind).
    let tmp_covered = binds.iter().any(|b| {
        let t = b.to.as_os_str();
        t == "/tmp" || t.to_string_lossy().starts_with("/tmp/")
    });
    if !tmp_covered {
        let tmp_size = cfg.tmp_size_mb.unwrap_or(256);
        let tmp_bytes = tmp_size as u64 * 1024 * 1024;
        args.push("--size".into());
        args.push(tmp_bytes.to_string());
        args.push("--tmpfs".into());
        args.push("/tmp".into());
    }

    if !cfg.network.unwrap_or(false) {
        args.push("--unshare-net".into());
    }
    if cfg.pid_isolation.unwrap_or(true) {
        args.push("--unshare-pid".into());
    }
    if cfg.drop_caps.unwrap_or(true) {
        args.push("--cap-drop".into());
        args.push("ALL".into());
    }
    args.push("--die-with-parent".into());

    // Environment isolation (see doc/sandbox.md, "Environment isolation").
    // bwrap inherits the parent environment verbatim, which would otherwise
    // leak host secrets (API keys, tokens, SSH agent socket, ...) into the
    // LLM-driven command. We start from a clean env (`--clearenv` retains
    // only PWD) and explicitly re-expose exactly the variables the policy
    // permits. The policy is deny-by-default (secrets) unless the user set an
    // explicit `env` block. Always emit `--clearenv` (even when nothing is
    // kept) so the policy is enforced regardless of how many vars survive.
    let policy = cfg.env_policy();
    let keep = env_vars_to_keep(&policy, &std::env::vars().collect::<Vec<_>>());
    args.push("--clearenv".into());
    for (name, value) in &keep {
        args.push("--setenv".into());
        args.push(name.clone());
        args.push(value.clone());
    }

    Ok(args)
}

/// Build a `duct` expression that runs `cmd` inside a bwrap namespace, with its
/// combined output redirected into the bound host temp file `out_path` (the
/// shell path). Used by the bash tool.
///
/// NOTE: the shell payload explicitly redirects its output to `/tmp/.picocode-out`
/// (which is bound from `out_path`) rather than relying on the inherited fd, so
/// the parent reads it back from `out_path` after the process exits.
fn build_bwrap_expr(
    cmd: &str,
    cfg: &SandboxConfig,
    out_path: &std::path::Path,
    read_only: bool,
) -> Result<duct::Expression, ToolError> {
    let mut args = build_bwrap_args(cfg, read_only)?;

    // Bind the host temp output file into the namespace (read-write) AFTER user
    // binds, so it is never shadowed by a user /tmp bind.
    args.push("--bind".into());
    args.push(out_path.to_string_lossy().to_string());
    args.push("/tmp/.picocode-out".into());

    // Build the in-namespace payload. Memory limit via RLIMIT_AS (ulimit -v),
    // scoped to the command's process tree by the PID namespace. Combined
    // output is redirected to the bound host temp file.
    let cmd_quoted = sh_single_quote(&cmd);
    let inner = if let Some(mb) = cfg.memory_mb {
        let kb = mb * 1024;
        format!("ulimit -v {kb}; exec sh -c {cmd_quoted}")
    } else {
        format!("exec sh -c {cmd_quoted}")
    };
    let payload = format!("{inner} > /tmp/.picocode-out 2>&1");
    args.push("--".into());
    args.push("sh".into());
    args.push("-c".into());
    args.push(payload);

    Ok(duct::cmd("bwrap", &args))
}

/// Build a `duct` expression that runs a program with an explicit argv list
/// inside a bwrap namespace, with NO shell in between. Each argument is passed
/// verbatim to the program, so regex patterns and paths need no quoting or
/// escaping and shell metacharacters are never interpreted. Output is captured
/// directly by duct (bwrap forwards the child's stdio to its inherited fds).
fn build_bwrap_expr_argv(
    argv: &[String],
    cfg: &SandboxConfig,
    read_only: bool,
) -> Result<duct::Expression, ToolError> {
    let mut args = build_bwrap_args(cfg, read_only)?;
    args.push("--".into());
    for a in argv {
        args.push(a.clone());
    }
    Ok(duct::cmd("bwrap", &args))
}

/// Best-effort home directory lookup for `~` expansion in bindings.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[rig_tool(description = "List files and directories in a path", required(path))]
pub async fn list_dir(path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;

    let entries = tokio::task::spawn_blocking(move || {
        ignore::WalkBuilder::new(&base)
            .hidden(false)
            .require_git(false)
            .max_depth(Some(1))
            .build()
            .filter_map(|r| r.ok())
            .filter(|e| e.depth() > 0) // Skip the root directory itself
            .map(|e| {
                let name = e.file_name().to_string_lossy();
                let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                format!("{}{}", name, if is_dir { "/" } else { "" })
            })
            .collect::<Vec<String>>()
    })
    .await?;

    let mut res = entries;
    res.sort();
    Ok(if res.is_empty() {
        "(empty)".into()
    } else {
        res.join("\n")
    })
}

#[rig_tool(
    description = "Create a directory (including parent directories)",
    required(path)
)]
pub async fn make_dir(path: String) -> Result<String, ToolError> {
    fs::create_dir_all(get_path_with(&path, true)?).await?;
    Ok("ok".into())
}

#[rig_tool(description = "Remove a file or directory", required(path, recursive))]
pub async fn remove(path: String, recursive: bool) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    if p.is_dir() {
        if recursive {
            fs::remove_dir_all(p).await?;
        } else {
            fs::remove_dir(p).await?;
        }
    } else {
        fs::remove_file(p).await?;
    }
    Ok("ok".into())
}

#[rig_tool(description = "Move or rename a file or directory", required(src, dst))]
pub async fn move_file(src: String, dst: String) -> Result<String, ToolError> {
    fs::rename(get_path_with(&src, true)?, get_path_with(&dst, true)?).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Copy a file (does not support directories yet)",
    required(src, dst)
)]
pub async fn copy_file(src: String, dst: String) -> Result<String, ToolError> {
    fs::copy(get_path_with(&src, true)?, get_path_with(&dst, true)?).await?;
    Ok("ok".into())
}

#[rig_tool(
    description = "Browser automation CLI for AI agents.
Core workflow:
1. Navigate: agent-browser open <url>
2. Snapshot: agent-browser snapshot -i (returns elements with refs like @e1, @e2)
3. Interact: click @e1, fill @e2 \"text\", etc.
4. Re-snapshot after navigation or significant DOM changes

Commands:
- Navigation: open <url>, back, forward, reload, close
- Snapshot: snapshot (full tree), snapshot -i (interactive only), snapshot -c (compact)
- Interactions: click, dblclick, fill, type, press <key>, hover, check, uncheck, select, scroll, scrollintoview
- Information: get text, get value, get title, get url
- Screenshots: screenshot [path] [--full]
- Wait: wait @e1, wait <ms>, wait --text <text>, wait --load networkidle
- Sessions: --session <name> (parallel browsers)
- Output: Add --json for machine-readable output",
    required(args)
)]
pub async fn agent_browser(args: String) -> Result<String, ToolError> {
    run_sandboxed(&format!("agent-browser {}", args), None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_validate_path_normal() {
        let base = Path::new("/work");
        assert_eq!(
            validate_path(base, "file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert_eq!(
            validate_path(base, "subdir/file.txt").unwrap(),
            Path::new("/work/subdir/file.txt")
        );
    }

    #[test]
    fn test_validate_path_current_dir() {
        let base = Path::new("/work");
        assert_eq!(validate_path(base, ".").unwrap(), Path::new("/work"));
        assert_eq!(
            validate_path(base, "./file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
    }

    #[test]
    fn test_validate_path_escape_parent() {
        let base = Path::new("/work");
        assert!(validate_path(base, "..").is_err());
        assert!(validate_path(base, "../../etc/passwd").is_err());
    }

    #[test]
    fn test_validate_path_stay_in_bounds() {
        let base = Path::new("/work");
        assert_eq!(
            validate_path(base, "subdir/../file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert_eq!(
            validate_path(base, "subdir/./file.txt").unwrap(),
            Path::new("/work/subdir/file.txt")
        );
    }

    #[test]
    fn test_validate_path_absolute() {
        let base = Path::new("/work");
        // Absolute paths should be allowed if they are inside base
        assert_eq!(
            validate_path(base, "/work/file.txt").unwrap(),
            Path::new("/work/file.txt")
        );
        assert!(validate_path(base, "/etc/passwd").is_err());
    }

    #[test]
    fn test_validate_path_empty() {
        let base = Path::new("/work");
        assert_eq!(validate_path(base, "").unwrap(), Path::new("/work"));
    }

    #[test]
    fn test_validate_path_unforgiving_edge_cases() {
        let base = Path::new("/work");

        // Trying to be clever with many dots
        assert!(validate_path(base, "subdir/../../outside").is_err());

        // Symlink-like behavior (though validate_path doesn't resolve actual symlinks, just components)
        // If we have a path that looks like it's escaping but it's not
        assert_eq!(
            validate_path(base, "a/b/../../c").unwrap(),
            Path::new("/work/c")
        );

        // Path that starts with many slashes
        assert!(validate_path(base, "///etc/passwd").is_err());

        // Path that is just dots - "..." is a valid filename but ".." is not allowed to escape
        assert_eq!(validate_path(base, "...").unwrap(), Path::new("/work/..."));

        // Root path
        assert!(validate_path(base, "/").is_err());
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use crate::config::{BindMode, BindSpec, EnvMode, EnvPolicy, SandboxConfig};
    use serial_test::serial;

    fn bwrap_on_path() -> bool {
        std::process::Command::new("which")
            .arg("bwrap")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn sandbox_with(paths: Vec<BindSpec>, timeout_secs: Option<u64>) -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            timeout_secs,
            network: Some(false),
            memory_mb: None,
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: Some(16),
            paths,
            env: None,
        }
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_echo_works() {
        if !bwrap_on_path() {
            return;
        }
        // Default (empty paths): read-only CWD + writable tmpfs.
        set_sandbox(Some(sandbox_with(vec![], Some(30))));
        let out = run_sandboxed("echo hello-world", None).await.unwrap();
        assert_eq!(out, "hello-world");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_tmpfs_is_writable() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], Some(30))));
        let out = run_sandboxed("echo x > /tmp/probe && cat /tmp/probe", None).await.unwrap();
        assert_eq!(out, "x");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_timeout_kills_hung_command() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], Some(1))));
        let res = run_sandboxed("sleep 30", None).await;
        assert!(res.is_err(), "expected timeout error, got {:?}", res);
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_rw_bind_sees_host_file() {
        if !bwrap_on_path() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Bind the temp dir read-write at /work inside the namespace.
        let spec = BindSpec {
            mode: BindMode::Rw,
            from: dir.path().to_string_lossy().to_string(),
            to: "/work".to_string(),
        };
        set_sandbox(Some(sandbox_with(vec![spec], Some(30))));
        let out = run_sandboxed("echo inside > /work/probe.txt && cat /work/probe.txt", None)
            .await
            .unwrap();
        assert_eq!(out, "inside");
        // The write landed on the host temp dir.
        assert!(dir.path().join("probe.txt").exists());
    }

    #[tokio::test]
    #[serial]
    async fn unsandboxed_timeout_still_enforced() {
        // No bwrap needed: disabled sandbox still applies the timeout.
        set_sandbox(Some(SandboxConfig {
            enabled: Some(false),
            timeout_secs: Some(1),
            network: None,
            memory_mb: None,
            pid_isolation: None,
            drop_caps: None,
            tmp_size_mb: None,
            paths: vec![],
            env: None,
        }));
        let res = run_sandboxed("sleep 30", None).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    // ---- Phase 1b: LLM-configurable per-call timeout override ----

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_short_kills_hung_command() {
        // A short per-call override short-circuits the long configured default
        // and kills a hung command.
        set_sandbox(Some(sandbox_with(vec![], Some(300))));
        let res = run_sandboxed("sleep 30", Some(1)).await;
        assert!(res.is_err(), "expected timeout error, got {:?}", res);
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_positive_works() {
        // A generous override lets a quick command succeed even when the
        // configured default is too short.
        set_sandbox(Some(sandbox_with(vec![], Some(1))));
        let out = run_sandboxed("echo ok", Some(30)).await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_zero_rejected() {
        set_sandbox(Some(sandbox_with(vec![], Some(300))));
        let res = run_sandboxed("echo hi", Some(0)).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid timeout"));
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_above_max_rejected() {
        set_sandbox(Some(sandbox_with(vec![], Some(300))));
        let res = run_sandboxed("echo hi", Some(super::MAX_TIMEOUT_SECS + 1)).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid timeout"));
    }

    // ---- Phase 3: non-shell path policy ----

    /// Set a sandbox with explicit bindings so the non-shell path policy is
    /// active (requires bwrap, like the shell sandbox).
    fn sandbox_with_paths(paths: Vec<BindSpec>) {
        set_sandbox(Some(sandbox_with(paths, Some(30))));
    }

    #[tokio::test]
    #[serial]
    async fn nonshell_ro_write_rejected() {
        if !bwrap_on_path() {
            return;
        }
        // CWD bound read-only.
        let cwd = std::env::current_dir().unwrap();
        sandbox_with_paths(vec![BindSpec {
            mode: BindMode::Ro,
            from: cwd.to_string_lossy().to_string(),
            to: cwd.to_string_lossy().to_string(),
        }]);
        let res = super::write_file("should_fail.txt".to_string(), "x".to_string()).await;
        assert!(res.is_err(), "write under ro binding should be denied");
        assert!(res.unwrap_err().to_string().contains("read-only"));
    }

    #[tokio::test]
    #[serial]
    async fn nonshell_unbound_path_rejected() {
        if !bwrap_on_path() {
            return;
        }
        // Only a subdir is bound; writing elsewhere (still under CWD) is denied.
        let dir = tempfile::tempdir().unwrap();
        sandbox_with_paths(vec![BindSpec {
            mode: BindMode::Rw,
            from: dir.path().to_string_lossy().to_string(),
            to: dir.path().to_string_lossy().to_string(),
        }]);
        // The crate root is not bound, so this is rejected.
        let res = super::write_file("outside.txt".to_string(), "x".to_string()).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("not within a permitted binding"));
    }

    #[tokio::test]
    #[serial]
    async fn nonshell_nested_ro_child_write_rejected() {
        if !bwrap_on_path() {
            return;
        }
        // rw parent with a nested ro child. A write under the ro child must be
        // denied even though the parent is writable (most-specific binding
        // wins), while a write under the rw parent must succeed. This is the
        // built-in `write` tool path (host-direct, via PathPolicy::check).
        let dir = tempfile::tempdir().unwrap();
        let ro_sub = dir.path().join("ro");
        std::fs::create_dir_all(&ro_sub).unwrap();
        sandbox_with_paths(vec![
            BindSpec {
                mode: BindMode::Rw,
                from: dir.path().to_string_lossy().to_string(),
                to: "/proj".to_string(),
            },
            BindSpec {
                mode: BindMode::Ro,
                from: ro_sub.to_string_lossy().to_string(),
                to: "/proj/ro".to_string(),
            },
        ]);
        // Write under the rw parent succeeds.
        super::write_file("/proj/writable.txt".to_string(), "x".to_string())
            .await
            .unwrap();
        // Write under the nested ro child is denied.
        let res = super::write_file("/proj/ro/x.txt".to_string(), "x".to_string()).await;
        assert!(res.is_err(), "write under nested ro child should be denied");
        assert!(res.unwrap_err().to_string().contains("read-only"));
    }

    #[tokio::test]
    #[serial]
    async fn nonshell_remap_consistency() {
        if !bwrap_on_path() {
            return;
        }
        // Bind a temp dir at the in-sandbox path /tmp (rw). Both the shell and
        // the non-shell tools must address the same host file.
        let dir = tempfile::tempdir().unwrap();
        sandbox_with_paths(vec![BindSpec {
            mode: BindMode::Rw,
            from: dir.path().to_string_lossy().to_string(),
            to: "/tmp".to_string(),
        }]);

        // Non-shell write to /tmp/x.
        super::write_file("/tmp/x.txt".to_string(), "from-tool".to_string())
            .await
            .unwrap();
        // Shell write to /tmp/x2.
        run_sandboxed("echo from-shell > /tmp/x2.txt", None).await.unwrap();

        // Both landed on the host temp dir.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt")).unwrap().trim(),
            "from-tool"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x2.txt")).unwrap().trim(),
            "from-shell"
        );
        // And the non-shell tool can read what the shell wrote.
        let read = super::read_file("/tmp/x2.txt".to_string(), 0, 0).await.unwrap();
        assert!(read.contains("from-shell"));
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_nested_ro_child_denies_write_reversed_order() {
        if !bwrap_on_path() {
            return;
        }
        // The ro child is listed BEFORE the rw parent in config. Before the
        // depth-sort fix, bwrap's "last mount wins" made the child writable
        // (a shell-side hole). After the fix, most-specific wins regardless of
        // order, so the nested ro child must deny writes while the rw parent
        // stays writable.
        let dir = tempfile::tempdir().unwrap();
        let ro_sub = dir.path().join("ro");
        std::fs::create_dir_all(&ro_sub).unwrap();
        let specs = vec![
            BindSpec {
                mode: BindMode::Ro,
                from: ro_sub.to_string_lossy().to_string(),
                to: "/proj/ro".to_string(),
            },
            BindSpec {
                mode: BindMode::Rw,
                from: dir.path().to_string_lossy().to_string(),
                to: "/proj".to_string(),
            },
        ];
        set_sandbox(Some(sandbox_with(specs, Some(30))));
        let out = run_sandboxed(
            "touch /proj/ro/x; if [ -e /proj/ro/x ]; then echo WRITE_OK; else echo WRITE_DENIED; fi; \
             touch /proj/parent_file.txt; if [ -e /proj/parent_file.txt ]; then echo PARENT_OK; else echo PARENT_DENIED; fi",
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("WRITE_DENIED"), "nested ro child should deny write: {out}");
        assert!(out.contains("PARENT_OK"), "rw parent should allow write: {out}");
    }

    // ---- Environment isolation ----

    /// Set a sandbox with an explicit `env` policy.
    fn sandbox_with_env(env: EnvPolicy) -> SandboxConfig {
        SandboxConfig {
            enabled: Some(true),
            timeout_secs: Some(30),
            network: Some(false),
            memory_mb: None,
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: Some(16),
            paths: vec![],
            env: Some(env),
        }
    }

    #[tokio::test]
    #[serial]
    async fn default_env_drops_secret_but_keeps_path() {
        if !bwrap_on_path() {
            return;
        }
        // No `env` configured => built-in deny-secrets default (drops *_KEY etc).
        set_sandbox(Some(sandbox_with(vec![], Some(30))));
        std::env::set_var("PICOCODE_TEST_KEY", "supersecret");
        // PATH must still be present for `cat` (external) to work, proving the
        // default is a blacklist, not a blanket wipe.
        let out = run_sandboxed(
            "echo \"K=[$PICOCODE_TEST_KEY] PATH_OK=$PATH\"",
            None,
        )
        .await
        .unwrap();
        assert!(
            !out.contains("supersecret"),
            "secret leaked into sandbox: {out}"
        );
        assert!(out.contains("PATH_OK="), "PATH missing: {out}");
        std::env::remove_var("PICOCODE_TEST_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn only_allow_policy_keeps_path_drops_secret() {
        if !bwrap_on_path() {
            return;
        }
        std::env::set_var("PICOCODE_TEST_KEY2", "topsecret");
        set_sandbox(Some(sandbox_with_env(EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec!["PATH".into()],
        })));
        let out = run_sandboxed(
            "echo \"K=[$PICOCODE_TEST_KEY2] PATH_OK=$PATH\"",
            None,
        )
        .await
        .unwrap();
        assert!(
            !out.contains("topsecret"),
            "secret leaked under only_allow: {out}"
        );
        assert!(out.contains("PATH_OK="), "PATH missing: {out}");
        std::env::remove_var("PICOCODE_TEST_KEY2");
    }

    #[tokio::test]
    #[serial]
    async fn only_allow_empty_wipes_env() {
        if !bwrap_on_path() {
            return;
        }
        // An empty only_allow list denies everything (besides PWD, and bwrap's
        // own default PATH which it always injects so commands can run).
        // Verify a host secret we set is NOT visible, and that host HOME (which
        // bwrap does not default) is wiped.
        set_sandbox(Some(sandbox_with_env(EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec![],
        })));
        std::env::set_var("PICOCODE_WIPE_TEST", "wipedme");
        let out = run_sandboxed("echo \"K=[$PICOCODE_WIPE_TEST] HOME=[$HOME]\"", None)
            .await
            .unwrap();
        assert!(
            !out.contains("wipedme"),
            "host secret leaked under empty only_allow: {out}"
        );
        assert!(out.contains("HOME=[]"), "host HOME not wiped: {out}");
        std::env::remove_var("PICOCODE_WIPE_TEST");
    }

    #[tokio::test]
    #[serial]
    async fn unsandboxed_captures_stdout() {
        // When the sandbox is disabled, output must still be captured (not
        // leaked to the parent's stdout / returned empty).
        set_sandbox(None);
        let out = run_sandboxed("echo hi-there", None).await.unwrap();
        assert_eq!(out.trim(), "hi-there");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_captures_whole_command_and_expands_vars() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], Some(30))));
        // A multi-statement command: the trailing `> /tmp/.picocode-out`
        // redirect must scope the *whole* command, not just the last segment.
        let multi = run_sandboxed("echo a; echo b; echo c", None).await.unwrap();
        assert!(
            multi.contains("a") && multi.contains("b") && multi.contains("c"),
            "got: {multi}"
        );
        // Shell-local variables must expand during execution (single shell
        // layer), not be pre-expanded by the outer `sh -c` against the clean env.
        let looped = run_sandboxed("for i in 1 2 3; do echo num-$i; done", None)
            .await
            .unwrap();
        assert!(
            looped.contains("num-1") && looped.contains("num-2") && looped.contains("num-3"),
            "variable expansion broken, got: {looped}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn grep_text_uses_ripgrep_and_sandboxes() {
        if !bwrap_on_path() {
            return;
        }
        // Reset sandbox state so grep uses the default (CWD-visible, read-only)
        // namespace and the search root resolves against cwd bounds.
        set_sandbox(None);
        // No sandbox bindings: ripgrep runs confined over the current working
        // dir (ro-mounted by bwrap). Search a real source file for a known token.
        let out = grep_text("fn main".to_string(), "src/main.rs".to_string())
            .await
            .unwrap();
        assert!(
            out.contains("src/main.rs"),
            "expected path in output, got: {out}"
        );
        assert!(
            out.contains("fn main"),
            "expected match in output, got: {out}"
        );
    }
}
