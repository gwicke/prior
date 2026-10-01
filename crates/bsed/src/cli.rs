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
    -p, --path <PATH>           File to preview; a directory is walked
    -r, --recursive             Walk a directory (.gitignore-aware, hidden files
                                skipped) and preview every file, emitting one blob
                                with a header per file. A directory is walked
                                without it; this states the intent.
    -g, --glob <PATTERN>        When walking, only files whose path relative to the
                                root matches. Repeatable, and any match wins. Same
                                semantics as the `find` tool, so use **/*.rs for a
                                recursive match.
    -c, --context <N>           Unchanged lines on each side of a change, like
                                grep -C (default 1)
    -t, --trailing <N>          Extra unchanged lines after, on top of -c
                                (default 0)
        --no-grow               Do not extend blocks to a balanced bracket
    -b, --budget <SIZE>         Payload budget; a tree preview is cut at a file
                                boundary. Accepts units: 32kb, 3mb, 1gib
                                (default 512kb)
    -u, --unified              Emit a unified diff instead of blocks, for reading
                                in a pager. A diff cannot be applied by anything:
                                drop -u for blocks the `edit` tool takes. The
                                hunk widens by -c plus -t
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
    bsed -u 's/old/new/' src/x.rs | less
    bsed -p src 's/old/new/' > blocks.txt
    bsed apply -i < blocks.txt
    bsed apply src/x.rs < blocks.txt | diff src/x.rs -
";

/// Default payload budget for the `bsed` binary: 512 KB.
///
/// Higher than [`crate::DEFAULT_PREVIEW_BUDGET`] on purpose. The library default
/// is sized for a model reading a paste into `edit`; a person running `bsed -r`
/// in a pager wants as much of the tree as will fit on a screen, and silently
/// dropping the rest is the one outcome this tool must not produce quietly.
const DEFAULT_CLI_BUDGET: usize = 512_000;

/// Unchanged lines on each side of a change, when neither flag says otherwise.
///
/// The symmetric reading of "context", and the same one `grep -C` uses.
const DEFAULT_CONTEXT: usize = 1;

/// Which of the two renderings to use, and with how much context.
#[derive(Clone, Copy)]
struct Mode {
    /// A unified diff for a person, rather than blocks for `edit`.
    unified: bool,
    /// Unchanged lines each side of a diff hunk. Only consulted when `unified`.
    ///
    /// A hunk has one context setting rather than a before and an after, so
    /// this is whichever of `-c`/`-t` the caller gave first, and the default when
    /// they gave neither. The default matches the block one, so `-u` shows the
    /// same one-line-each-side window the blocks carry.
    diff_context: usize,
}

/// What a successful run produced, split by where it has to go.
///
/// The split is the whole point: stdout is the *payload* and nothing else, so
/// `bsed ... > blocks.txt` leaves a file with the blocks in it and no
/// commentary, and `bsed ... | less` shows the diff rather than a summary line
/// above it. The banner, the "N of M files changed" line, and every NOTE go to
/// stderr, which a pipe throws away and a terminal shows.
///
/// `apply` is the exception and keeps one stream. Its report is deliberately
/// written as a valid blocks file -- the prose and the blocks interleaved, so it
/// can be edited and re-piped -- so it cannot be torn in two without losing that.
struct Out {
    payload: String,
    status: String,
}

impl Out {
    /// Everything in the payload, nothing in the status. For `apply`, whose
    /// report has to stay one piece.
    fn all(payload: String) -> Out {
        Out {
            payload,
            status: String::new(),
        }
    }
}

/// Entry point shared by the standalone binary and picocode's shim, so both
/// resolve the same arguments the same way.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            // Status first: on a terminal the summary belongs above the blocks,
            // and stdout may be buffered where stderr is not.
            if !out.status.is_empty() {
                eprint!("{}", out.status);
            }
            if !out.payload.is_empty() {
                emit(&out.payload);
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
fn run(args: &[String]) -> Result<Out, Failure> {
    match args.first().map(String::as_str) {
        // No arguments at all: print usage rather than block forever waiting for
        // expressions on stdin, which is what `bsed` on its own used to do.
        None => Ok(Out::all(USAGE.to_string())),
        Some("apply") => apply(&args[1..]),
        Some("-h") | Some("--help") | Some("help") => Ok(Out::all(USAGE.to_string())),
        Some("--version") | Some("-V") => {
            Ok(Out::all(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))))
        }
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

fn preview(args: &[String]) -> Result<Out, Failure> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut recursive = false;
    let mut globs: Vec<String> = Vec::new();
    let mut context: Option<usize> = None;
    let mut trailing: Option<usize> = None;
    let mut grow = true;
    // The CLI's own default is higher than the library's: here the reader is a
    // person at a pager, and a truncated tree preview is the worst outcome.
    let mut budget = DEFAULT_CLI_BUDGET;
    let mut script_parts: Vec<String> = Vec::new();
    let mut only_positional = false;
    let mut saw_e_or_f = false;
    let mut unified = false;

    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        if only_positional {
            // After --, everything is a file
            paths.push(PathBuf::from(a.clone()));
            continue;
        }
        match a.as_str() {
            "--" => only_positional = true,
            "-h" | "--help" => return Ok(Out::all(USAGE.to_string())),
            "--version" | "-V" => {
                return Ok(Out::all(format!("bsed {}\n", env!("CARGO_PKG_VERSION"))))
            }
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
            "-c" | "--context" => context = Some(need_num(it.next(), "-c")?),
            "-t" | "--trailing" => trailing = Some(need_num(it.next(), "-t")?),
            "-b" | "--budget" => {
                budget =
                    crate::script::parse_size(need(it.next(), "-b")?).map_err(Failure::Usage)?;
            }
            "--no-grow" => grow = false,
            "-u" | "--unified" => unified = true,
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

    // `-c` is the symmetric context, the way `grep -C` is: N lines on each
    // side, because that is what a reader means by "show me context". `-t` is
    // then a bonus *on top*, for the case where the interesting continuation is
    // after the change and not before it -- which is the asymmetry worth being
    // able to ask for, but not worth being the default.
    let around = context.unwrap_or(DEFAULT_CONTEXT);
    let ctx = Context {
        leading: around,
        trailing: around + trailing.unwrap_or(0),
        grow,
        growth_cap: 20,
    };
    let mode = Mode {
        unified,
        // A hunk has one symmetric context setting, so `-c` is it and the
        // trailing bonus widens it rather than being dropped: a reader who asked
        // for more room around the change gets it either way.
        diff_context: ctx.trailing,
    };

    // A directory means "walk it". Asking for a tree is what a directory *is* to
    // this tool, the way it is to `grep -r` and `sed`, and making it a flag the
    // caller has to remember meant the common case -- "preview the change across
    // src/" -- was the one that errored. `-r` is still accepted and still means
    // the same thing; it is a statement of intent rather than a requirement.
    //
    // The converse stays an error: `-r` on a file is asking to walk something
    // that is not a directory, and quietly treating it as a single file would
    // hide a typo.
    let any_dir = input_files.iter().any(|p| p.is_dir());
    if any_dir {
        if input_files.len() > 1 || recursive && input_files[0].is_file() {
            return Err(Failure::Usage(format!(
                "a walk takes one directory, but {} paths were given. Name the directory alone.",
                input_files.len()
            )));
        }
        // No `--glob` check here: this *is* the walk, so a filter always applies.
        // The single-file path below is where a filter has nothing to filter.
        return recursive_preview(&input_files[0], &script, ctx, &globs, mode, budget);
    }
    if recursive {
        return Err(Failure::Usage(format!(
            "--recursive needs a directory, and {} is not one.",
            input_files[0].display()
        )));
    }
    if !globs.is_empty() {
        return Err(Failure::Usage("--glob only applies to a directory".into()));
    }

    // Handle single file or multiple files
    if input_files.len() == 1 {
        let p = &input_files[0];
        if p == Path::new("-") {
            let text = read_stdin()?;
            return one_file(&text, "(stdin)", &script, ctx, budget, mode);
        }
        let text = std::fs::read_to_string(p)
            .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
        one_file(&text, &p.display().to_string(), &script, ctx, budget, mode)
    } else {
        // Multiple files: preview each, and keep the two streams apart as we go
        // so the payloads concatenate into one pipeable blob.
        let mut out = Out {
            payload: String::new(),
            status: String::new(),
        };
        for p in &input_files {
            let shown = if p == Path::new("-") {
                let text = read_stdin()?;
                one_file(&text, "(stdin)", &script, ctx, budget, mode)?
            } else {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
                one_file(&text, &p.display().to_string(), &script, ctx, budget, mode)?
            };
            if !out.payload.is_empty() && !shown.payload.is_empty() {
                out.payload.push('\n');
            }
            out.payload.push_str(&shown.payload);
            out.status.push_str(&shown.status);
        }
        Ok(out)
    }
}

/// One file, in whichever of the two output modes was asked for.
///
/// Both modes run the same script through the same engine, so they cannot
/// disagree about what sed would write -- only about how the change is shown.
/// The split exists because the two have different readers: blocks are pasted
/// into `edit`, a diff is read in a pager.
fn one_file(
    text: &str,
    shown: &str,
    expressions: &str,
    ctx: Context,
    budget: usize,
    mode: Mode,
) -> Result<Out, Failure> {
    if mode.unified {
        diff_one(text, shown, expressions, mode.diff_context)
    } else {
        single(text, shown, expressions, ctx, budget)
    }
}

/// One file as a unified diff, for `-u`.
///
/// This is the mode that is *only* read. The output is a real unified diff --
/// `---`/`+++` headers and `@@` hunk markers, the shape `patch` and every diff
/// tool reads -- because the `path:LINE` header that carries the file name in a
/// block has no equivalent outside the block format, and a multi-file run would
/// otherwise have no way to say which file a hunk came from.
///
/// The context is the caller's `-c`/`-t`, and it is the only knob that applies:
/// a diff has no budget, since there are no blocks to drop, and a truncated
/// diff is a diff that lies about what it skipped.
fn diff_one(
    text: &str,
    shown: &str,
    expressions: &str,
    diff_context: usize,
) -> Result<Out, Failure> {
    let (new, n) =
        crate::preview_new(text, shown, expressions).map_err(|e| Failure::User(e.to_string()))?;
    let d = blockedit::udiff::unified(text, &new, shown, diff_context);
    if d.is_empty() {
        return Ok(Out {
            payload: String::new(),
            status: format!(
                "sed diff: no lines matched — {shown} is unchanged by these {} expression{}.\n",
                n,
                if n == 1 { "" } else { "s" }
            ),
        });
    }
    let hunks = d.matches("\n@@").count() + usize::from(d.starts_with("@@"));
    Ok(Out {
        status: format!(
            "sed diff: {shown} — {hunks} hunk{} from {} expression{}. A diff is for reading; \
             drop -u for blocks the `edit` tool can apply.\n",
            if hunks == 1 { "" } else { "s" },
            n,
            if n == 1 { "" } else { "s" }
        ),
        payload: format!("{d}\n"),
    })
}

/// One file, with the agent-facing preview budget applied.
fn single(
    text: &str,
    shown: &str,
    expressions: &str,
    ctx: Context,
    budget: usize,
) -> Result<Out, Failure> {
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
        return Ok(Out {
            status: format!("{}\n", p.banner),
            payload: String::new(),
        });
    }
    // The banner is a status line about the payload, not part of it. It used to
    // lead the blob, which the `edit` tool's parser tolerates -- a header is a
    // whitespace-free token and a prose line is not -- but it meant
    // `bsed ... > blocks.txt` wrote a file that opened with a sentence.
    let mut payload = p.blob;
    // `render` strips the blob's trailing newline so the payload round-trips as
    // typed; put one back, since a text file that ends mid-line is its own
    // surprise for whatever reads it next.
    payload.push('\n');
    Ok(Out {
        status: format!("{}\n", p.banner),
        payload,
    })
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
    mode: Mode,
    budget: usize,
) -> Result<Out, Failure> {
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
    // The walk does not stop when the budget runs out. Every file is read, run
    // and diffed regardless -- that work is already done by the time anything is
    // emitted -- so the *total* is free, and a total is what makes a partial
    // result legible: "returned 12 of 140" tells a reader there is more to come,
    // where a bare "12" does not. These are what got left out.
    let mut emitted_files = 0usize;
    let mut emitted_blocks = 0usize;
    let mut skipped = 0usize;
    let mut blocked: Vec<(String, String)> = Vec::new();
    // Kept apart from `blocked` because the advice differs. "Re-run on that file
    // alone" is what rescues a block that cannot be expressed; a CRLF file is
    // refused either way, so telling the reader to retry it would be a loop.
    let mut crlf: Vec<String> = Vec::new();

    // Add one file's worth of payload, unless it would not fit. A file is kept
    // whole or left out: half a file's blocks is a payload that neither applies
    // nor describes what it left out, and a boundary at the file is the one place
    // a reader can see where a batch stopped.
    let fits = |blob: &mut String, piece: &str, room: &mut isize| -> bool {
        let cost = piece.len() as isize + usize::from(!blob.is_empty()) as isize;
        if cost > *room {
            return false;
        }
        if !blob.is_empty() {
            blob.push('\n');
        }
        blob.push_str(piece);
        *room -= cost;
        true
    };
    let mut room = budget as isize;

    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            skipped += 1; // binary or unreadable
            continue;
        };
        let shown = f.display().to_string();
        if mode.unified {
            // A diff is built straight from the two versions of the file, so
            // there is no block budget and no "could not be expressed" case to
            // report: a file either differs or it does not.
            let (new, _) = match crate::preview_new(&text, &shown, expressions) {
                Ok(v) => v,
                Err(crate::Error::CarriageReturn(_)) => {
                    crlf.push(shown);
                    continue;
                }
                Err(e) => return Err(Failure::User(e.to_string())),
            };
            let d = blockedit::udiff::unified(&text, &new, &shown, mode.diff_context);
            if d.is_empty() {
                continue;
            }
            changed_files += 1;
            let piece = format!("{d}\n");
            let _ = fits(&mut blob, &piece, &mut room);
            continue;
        }
        let p = match crate::preview_text(&text, &shown, expressions, opts) {
            Ok(p) => p,
            // One refused file must not sink the tree, but it must be named:
            // silence here would read as "that file needed no change".
            Err(crate::Error::CarriageReturn(_)) => {
                crlf.push(shown);
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
            blocked.push((shown, why));
            continue;
        }
        let b = p.blob;
        changed_files += 1;
        blocks += count_blocks(&b);
        // The first changed file always gets through, whatever the budget. A
        // budget too small for the first file would otherwise return nothing and
        // say "re-run for the next batch" -- which returns nothing again, since
        // the budget is the same. Iterative coverage depends on every call
        // making progress, so the budget bounds everything after the first file
        // and never the first file itself. It matches the single-file rule that
        // one block is emitted whatever it costs.
        if emitted_files == 0 {
            // The first file goes in whatever it costs, but it still has to come
            // *out* of the budget: leaving `room` whole would let the second
            // file fit as though the first were free, and the cut would never
            // happen at all.
            if !blob.is_empty() {
                blob.push('\n');
            }
            blob.push_str(&b);
            room -= b.len() as isize + 1;
        } else if !fits(&mut blob, &b, &mut room) {
            continue;
        }
        emitted_files += 1;
        emitted_blocks += count_blocks(&b);
    }

    // The denominator of every count below. A file that cannot be batched --
    // skipped, CRLF, or unexpressible -- is not a file waiting for a later
    // batch, it is a file that will never be previewed at all, so counting it
    // here would put a permanent exclusion among the transient ones. "1 of 2
    // files changed" reads as half-done-and-there's-more; what it means is
    // that one file is finished and the other is stuck. The refused files get
    // their own notes, which say *which* files and what to do about them, so
    // this count only ever describes the population that can actually batch.
    let considered = files.len() - skipped - crlf.len() - blocked.len();

    if changed_files == 0 {
        let mut out = format!(
            "no lines matched — none of the {considered} file(s) under {} changed\n",
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
                 NOT in the output. Re-run on those files alone:\n",
                blocked.len()
            ));
            push_list(
                &mut out,
                blocked.iter().map(|(p, why)| format!("{p}: {why}")),
            );
        }
        // "no lines matched" is the wrong sentence when a file did match and was
        // refused, so the CRLF files are spelled out rather than just counted.
        if !crlf.is_empty() {
            out.push_str(&crlf_report(&crlf));
        }
        // Nothing to preview, so there is no payload and stdout stays empty --
        // a reader is left with the explanation on stderr and no file on disk
        // that looks like it was written. Note that a run with nothing to show
        // still exits 0: the expression was fine, the file simply matched
        // nothing, which is an answer rather than a failure.
        return Ok(Out {
            payload: String::new(),
            status: out,
        });
    }

    let noun = if mode.unified { "diff" } else { "preview" };
    // `changed_files == 0` returned above, so from here there is something to
    // describe and the only question is whether all of it fit.
    let mut out = if emitted_blocks == blocks {
        // Everything fit, so there is no batch to describe and nothing to warn
        // about. This is the common case and it should read like the common case.
        if mode.unified {
            format!(
                "sed diff: {changed_files} of {} file(s) under {} changed. A diff is for \
                 reading; drop -u for blocks `edit` can apply.\n",
                considered,
                root.display(),
            )
        } else {
            format!(
                "sed preview: {blocks} block(s) in {changed_files} of {} file(s) under {}. \
                 Check and apply them with `edit`.\n",
                considered,
                root.display(),
            )
        }
    } else {
        // Partial. The two numbers are the whole message: how much of the work
        // this call carries, and how much is left, so a reader can tell a batch
        // from the end of a refactor without counting anything.
        format!(
            "sed {noun}: {blocks} match(es) in {changed_files} of {} file(s) under {}; \
             returned {emitted_blocks} of {blocks} in {emitted_files} file(s). This is ONE \
             batch. Check and apply it with `edit`, then re-run sed for the next.\n",
            considered,
            root.display(),
        )
    };
    if skipped > 0 {
        out.push_str(&format!(
            "NOTE: {skipped} file(s) skipped (not UTF-8 text).\n"
        ));
    }
    // Nothing can be "unexpressible as blocks" in a mode that emits none.
    if !mode.unified && !blocked.is_empty() {
        out.push_str(&format!(
            "NOTE: {} file(s) matched but cannot be expressed as appliable blocks, and are \
             NOT in the output:\n",
            blocked.len()
        ));
        push_list(
            &mut out,
            blocked.iter().map(|(p, why)| format!("{p}: {why}")),
        );
    }
    if !crlf.is_empty() {
        out.push_str(&crlf_report(&crlf));
    }
    // The summary and every NOTE are about the blob, so they go with the
    // terminal rather than into it. `bsed -r ... > blocks.txt` now writes blocks.
    let mut payload = blob;
    payload.push('\n');
    Ok(Out {
        payload,
        status: out,
    })
}

/// How many filenames a refusal note names before it counts the rest.
const MAX_LISTED: usize = 10;

/// Append at most [`MAX_LISTED`] indented entries, then a count of the remainder.
///
/// A tree can refuse hundreds of files for one reason, and naming every one of
/// them costs more of a reader's attention than it returns. The count is what
/// tells a reader how big the problem is and the first few are enough to show
/// its shape; the rest are recoverable by looking at the tree, which is what a
/// reader is going to do with the count anyway.
fn push_list(out: &mut String, items: impl Iterator<Item = String>) {
    let mut items = items.peekable();
    let mut n = 0usize;
    while n < MAX_LISTED {
        let Some(item) = items.next() else { return };
        out.push_str("  ");
        out.push_str(&item);
        out.push('\n');
        n += 1;
    }
    let rest = items.count();
    if rest > 0 {
        out.push_str(&format!("  ... and {rest} more\n"));
    }
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
    push_list(&mut s, paths.iter().map(|p| p.to_string()));
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

fn apply(args: &[String]) -> Result<Out, Failure> {
    let mut path: Option<PathBuf> = None;
    let mut in_place = false;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Out::all(USAGE.to_string())),
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
            return Ok(Out::all(text));
        }
    }
    // `apply` keeps one stream on purpose: its report is written as a valid
    // blocks file so it can be edited and re-piped, and tearing the prose off
    // would stop the blocks from parsing on their own.
    Ok(Out::all(out))
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
