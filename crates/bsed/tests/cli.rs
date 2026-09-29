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
    assert!(out.contains("one.rs: FAILED"), "{out}");
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
