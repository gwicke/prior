//! The `bsed` command line.
//!
//! Two subcommands, one library:
//!
//! ```text
//! bsed preview [-c N] [-p PATH] <expressions>   # print the block blob (default)
//! bsed apply   [-p PATH] [-i] [FILE]           # read blocks from stdin, apply, write
//! ```
//!
//! `preview` never writes and is what picocode runs (inside a read-only
//! sandbox). `apply` is for humans at a shell, and without `-i` it writes the
//! result to stdout, so `bsed apply f.rs < blocks.txt | diff f.rs -` is a review
//! loop that cannot damage anything.
//!
//! Exit codes are a contract with the caller: `0` success, `2` the caller's
//! problem (a rejected expression, an unresolvable block), `1` ours.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use blockedit::{apply::ApplyOutcome, EditOp, ParsedEdit};

use crate::{Context, Options};

const USAGE: &str = "\
bsed — block-edit sed

USAGE:
    bsed preview [OPTIONS] <expressions>
    bsed apply   [OPTIONS] [FILE]

PREVIEW (never writes; the default):
    -p, --path <PATH>     File to preview, or a directory with -r
    -r, --recursive       Walk a directory (.gitignore-aware, hidden files
                          skipped) and preview every file, emitting one blob
                          with a header per file. No budget: a human is reading
                          this, not a model.
    -c, --context <N>     Unchanged lines each side; trailing is always at
                          least 2 (default 0)
    -t, --trailing <N>    Minimum unchanged lines after a change (default 2)
        --no-grow         Do not extend blocks to a balanced bracket
    -b, --budget <BYTES>  Preview budget, single-file mode only (default 51200)
    -h, --help

APPLY (writes):
    -p, --path <PATH>     File to edit (default: the FILE argument, else -)
    -i, --in-place        Write the file; without it the result goes to stdout.
                          Required for a multi-file blob.
    A blob may span files. Each file is applied independently -- a file whose
    blocks do not resolve is reported and left alone, and the others still go
    through. The failed blocks are echoed back so they can be fixed and
    re-piped.

EXAMPLES:
    bsed preview '177s/edits: EditInput,/edits: serde_json::Value,/' -p src/x.rs
    bsed preview -r -p src 's/EditInput/serde_json::Value/g' > blocks.txt
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
                print!("{out}");
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
        Err(Failure::Partial(m)) => {
            // The report is the *result* of the run -- which files applied, and
            // the blocks to fix for the ones that did not -- so it goes to
            // stdout where a redirect or a pager will catch it. stderr carries
            // only the one-line signal a script needs to notice.
            print!("{m}");
            let failed = m.matches("FAILED").count();
            eprintln!("bsed: {failed} file(s) failed; see the report above");
            ExitCode::from(2)
        }
        Err(Failure::Internal(m)) => {
            eprintln!("bsed: {m}");
            ExitCode::FAILURE
        }
    }
}

/// Why a run stopped, and who has to fix it.
enum Failure {
    /// Bad invocation: wrong flags, missing argument.
    Usage(String),
    /// Some files applied and some did not. The message says both; the exit
    /// code is 2, because a caller that ignores it now has a half-done tree.
    Partial(String),
    /// The caller's input is wrong (a rejected expression, an unresolvable
    /// block). Model-fixable.
    User(String),
    /// Ours: an I/O failure or an inconsistent internal state.
    Internal(String),
}

fn run(args: &[String]) -> Result<String, Failure> {
    match args.first().map(String::as_str) {
        None => preview(&[]),
        Some("preview") => preview(&args[1..]),
        Some("apply") => apply(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => Ok(USAGE.to_string()),
        Some("--version") | Some("-V") => Ok(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))),
        // A leading flag cannot be an expression, so it is a usage error.
        Some(other) if other.len() > 1 && other.starts_with('-') => {
            Err(Failure::Usage(format!("unknown option `{other}`")))
        }
        // Otherwise the first word is an expression: `bsed 's/x/y/' -p f.rs`
        // is the form people actually type.
        Some(_) => preview(args),
    }
}

fn preview(args: &[String]) -> Result<String, Failure> {
    let mut path: Option<PathBuf> = None;
    let mut recursive = false;
    let mut context: usize = 0;
    let mut trailing: Option<usize> = None;
    let mut grow = true;
    let mut budget = Options::default().max_preview_bytes;
    let mut script: Vec<String> = Vec::new();
    let mut only_positional = false;

    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if only_positional {
            script.push(a.clone());
            continue;
        }
        match a.as_str() {
            "--" => only_positional = true,
            "-h" | "--help" => return Ok(USAGE.to_string()),
            "--version" | "-V" => return Ok(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))),
            "-p" | "--path" => path = Some(PathBuf::from(need(it.next(), "-p")?)),
            "-r" | "--recursive" => recursive = true,
            "-c" | "--context" => context = need_num(it.next(), "-c")?,
            "-t" | "--trailing" => trailing = Some(need_num(it.next(), "-t")?),
            "-b" | "--budget" => budget = need_num(it.next(), "-b")?,
            "--no-grow" => grow = false,
            other if other.len() > 1 && other.starts_with('-') => {
                return Err(Failure::Usage(format!("unknown option `{other}`")));
            }
            other => script.push(other.to_string()),
        }
    }

    // Expressions come from the command line when given, else from stdin; the
    // file comes from -p, else from stdin. Both from stdin is ambiguous, so it
    // is a usage error rather than a guess.
    let (expressions, from_stdin_script) = if script.is_empty() {
        (read_stdin()?, true)
    } else {
        (script.join("\n"), false)
    };
    if expressions.trim().is_empty() {
        return Err(Failure::Usage("no sed expressions given".into()));
    }
    if path.is_none() && from_stdin_script {
        return Err(Failure::Usage(
            "with expressions on stdin, name the file: bsed preview -p FILE < expressions".into(),
        ));
    }

    let ctx = Context {
        leading: context,
        trailing: trailing.unwrap_or(2),
        grow,
        growth_cap: 20,
    };

    let Some(p) = path.as_ref() else {
        // No `-p` at all: the file arrives on stdin (`cat f.rs | bsed preview
        // 's/x/y/'`), which cannot be recursive.
        let text = read_stdin()?;
        return single(&text, "(stdin)", &expressions, ctx, budget);
    };
    if p == Path::new("-") {
        let text = read_stdin()?;
        return single(&text, "(stdin)", &expressions, ctx, budget);
    }
    if p.is_dir() {
        if !recursive {
            return Err(Failure::Usage(format!(
                "{} is a directory. Use `bsed preview -r -p {}` to preview every file under it.",
                p.display(),
                p.display()
            )));
        }
        return recursive_preview(p, &expressions, ctx);
    }
    if recursive {
        return Err(Failure::Usage(
            "--recursive needs a directory; -p named a file".into(),
        ));
    }
    let text = std::fs::read_to_string(p)
        .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
    single(&text, &p.display().to_string(), &expressions, ctx, budget)
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
    let (banner, blob) = crate::preview_text(text, shown, expressions, opts)
        .map_err(|e| Failure::User(e.to_string()))?;
    if blob.is_empty() {
        return Ok(format!("{banner}\n"));
    }
    Ok(format!("{banner}\n{blob}\n"))
}

/// Preview every file under `root`, emitting one blob with a header per file.
///
/// The recursive mode has no preview budget: a human is reading the output, not
/// a model paying per token, and a silently truncated tree refactor is the worst
/// possible outcome. Files that cannot be read as UTF-8 are skipped and counted
/// rather than failing the run — a tree is allowed to contain binaries.
fn recursive_preview(root: &Path, expressions: &str, ctx: Context) -> Result<String, Failure> {
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

    let mut blob = String::new();
    let mut changed_files = 0usize;
    let mut blocks = 0usize;
    let mut skipped = 0usize;

    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            skipped += 1; // binary or unreadable
            continue;
        };
        let (_, b) = crate::preview_text(&text, &f.display().to_string(), expressions, opts)
            .map_err(|e| Failure::User(e.to_string()))?;
        if b.is_empty() {
            continue;
        }
        if !blob.is_empty() {
            blob.push('\n');
        }
        blob.push_str(&b);
        changed_files += 1;
        blocks += count_blocks(&b);
    }

    if changed_files == 0 {
        return Ok(format!(
            "no lines matched — none of the {} file(s) under {} changed{}\n",
            files.len() - skipped,
            root.display(),
            if skipped > 0 {
                format!(" ({skipped} skipped: not UTF-8 text)")
            } else {
                String::new()
            }
        ));
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
    out.push_str(&blob);
    out.push('\n');
    Ok(out)
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
    // Blocks arrive on stdin, so a file named on the command line wins and the
    // blob's own headers are the fallback -- the same precedence as the `edit`
    // tool's `path` argument. Parse first, because the headers are where the
    // fallback comes from and because a blob may span many files.
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
        let edits: Vec<EditOp> = to_ops(batch.edits.clone());
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

    let mut out = String::new();
    for (path, n) in &written {
        out.push_str(&format!("{path}: {n} block(s) applied\n"));
    }
    for (path, why, echo) in &failed {
        out.push_str(&format!("{path}: FAILED — {why}\n"));
        if !echo.is_empty() {
            out.push_str(&format!(
                "  the {n} block(s) below did not apply to {path}; fix them (or re-preview \
                 with a narrower expression) and re-pipe this file:\n",
                n = echo.len()
            ));
            // Re-render with the *anchored* headers `preview` emits, so the
            // failed blocks can be fixed and re-piped through `apply` exactly as
            // they came out — anchors included, or a duplicate would resolve by
            // content alone on the retry.
            let blocks: Vec<crate::Block> = echo
                .iter()
                .map(|e| crate::Block {
                    anchor: e.start_line.unwrap_or(1) as usize,
                    old_text: e.old_text.clone(),
                    new_text: e.new_text.clone(),
                })
                .collect();
            let blob = crate::blocks::render(path, &blocks, usize::MAX).blob;
            out.push_str(&indent(&blob));
            out.push('\n');
        }
    }
    for p in &problems {
        out.push_str(&format!("{p}\n"));
    }
    if out.is_empty() {
        out.push_str("nothing to do\n");
    }
    if !failed.is_empty() || !problems.is_empty() {
        out.push_str(&format!(
            "{} file(s) applied, {} file(s) failed. Nothing was written for the failures.\n",
            written.len(),
            failed.len()
        ));
        return Err(Failure::Partial(out));
    }
    // Single file, no -i: the result itself is the output, so it can be diffed.
    if !in_place && batches.len() == 1 {
        let path = &batches[0].path;
        let text = std::fs::read_to_string(path)
            .map_err(|e| Failure::Internal(format!("{}: {e}", path)))?;
        let edits: Vec<EditOp> = to_ops(batches[0].edits.clone());
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

/// Indent a rendered blob so it reads as quoted text under its error line.
fn indent(blob: &str) -> String {
    blob.lines()
        .map(|l| format!("    {l}\n"))
        .collect::<Vec<_>>()
        .join("")
}
fn to_ops(edits: Vec<blockedit::ParsedEdit>) -> Vec<EditOp> {
    edits
        .into_iter()
        .map(|e| EditOp {
            old_text: e.old_text,
            new_text: e.new_text,
            start_line: e.start_line.map(blockedit::EditStartLine),
        })
        .collect()
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
