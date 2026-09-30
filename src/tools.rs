use rig_core::tool::ToolContext;
use rig_core::tool_macro as rig_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;

#[allow(unused_imports)]
use crate::config::{
    env_vars_to_keep, resolve_path_policy, resolve_paths, BashTimeout, ContextLines, ReadLimit,
    ReadOffset, SandboxConfig, SandboxMaxOutputBytes, SandboxMemoryMb, SandboxTimeoutSecs,
    SandboxTmpSizeMb,
};
use crate::path_util::relativize_path;

// The block-edit core moved to the `blockedit` crate so the `bsed` binary
// previews and applies in exactly the format the `edit` tool consumes. These
// re-exports keep every call site in this file spelled as before.
pub use blockedit::wire::{EditArgs, FileEdit, MAX_ECHOED_ARGS};
// The advertised blob field is spelled by the same const the parser reads, so the
// schema and `EditArgs::from_wire` cannot drift into naming two different fields.
#[allow(unused)]
use blockedit::blob::{BLOCKS_FIELD, LEGACY_BLOCKS_FIELD};
use blockedit::ApplyOutcome;
pub use blockedit::{apply_to_text, EditOp};

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
/// Returns the **host path** (after remapping) for actual filesystem operations.
fn get_path_with(path: &str, is_write: bool) -> Result<PathBuf, ToolError> {
    let base = std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?;

    // When a path policy is active (sandbox active with explicit bindings),
    // rewrite via the remap table, then enforce coverage + mode.
    if let Some(policy) = current_path_policy() {
        let host = policy.resolve(path);
        return policy
            .check(&host, is_write)
            .map(|_| host)
            .map_err(|denied| {
                // Show the caller the path *it* supplied — already an in-sandbox
                // path, so it can act on it without needing to know the remap
                // table. The host form is the operator's datum and never goes in
                // this string; see `doc/path_leak_risks.md`.
                ToolError::Generic(denied.model_message(path))
            });
    }

    validate_path(&base, path)
}

/// Resolve `path` to an absolute sandbox path **without remapping**.
/// This is used for display purposes (tool results) so the agent sees
/// sandbox-relative paths, not host paths.
fn get_sandbox_path(path: &str) -> Result<PathBuf, ToolError> {
    if let Some(policy) = current_path_policy() {
        Ok(policy.resolve_sandbox(path))
    } else {
        let base = std::env::current_dir().map_err(|e| ToolError::Io(e.to_string()))?;
        validate_path(&base, path)
    }
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
    required(path)
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

#[rig_tool(
    name = "write",
    description = "Write content to file",
    required(path, content)
)]
pub async fn write_file(path: String, content: String) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    fs::write(&p, &content).await?;
    // Return a structured result similar to read_file: include the path and
    // content preview so display_tool_result can render it with syntax
    // highlighting and the user sees what was written.
    // Use the sandbox path (not the remapped host path) for the response.
    let sandbox_path = get_sandbox_path(&path)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| sandbox_path.clone());
    let shown = crate::path_util::relativize_path(&sandbox_path, &cwd);
    // Limit preview to first 50 lines, without line numbers (display handles formatting)
    let preview = content.lines().take(50).collect::<Vec<_>>().join("\n");
    Ok(serde_json::json!({
        "path": shown,
        "content": preview,
        "lines": content.lines().count(),
        "bytes": content.len(),
    })
    .to_string())
}

/// The block syntax, shown to the model.
///
/// Defined once and interpolated into both the tool description and the
/// `blocks` parameter description. These were two byte-identical copies, which
/// is two chances to drift — and a stale copy is a bug the model feels, not one
/// a compiler catches. The backticks in [`BLOCK_POLICY`] are why this is a const
/// and not a `concat!`: the policy needs a format string, and the syntax is
/// pasted into a markdown fence where braces must survive it.
const BLOCK_SYNTAX: &str = "\
some/path:1234
@@@OLD
the original text
@@@NEW
the replacement text
@@@END";

/// The batch policy sentence that follows the syntax: atomicity, and the
/// `path:LINE` anchor the model needs for repeated text.
const BLOCK_POLICY: &str = "Each block MUST specify its file path, and SHOULD \
provide the 1-based start line number hint, from `read` or `grep`. \
Line numbers greatly reduce the chance of edit failures, \
especially when there are similar content lines in other parts of the file. Blocks \
editing the same file are executed all-or-nothing";

/// Static tool facade for `edit`. Hand-written (rather than `#[rig_tool]`) so
/// the args deserializer can select a variant from the raw JSON and accept the
/// legacy shapes without advertising them — see [`EditArgs`]. The `Bash` tool
/// follows the same manual-impl pattern.
pub struct EditFile;

impl rig_core::tool::Tool for EditFile {
    type Args = EditArgs;
    type Output = String;
    type Error = ToolError;

    const NAME: &'static str = "edit";

    fn description(&self) -> String {
        format!(
            "Replace text in a file. Prefer the `blocks` string syntax with batching support:\n```\n{BLOCK_SYNTAX}\n```\n\
             {BLOCK_POLICY}\n\
             For single edits, the traditional OpenAI path / oldText / newText API is available as well."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        // A flat object of optional scalars — deliberately no `anyOf`/nested
        // array, which weak tool-call harnesses tend to mangle (stringify).
        serde_json::json!({
            "type": "object",
            "properties": {
                (BLOCKS_FIELD): {
                    "type": "string",
                    "description": format!("Blocks format: Primary edit entry point with batching support. Block syntax:\n```\n{BLOCK_SYNTAX}\n```\n{BLOCK_POLICY}")
                },
                "path": {
                    "type": "string",
                    "description": "OpenAI format: File to edit, relative to the working directory. For `blocks` based edits, specify path:LINE in each block header instead."
                },
                "oldText": {
                    "type": "string",
                    "description": "OpenAI format: Exact text to replace (single-edit form). Must occur exactly once in the file, or pair with `startLine`/a `path:LINE` suffix."
                },
                "newText": {
                    "type": "string",
                    "description": "OpenAI format: Replacement text (single-edit form)."
                },
                "startLine": {
                    "type": "integer",
                    "description": "OpenAI format: a 1-indexed anchor line. Greatly reduces the chance of edit failures. When using `blocks`, use `path:LINE` block headers instead."
                },
            },
            "required": []
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        edit_files(args.files).await
    }
}

/// Apply a call's edits, one file at a time.
///
/// Failure granularity is the **file**: each file is applied all-or-nothing on
/// its own, and a file that fails is reported without blocking the others.
/// That is what the tool description advertises, and it is why a call about
/// three files is three independent writes rather than one giant atomic one.
///
/// A single-file call is `edit_file` verbatim, so the single-file path — still
/// the overwhelmingly common one — is unchanged by construction.
///
/// This applies each file as it resolves. The follow-up (W4 in
/// `doc/batch_editing_v2.md`) resolves *every* file in memory first, so a call
/// reports all of its problems in one round trip instead of one file per trip;
/// `apply_to_text` is pure over text, so that is a change to the write step
/// only.
pub async fn edit_files(files: Vec<FileEdit>) -> Result<String, ToolError> {
    // A single-file call is byte-identical to `edit_file` — a bare success note
    // or a bare `{ui_view, model_view}` envelope, never a one-element array, so
    // the overwhelmingly common case is untouched.
    if files.len() == 1 {
        let FileEdit { path, edits } = files.into_iter().next().unwrap();
        return edit_file(path, edits).await;
    }
    let mut results = Vec::with_capacity(files.len());
    for FileEdit { path, edits } in files {
        // A file that could not be resolved, read, or written is *that file's*
        // failure, not the call's: report it and keep going, so the model
        // learns every file's outcome in one round trip. Propagating the `Err`
        // would discard the other files' reports and reduce the whole call to
        // a bare "the tool failed".
        let raw = match edit_file(path.clone(), edits).await {
            Ok(r) => r,
            Err(e) => {
                results.push(blockedit::FileResult {
                    path: path.clone(),
                    result: serde_json::Value::String(format!("could not edit {path}: {e}")),
                });
                continue;
            }
        };
        // A success note is plain text (not JSON); a failure envelope is
        // already an object. Hold both as a `Value` so the array carries the
        // two shapes verbatim rather than re-encoding them.
        let result = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(raw),
        };
        results.push(blockedit::FileResult { path, result });
    }
    Ok(blockedit::render_file_results(&results))
}

/// Apply a batch of edits to one file, all-or-nothing.
///
/// The matching and application logic lives in the `blockedit` crate (shared
/// with the `bsed` binary); this wrapper is the agent-facing IO: resolve the
/// path against the sandbox policy, read, apply, write, and report.
pub async fn edit_file(path: String, edits: Vec<EditOp>) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    let text = fs::read_to_string(&p).await?;

    // All-or-nothing: on any failure nothing is written. Surfacing the full
    // list lets the model fix all problems in one retry instead of ping-ponging.
    let (result, matched_lines) = match apply_to_text(&text, &edits) {
        ApplyOutcome::Applied {
            text,
            matched_lines,
        } => (text, matched_lines),
        // The model is shown the whole report; the per-edit indices are for
        // callers that re-emit only the failures (`bsed apply`).
        ApplyOutcome::Failed { report, .. } => return Ok(report),
    };

    fs::write(&p, result).await?;
    // Render the path the model will see as relative to cwd when possible,
    // so the success string stays portable across checkouts (the model
    // already has cwd in its system prompt) and tokens aren't wasted on
    // the absolute prefix. We deliberately don't try to relativize paths
    // outside cwd — see `path_util::relativize_path`.
    // Use the sandbox path (not the remapped host path) for the response.
    let sandbox_path = get_sandbox_path(&path)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| sandbox_path.clone());
    let shown = relativize_path(&sandbox_path, &cwd);
    Ok(blockedit::success_note(&edits, &matched_lines, &shown))
}

/// Budget for a `sed` preview's block payload, in bytes.
///
/// The `bsed` process is run with a capture cap just above this so a truncated
/// read is detectable rather than silently acted on.
const SED_PREVIEW_BUDGET: usize = 50 * 1024;
const SED_CAPTURE_SLACK: usize = 8 * 1024;

/// The absolute path of the `bsed` binary, or `None` when it is not installed.
///
/// Resolved rather than passed by name, because the tool call goes through
/// Bubblewrap, and the namespace does not inherit `$PATH` (it starts from a
/// cleared environment) and only binds the workspace plus `/usr`, `/bin`,
/// `/lib`, `/lib64`. A `cargo install --path .` puts `bsed` in `~/.cargo/bin`,
/// which is neither on the namespace's default exec path nor bound — so by name
/// it resolves on the host and then fails to exec inside the sandbox. The
/// absolute path is also handed to the sandbox as an extra read-only bind, so the
/// binary is actually reachable there.
///
/// Probed once: what is installed is a property of the machine, not of the turn.
pub(crate) fn bsed_path() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(p) = test_bsed_path() {
        return Some(p);
    }
    static FOUND: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|dir| dir.join(BSED_BIN))
                .find(|c| c.is_file())
        })
        .clone()
}

/// Whether the `sed` tool can run: the binary exists and answers `--version`.
pub(crate) fn bsed_available() -> bool {
    #[cfg(test)]
    if let Some(p) = test_bsed_path() {
        return p.is_file();
    }
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new(BSED_BIN)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Test-only: resolve the bsed this workspace built
// ---------------------------------------------------------------------------

/// Where a test has said `bsed` should be found, if anywhere.
#[cfg(test)]
static TEST_BSED: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn test_bsed_path() -> Option<std::path::PathBuf> {
    TEST_BSED.lock().ok()?.clone()
}

/// Point `bsed_path` at the binary this workspace just built, for as long as the
/// returned guard lives.
///
/// Production resolves `bsed` from `$PATH`, and that is right — a user has it
/// wherever they installed it. A test must not, for two reasons that both bite
/// in practice:
///
/// * `$PATH` may have no `bsed` at all, and the test then quietly skips. A test
///   that skips is indistinguishable from a test that passes, so a broken tool
///   can go unnoticed for as long as nobody has the binary installed.
/// * `$PATH` may have an *older* one, and then the test exercises a parser that
///   is not this checkout's. The tool builds `bsed`'s argv, so the two have to
///   agree; when they did not, the test failed against a stale install rather
///   than telling anyone the argv was wrong.
///
/// The binary is found next to the running test executable
/// (`target/<profile>/deps/<test>` -> `target/<profile>/bsed`), which is where
/// `cargo test` puts it. It has to be built: the tests are in the same crate as
/// the `[[bin]]`, and Cargo does not build binaries for unit-test targets, so a
/// bare `cargo test` needs a `cargo build` first. The assertion below says so
/// rather than skipping.
#[cfg(test)]
pub(crate) fn use_internal_bsed() -> impl Drop {
    let exe = std::env::current_exe().expect("path of the running test");
    // .../target/<profile>/deps/<test-bin> -> .../target/<profile>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap_or_else(|| panic!("cannot locate the target profile dir from {exe:?}"));
    let bin = profile_dir.join(BSED_BIN);
    assert!(
        bin.is_file(),
        "expected the workspace `{BSED_BIN}` at {bin:?}; run `cargo build` before `cargo test`"
    );
    let previous = {
        let mut slot = TEST_BSED.lock().expect("TEST_BSED lock");
        std::mem::replace(&mut *slot, Some(bin))
    };
    TestBsedRestores { previous }
}

/// Puts the resolver back the way the test found it, so an override installed
/// for one test cannot leak into the next one.
#[cfg(test)]
struct TestBsedRestores {
    previous: Option<std::path::PathBuf>,
}

#[cfg(test)]
impl Drop for TestBsedRestores {
    fn drop(&mut self) {
        if let Ok(mut slot) = TEST_BSED.lock() {
            *slot = self.previous.take();
        }
    }
}

/// The `bsed` binary name. Kept in one place so the probe and the call agree.
const BSED_BIN: &str = "bsed";

#[rig_tool(
    name = "sed",
    description = "Preview what GNU sed expressions would change in a file, as editable blocks. \
This tool NEVER writes the file: it returns the change as @@@OLD / @@@NEW blocks, each \
anchored to a line with a `path:LINE` header. Review them, then pass them to the `edit` tool \
as its `blocks` argument to apply. One expression per line, GNU sed syntax (s///, d, a, i, c, \
y///, addresses, ranges) — `c` works over an address range, replacing it with one copy of the \
text. `w`, `e` and `q` are refused: use the `edit` or `bash` tools for those. Files with CRLF \
line endings are refused; convert them to LF first.",
    required(path, expressions)
)]
pub async fn sed_preview(
    path: String,
    expressions: String,
    context: ContextLines,
) -> Result<String, ToolError> {
    if !bsed_available() {
        return Err(ToolError::Generic(format!(
            "the `{BSED_BIN}` binary is not on PATH, so the sed tool cannot run. \
             Install it with `cargo install --path .` (it ships alongside picocode)."
        )));
    }
    // The path is bounds-checked against the sandbox policy, and the *check*
    // is on the host path — but the path handed to the child must be the
    // **in-sandbox** one. `bsed` runs inside the namespace, where a remapped
    // binding lives at its in-sandbox location, not its host location. With
    // `rw = [{from: "/home/me/scratch", to: "/tmp"}]`, the model names
    // `/tmp/f.txt`; that resolves to the host path `/home/me/scratch/f.txt`,
    // which does not exist inside the namespace (only `/tmp` is bound there).
    // Handing the host path to `bsed` therefore pointed it at a file that was
    // never there, and the tool failed on a perfectly valid path.
    //
    // This is why `sed` cannot follow `edit`'s shape: `edit` does its own IO
    // in this process, so the host path is exactly right for it and it only
    // uses the sandbox path for display. `sed` shells *out* into the
    // namespace, so the two paths have to be kept distinct — check against the
    // host one, hand over the in-sandbox one.
    //
    // The bounds check stays on the host path: under an active policy
    // `get_sandbox_path` only resolves and never enforces coverage or mode, so
    // the two calls are complementary — one authorises, the other reads.
    let _authorised = get_path_with(&path, true)?;
    let sandbox_path = get_sandbox_path(&path)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| sandbox_path.clone());
    let rel = relativize_path(&sandbox_path, &cwd);

    let cfg = sandbox_config();
    let cfg = cfg.as_ref().as_ref().cloned().unwrap_or_default();
    let timeout: Option<u64> = cfg.timeout_secs.map(|v| v.into());

    // Model-supplied text rides as ONE argv element: data, never structure. No
    // shell is involved, so a metacharacter in an expression is just a character.
    let bin = bsed_path().ok_or_else(|| {
        ToolError::Generic(format!(
            "the `{BSED_BIN}` binary is not on PATH, so the sed tool cannot run. \
             Install it with `cargo install --path .` (it ships alongside picocode)."
        ))
    })?;
    // The script leads, then the flags. `bsed` now takes options anywhere, but
    // this ordering is what every version of it accepts -- a leading flag was
    // rejected until the `preview` subcommand went away -- and it is the form
    // the usage text leads with, so the two cannot drift.
    let argv = vec![
        bin.to_string_lossy().to_string(),
        expressions,
        "-c".into(),
        context.clamped().to_string(),
        "-b".into(),
        SED_PREVIEW_BUDGET.to_string(),
        "-p".into(),
        rel,
    ];
    let (raw, code) = run_sandboxed_ro_status(
        &argv,
        &cfg,
        timeout,
        Some(SED_PREVIEW_BUDGET + SED_CAPTURE_SLACK),
        std::slice::from_ref(&bin),
    )
    .await?;

    // The capture merges the child's stderr into stdout, which is what keeps the
    // banner here. `bsed` puts the blocks on stdout and everything about them --
    // the banner, the "N of M files changed" line, the NOTEs -- on stderr, so a
    // shell user gets a clean pipeable blob while the model still reads the
    // summary and the "pass them to `edit`" instruction ahead of the blocks.
    //
    // Exit 2 is the caller's problem (a refused expression, an unappliable
    // block) and is returned as a normal result so the model can read and fix it;
    // anything else is ours.
    match code {
        Some(0) | Some(2) | None => Ok(raw),
        Some(n) => Err(ToolError::Generic(format!(
            "sed preview failed (exit {n}): {}",
            cap_lines(raw.trim(), 40)
        ))),
    }
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

#[rig_tool(
    name = "grep",
    description = "Search files for a regex pattern (ripgrep)",
    required(pattern, path)
)]
pub async fn grep_text(pattern: String, path: String) -> Result<String, ToolError> {
    // Validate the search root against the active path policy (or cwd bounds);
    // ripgrep itself runs confined inside a read-only sandbox. The policy
    // check is on the host path — see below for why rg gets the other one.
    let _authorised = get_path(&path)?;

    // Pre-relativize the search root so ripgrep's `--with-filename` output
    // comes back in the portable, token-friendly form. We deliberately do
    // NOT post-process ripgrep's output (which contains matched file
    // *content*) — that would risk rewriting paths inside the lines the
    // user actually searched for. ripgrep echoes the exact path it was
    // given, so as long as we feed it a relative form, the filename
    // column stays relative.
    //
    // `get_path` returns the **host** path, and that is the right path for
    // this process to walk — but ripgrep does not run in this process, it
    // runs *inside* the namespace (below, `run_sandboxed_ro`). Under a
    // remapping binding the two views disagree: with
    // `rw = [{from: "/home/me/scratch", to: "/tmp"}]`, the host path of
    // `/tmp/f.txt` is `/home/me/scratch/f.txt`, and that path is not bound
    // inside the namespace, so rg reports "No such file or directory" for a
    // file that plainly exists. Feed it the path as the namespace sees it.
    // See `sed_preview`, which crosses the same boundary the same way.
    //
    // The bounds check still runs on the host path: when a policy is active
    // `get_sandbox_path` only resolves and never enforces coverage or mode,
    // so the two calls are complementary — one authorises, the other reads.
    let sandbox_path = get_sandbox_path(&path)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| sandbox_path.clone());
    let rg_path = relativize_path(&sandbox_path, &cwd);

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
        let (out, exit_code) =
            run_sandboxed_with_status(&args.command, args.timeout.map(|v| v.into())).await?;
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
    // Dir visibility inside the sandbox may have changed with the
    // config — drop the completion L1 cache so the next completion
    // re-lists (and `listing_stale` reports stale to row caches).
    crate::path_complete::invalidate_listing();
}

pub(crate) fn sandbox_config() -> Arc<Option<SandboxConfig>> {
    SANDBOX.lock().expect("sandbox lock poisoned").clone()
}

/// Run `ls -la` with an ISO-8601 (seconds, local) time style inside the
/// sandbox and return the **raw lines** (metadata intact). Returns `None`
/// when no sandbox is configured, bwrap is unavailable, or `ls` fails
/// (e.g. the directory does not exist inside the sandbox).
///
/// Parsing lives in [`crate::path_complete`] — this is the raw IO edge.
pub(crate) fn sandbox_list_lines(dir: &str) -> Option<Vec<String>> {
    let cfg = sandbox_config();
    let mut cfg = (*cfg).clone()?;
    crate::sandbox_state::merge_into(&mut cfg);
    let args = build_bwrap_args(&cfg, true, &[]).ok()?;

    let output = std::process::Command::new("bwrap")
        .args(&args)
        .arg("--")
        .args(["ls", "-lap", "--time-style=+%Y-%m-%dT%H:%M:%S", dir])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.lines().map(|l| l.to_string()).collect())
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
    let configured = cfg
        .as_ref()
        .as_ref()
        .and_then(|c| c.timeout_secs.clone().map(|v| v.into()));
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
    let configured = cfg
        .as_ref()
        .as_ref()
        .and_then(|c| c.timeout_secs.clone().map(|v| v.into()));
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
    let output =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Option<i32>), ToolError> {
            let expr = build_bwrap_expr_argv(&argv, &cfg, true, &[])?;
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

/// Like [`run_sandboxed_ro`] but also returns the child's exit code, so the
/// caller can tell "the tool said no" from "the tool could not run".
///
/// `cap` overrides the sandbox's `max_output_bytes`: `capture_bounded` silently
/// truncates, and a caller that parses its output (the `sed` tool parses a
/// block blob) must treat a short read as a failure rather than act on half a
/// payload. `None` uses the configured default.
pub(crate) async fn run_sandboxed_ro_status(
    argv: &[String],
    cfg: &SandboxConfig,
    timeout: Option<u64>,
    cap: Option<usize>,
    extra_ro: &[PathBuf],
) -> Result<(String, Option<i32>), ToolError> {
    let argv = argv.to_vec();
    let cfg = cfg.clone();
    let extra_ro = extra_ro.to_vec();
    let cap = cap.unwrap_or_else(|| cfg.max_output_bytes());
    let output =
        tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, Option<i32>), ToolError> {
            let expr = build_bwrap_expr_argv(&argv, &cfg, true, &extra_ro)?;
            capture_bounded(expr, timeout, cap)
        })
        .await
        .map_err(|e| ToolError::Generic(e.to_string()))??;
    let (output, exit) = output;
    Ok((String::from_utf8_lossy(&output).to_string(), exit))
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
        let note =
            format!("\n\n[output truncated: {dropped} bytes dropped, cap is {max_bytes} bytes]");
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
        return vec![
            "sh".into(),
            "-c".into(),
            format!("ulimit -v {kb}; {payload}"),
        ];
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
    extra_ro: &[PathBuf],
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

    // Extra read-only binds for programs we invoke by absolute path that live
    // outside the workspace and the system directories — today just the `bsed`
    // binary, which `cargo install` puts in `~/.cargo/bin`. Without this the
    // namespace has no path to it at all: the environment starts cleared, so
    // there is no `$PATH` to resolve the name by, and the bind is what makes the
    // file itself exist inside. Read-only regardless of `read_only`: none of
    // these is something a tool call should be able to rewrite.
    for f in extra_ro {
        args.push("--ro-bind".into());
        args.push(f.to_string_lossy().to_string());
        args.push(f.to_string_lossy().to_string());
    }

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
    let etc_sub = binds.iter().any(|b| b.to.starts_with("/etc/"));
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
    let mut args = build_bwrap_args(cfg, read_only, &[])?;

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
    extra_ro: &[PathBuf],
) -> Result<duct::Expression, ToolError> {
    let mut args = build_bwrap_args(cfg, read_only, extra_ro)?;
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

#[rig_tool(
    name = "ls",
    description = "List files and directories in a path",
    required(path)
)]
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
    use serial_test::serial;
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
        let res = glob_files("*.txt".to_string(), search_root).await.unwrap();

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

    pub(crate) fn bwrap_on_path() -> bool {
        std::process::Command::new("which")
            .arg("bwrap")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub(crate) fn sandbox_with(
        ro: Vec<BindPath>,
        rw: Vec<BindPath>,
        timeout_secs: Option<u64>,
    ) -> SandboxConfig {
        SandboxConfig {
            timeout_secs: timeout_secs
                .map(|v| crate::config::SandboxTimeoutSecs(Some(v)))
                .unwrap_or(crate::config::SandboxTimeoutSecs(None)),
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
        let out = run_sandboxed("echo x > /tmp/probe && cat /tmp/probe", None)
            .await
            .unwrap();
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
        set_sandbox(Some(sandbox_with(
            vec![],
            vec![BindPath::Simple("/dev".into())],
            Some(30),
        )));
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
        set_sandbox(Some(sandbox_with(
            vec![],
            vec![BindPath::Simple("/proc".into())],
            Some(30),
        )));
        let out = run_sandboxed(
            "test -f /proc/self/status && echo PROC_OK || echo PROC_FAIL",
            None,
        )
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
        assert!(res
            .unwrap_err()
            .to_string()
            .contains("not within a permitted binding"));
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
        run_sandboxed("echo from-shell > /tmp/x2.txt", None)
            .await
            .unwrap();

        // Both landed on the host temp dir.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.txt"))
                .unwrap()
                .trim(),
            "from-tool"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x2.txt"))
                .unwrap()
                .trim(),
            "from-shell"
        );
        // And the non-shell tool can read what the shell wrote.
        let read = super::read_file(
            "/tmp/x2.txt".to_string(),
            crate::config::ReadOffset(0),
            crate::config::ReadLimit(0),
        )
        .await
        .unwrap();
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
        assert!(
            out.contains("WRITE_DENIED"),
            "nested ro child should deny write: {out}"
        );
        assert!(
            out.contains("PARENT_OK"),
            "rw parent should allow write: {out}"
        );
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
        let out = run_sandboxed("echo \"K=[$PICOCODE_TEST_KEY] PATH_OK=$PATH\"", None)
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
        let out = run_sandboxed("echo \"K=[$PICOCODE_TEST_KEY2] PATH_OK=$PATH\"", None)
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
            res.unwrap_err()
                .to_string()
                .contains("sandbox not configured"),
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
        let out = grep_text(
            "fn main".to_string(),
            absolute.to_string_lossy().into_owned(),
        )
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

    use super::sandbox_tests::{bwrap_on_path, sandbox_with};
    use super::*;
    use crate::config::BindPath;
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

    /// The sandbox mounts a scratch tmpfs over `/tmp`, which shadows the
    /// read-only bind of the working directory when the two overlap — so a test
    /// whose cwd is a `/tmp` tempdir cannot see its own files inside the
    /// namespace. Point `TMPDIR` inside the repo for these tests, which is
    /// bound. (A real project under `/tmp` hits the same shadowing with `rg` and
    /// `bash`; it is a property of the sandbox, not of the `sed` tool.)
    fn tmpdir_inside_repo() -> TmpdirRedirect {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/t");
        std::fs::create_dir_all(&dir).unwrap();
        TmpdirRedirect::set(&dir)
    }

    /// Restores `TMPDIR` on drop, panic or not.
    ///
    /// `TMPDIR` is process-global, so leaving it redirected leaks into every test
    /// that runs afterwards: they all get their tempdirs from the repo, which
    /// makes their paths long enough to be ellipsised when rendered — enough to
    /// break an unrelated assertion elsewhere in the suite.
    struct TmpdirRedirect(Option<std::ffi::OsString>);

    impl TmpdirRedirect {
        fn set(dir: &std::path::Path) -> Self {
            let previous = std::env::var_os("TMPDIR");
            std::env::set_var("TMPDIR", dir);
            Self(previous)
        }
    }

    impl Drop for TmpdirRedirect {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("TMPDIR", v),
                None => std::env::remove_var("TMPDIR"),
            }
        }
    }

    /// The regression that matters for anyone who installed with
    /// `cargo install --path .`: `bsed` lands in `~/.cargo/bin`, which is
    /// neither on the namespace's default exec path (the environment starts
    /// cleared, so there is no `$PATH`) nor bound. Resolving the absolute path is
    /// only half the fix — the file has to be bound in, or `execvp` still fails.
    #[test]
    fn bsed_is_resolved_and_bound_into_the_namespace() {
        // The workspace binary, for the same reason the `sed` tool tests use it:
        // a missing install would skip this, and an older one would test a
        // resolver this checkout does not have.
        let _bsed = super::use_internal_bsed();
        let Some(bin) = super::bsed_path() else {
            panic!("bsed did not resolve, but the workspace binary exists");
        };
        assert!(
            bin.is_absolute(),
            "bsed must be resolved to an absolute path"
        );
        // It is a real file: the sandbox binds this path, and a bind of a
        // missing file is how the tool would fail at run time rather than here.
        assert!(
            bin.is_file(),
            "bsed must resolve to a file: {}",
            bin.display()
        );

        let cfg = crate::config::SandboxConfig::default().with_defaults();
        let want = bin.to_string_lossy().to_string();
        let bound = |args: &[String], flag: &str| {
            args.windows(3)
                .filter(|w| w[0] == flag)
                .any(|w| w[1] == want && w[2] == want)
        };
        let args = build_bwrap_args(&cfg, true, std::slice::from_ref(&bin)).expect("args");
        assert!(bound(&args, "--ro-bind"), "bsed must be bound: {args:?}");
        // Read-only even in a writable namespace: a tool call has no business
        // rewriting the binary it is running.
        let rw = build_bwrap_args(&cfg, false, std::slice::from_ref(&bin)).expect("args");
        assert!(!bound(&rw, "--bind"), "bsed must never be writable: {rw:?}");
    }

    #[tokio::test]
    #[serial]
    async fn sed_preview_is_read_only_and_anchors_every_block() {
        let _tmpdir = tmpdir_inside_repo();
        // End-to-end through the real tool: path policy -> argv -> `bsed`. The
        // sandbox is not configured here (CwdGuard clears it), which exercises
        // the same code path minus the namespace, so the test runs anywhere.
        let _cwd = CwdGuard::new();
        tokio::fs::write(
            "a.txt",
            "fn main() {\n    let x = 1;\n    let y = 2;\n    run();\n}\n",
        )
        .await
        .unwrap();

        // This workspace's own `bsed`, not whatever `$PATH` has: the tool's
        // argv and the binary's parser have to be the same version, and a
        // missing install must fail the test rather than skip it.
        let _bsed = super::use_internal_bsed();
        assert!(
            super::bsed_available(),
            "the workspace bsed should be available once the override is installed"
        );

        let out = sed_preview("a.txt".into(), "4s/run/walk/".into(), ContextLines(0))
            .await
            .expect("preview");

        // The file is untouched: this is the whole point of the tool.
        assert_eq!(
            tokio::fs::read_to_string("a.txt").await.unwrap(),
            "fn main() {\n    let x = 1;\n    let y = 2;\n    run();\n}\n"
        );
        assert!(out.contains("@@@OLD"), "{out}");
        assert!(out.contains("walk();"), "{out}");

        // And the preview applies: the anchor resolves against the unchanged
        // file, which is the claim the whole design rests on.
        let parsed = crate::edit_blocks::parse_blocks(&out).expect("preview parses");
        let edits: Vec<EditOp> = parsed
            .edits
            .iter()
            .map(|e| EditOp {
                old_text: e.old_text.clone(),
                new_text: e.new_text.clone(),
                start_line: e.start_line.map(crate::config::EditStartLine),
            })
            .collect();
        assert_eq!(parsed.edits[0].start_line, Some(4));
        let applied = edit_file("a.txt".into(), edits).await.unwrap();
        assert!(applied.starts_with("Replaced 1 block(s)"), "{applied}");
        assert!(tokio::fs::read_to_string("a.txt")
            .await
            .unwrap()
            .contains("walk();"));
    }

    /// A policy denial must report the path the model supplied, never the host
    /// path it resolves to. Under a remapping binding the two differ, and the
    /// model can only act on the former — if it is handed the host path it has
    /// no way to name it back. Pins the *wiring* in `get_path_with`, not just
    /// the rendering on `PolicyDenied`. See `doc/path_leak_risks.md`.
    #[tokio::test]
    #[serial]
    async fn a_denied_path_reports_the_supplied_path_not_the_host_one() {
        let _tmpdir = tmpdir_inside_repo();
        let _cwd = CwdGuard::new();

        // Read-only binding, and we attempt a write -> denied with a remap in
        // play, so the two path forms are genuinely different strings.
        let host = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/deny-scratch");
        set_sandbox(Some(sandbox_with(
            vec![BindPath::Remap {
                from: host.to_string_lossy().to_string(),
                to: "/scratch".to_string(),
            }],
            vec![],
            Some(30),
        )));

        let err = write_file("/scratch/f.txt".to_string(), "x".to_string())
            .await
            .expect_err("writing under an ro binding must be denied")
            .to_string();

        assert!(
            err.contains("/scratch/f.txt"),
            "message should echo the supplied path: {err}"
        );
        assert!(
            !err.contains("deny-scratch"),
            "message leaked the host path: {err}"
        );
        assert!(err.contains("read-only"), "reason lost: {err}");
    }

    /// `grep` crosses into the namespace like `sed` does, so it needs the
    /// in-sandbox path too. Reproduced on a real config:
    /// `rg: /home/me/tmp/agents/f.txt: No such file or directory` for a file
    /// that plainly exists at the path the model named (`/tmp/f.txt`).
    #[tokio::test]
    #[serial]
    async fn grep_works_on_a_path_that_is_only_visible_in_sandbox() {
        let _tmpdir = tmpdir_inside_repo();
        if !bwrap_on_path() {
            return;
        }
        let _cwd = CwdGuard::new();

        let host = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/grep-scratch");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("f.txt"), "alpha needle beta\n").unwrap();

        set_sandbox(Some(sandbox_with(
            vec![],
            vec![BindPath::Remap {
                from: host.to_string_lossy().to_string(),
                to: "/scratch".to_string(),
            }],
            Some(30),
        )));

        let out = grep_text("needle".into(), "/scratch/f.txt".into())
            .await
            .expect("grep on a remapped path");
        assert!(out.contains("needle"), "{out}");
        assert!(
            !out.contains("No such file"),
            "ripgrep was handed a host path: {out}"
        );
    }

    /// A path the model can see *only* at its in-sandbox location must work.
    ///
    /// Regression: `sed` resolved the path to its **host** location and handed
    /// that to `bsed`, which runs inside the namespace. With a remapping
    /// binding (`from: <host dir>, to: /tmp`) the host path is not bound inside
    /// the sandbox at all, so a perfectly valid `/tmp/f.txt` failed as if the
    /// file did not exist. The check is against the host path; the path handed
    /// over must be the in-sandbox one.
    #[tokio::test]
    #[serial]
    async fn sed_preview_works_on_a_path_that_is_only_visible_in_sandbox() {
        let _tmpdir = tmpdir_inside_repo();
        if !bwrap_on_path() {
            return;
        }
        let _cwd = CwdGuard::new();

        // A host directory that is NOT itself bound; only its *sandbox*
        // location (/scratch) is.
        let host = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sed-scratch");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("f.txt"), "alpha\nbeta\n").unwrap();

        set_sandbox(Some(sandbox_with(
            vec![],
            vec![BindPath::Remap {
                from: host.to_string_lossy().to_string(),
                to: "/scratch".to_string(),
            }],
            Some(30),
        )));

        // The model names the sandbox path — the only one it can see.
        let out = sed_preview(
            "/scratch/f.txt".into(),
            "s/alpha/ALPHA/".into(),
            ContextLines(0),
        )
        .await
        .expect("preview on a remapped path");
        assert!(out.contains("ALPHA"), "{out}");
        // The file is untouched, and still at its host location.
        assert_eq!(
            std::fs::read_to_string(host.join("f.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn sed_preview_reports_a_rejected_expression_without_failing() {
        let _tmpdir = tmpdir_inside_repo();
        // A refused expression is the model's to fix, so it comes back as a
        // normal result (exit 2), not as a tool error.
        let _cwd = CwdGuard::new();
        tokio::fs::write("a.txt", "x\n").await.unwrap();
        let _bsed = super::use_internal_bsed();
        let out = sed_preview("a.txt".into(), "1w /tmp/pwned".into(), ContextLines(0))
            .await
            .expect("preview returns the message");
        assert!(out.contains("never writes"), "{out}");
        assert!(!std::path::Path::new("/tmp/pwned").exists());
    }

    #[tokio::test]
    #[serial]
    async fn sed_preview_rejects_a_path_outside_cwd() {
        // Same policy as `edit`: the sandbox bounds are enforced here, before
        // the child ever sees the path.
        let _cwd = CwdGuard::new();
        let err = sed_preview("/etc/hostname".into(), "s/a/b/".into(), ContextLines(0))
            .await
            .expect_err("outside cwd");
        assert!(err.to_string().to_lowercase().contains("path"), "{err}");
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
                start_line: None,
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
                start_line: None,
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
        tokio::fs::write("b.txt", "fn main() {\n    let x = 1;\n    let y = 2;\n}\n")
            .await
            .unwrap();
        // Off-by-one indentation in the query — close enough to diagnose.
        let res = edit_file(
            "b.txt".into(),
            vec![EditOp {
                old_text: "fn main() {\n   let x = 1;\n    let y = 2;\n}\n".into(),
                new_text: "noop".into(),
                start_line: None,
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
                start_line: None,
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
            model.contains("no similar region") || model.contains("No close-enough region"),
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
                start_line: None,
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
        let content: String = (1..=15).map(|i| format!("x {i}\n")).collect();
        tokio::fs::write("many.txt", &content).await.unwrap();
        let res = edit_file(
            "many.txt".into(),
            vec![EditOp {
                old_text: "x ".into(),
                new_text: "y ".into(),
                start_line: None,
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
                    start_line: None,
                },
                EditOp {
                    old_text: "    let nonexistent = 99;".into(),
                    new_text: "    let z = 99;".into(),
                    start_line: None,
                },
                EditOp {
                    old_text: "fn".into(),
                    new_text: "function".into(),
                    start_line: None,
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
        assert!(
            after.contains("let a = 1;"),
            "edit #1 should NOT have applied"
        );
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
                start_line: None,
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

    // ---- Batch shape ----

    #[tokio::test]
    #[serial]
    async fn an_edits_array_is_rejected_and_writes_nothing() {
        // `edits: [...]` is pi's storage form, not a wire one. The point of
        // refusing it is that a harness which stringifies the array cannot get
        // far enough to half-apply a batch, so the file must be untouched.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("canon.txt", "hello\nworld\n")
            .await
            .unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "canon.txt",
            "edits": [{"oldText": "hello", "newText": "goodbye"}]
        }))
        .await;
        assert!(result.is_error(), "array must not dispatch: {result:?}");
        let text = result.output().render();
        assert!(text.contains("`blocks`"), "must name the field: {text}");
        let after = tokio::fs::read_to_string("canon.txt").await.unwrap();
        assert_eq!(after, "hello\nworld\n", "the file must be untouched");
    }

    // ---- Single shape (accepted, undocumented) ----

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
        // What the *model* sees: the redundant wrappers stripped, leaving the
        // problem, the line-based `Expected:` block, and the echo. (The raw
        // result still carries rig's wrapper; the agent hook removes it.)
        let raw = result.output().render();
        let msg = blockedit::strip_arg_error_prefixes(&raw);
        assert!(
            msg.contains("Expected:"),
            "must show expected shape, got: {msg}"
        );
        assert!(
            msg.contains("@@@OLD"),
            "must show the block example, got: {msg}"
        );
        assert!(!msg.contains("Expected one of these"), "terse, got: {msg}");
        // Echo of the actual (bad) input so the model can see what it sent.
        assert!(
            msg.contains("wrong field names entirely"),
            "must echo the received args, got: {msg}"
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
        let raw = result.output().render();
        let msg = blockedit::strip_arg_error_prefixes(&raw);
        assert!(msg.contains("Expected:"), "got: {msg}");
        assert!(msg.contains("@@@OLD"), "got: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn non_object_args_error_informatively() {
        let result = execute_edit(serde_json::json!("just a string")).await;
        assert!(result.is_error());
        let raw = result.output().render();
        let msg = blockedit::strip_arg_error_prefixes(&raw);
        assert!(msg.contains("must be a JSON object"), "got: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn oversized_bad_args_are_truncated_in_error() {
        // A huge blob in the wrong field must not blow up the tool message.
        let blob = "x".repeat(5_000);
        let result = execute_edit(serde_json::json!({ "path": "p", "stuff": blob })).await;
        assert!(result.is_error());
        let msg = result.output().render();
        // The template is fixed-size; the point is that the 5_000-char blob
        // is not echoed raw (it is truncated to MAX_ECHOED_ARGS + `…`).
        assert!(
            msg.len() < 1_200,
            "echo must be truncated, got {} chars",
            msg.len()
        );
        assert!(msg.contains("…"), "truncation marker missing: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn block_form_top_level_startline_is_ignored() {
        // Shape (B): in the blob form anchors come *only* from `path:LINE`
        // headers. A stray top-level `startLine` must not silently aim a
        // header-less block, so it is ignored — the ambiguous block still
        // fails loudly rather than editing a guessed occurrence.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("win.txt", "x\nkeep\nx\n").await.unwrap();
        let blob = "win.txt\n@@@OLD\nx\n@@@NEW\ny\n@@@END";

        let result = execute_edit(serde_json::json!({
            "path": "win.txt", "blocks": blob, "startLine": 3
        }))
        .await;
        assert!(
            result.output().render().contains("appears 2 times"),
            "top-level startLine must be ignored in blob form; got: {}",
            result.output().render()
        );
        // Nothing applied.
        assert_eq!(
            tokio::fs::read_to_string("win.txt").await.unwrap(),
            "x\nkeep\nx\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn block_form_header_anchor_disambiguates() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("win2.txt", "x\nkeep\nx\n").await.unwrap();
        // The header anchors the block to line 3 (the second `x`).
        let blob = "win2.txt:3\n@@@OLD\nx\n@@@NEW\ny\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "win2.txt", "blocks": blob})).await;
        assert!(result.is_success(), "got: {result:?}");
        assert_eq!(
            tokio::fs::read_to_string("win2.txt").await.unwrap(),
            "x\nkeep\ny\n"
        );
    }

    // ---- `path:LINE` on the JSON `path` argument ----

    #[tokio::test]
    #[serial]
    async fn path_argument_line_suffix_anchors_single_edit() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("pa.txt", "x\nkeep\nx\n").await.unwrap();
        // `path` carries the anchor, so it also proves the suffix is stripped:
        // the file resolves to `pa.txt`, not a literal `pa.txt:3`.
        let result = execute_edit(serde_json::json!({
            "path": "pa.txt:3", "oldText": "x", "newText": "y"
        }))
        .await;
        assert!(result.is_success(), "got: {result:?}");
        assert_eq!(
            tokio::fs::read_to_string("pa.txt").await.unwrap(),
            "x\nkeep\ny\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn path_argument_line_and_startline_must_agree() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        let result = execute_edit(serde_json::json!({
            "path": "pf.txt:3", "oldText": "x", "newText": "y", "startLine": 5
        }))
        .await;
        assert!(result.is_error(), "conflict must fail: {result:?}");
        assert!(
            result.output().render().contains("must agree"),
            "got: {}",
            result.output().render()
        );
    }

    #[tokio::test]
    #[serial]
    async fn blob_form_path_line_suffix_is_stripped_but_not_an_anchor() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("pb.txt", "x\nkeep\nx\n").await.unwrap();
        // In blob form the `path` suffix is only stripped (file resolves), NOT
        // used as an anchor — repeated text stays ambiguous and fails loudly
        // rather than silently aiming an unrelated block.
        let result = execute_edit(serde_json::json!({
            "path": "pb.txt:3",
            "blocks": "pb.txt\n@@@OLD\nx\n@@@NEW\ny\n@@@END"
        }))
        .await;
        assert!(
            result.output().render().contains("appears 2 times"),
            "got: {}",
            result.output().render()
        );
        assert_eq!(
            tokio::fs::read_to_string("pb.txt").await.unwrap(),
            "x\nkeep\nx\n",
        );
    }

    // ---- Success-string match location (imitation hint) ----

    #[tokio::test]
    #[serial]
    async fn success_string_reports_match_location_when_unanchored() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("m.txt", "a\nb\nc\n").await.unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "m.txt", "blocks": "m.txt\n@@@OLD\nb\n@@@NEW\nB\n@@@END"
        }))
        .await;
        assert!(result.is_success(), "got: {result:?}");
        // The match sits on line 2; the result teaches the `path:LINE` idiom.
        assert!(
            result.output().render().contains("(matched at ./m.txt:2)"),
            "got: {}",
            result.output().render()
        );
    }

    #[tokio::test]
    #[serial]
    async fn success_string_omits_location_when_anchored() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("ma.txt", "x\nkeep\nx\n").await.unwrap();
        let result = execute_edit(serde_json::json!({
            "path": "ma.txt", "blocks": "ma.txt:3\n@@@OLD\nx\n@@@NEW\ny\n@@@END"
        }))
        .await;
        assert!(result.is_success(), "got: {result:?}");
        // The model already anchored — no teaching hint needed.
        assert!(
            !result.output().render().contains("matched"),
            "anchored call must not add a location hint: {}",
            result.output().render()
        );
    }

    // ---- Per-block line anchors (`path:LINE` headers) ----

    #[tokio::test]
    #[serial]
    async fn header_anchor_selects_nearest_duplicate() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        let before = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n\nfn b() {\n    let x = 1;\n    let y = 2;\n}\n";
        tokio::fs::write("anchor.txt", before).await.unwrap();
        // Two identical 2-line bodies (lines 2 and 7). The header anchors to
        // line 7, so the *second* occurrence must be replaced.
        let blob = "anchor.txt:7\n@@@OLD\n    let x = 1;\n    let y = 2;\n@@@NEW\n    let x = 9;\n    let y = 9;\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "anchor.txt", "blocks": blob})).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        let after = tokio::fs::read_to_string("anchor.txt").await.unwrap();
        assert_eq!(
            after,
            "fn a() {\n    let x = 1;\n    let y = 2;\n}\n\nfn b() {\n    let x = 9;\n    let y = 9;\n}\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn header_anchor_within_tolerance_applies() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("tol.txt", "alpha\nbeta\ngamma\nzzz\nalpha\nbeta\ngamma\n")
            .await
            .unwrap();
        // 3-line context → tolerance 1. Occurrences start at lines 1 and 5;
        // anchoring to line 6 lands within 1 of the second.
        let blob = "tol.txt:6\n@@@OLD\nalpha\nbeta\ngamma\n@@@NEW\nA\nB\nC\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "tol.txt", "blocks": blob})).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        assert_eq!(
            tokio::fs::read_to_string("tol.txt").await.unwrap(),
            "alpha\nbeta\ngamma\nzzz\nA\nB\nC\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn header_anchor_out_of_tolerance_errors() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("far.txt", "alpha\nbeta\ngamma\nzzz\nalpha\nbeta\ngamma\n")
            .await
            .unwrap();
        // Anchor line 3 is 2 lines from *both* occurrences (lines 1 and 5):
        // outside the ±1 tolerance and not decisively nearer (d1 == d2, a tie).
        let blob = "far.txt:3\n@@@OLD\nalpha\nbeta\ngamma\n@@@NEW\nA\nB\nC\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "far.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(
            out.contains("not decisive"),
            "expected out-of-tolerance failure, got: {out}",
        );
        // The message must show both distances so the model can act on it.
        assert!(out.contains("nearest at line"), "got: {out}");
        // Nothing was applied.
        assert_eq!(
            tokio::fs::read_to_string("far.txt").await.unwrap(),
            "alpha\nbeta\ngamma\nzzz\nalpha\nbeta\ngamma\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn header_anchor_ignored_when_match_is_unique() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("uniq.txt", "only\nhere\n").await.unwrap();
        // A wildly-off anchor is harmless when oldText occurs exactly once.
        let blob = "uniq.txt:100\n@@@OLD\nhere\n@@@NEW\nthere\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "uniq.txt", "blocks": blob})).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        assert_eq!(
            tokio::fs::read_to_string("uniq.txt").await.unwrap(),
            "only\nthere\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn header_anchor_tie_is_ambiguous() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        // Two occurrences on the same line, equidistant from the anchor.
        tokio::fs::write("amb.txt", "x x\n").await.unwrap();
        let blob = "amb.txt:1\n@@@OLD\nx\n@@@NEW\ny\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "amb.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(
            out.contains("ambiguous"),
            "expected ambiguity failure, got: {out}"
        );
        assert_eq!(tokio::fs::read_to_string("amb.txt").await.unwrap(), "x x\n");
    }

    // ---- Skew rule (ratio-based anchor acceptance) ----

    /// 52-line file with `alpha` on lines 1 and 50.
    async fn write_alpha_lines_1_50() {
        let mut lines: Vec<String> = (1..=52).map(|i| format!("line {i}")).collect();
        lines[0] = "alpha".into(); // line 1
        lines[49] = "alpha".into(); // line 50
        tokio::fs::write("skew.txt", format!("{}\n", lines.join("\n")))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn anchor_skew_accepts_far_but_decisive() {
        // Stale anchor: nearest is 2 lines away (tolerance is 0 for a
        // single-line oldText) — the old rule rejected this, the skew rule
        // accepts because the nearest is far decisively nearer than the
        // second (2 vs 51 lines).
        let _cwd = super::edit_file_tests::CwdGuard::new();
        write_alpha_lines_1_50().await;
        let blob = "skew.txt:52\n@@@OLD\nalpha\n@@@NEW\nhit\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "skew.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(
            out.starts_with("Replaced"),
            "expected skew acceptance, got: {out}"
        );
        let after = tokio::fs::read_to_string("skew.txt").await.unwrap();
        let lines: Vec<&str> = after.lines().collect();
        assert_eq!(lines[49], "hit", "line 50 should be replaced");
        assert_eq!(lines[0], "alpha", "line 1 must stay untouched");
    }

    #[tokio::test]
    #[serial]
    async fn anchor_skew_rejects_near_tie_beyond_tolerance() {
        // Occurrences on lines 40/45, anchor 43: d1=2, d2=3 — beyond the
        // zero tolerance and NOT decisively skewed → reject, and the message
        // must show both distances.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        let mut lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        lines[39] = "alpha".into(); // line 40
        lines[44] = "alpha".into(); // line 45
        tokio::fs::write("tie.txt", format!("{}\n", lines.join("\n")))
            .await
            .unwrap();
        let blob = "tie.txt:43\n@@@OLD\nalpha\n@@@NEW\nhit\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "tie.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(
            looks_like_edit_failure(&out),
            "near-tie must be rejected, got: {out}"
        );
        assert!(
            out.contains("next-nearest at line 40"),
            "d2 must be reported: {out}"
        );
        assert_eq!(
            tokio::fs::read_to_string("tie.txt")
                .await
                .unwrap()
                .lines()
                .nth(39)
                .unwrap(),
            "alpha",
            "nothing applied",
        );
    }

    #[tokio::test]
    #[serial]
    async fn anchor_skew_respects_absolute_cap() {
        // d1 = 65 lines (> cap 60) with a decisively skewed second candidate
        // (d2 = 199) — still rejected: the cap bounds stale-anchor damage.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        let mut lines: Vec<String> = (1..=265).map(|i| format!("line {i}")).collect();
        lines[0] = "alpha".into(); // line 1
        lines[264] = "alpha".into(); // line 265
        tokio::fs::write("cap.txt", format!("{}\n", lines.join("\n")))
            .await
            .unwrap();
        let blob = "cap.txt:66\n@@@OLD\nalpha\n@@@NEW\nhit\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "cap.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(looks_like_edit_failure(&out), "cap must reject, got: {out}");
        assert!(out.contains("not decisive"), "got: {out}");
        assert_eq!(
            tokio::fs::read_to_string("cap.txt")
                .await
                .unwrap()
                .lines()
                .next()
                .unwrap(),
            "alpha",
            "nothing applied",
        );
    }

    // ---- Claim-based two-pass resolution ----

    #[tokio::test]
    #[serial]
    async fn duplicate_unique_text_across_blocks_fails_loudly() {
        // Regression probe: two blocks sharing one *unique* oldText used to
        // both resolve to the same bytes and corrupt the file. Now the second
        // block reports that its only candidate is already claimed.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("dup.txt", "hello world\n").await.unwrap();
        let blob =
            "dup.txt\n@@@OLD\nhello\n@@@NEW\nbye\n@@@END\n\ndup.txt\n@@@OLD\nhello\n@@@NEW\nhi\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "dup.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(looks_like_edit_failure(&out), "must fail, got: {out}");
        assert!(
            out.contains("already matched by edit(s) #1"),
            "expected all-candidates-claimed diagnostic, got: {out}",
        );
        assert_eq!(
            tokio::fs::read_to_string("dup.txt").await.unwrap(),
            "hello world\n",
            "file must be untouched",
        );
    }

    #[tokio::test]
    #[serial]
    async fn anchored_block_resolves_later_unanchored_duplicate() {
        // The motivating scenario: the model anchors the SECOND block and
        // leaves the first un-anchored, expecting elimination. Pass 1 claims
        // line 3; pass 2's block 1 sees only line 1 free.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("mot.txt", "foo\nbar\nfoo\n")
            .await
            .unwrap();
        let blob = "mot.txt\n@@@OLD\nfoo\n@@@NEW\nAAA\n@@@END\n\nmot.txt:3\n@@@OLD\nfoo\n@@@NEW\nBBB\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "mot.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(out.starts_with("Replaced"), "expected success, got: {out}");
        assert_eq!(
            tokio::fs::read_to_string("mot.txt").await.unwrap(),
            "AAA\nbar\nBBB\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn pass2_claims_narrow_later_pass2_blocks() {
        // Transitive narrowing among content-only blocks: block 1's unique
        // match claims a region containing one of block 2's candidates,
        // leaving block 2 exactly one free occurrence.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("chain.txt", "foo bar\nbaz\nbar\n")
            .await
            .unwrap();
        let blob =
            "chain.txt\n@@@OLD\nfoo bar\n@@@NEW\nAAA\n@@@END\n\nchain.txt\n@@@OLD\nbar\n@@@NEW\nBBB\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "chain.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(out.starts_with("Replaced"), "expected success, got: {out}");
        assert_eq!(
            tokio::fs::read_to_string("chain.txt").await.unwrap(),
            "AAA\nbaz\nBBB\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn overlapping_anchored_matches_conflict() {
        // Two anchored blocks whose chosen ranges intersect: the later one is
        // rejected (block order), naming both edits and their line ranges.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("ov.txt", "hello world\n").await.unwrap();
        let blob = "ov.txt:1\n@@@OLD\nhello\n@@@NEW\nA\n@@@END\n\nov.txt:1\n@@@OLD\nhello world\n@@@NEW\nB\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "ov.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(looks_like_edit_failure(&out), "must conflict, got: {out}");
        assert!(
            out.contains("overlaps edit #1"),
            "expected overlap conflict naming edit #1, got: {out}",
        );
        assert_eq!(
            tokio::fs::read_to_string("ov.txt").await.unwrap(),
            "hello world\n",
        );
    }

    #[tokio::test]
    #[serial]
    async fn failed_block_claims_nothing_diagnostics_complete() {
        // Pass-1 failure (anchor rejected) must NOT claim its region: the
        // later un-anchored block still sees both occurrences free and reports
        // plain ambiguity. Both diagnostics arrive in one response, in block
        // order.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("noclaim.txt", "dup\na\nb\nc\ndup\n")
            .await
            .unwrap();
        let blob =
            "noclaim.txt:3\n@@@OLD\ndup\n@@@NEW\nX\n@@@END\n\nnoclaim.txt\n@@@OLD\ndup\n@@@NEW\nY\n@@@END";
        let result = execute_edit(serde_json::json!({"path": "noclaim.txt", "blocks": blob})).await;
        let out = result.output().render();
        assert!(looks_like_edit_failure(&out), "batch must fail, got: {out}");
        // Pass-1 failure: anchor not decisive (d1=2 at line 1, d2=3 at line 5... anchor 3 → |3-1|=2, |3-5|=2 tie).
        assert!(out.contains("edit #1"), "pass-1 diagnostic missing: {out}");
        // Pass-2: no claims exist → plain ambiguity, explicitly NOT claimed.
        assert!(
            out.contains("appears 2 times"),
            "pass-2 diagnostic missing: {out}"
        );
        assert!(
            !out.contains("already matched"),
            "failed block must not have claimed: {out}",
        );
        // Block order in the aggregated diagnostics.
        let e1 = out.find("edit #1").expect("edit #1 present");
        let e2 = out.find("edit #2").expect("edit #2 present");
        assert!(e1 < e2, "diagnostics must be in block order: {out}");
        assert_eq!(
            tokio::fs::read_to_string("noclaim.txt").await.unwrap(),
            "dup\na\nb\nc\ndup\n",
        );
    }

    // ---- Schema advertisement ----

    #[test]
    fn schema_is_flat_with_no_anyof() {
        // The whole point of v2: a flat bag of optional scalars, with NO
        // `anyOf` union or nested array that weak harnesses stringify.
        let schema = EditFile.parameters();
        assert!(
            schema.get("anyOf").is_none(),
            "schema must not use anyOf: {schema}"
        );
        assert!(
            schema.get("oneOf").is_none(),
            "schema must not use oneOf: {schema}"
        );

        let props = schema.get("properties").expect("properties present");
        for key in ["path", "oldText", "newText", "startLine", BLOCKS_FIELD] {
            assert!(
                props.get(key).is_some(),
                "missing property `{key}`: {schema}"
            );
        }
        let desc = EditFile.description();
        assert!(
            props.get("endLine").is_none(),
            "`endLine` must no longer be advertised: {schema}"
        );
        // The blob is advertised as `blocks`, a *string*. `edits` is not
        // advertised at all: it is the canonical array pi reads off disk, and
        // advertising it is what invites the nested array back onto the wire.
        assert_eq!(
            props
                .get(BLOCKS_FIELD)
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str()),
            Some("string"),
            "the blob field must be advertised as a string"
        );
        assert!(
            props.get(LEGACY_BLOCKS_FIELD).is_none(),
            "the canonical array must not be advertised: it is storage-only: {schema}"
        );
        assert!(
            props.get("batches").is_none(),
            "an intermediate name must not linger: {schema}"
        );
        // No property may be a JSON array — weak harnesses stringify those.
        for (key, prop) in props.as_object().expect("properties is an object") {
            let ty = prop.get("type").and_then(|t| t.as_str()).unwrap_or("");
            assert_ne!(ty, "array", "`{key}` must not be an array: {schema}");
        }
        // The block syntax and the policy sentence live in one const each, so
        // the description and the parameter cannot drift apart.
        assert_eq!(
            desc.matches("@@@OLD").count(),
            1,
            "the syntax example must appear once in the description: {desc}"
        );
        assert!(
            desc.contains(&format!("\n{BLOCK_SYNTAX}\n")),
            "the description must interpolate BLOCK_SYNTAX"
        );

        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array present");
        assert!(
            required.is_empty(),
            "no parameter is required: {required:?}"
        );
    }

    /// The advertised schema and the parser must agree on which fields exist.
    ///
    /// They are written down separately — a `json!` literal here, a `from_wire`
    /// match over the raw `Value` in the crate — and nothing connected them. A
    /// *stale copy* of this schema, served to a model by a client, is not even
    /// observable from here: the repo stays self-consistent and green while the
    /// model is told to send a field the parser no longer reads. That is not
    /// hypothetical — a session was handed `edits` while the tool it reached
    /// expected `blocks`, and the mismatch was invisible to every test in the
    /// tree, because they all read the live `parameters()`.
    ///
    /// So: every advertised property must be a name `from_wire` actually reads,
    /// and the blob property must be spelled by the parser's own const. A field
    /// advertised here but ignored there is a silent dead end for the model.
    #[test]
    fn every_advertised_property_is_one_the_parser_reads() {
        let schema = EditFile.parameters();
        let props = schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("properties present");

        // What `EditArgs::from_wire` reads. Explicit, so that adding a field to
        // one side without the other fails here rather than in a session.
        let parsed = ["path", "oldText", "newText", "startLine", BLOCKS_FIELD];

        for name in props.keys() {
            assert!(
                parsed.contains(&name.as_str()),
                "`{name}` is advertised but `from_wire` never reads it: {schema}"
            );
        }
        for name in parsed {
            assert!(
                props.contains_key(name),
                "`{name}` is read by the parser but not advertised: {schema}"
            );
        }

        // The description must name the advertised field, or the model is told
        // one thing and handed another.
        let desc = EditFile.description();
        assert!(
            desc.contains(BLOCKS_FIELD),
            "the description must name the field it advertises: {desc}"
        );
        for retired in [LEGACY_BLOCKS_FIELD, "batches"] {
            assert!(
                !desc.contains(&format!("`{retired}`")),
                "the description still names the retired `{retired}`: {desc}"
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn block_form_applies_all_edits() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("blob.txt", "a\nb\nc\n").await.unwrap();
        let blob =
            "blob.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\nblob.txt\n@@@OLD\nc\n@@@NEW\nC\n@@@END";
        let result = execute_edit(serde_json::json!({
            "path": "blob.txt",
            "blocks": blob
        }))
        .await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        let after = tokio::fs::read_to_string("blob.txt").await.unwrap();
        assert_eq!(after, "A\nb\nC\n");
    }

    #[tokio::test]
    #[serial]
    async fn a_multi_file_blob_edits_every_named_file() {
        // The W2 wiring, end to end through the erased tool: one call, three
        // files, each applied independently.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("m1.txt", "a\n").await.unwrap();
        tokio::fs::write("m2.txt", "b\n").await.unwrap();
        tokio::fs::write("m3.txt", "c\n").await.unwrap();
        let blob = "m1.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n\
                    m2.txt\n@@@OLD\nb\n@@@NEW\nB\n@@@END\n\n\
                    m3.txt\n@@@OLD\nc\n@@@NEW\nC\n@@@END";
        let result = execute_edit(serde_json::json!({ "blocks": blob })).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        assert_eq!(tokio::fs::read_to_string("m1.txt").await.unwrap(), "A\n");
        assert_eq!(tokio::fs::read_to_string("m2.txt").await.unwrap(), "B\n");
        assert_eq!(tokio::fs::read_to_string("m3.txt").await.unwrap(), "C\n");
    }

    #[tokio::test]
    #[serial]
    async fn one_failing_file_does_not_block_the_others() {
        // Failure granularity is the file, which is what the tool description
        // advertises: m2 does not match, so m1 and m3 must still be written.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("p1.txt", "a\n").await.unwrap();
        tokio::fs::write("p2.txt", "different\n").await.unwrap();
        tokio::fs::write("p3.txt", "c\n").await.unwrap();
        let blob = "p1.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n\
                    p2.txt\n@@@OLD\nnope\n@@@NEW\nN\n@@@END\n\n\
                    p3.txt\n@@@OLD\nc\n@@@NEW\nC\n@@@END";
        let _ = execute_edit(serde_json::json!({ "blocks": blob })).await;
        // The other two files are written despite p2 failing.
        assert_eq!(tokio::fs::read_to_string("p1.txt").await.unwrap(), "A\n");
        assert_eq!(tokio::fs::read_to_string("p3.txt").await.unwrap(), "C\n");
        assert_eq!(
            tokio::fs::read_to_string("p2.txt").await.unwrap(),
            "different\n",
            "the failing file must be untouched"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_multi_file_result_is_a_bare_array_with_per_file_results() {
        // The wire shape W3 fixes: a bare array of `{path, result}`, each result
        // the file's single-file report verbatim. It must reach the history
        // intact so W7/W8 can split it (see doc/batch_editing_v2.md).
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("w1.txt", "a\n").await.unwrap();
        tokio::fs::write("w2.txt", "different\n").await.unwrap();
        let blob = "w1.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n\
                    w2.txt\n@@@OLD\nnope\n@@@NEW\nN\n@@@END";
        let raw = execute_edit(serde_json::json!({ "blocks": blob })).await;
        let out = raw.output().render();
        let v: serde_json::Value = serde_json::from_str(&out).expect("result must be JSON");
        let arr = v.as_array().expect("multi-file result is a bare array");
        assert_eq!(arr.len(), 2);
        assert!(v.get("ui_view").is_none(), "no aggregate wrapper: {out}");
        assert_eq!(arr[0]["path"], "w1.txt");
        assert!(
            arr[0]["result"].is_string(),
            "success is a bare note: {out}"
        );
        assert_eq!(arr[1]["path"], "w2.txt");
        // The failing file carries the existing {ui_view, model_view} envelope.
        assert!(arr[1]["result"]["ui_view"].is_string(), "{out}");
    }

    #[tokio::test]
    #[serial]
    async fn an_unreadable_file_does_not_discard_the_other_files_reports() {
        // Regression: a missing file used to abort the whole call with a bare
        // "the tool failed", losing the files that did apply. Failure
        // granularity is the file.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("u1.txt", "a\n").await.unwrap();
        tokio::fs::write("u3.txt", "c\n").await.unwrap();
        let blob = "u1.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n\
                    nosuch.txt\n@@@OLD\nb\n@@@NEW\nB\n@@@END\n\n\
                    u3.txt\n@@@OLD\nc\n@@@NEW\nC\n@@@END";
        let out = execute_edit(serde_json::json!({ "blocks": blob }))
            .await
            .output()
            .render();
        let v: serde_json::Value = serde_json::from_str(&out).expect("array result");
        let arr = v.as_array().expect("bare array");
        assert_eq!(arr.len(), 3, "one entry per file, always: {out}");
        // The readable files applied...
        assert_eq!(tokio::fs::read_to_string("u1.txt").await.unwrap(), "A\n");
        assert_eq!(tokio::fs::read_to_string("u3.txt").await.unwrap(), "C\n");
        // ...and the missing one is named, not swallowed.
        assert_eq!(arr[1]["path"], "nosuch.txt");
        let msg = arr[1]["result"].as_str().expect("IO failure is a string");
        assert!(msg.contains("nosuch.txt"), "must name the file: {msg}");
    }

    #[tokio::test]
    #[serial]
    async fn a_single_file_result_is_never_a_one_element_array() {
        // Byte-identical to today: a bare note, not an array.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("solo.txt", "a\n").await.unwrap();
        let blob = "solo.txt\n@@@OLD\na\n@@@NEW\nA\n@@@END";
        let out = execute_edit(serde_json::json!({ "blocks": blob }))
            .await
            .output()
            .render();
        assert!(out.contains("Replaced 1 block(s)"), "{out}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&out)
                .ok()
                .and_then(|v| v.as_array().cloned())
                .is_none(),
            "single-file must not become an array: {out}"
        );
    }

    #[tokio::test]
    #[serial]
    async fn block_form_infers_path_from_header() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("infer.txt", "hello\n").await.unwrap();
        let blob = "infer.txt\n@@@OLD\nhello\n@@@NEW\nworld\n@@@END";
        let result = execute_edit(serde_json::json!({ "blocks": blob })).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        let after = tokio::fs::read_to_string("infer.txt").await.unwrap();
        assert_eq!(after, "world\n");
    }

    #[tokio::test]
    #[serial]
    async fn a_redundant_path_argument_never_overrides_a_block_header() {
        // Block paths are authoritative, and `path` is not consulted for the
        // block form. The header names b.txt, so b.txt is edited — a.txt, which
        // the redundant argument named, is not touched at all.
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("a.txt", "untouched\n").await.unwrap();
        tokio::fs::write("b.txt", "x\n").await.unwrap();
        let blob = "b.txt\n@@@OLD\nx\n@@@NEW\ny\n@@@END";
        let result = execute_edit(serde_json::json!({ "path": "a.txt", "blocks": blob })).await;
        assert!(result.is_success(), "expected success, got: {result:?}");
        assert_eq!(
            tokio::fs::read_to_string("b.txt").await.unwrap(),
            "y\n",
            "the header names the file that is edited"
        );
        assert_eq!(
            tokio::fs::read_to_string("a.txt").await.unwrap(),
            "untouched\n",
            "the `path` argument must not re-target the call"
        );
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

#[cfg(test)]
mod rename_verification {
    //! Pins the post-rename contract end-to-end, at the boundary a real model
    //! call crosses. Written to fail loudly if any layer drifts back.
    use super::*;
    use crate::edit_match::looks_like_edit_failure;
    use rig_core::tool::{ErasedTool, ToolContext};
    use serial_test::serial;

    async fn call(args: serde_json::Value) -> rig_core::tool::ToolResult {
        let ctx = &mut ToolContext::new();
        EditFile
            .execute(serde_json::to_string(&args).unwrap(), ctx)
            .await
    }

    #[tokio::test]
    #[serial]
    async fn blocks_applies_and_the_array_is_refused_without_touching_the_file() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("v.txt", "hello\nworld\n").await.unwrap();

        // 1. the blob field works end to end
        let ok = call(serde_json::json!({
            "path": "v.txt",
            "blocks": "v.txt\n@@@OLD\nhello\n@@@NEW\ngoodbye\n@@@END"
        }))
        .await;
        assert!(ok.is_success(), "{ok:?}");
        assert_eq!(
            tokio::fs::read_to_string("v.txt").await.unwrap(),
            "goodbye\nworld\n"
        );

        // 2. the array is refused, by name, and changes nothing
        let bad = call(serde_json::json!({
            "path": "v.txt",
            "edits": [{"oldText": "world", "newText": "there"}]
        }))
        .await;
        assert!(bad.is_error(), "array must not dispatch: {bad:?}");
        let text = bad.output().render();
        assert!(text.contains("blocks"), "must name the field: {text}");
        assert!(
            !looks_like_edit_failure(&text),
            "an arg error is not a batch-failure envelope: {text}"
        );
        assert_eq!(
            tokio::fs::read_to_string("v.txt").await.unwrap(),
            "goodbye\nworld\n",
            "the refused array must not write"
        );
    }

    /// The rename must not break a caller that never saw it. This is the shape
    /// the tool advertised for its whole life.
    /// The exact payload a real client produced this session, replayed verbatim:
    /// the argument object wrapped into the blob field, blob mislabelled `path`.
    /// It must be refused *and named* — a client author reading this can fix it.
    #[tokio::test]
    #[serial]
    async fn the_observed_malformation_is_diagnosed_by_name() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("mangled.txt", "one\n").await.unwrap();
        let r = call(serde_json::json!({
            "edits": { "path": "mangled.txt\n@@@OLD\none\n@@@NEW\n1\n@@@END" },
            "path": "mangled.txt"
        }))
        .await;
        assert!(r.is_error(), "{r:?}");
        let text = r.output().render();
        assert!(text.contains("is an object"), "must name the fault: {text}");
        assert_eq!(
            tokio::fs::read_to_string("mangled.txt").await.unwrap(),
            "one\n",
            "a malformed call must not write"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_legacy_edits_string_applies_at_the_tool_boundary() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("legacy.txt", "one\ntwo\n").await.unwrap();
        let r = call(serde_json::json!({
            "path": "legacy.txt",
            "edits": "legacy.txt\n@@@OLD\none\n@@@NEW\n1\n@@@END"
        }))
        .await;
        assert!(r.is_success(), "a pre-rename call must still work: {r:?}");
        assert_eq!(
            tokio::fs::read_to_string("legacy.txt").await.unwrap(),
            "1\ntwo\n"
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_blob_getting_the_file_from_a_header_alone_still_works() {
        let _cwd = super::edit_file_tests::CwdGuard::new();
        tokio::fs::write("h.txt", "one\n").await.unwrap();
        let r = call(serde_json::json!({
            "blocks": "h.txt\n@@@OLD\none\n@@@NEW\n1\n@@@END"
        }))
        .await;
        assert!(r.is_success(), "{r:?}");
        assert_eq!(tokio::fs::read_to_string("h.txt").await.unwrap(), "1\n");
    }
}
