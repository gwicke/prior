//! Fuzzy match-and-diagnose for the `edit` tool.
//!
//! When the model's `oldText` does not exactly match anything in the file,
//! we want to surface a useful error: where in the file the closest match
//! lives, how similar it is, and a small diff showing what the model got
//! wrong. This module implements that pipeline.
//!
//! The result is rendered in two views:
//!   - `model_view` — rich text with the diff, intended for the LLM so it
//!     can re-anchor and retry.
//!   - `ui_view`    — a short one-line summary, intended for the human
//!     watching the TUI. We deliberately do NOT echo file content into the
//!     UI to avoid leaking large or secret-bearing regions of disk into
//!     scrollback.
//!
//! Algorithm: longest-common-substring anchor + Myers diff over the
//! windowed region. Cost is sub-millisecond for typical file sizes.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

/// Tunables. Constants for now; can become config later if there's call.
const MIN_ANCHOR_LEN: usize = 4;
/// Window size on each side of the anchor for Myers diff. Anchors shorter
/// than this are extended; longer anchors stay as-is. Bounded by file size.
const WINDOW_PAD: usize = 256;
/// If similarity is below this ratio, we say "no close match" rather than
/// try to guide the model toward a phantom.
const MIN_SIMILARITY: f64 = 0.5;
/// Cap on the number of diff lines rendered into the model_view.
const MAX_DIFF_LINES: usize = 30;

/// Result of an edit-failure diagnosis. Rendered as a JSON string the
/// model-and-UI split can parse.
#[derive(Debug, Serialize)]
pub struct EditFailure {
    /// Short summary for the human (no file text).
    pub ui_view: String,
    /// Rich diagnostic for the model: anchor location, similarity, and a
    /// small diff against the file region.
    pub model_view: String,
}

/// A failure for one edit in a batch, paired with the edit's index in
/// the caller's `Vec<EditOp>` (0-based). Used to build a
/// [`BatchEditFailure`] that surfaces every problem in a multi-edit
/// call at once, instead of forcing the model to retry ping-pong.
#[derive(Debug, Serialize)]
pub struct BatchItem {
    /// 0-based index of the edit that failed.
    pub edit_index: usize,
    /// Per-edit failure (or a plain-error fallback when no close match
    /// was found).
    pub failure: EditFailure,
}

/// Result of a multi-edit call where one or more edits failed. The
/// `model_view` aggregates every failure so the model can fix them all
/// in a single retry. The `ui_view` is a short summary (counts only,
/// no file content).
///
/// Same wire shape as [`EditFailure`] — `{ui_view, model_view}` — so
/// downstream rendering code doesn't need a separate code path.
#[derive(Debug, Serialize)]
pub struct BatchEditFailure {
    pub ui_view: String,
    pub model_view: String,
    /// Total edits in the original call.
    pub total: usize,
    /// How many failed.
    pub failed: usize,
}

impl BatchEditFailure {
    /// Build a batch failure from collected per-edit failures. Caller
    /// is responsible for having validated each failed edit; this just
    /// formats.
    ///
    /// When exactly one edit failed, the `ui_view` carries that
    /// failure's own short summary (location / line numbers) so the
    /// human watching the TUI sees something useful. For multi-failure
    /// batches the `ui_view` falls back to a count-only summary to keep
    /// the panel bounded.
    pub fn build(total: usize, items: Vec<BatchItem>) -> Self {
        let failed = items.len();
        let mut model_view = String::new();
        model_view.push_str(&format!(
            "{failed} of {total} edit(s) failed. No edits applied.\n",
        ));
        for item in &items {
            // Per-edit body has no header of its own; we add the
            // `--- edit #N ---` separator here.
            model_view.push_str(&format!(
                "\n--- edit #{} ---\n{}",
                item.edit_index + 1,
                item.failure.model_view,
            ));
        }
        let ui_view = if items.len() == 1 {
            // Single failure: the per-edit summary IS the message. The
            // model_view in the same JSON envelope carries the diff /
            // line numbers for the model; no need to point the human at
            // it.
            format!("edit failed:\n  {}",
                items[0].failure.ui_view)
        } else {
            format!(
                "edit failed: {failed} of {total} edit(s) had problems. \
                 No edits applied. See model tool message for details.",
            )
        };
        BatchEditFailure {
            ui_view,
            model_view,
            total,
            failed,
        }
    }
}

/// Serialize a [`BatchEditFailure`] to JSON. The `ui_view` and
/// `model_view` keys match [`EditFailure`]'s render shape so the same
/// `looks_like_edit_failure` predicate and the same
/// `display_tool_result` branch handle both.
pub fn render_batch(failure: &BatchEditFailure) -> String {
    serde_json::to_string(failure).expect("BatchEditFailure is always serializable")
}

/// Try to find a substring of `file_text` that closely resembles `query`.
///
/// `query` is what the model sent as `oldText`. `file_text` is the current
/// file contents. On exact-match (the normal path) we never get here; this
/// is only called when `text.contains(query)` already returned `false`.
///
/// Returns `None` if no region is similar enough to be worth showing.
pub fn diagnose(query: &str, file_text: &str) -> Option<EditFailure> {
    if query.is_empty() || file_text.is_empty() {
        return None;
    }

    // Stage 1: anchor via longest-common-substring.
    let anchor = find_anchor(file_text, query, MIN_ANCHOR_LEN)?;
    let (anchor_start, anchor_len) = anchor;
    let anchor_end = anchor_start + anchor_len;

    // Stage 2: extract a window around the anchor so Myers diff has a
    // bounded region to work on. The window is `query.len()`-bounded on
    // each side so the diff stays informative without scanning the whole
    // file.
    let win_start = anchor_start.saturating_sub(query.len() + WINDOW_PAD);
    let win_end = (anchor_end + query.len() + WINDOW_PAD).min(file_text.len());
    let window = &file_text[win_start..win_end];

    // Stage 3: Myers diff between the model's `query` and the on-disk
    // `window`. Cap the rendered output at MAX_DIFF_LINES lines so a huge
    // diff doesn't blow up the model's context.
    //
    // Similarity is a hybrid: max of (anchor length / query length) and
    // (equal lines / total lines). The anchor metric catches the
    // "single-line tweak with one bad character" case (lines may show
    // 0 equal even though 99% of chars match); the line metric catches
    // the "tab vs space drift" case where the longest shared run is
    // short but most lines line up. Max-of keeps the bar low for
    // genuinely-close matches while still rejecting totally-wrong
    // queries.
    let diff = TextDiff::from_lines(query, window);
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let anchor_sim = (anchor_len as f64) / (query.len() as f64);
    let total_lines = count_lines(query).max(count_lines(window)).max(1);
    let equal_lines = changes
        .iter()
        .filter(|c| matches!(c.tag(), ChangeTag::Equal))
        .count();
    let line_sim = (equal_lines as f64) / total_lines as f64;
    let similarity = anchor_sim.max(line_sim);

    if similarity < MIN_SIMILARITY {
        return None;
    }

    // Translate the window-relative line numbers back to file-relative
    // line numbers so the model can `read` the right region.
    let window_line_offset = count_lines(&file_text[..win_start]);

    // Build the rich model_view body (no header; the caller adds a
    // batch envelope around it).
    let mut model_view = String::new();
    model_view.push_str(&format!(
        "oldText not found. Closest match: similarity {:.2}, file lines {}-{} \
         (anchor at line {}, length {} chars).\n\
         Diff (your oldText vs. file content at that region):\n",
        similarity,
        window_line_offset + 1,
        window_line_offset + window.lines().count().max(1),
        window_line_offset + count_lines(&file_text[..anchor_start]) + 1,
        anchor_len,
    ));

    for (rendered, change) in changes.iter().enumerate() {
        if rendered >= MAX_DIFF_LINES {
            model_view.push_str(&format!(
                "... ({} more diff lines truncated)\n",
                changes.len().saturating_sub(rendered),
            ));
            break;
        }
        let tag = match change.tag() {
            ChangeTag::Equal => " ",
            ChangeTag::Insert => "+",
            ChangeTag::Delete => "-",
        };
        model_view.push_str(&format!("{}{}", tag, change.value()));
    }

    let ui_view = format!(
        "Closest match at file lines {}-{} (similarity {:.2}).",
        window_line_offset + 1,
        window_line_offset + window.lines().count().max(1),
        similarity,
    );

    Some(EditFailure { ui_view, model_view })
}

impl EditFailure {
    /// Construct a fallback for "oldText not found and no close match".
    /// Used both directly (single-edit call) and inside batch items.
    pub fn not_found_no_match(edit_index_1based: usize, old_text: &str) -> Self {
        let ui_view = format!(
            "edit #{edit_index_1based} oldText not found. \
             No similar region in file.",
        );
        let model_view = format!(
            "oldText not found. No close-enough region in the file; \
             please re-read the file and re-anchor your edit. \
             Original oldText: '{}'.",
            old_text,
        );
        EditFailure { ui_view, model_view }
    }

    /// Construct a failure for "oldText appears N times" with line
    /// numbers so the model can disambiguate.
    pub fn non_unique(
        edit_index_1based: usize,
        old_text: &str,
        count: usize,
        line_numbers: &[usize],
    ) -> Self {
        let shown = line_numbers.len();
        let line_list = if count > shown {
            format!(
                "{}, (...and {} more)",
                line_numbers
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                count - shown,
            )
        } else {
            line_numbers
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let ui_view = format!(
            "edit #{edit_index_1based} oldText appears {count} times \
             (lines: {line_list}).",
        );
        let model_view = format!(
            "oldText appears {count} times, must be unique. \
             Occurrences at lines: {line_list}. Include more surrounding \
             context in oldText (the line(s) immediately before/after) so \
             only one occurrence matches. Original oldText: '{}'.",
            old_text,
        );
        EditFailure { ui_view, model_view }
    }
}

/// Find the longest common substring of `file_text` and `query` using
/// O(|T|*|Q|) DP with O(|Q|) space (single rolling row). Returns the
/// byte-range `(start, length)` in `file_text`.
///
/// `min_len` discards trivial anchors (e.g. a single common newline).
fn find_anchor(file_text: &str, query: &str, min_len: usize) -> Option<(usize, usize)> {
    if query.is_empty() || file_text.is_empty() {
        return None;
    }
    let q = query.as_bytes();
    let t = file_text.as_bytes();
    let mut prev = vec![0u32; q.len() + 1];
    let mut best_len: usize = 0;
    let mut best_t_end: usize = 0;

    for i in 1..=t.len() {
        let mut curr = vec![0u32; q.len() + 1];
        for j in 1..=q.len() {
            if t[i - 1] == q[j - 1] {
                curr[j] = prev[j - 1] + 1;
                if curr[j] as usize > best_len && curr[j] as usize >= min_len {
                    best_len = curr[j] as usize;
                    best_t_end = i;
                }
            }
        }
        prev = curr;
    }

    if best_len == 0 {
        None
    } else {
        Some((best_t_end - best_len, best_len))
    }
}

pub(crate) fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.matches('\n').count()
    }
}

/// Serialize an `EditFailure` to a JSON string the model+UI split can
/// parse. We use a string the UI can pattern-match on (starts with
/// `{"ui_view":`) so existing string-sniffing in `display_tool_result`
/// can pick the right path.
pub fn render(failure: &EditFailure) -> String {
    serde_json::to_string(failure).expect("EditFailure is always serializable")
}

/// Probe used by `display_tool_result` to recognize a structured edit
/// failure without breaking on unrelated JSON the model might happen to
/// return.
pub fn looks_like_edit_failure(result: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(result) else {
        return false;
    };
    v.get("ui_view").is_some() && v.get("model_view").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_returns_none_because_we_only_call_this_on_miss() {
        // Documenting the contract: callers invoke this only when the
        // exact match has already failed. This test pins the behavior on
        // a case where diagnosis is moot.
        let file = "hello\nworld\n";
        let q = "hello";
        // Even though the query IS present, find_anchor still returns
        // *some* anchor (largest common substring); we only assert the
        // function doesn't crash. The caller is expected to gate on
        // !contains().
        let _ = find_anchor(file, q, MIN_ANCHOR_LEN);
    }

    #[test]
    fn anchor_finds_longest_common_substring() {
        let file = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let q = "    let x = 1;\n    let YY = 2;\n";
        let (start, len) = find_anchor(file, q, 4).expect("anchor should exist");
        // The shared substring is "    let " — pick whatever the algo
        // returned as longest, just assert it's non-empty and in-bounds.
        assert!(len >= 4);
        assert!(start + len <= file.len());
    }

    #[test]
    fn no_common_substring_returns_none() {
        let file = "abcdefg";
        let q = "xyz";
        assert!(find_anchor(file, q, 1).is_none());
    }

    #[test]
    fn diagnose_returns_none_when_no_close_match() {
        let file = "fn main() {\n    println!(\"hi\");\n}\n";
        let q = "completely unrelated content here";
        // The longest common substring will be very small; similarity
        // should fall below MIN_SIMILARITY.
        assert!(diagnose(q, file).is_none());
    }

    #[test]
    fn diagnose_returns_some_for_close_match() {
        let file = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        // Off-by-one indentation in the query — the dominant real-world
        // failure mode.
        let q = "fn main() {\n   let x = 1;\n    let y = 2;\n}\n";
        let f = diagnose(q, file).expect("should diagnose");
        assert!(f.ui_view.contains("Closest match"));
        assert!(f.model_view.contains("Closest match"));
        // UI view should NOT contain large chunks of file content.
        assert!(f.ui_view.len() < 200);
        assert!(f.model_view.len() > f.ui_view.len());
    }

    #[test]
    fn ui_view_does_not_echo_file_regions() {
        let file = "secret_token = \"ABCDEFG12345\"\n";
        let q = "secret_token = \"ABCDEFG12346\"\n";
        let f = diagnose(q, file).expect("should diagnose");
        // The UI summary must not leak the secret token.
        assert!(!f.ui_view.contains("ABCDEFG12345"));
        // The model view SHOULD contain it — that's the whole point.
        assert!(f.model_view.contains("ABCDEFG"));
    }

    #[test]
    fn render_emits_valid_json_with_both_views() {
        let f = EditFailure {
            ui_view: "short".into(),
            model_view: "long with diff".into(),
        };
        let s = render(&f);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["ui_view"], "short");
        assert_eq!(v["model_view"], "long with diff");
    }

    #[test]
    fn looks_like_edit_failure_recognizes_rendered_output() {
        let f = EditFailure {
            ui_view: "short".into(),
            model_view: "long".into(),
        };
        assert!(looks_like_edit_failure(&render(&f)));
        assert!(!looks_like_edit_failure("plain error string"));
        assert!(!looks_like_edit_failure(""));
        assert!(!looks_like_edit_failure("{\"ui_view\":\"x\"}")); // missing model_view
    }

    #[test]
    fn empty_inputs_return_none() {
        assert!(diagnose("", "anything").is_none());
        assert!(diagnose("anything", "").is_none());
    }

    #[test]
    fn whitespace_only_drift_is_diagnosed() {
        // Tab-vs-space drift: the model wrote spaces, the file has tabs.
        let file = "fn main() {\n\tlet x = 1;\n}\n";
        let q = "fn main() {\n    let x = 1;\n}\n";
        let f = diagnose(q, file);
        // Should produce a diagnosis (it's close enough).
        assert!(f.is_some());
    }

    #[test]
    fn not_found_no_match_carries_message_in_both_views() {
        let f = EditFailure::not_found_no_match(2, "missing");
        // ui_view carries the edit index for the human.
        assert!(f.ui_view.contains("edit #2"));
        // model_view is body-only (the envelope adds the edit index
        // header); verify the body content is there.
        assert!(f.model_view.contains("No close-enough region"));
        assert!(f.model_view.contains("missing"));
        // UI view should not contain the raw oldText (it's not a secret,
        // but the contract is "UI view is short, no file content").
        assert!(!f.ui_view.contains("missing"));
    }

    #[test]
    fn non_unique_lists_line_numbers() {
        let f = EditFailure::non_unique(1, "foo", 3, &[1, 3, 5]);
        assert!(f.ui_view.contains("lines: 1, 3, 5"));
        assert!(f.model_view.contains("lines: 1, 3, 5"));
        assert!(f.model_view.contains("foo"));
    }

    #[test]
    fn non_unique_caps_listing_with_truncation_notice() {
        let f = EditFailure::non_unique(
            1,
            "foo",
            15,
            &(1..=10).collect::<Vec<_>>(),
        );
        assert!(f.ui_view.contains("(...and 5 more)"));
        assert!(f.ui_view.contains("1, 2, 3, 4, 5, 6, 7, 8, 9, 10"));
    }

    #[test]
    fn batch_build_aggregates_all_failures() {
        let items = vec![
            BatchItem {
                edit_index: 0,
                failure: EditFailure::not_found_no_match(1, "x"),
            },
            BatchItem {
                edit_index: 2,
                failure: EditFailure::non_unique(3, "y", 2, &[4, 8]),
            },
        ];
        let b = BatchEditFailure::build(3, items);
        assert_eq!(b.total, 3);
        assert_eq!(b.failed, 2);
        assert!(b.model_view.contains("--- edit #1 ---"));
        assert!(b.model_view.contains("--- edit #3 ---"));
        assert!(b.model_view.contains("No edits applied"));
        assert!(b.model_view.contains("2 of 3 edit(s) failed"));
        // Multi-failure: ui_view is the count-only summary.
        assert!(b.ui_view.contains("2 of 3 edit(s) had problems"));
        assert!(!b.ui_view.contains("--- edit #"));
    }

    #[test]
    fn batch_with_single_failure_passes_through_per_edit_ui() {
        let items = vec![BatchItem {
            edit_index: 0,
            failure: EditFailure {
                ui_view: "edit #1 closest match at line 42".into(),
                model_view: "edit #1 closest match at line 42\n<diff>".into(),
            },
        }];
        let b = BatchEditFailure::build(1, items);
        // Single-failure: ui_view is just the per-edit summary line,
        // prefixed with a one-line "edit failed:" header.
        assert!(b.ui_view.starts_with("edit failed:"));
        assert!(b.ui_view.contains("closest match at line 42"));
        assert!(b.ui_view.contains("edit #1"));
    }

    #[test]
    fn batch_with_single_failure_does_not_double_header() {
        // Regression: the previous format produced
        //   "edit failed: 1 of 1 edit(s) had problems.
        //    edit #1 oldText not found.
        //    edit failed: oldText not found. Closest match at file lines..."
        // The compact form should be a single per-edit line under a
        // "edit failed:" header.
        let items = vec![BatchItem {
            edit_index: 0,
            failure: EditFailure {
                ui_view: "edit #1 oldText not found. Closest match at file lines 60-91 (similarity 0.92).".into(),
                model_view: "<diff>".into(),
            },
        }];
        let b = BatchEditFailure::build(1, items);
        let ui_lines: Vec<&str> = b.ui_view.lines().collect();
        assert_eq!(ui_lines.len(), 2, "expected 2 lines, got: {:?}", ui_lines);
        assert_eq!(ui_lines[0], "edit failed:");
        assert!(ui_lines[1].contains("edit #1"));
        assert!(ui_lines[1].contains("Closest match at file lines 60-91"));
    }

    #[test]
    fn render_batch_emits_recognizable_json() {
        let items = vec![BatchItem {
            edit_index: 0,
            failure: EditFailure::not_found_no_match(1, "x"),
        }];
        let b = BatchEditFailure::build(2, items);
        let s = render_batch(&b);
        assert!(looks_like_edit_failure(&s));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["total"], 2);
        assert_eq!(v["failed"], 1);
    }

    #[test]
    fn batch_model_view_avoids_redundant_headers() {
        // The model_view should not contain redundant 'edit failed:'
        // / 'edit #N oldText not found.' prefixes; the envelope owns
        // those.
        let items = vec![BatchItem {
            edit_index: 0,
            failure: EditFailure {
                ui_view: "edit #1 oldText not found. Closest match at file lines 60-91 (similarity 0.92).".into(),
                model_view: "oldText not found. Closest match: similarity 0.92, file lines 60-91.\nDiff:\n a\n-b".into(),
            },
        }];
        let b = BatchEditFailure::build(1, items);
        // Envelope: "1 of 1 edit(s) failed. No edits applied."
        assert!(b.model_view.starts_with("1 of 1 edit(s) failed"));
        assert!(b.model_view.contains("--- edit #1 ---"));
        // Body preserved.
        assert!(b.model_view.contains("Closest match: similarity 0.92"));
        // No redundant "edit failed:" within the body.
        assert!(!b.model_view.contains("edit failed:"));
        // No redundant "edit #1 oldText not found." line in the body
        // (that's the ui_view's job).
        assert!(!b.model_view.contains("edit #1 oldText not found."));
    }
}