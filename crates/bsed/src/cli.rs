//! The `bsed` command line.
//!
//! One subcommand, and the default mode needs no word to reach it:
//!
//! ```text
//! bsed [OPTIONS] [SCRIPT] [FILE]...   # print the block blob (the default)
//! bsed apply [-p PATH] [-i] [FILE]    # read blocks from stdin, apply, write
//! ```
//!
//! Previewing never writes and is what picocode runs (inside a read-only
//! sandbox). `apply` is for humans at a shell, and without `-i` it writes the
//! result to stdout, so `bsed apply f.rs < blocks.txt | diff f.rs -` is a review
//! loop that cannot damage anything.
//!
//! Exit codes are a contract with the caller: `0` success, `2` the caller's
//! problem (a rejected expression, an unresolvable block), `1` ours.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use blockedit::{apply::ApplyOutcome, EditOp, ParsedEdit};

use crate::{Context, Options};

const USAGE: &str = "\
bsed — block-edit sed

USAGE:
    bsed [OPTIONS]... [SCRIPT] [FILE]...
    bsed apply [OPTIONS] [FILE]

PREVIEW (never writes; the default):
    -e, --expression <SCRIPT>   Add the script to the commands to execute
    -f, --file <SCRIPT_FILE>    Add the contents of script-file to the commands
    -p, --path <PATH>           File to preview, or a directory with -r
    -r, --recursive             Walk a directory (.gitignore-aware, hidden files
                                skipped) and preview every file, emitting one blob
                                with a header per file. No budget: a human is reading
                                this, not a model.
    -g, --glob <PATTERN>        With -r, only files whose path relative to the root
                                matches. Repeatable, and any match wins. Same
                                semantics as the `find` tool, so use **/*.rs for a
                                recursive match.
    -c, --context <N>           Unchanged lines to add before each change (default 0)
    -t, --trailing <N>          Unchanged lines to add after a change (default 0)
        --no-grow               Do not extend blocks to a balanced bracket
    -b, --budget <BYTES>        Preview budget, single-file mode only (default 51200)
    -h, --help
    -n, --quiet                 Suppress automatic printing (ignored; preview only)
    SCRIPT                      If no -e/-f given, first non-option is the script
    FILE...                     Input files (default: stdin if no files given)

APPLY (writes):
    -p, --path <PATH>           File to edit (default: the FILE argument, else -)
    -i, --in-place              Write the file; without it the result goes to stdout.
                                Required for a multi-file blob.
    A blob may span files. Each file is applied independently -- a file whose
    blocks do not resolve is reported and left alone, and the others still go
    through. The failed blocks are echoed back so they can be fixed and
    re-piped.

EXAMPLES:
    bsed 's/old/new/' src/x.rs
    bsed -e 's/old/new/' -p src/x.rs
    bsed -f script.sed src/x.rs
    bsed -r -p src 's/old/new/' > blocks.txt
    bsed apply -i < blocks.txt
    bsed apply src/x.rs < blocks.txt | diff src/x.rs -
";

/// Entry point shared by the standalone binary and picocode's shim, so both
/// resolve the same arguments the same way.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            if !out.is_empty() {
                emit(&out);
            }
            ExitCode::SUCCESS
        }
        Err(Failure::Usage(m)) => {
            eprintln!("{m}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::User(m)) => {
            eprintln!("{m}");
            ExitCode::from(2)
        }
        Err(Failure::Partial {
            report,
            failed_files,
        }) => {
            // The report is the *result* of the run -- which files applied, and
            // the blocks to fix for the ones that did not -- so it goes to
            // stdout where a redirect or a pager will catch it. stderr carries
            // only the one-line signal a script needs to notice.
            emit(&report);
            eprintln!("bsed: {failed_files} file(s) failed; see the report above");
            ExitCode::from(2)
        }
        Err(Failure::Internal(m)) => {
            eprintln!("bsed: {m}");
            ExitCode::FAILURE
        }
    }
}

/// Print `body` to stdout, and when stdout is a terminal also leave a copy in
/// the temp directory and say where.
///
/// The case this exists for: a recursive preview over a few hundred files, or a
/// failure report with blocks in it, read on a screen. Scrolling back through
/// a terminal to copy a block verbatim is exactly the mistake that produces a
/// wrong edit, and the report is meant to be re-piped rather than retyped. When
/// stdout is a pipe the caller is already capturing the bytes, so we leave the
/// filesystem alone.
fn emit(body: &str) {
    print!("{body}");
    if !should_spool(body, std::io::stdout().is_terminal()) {
        return;
    }
    match spool(body) {
        Some(path) => eprintln!(
            "bsed: wrote a copy to {}\n      edit it, then apply with: bsed apply -i < {}",
            path.display(),
            path.display()
        ),
        None => {}
    }
}

/// Whether this run should leave a copy on disk.
///
/// Only when the output is *only* going to a screen (a pipe means the caller
/// already has the bytes) and only when it carries blocks, since a copy is only
/// useful if there is something in it to edit and re-pipe.
fn should_spool(body: &str, is_tty: bool) -> bool {
    is_tty && body.contains(blockedit::SEARCH_MARKER)
}

/// Write `body` to a fresh temp file whose name says what it is.
///
/// `BSED_SPOOL_DIR` overrides the location, which is mostly for tests (a global
/// /tmp counter is not hermetic) and for anyone who would rather these landed
/// somewhere they are going to clean up.
fn spool(body: &str) -> Option<PathBuf> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let dir = std::env::var_os("BSED_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("bsed-{}-{nanos}.blocks", std::process::id()));
    let mut f = std::fs::File::create(&path).ok()?;
    f.write_all(body.as_bytes()).ok()?;
    Some(path)
}

/// Why a run stopped, and who has to fix it.
enum Failure {
    /// Bad invocation: wrong flags, missing argument.
    Usage(String),
    /// Some files applied and some did not. Carries the report and how many
    /// files failed: the report is the run's result and goes to stdout, the
    /// count is a one-line signal for stderr. The exit code is 2, because a
    /// caller that ignores it now has a half-done tree.
    Partial { report: String, failed_files: usize },
    /// The caller's input is wrong (a rejected expression, an unresolvable
    /// block). Model-fixable.
    User(String),
    /// Ours: an I/O failure or an inconsistent internal state.
    Internal(String),
}

/// Pick the mode. `apply` is the only subcommand; everything else previews,
/// which is what the name is for.
///
/// The dispatcher stays this thin on purpose. It used to reject a leading flag
/// with "unknown option", on the reasoning that a flag cannot be an expression —
/// true, but it left `bsed -p f.rs 's/a/b/'` unusable even though that is exactly
/// what `sed` accepts, and what picocode's `sed` tool wants to run. Handing every
/// non-`apply` argument list to the preview parser instead means options may
/// lead, the script may come from `-e`, `-f` or a positional, and an option that
/// exists in neither mode is still rejected — by the parser that knows the
/// options, with the same message and the same usage text.
fn run(args: &[String]) -> Result<String, Failure> {
    match args.first().map(String::as_str) {
        // No arguments at all: print usage rather than block forever waiting for
        // expressions on stdin, which is what `bsed` on its own used to do.
        None => Ok(USAGE.to_string()),
        Some("apply") => apply(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => Ok(USAGE.to_string()),
        Some("--version") | Some("-V") => Ok(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))),
        // `preview` was a subcommand once. Left here so the muscle memory lands
        // on the reason rather than on a parse error about the word itself.
        Some("preview") => Err(Failure::Usage(
            "there is no `preview` subcommand — previewing is what `bsed` does by default. \
             Drop the word: `bsed 's/a/b/' f.rs`."
                .into(),
        )),
        Some(_) => preview(args),
    }
}

fn preview(args: &[String]) -> Result<String, Failure> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut recursive = false;
    let mut globs: Vec<String> = Vec::new();
    let mut context: usize = 0;
    let mut trailing: Option<usize> = None;
    let mut grow = true;
    let mut budget = Options::default().max_preview_bytes;
    let mut script_parts: Vec<String> = Vec::new();
    let mut only_positional = false;
    let mut saw_e_or_f = false;

    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if only_positional {
            // After --, everything is a file
            paths.push(PathBuf::from(a.clone()));
            continue;
        }
        match a.as_str() {
            "--" => only_positional = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            "--version" | "-V" => return Ok(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))),
            "-e" | "--expression" => {
                saw_e_or_f = true;
                script_parts.push(need(it.next(), "-e")?.to_string());
            }
            "-f" | "--file" => {
                saw_e_or_f = true;
                let script_file = need(it.next(), "-f")?;
                let content = std::fs::read_to_string(script_file)
                    .map_err(|e| Failure::Internal(format!("{script_file}: {e}")))?;
                script_parts.push(content);
            }
            "-p" | "--path" => paths.push(PathBuf::from(need(it.next(), "-p")?)),
            "-r" | "--recursive" => recursive = true,
            "-g" | "--glob" => globs.push(need(it.next(), "-g")?.to_string()),
            "-c" | "--context" => context = need_num(it.next(), "-c")?,
            "-t" | "--trailing" => trailing = Some(need_num(it.next(), "-t")?),
            "-b" | "--budget" => budget = need_num(it.next(), "-b")?,
            "--no-grow" => grow = false,
            "-n" | "--quiet" => {
                // Accepted for sed compatibility; preview ignores it
            }
            other if other.len() > 1 && other.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown option `{other}`")));
            }
            other => {
                // First non-option is the script (if no -e/-f), rest are files
                if !saw_e_or_f && script_parts.is_empty() {
                    script_parts.push(other.to_string());
                } else {
                    paths.push(PathBuf::from(other));
                }
            }
        }
    }

    // Build the script from -e/-f or the first positional
    let script = if script_parts.is_empty() {
        // No script on command line: read from stdin
        if std::io::stdin().is_terminal() {
            return Err(Failure::Usage(
                "no sed script given. Usage: bsed [-e SCRIPT] [-f FILE] [FILE]...".into(),
            ));
        }
        read_stdin()?
    } else {
        script_parts.join("\n")
    };
    if script.trim().is_empty() {
        return Err(Failure::Usage("no sed script given".into()));
    }

    // Determine input files: -p arguments, then positional files, then stdin
    let input_files = if paths.is_empty() {
        // No -p and no positional files: read from stdin
        vec![PathBuf::from("-")]
    } else {
        paths
    };

    let ctx = Context {
        leading: context,
        trailing: trailing.unwrap_or(0),
        grow,
        growth_cap: 20,
    };

    // Handle single file or multiple files
    if input_files.len() == 1 && !recursive {
        let p = &input_files[0];
        if p == Path::new("-") {
            let text = read_stdin()?;
            return single(&text, "(stdin)", &script, ctx, budget);
        }
        if p.is_dir() {
            if !recursive {
                return Err(Failure::Usage(format!(
                    "{} is a directory. Use `bsed -r -p {}` to preview every file under it.",
                    p.display(),
                    p.display()
                )));
            }
            return recursive_preview(p, &script, ctx, &globs);
        }
        if !globs.is_empty() {
            return Err(Failure::Usage(
                "--glob only applies with --recursive".into(),
            ));
        }
        let text = std::fs::read_to_string(p)
            .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
        single(&text, &p.display().to_string(), &script, ctx, budget)
    } else if recursive && input_files.len() == 1 && input_files[0].is_dir() {
        // Single directory with -r
        recursive_preview(&input_files[0], &script, ctx, &globs)
    } else if recursive {
        // Multiple files with -r: treat each as a root to walk
        // (This is a simplification; typically -r takes one directory)
        return Err(Failure::Usage(
            "--recursive expects a single directory".into(),
        ));
    } else {
        // Multiple files: preview each and concatenate
        let mut combined = String::new();
        for p in &input_files {
            if p == Path::new("-") {
                let text = read_stdin()?;
                let result = single(&text, "(stdin)", &script, ctx, budget)?;
                combined.push_str(&result);
            } else if p.is_dir() {
                return Err(Failure::Usage(format!(
                    "{} is a directory. Use -r to preview recursively.",
                    p.display()
                )));
            } else {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
                let result = single(&text, &p.display().to_string(), &script, ctx, budget)?;
                combined.push_str(&result);
            }
        }
        Ok(combined)
    }
}

/// One file, with the agent-facing preview budget applied.
fn single(
    text: &str,
    shown: &str,
    expressions: &str,
    ctx: Context,
    budget: usize,
) -> Result<String, Failure> {
    let opts = Options {
        context: ctx,
        max_preview_bytes: budget,
    };
    let p = crate::preview_text(text, shown, expressions, opts)
        .map_err(|e| Failure::User(e.to_string()))?;
    // A block the `edit` tool would misread is a real limit, not a formatting
    // nit: refuse it here rather than hand back a blob that cannot be applied.
    if let Some(why) = p.blocked {
        return Err(Failure::User(format!(
            "the sed preview could not be expressed as appliable blocks for {shown}:\n{why}"
        )));
    }
    if p.blob.is_empty() {
        return Ok(format!("{}\n", p.banner));
    }
    Ok(format!("{}\n{}\n", p.banner, p.blob))
}

/// Preview every file under `root`, emitting one blob with a header per file.
///
/// The recursive mode has no preview budget: a human is reading the output, not
/// a model paying per token, and a silently truncated tree refactor is the worst
/// possible outcome. Files that cannot be read as UTF-8 are skipped and counted
/// rather than failing the run — a tree is allowed to contain binaries.
fn recursive_preview(
    root: &Path,
    expressions: &str,
    ctx: Context,
    globs: &[String],
) -> Result<String, Failure> {
    // Same matcher and the same semantics as picocode's `find` tool: the pattern
    // is matched against the path *relative to the walk root*, so a recursive
    // match is written `**/*.rs`.
    let matcher = match build_matcher(globs) {
        Ok(m) => m,
        Err(e) => return Err(e),
    };
    let opts = Options {
        context: ctx,
        // Unbounded: see above. `Options` still needs *a* value; the renderer
        // is only consulted per file below, with usize::MAX.
        max_preview_bytes: usize::MAX,
    };

    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        // Respect .gitignore outside a repository too: skipping node_modules
        // and target should not depend on whether the tree happens to be
        // under version control.
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .build()
        .filter_map(|e| match e {
            Ok(e) if e.file_type().map(|t| t.is_file()).unwrap_or(false) => Some(e.into_path()),
            // A walker error (permission, broken link) should not sink the run.
            _ => None,
        })
        .collect();
    // Deterministic order, so two runs over the same tree are diffable.
    files.sort();
    if let Some(m) = &matcher {
        files.retain(|f| m.is_match(&relative_to(root, f)));
    }

    let mut blob = String::new();
    let mut changed_files = 0usize;
    let mut blocks = 0usize;
    let mut skipped = 0usize;
    let mut blocked: Vec<(String, String)> = Vec::new();
    // Kept apart from `blocked` because the advice differs. "Re-run on that file
    // alone" is what rescues a block that cannot be expressed; a CRLF file is
    // refused either way, so telling the reader to retry it would be a loop.
    let mut crlf: Vec<String> = Vec::new();

    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            skipped += 1; // binary or unreadable
            continue;
        };
        let p = match crate::preview_text(&text, &f.display().to_string(), expressions, opts) {
            Ok(p) => p,
            // One refused file must not sink the tree, but it must be named:
            // silence here would read as "that file needed no change".
            Err(crate::Error::CarriageReturn(_)) => {
                crlf.push(f.display().to_string());
                continue;
            }
            Err(e) => return Err(Failure::User(e.to_string())),
        };
        if p.blob.is_empty() {
            continue;
        }
        if let Some(why) = p.blocked {
            // One unrepresentable file must not sink the tree, but it must be
            // named: silence here would read as "that file needed no change".
            blocked.push((f.display().to_string(), why));
            continue;
        }
        let b = p.blob;
        if !blob.is_empty() {
            blob.push('\n');
        }
        blob.push_str(&b);
        changed_files += 1;
        blocks += count_blocks(&b);
    }

    if changed_files == 0 {
        let mut out = format!(
            "no lines matched — none of the {} file(s) under {} changed{}{}\n",
            files.len() - skipped,
            root.display(),
            if skipped > 0 {
                format!(" ({skipped} skipped: not UTF-8 text)")
            } else {
                String::new()
            },
            if blocked.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} matched but not expressible as blocks — re-run on those files alone)",
                    blocked.len()
                )
            }
        );
        // "no lines matched" is the wrong sentence when a file did match and was
        // refused, so the CRLF files are spelled out rather than just counted.
        if !crlf.is_empty() {
            out.push_str(&crlf_report(&crlf));
        }
        return Ok(out);
    }

    let mut out = format!(
        "sed preview: {changed_files} of {} file(s) under {} changed, {blocks} block(s) \
         total. Review the blocks below, then apply them with `bsed apply -i`.\n",
        files.len(),
        root.display(),
    );
    if skipped > 0 {
        out.push_str(&format!(
            "NOTE: {skipped} file(s) skipped (not UTF-8 text).\n"
        ));
    }
    if !blocked.is_empty() {
        out.push_str(&format!(
            "NOTE: {} file(s) matched but cannot be expressed as appliable blocks, and are \
             NOT in the output:\n",
            blocked.len()
        ));
        for (path, why) in &blocked {
            out.push_str(&format!("  {path}: {why}\n"));
        }
    }
    if !crlf.is_empty() {
        out.push_str(&crlf_report(&crlf));
    }
    out.push_str(&blob);
    out.push('\n');
    Ok(out)
}

/// Name the files that were refused for CRLF, and what to do about them.
///
/// Separate from the "not expressible as blocks" note because the two need
/// opposite responses: an unexpressible block may be rescued by re-running on
/// that file alone, while a CRLF file is refused whatever the script is and
/// only converting it will help. They are listed whenever any file was skipped
/// for line endings, not only when the run found nothing at all.
fn crlf_report(paths: &[String]) -> String {
    let mut s = format!(
        "NOTE: {} file(s) use CRLF line endings and were NOT previewed. The line endings \
         come off before the change is worked out, so a preview would describe a different \
         file and no block would apply back. Convert {} to LF and re-run.\n",
        paths.len(),
        if paths.len() == 1 { "it" } else { "them" }
    );
    for p in paths {
        s.push_str(&format!("  {p}\n"));
    }
    s
}

/// Compile the `--glob` patterns into one allow-list matcher.
fn build_matcher(globs: &[String]) -> Result<Option<globset::GlobSet>, Failure> {
    if globs.is_empty() {
        return Ok(None);
    }
    let mut b = globset::GlobSetBuilder::new();
    for g in globs {
        let glob = globset::Glob::new(g)
            .map_err(|e| Failure::Usage(format!("invalid --glob `{g}`: {e}")))?;
        b.add(glob);
    }
    b.build()
        .map(Some)
        .map_err(|e| Failure::Usage(format!("invalid --glob set: {e}")))
}

/// A path relative to the walk root, for glob matching and display.
fn relative_to(root: &Path, f: &Path) -> std::path::PathBuf {
    f.strip_prefix(root).unwrap_or(f).to_path_buf()
}

/// How many blocks a rendered blob carries: one `@@@OLD` per block.
fn count_blocks(blob: &str) -> usize {
    blob.lines()
        .filter(|l| l.trim() == blockedit::SEARCH_MARKER)
        .count()
}

fn read_stdin() -> Result<String, Failure> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| Failure::Internal(e.to_string()))?;
    Ok(buf)
}

fn apply(args: &[String]) -> Result<String, Failure> {
    let mut path: Option<PathBuf> = None;
    let mut in_place = false;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(USAGE.to_string()),
            "-p" | "--path" => path = Some(PathBuf::from(need(it.next(), "-p")?)),
            "-i" | "--in-place" => in_place = true,
            other if other.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown option `{other}`")));
            }
            other => path = Some(PathBuf::from(other)),
        }
    }
    // Blocks arrive on stdin. This is `group_by_file`, which is
    // *header-authoritative*: a block that names its own file uses that name,
    // and a file named on the command line only fills the gaps left by blocks
    // with no header. (The `edit` tool has no such fallback — its
    // `split_by_file` requires every block to name its own file — so do not
    // read this precedence as the tool's.) Parse first, because the headers are
    // where the fallback comes from and because a blob may span many files.
    let named = match path {
        Some(p) if p.as_os_str().is_empty() || p == PathBuf::from("-") => None,
        Some(p) => Some(p),
        None => None,
    };
    let blob = read_stdin()?;
    let parsed = blockedit::parse_blocks(&blob).map_err(|e| Failure::User(e.to_string()))?;
    let batches =
        blockedit::group_by_file(parsed, named.as_deref().map(|p| p.to_str().unwrap_or("")))
            .map_err(Failure::User)?;

    if batches.len() > 1 && !in_place {
        return Err(Failure::Usage(format!(
            "this blob covers {} files, so there is no single result to print. Re-run with \
             `-i` to write each file, or split the blob per file.",
            batches.len()
        )));
    }

    let mut written: Vec<(String, usize)> = Vec::new();
    let mut failed: Vec<(String, String, Vec<ParsedEdit>)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    // Each file is applied independently: a file whose blocks do not resolve is
    // reported and left untouched, and the rest still go through. That is the
    // per-file atomicity the recursive flow needs -- one stale file in a tree of
    // two hundred should not block the other hundred and ninety-nine.
    for batch in &batches {
        let path = &batch.path;
        let edits: Vec<EditOp> = blockedit::to_ops(&batch.edits);
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                problems.push(format!("{}: {e}", path));
                continue;
            }
        };
        match blockedit::apply_to_text(&text, &edits) {
            ApplyOutcome::Applied { text, .. } => {
                if in_place {
                    if let Err(e) = std::fs::write(path, &text) {
                        problems.push(format!("{}: {e}", path));
                        continue;
                    }
                }
                written.push((path.clone(), edits.len()));
            }
            ApplyOutcome::Failed {
                report,
                failed: bad,
            } => {
                // Echo back only the blocks that failed, so they can be fixed
                // and re-piped without re-deciding the ones that were fine.
                let echo: Vec<ParsedEdit> = bad
                    .iter()
                    .filter_map(|i| batch.edits.get(*i).cloned())
                    .collect();
                let why = human_reason(&report);
                failed.push((path.clone(), why, echo));
            }
        }
    }

    // The report is written so that it is *itself a valid blocks file*: the
    // diagnosis goes in prose, and the blocks come out bare at the end. That
    // means `bsed apply -i < report.txt` re-applies exactly the failed blocks
    // once they are fixed -- no detangling, and nothing lost but the failures
    // themselves. Prose between blocks is ignored by the parser, and `prose()`
    // guarantees no line we emit can be mistaken for a `path:LINE` header.
    let mut out = String::new();
    for (path, n) in &written {
        out.push_str(&format!("{path}: {n} block(s) applied\n"));
    }
    for p in &problems {
        out.push_str(&format!("{p}\n"));
    }
    if out.is_empty() && failed.is_empty() {
        out.push_str("nothing to do\n");
    }

    if !failed.is_empty() {
        let total: usize = failed.iter().map(|(_, _, echo)| echo.len()).sum();
        out.push_str(&prose(&format!(
            "{written_len} file(s) applied, {} failed; nothing was written for the failures.\n\
             The blocks below are the {total} that did not apply, as a valid blocks file: edit \
             them (or re-preview with a narrower expression) and re-pipe with \
             `bsed apply -i < this file`.",
            failed.len(),
            written_len = written.len(),
        )));
        for (path, why, _) in &failed {
            out.push_str(&prose(&format!("--- {path}: {why}\n")));
        }
        out.push('\n');
        // Anchored headers, exactly as `preview` emits them, so a re-pipe
        // resolves a duplicated line by its anchor and not by content alone.
        for (path, _, echo) in &failed {
            if echo.is_empty() {
                continue;
            }
            let blocks: Vec<crate::Block> = echo
                .iter()
                .map(|e| crate::Block {
                    anchor: e.start_line.unwrap_or(1) as usize,
                    old_text: e.old_text.clone(),
                    new_text: e.new_text.clone(),
                })
                .collect();
            out.push_str(&crate::blocks::render(path, &blocks, usize::MAX).blob);
            out.push_str("\n\n");
        }
        return Err(Failure::Partial {
            failed_files: failed.len(),
            report: out,
        });
    }

    // Single file, no -i: the result itself is the output, so it can be diffed.
    if !in_place && batches.len() == 1 {
        let path = &batches[0].path;
        let text = std::fs::read_to_string(path)
            .map_err(|e| Failure::Internal(format!("{}: {e}", path)))?;
        let edits: Vec<EditOp> = blockedit::to_ops(&batches[0].edits);
        if let ApplyOutcome::Applied { text, .. } = blockedit::apply_to_text(&text, &edits) {
            return Ok(text);
        }
    }
    Ok(out)
}

/// Turn the JSON failure report into one human-readable line.
///
/// The structured report is for a model; a terminal wants the sentence, and the
/// per-block detail is re-emitted as real blocks right after it.
fn human_reason(report: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(report) else {
        return "blocks did not resolve".to_string();
    };
    let raw = v
        .get("ui_view")
        .and_then(|x| x.as_str())
        .unwrap_or("blocks did not resolve");
    // The report's own "edit failed:" header duplicates what the line already
    // says, and its newlines are layout, not content.
    raw.strip_prefix("edit failed:")
        .unwrap_or(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Emit a diagnostic line that the blob parser cannot mistake for structure.
///
/// `parse_blocks` treats a whitespace-free line above a block as that block's
/// `path:LINE` header, and a line that is exactly `@@@OLD`/`@@@NEW`/`@@@END` as
/// a delimiter. Keeping every diagnostic line out of both classes is what lets
/// the failure report double as a blocks file, so the rule is enforced here
/// rather than trusted to the wording.
fn prose(line: &str) -> String {
    let mut out = String::new();
    for (i, l) in line.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Empty lines are ignored by the parser; a lone token is not.
        if !l.trim().is_empty() && l.chars().all(|c| !c.is_whitespace()) {
            out.push(' ');
        }
        out.push_str(l);
    }
    out.push('\n');
    out
}
fn need<'a>(v: Option<&'a String>, flag: &str) -> Result<&'a str, Failure> {
    v.map(String::as_str)
        .ok_or_else(|| Failure::Usage(format!("{flag} needs a value")))
}

fn need_num(v: Option<&String>, flag: &str) -> Result<usize, Failure> {
    let raw = need(v, flag)?;
    raw.trim()
        .parse()
        .map_err(|_| Failure::Usage(format!("{flag} needs a number, got `{raw}`")))
}

#[cfg(test)]
mod emit_tests {
    use super::{should_spool, spool};

    #[test]
    fn spools_only_to_a_terminal_and_only_with_blocks() {
        // The rules, without needing a pty: a screen means the bytes would
        // otherwise be lost to a scrollback, and there has to be something
        // worth re-piping in the file.
        assert!(should_spool("@@@OLD\na\n@@@END", true));
        assert!(!should_spool("@@@OLD\na\n@@@END", false));
        assert!(!should_spool("no lines matched -- nothing changed", true));
    }

    #[test]
    fn spool_respects_the_directory_override() {
        let dir = tempfile::tempdir().unwrap();
        // Safety: single-threaded test, and the variable is only read by a
        // child process this test makes itself.
        std::env::set_var("BSED_SPOOL_DIR", dir.path());
        let p = spool("body").expect("spooled");
        std::env::remove_var("BSED_SPOOL_DIR");
        assert_eq!(p.parent(), Some(dir.path()));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "body");
    }

    #[test]
    fn a_poem_of_prose_lines_cannot_be_read_as_headers() {
        // The invariant that lets the failure report double as a blocks file: no
        // diagnostic line may be a bare token, which the parser would take for a
        // `path:LINE` header.
        use super::prose;
        for line in [
            "one.rs",
            "@@@OLD",
            "no-blocks-here",
            "",
            "  leading and trailing  ",
        ] {
            for out in prose(line).lines() {
                if out.trim().is_empty() {
                    continue; // ignored by the parser
                }
                assert!(
                    out.chars().any(char::is_whitespace),
                    "{out:?} could be read as a block header"
                );
            }
        }
    }
}
