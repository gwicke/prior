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
pub use script::ChangeRange;

/// Everything the caller can tune.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub context: Context,
    /// Budget for the rendered blob. Blocks are dropped whole once it is hit.
    pub max_preview_bytes: usize,
}

/// Default preview budget for a library caller: 32 KB.
///
/// Small on purpose. The payload is a paste into the `edit` tool, and its cost is
/// paid in a context window the caller is sharing with a conversation, so the
/// default is set for the model that has to read it rather than for how much of
/// a refactor it can cover. A caller that knows its reader wants more passes a
/// bigger budget; the `bsed` binary's own default is higher, because there the
/// reader is a person in a pager.
pub const DEFAULT_PREVIEW_BUDGET: usize = 32_000;

impl Default for Options {
    fn default() -> Self {
        Options {
            context: Context::default(),
            max_preview_bytes: DEFAULT_PREVIEW_BUDGET,
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
             Review the blocks below, then pass them to the `edit` tool as its `blocks` argument.",
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
    let p = preview_text(&original, shown_path, expressions, opts)?;
    if let Some(why) = p.blocked {
        return Err(Error::Blocked(why));
    }
    if p.blob.is_empty() {
        return Ok(format!("{}\n", p.banner));
    }
    Ok(format!("{}\n{}\n", p.banner, p.blob))
}

/// A preview of one file's worth of text.
#[derive(Debug, Clone)]
pub struct TextPreview {
    /// The one-line summary (plus any notes), for a human.
    pub banner: String,
    /// The block blob: the `edits` payload, verbatim.
    pub blob: String,
    /// Set when the diff produced blocks the `edit` tool cannot accept — today
    /// only when a block would contain a line that is exactly a block marker,
    /// which would be read as structure instead of content.
    ///
    /// The blob is still returned, because seeing the problem is more useful
    /// than an error, but callers must not present it as appliable.
    pub blocked: Option<String>,
}

/// The text `expressions` turns `original` into, with the screened script.
///
/// The first half of a preview, and on its own enough for `--unified`: a diff
/// needs the two versions of the file, not the blocks built between them.
fn sed_output(
    original: &str,
    shown_path: &str,
    expressions: &str,
) -> Result<(script::Script, String), Error> {
    if has_carriage_return(original) {
        return Err(Error::CarriageReturn(shown_path.to_string()));
    }
    let scr = script::build(expressions).map_err(Error::Script)?;
    let new = engine::run(
        &scr.text,
        original,
        scr.used_case_sentinels,
        &scr.change_ranges,
    )
    .map_err(|e| Error::Engine(e.0))?;
    Ok((scr, new))
}

/// `expressions` applied to `original`, for a caller that wants to diff the
/// result rather than receive blocks.
///
/// Screening, the CRLF refusal, and the engine are the same path a block
/// preview takes; only the rendering afterwards differs, so a `-u` run and a
/// block run of the same script cannot disagree about what sed would write.
pub fn preview_new(
    original: &str,
    shown_path: &str,
    expressions: &str,
) -> Result<(String, usize), Error> {
    let (scr, new) = sed_output(original, shown_path, expressions)?;
    Ok((new, scr.count))
}

/// The same preview over text already in hand.
pub fn preview_text(
    original: &str,
    shown_path: &str,
    expressions: &str,
    opts: Options,
) -> Result<TextPreview, Error> {
    let (scr, new) = sed_output(original, shown_path, expressions)?;

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
    if let Err(why) = blocks::verify(&p.blocks, &r.blob) {
        p.blocked = Some(why);
    }
    let banner = p.banner(shown_path, &r);
    Ok(TextPreview {
        banner,
        blob: r.blob,
        blocked: p.blocked,
    })
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
    /// The diff is real but cannot be expressed as an appliable block set.
    Blocked(String),
    /// The file has carriage returns this tool cannot represent faithfully.
    /// Carries the path to name in the message.
    CarriageReturn(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Script(e) => write!(f, "{e}"),
            Error::Engine(e) => write!(f, "{e}"),
            Error::Blocked(e) => write!(f, "{e}"),
            Error::CarriageReturn(path) => write!(
                f,
                "{path} has carriage returns this tool cannot carry. A CRLF file loses its \
                 line endings before the change is worked out, so the preview would describe a \
                 different file and no block would apply back. Convert the file to LF and try \
                 again, or edit it with a tool that preserves line endings."
            ),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Whether `text` has a carriage return this tool cannot represent.
///
/// Two positions matter, and they fail differently:
///
/// * Before a newline — a CRLF line ending. The engine strips the `\r` as it
///   reads, so a CRLF file reaches the diff as a different file and every line
///   looks edited. This is the case worth catching early, since the resulting
///   blocks apply to nothing.
/// * At the very end, with no newline to pair it. The block format drops a `\r
///   that sits directly before a line break, and the renderer always ends a
///   block with one, so a block whose text ends in `\r` cannot round-trip.
///
/// A `\r` anywhere else is ordinary content: `str::lines()` and the block
/// format both keep it, and the preview is exact.
fn has_carriage_return(text: &str) -> bool {
    text.as_bytes().windows(2).any(|w| w == b"\r\n") || text.ends_with('\r')
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
        let TextPreview { banner, blob, .. } =
            preview_text(FILE, "src/foo.rs", "5s/run/walk/", opts()).unwrap();
        assert!(banner.contains("1 block"), "{banner}");
        assert!(blob.starts_with("src/foo.rs:5\n@@@OLD"), "{blob}");
        assert!(blob.contains("run();"), "{blob}");
        assert!(blob.contains("walk();"), "{blob}");
    }

    #[test]
    fn no_op_says_so_and_returns_no_blocks() {
        let TextPreview { banner, blob, .. } =
            preview_text(FILE, "src/foo.rs", "999s/zzz/yyy/", opts()).unwrap();
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

    /// A CRLF file is refused rather than previewed. The engine strips the `\r`
    /// as it reads, so what reached the diff was a different file: every line
    /// looked edited, and the blocks that came back named `old_text` in `\n`
    /// that appears nowhere in the file. They applied to nothing.
    #[test]
    fn crlf_is_refused_rather_than_mis_previewed() {
        let e = preview_text("a\r\nb\r\n", "crlf.txt", "s/b/B/", opts()).unwrap_err();
        assert!(
            matches!(e, Error::CarriageReturn(ref p) if p == "crlf.txt"),
            "{e:?}"
        );
        let msg = e.to_string();
        assert!(msg.contains("crlf.txt"), "{msg}");
        assert!(msg.contains("Convert the file to LF"), "{msg}");
    }

    /// A mixed-ending file is the same failure partway through, and no more
    /// previewable than a wholly-CRLF one.
    #[test]
    fn mixed_line_endings_are_refused() {
        assert!(preview_text("a\r\nb\n", "f.rs", "s/b/B/", opts()).is_err());
    }

    /// A `\r` at the very end, with no newline to pair it, cannot round-trip
    /// through the block format either — the renderer ends a block with a
    /// newline, and the parser drops a `\r` that sits before one.
    #[test]
    fn a_trailing_carriage_return_is_refused() {
        let e = preview_text("a\nb\r", "f.rs", "s/a/A/", opts()).unwrap_err();
        assert!(matches!(e, Error::CarriageReturn(_)), "{e:?}");
    }

    /// A `\r` anywhere else is ordinary content: both the line splitter and the
    /// block format keep it, so the preview is exact and must not be refused.
    /// The line it sits on is changed here so the block carries it, which is
    /// where it would have to survive.
    #[test]
    fn a_mid_line_carriage_return_is_content_not_an_ending() {
        let TextPreview { blob, blocked, .. } =
            preview_text("a\rb\nc\n", "f.rs", "s/a/X/", opts()).unwrap();
        assert!(blocked.is_none(), "{blocked:?}");
        assert!(blob.contains("a\rb"), "{blob:?}");
        assert!(blob.contains("X\rb"), "{blob:?}");
    }

    /// The same file, once converted, previews as it always should — the check
    /// is about the bytes, not the file's name.
    #[test]
    fn the_lf_equivalent_of_a_crlf_file_previews_normally() {
        assert!(preview_text("a\nb\n", "f.rs", "s/b/B/", opts()).is_ok());
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
        let TextPreview { blob, .. } =
            preview_text(FILE, "f.rs", &format!("1r {}", f.display()), opts()).unwrap();
        std::fs::remove_file(&f).ok();
        assert!(blob.contains("SPLICED LINE"), "{blob}");
        // It arrives as an insertion with a borrowed OLD, so it is appliable.
        assert!(blob.contains("@@@OLD"), "{blob}");
    }

    #[test]
    fn two_expressions_on_one_line_behave_like_two() {
        let a = preview_text(FILE, "f.rs", "2d;3d", opts()).unwrap().blob;
        let b = preview_text(FILE, "f.rs", "2d\n3d", opts()).unwrap().blob;
        assert_eq!(a, b);
    }

    #[test]
    fn trailing_floor_is_applied_by_default() {
        let TextPreview { blob, .. } = preview_text(
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
        let TextPreview { blob, .. } =
            preview_text(&file, "src/x.rs", "s/run/walk/g", opts()).unwrap();
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

    /// A file with no final newline, against a script that does not end in one
    /// either. GNU's rule is subtle enough to be worth pinning end to end: the
    /// terminator is absent only while the input's last line is still the
    /// output's last line.
    #[test]
    fn an_unterminated_file_matches_gnu_across_its_cases() {
        let before = "a\nb\nc";
        for (script, after) in [
            ("s/zzz/yyy/", "a\nb\nc"),    // no-op: still the same last line
            ("s/c/z/", "a\nb\nz"),        // rewritten in place
            ("1d", "b\nc"),               // moved up, still the last line
            ("2i\\X", "a\nX\nb\nc"),      // pushed up
            ("$d", "a\nb\n"),             // gone, so GNU re-terminates
            ("$a app", "a\nb\nc\napp\n"), // something now follows it
        ] {
            let p = preview_text(before, "f.txt", script, opts()).unwrap();
            let (banner, blob) = (p.banner, p.blob);
            if blob.is_empty() {
                assert_eq!(after, before, "{script}: {banner}");
                continue;
            }
            let parsed =
                blockedit::parse_blocks(&blob).unwrap_or_else(|e| panic!("{script}: {e}\n{blob}"));
            let edits: Vec<EditOp> = parsed
                .edits
                .iter()
                .map(|e| EditOp {
                    old_text: e.old_text.clone(),
                    new_text: e.new_text.clone(),
                    start_line: e.start_line.map(blockedit::EditStartLine),
                    origin: None,
                })
                .collect();
            let applied = match blockedit::apply_to_text(before, &edits) {
                ApplyOutcome::Applied { text, .. } => text,
                ApplyOutcome::Failed { report, .. } => panic!("{script}: {report}"),
            };
            assert_eq!(applied, after, "{script}");
        }
    }

    #[test]
    fn file_without_trailing_newline_is_unchanged_by_a_no_op() {
        let file = "a\nb";
        let TextPreview { blob, .. } = preview_text(file, "f.rs", "s/zzz/yyy/", opts()).unwrap();
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
            // Escapes in the text of a/i/c. `sed-rs` copies that text verbatim
            // where GNU decodes it, so without the rewrite these preview as a
            // literal `\n` and apply back as the wrong bytes.
            r"2a\ntwo",
            r"2a\ttabbed",
            r"2a\x41",
            r"2a\o101",
            r"2a\d065",
            r"2a\\backslash",
            r"2a\q",
            r"3i\ninserted",
            r"2c\treplaced",
            r"7,8c\nreplaced",
            // Indentation in inserted text. The backslash is what makes the
            // whitespace content rather than a separator, so these are the cases
            // that come back dedented if the text argument is trimmed.
            r"2a\    deep",
            r"2a\  two spaces",
            r"2a\ one space",
            r"2a\	tabbed",
            r"4i\    deep",
            r"2c\    deep",
            r"7,8c\    deep",
            // ...and the backslash-less form is the opposite: the run separates.
            r"2a    shallow",
            // A carriage return inside a line, from the replacement. The diff
            // counted it as a line break that the spans did not, so the block
            // covered one line of OLD against two of NEW and applying it
            // duplicated a line. A CR on its own is a line ending and worked.
            r"3s/let y = 2;/CR\rlet y = 2;/",
            r"5s/run/\r/",
            // Deleting through the end of the file. The last line's terminator
            // has to go with it, and a block that fails to take it still applies
            // cleanly while leaving a blank line behind.
            "$d",
            "1,2!d",
            "1,9d",
            "8,9d",
            // ...and the same for a deletion that just is not at the end. With
            // no trailing context to absorb the newline, and the diff not
            // always pairing the following line into the block, an interior
            // deletion leaves a blank line too.
            "6,7d",
            "2d",
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

            let TextPreview { banner, blob, .. } =
                preview_text(FILE, "src/foo.rs", script, opts()).unwrap();
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
                    origin: None,
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
        let TextPreview { blob, .. } = preview_text(FILE, "f.rs", "s/run/\\U&/", opts()).unwrap();
        assert!(blob.contains("RUN();"), "{blob}");
        assert!(!blob.contains("\\U"), "{blob}");
    }
}

pub mod cli;
