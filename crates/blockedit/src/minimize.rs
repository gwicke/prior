//! Shrink an edit to the lines that actually changed, anchored at the line it
//! resolved to.
//!
//! This runs only for edits that applied successfully. The caller supplies the
//! resolved 1-based start line from [`crate::apply::ApplyOutcome::Applied`], so
//! no file access is needed and a file that changed after execution cannot
//! invalidate the anchor.

use crate::types::{EditOp, EditStartLine};

/// Split text into lines, keeping each line's terminator. Joining the result
/// reproduces the input exactly, including CRLF and a missing final newline.
fn lines_with_terminators(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn join(lines: &[&str]) -> String {
    lines.concat()
}

/// Minimize one edit against the line where its OLD text resolved.
///
/// Removes the longest common whole-line prefix and suffix. The new anchor is
/// the resolved line plus the number of removed prefix lines. Never trims
/// inside a line, and keeps at least one OLD line when trimming would empty it
/// (the blob parser rejects an empty `@@@OLD`).
pub fn minimize_edit(edit: &EditOp, resolved_start_line: usize) -> EditOp {
    let old = lines_with_terminators(&edit.old_text);
    let new = lines_with_terminators(&edit.new_text);

    let max = old.len().min(new.len());
    let mut prefix = 0;
    while prefix < max && old[prefix] == new[prefix] {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < max - prefix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }

    // A pure insertion (or an old text wholly contained in the common lines)
    // would leave `@@@OLD` empty. Keep one line as context: drop a prefix line
    // when there is one, otherwise a suffix line. This also means the anchor
    // moves back with that line, so it still names the first OLD line.
    if prefix + suffix >= old.len() && !old.is_empty() {
        if prefix > 0 {
            prefix -= 1;
        } else if suffix > 0 {
            suffix -= 1;
        }
    }

    let old_kept = &old[prefix..old.len() - suffix];
    let new_kept = &new[prefix..new.len() - suffix];

    EditOp {
        old_text: join(old_kept),
        new_text: join(new_kept),
        start_line: Some(EditStartLine(resolved_start_line as u64 + prefix as u64)),
        origin: edit.origin.clone(),
    }
}

/// Minimize a batch in order. `matched_lines` comes from
/// [`crate::apply::ApplyOutcome::Applied`] and is indexed like `edits`.
///
/// A length mismatch is a caller bug; returning the edits unchanged is the
/// safe fallback, since an unchanged edit still applies correctly.
pub fn minimize_ops(edits: &[EditOp], matched_lines: &[usize]) -> Vec<EditOp> {
    if edits.len() != matched_lines.len() {
        return edits.to_vec();
    }
    edits
        .iter()
        .zip(matched_lines)
        .map(|(edit, line)| minimize_edit(edit, *line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(old: &str, new: &str) -> EditOp {
        EditOp {
            old_text: old.into(),
            new_text: new.into(),
            start_line: None,
            origin: None,
        }
    }

    fn minimized(old: &str, new: &str, line: usize) -> (String, String, u64) {
        let out = minimize_edit(&op(old, new), line);
        (
            out.old_text,
            out.new_text,
            out.start_line.expect("anchored").0,
        )
    }

    #[test]
    fn trims_common_prefix_and_suffix_lines() {
        let (old, new, line) = minimized("a\nb\nc\nd\n", "a\nb\nX\nd\n", 10);
        assert_eq!(old, "c\n");
        assert_eq!(new, "X\n");
        assert_eq!(line, 12);
    }

    #[test]
    fn keeps_a_context_line_for_a_pure_insertion() {
        let (old, new, line) = minimized("a\n", "a\nb\n", 5);
        assert_eq!(old, "a\n");
        assert_eq!(new, "a\nb\n");
        assert_eq!(line, 5);
    }

    #[test]
    fn keeps_a_context_line_when_old_is_fully_shared() {
        let (old, new, line) = minimized("a\nb\n", "X\na\nb\n", 5);
        assert_eq!(old, "a\n");
        assert_eq!(new, "X\na\n");
        assert_eq!(line, 5);
    }

    #[test]
    fn partial_line_edits_are_unchanged_apart_from_the_anchor() {
        let (old, new, line) = minimized("foo bar", "foo baz", 7);
        assert_eq!(old, "foo bar");
        assert_eq!(new, "foo baz");
        assert_eq!(line, 7);
    }

    #[test]
    fn preserves_terminators_exactly() {
        let (old, new, line) = minimized("a\r\nb\r\nc\r\n", "a\r\nB\r\nc\r\n", 3);
        assert_eq!(old, "b\r\n");
        assert_eq!(new, "B\r\n");
        assert_eq!(line, 4);
    }

    #[test]
    fn idempotent() {
        let first = minimize_edit(&op("a\nb\nc\nd\n", "a\nb\nX\nd\n"), 10);
        let second = minimize_edit(&first, first.start_line.unwrap().0 as usize);
        assert_eq!(first.old_text, second.old_text);
        assert_eq!(first.new_text, second.new_text);
        assert_eq!(first.start_line, second.start_line);
    }

    #[test]
    fn batch_mismatch_is_a_no_op() {
        let edits = vec![op("a", "b")];
        let out = minimize_ops(&edits, &[]);
        assert_eq!(out[0].old_text, "a");
        assert_eq!(out[0].new_text, "b");
        assert!(out[0].start_line.is_none());
    }
}
