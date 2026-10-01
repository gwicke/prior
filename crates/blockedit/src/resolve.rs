//! Resolving a batch of edits against file text: where each `old_text`
//! actually matches, and why it did not when it did not.
//!
//! Two passes over the *original* text (never the output of an earlier edit in
//! the same batch), mirroring the order the model wrote the blocks in:
//!
//! 1. **Line-anchored blocks.** An anchor is a positive statement of location,
//!    so anchored matches resolve (and claim their byte range) first.
//! 2. **Content-only blocks.** Each candidate overlapping a claim is excluded;
//!    accept iff exactly one free candidate remains.
//!
//! Pure: no filesystem, no agent types. `apply.rs` turns the result into text.

use crate::diagnose::{self, BatchItem, EditFailure};
use crate::types::EditOp;

/// Distance tolerance (in lines) for anchor-based matching.
///
/// Scales with the amount of SEARCH context: more context makes a false
/// nearest-match less likely, so a wider window is safe. `(context_lines / 3)`
/// capped at 4 — so three context lines (a common one-before/one-after shape)
/// enables a ±1 window, while a bare single-line SEARCH stays exact (`0`).
///
/// This is the *fast path* of the acceptance rule
/// ([`diagnose::anchor_accepts`]): inside this window the nearest candidate
/// always wins; beyond it the skew rule may still accept. Tolerance only
/// matters when `old_text` occurs more than once — a globally unique match is
/// applied regardless of the anchor.
pub fn anchor_tolerance(old_text: &str) -> usize {
    (old_text.lines().count() / 3).min(4)
}

/// Every occurrence of `old_text` in `text` as `(absolute byte offset,
/// 1-based file line)`.
pub fn occurrences_of(text: &str, old_text: &str) -> Vec<(usize, usize)> {
    text.match_indices(old_text)
        .map(|(off, _)| (off, diagnose::count_lines(&text[..off]) + 1))
        .collect()
}

/// 1-based line range `(first, last)` covered by the byte span `[start, end)`.
pub fn span_line_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let first = diagnose::count_lines(&text[..start]) + 1;
    if end <= start {
        return (first, first);
    }
    let mut newlines = diagnose::count_lines(&text[start..end]);
    // A span ending in a newline does not occupy the line after it.
    if text[start..end].ends_with('\n') && newlines > 0 {
        newlines -= 1;
    }
    (first, first + newlines)
}

/// Index of the first claim whose byte range overlaps `[start, end)`, if any.
fn first_claim_overlap(
    claims: &[(usize, usize, usize)], // (edit idx, start, end)
    start: usize,
    end: usize,
) -> Option<usize> {
    claims
        .iter()
        .position(|&(_, cs, ce)| start < ce && cs < end)
}

/// Wrap a per-edit failure with the identifying data the batch envelope needs.
///
/// One constructor for all six failure kinds, so the identifier fields cannot
/// drift between them: the origin string is what lets the model find *which* of
/// its blocks this was, and it must be attached uniformly or a rarer failure
/// becomes the one that cannot name itself.
fn batch_item(edit_index0: usize, edit: &EditOp, failure: EditFailure) -> BatchItem {
    BatchItem {
        edit_index: edit_index0,
        failure,
        origin: edit.origin.clone(),
        anchored: edit.start_line.is_some(),
        old_text: edit.old_text.clone(),
    }
}

/// The "oldText not found" failure, shared by both resolution passes.
///
/// The block's anchor is deliberately absent from this message. A line number
/// is a disambiguator, not a validity check — a content-unique match is applied
/// whatever the anchor says — so naming it here would imply a wrong anchor could
/// have caused a failure. The anchor reaches the model as part of the origin
/// string, which is an identifier and nothing more.
fn not_found_failure(edit_index0: usize, edit: &EditOp, text: &str) -> BatchItem {
    let edit_idx_1 = edit_index0 + 1;
    let mut f = match diagnose::diagnose(&edit.old_text, text) {
        Some(f) => f,
        None => EditFailure::not_found_no_match(),
    };
    f.ui_view = format!("edit #{edit_idx_1} oldText not found. {}", f.ui_view);
    batch_item(edit_index0, edit, f)
}

/// Resolve the `edit` tool's matching in two passes over the original file:
///
/// 1. **Line-anchored blocks, in block order.** An anchor is a positive
///    statement of location, so anchored matches resolve (and *claim* their
///    byte range) first. The rule: nearest within [`anchor_tolerance`] always;
///    beyond that only under the skew rule ([`diagnose::anchor_accepts`]);
///    ties are ambiguous. A chosen match overlapping an earlier claim is a
///    conflict — rejected, never silently re-aimed at another candidate.
/// 2. **Content-only blocks, in block order.** Each candidate overlapping any
///    claim is excluded; accept iff exactly one free candidate remains, then
///    claim it (so later blocks see the new constraint — narrowing chains).
///    Zero free → "all claimed"; ≥2 free → ambiguous (listing free lines and
///    the excluded count).
///
/// Failures claim nothing (the batch applies all-or-nothing anyway), so a
/// failed block never hides candidates from later diagnostics; both passes run
/// to completion and every failure is reported together.
///
/// Returns `(claims, resolved)` where `resolved[i]` is the byte offset of the
/// match for edit `i` — all `Some` when `failures` is empty.
pub fn resolve_edits(
    text: &str,
    edits: &[EditOp],
    failures: &mut Vec<BatchItem>,
) -> (Vec<(usize, usize, usize)>, Vec<Option<usize>>) {
    let total = edits.len();
    let mut claims: Vec<(usize, usize, usize)> = Vec::new();
    let mut resolved: Vec<Option<usize>> = vec![None; total];

    // ---- Pass 1: line-anchored blocks, in block order --------------------
    for (i, edit) in edits.iter().enumerate() {
        let Some(anchor) = edit.start_line.map(|l| l.0) else {
            continue;
        };
        let occs = occurrences_of(text, &edit.old_text);
        if occs.is_empty() {
            failures.push(not_found_failure(i, edit, text));
            continue;
        }
        let chosen: usize = if occs.len() == 1 {
            // Globally unambiguous — the anchor plays no part.
            occs[0].0
        } else {
            let tol = anchor_tolerance(&edit.old_text);
            let mut ranked: Vec<(usize, usize, usize)> = occs
                .iter()
                .map(|&(byte, line)| (byte, line, line.abs_diff(anchor as usize)))
                .collect();
            ranked.sort_by_key(|(_, _, d)| *d);
            let (nearest_byte, _nearest_line, d1) = ranked[0];
            let d2 = ranked[1].2;
            if !diagnose::anchor_accepts(d1, d2, tol) {
                let candidates: Vec<(usize, usize)> =
                    ranked.iter().take(10).map(|&(_, l, d)| (l, d)).collect();
                failures.push(batch_item(
                    i,
                    edit,
                    EditFailure::anchor_out_of_tolerance(
                        i + 1,
                        anchor as usize,
                        tol,
                        occs.len(),
                        &candidates,
                    ),
                ));
                continue;
            }
            if ranked.iter().filter(|(_, _, d)| *d == d1).count() > 1 {
                let lines: Vec<usize> = ranked.iter().take(10).map(|&(_, l, _)| l).collect();
                failures.push(batch_item(
                    i,
                    edit,
                    EditFailure::anchor_ambiguous(i + 1, anchor as usize, &lines),
                ));
                continue;
            }
            nearest_byte
        };
        // Disjointness: reject (never re-aim) on overlap with an earlier
        // anchored match.
        let end = chosen + edit.old_text.len();
        if let Some(ci) = first_claim_overlap(&claims, chosen, end) {
            let (other_i, cs, ce) = claims[ci];
            failures.push(batch_item(
                i,
                edit,
                EditFailure::overlap_conflict(
                    i + 1,
                    span_line_range(text, chosen, end),
                    other_i + 1,
                    span_line_range(text, cs, ce),
                ),
            ));
            continue;
        }
        claims.push((i, chosen, end));
        resolved[i] = Some(chosen);
    }

    // ---- Pass 2: content-only blocks, in block order ----------------------
    for (i, edit) in edits.iter().enumerate() {
        if edit.start_line.is_some() {
            continue; // handled (or failed) in pass 1
        }
        let occs = occurrences_of(text, &edit.old_text);
        if occs.is_empty() {
            failures.push(not_found_failure(i, edit, text));
            continue;
        }
        let old_len = edit.old_text.len();
        let mut free: Vec<(usize, usize)> = Vec::new(); // (byte, line)
        let mut excluded = 0usize;
        let mut claimers: Vec<usize> = Vec::new(); // 1-based edit indices
        for &(byte, line) in &occs {
            if let Some(ci) = first_claim_overlap(&claims, byte, byte + old_len) {
                excluded += 1;
                let editor = claims[ci].0 + 1;
                if !claimers.contains(&editor) {
                    claimers.push(editor);
                }
            } else {
                free.push((byte, line));
            }
        }
        claimers.sort_unstable();

        if free.is_empty() {
            let lines: Vec<usize> = occs.iter().take(10).map(|&(_, l)| l).collect();
            failures.push(batch_item(
                i,
                edit,
                EditFailure::all_candidates_claimed(i + 1, occs.len(), &lines, &claimers),
            ));
            continue;
        }
        if free.len() > 1 {
            let free_lines: Vec<usize> = free.iter().take(10).map(|&(_, l)| l).collect();
            failures.push(batch_item(
                i,
                edit,
                EditFailure::non_unique(i + 1, occs.len(), &free_lines, excluded),
            ));
            continue;
        }
        let (chosen, _line) = free[0];
        claims.push((i, chosen, chosen + old_len));
        resolved[i] = Some(chosen);
    }

    // Stable, block-ordered diagnostics regardless of which pass produced them.
    failures.sort_by_key(|f| f.edit_index);
    (claims, resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_tolerance_scales_then_caps() {
        assert_eq!(anchor_tolerance("x"), 0);
        assert_eq!(anchor_tolerance("a\nb\nc"), 1);
        assert_eq!(anchor_tolerance("a\nb\nc\nd\ne\nf"), 2);
        assert_eq!(anchor_tolerance(&"l\n".repeat(12)), 4);
        assert_eq!(anchor_tolerance(&"l\n".repeat(120)), 4);
    }

    fn op(old: &str, new: &str, anchor: Option<u64>) -> EditOp {
        EditOp {
            old_text: old.to_string(),
            new_text: new.to_string(),
            start_line: anchor.map(crate::types::EditStartLine),
            origin: None,
        }
    }

    /// The claim that makes `context = 0` viable: N identical lines, each
    /// anchored to its own line, all resolve to the occurrence the anchor
    /// names — no uniqueness requirement, no tolerance needed.
    #[test]
    fn forty_duplicate_lines_all_resolve_by_anchor() {
        let text: String = (1..=40).map(|i| format!("line {i} run();\n")).collect();
        let dup = "run();";
        // Make every line identical in the OLD text we search for.
        let flat: String = (0..40).map(|_| format!("{dup}\n")).collect();
        let _ = text;
        let edits: Vec<EditOp> = (0..40)
            .map(|i| op(dup, "walk();", Some(i as u64 + 1)))
            .collect();
        let mut failures = Vec::new();
        let (_, resolved) = resolve_edits(&flat, &edits, &mut failures);
        assert!(failures.is_empty(), "unexpected failures: {failures:?}");
        assert_eq!(resolved.len(), 40);
        assert!(resolved.iter().all(|r| r.is_some()));
        // Each resolved offset is the one for its own line.
        for (i, r) in resolved.iter().enumerate() {
            let want = flat.match_indices(dup).nth(i).expect("occurrence").0;
            assert_eq!(*r, Some(want), "block {i} aimed at the wrong line");
        }
    }

    /// A stale anchor — one that no longer names a matching line — must fail
    /// loudly rather than apply to a neighbouring copy. Here the two
    /// candidates are 19 and 20 lines from the anchor, so the skew rule (which
    /// needs the runner-up to be 3x farther) refuses both.
    #[test]
    fn stale_anchor_on_duplicates_fails_cleanly() {
        let mut lines: Vec<String> = (1..=40).map(|i| format!("line {i}\n")).collect();
        lines[0] = "run();\n".into(); // line 1
        lines[39] = "run();\n".into(); // line 40
        let text: String = lines.concat();
        let edits = vec![op("run();", "walk();", Some(20))];
        let mut failures = Vec::new();
        let (_, resolved) = resolve_edits(&text, &edits, &mut failures);
        assert!(resolved[0].is_none());
        assert_eq!(failures.len(), 1);
        // The diagnostic must name both candidates and the reason, so the
        // model can re-anchor instead of guessing.
        let mv = &failures[0].failure.model_view;
        assert!(mv.contains("line 1"), "{mv}");
        assert!(mv.contains("line 40"), "{mv}");
        assert!(mv.contains("3×"), "{mv}");
        // The UI view stays short and never echoes file content.
        assert!(
            !failures[0].failure.ui_view.contains("run();"),
            "{:?}",
            failures[0]
        );
    }

    #[test]
    fn overlap_between_blocks_is_rejected() {
        let text = "aaa bbb\n";
        let edits = vec![op("aaa", "x", Some(1)), op("aaa bbb", "y", Some(1))];
        let mut failures = Vec::new();
        let _ = resolve_edits(text, &edits, &mut failures);
        assert_eq!(failures.len(), 1, "expected one conflict: {failures:?}");
    }
}
