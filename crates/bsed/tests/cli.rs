//! End-to-end tests for the `bsed` command line, driving the real binary.
//!
//! The unit tests cover the engine; these cover the parts a user actually
//! touches — flags, exit codes, the recursive walk, and what a failed apply
//! hands back.

use std::path::Path;
use std::process::{Command, Output};

fn bsed() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bsed"))
}

/// Run `bsed` in `dir` with `args`, feeding `stdin` when given.
fn run_in(dir: &Path, stdin: Option<&str>, args: &[&str]) -> Output {
    let mut c = bsed();
    c.current_dir(dir).args(args);
    c.stdin(if stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    let mut child = c.spawn().expect("spawn bsed");
    if let Some(text) = stdin {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    child.wait_with_output().expect("bsed runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}
fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A small tree: two files to change, one ignored, one not text.
fn tree() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    write(root, "one.rs", "fn a() {\n    let old_name = 1;\n}\n");
    write(root, "sub/two.rs", "fn b() {\n    let old_name = 2;\n}\n");
    write(root, "target/skip.rs", "let old_name = 3;\n");
    write(root, "node_modules/dep.rs", "let old_name = 4;\n");
    write(root, ".gitignore", "target\nnode_modules\n");
    std::fs::write(root.join("blob.rs"), [0xff, 0xfe, b'o', b'l', b'd']).unwrap();
    d
}

#[test]
fn a_directory_without_recursive_is_a_usage_error() {
    let d = tree();
    let o = run_in(d.path(), None, &["-p", ".", "s/a/b/"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("--recursive"), "{}", stderr(&o));
}

#[test]
fn a_crlf_file_is_refused_with_a_reason() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("crlf.txt"), "a\r\nb\r\n").unwrap();
    let o = run_in(d.path(), None, &["s/b/B/", "crlf.txt"]);
    assert_eq!(code(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("carriage returns"), "{err}");
    assert!(err.contains("LF"), "{err}");
    // Nothing was written, and no block was offered that would not apply.
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
    assert_eq!(
        std::fs::read_to_string(d.path().join("crlf.txt")).unwrap(),
        "a\r\nb\r\n"
    );
}

/// One refused file must not sink the walk, and it must be named: silence
/// would read as "that file needed no change".
#[test]
fn a_crlf_file_does_not_sink_a_recursive_preview() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("crlf.txt"), "a\r\nb\r\n").unwrap();
    std::fs::write(d.path().join("ok.txt"), "a\nb\n").unwrap();
    let o = run_in(d.path(), None, &["-r", "-p", ".", "s/b/B/"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    // The LF file was still previewed...
    assert!(out.contains("ok.txt:1"), "{out}");
    assert!(out.contains("@@@NEW"), "{out}");
    // ...and the CRLF one is named with the reason, in the status, not folded
    // into the payload or into a count.
    let err = stderr(&o);
    assert!(err.contains("crlf.txt"), "{err}");
    assert!(err.contains("CRLF"), "{err}");
    assert!(!out.contains("crlf.txt"), "{out}");
}

/// A tree that is *only* refused files must not report "no lines matched" and
/// stop there — the expression did match, the tool declined to look.
#[test]
fn a_tree_of_only_crlf_files_says_why() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("crlf.txt"), "a\r\nb\r\n").unwrap();
    let o = run_in(d.path(), None, &["-r", "-p", ".", "s/b/B/"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("no lines matched"), "{err}");
    // ...but it also says which file matched and was skipped, and why. Nothing
    // went to stdout: there was nothing to preview.
    assert!(err.contains("crlf.txt"), "{err}");
    assert!(err.contains("CRLF"), "{err}");
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
}

/// Previewing is the default mode: no subcommand word is needed, and the script
/// may come first or from `-e`.
#[test]
fn previewing_needs_no_subcommand() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "a.rs", "one\ntwo\n");
    for args in [
        &["s/one/1/", "a.rs"][..],
        &["s/one/1/", "-p", "a.rs"][..],
        &["-c", "0", "-p", "a.rs", "s/one/1/"][..],
        &["-e", "s/one/1/", "-p", "a.rs"][..],
    ] {
        let o = run_in(d.path(), None, args);
        assert_eq!(code(&o), 0, "{args:?}: {}", stderr(&o));
        assert!(stdout(&o).contains("a.rs:1"), "{args:?}: {}", stdout(&o));
    }
}

/// A leading option used to be rejected on the grounds that a flag cannot be an
/// expression. That left the `sed` tool's own invocation unusable; options may
/// lead now, the way `sed` accepts them.
#[test]
fn an_option_may_lead_the_command_line() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "a.rs", "one\ntwo\n");
    let o = run_in(d.path(), None, &["-c", "1", "-p", "a.rs", "s/one/1/"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("a.rs:1"), "{}", stdout(&o));
}

/// An option that exists in neither mode is still an error, with the usage
/// text -- the preview parser is what knows the options, so it is what reports.
#[test]
fn an_unknown_option_is_still_refused() {
    let d = tempfile::tempdir().unwrap();
    let o = run_in(d.path(), None, &["-z", "a.rs"]);
    assert_eq!(code(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("unknown option `-z`"), "{err}");
    assert!(err.contains("USAGE"), "{err}");
}

/// `-u` emits a unified diff: file headers, hunk markers, real `-`/`+` lines.
/// No anchor, no `@@@OLD`, nothing the `edit` tool could take.
#[test]
fn unified_emits_a_unified_diff() {
    let d = tempfile::tempdir().unwrap();
    write(
        &d.path().to_path_buf(),
        "a.rs",
        "fn main() {\n    let x = 1;\n    let y = 2;\n}\n",
    );
    let o = run_in(
        d.path(),
        None,
        &["-u", "-p", "a.rs", "2s/let x = 1;/let x = 42;/"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("--- a.rs\n"), "{out}");
    assert!(out.contains("+++ a.rs\n"), "{out}");
    assert!(out.contains("@@ -"), "{out}");
    assert!(out.contains("-    let x = 1;"), "{out}");
    assert!(out.contains("+    let x = 42;"), "{out}");
    assert!(!out.contains("@@@OLD"), "{out}");
    // Status still goes to stderr, and must not tell the reader to apply it.
    let err = stderr(&o);
    assert!(err.contains("drop -u"), "{err}");
    assert!(!err.contains("apply them"), "{err}");
}
/// A diff takes the wider of `-c` and `-t`, so with the defaults it shows the
/// same one line each side the blocks do. `-c 0` is an explicit request for none
/// and is honoured, and a wider request is honoured too.
#[test]
fn unified_takes_the_context_flags_and_honours_c_zero() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "a.rs", "a\nb\nc\nd\ne\nf\n");
    let s = "3s/c/C/";
    let def = stdout(&run_in(d.path(), None, &["-u", "-p", "a.rs", s]));
    assert!(
        def.contains("\n b\n") && def.contains("\n d\n"),
        "default: {def}"
    );
    assert!(
        !def.contains("\n a\n"),
        "more than one line of context: {def}"
    );
    let none = stdout(&run_in(d.path(), None, &["-u", "-c", "0", "-p", "a.rs", s]));
    assert!(
        !none.contains("\n b\n") && !none.contains("\n d\n"),
        "-c 0: {none}"
    );
    let wide = stdout(&run_in(d.path(), None, &["-u", "-c", "3", "-p", "a.rs", s]));
    assert!(
        wide.contains("\n a\n") && wide.contains("\n e\n"),
        "-c 3: {wide}"
    );
    // Blocks carry the same window by default, and `-c 0` takes it away.
    let blocks = stdout(&run_in(d.path(), None, &["-p", "a.rs", s]));
    assert!(blocks.starts_with("a.rs:2\n@@@OLD\nb\nc\n"), "{blocks}");
    assert!(blocks.contains("\nd\n"), "no trailing context: {blocks}");
    let bare = stdout(&run_in(
        d.path(),
        None,
        &["-c", "0", "-t", "0", "-p", "a.rs", s],
    ));
    assert!(bare.starts_with("a.rs:3\n@@@OLD\nc\n"), "{bare}");
}

/// More than one file needs the `---`/`+++` headers, since a diff has no
/// `path:LINE` anchor to say which file a hunk came from.
#[test]
fn unified_separates_files_in_a_recursive_run() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "one.rs", "a\nb\n");
    write(&d.path().to_path_buf(), "two.rs", "b\nc\n");
    let o = run_in(d.path(), None, &["-u", "-r", "-p", ".", "s/b/B/"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    // The label is the same path string the block headers use, `./` prefix and
    // all, so the two modes name a file identically.
    assert!(out.contains("--- ./one.rs\n"), "{out}");
    assert!(out.contains("--- ./two.rs\n"), "{out}");
    // Files are ordered, and a blank line separates them.
    let a = out.find("--- ./one.rs").unwrap();
    let b = out.find("--- ./two.rs").unwrap();
    assert!(a < b, "{out}");
    assert!(
        out.contains("B\n\n--- ./two.rs"),
        "no blank line between: {out}"
    );
}

/// Nothing changed is still exit 0 and an empty stdout, whichever mode.
#[test]
fn unified_with_no_change_is_silent() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "a.rs", "a\n");
    let o = run_in(d.path(), None, &["-u", "-p", "a.rs", "s/zzz/yyy/"]);
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
    assert!(stderr(&o).contains("no lines matched"), "{}", stderr(&o));
}

/// CRLF is refused in this mode too, rather than diffed into something wrong.
#[test]
fn unified_refuses_a_crlf_file() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "crlf.txt", "a\r\nb\r\n");
    let o = run_in(d.path(), None, &["-u", "-p", "crlf.txt", "s/b/B/"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("carriage returns"), "{}", stderr(&o));
}

/// `preview` used to be the subcommand. The word is gone, but saying so beats
/// letting it be read as a script and failing on the quotes in the next one.
#[test]
fn the_old_preview_subcommand_says_what_changed() {
    let d = tempfile::tempdir().unwrap();
    write(&d.path().to_path_buf(), "a.rs", "one\n");
    let o = run_in(d.path(), None, &["preview", "s/one/1/", "-p", "a.rs"]);
    assert_eq!(code(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("no `preview` subcommand"), "{err}");
    assert!(err.contains("bsed 's/a/b/' f.rs"), "{err}");
}

#[test]
fn recursive_preview_walks_the_tree_and_reports_counts() {
    let d = tree();
    let o = run_in(d.path(), None, &["-r", "-p", ".", "s/old_name/new_name/g"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    // The blocks are the payload, and nothing else is: this is what a redirect
    // to a file now captures.
    let out = stdout(&o);
    // One line of leading context by default, so a change on line 2 anchors
    // its block at line 1.
    assert!(out.contains("one.rs:1"), "{out}");
    assert!(out.contains("sub/two.rs:1"), "{out}");
    assert!(
        !out.contains("sed preview:"),
        "status leaked into the payload: {out}"
    );

    // The summary and the notes are about the payload, so they are on stderr.
    let err = stderr(&o);
    assert!(err.contains("2 of "), "{err}"); // two files changed
                                             // .gitignore is respected without needing a git repo, and non-text is
                                             // counted rather than fatal.
    assert!(!err.contains("skip.rs"), "{err}");
    assert!(!err.contains("dep.rs"), "{err}");
    assert!(err.contains("not UTF-8"), "{err}");
}

/// `-c N` is symmetric context, the way `grep -C` is, and `-t N` is a bonus
/// *after* on top of it. Measured off the anchor and the block length, on a file
/// long enough that the diff's own minimum grouping does not decide the shape.
#[test]
fn context_is_symmetric_and_trailing_is_a_bonus() {
    let d = tempfile::tempdir().unwrap();
    let big: String = (1..=20).map(|i| format!("L{i}\n")).collect();
    write(&d.path().to_path_buf(), "big.txt", &big);
    let s = "10s/L10/CHANGED/";

    // Returns (leading, trailing) as the block actually came out.
    let span = |args: &[&str]| -> (usize, usize) {
        let o = run_in(d.path(), None, args);
        assert_eq!(code(&o), 0, "{args:?}: {}", stderr(&o));
        let out = stdout(&o);
        let anchor: usize = out
            .lines()
            .next()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let old: Vec<&str> = out
            .split("@@@OLD\n")
            .nth(1)
            .unwrap()
            .split("\n@@@NEW")
            .next()
            .unwrap()
            .lines()
            .collect();
        (10 - anchor, old.len() - 1 - (10 - anchor))
    };

    let p = &d.path().join("big.txt").display().to_string();
    let args = |extra: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        v.extend(["-p".into(), p.clone(), s.into()]);
        v
    };
    let span_of = |extra: &[&str]| {
        let v = args(extra);
        let refs: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
        span(&refs)
    };

    assert_eq!(span_of(&["-c", "1"]), (1, 1), "-c is symmetric");
    assert_eq!(span_of(&["-c", "2"]), (2, 2), "-c is symmetric");
    assert_eq!(
        span_of(&["-c", "1", "-t", "3"]),
        (1, 4),
        "-t is a bonus after"
    );
    assert_eq!(span_of(&["-c", "2", "-t", "2"]), (2, 4), "-t adds to -c");
    assert_eq!(
        span_of(&["-c", "0", "-t", "4"]),
        (0, 4),
        "-t works without -c"
    );
    // No flags at all is one line each side, the same as an explicit `-c 1`.
    assert_eq!(span_of(&[]), span_of(&["-c", "1"]), "default matches -c 1");
}

/// A file with no final newline, previewed then applied, against GNU sed. The
/// terminator is absent only while the input's last line is still the output's,
/// which is subtle enough that the block shape alone is not evidence.
#[test]
fn an_unterminated_file_round_trips_through_gnu() {
    let d = tempfile::tempdir().unwrap();
    let body = "a\nb\nc";
    for (script, want) in [
        ("s/zzz/yyy/", body),
        ("s/c/z/", "a\nb\nz"),
        ("1d", "b\nc"),
        ("2i\\X", "a\nX\nb\nc"),
        ("$d", "a\nb\n"),
        ("$a app", "a\nb\nc\napp\n"),
    ] {
        write(&d.path().to_path_buf(), "u.txt", body);
        let want = want.to_string();
        let blocks = run_in(d.path(), None, &["-p", "u.txt", script]);
        if stdout(&blocks).is_empty() {
            assert_eq!(want, body, "{script}");
            continue;
        }
        assert_eq!(code(&blocks), 0, "{script}: {}", stderr(&blocks));
        let o = run_in(d.path(), Some(&stdout(&blocks)), &["apply", "u.txt"]);
        assert_eq!(code(&o), 0, "{script}: {}", stdout(&o));
        assert_eq!(stdout(&o), want, "{script}");
    }
}

/// stdout is the payload and stderr is the commentary, so a redirect to a file
/// yields a file the `edit` tool can take as its `blocks` argument verbatim --
/// no leading sentence, no trailing notes.
#[test]
fn a_redirected_preview_is_only_the_blocks() {
    let d = tree();
    let o = run_in(d.path(), None, &["-r", "-p", ".", "s/old_name/new_name/g"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    let parsed = blockedit::parse_blocks(&out).expect("payload parses as blocks");
    assert_eq!(parsed.edits.len(), 2, "{out}");
    for line in out.lines() {
        assert!(
            !line.starts_with("sed preview:") && !line.starts_with("NOTE:"),
            "status line in the payload: {line}"
        );
    }
    // The same for a single file, where the banner used to lead the blob.
    let o = run_in(d.path(), None, &["-p", "one.rs", "s/old_name/new_name/g"]);
    let out = stdout(&o);
    assert!(out.starts_with("one.rs:"), "{out}");
    assert!(
        out.lines().next().unwrap().contains(':'),
        "header first: {out}"
    );
    blockedit::parse_blocks(&out).expect("payload parses as blocks");
    assert!(!out.contains("sed preview:"), "{out}");
}

#[test]
fn recursive_preview_with_no_matches_says_so() {
    let d = tree();
    let o = run_in(d.path(), None, &["-r", "-p", ".", "s/zzz/yyy/g"]);
    assert_eq!(code(&o), 0);
    assert!(stderr(&o).contains("no lines matched"), "{}", stderr(&o));
    // A successful run that found nothing writes no file content, so a redirect
    // gives an empty file rather than one holding the explanation.
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
}

#[test]
fn recursive_apply_is_atomic_per_file_and_echoes_the_failures() {
    let d = tree();
    let root = d.path().to_path_buf();
    let blocks = stdout(&run_in(
        &root,
        None,
        &["-r", "-p", ".", "s/old_name/new_name/g"],
    ));

    // Move one file on after the preview, so its block no longer matches.
    write(&root, "one.rs", "fn a() {\n    let MOVED_ON = 1;\n}\n");

    let o = run_in(&root, Some(&blocks), &["apply", "-i"]);
    let out = stdout(&o);

    // The untouched file applied; the moved one did not, and was not written.
    assert!(std::fs::read_to_string(root.join("sub/two.rs"))
        .unwrap()
        .contains("new_name"));
    assert!(std::fs::read_to_string(root.join("one.rs"))
        .unwrap()
        .contains("MOVED_ON"));
    assert!(
        out.contains("one.rs:"),
        "the failure must name its file: {out}"
    );
    assert!(out.contains("oldText not found"), "{out}");
    assert!(
        out.contains("2 file(s)") || out.contains("1 file(s) applied"),
        "{out}"
    );

    // The failed block comes back with its path, ready to fix and re-pipe.
    // One line of leading context, so the block anchors a line above the change.
    assert!(out.contains("one.rs:1"), "missing the block header: {out}");
    assert!(out.contains("@@@OLD"), "{out}");
    assert!(out.contains("let old_name = 1;"), "{out}");
    // And a partial run is an error, with a one-line signal on stderr for a
    // script that is not reading the report.
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("1 file(s) failed"), "{}", stderr(&o));
}

#[test]
fn a_multi_file_blob_needs_in_place() {
    let d = tree();
    let root = d.path().to_path_buf();
    let blocks = stdout(&run_in(
        &root,
        None,
        &["-r", "-p", ".", "s/old_name/new_name/g"],
    ));
    let o = run_in(&root, Some(&blocks), &["apply"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("-i"), "{}", stderr(&o));
    // Nothing was written.
    assert!(std::fs::read_to_string(root.join("one.rs"))
        .unwrap()
        .contains("old_name"));
}

#[test]
fn single_file_apply_without_in_place_prints_and_does_not_write() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    write(&root, "a.rs", "fn a() {\n    let old_name = 1;\n}\n");
    let blocks = stdout(&run_in(
        &root,
        None,
        &["-p", "a.rs", "s/old_name/new_name/g"],
    ));
    let o = run_in(&root, Some(&blocks), &["apply", "a.rs"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("new_name"), "{}", stdout(&o));
    assert!(std::fs::read_to_string(root.join("a.rs"))
        .unwrap()
        .contains("old_name"));
}

#[test]
fn a_refused_expression_exits_two_and_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    write(&root, "a.rs", "one\n");
    for script in ["1w /tmp/bsed-should-not-exist", "1e touch /tmp/x", "1q"] {
        let o = run_in(&root, None, &["-p", "a.rs", script]);
        assert_eq!(code(&o), 2, "{script}");
        assert!(
            stderr(&o).contains("never writes")
                || stderr(&o).contains("does not execute")
                || stderr(&o).contains("delete the rest of the file"),
            "{script}: {}",
            stderr(&o)
        );
    }
    assert!(!Path::new("/tmp/bsed-should-not-exist").exists());
}

/// The property that matters most: a recursive preview, applied, produces
/// exactly what running the script over the tree would have produced.
#[test]
fn recursive_preview_then_apply_reproduces_sed_over_the_tree() {
    let d = tree();
    let root = d.path().to_path_buf();
    let script = "s/old_name/new_name/g";

    // Reference: what `sed -i` would leave behind, for the files bsed walks.
    let mut reference = std::collections::BTreeMap::new();
    for rel in [
        "one.rs",
        "sub/two.rs",
        "target/skip.rs",
        "node_modules/dep.rs",
    ] {
        let before = std::fs::read_to_string(root.join(rel)).unwrap();
        reference.insert(rel.to_string(), before.replace("old_name", "new_name"));
    }

    let blocks = stdout(&run_in(&root, None, &["-r", "-p", ".", script]));
    let o = run_in(&root, Some(&blocks), &["apply", "-i"]);
    assert_eq!(code(&o), 0, "{}", stdout(&o));

    for (rel, want) in &reference {
        let got = std::fs::read_to_string(root.join(rel)).unwrap();
        if rel == "target/skip.rs" || rel == "node_modules/dep.rs" {
            // Ignored by the walk, so deliberately untouched.
            assert!(!got.contains("new_name"), "{rel} was edited: {got}");
        } else {
            assert_eq!(&got, want, "{rel}");
        }
    }
}

#[test]
fn glob_narrows_the_recursive_walk() {
    let d = tree();
    let root = d.path().to_path_buf();
    // Only .rs files, which is every walked file here, so the *count* proves the
    // walk happened and a non-matching pattern proves the filter.
    let o = run_in(
        &root,
        None,
        &["-r", "-g", "**/*.rs", "-p", ".", "s/old_name/new_name/g"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("one.rs:1"), "{}", stdout(&o));

    // A pattern nothing matches yields an empty, successful preview.
    let o = run_in(
        &root,
        None,
        &["-r", "-g", "**/*.py", "-p", ".", "s/old_name/new_name/g"],
    );
    assert_eq!(code(&o), 0);
    // "no lines matched" is a status line, and nothing matched so there is no
    // payload for it to describe.
    assert!(stderr(&o).contains("no lines matched"), "{}", stderr(&o));
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
}

#[test]
fn glob_without_recursive_is_a_usage_error() {
    let d = tree();
    let o = run_in(d.path(), None, &["-g", "*.rs", "-p", "one.rs", "s/a/b/"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("--glob"), "{}", stderr(&o));
}

/// A file whose own content is a blocks file cannot be previewed as blocks: the
/// marker lines would be read as structure. The single-file path refuses, and
/// the recursive path names the file instead of silently dropping it.
#[test]
fn a_file_containing_block_markers_is_refused_not_mangled() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().to_path_buf();
    write(
        &root,
        "self.rs",
        "sed preview: 1 block\nf:1\n@@@OLD\nold\n@@@NEW\nnew\n@@@END\n",
    );
    write(&root, "plain.rs", "let old = 1;\n");

    let o = run_in(&root, None, &["-p", "self.rs", "s/old/new/g"]);
    assert_eq!(code(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("appliable blocks"), "{err}");
    // Which marker the block swallowed moved when the leading-context default
    // did: the block now starts a line earlier, so `@@@OLD` is the first one
    // inside it. Either is the point -- a marker as content is refused.
    assert!(err.contains("@@@OLD") || err.contains("@@@NEW"), "{err}");

    // Recursively, the un-representable file is named and the other still works.
    let o = run_in(&root, None, &["-r", "-p", ".", "s/old/new/g"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("plain.rs:1"), "{out}");
    assert!(!out.contains("@@@OLD\nold"), "{out}");
    // Naming the file that could not be previewed is a status line, so it is
    // on stderr -- and it must be *somewhere*, or the run reads as if that file
    // simply needed no change.
    let err = stderr(&o);
    assert!(err.contains("self.rs"), "the bad file must be named: {err}");
}

/// The point of the report being a valid blocks file: after fixing the block,
/// re-piping the report applies it, with no detangling.
#[test]
fn the_failure_report_is_itself_a_repipable_blocks_file() {
    let d = tree();
    let root = d.path().to_path_buf();
    let blocks = stdout(&run_in(
        &root,
        None,
        &["-r", "-p", ".", "s/old_name/new_name/g"],
    ));
    // Retire one file's content so its block cannot apply.
    write(&root, "one.rs", "fn a() {\n    let CHANGED = 1;\n}\n");

    let first = run_in(&root, Some(&blocks), &["apply", "-i"]);
    assert_eq!(code(&first), 2);
    let report = stdout(&first);

    // The report parses: re-piping it is accepted, and it reports the same
    // failure rather than "no blocks found" or a parse error.
    let second = run_in(&root, Some(&report), &["apply", "-i"]);
    assert_eq!(code(&second), 2);
    let again = stdout(&second);
    assert!(
        again.contains("one.rs: FAILED") || again.contains("one.rs:"),
        "{again}"
    );
    assert!(!again.contains("no blocks"), "{again}");

    // Fix the block the way a user would: the report is the file to edit.
    let fixed = report.replace("let old_name = 1;", "let CHANGED = 1;");
    let third = run_in(&root, Some(&fixed), &["apply", "-i"]);
    assert_eq!(code(&third), 0, "{}", stdout(&third));
    assert!(
        std::fs::read_to_string(root.join("one.rs"))
            .unwrap()
            .contains("new_name"),
        "the fixed block should have applied"
    );
}

/// A tty run leaves a re-pipable copy on disk; a piped run does not litter.
///
/// Driven over a real pty (via portable-pty, already in the tree) because
/// "is stdout a terminal" is exactly the question, and shelling out to a python
/// pty driver would make the test depend on python being installed.
#[test]
fn a_terminal_run_spools_the_output_to_a_file() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Read;

    let d = tree();
    let root = d.path().to_path_buf();
    // A private spool dir, so this test does not race other tests or whatever
    // a previous run left in /tmp.
    let spool_dir = tempfile::tempdir().unwrap();
    let before = count_spools(spool_dir.path());

    let bin = env!("CARGO_BIN_EXE_bsed");
    let mut cmd = CommandBuilder::new(bin);
    cmd.args(["-r", "-p", ".", "s/old_name/new_name/g"]);
    cmd.cwd(&root);
    cmd.env("BSED_SPOOL_DIR", spool_dir.path());

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut seen = String::new();
    reader.read_to_string(&mut seen).expect("read pty");
    let _ = child.wait();

    // On a pty, stdout and stderr share one stream.
    assert!(seen.contains("@@@OLD"), "{seen}");
    assert!(seen.contains("wrote a copy to"), "{seen}");
    assert_eq!(
        count_spools(spool_dir.path()),
        before + 1,
        "expected one spool file"
    );

    // The spooled copy is a usable blocks file: preview, apply.
    let spooled = std::fs::read_dir(spool_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("bsed-") && n.ends_with(".blocks"))
        })
        .max()
        .expect("a spool file");
    let body = std::fs::read_to_string(&spooled).unwrap();
    let o = run_in(&root, Some(&body), &["apply", "-i"]);
    assert_eq!(code(&o), 0, "{}", stdout(&o));
    assert!(std::fs::read_to_string(root.join("one.rs"))
        .unwrap()
        .contains("new_name"));

    // A piped run leaves none behind.
    let mut c2 = Command::new(bin);
    c2.current_dir(&root)
        .args(["-r", "-p", ".", "s/old_name/new_name/g"])
        .env("BSED_SPOOL_DIR", spool_dir.path())
        .stdout(std::process::Stdio::piped());
    c2.output().expect("piped preview");
    assert_eq!(
        count_spools(spool_dir.path()),
        before + 1,
        "a piped run must not spool"
    );
}

fn count_spools(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("bsed-")
                        && e.file_name().to_string_lossy().ends_with(".blocks")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Bare `bsed` used to read expressions from stdin, so with nothing to read it
/// looked like a hang — and `bsed` on its own is a thing people type.
#[test]
fn no_arguments_prints_usage_instead_of_blocking() {
    let d = tempfile::tempdir().unwrap();
    let o = run_in(d.path(), None, &[]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("USAGE:"), "{}", stdout(&o));
}
