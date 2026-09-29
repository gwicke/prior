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

use blockedit::{apply::ApplyOutcome, EditOp};

use crate::{Context, Options};

const USAGE: &str = "\
bsed — block-edit sed

USAGE:
    bsed preview [OPTIONS] <expressions>
    bsed apply   [OPTIONS] [FILE]

PREVIEW (never writes; the default):
    -p, --path <PATH>     File to preview (default: - for stdin)
    -c, --context <N>     Unchanged lines each side; trailing is always at
                          least 2 (default 0)
    -t, --trailing <N>    Minimum unchanged lines after a change (default 2)
        --no-grow         Do not extend blocks to a balanced bracket
    -b, --budget <BYTES>  Preview budget (default 51200)
    -h, --help

APPLY (writes):
    -p, --path <PATH>     File to edit (default: the FILE argument, else -)
    -i, --in-place        Write the file; without it the result goes to stdout

EXAMPLES:
    bsed preview '177s/edits: EditInput,/edits: serde_json::Value,/' -p src/x.rs
    bsed preview 's/foo/bar/g' < expressions.txt -p src/x.rs
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

    // `-p -` (or no `-p` at all) means the file arrives on stdin, which is the
    // pipeline form: `cat f.rs | bsed preview 's/x/y/'`.
    let (text, shown) = if path.as_deref().map(|p| p.as_os_str()) == Some(std::ffi::OsStr::new("-"))
        || path.is_none()
    {
        (read_stdin()?, "(stdin)".to_string())
    } else {
        let p = path.as_ref().expect("checked above");
        let text = std::fs::read_to_string(p)
            .map_err(|e| Failure::Internal(format!("{}: {e}", p.display())))?;
        (text, p.display().to_string())
    };

    let opts = Options {
        context: Context {
            leading: context,
            trailing: trailing.unwrap_or(2),
            grow,
            growth_cap: 20,
        },
        max_preview_bytes: budget,
    };
    let (banner, blob) = crate::preview_text(&text, &shown, &expressions, opts)
        .map_err(|e| Failure::User(e.to_string()))?;
    if blob.is_empty() {
        return Ok(format!("{banner}\n"));
    }
    Ok(format!("{banner}\n{blob}\n"))
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
    // blob's own header is the fallback -- the same precedence as the `edit`
    // tool's `path` argument. Parse first, because the header is where the
    // fallback comes from.
    let named = match path {
        Some(p) if p.as_os_str().is_empty() || p == PathBuf::from("-") => None,
        Some(p) => Some(p),
        None => None,
    };
    let blob = read_stdin()?;
    let parsed = blockedit::parse_blocks(&blob).map_err(|e| Failure::User(e.to_string()))?;

    let resolved: PathBuf = match (named, parsed.path.clone()) {
        (Some(p), Some(h)) if same_file(&p, &h) => p,
        (Some(p), _) => p,
        (None, Some(h)) => PathBuf::from(h),
        (None, None) => {
            return Err(Failure::User(
                "no file to apply to: pass a path, or include a filename line above the first \
                 @@@OLD marker"
                    .into(),
            ))
        }
    };
    let edits: Vec<EditOp> = to_ops(parsed.edits);
    let text = std::fs::read_to_string(&resolved)
        .map_err(|e| Failure::Internal(format!("{}: {e}", resolved.display())))?;

    let applied = match blockedit::apply_to_text(&text, &edits) {
        ApplyOutcome::Applied { text, .. } => text,
        ApplyOutcome::Failed { report } => return Err(Failure::User(report)),
    };

    if in_place {
        std::fs::write(&resolved, &applied)
            .map_err(|e| Failure::Internal(format!("{}: {e}", resolved.display())))?;
        Ok(format!(
            "{}\n",
            blockedit::success_note(&edits, &[], &resolved.display().to_string())
        ))
    } else {
        Ok(applied)
    }
}

/// Compare a command-line path with a header path the way the blob parser
/// does, so `./src/x.rs` and `src/x.rs` agree.
fn same_file(a: &Path, b: &str) -> bool {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return a == Path::new(b),
    };
    blockedit::path::resolve_normalized(&a.to_string_lossy(), &cwd)
        == blockedit::path::resolve_normalized(b, &cwd)
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
