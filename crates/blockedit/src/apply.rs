//! Applying a resolved batch of edits to text, and (for the `edit` tool) to a
//! file.
//!
//! [`apply_to_text`] is the whole of the interesting logic and is pure, so the
//! `bsed apply` CLI and the agent share exactly one implementation. The file
//! wrapper keeps the all-or-nothing contract: on any failure **nothing** is
//! written.

use std::path::Path;

use crate::diagnose::{self, BatchItem};
use crate::resolve::{resolve_edits, span_line_range};
use crate::types::EditOp;

/// The outcome of applying a batch: either new text, or the rendered failure
/// describing every block that could not be resolved.
pub enum ApplyOutcome {
    Applied {
        text: String,
        /// 1-based start line of each edit, in the *original* text. Used to
        /// teach the `path:LINE` anchor by example when the model did not
        /// supply one — computed from the original because the offsets that
        /// produced them target it.
        matched_lines: Vec<usize>,
    },
    Failed {
        /// The rendered batch failure, ready to hand back to the model.
        report: String,
        /// 0-based indices of the edits that failed, in batch order. The
        /// rendered report says *what* went wrong; this says *which* blocks, so
        /// a caller can hand back exactly those for a retry instead of the whole
        /// batch. (The `edit` tool does not need it — it shows the model
        /// everything — but `bsed apply` does, to re-emit just the failures.)
        failed: Vec<usize>,
    },
}

/// Apply `edits` to `text`, all-or-nothing.
///
/// Returns [`ApplyOutcome::Failed`] with a full report when any block does not
/// resolve, so the model can fix every problem in one retry instead of
/// ping-ponging. The report lists failures in block order.
pub fn apply_to_text(text: &str, edits: &[EditOp]) -> ApplyOutcome {
    let total = edits.len();
    let mut failures: Vec<BatchItem> = Vec::new();
    let (_claims, resolved) = resolve_edits(text, edits, &mut failures);

    if !failures.is_empty() {
        // `resolve_edits` sorts failures by index, so this is batch order.
        let failed: Vec<usize> = failures.iter().map(|f| f.edit_index).collect();
        let batch = diagnose::BatchEditFailure::build(total, failures);
        return ApplyOutcome::Failed {
            report: diagnose::render_batch(&batch),
            failed,
        };
    }

    // Compute all offsets against the original text first, then apply from end
    // to start. Edits target the original file, not the output of previous
    // edits. `resolved` holds the validated match offset per edit (all Some
    // when there were no failures).
    debug_assert_eq!(resolved.len(), edits.len());
    let resolved: Vec<usize> = resolved
        .into_iter()
        .map(|o| o.expect("resolve_edits: no failures implies all resolved"))
        .collect();

    // Match line of each edit, from the *original* text.
    let matched_lines: Vec<usize> = resolved
        .iter()
        .map(|off| diagnose::count_lines(&text[..*off]) + 1)
        .collect();

    let mut replacements: Vec<(usize, usize, String)> = Vec::new(); // (start, end, new_text)
    for (edit, abs_start) in edits.iter().zip(resolved.iter()) {
        let abs_start = *abs_start;
        let abs_end = abs_start + edit.old_text.len();
        replacements.push((abs_start, abs_end, edit.new_text.clone()));
    }

    // Sort by start position descending so we can apply from end to start.
    replacements.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut result = text.to_string();
    for (start, end, new_text) in replacements {
        result.replace_range(start..end, &new_text);
    }

    ApplyOutcome::Applied {
        text: result,
        matched_lines,
    }
}

/// How a caller should describe a successful apply. `shown` is the path the
/// caller wants in the message (usually cwd-relative).
///
/// No match location is reported: successful history now carries the resolved
/// line anchor in the block header (`path:LINE`), which is the single source of
/// truth for where a block applied. Echoing a second, pre-trim line here used to
/// disagree with that anchor and confuse the model, so it was dropped.
pub fn success_note(edits: &[EditOp], shown: &str) -> String {
    format!("Replaced {} block(s) in {}", edits.len(), shown)
}

/// Apply `edits` to the file at `path`, or write nothing and return the failure
/// report. Kept here (rather than in the agent) so `bsed apply` and the `edit`
/// tool cannot drift apart.
pub fn apply_to_file(path: &Path, edits: &[EditOp]) -> std::io::Result<ApplyOutcome> {
    let text = std::fs::read_to_string(path)?;
    Ok(apply_to_text(&text, edits))
}

/// The 1-based line span each edit covers in the original text — for callers
/// that want to report a map rather than a note.
pub fn matched_spans(text: &str, edits: &[EditOp], resolved: &[usize]) -> Vec<(usize, usize)> {
    edits
        .iter()
        .zip(resolved.iter())
        .map(|(e, &off)| span_line_range(text, off, off + e.old_text.len()))
        .collect()
}
