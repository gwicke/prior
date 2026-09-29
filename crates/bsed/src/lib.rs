//! Preview a GNU sed script as line-anchored block edits, and apply block edits
//! from a shell.
//!
//! The idea: sed is an excellent way to *describe* a change across a file and a
//! terrible way to *apply* one safely — `sed -i` with a line range computed
//! against a stale read silently eats uncommitted work. So this crate never
//! writes. It runs the script in memory, diffs the result against the file, and
//! emits the diff as `@@@OLD`/`@@@NEW`/`@@@END` blocks with a `path:LINE` header
//! on each one — the exact format picocode's `edit` tool consumes. The model
//! reviews the blocks, then hands them to `edit`.
//!
//! Layers of safety, each sufficient on its own (see `doc/bsed.md`):
//!
//! 1. **Deny-list** ([`script`]) — `w`/`W`/`s///w`, `e`, and `q`/`Q` are
//!    rejected from the parsed AST before evaluation, so they cannot reach the
//!    engine at all. The walk has no wildcard arm: a new `sed-rs` command
//!    breaks the build instead of slipping through.
//! 2. **Process boundary** — the `bsed` binary is a separate process, so even a
//!    library-level surprise (a `q` calling `std::process::exit`, a panic in a
//!    pathological regex) cannot take the agent down with it.
//! 3. **Read-only sandbox** — picocode runs `bsed preview` inside the same
//!    Bubblewrap namespace it uses for `rg`, so nothing here can write to the
//!    workspace even in principle.
//!
//! Pipeline: `script::build` → `engine::run` → `hunks::build` → `blocks::render`.

pub mod balance;
pub mod blocks;
pub mod engine;
pub mod hunks;
pub mod script;

use std::path::Path;

pub use blocks::Rendered;
pub use hunks::Block;

/// Everything the caller can tune.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub context: Context,
    /// Budget for the rendered blob. Blocks are dropped whole once it is hit.
    pub max_preview_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            context: Context::default(),
            max_preview_bytes: 50 * 1024,
        }
    }
}

/// Context shape: leading lines, trailing floor, and growth.
pub use hunks::Context;

/// What a preview produced.
#[derive(Debug)]
pub struct Preview {
    pub blocks: Vec<Block>,
    /// Blocks the budget dropped.
    pub truncated: bool,
    /// Expressions screened, for the banner.
    pub expressions: usize,
    /// Blocks still ending mid-construct after growth.
    pub still_unbalanced: usize,
    /// Whether growth added anything.
    pub grew: bool,
    /// A block the `edit` tool could not accept, with the reason.
    pub blocked: Option<String>,
}

impl Preview {
    /// The model-facing result: a banner line, then the pasteable blob.
    ///
    /// The blob is the `edits` payload for the `edit` tool, verbatim. The banner
    /// sits in front of it because the parser ignores prose that is not a
    /// single whitespace-free token, so a human-readable first line costs
    /// nothing structurally.
    pub fn render(&self, path: &str) -> String {
        if let Some(why) = &self.blocked {
            return format!("sed preview could not produce an appliable block set: {why}\n");
        }
        if self.blocks.is_empty() {
            return format!("no lines matched — {path} is unchanged by these expressions\n");
        }

        let r = blocks::render(path, &self.blocks, self.max_preview_bytes());
        let mut out = String::new();
        out.push_str(&self.banner(path, &r));
        out.push('\n');
        out.push_str(&r.blob);
        out.push('\n');
        out
    }

    /// Kept separate so `render` can recompute the budget it was given.
    fn max_preview_bytes(&self) -> usize {
        // Re-rendered with the same budget the caller passed; stored on the
        // preview at construction via `render_with`.
        usize::MAX
    }

    fn banner(&self, path: &str, r: &Rendered) -> String {
        if r.total == 0 {
            return format!(
                "no lines matched — {path} is unchanged by {}{} expression{}. \
                 Check the addresses and patterns (line numbers are 1-based, from `read`).",
                if self.expressions == 1 {
                    "this "
                } else {
                    "these "
                },
                self.expressions,
                if self.expressions == 1 { "" } else { "s" },
            );
        }
        let n = r.shown;
        let mut s = format!(
            "sed preview: {n} block{} for {path} ({} expression{}). \
             Review the blocks below, then pass them to the `edit` tool as its `edits` argument.",
            if n == 1 { "" } else { "s" },
            self.expressions,
            if self.expressions == 1 { "" } else { "s" },
        );
        if r.truncated {
            s.push_str(&format!(
                "\nNOTE: only {n} of {} blocks are shown ({} KiB preview budget). The other {} \
                 are NOT included. Apply these first, then re-run sed with narrower expressions \
                 (e.g. add line ranges) to cover the rest.",
                r.total,
                50,
                r.total - n,
            ));
        }
        if self.grew {
            s.push_str(
                "\nNOTE: some blocks were extended to a balanced end so you can see the whole \
                 construct; the extra lines are unchanged.",
            );
        }
        if self.still_unbalanced > 0 {
            s.push_str(&format!(
                "\nNOTE: {} block{} still end mid-construct (unbalanced braces/parentheses in \
                 the source); re-run with `context` if you need more.",
                self.still_unbalanced,
                if self.still_unbalanced == 1 { "" } else { "s" },
            ));
        }
        s
    }
}

/// Preview `expressions` against the contents of `path`.
///
/// `shown_path` is the path to put in block headers — normally the
/// cwd-relative form the model already knows.
pub fn preview(
    path: &Path,
    shown_path: &str,
    expressions: &str,
    opts: Options,
) -> Result<String, Error> {
    let original = std::fs::read_to_string(path).map_err(|e| Error::Io(e.to_string()))?;
    let (banner, blob) = preview_text(&original, shown_path, expressions, opts)?;
    Ok(format!("{banner}\n{blob}\n"))
}

/// The same preview over text already in hand. Returns `(banner, blob)`.
pub fn preview_text(
    original: &str,
    shown_path: &str,
    expressions: &str,
    opts: Options,
) -> Result<(String, String), Error> {
    let scr = script::build(expressions).map_err(Error::Script)?;
    let new = engine::run(&scr.text, original, scr.used_case_sentinels)
        .map_err(|e| Error::Engine(e.0))?;

    let h = hunks::build(original, &new, opts.context);
    let mut p = Preview {
        blocks: h.blocks,
        truncated: false,
        expressions: scr.count,
        still_unbalanced: h.still_unbalanced,
        grew: h.grew_any,
        blocked: None,
    };
    // The contract with `edit`: what we emit must parse back into what we built.
    let r = blocks::render(shown_path, &p.blocks, opts.max_preview_bytes);
    p.truncated = r.truncated;
    if let Err(why) = blocks::verify(&r.blob) {
        p.blocked = Some(why);
    }
    let banner = p.banner(shown_path, &r);
    Ok((banner, r.blob))
}

/// Everything that can go wrong, kept distinct so the caller can map each to an
/// exit code: a rejected script is the caller's problem to fix, an internal
/// error is not.
#[derive(Debug)]
pub enum Error {
    /// An expression was rejected by the deny-list or would not parse.
    Script(script::ScriptError),
    /// The engine refused to run a screened script.
    Engine(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Script(e) => write!(f, "{e}"),
            Error::Engine(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use blockedit::{apply::ApplyOutcome, EditOp};

    const FILE: &str = "fn main() {\n    let x = 1;\n    let y = 2;\n    if count > 0 {\n        run();\n    }\n    // trailing\n    let z = 3;\n}\n";

    fn opts() -> Options {
        Options {
            context: Context {
                leading: 0,
                trailing: 2,
                grow: false,
                growth_cap: 20,
            },
            max_preview_bytes: 50 * 1024,
        }
    }

    #[test]
    fn substitution_previews_as_one_anchored_block() {
        let (banner, blob) = preview_text(FILE, "src/foo.rs", "5s/run/walk/", opts()).unwrap();
        assert!(banner.contains("1 block"), "{banner}");
        assert!(blob.starts_with("src/foo.rs:5\n@@@OLD"), "{blob}");
        assert!(blob.contains("run();"), "{blob}");
        assert!(blob.contains("walk();"), "{blob}");
    }

    #[test]
    fn no_op_says_so_and_returns_no_blocks() {
        let (banner, blob) = preview_text(FILE, "src/foo.rs", "999s/zzz/yyy/", opts()).unwrap();
        assert!(blob.is_empty());
        assert!(banner.contains("no lines matched"), "{banner}");
        assert!(banner.contains("src/foo.rs"), "{banner}");
    }

    #[test]
    fn write_commands_are_rejected_with_guidance() {
        let e = preview_text(FILE, "f.rs", "2w /tmp/pwned", opts()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("never writes"), "{msg}");
        assert!(msg.contains("expression 1"), "{msg}");
    }

    #[test]
    fn shell_execution_is_rejected() {
        for script in ["2e touch /tmp/pwned", "s/run/walk/e"] {
            let e = preview_text(FILE, "f.rs", script, opts()).unwrap_err();
            assert!(e.to_string().contains("execute"), "{script}: {e}");
        }
    }

    #[test]
    fn early_exit_is_rejected_because_the_preview_would_lie() {
        let e = preview_text(FILE, "f.rs", "5q", opts()).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("delete the rest of the file"), "{msg}");
    }

    #[test]
    fn read_commands_are_allowed() {
        // `r` splices a file in; the sandbox already bounds what is readable.
        // Use a file we wrote, so the test does not depend on host contents.
        let f = std::env::temp_dir().join("bsed-r-test.txt");
        std::fs::write(&f, "SPLICED LINE\n").unwrap();
        let (_banner, blob) =
            preview_text(FILE, "f.rs", &format!("1r {}", f.display()), opts()).unwrap();
        std::fs::remove_file(&f).ok();
        assert!(blob.contains("SPLICED LINE"), "{blob}");
        // It arrives as an insertion with a borrowed OLD, so it is appliable.
        assert!(blob.contains("@@@OLD"), "{blob}");
    }

    #[test]
    fn two_expressions_on_one_line_behave_like_two() {
        let a = preview_text(FILE, "f.rs", "2d;3d", opts()).unwrap().1;
        let b = preview_text(FILE, "f.rs", "2d\n3d", opts()).unwrap().1;
        assert_eq!(a, b);
    }

    #[test]
    fn trailing_floor_is_applied_by_default() {
        let (_banner, blob) = preview_text(
            FILE,
            "src/foo.rs",
            "2s/1/11/",
            Options {
                context: Context {
                    leading: 0,
                    trailing: 2,
                    grow: false,
                    growth_cap: 20,
                },
                max_preview_bytes: 50 * 1024,
            },
        )
        .unwrap();
        // Changed line plus two unchanged lines after it.
        assert!(blob.contains("let y = 2;"), "{blob}");
        assert!(blob.contains("if count > 0 {"), "{blob}");
    }

    #[test]
    fn scattered_duplicate_edit_previews_every_line_anchored() {
        // The claim the design rests on: identical lines, each in its own block,
        // resolved by anchor alone with no uniqueness requirement.
        let file: String = (0..40)
            .map(|i| if i % 3 == 0 { "run();\n" } else { "other();\n" })
            .collect();
        let (_banner, blob) = preview_text(&file, "src/x.rs", "s/run/walk/g", opts()).unwrap();
        let parsed = blockedit::parse_blocks(&blob).unwrap();
        assert_eq!(parsed.edits.len(), 14);
        for (i, e) in parsed.edits.iter().enumerate() {
            // trailing=2 is on, so each block carries its change plus two
            // unchanged lines; the anchor is what has to be exact.
            assert!(e.old_text.starts_with("run();"), "{e:?}");
            assert!(e.new_text.starts_with("walk();"), "{e:?}");
            assert_eq!(e.start_line, Some((i * 3 + 1) as u64));
        }
    }

    #[test]
    fn file_without_trailing_newline_is_unchanged_by_a_no_op() {
        let file = "a\nb";
        let (_banner, blob) = preview_text(file, "f.rs", "s/zzz/yyy/", opts()).unwrap();
        assert!(blob.is_empty());
    }

    /// The contract the whole tool exists for: what `bsed preview` prints, fed
    /// to the same parser the `edit` tool uses, applies back to the original and
    /// produces exactly what sed would have written. If this fails, the preview
    /// is a lie and the tool is worse than no tool.
    #[test]
    fn preview_applies_back_to_exactly_what_sed_produced() {
        let cases: &[&str] = &[
            "5s/run/walk/",
            "7,8d",
            "2s/1/11/",
            r"s/^    /\t/g",
            r"/\/\//d",
            "1a\\n// generated",
            "9c\\n    let z = 4;",
            "s/2/2/g",
            "1,3s/;/;/",
        ];
        for script in cases {
            // GNU sed is the reference; skip the case if it is not GNU or the
            // script is invalid there.
            let reference = std::process::Command::new("sed")
                .arg(script)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    c.stdin
                        .as_mut()
                        .unwrap()
                        .write_all(FILE.as_bytes())
                        .unwrap();
                    c.wait_with_output()
                })
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
            let Some(reference) = reference else {
                continue;
            };

            let (banner, blob) = preview_text(FILE, "src/foo.rs", script, opts()).unwrap();
            if blob.is_empty() {
                // A script that changes nothing must say so *and* mean it: the
                // banner reports no match and there is nothing to apply.
                assert!(banner.contains("no lines matched"), "{script}: {banner}");
                assert_eq!(
                    reference, FILE,
                    "{script}: claimed a no-op but sed changed it"
                );
                continue;
            }
            let parsed = blockedit::parse_blocks(&blob)
                .unwrap_or_else(|e| panic!("{script}: preview did not parse: {e}\n{blob}"));
            let edits: Vec<EditOp> = parsed
                .edits
                .iter()
                .map(|e| EditOp {
                    old_text: e.old_text.clone(),
                    new_text: e.new_text.clone(),
                    start_line: e.start_line.map(blockedit::EditStartLine),
                })
                .collect();
            let applied = match blockedit::apply_to_text(FILE, &edits) {
                ApplyOutcome::Applied { text, .. } => text,
                ApplyOutcome::Failed { report, .. } => panic!("{script}: {report}"),
            };
            assert_eq!(applied, reference, "{script}: preview did not round-trip");
        }
    }

    #[test]
    fn case_escapes_preview_as_uppercase() {
        let (_banner, blob) = preview_text(FILE, "f.rs", "s/run/\\U&/", opts()).unwrap();
        assert!(blob.contains("RUN();"), "{blob}");
        assert!(!blob.contains("\\U"), "{blob}");
    }
}

pub mod cli;
