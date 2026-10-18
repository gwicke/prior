//! Shrink an edit to the lines that actually changed, anchored at the line it
//! resolved to.
//!
//! This runs only for edits that applied successfully. The caller supplies the
//! resolved 1-based start line from [`crate::apply::ApplyOutcome::Applied`], so
//! no file access is needed and a file that changed after execution cannot
//! invalidate the anchor.

use crate::types::{EditOp, EditStartLine};
use crate::udiff;

/// Unchanged lines that may separate two changed regions *within one block*
/// before splitting is worth it. Splitting moves a run of shared lines into a
/// new block, which costs a `path:LINE` header, three markers and a separator
/// (~20-25 tokens); each shared line removed saves twice its own tokens (it was
/// present in both OLD and NEW). For ordinary code (~8-10 tokens/line) the
/// break-even is about one line, so split only above two — a gap of three clears
/// it with margin, and a gap of one or two is left in place.
pub const SPLIT_GAP: usize = 2;

/// A kept OLD fragment with fewer non-whitespace characters than this is a
/// *weak locator*: `}`, `end`, `);` and the like. The line anchor resolves
/// placement exactly, but a model biassed against trusting anchor line numbers
/// sees nothing it can recognise in a bare `}` and re-reads the file to
/// reassure itself — costing far more than one retained context line. When the
/// trim leaves a weak locator, [`minimize_edit`] keeps one trimmed common line
/// so the block shows something recognisable.
///
/// Whitespace is not counted because indentation is the part a model cannot
/// distinguish; `    }` is no more informative than `}`.
pub const MIN_LOCATOR_CHARS: usize = 8;

/// Split text into lines, keeping each line's terminator. Joining the result
/// reproduces the input exactly, including CRLF and a missing final newline.
fn lines_with_terminators(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn join(lines: &[&str]) -> String {
    lines.concat()
}

fn non_whitespace_len(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Whether `text` is a weak locator: too little non-whitespace to recognise.
fn is_weak_locator(text: &str) -> bool {
    non_whitespace_len(text) < MIN_LOCATOR_CHARS
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

    // Weak-locator floor: a trimmed OLD of just `}`, `end`, `);` gives the
    // model nothing to recognise, so it distrusts the anchor and re-reads the
    // file. Keep one trimmed common line for company, from whichever side has
    // more non-whitespace content (ties prefer the preceding line, usually the
    // construct being closed). The restored line is common to OLD and NEW, so
    // the edit still applies identically; and because it is persisted, the next
    // pass trims it again and restores the same side -- a stable fixed point.
    if !old.is_empty() {
        let kept = &old[prefix..old.len() - suffix];
        if is_weak_locator(&join(kept)) {
            let above = prefix.checked_sub(1).map(|i| old[i]);
            let below = (suffix > 0).then(|| old[old.len() - suffix]);
            match (above, below) {
                (Some(a), Some(b)) => {
                    if non_whitespace_len(a) >= non_whitespace_len(b) {
                        prefix -= 1;
                    } else {
                        suffix -= 1;
                    }
                }
                (Some(_), None) => prefix -= 1,
                (None, Some(_)) => suffix -= 1,
                (None, None) => {}
            }
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

/// Split one edit at long internal unchanged runs, then minimize each piece.
///
/// A block that touches two distant regions carries every unchanged line
/// between them, in *both* OLD and NEW. [`udiff::hunks`] diffs the block and
/// keeps a hunk together only across a gap of at most [`SPLIT_GAP`] shared
/// lines; each surviving hunk becomes one anchored sub-edit. Prefix/suffix
/// trimming falls out of the same diff (leading/trailing unchanged runs belong
/// to no hunk), and [`minimize_edit`] still decides what context survives inside
/// each piece, so this is the *only* place that splits and one function still
/// owns the trim.
///
/// `resolved_start_line` is the 1-based line the whole edit's OLD text resolved
/// to; a sub-edit's anchor is that plus its own OLD offset, so the ledger does
/// not need to change (sub-blocks are anchored inside the input edit).
///
/// Idempotent: sub-edits are anchored and at most [`SPLIT_GAP`] lines apart, so
/// a second pass trims each to itself and finds nothing to split. Their OLD
/// ranges are disjoint and exactly anchored (`d1 = 0`, always accepted).
///
/// A pure insertion has an empty OLD side; the blob parser rejects an empty
/// `@@@OLD`, so one unchanged neighbour (following first, then preceding) is
/// borrowed as context. A pure deletion keeps an empty NEW, which is valid.
pub fn split_and_minimize(edit: &EditOp, resolved_start_line: usize) -> Vec<EditOp> {
    let old = lines_with_terminators(&edit.old_text);
    let new = lines_with_terminators(&edit.new_text);
    let groups = udiff::hunks(&old, &new, SPLIT_GAP);
    if groups.is_empty() {
        // No change at all: nothing to split, keep the existing trim behaviour.
        return vec![minimize_edit(edit, resolved_start_line)];
    }

    let mut out = Vec::with_capacity(groups.len());
    for h in groups {
        let mut old_start = h.old_range.start;
        let mut old_end = h.old_range.end;
        let mut new_start = h.new_range.start;
        let mut new_end = h.new_range.end;

        // Borrow a context line for a pure insertion. A pure insert is always
        // bounded by an unchanged run on at least one side, so this always
        // finds one; the equality test guards against borrowing a changed line.
        if old_start == old_end {
            if old_end < old.len() && new_end < new.len() && old[old_end] == new[new_end] {
                old_end += 1;
                new_end += 1;
            } else if old_start > 0 && new_start > 0 && old[old_start - 1] == new[new_start - 1] {
                old_start -= 1;
                new_start -= 1;
            }
        }

        // A weak hunk (a lone `}` or `end`) gains a recognisable neighbour from
        // the unchanged run on either side. `minimize_edit` re-trims it for a
        // strong hunk, so this costs nothing unless the locator is weak. The
        // equality test only borrows genuinely unchanged lines.
        if is_weak_locator(&join(&old[old_start..old_end])) {
            if old_start > 0 && new_start > 0 && old[old_start - 1] == new[new_start - 1] {
                old_start -= 1;
                new_start -= 1;
            }
            if old_end < old.len() && new_end < new.len() && old[old_end] == new[new_end] {
                old_end += 1;
                new_end += 1;
            }
        }

        let candidate = EditOp {
            old_text: join(&old[old_start..old_end]),
            new_text: join(&new[new_start..new_end]),
            start_line: Some(EditStartLine(resolved_start_line as u64 + old_start as u64)),
            origin: edit.origin.clone(),
        };
        out.push(minimize_edit(&candidate, resolved_start_line + old_start));
    }
    out
}

/// Split-and-minimize a batch in order. `matched_lines` comes from
/// [`crate::apply::ApplyOutcome::Applied`] and is indexed like `edits`.
///
/// A length mismatch is a caller bug; returning the edits unchanged is the safe
/// fallback, since an unchanged edit still applies correctly.
pub fn split_and_minimize_ops(edits: &[EditOp], matched_lines: &[usize]) -> Vec<EditOp> {
    if edits.len() != matched_lines.len() {
        return edits.to_vec();
    }
    edits
        .iter()
        .zip(matched_lines)
        .flat_map(|(edit, line)| split_and_minimize(edit, *line))
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
        let (old, new, line) = minimized(
            "alpha_line\nbravo_line\nTARGET_old_x\ndelta_line\n",
            "alpha_line\nbravo_line\nTARGET_new_x\ndelta_line\n",
            10,
        );
        assert_eq!(old, "TARGET_old_x\n");
        assert_eq!(new, "TARGET_new_x\n");
        assert_eq!(line, 12);
    }

    #[test]
    fn keeps_a_context_line_for_a_pure_insertion() {
        let (old, new, line) = minimized("alpha_line\n", "alpha_line\ninserted_line\n", 5);
        assert_eq!(old, "alpha_line\n");
        assert_eq!(new, "alpha_line\ninserted_line\n");
        assert_eq!(line, 5);
    }

    #[test]
    fn keeps_a_context_line_when_old_is_fully_shared() {
        let (old, new, line) = minimized(
            "alpha_line\nbravo_line\n",
            "inserted_line\nalpha_line\nbravo_line\n",
            5,
        );
        assert_eq!(old, "alpha_line\n");
        assert_eq!(new, "inserted_line\nalpha_line\n");
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
        let (old, new, line) = minimized(
            "alpha_line\r\nbravo_line\r\ncharlie_line\r\n",
            "alpha_line\r\nBRAVO_CHANGED\r\ncharlie_line\r\n",
            3,
        );
        assert_eq!(old, "bravo_line\r\n");
        assert_eq!(new, "BRAVO_CHANGED\r\n");
        assert_eq!(line, 4);
    }

    #[test]
    fn idempotent() {
        let first = minimize_edit(
            &op(
                "alpha_line\nbravo_line\nTARGET_old_x\ndelta_line\n",
                "alpha_line\nbravo_line\nTARGET_new_x\ndelta_line\n",
            ),
            10,
        );
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

    fn anchors_and_texts(edits: &[EditOp]) -> Vec<(String, String, u64)> {
        edits
            .iter()
            .map(|e| {
                (
                    e.old_text.clone(),
                    e.new_text.clone(),
                    e.start_line.expect("anchored").0,
                )
            })
            .collect()
    }

    #[test]
    fn splits_at_a_long_internal_gap() {
        // old_x->new_x and old_y->new_y, three shared lines between them.
        let out = split_and_minimize(
            &op(
                "alpha_line\nold_x_line\nbravo_line\ncharlie_line\ndelta_line\nold_y_line\necho_line\n",
                "alpha_line\nnew_x_line\nbravo_line\ncharlie_line\ndelta_line\nnew_y_line\necho_line\n",
            ),
            10,
        );
        assert_eq!(
            anchors_and_texts(&out),
            vec![
                ("old_x_line\n".into(), "new_x_line\n".into(), 11),
                ("old_y_line\n".into(), "new_y_line\n".into(), 15),
            ]
        );
    }

    #[test]
    fn keeps_a_short_gap_inside_one_block() {
        // The two changes are separated by only two shared lines: one block.
        let out = split_and_minimize(
            &op(
                "alpha_line\nold_x_line\nbravo_line\ncharlie_line\nold_y_line\necho_line\n",
                "alpha_line\nnew_x_line\nbravo_line\ncharlie_line\nnew_y_line\necho_line\n",
            ),
            10,
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].old_text,
            "old_x_line\nbravo_line\ncharlie_line\nold_y_line\n"
        );
        assert_eq!(
            out[0].new_text,
            "new_x_line\nbravo_line\ncharlie_line\nnew_y_line\n"
        );
        assert_eq!(out[0].start_line.unwrap().0, 11);
    }

    #[test]
    fn a_split_insertion_borrows_following_context() {
        let out = split_and_minimize(
            &op(
                "alpha_line\nbravo_line\n",
                "alpha_line\ninserted_line\nbravo_line\n",
            ),
            5,
        );
        assert_eq!(
            anchors_and_texts(&out),
            vec![(
                "bravo_line\n".into(),
                "inserted_line\nbravo_line\n".into(),
                6
            )]
        );
    }

    #[test]
    fn a_trailing_insertion_borrows_preceding_context() {
        let out = split_and_minimize(
            &op(
                "alpha_line\nbravo_line\n",
                "alpha_line\nbravo_line\ninserted_line\n",
            ),
            5,
        );
        assert_eq!(
            anchors_and_texts(&out),
            vec![(
                "bravo_line\n".into(),
                "bravo_line\ninserted_line\n".into(),
                6
            )]
        );
    }

    #[test]
    fn a_pure_deletion_keeps_an_empty_new() {
        let out = split_and_minimize(
            &op(
                "alpha_line\nbravo_line\ncharlie_line\n",
                "alpha_line\ncharlie_line\n",
            ),
            3,
        );
        assert_eq!(
            anchors_and_texts(&out),
            vec![("bravo_line\n".into(), String::new(), 4)]
        );
    }

    #[test]
    fn splitting_is_idempotent() {
        let first = split_and_minimize(
            &op(
                "alpha_line\nold_x_line\nbravo_line\ncharlie_line\ndelta_line\nold_y_line\necho_line\n",
                "alpha_line\nnew_x_line\nbravo_line\ncharlie_line\ndelta_line\nnew_y_line\necho_line\n",
            ),
            10,
        );
        let mut second = Vec::new();
        for e in &first {
            second.extend(split_and_minimize(e, e.start_line.unwrap().0 as usize));
        }
        assert_eq!(anchors_and_texts(&first), anchors_and_texts(&second));
    }

    #[test]
    fn sub_blocks_partition_the_same_change() {
        // Applying the pieces one after another reproduces applying the whole.
        let old =
            "alpha_line\nold_x_line\nbravo_line\ncharlie_line\ndelta_line\nold_y_line\necho_line\n";
        let new =
            "alpha_line\nnew_x_line\nbravo_line\ncharlie_line\ndelta_line\nnew_y_line\necho_line\n";
        let whole = crate::apply::apply_to_text(old, &[op(old, new)]);
        let pieces = split_and_minimize(&op(old, new), 1);
        let mut text = old.to_string();
        // Anchored sub-blocks are disjoint, so any order applies cleanly; here
        // we go backwards like the applier does.
        for e in pieces.iter().rev() {
            let anchored = EditOp {
                old_text: e.old_text.clone(),
                new_text: e.new_text.clone(),
                start_line: e.start_line,
                origin: None,
            };
            match crate::apply::apply_to_text(&text, &[anchored]) {
                crate::apply::ApplyOutcome::Applied { text: t, .. } => text = t,
                crate::apply::ApplyOutcome::Failed { report, .. } => panic!("{report}"),
            }
        }
        let crate::apply::ApplyOutcome::Applied { text: want, .. } = whole else {
            panic!("whole edit failed");
        };
        assert_eq!(text, want);
    }

    #[test]
    fn no_change_is_a_single_minimized_edit() {
        let out = split_and_minimize(
            &op("alpha_line\nbravo_line\n", "alpha_line\nbravo_line\n"),
            5,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn weak_old_keeps_one_common_neighbour() {
        let (old, new, line) = minimized("let alpha = 1;\n}\n", "let alpha = 1;\n},\n", 10);
        assert_eq!(old, "let alpha = 1;\n}\n");
        assert_eq!(new, "let alpha = 1;\n},\n");
        assert_eq!(line, 10);
    }

    #[test]
    fn weak_old_prefers_the_longer_neighbour() {
        let (old, new, line) = minimized("x\n}\nlong_below_line\n", "x\n},\nlong_below_line\n", 10);
        assert_eq!(old, "}\nlong_below_line\n");
        assert_eq!(new, "},\nlong_below_line\n");
        assert_eq!(line, 11);
    }

    #[test]
    fn weak_floor_is_idempotent() {
        let first = minimize_edit(&op("let alpha = 1;\n}\n", "let alpha = 1;\n},\n"), 10);
        let second = minimize_edit(&first, first.start_line.unwrap().0 as usize);
        assert_eq!(first.old_text, second.old_text);
        assert_eq!(first.new_text, second.new_text);
        assert_eq!(first.start_line, second.start_line);
    }

    #[test]
    fn a_weak_hunk_borrows_a_neighbouring_line() {
        let out = split_and_minimize(
            &op(
                "alpha_line\n}\nbravo_line\n",
                "alpha_line\n},\nbravo_line\n",
            ),
            10,
        );
        assert_eq!(
            anchors_and_texts(&out),
            vec![("alpha_line\n}\n".into(), "alpha_line\n},\n".into(), 10)]
        );
    }
}
