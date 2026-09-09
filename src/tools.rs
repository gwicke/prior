use rig_core::tool_macro as rig_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use rig_core::tool::ToolContext;

#[allow(unused_imports)]
use crate::config::{resolve_path_policy, resolve_paths, env_vars_to_keep, SandboxConfig, ReadOffset, ReadLimit, BashTimeout, SandboxTimeoutSecs, SandboxMemoryMb, SandboxTmpSizeMb, SandboxMaxOutputBytes};
use crate::edit_match;
use crate::path_util::relativize_path;

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

/// Process-wide registry of tool-cancel senders.  When `<Esc>` is pressed,
/// `spawn_in_flight_reader` sends `()` on every registered sender, which wakes
/// the corresponding watchdog thread to kill the child process.
///
/// Registration is scoped: `capture_bounded` pushes a clone of its `cancel_tx`
/// before spawning the watchdog and pops it (via a drop guard) when the
/// function returns.  This ensures a stale sender never kills the *next* tool.
pub(crate) static TOOL_CANCEL_SENDERS: std::sync::LazyLock<
    std::sync::Mutex<Vec<std::sync::mpsc::Sender<()>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

fn get_path(path: &str) -> Result<PathBuf, ToolError> {
    get_path_with(path, false)
}

/// Resolve `path` for a filesystem tool, enforcing the sandbox path policy when
/// active. `is_write` marks mutating operations (write/edit/make_dir/remove/
/// move-dest/copy-dest), which require an `rw` binding under the policy.
fn get_path_with(path: &str, is_write: bool) -> Result<PathBuf, ToolError> {
    let base = std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?;

    // When a path policy is active (sandbox active with explicit bindings),
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
    name = "read",
    description = "Read file with line numbers",
    required(path, offset, limit)
)]
pub async fn read_file(
    path: String,
    offset: ReadOffset,
    limit: ReadLimit,
) -> Result<String, ToolError> {
    let offset: u64 = offset.into();
    let limit: u64 = limit.into();
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

#[rig_tool(name = "write", description = "Write content to file", required(path, content))]
pub async fn write_file(path: String, content: String) -> Result<String, ToolError> {
    fs::write(get_path_with(&path, true)?, content).await?;
    Ok("ok".into())
}

/// A single targeted text replacement within a file.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct EditOp {
    /// Exact text to replace. Must be unique in the file and must not
    /// overlap with any other edit's old_text in the same call.
    #[serde(rename = "oldText")]
    pub old_text: String,
    /// Replacement text.
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// Arguments accepted by the `edit` tool.
///
/// The canonical wire shape is `{path, edits: [{oldText, newText}]}`. The
/// custom `Deserialize` impl additionally tolerates a legacy single-edit
/// shape (`{path, oldText, newText}`) by normalizing it onto `edits`. That
/// legacy form is deliberately undocumented: the schema (see
/// `EditFile::parameters`) and every error message only ever show the
/// canonical shape, so models are guided toward it.
#[derive(Serialize, JsonSchema)]
pub struct EditArgs {
    /// File to edit, relative to the working directory.
    pub path: String,
    /// One or more targeted text replacements.
    pub edits: Vec<EditOp>,
}

/// Cap on the JSON echoed back in an invalid-parameters error, so a bad call
/// carrying a huge blob (e.g. file contents pasted into the wrong field)
/// can't blow up the tool message.
const MAX_ECHOED_ARGS: usize = 400;

const EDIT_ARGS_EXAMPLE: &str = r#"{
    "path": "some/path",
    "edits": [
        { "oldText": "The original text", "newText": "The replacement text" }
    ]
}"#;

/// Truncate the echoed parameter JSON to [`MAX_ECHOED_ARGS`] chars on a
/// char boundary, appending an ellipsis when cut.
fn truncate_wire_repr(raw: &serde_json::Value) -> String {
    let s = raw.to_string();
    if s.len() <= MAX_ECHOED_ARGS {
        return s;
    }
    let mut cut = MAX_ECHOED_ARGS;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} …", &s[..cut])
}

impl EditArgs {
    /// Parse the wire JSON, accepting the canonical `edits` array and — as
    /// an undocumented compatibility shim — the legacy top-level
    /// single-edit shape. Any other payload yields an error that echoes the
    /// received parameters (truncated) plus the canonical shape, so the
    /// model can self-correct in a single retry.
    fn from_wire(raw: &serde_json::Value) -> Result<Self, String> {
        // Canonical first: exact field names, unknown extras ignored.
        #[derive(serde::Deserialize)]
        struct Canonical {
            path: String,
            edits: Vec<EditOp>,
        }
        if let Ok(c) = serde_json::from_value::<Canonical>(raw.clone()) {
            return Ok(EditArgs {
                path: c.path,
                edits: c.edits,
            });
        }

        // Legacy single-edit shim: {path, oldText, newText} -> one edit.
        #[derive(serde::Deserialize)]
        struct Legacy {
            path: String,
            #[serde(rename = "oldText")]
            old_text: String,
            #[serde(rename = "newText")]
            new_text: String,
        }
        if let Ok(l) = serde_json::from_value::<Legacy>(raw.clone()) {
            return Ok(EditArgs {
                path: l.path,
                edits: vec![EditOp {
                    old_text: l.old_text,
                    new_text: l.new_text,
                }],
            });
        }

        Err(format!(
            "Expected parameters like:\n{}\nInvalid parameters received: {}",
            EDIT_ARGS_EXAMPLE,
            truncate_wire_repr(raw),
        ))
    }
}

impl<'de> Deserialize<'de> for EditArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

/// Static tool facade for `edit`, mirroring what `#[rig_tool]` would emit.
/// Hand-written instead of macro-generated so the args deserializer can
/// accept the legacy single-edit shape without advertising it — see
/// [`EditArgs`]. The `Bash` tool follows the same manual-impl pattern.
pub struct EditFile;

impl rig_core::tool::Tool for EditFile {
    type Args = EditArgs;
    type Output = String;
    type Error = ToolError;

    const NAME: &'static str = "edit";

    fn description(&self) -> String {
        "Replace text in file using one or more targeted edits. Each edit must match unique text. Edits target the original file, not the output of previous edits.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        static SCHEMA: std::sync::LazyLock<serde_json::Value> = std::sync::LazyLock::new(|| {
            let mut schema = schemars::schema_for!(EditArgs).to_value();
            // Providers expect an explicit `required` array even when no
            // parameter is required (mirrors the macro-generated schema).
            if let Some(object) = schema.as_object_mut() {
                object
                    .entry("required")
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            }
            schema
        });
        SCHEMA.clone()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        edit_file(args.path, args.edits).await
    }
}

pub async fn edit_file(
    path: String,
    edits: Vec<EditOp>,
) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    let text = fs::read_to_string(&p).await?;
    let total = edits.len();
    let mut failures: Vec<edit_match::BatchItem> = Vec::new();

    for (i, edit) in edits.iter().enumerate() {
        if !text.contains(&edit.old_text) {
            // Exact match failed. Try to give the model a useful
            // diagnostic for this edit; if no close match exists, fall
            // back to a plain error. We DO NOT return early — collect
            // every edit's failure so the model can fix them all in one
            // retry.
            let edit_idx_1 = i + 1;
            let failure = match edit_match::diagnose(&edit.old_text, &text) {
                Some(mut f) => {
                    // Combine the per-edit ui_view into a single line
                    // so the human sees:
                    //   "edit #1 oldText not found. Closest match at..."
                    // The model_view body remains header-free; the
                    // batch envelope adds the `--- edit #N ---` separator
                    // and the "N of M failed" header.
                    f.ui_view = format!(
                        "edit #{edit_idx_1} oldText not found. {}",
                        f.ui_view,
                    );
                    f
                }
                None => edit_match::EditFailure::not_found_no_match(
                    edit_idx_1, &edit.old_text,
                ),
            };
            failures.push(edit_match::BatchItem {
                edit_index: i,
                failure,
            });
            continue;
        }
        let count = text.matches(&edit.old_text).count();
        if count > 1 {
            // List the line numbers of every occurrence so the model can
            // re-anchor and pick one (or include more surrounding context
            // in oldText). Cap at 10 to keep the message bounded.
            let mut line_numbers: Vec<usize> = Vec::with_capacity(count.min(10));
            let mut listed = 0usize;
            for (byte_off, _) in text.match_indices(&edit.old_text) {
                if listed >= 10 {
                    break;
                }
                line_numbers.push(
                    crate::edit_match::count_lines(&text[..byte_off]) + 1,
                );
                listed += 1;
            }
            failures.push(edit_match::BatchItem {
                edit_index: i,
                failure: edit_match::EditFailure::non_unique(
                    i + 1,
                    &edit.old_text,
                    count,
                    &line_numbers,
                ),
            });
        }
    }

    if !failures.is_empty() {
        // All-or-nothing: do NOT apply any edits when the validation
        // pass produced failures. Surfacing the full list lets the model
        // fix all problems in one retry instead of ping-ponging.
        let batch = edit_match::BatchEditFailure::build(total, failures);
        return Ok(edit_match::render_batch(&batch));
    }

    let mut result = text;
    for edit in &edits {
        result = result.replacen(&edit.old_text, &edit.new_text, 1);
    }
    fs::write(&p, result).await?;
    // Render the path the model will see as relative to cwd when possible,
    // so the success string stays portable across checkouts (the model
    // already has cwd in its system prompt) and tokens aren't wasted on
    // the absolute prefix. We deliberately don't try to relativize paths
    // outside cwd — see `path_util::relativize_path`.
    let cwd = std::env::current_dir().unwrap_or_else(|_| p.clone());
    let shown = relativize_path(&p, &cwd);
    Ok(format!(
        "Replaced {} block(s) in {}",
        edits.len(),
        shown
    ))
}

#[rig_tool(
    name = "find",
    description = "Find files by pattern, sorted by mtime",
    required(pattern, path)
)]
pub async fn glob_files(pattern: String, path: String) -> Result<String, ToolError> {
    let base = get_path(&path)?;
    let matcher = globset::Glob::new(&pattern)
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
    // Format matches as paths relative to cwd (when under cwd). This keeps
    // the model-facing output portable across checkouts of the same repo
    // and avoids burning tokens on the absolute cwd prefix. We resolve cwd
    // here (after the walker has returned and `base` is no longer in use)
    // and reuse it for every entry.
    let cwd = std::env::current_dir().unwrap_or_default();
    let res = files
        .iter()
        .map(|(f, _)| relativize_path(f, &cwd))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(if res.is_empty() { "none".into() } else { res })
}

#[rig_tool(name = "grep", description = "Search files for a regex pattern (ripgrep)", required(pattern, path))]
pub async fn grep_text(pattern: String, path: String) -> Result<String, ToolError> {
    // Validate the search root against the active path policy (or cwd bounds);
    // ripgrep itself runs confined inside a read-only sandbox.
    let resolved = get_path(&path)?;

    // Pre-relativize the search root so ripgrep's `--with-filename` output
    // comes back in the portable, token-friendly form. We deliberately do
    // NOT post-process ripgrep's output (which contains matched file
    // *content*) — that would risk rewriting paths inside the lines the
    // user actually searched for. ripgrep echoes the exact path it was
    // given, so as long as we feed it a relative form, the filename
    // column stays relative.
    let cwd = std::env::current_dir().unwrap_or_else(|_| resolved.clone());
    let rg_path = relativize_path(&resolved, &cwd);

    let cfg = sandbox_config();
    let cfg = cfg.as_ref().as_ref().cloned().unwrap_or_default();
    let timeout: Option<u64> = cfg.timeout_secs.map(|v| v.into());

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
        pattern,
        rg_path,
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
    pub command: String,
    /// Optional timeout in seconds for this command. Overrides the configured
    /// default (`tool_config.bash.sandbox.timeout_secs`). Must be a positive
    /// finite number; values above the maximum are rejected. Leave unset to use
    /// the configured default.
    #[serde(default)]
    pub timeout: BashTimeout,
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
        let (out, exit_code) = run_sandboxed_with_status(&args.command, args.timeout.map(|v| v.into())).await?;
        // The bash tool result is shown verbatim to the user (via
        // `LoggingHook::on_tool_result` → `display_tool_result`) AND handed
        // straight back to the LLM as the tool message. An empty stdout
        // is therefore ambiguous to both audiences: the model can't tell
        // success (e.g. `mkdir -p foo`) from failure (e.g. `false` or
        // command-not-found) and the user sees the same opaque "no
        // output". Surface the exit code so both can act on it.
        // `run_sandboxed` itself returns `"(empty)"` for an empty stream;
        // override that here with the more informative message.
        if out == "(empty)" {
            let code = match exit_code {
                Some(n) => n.to_string(),
                None => "unknown (terminated by signal)".to_string(),
            };
            Ok(format!("[bash] no output, exit code {code}"))
        } else {
            Ok(out)
        }
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

pub(crate) fn sandbox_config() -> Arc<Option<SandboxConfig>> {
    SANDBOX.lock().expect("sandbox lock poisoned").clone()
}

/// Run `ls -1p` inside the sandbox and return the listing.
/// Returns `None` when no sandbox is configured, bwrap is unavailable,
/// or the directory doesn't exist inside the sandbox.
pub(crate) fn sandbox_list_dir(dir: &str) -> Option<Vec<String>> {
    let cfg = sandbox_config();
    let mut cfg = (*cfg).clone()?;
    crate::sandbox_bindings::merge_into(&mut cfg);
    let args = build_bwrap_args(&cfg, true).ok()?;

    let output = std::process::Command::new("bwrap")
        .args(&args)
        .arg("--")
        .args(["ls", "-1ap", dir])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(
        stdout
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
    )
}

/// Resolve the active path policy for the non-shell file tools. Returns `None`
/// when the sandbox is disabled, bwrap is absent, or no explicit bindings are
/// configured (permissive default). Mirrors the shell-sandboxing activation
/// condition so the two worlds stay consistent.
fn current_path_policy() -> Option<crate::config::PathPolicy> {
    let cfg = sandbox_config();
    let c = cfg.as_ref().as_ref()?;
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
/// timeout (so a hung command reaps and releases the blocking thread) and
/// always wraps the command in a Bubblewrap namespace. `bwrap` is required at
/// startup (see `check_runtime_deps`), so its absence is handled before any
/// command runs; if no sandbox config is set, the command is refused rather
/// than run unsandboxed — there is no plain-shell fallback (and no enable
/// switch).
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
    // timeout is enforced whenever the command actually runs; an unset sandbox
    // config is refused before the timeout is relevant (see the closure below).
    let configured = cfg.as_ref().as_ref().and_then(|c| c.timeout_secs.clone().map(|v| v.into()));
    let timeout = resolve_timeout(override_timeout, configured)?;

    // Move owned copies into the blocking task (`cmd` cloned to a String, the
    // `Arc` config cheaply cloned) so the closure is `'static`. Everything runs
    // in one task so the non-`Clone` `Handle` stays owned where it can be killed.
    let cmd = cmd.to_string();
    let cfg = cfg.clone();
    let output = tokio::task::spawn_blocking(move || {
        // Sandboxing is mandatory: every shell command runs inside the bwrap
        // namespace. There is deliberately no unsandboxed fallback — running a
        // command outside the sandbox would grant it full access to the host
        // filesystem. `bwrap` is required at startup (see `check_runtime_deps`)
        // and the resolved config is always `Some(..)`, so this error only fires
        // if the sandbox was never configured/published.
        let cfg = match cfg.as_ref().as_ref() {
            Some(c) => c,
            None => {
                return Err(ToolError::Generic(
                    "sandbox not configured; refusing to run command".into(),
                ))
            }
        };
        // Sandboxed: stream combined stdout+stderr with a bounded reader. bwrap
        // forwards the child's stdio to its inherited fds, so we read the pipe
        // in fixed chunks and keep at most `max_output_bytes` (configurable:
        // `[tool_config.bash.sandbox].max_output_bytes`, default 20 KiB) in
        // memory — no temp file and no unbounded `read_to_end` (see
        // `capture_bounded`).
        let cap = cfg.max_output_bytes();
        let expr = build_bwrap_expr(&cmd, cfg, false)?;
        // Discard the exit code here — `run_sandboxed` is the shared helper
        // for paths (the `!`/`!!` user shell, `agent_browser`) that don't
        // surface it. The bash tool uses `Bash::call` directly, which goes
        // through `run_sandboxed_with_status` to get the exit code.
        capture_bounded(expr, timeout, cap).map(|(out, _exit)| out)
    })
    .await
    .map_err(|e| ToolError::Generic(e.to_string()))??;

    Ok(trim_output(output))
}

/// Same as [`run_sandboxed`] but also returns the child's exit code, so the
/// bash tool can report a meaningful "[bash] no output, exit code N" when the
/// command produced no output. `None` means the child was killed by a signal
/// or otherwise terminated without an observable exit code (e.g. watchdog
/// kill, IO error mid-run).
pub(crate) async fn run_sandboxed_with_status(
    cmd: &str,
    override_timeout: Option<u64>,
) -> Result<(String, Option<i32>), ToolError> {
    let cfg = sandbox_config();

    // The LLM may override the configured default timeout per command. The
    // timeout is enforced whenever the command actually runs; an unset sandbox
    // config is refused before the timeout is relevant (see the closure below).
    let configured = cfg.as_ref().as_ref().and_then(|c| c.timeout_secs.clone().map(|v| v.into()));
    let timeout = resolve_timeout(override_timeout, configured)?;

    // Move owned copies into the blocking task (`cmd` cloned to a String, the
    // `Arc` config cheaply cloned) so the closure is `'static`. Everything runs
    // in one task so the non-`Clone` `Handle` stays owned where it can be killed.
    let cmd = cmd.to_string();
    let cfg = cfg.clone();
    let (output, exit_code) = tokio::task::spawn_blocking(move || {
        // Same sandbox-required contract as `run_sandboxed` — see that fn's
        // comment for the rationale. The body is intentionally duplicated
        // rather than shared via a free helper because the only divergent
        // piece is which return shape we hand back; folding them together
        // would require every other caller to destructure a tuple.
        let cfg = match cfg.as_ref().as_ref() {
            Some(c) => c,
            None => {
                return Err(ToolError::Generic(
                    "sandbox not configured; refusing to run command".into(),
                ))
            }
        };
        let cap = cfg.max_output_bytes();
        let expr = build_bwrap_expr(&cmd, cfg, false)?;
        capture_bounded(expr, timeout, cap)
    })
    .await
    .map_err(|e| ToolError::Generic(e.to_string()))??;

    Ok((trim_output(output), exit_code))
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
    let cap = cfg.max_output_bytes();
    let output = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Option<i32>), ToolError> {
        let expr = build_bwrap_expr_argv(&argv, &cfg, true)?;
        // Stream merged stdout+stderr in bounded chunks (matches the shell
        // path: bwrap forwards stdio to inherited fds; stderr is merged in,
        // as `2>&1` would do). Output is capped to `cap` (configurable:
        // `[tool_config.bash.sandbox].max_output_bytes`, default 20 KiB).
        capture_bounded(expr, timeout, cap)
    })
    .await
    .map_err(|e| ToolError::Generic(e.to_string()))??;
    // Exit code is discarded — read-only tools don't surface it to the model
    // (grep/list_dir etc. treat a non-zero status as a non-error "no match").
    let (output, _exit) = output;
    Ok(String::from_utf8_lossy(&output).to_string())
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

/// Stream merged stdout+stderr from a started bwrap expression in bounded
/// chunks, returning at most `max_bytes` of output.
///
/// Unlike [`duct::Expression::stdout_capture`], which materializes the *entire*
/// child stream into an unbounded `Vec` (a `read_to_end` with no size limit),
/// this reads through an `os_pipe` `ReaderHandle` in fixed 8 KiB chunks and
/// keeps at most `max_bytes` in memory. Once the cap is reached the pipe is
/// still **drained** (so the child never blocks on a full kernel buffer), but
/// the surplus bytes are discarded. stderr is merged into stdout via
/// `stderr_to_stdout`, matching the shell `2>&1` semantics both callers want.
///
/// The wall-clock `timeout` is enforced by a watchdog thread. `ReaderHandle`
/// has no `wait_timeout`, so the watchdog sleeps for `timeout` and then calls
/// `kill()`, which closes the child's pipe and unblocks the reading thread.
/// `read()` is a blocking syscall, so the read loop is *not* a busy-wait: the
/// thread parks in the kernel until data/EOF arrives and the timeout check runs
/// only once per returned chunk.
fn capture_bounded(
    expr: duct::Expression,
    timeout: Option<u64>,
    max_bytes: usize,
) -> Result<(Vec<u8>, Option<i32>), ToolError> {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    let handle = expr
        .stderr_to_stdout()
        .unchecked()
        .reader()
        .map_err(|e| ToolError::Io(e.to_string()))?;
    let handle = Arc::new(handle);

    // Watchdog: if the command runs past `timeout`, OR the user presses `<Esc>`,
    // kill the child so the blocked `read` below returns and we surface a clear
    // error.  Only direct children (bwrap) are killed; bwrap's
    // `--die-with-parent`/namespace reaping handles the sandbox internals.
    //
    // `cancel_tx` is the early-exit signal: dropping it closes the channel,
    // which unblocks `recv_timeout` in the watchdog thread immediately so that
    // `g.join()` returns promptly when the command finishes before the timeout.
    // A clone of `cancel_tx` is registered in the process-wide
    // `TOOL_CANCEL_SENDERS` list so the in-flight reader thread can send `()`
    // on it when `<Esc>` is pressed — waking the watchdog to kill the child.
    let timed_out = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();

    // Register a clone of cancel_tx in the process-wide sender registry,
    // so the in-flight reader thread can send `()` on it when `<Esc>` is
    // pressed.  We deregister explicitly before `drop(cancel_tx)` below so
    // the channel is fully disconnected and the watchdog exits promptly.
    if let Ok(mut senders) = TOOL_CANCEL_SENDERS.lock() {
        senders.push(cancel_tx.clone());
    }

    let guard: Option<std::thread::JoinHandle<()>> = timeout.map(|secs| {
        let w = handle.clone();
        let flag = timed_out.clone();
        let cancel_flag = cancelled.clone();
        std::thread::Builder::new()
            .name("sandbox-timeout".to_string())
            .spawn(move || {
                // Block until the timeout elapses *or* something sends on
                // cancel_rx (either `cancel_tx` being dropped because the
                // command finished, or a `send(())` from the in-flight reader
                // when the user pressed `<Esc>`).
                match cancel_rx.recv_timeout(Duration::from_secs(secs)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // Timeout elapsed: mark as timed out and kill.
                        flag.store(true, Ordering::SeqCst);
                        let _ = w.kill();
                    }
                    Ok(()) => {
                        // Woken by a cancel signal (user pressed `<Esc>`).
                        cancel_flag.store(true, Ordering::SeqCst);
                        let _ = w.kill();
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        // All senders dropped — command finished normally.
                        // No kill needed.
                    }
                }
            })
            .map_err(|e| ToolError::Io(e.to_string()))
            .expect("sandbox timeout thread")
    });

    let mut out: Vec<u8> = Vec::with_capacity(max_bytes.min(8192).max(1));
    let mut buf = [0u8; 8192];
    let mut total: usize = 0;
    let mut exceeded = false;
    let mut reader: &duct::ReaderHandle = &handle;

    let io_result = loop {
        match (&mut reader).read(&mut buf) {
            // EOF: duct's Read impl already called handle.wait() internally.
            Ok(0) => break Ok(()),
            Ok(n) => {
                total += n;
                if !exceeded {
                    let room = max_bytes.saturating_sub(out.len());
                    if n <= room {
                        out.extend_from_slice(&buf[..n]);
                    } else {
                        out.extend_from_slice(&buf[..room]);
                        exceeded = true;
                    }
                }
            }
            // A read error after the watchdog fired means we killed the
            // child; surface as a timeout rather than a spurious IO error.
            Err(e) => break Err(e),
        }
    };

    // Deregister from the process-wide cancel-sender registry BEFORE
    // dropping `cancel_tx`, so the channel is fully disconnected and the
    // watchdog's `recv_timeout` returns `Disconnected` immediately.
    if let Ok(mut senders) = TOOL_CANCEL_SENDERS.lock() {
        let _ = senders.pop();
    }

    // Signal the watchdog to exit immediately: closing cancel_tx makes
    // recv_timeout return Disconnected, so the watchdog exits without killing
    // anything.  If the watchdog already fired (Timeout/Cancel), this is a
    // no-op and join() returns as soon as the thread finishes its kill() call.
    drop(cancel_tx);
    if let Some(g) = guard {
        let _ = g.join();
    }

    // Now that the watchdog has definitively finished, check its flags.
    // Timeout takes precedence (more informative error), then cancel.
    if timed_out.load(Ordering::SeqCst) {
        return Err(ToolError::Generic(format!(
            "command timed out after {}s",
            timeout.unwrap_or(0)
        )));
    }
    if cancelled.load(Ordering::SeqCst) {
        return Err(ToolError::Generic("cancelled by user".into()));
    }
    if let Err(e) = io_result {
        return Err(ToolError::Io(e.to_string()));
    }

    if exceeded {
        let dropped = total.saturating_sub(max_bytes);
        let note = format!(
            "\n\n[output truncated: {dropped} bytes dropped, cap is {max_bytes} bytes]"
        );
        out.extend_from_slice(note.as_bytes());
    }

    // The child has either exited (EOF observed) or been killed by the
    // watchdog (in which case `try_wait` still resolves to the dead process's
    // status — typically the signal code, which `ExitStatus::code` reports as
    // `None`). Either way the call is non-blocking now: the IO thread has
    // already finished by the time the read loop returned `Ok(0)`, and a
    // watchdog kill closes the pipe so the read also unblocks. We surface
    // the exit code (or `None` for signal-terminated / unobservable
    // terminations) so callers like the bash tool can render a meaningful
    // "[bash] no output, exit code N" message when the command produced no
    // stdout/stderr.
    let exit_code = handle
        .try_wait()
        .ok()
        .flatten()
        .and_then(|out| out.status.code());

    Ok((out, exit_code))
}

/// Read-only system directories bound into every sandbox so the shell and
/// common tools can execute. They are read-only, so the host OS stays hidden
/// from writes; only explicitly declared bindings add writable/project paths.
const ESSENTIAL_RO_DIRS: &[&str] = &["/usr", "/bin", "/lib", "/lib64"];

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

/// Whether `prlimit` (util-linux) is available to set `RLIMIT_AS` via a direct
/// argv exec. Cached after the first probe: `prlimit` lives under `/usr/bin`,
/// which is bound read-only into every sandbox, so host availability implies
/// in-namespace availability.
fn prlimit_available() -> bool {
    static AVAILABLE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::process::Command::new("prlimit")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    *AVAILABLE
}

/// Return the post-`--` argv that runs `target` under the configured memory cap
/// (if any).
///
/// * With `prlimit` available: `prlimit --as=<bytes> -- <target…>` — a direct
///   argv exec, so the limit step is never interpreted by a shell. Works for
///   both the shell and argv execution paths.
/// * Without `prlimit`: when `allow_shell_fallback` is set (the shell path,
///   which already runs `sh -c`), prepend `ulimit -v <KB>; ` to the payload so
///   the cap is still applied via the shell builtin. The argv path passes
///   `false`, so it runs `target` uncapped (matching prior behavior, where the
///   argv path never enforced a memory limit).
fn memory_wrapped_target(
    memory_mb: crate::config::SandboxMemoryMb,
    target: Vec<String>,
    allow_shell_fallback: bool,
) -> Vec<String> {
    let memory_mb: Option<u64> = memory_mb.into();
    let Some(mb) = memory_mb else {
        return target;
    };
    if prlimit_available() {
        // `prlimit --as` takes bytes; the shell `ulimit -v` equivalent is KB.
        let mut v = vec![
            "prlimit".into(),
            "--as".into(),
            (mb * 1024 * 1024).to_string(),
            "--".into(),
        ];
        v.extend(target);
        return v;
    }
    if allow_shell_fallback {
        // `target` is `sh -c <payload>`; prepend `ulimit -v` to the payload so
        // the existing two-level shell still applies the cap. This reproduces
        // the original `sh -c 'ulimit -v <KB>; exec sh -c <cmd>'` wrapper
        // exactly (the limit is set before the inner shell is exec'd).
        let payload = target.get(2).cloned().unwrap_or_default();
        let kb = mb * 1024;
        return vec!["sh".into(), "-c".into(), format!("ulimit -v {kb}; {payload}")];
    }
    target
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
    let mut binds = resolve_paths(&cwd, home.as_deref(), &cfg.ro, &cfg.rw)
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
    // Mount /etc: when the user binds sub-paths of /etc, we create an
    // empty directory first so their mounts have a place to land.  When
    // nothing references /etc at all, fall back to ro-binding the whole
    // directory (old behavior).  When an exact /etc bind is configured,
    // the user takes full control and we add nothing extra.
    let etc_exact = binds.iter().any(|b| b.to == std::path::Path::new("/etc"));
    let etc_sub   = binds.iter().any(|b| b.to.starts_with("/etc/"));
    if etc_exact {
        // User provides their own /etc binding.
    } else if etc_sub {
        // User binds sub-paths under /etc; create the empty directory first.
        args.push("--dir".into());
        args.push("/etc".into());
    } else if std::path::Path::new("/etc").is_dir() {
        // No /etc bindings at all — ro-bind the full directory.
        args.push("--ro-bind".into());
        args.push("/etc".into());
        args.push("/etc".into());
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
            // `/dev` and `/proc` can NOT be mounted with `--bind`/`--ro-bind`.
            // Binding `/dev` that way produces a *non-functional* device tree
            // (e.g. `/dev/null` becomes unopenable with EACCES), and `/proc`
            // simply is not mountable as a regular bind at all. bwrap provides
            // dedicated `--dev <mnt>` and `--proc <mnt>` flags that populate a
            // fresh, working device tree / procfs at the given mountpoint. So a
            // binding whose in-sandbox target is `/dev`/`/proc` is translated to
            // the corresponding special flag. The host `from` path is ignored
            // by `--dev`/`--proc` (they always build a fresh instance). A
            // trailing slash is tolerated for `to` (e.g. "/dev/").
            let to = b.to.to_string_lossy();
            let to = to.trim_end_matches('/');
            if to == "/dev" {
                args.push("--dev".into());
                args.push("/dev".into());
                continue;
            }
            if to == "/proc" {
                args.push("--proc".into());
                args.push("/proc".into());
                continue;
            }
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
/// combined stdout/stderr captured directly by duct (bwrap forwards the child's
/// stdio to its inherited fds — the same mechanism the read-only argv path uses
/// for grep/rg). No temp file is involved; the caller captures the output via
/// `stdout_capture`/`stderr_capture`. Used by the bash tool.
fn build_bwrap_expr(
    cmd: &str,
    cfg: &SandboxConfig,
    read_only: bool,
) -> Result<duct::Expression, ToolError> {
    let mut args = build_bwrap_args(cfg, read_only)?;

    // The user command is a shell string, so it must run via `sh -c`. We use a
    // two-level shell: the outer `sh -c` `exec`s an inner `sh -c` that actually
    // interprets the (single-quoted) user command, so pipes/redirections inside
    // it work. `exec` replaces the outer shell so only one shell process runs
    // the command. Output is captured by the caller (direct stdout reads).
    let cmd_quoted = sh_single_quote(&cmd);
    let payload = format!("exec sh -c {cmd_quoted}");
    let target: Vec<String> = vec!["sh".into(), "-c".into(), payload];

    // Apply the memory cap as a `prlimit --as=…` wrapper when available (direct
    // argv, no shell interpreting the limit). Fall back to the shell `ulimit`
    // builtin, which reproduces the original `sh -c 'ulimit -v …; exec sh -c
    // <cmd>'` wrapper exactly. See `memory_wrapped_target`.
    let post = memory_wrapped_target(cfg.memory_mb, target, true);

    args.push("--".into());
    args.extend(post);

    Ok(duct::cmd("bwrap", &args))
}

/// Build a `duct` expression that runs a program with an explicit argv list
/// inside a bwrap namespace, with NO shell in between. Each argument is passed
/// verbatim to the program, so regex patterns and paths need no quoting or
/// escaping and shell metacharacters are never interpreted. Output is captured
/// directly by duct (bwrap forwards the child's stdio to its inherited fds).
///
/// The memory cap (if configured) is applied via `prlimit --as=…`, a direct
/// argv exec — so this path now enforces `memory_mb`, which it previously did
/// not.
fn build_bwrap_expr_argv(
    argv: &[String],
    cfg: &SandboxConfig,
    read_only: bool,
) -> Result<duct::Expression, ToolError> {
    let mut args = build_bwrap_args(cfg, read_only)?;
    // `false`: no shell is involved, so the `ulimit` fallback cannot apply;
    // when `prlimit` is absent the cap is simply skipped (prior behavior).
    let post = memory_wrapped_target(cfg.memory_mb, argv.to_vec(), false);
    args.push("--".into());
    args.extend(post);
    Ok(duct::cmd("bwrap", &args))
}

/// Best-effort home directory lookup for `~` expansion in bindings.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[rig_tool(name = "ls", description = "List files and directories in a path", required(path))]
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
    use serial_test::serial;

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

    /// RAII helper that chdirs into a fresh tempdir and restores the
    /// original cwd on drop. Required by `glob_files` tests because
    /// `get_path` enforces the result is inside cwd (and the global cwd
    /// races across parallel tests if mutated inline).
    struct TempCwd {
        _dir: tempfile::TempDir,
        prev: std::path::PathBuf,
    }

    impl TempCwd {
        fn new() -> Self {
            crate::tools::set_sandbox(None);
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir.path()).unwrap();
            TempCwd { _dir: dir, prev }
        }
    }

    impl Drop for TempCwd {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    #[tokio::test]
    #[serial]
    async fn glob_files_results_are_relative_to_cwd() {
        // The model passes an absolute path to glob_files; the matched
        // file paths in the tool result must come back as `./x` (or
        // `./sub/x`), not `<tempdir>/x`. Otherwise the absolute cwd prefix
        // burns tokens on every result and breaks session portability
        // across checkouts of the same repo.
        let guard = TempCwd::new();
        tokio::fs::create_dir_all("sub").await.unwrap();
        tokio::fs::write("a.txt", "x").await.unwrap();
        tokio::fs::write("sub/b.txt", "y").await.unwrap();

        let cwd = std::env::current_dir().unwrap();
        let search_root = cwd.to_string_lossy().into_owned();
        let res = glob_files("*.txt".to_string(), search_root)
            .await
            .unwrap();

        // The matched path must appear in relative form.
        assert!(
            res.contains("./a.txt"),
            "expected `./a.txt` in glob output, got: {res}",
        );
        // And the absolute tempdir path must NOT leak.
        let tempdir_str = guard._dir.path().to_string_lossy().into_owned();
        assert!(
            !res.contains(&tempdir_str),
            "absolute tempdir leaked into glob output: {res}",
        );
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use crate::config::{BindPath, EnvMode, EnvPolicy, SandboxConfig};
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

    fn sandbox_with(
        ro: Vec<BindPath>,
        rw: Vec<BindPath>,
        timeout_secs: Option<u64>,
    ) -> SandboxConfig {
        SandboxConfig {
            timeout_secs: timeout_secs.map(|v| crate::config::SandboxTimeoutSecs(Some(v))).unwrap_or(crate::config::SandboxTimeoutSecs(None)),
            network: Some(false),
            memory_mb: crate::config::SandboxMemoryMb(None),
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: crate::config::SandboxTmpSizeMb(Some(16)),
            ro,
            rw,
            env: None,
            max_output_bytes: crate::config::SandboxMaxOutputBytes(None),
        }
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_echo_works() {
        if !bwrap_on_path() {
            return;
        }
        // Default (empty paths): read-only CWD + writable tmpfs.
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        let out = run_sandboxed("echo hello-world", None).await.unwrap();
        assert_eq!(out, "hello-world");
    }

    #[tokio::test]
    #[serial]
    async fn run_sandboxed_with_status_reports_exit_code_on_success() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        let (out, code) = run_sandboxed_with_status("true", None).await.unwrap();
        assert_eq!(out, "(empty)");
        assert_eq!(code, Some(0));
    }

    #[tokio::test]
    #[serial]
    async fn run_sandboxed_with_status_reports_nonzero_exit_code() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        // `sh -c 'exit 7'` produces no output but a known non-zero exit.
        let (out, code) = run_sandboxed_with_status("exit 7", None).await.unwrap();
        assert_eq!(out, "(empty)");
        assert_eq!(code, Some(7));
    }

    #[tokio::test]
    #[serial]
    async fn bash_tool_empty_output_includes_exit_code() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        // Drive the tool the same way rig does: build a ToolContext and call
        // `call` with a deserialised BashArgs. The empty-output case is
        // re-shaped into the "[bash] no output, exit code N" message so
        // both the user (via `display_tool_result`) and the model (via
        // rig's tool message) see a useful signal instead of `"(empty)"`.
        use rig_core::tool::Tool;
        let mut ctx = rig_core::tool::ToolContext::default();
        let args = serde_json::from_value::<BashArgs>(serde_json::json!({
            "command": "exit 0",
        }))
        .unwrap();
        let out = Bash.call(&mut ctx, args).await.unwrap();
        assert_eq!(out, "[bash] no output, exit code 0");

        let args = serde_json::from_value::<BashArgs>(serde_json::json!({
            "command": "exit 42",
        }))
        .unwrap();
        let out = Bash.call(&mut ctx, args).await.unwrap();
        assert_eq!(out, "[bash] no output, exit code 42");
    }

    #[tokio::test]
    #[serial]
    async fn bash_tool_passes_through_nonempty_output_unchanged() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        use rig_core::tool::Tool;
        let mut ctx = rig_core::tool::ToolContext::default();
        let args = serde_json::from_value::<BashArgs>(serde_json::json!({
            "command": "echo hi",
        }))
        .unwrap();
        let out = Bash.call(&mut ctx, args).await.unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_tmpfs_is_writable() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        let out = run_sandboxed("echo x > /tmp/probe && cat /tmp/probe", None).await.unwrap();
        assert_eq!(out, "x");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_timeout_kills_hung_command() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(1))));
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
        set_sandbox(Some(sandbox_with(
            vec![],
            vec![BindPath::Remap {
                from: dir.path().to_string_lossy().to_string(),
                to: "/work".to_string(),
            }],
            Some(30),
        )));
        let out = run_sandboxed("echo inside > /work/probe.txt && cat /work/probe.txt", None)
            .await
            .unwrap();
        assert_eq!(out, "inside");
        // The write landed on the host temp dir.
        assert!(dir.path().join("probe.txt").exists());
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_dev_bind_is_functional() {
        // A naive `--bind /dev /dev` yields a non-functional device tree
        // (e.g. `/dev/null` becomes unopenable). `build_bwrap_args` must
        // translate a `/dev` binding to bwrap's dedicated `--dev /dev` flag so
        // the standard device nodes actually work inside the namespace.
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![BindPath::Simple("/dev".into())], Some(30))));
        // Open /dev/null read-write; many tools (git, compilers, ...) require it.
        let out = run_sandboxed(
            "exec 3<>/dev/null && echo DEVNULL_RW_OK || echo DEVNULL_RW_FAIL",
            None,
        )
        .await
        .unwrap();
        assert_eq!(out, "DEVNULL_RW_OK");
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_proc_bind_is_functional() {
        // `/proc` must be translated to bwrap's `--proc /proc` flag; a plain
        // bind would not mount a usable procfs. With PID isolation on, the
        // namespace's own init should be visible under /proc.
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![BindPath::Simple("/proc".into())], Some(30))));
        let out = run_sandboxed("test -f /proc/self/status && echo PROC_OK || echo PROC_FAIL", None)
            .await
            .unwrap();
        assert_eq!(out, "PROC_OK");
    }

    // ---- Phase 1b: LLM-configurable per-call timeout override ----

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_short_kills_hung_command() {
        // A short per-call override short-circuits the long configured default
        // and kills a hung command.
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(300))));
        let res = run_sandboxed("sleep 30", Some(1)).await;
        assert!(res.is_err(), "expected timeout error, got {:?}", res);
        assert!(res.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_positive_works() {
        // A generous override lets a quick command succeed even when the
        // configured default is too short.
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(1))));
        let out = run_sandboxed("echo ok", Some(30)).await.unwrap();
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_zero_rejected() {
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(300))));
        let res = run_sandboxed("echo hi", Some(0)).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid timeout"));
    }

    #[tokio::test]
    #[serial]
    async fn bash_timeout_override_above_max_rejected() {
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(300))));
        let res = run_sandboxed("echo hi", Some(super::MAX_TIMEOUT_SECS + 1)).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid timeout"));
    }

    // ---- Phase 3: non-shell path policy ----

    /// Set a sandbox with explicit bindings so the non-shell path policy is
    /// active (requires bwrap, like the shell sandbox).
    fn sandbox_with_paths(ro: Vec<BindPath>, rw: Vec<BindPath>) {
        set_sandbox(Some(sandbox_with(ro, rw, Some(30))));
    }

    #[tokio::test]
    #[serial]
    async fn nonshell_ro_write_rejected() {
        if !bwrap_on_path() {
            return;
        }
        // CWD bound read-only.
        let cwd = std::env::current_dir().unwrap();
        sandbox_with_paths(
            vec![BindPath::Remap {
                from: cwd.to_string_lossy().to_string(),
                to: cwd.to_string_lossy().to_string(),
            }],
            vec![],
        );
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
        sandbox_with_paths(
            vec![],
            vec![BindPath::Remap {
                from: dir.path().to_string_lossy().to_string(),
                to: dir.path().to_string_lossy().to_string(),
            }],
        );
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
        sandbox_with_paths(
            vec![BindPath::Remap {
                from: ro_sub.to_string_lossy().to_string(),
                to: "/proj/ro".to_string(),
            }],
            vec![BindPath::Remap {
                from: dir.path().to_string_lossy().to_string(),
                to: "/proj".to_string(),
            }],
        );
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
        sandbox_with_paths(
            vec![],
            vec![BindPath::Remap {
                from: dir.path().to_string_lossy().to_string(),
                to: "/tmp".to_string(),
            }],
        );

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
        let read = super::read_file("/tmp/x2.txt".to_string(), crate::config::ReadOffset(0), crate::config::ReadLimit(0)).await.unwrap();
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
        let specs_ro = vec![BindPath::Remap {
            from: ro_sub.to_string_lossy().to_string(),
            to: "/proj/ro".to_string(),
        }];
        let specs_rw = vec![BindPath::Remap {
            from: dir.path().to_string_lossy().to_string(),
            to: "/proj".to_string(),
        }];
        set_sandbox(Some(sandbox_with(specs_ro, specs_rw, Some(30))));
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
            timeout_secs: crate::config::SandboxTimeoutSecs(Some(30)),
            network: Some(false),
            memory_mb: crate::config::SandboxMemoryMb(None),
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: crate::config::SandboxTmpSizeMb(Some(16)),
            ro: vec![],
            rw: vec![],
            env: Some(env),
            max_output_bytes: crate::config::SandboxMaxOutputBytes(None),
        }
    }

    #[tokio::test]
    #[serial]
    async fn default_env_drops_secret_but_keeps_path() {
        if !bwrap_on_path() {
            return;
        }
        // No `env` configured => built-in deny-secrets default (drops *_KEY etc).
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
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
    async fn missing_sandbox_is_refused() {
        // With no sandbox config set, a command must be refused rather than run
        // unsandboxed through a plain shell.
        set_sandbox(None);
        let res = run_sandboxed("echo hi-there", None).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err().to_string().contains("sandbox not configured"),
            "expected refusal, not an unsandboxed run"
        );
    }

    #[tokio::test]
    #[serial]
    async fn sandboxed_captures_whole_command_and_expands_vars() {
        if !bwrap_on_path() {
            return;
        }
        set_sandbox(Some(sandbox_with(vec![], vec![], Some(30))));
        // A multi-statement command: bwrap captures the *whole* command's
        // combined output directly, not just the last segment.
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

    #[tokio::test]
    #[serial]
    async fn grep_text_uses_relative_search_root() {
        if !bwrap_on_path() {
            return;
        }
        // Even when the model passes an ABSOLUTE path to grep_text, the
        // `--with-filename` line prefix must come back relative to cwd —
        // otherwise every grep result burns tokens on the cwd prefix and a
        // captured session breaks when replayed at a different checkout.
        set_sandbox(None);
        let cwd = std::env::current_dir().unwrap();
        let absolute = cwd.join("src/main.rs");
        let out = grep_text("fn main".to_string(), absolute.to_string_lossy().into_owned())
            .await
            .unwrap();
        assert!(
            out.contains("src/main.rs:"),
            "expected `src/main.rs:` prefix (relative form), got: {out}",
        );
        assert!(
            !out.contains(&cwd.to_string_lossy().into_owned()),
            "absolute cwd leaked into grep output: {out}",
        );
    }
}

#[cfg(test)]
mod edit_file_tests {
    //! End-to-end tests for `edit_file`'s fuzzy-diagnosis path. We don't
    //! set up a sandbox (so the test stays fast and runs anywhere); the
    //! relevant code path bypasses the sandbox when none is configured.

    use super::*;
    use crate::edit_match::looks_like_edit_failure;
    use serde_json::Value;
    use serial_test::serial;
    use tempfile::tempdir;

    /// RAII guard that chdirs into a fresh tempdir and restores the
    /// original cwd on drop. Required because `get_path_with` requires
    /// the file to be inside CWD, and parallel tests stepping on the
    /// same global cwd would otherwise race. `pub(crate)` so sibling
    /// test modules (e.g. `edit_args_wire_tests`) can reuse it.
    pub(crate) struct CwdGuard {
        _dir: tempfile::TempDir,
        prev: std::path::PathBuf,
    }

    impl CwdGuard {
        pub(crate) fn new() -> Self {
            crate::tools::set_sandbox(None);
            let dir = tempdir().unwrap();
            let prev = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir.path()).unwrap();
            CwdGuard { _dir: dir, prev }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    #[tokio::test]
    #[serial]
    async fn exact_match_succeeds_and_does_not_emit_diagnostic() {
        let _cwd = CwdGuard::new();
        tokio::fs::write("a.txt", "hello\nworld\n").await.unwrap();
        let res = edit_file(
            "a.txt".into(),
            vec![EditOp {
                old_text: "hello".into(),
                new_text: "goodbye".into(),
            }],
        )
        .await
        .unwrap();
        assert!(res.starts_with("Replaced"));
        let after = tokio::fs::read_to_string("a.txt").await.unwrap();
        assert_eq!(after, "goodbye\nworld\n");
    }

    #[tokio::test]
    #[serial]
    async fn success_string_uses_relative_path() {
        // Inside `CwdGuard::new()` the tool resolves cwd to the tempdir it
        // chdir'd into. The file is `a.txt` *in that tempdir*, so the
        // success string must render the path as `./a.txt` — not the
        // absolute tempdir prefix. This keeps the model-facing output
        // portable (no cwd leakage) and token-efficient (no wasted cwd
        // prefix on every tool result).
        let guard = CwdGuard::new();
        tokio::fs::write("a.txt", "hello\n").await.unwrap();
        let res = edit_file(
            "a.txt".into(),
            vec![EditOp {
                old_text: "hello".into(),
                new_text: "goodbye".into(),
            }],
        )
        .await
        .unwrap();
        assert!(
            res.starts_with("Replaced 1 block(s) in ./a.txt"),
            "expected relative path in success string, got: {res}",
        );
        // Defensive: the absolute tempdir path must not appear in the
        // user-facing string.
        let tempdir_str = guard._dir.path().to_string_lossy().into_owned();
        assert!(
            !res.contains(&tempdir_str),
            "absolute tempdir leaked into success string: {res}",
        );
    }

    #[tokio::test]
    #[serial]
    async fn unmatched_old_text_emits_structured_diagnostic_with_both_views() {
        let _cwd = CwdGuard::new();
        tokio::fs::write(
            "b.txt",
            "fn main() {\n    let x = 1;\n    let y = 2;\n}\n",
        )
        .await
        .unwrap();
        // Off-by-one indentation in the query — close enough to diagnose.
        let res = edit_file(
            "b.txt".into(),
            vec![EditOp {
                old_text: "fn main() {\n   let x = 1;\n    let y = 2;\n}\n".into(),
                new_text: "noop".into(),
            }],
        )
        .await
        .unwrap();

        assert!(
            looks_like_edit_failure(&res),
            "expected structured failure, got: {res}",
        );
        let v: Value = serde_json::from_str(&res).unwrap();
        let ui = v.get("ui_view").and_then(|x| x.as_str()).unwrap();
        let model = v.get("model_view").and_then(|x| x.as_str()).unwrap();
        assert!(ui.contains("Closest match"));
        assert!(model.contains("Closest match"));
        // UI view must be short and not echo file content.
        assert!(ui.len() < 200, "ui_view too long: {ui}");
        assert!(model.len() > ui.len());
    }

    #[tokio::test]
    #[serial]
    async fn totally_unrelated_old_text_emits_no_match_fallback() {
        let _cwd = CwdGuard::new();
        tokio::fs::write("c.txt", "fn main() {}\n").await.unwrap();
        let res = edit_file(
            "c.txt".into(),
            vec![EditOp {
                old_text: "completely different unrelated content here that shares no characters with the file".into(),
                new_text: "noop".into(),
            }],
        )
        .await
        .unwrap();
        // Below the similarity threshold → falls back to the
        // "no similar region" failure, still wrapped in the structured
        // batch envelope so the UI can render it differently.
        assert!(
            looks_like_edit_failure(&res),
            "should be a structured failure, got: {res}",
        );
        let v: Value = serde_json::from_str(&res).unwrap();
        let model = v.get("model_view").and_then(|x| x.as_str()).unwrap();
        assert!(
            model.contains("no similar region")
                || model.contains("No close-enough region"),
            "expected no-match fallback message, got model_view: {model}",
        );
    }

    #[tokio::test]
    #[serial]
    async fn duplicate_match_returns_non_unique_error_with_line_numbers() {
        let _cwd = CwdGuard::new();
        // 'foo' on lines 1, 3, 5 (1-indexed). The file has a trailing
        // newline, so line N contains all chars before the Nth newline.
        tokio::fs::write("d.txt", "foo\nbar\nfoo\nbaz\nfoo\n")
            .await
            .unwrap();
        let res = edit_file(
            "d.txt".into(),
            vec![EditOp {
                old_text: "foo".into(),
                new_text: "bar".into(),
            }],
        )
        .await
        .unwrap();
        assert!(res.contains("appears 3 times"));
        assert!(
            res.contains("lines: 1, 3, 5"),
            "expected per-occurrence line numbers, got: {res}",
        );
    }

    #[tokio::test]
    #[serial]
    async fn duplicate_match_caps_listing_when_very_many() {
        let _cwd = CwdGuard::new();
        // 15 occurrences on lines 1..=15; we should list 10 and indicate
        // the remainder.
        let content: String = (1..=15)
            .map(|i| format!("x {i}\n"))
            .collect();
        tokio::fs::write("many.txt", &content).await.unwrap();
        let res = edit_file(
            "many.txt".into(),
            vec![EditOp {
                old_text: "x ".into(),
                new_text: "y ".into(),
            }],
        )
        .await
        .unwrap();
        assert!(res.contains("appears 15 times"));
        assert!(
            res.contains("(...and 5 more)"),
            "expected truncation notice, got: {res}",
        );
    }

    #[tokio::test]
    #[serial]
    async fn batch_reports_all_failures_in_one_shot() {
        let _cwd = CwdGuard::new();
        tokio::fs::write(
            "batch.txt",
            "fn alpha() {\n    let a = 1;\n}\n\
             fn beta() {\n    let b = 2;\n}\n\
             fn gamma() {\n    let c = 3;\n}\n",
        )
        .await
        .unwrap();
        // Three edits:
        //   #1: exact match succeeds.
        //   #2: oldText not in file (fuzzy candidate nearby).
        //   #3: oldText is non-unique (appears twice).
        let res = edit_file(
            "batch.txt".into(),
            vec![
                EditOp {
                    old_text: "    let a = 1;".into(),
                    new_text: "    let a = 42;".into(),
                },
                EditOp {
                    old_text: "    let nonexistent = 99;".into(),
                    new_text: "    let z = 99;".into(),
                },
                EditOp {
                    old_text: "fn".into(),
                    new_text: "function".into(),
                },
            ],
        )
        .await
        .unwrap();

        let v: Value = serde_json::from_str(&res).expect("structured failure");
        let model = v.get("model_view").and_then(|x| x.as_str()).unwrap();
        let ui = v.get("ui_view").and_then(|x| x.as_str()).unwrap();
        assert_eq!(v.get("total"), Some(&Value::from(3)));
        assert_eq!(v.get("failed"), Some(&Value::from(2)));

        // The model_view must mention BOTH failed edits, not just one.
        assert!(
            model.contains("--- edit #2 ---"),
            "expected edit #2 in batch summary, got: {model}",
        );
        assert!(
            model.contains("--- edit #3 ---"),
            "expected edit #3 in batch summary, got: {model}",
        );
        assert!(
            model.contains("No edits applied"),
            "must declare no edits applied: {model}",
        );

        // The UI view should be a count summary, not contain file
        // content (only one line range per failure, no diff excerpts).
        assert!(ui.contains("2 of 3 edit(s) had problems"));
        assert!(!ui.contains("--- edit #"));

        // The file must be unchanged.
        let after = tokio::fs::read_to_string("batch.txt").await.unwrap();
        assert!(after.contains("let a = 1;"), "edit #1 should NOT have applied");
        assert!(!after.contains("let a = 42;"));
    }

    #[tokio::test]
    #[serial]
    async fn batch_with_single_failure_passes_through_per_edit_ui_summary() {
        let _cwd = CwdGuard::new();
        tokio::fs::write("single.txt", "fn main() {\n    let x = 1;\n}\n")
            .await
            .unwrap();
        // Single edit, fuzzy candidate nearby.
        let res = edit_file(
            "single.txt".into(),
            vec![EditOp {
                old_text: "fn main() {\n   let x = 1;\n}\n".into(),
                new_text: "noop".into(),
            }],
        )
        .await
        .unwrap();
        let v: Value = serde_json::from_str(&res).unwrap();
        let ui = v.get("ui_view").and_then(|x| x.as_str()).unwrap();
        // Single-failure batch: the per-edit ui_view (which carries
        // the line-range summary) should pass through so the human
        // sees something useful.
        assert!(
            ui.contains("Closest match"),
            "expected per-edit ui summary in single-failure batch, got: {ui}",
        );
    }
}

#[cfg(test)]
mod edit_args_wire_tests {
    //! End-to-end tests for the `edit` tool's argument parsing: raw JSON wire
    //! input -> `EditArgs` -> execution, exercised through rig's erased-tool
    //! boundary (`ErasedTool::execute`) — the same dispatch path a real model
    //! tool call takes, without needing a mocked model turn.

    use super::*;
    use crate::edit_match::looks_like_edit_failure;
    use rig_core::tool::{ErasedTool, ToolContext};
    use serial_test::serial;

    /// Run the tool exactly as the runtime would: serialize the args JSON to
    /// a string and dispatch through the erased boundary.
    async fn execute_edit(args_json: serde_json::Value) -> rig_core::tool::ToolResult {
        let ctx = &mut ToolContext::new();
        EditFile
            .execute(serde_json::to_string(&args_json).unwrap(), ctx)
            .await
    }

    // ---- Canonical shape ----

    #[tokio::test]
    #[serial]
    async fn canonical_edits_array_parses_and_applies() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("canon.txt", "hello\nworld\n").await.unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "canon.txt",
            "edits": [{"oldText": "hello", "newText": "goodbye"}]
        }))
        .await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        assert!(
            result.output().render().starts_with("Replaced"),
            "unexpected output: {}",
            result.output().render()
        );
        let after = tokio::fs::read_to_string("canon.txt").await.unwrap();
        assert_eq!(after, "goodbye\nworld\n");
    }

    #[tokio::test]
    #[serial]
    async fn canonical_multi_edit_applies_all() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("multi.txt", "a\nb\nc\n").await.unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "multi.txt",
            "edits": [
                {"oldText": "a", "newText": "A"},
                {"oldText": "c", "newText": "C"}
            ]
        }))
        .await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        let after = tokio::fs::read_to_string("multi.txt").await.unwrap();
        assert_eq!(after, "A\nb\nC\n");
    }

    // ---- Legacy shape (accepted, undocumented) ----

    #[tokio::test]
    #[serial]
    async fn legacy_top_level_oldtext_newtext_is_accepted() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("legacy.txt", "keep me\nreplace me\n")
            .await
            .unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "legacy.txt",
            "oldText": "replace me",
            "newText": "replaced"
        }))
        .await;
        assert!(
            result.is_success(),
            "legacy single-edit shape must be accepted, got: {result:?}"
        );
        let after = tokio::fs::read_to_string("legacy.txt").await.unwrap();
        assert_eq!(after, "keep me\nreplaced\n");
    }

    // ---- Malformed shapes: echo + guidance, never applied ----

    #[tokio::test]
    #[serial]
    async fn malformed_args_echo_input_and_show_expected_shape() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("bad.txt", "unchanged\n").await.unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "bad.txt",
            "old": "wrong field names entirely"
        }))
        .await;
        assert!(result.is_error(), "malformed args must fail: {result:?}");
        let msg = result.output().render();
        // rig's generic parse-error prefix comes first; our informative
        // payload follows and is what matters. Canonical example first,
        // then the echo of the actual (bad) input — the input can be
        // arbitrarily long, so it goes last.
        assert!(
            msg.contains("Expected parameters like:\n{\n    \"path\": \"some/path\",\n    \"edits\": ["),
            "must lead with the canonical example, got: {msg}"
        );
        let example_end = msg.find("Invalid parameters received:").expect("echo marker");
        let echo_start = msg.find("wrong field names entirely").expect("echoed input");
        assert!(
            example_end < echo_start,
            "example must precede the echoed input, got: {msg}"
        );
        // Echo of the actual (bad) input so the model can see what it sent.
        assert!(
            msg.contains("wrong field names entirely"),
            "must echo the received args, got: {msg}"
        );
        // The expected shape is spelled out.
        assert!(
            msg.contains("\"edits\":") && msg.contains("\"oldText\":"),
            "must show the canonical example, got: {msg}"
        );
        let after = tokio::fs::read_to_string("bad.txt").await.unwrap();
        assert_eq!(after, "unchanged\n");
    }

    #[tokio::test]
    #[serial]
    async fn missing_new_text_errors_with_guidance() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        let result = execute_edit(serde_json::json!({
            "path": "whatever.txt",
            "oldText": "no newText field"
        }))
        .await;
        assert!(result.is_error(), "missing newText must fail: {result:?}");
        let msg = result.output().render();
        assert!(
            msg.contains("Invalid parameters received: "),
            "got: {msg}"
        );
        assert!(msg.contains("\"edits\":"), "got: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn non_object_args_error_informatively() {
        let result = execute_edit(serde_json::json!("just a string")).await;
        assert!(result.is_error());
        assert!(
            result
                .output()
                .render()
                .contains("Invalid parameters received:"),
        );
    }

    #[tokio::test]
    #[serial]
    async fn oversized_bad_args_are_truncated_in_error() {
        // A huge blob in the wrong field must not blow up the tool message.
        let blob = "x".repeat(5_000);
        let result = execute_edit(serde_json::json!({ "path": "p", "stuff": blob })).await;
        assert!(result.is_error());
        let msg = result.output().render();
        assert!(
            msg.len() < 1_000,
            "echo must be truncated, got {} chars",
            msg.len()
        );
        assert!(msg.contains("…"), "truncation marker missing: {msg}");
    }

    // ---- Schema advertisement ----

    #[test]
    fn schema_advertises_only_the_canonical_shape() {
        let schema = EditFile.parameters();
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array present")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        assert!(
            required.contains(&"path") && required.contains(&"edits"),
            "path and edits must be required: {required:?}"
        );
        // The legacy top-level fields must NOT be advertised.
        let props = schema.get("properties").unwrap();
        assert!(props.get("edits").is_some());
        assert!(props.get("oldText").is_none());
        assert!(props.get("newText").is_none());
    }

    #[test]
    fn edit_failure_envelope_still_recognized() {
        // The structured {ui_view, model_view} failure envelope keeps its
        // wire shape so display_tool_result routing is unaffected.
        let s = crate::edit_match::render_batch(&crate::edit_match::BatchEditFailure::build(
            1,
            vec![crate::edit_match::BatchItem {
                edit_index: 0,
                failure: crate::edit_match::EditFailure {
                    ui_view: "u".into(),
                    model_view: "m".into(),
                },
            }],
        ));
        assert!(looks_like_edit_failure(&s));
    }
}
