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
    let o = run_in(d.path(), None, &["preview", "-p", ".", "s/a/b/"]);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("--recursive"), "{}", stderr(&o));
}

#[test]
fn recursive_preview_walks_the_tree_and_reports_counts() {
    let d = tree();
    let o = run_in(
        d.path(),
        None,
        &["preview", "-r", "-p", ".", "s/old_name/new_name/g"],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("2 of "), "{out}"); // two files changed
    assert!(out.contains("one.rs:2"), "{out}");
    assert!(out.contains("sub/two.rs:2"), "{out}");
    // .gitignore is respected without needing a git repo, and non-text is
    // counted rather than fatal.
    assert!(!out.contains("skip.rs"), "{out}");
    assert!(!out.contains("dep.rs"), "{out}");
    assert!(out.contains("not UTF-8"), "{out}");
}

#[test]
fn recursive_preview_with_no_matches_says_so() {
    let d = tree();
    let o = run_in(d.path(), None, &["preview", "-r", "-p", ".", "s/zzz/yyy/g"]);
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("no lines matched"), "{}", stdout(&o));
}

#[test]
fn recursive_apply_is_atomic_per_file_and_echoes_the_failures() {
    let d = tree();
    let root = d.path().to_path_buf();
    let blocks = stdout(&run_in(
        &root,
        None,
        &["preview", "-r", "-p", ".", "s/old_name/new_name/g"],
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
    assert!(out.contains("one.rs:2"), "missing the block header: {out}");
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
        &["preview", "-r", "-p", ".", "s/old_name/new_name/g"],
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
        &["preview", "-p", "a.rs", "s/old_name/new_name/g"],
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
        let o = run_in(&root, None, &["preview", "-p", "a.rs", script]);
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

    let blocks = stdout(&run_in(&root, None, &["preview", "-r", "-p", ".", script]));
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
        &[
            "preview",
            "-r",
            "-g",
            "**/*.rs",
            "-p",
            ".",
            "s/old_name/new_name/g",
        ],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("one.rs:2"), "{}", stdout(&o));

    // A pattern nothing matches yields an empty, successful preview.
    let o = run_in(
        &root,
        None,
        &[
            "preview",
            "-r",
            "-g",
            "**/*.py",
            "-p",
            ".",
            "s/old_name/new_name/g",
        ],
    );
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("no lines matched"), "{}", stdout(&o));
}

#[test]
fn glob_without_recursive_is_a_usage_error() {
    let d = tree();
    let o = run_in(
        d.path(),
        None,
        &["preview", "-g", "*.rs", "-p", "one.rs", "s/a/b/"],
    );
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

    let o = run_in(&root, None, &["preview", "-p", "self.rs", "s/old/new/g"]);
    assert_eq!(code(&o), 2);
    let err = stderr(&o);
    assert!(err.contains("appliable blocks"), "{err}");
    assert!(err.contains("@@@NEW"), "{err}");

    // Recursively, the un-representable file is named and the other still works.
    let o = run_in(&root, None, &["preview", "-r", "-p", ".", "s/old/new/g"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("plain.rs:1"), "{out}");
    assert!(out.contains("self.rs"), "the bad file must be named: {out}");
    assert!(!out.contains("@@@OLD\nold"), "{out}");
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
        &["preview", "-r", "-p", ".", "s/old_name/new_name/g"],
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
    cmd.args(["preview", "-r", "-p", ".", "s/old_name/new_name/g"]);
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
        .args(["preview", "-r", "-p", ".", "s/old_name/new_name/g"])
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
