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
        let batch = diagnose::BatchEditFailure::build(total, failures);
        return ApplyOutcome::Failed {
            report: diagnose::render_batch(&batch),
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
    replacements.sort_by(|a, b| b.0.cmp(&a.0));
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
pub fn success_note(edits: &[EditOp], matched_lines: &[usize], shown: &str) -> String {
    // When no anchor was supplied, echo where each block actually matched. The
    // `path:LINE` idiom shown here is the same anchor syntax the blob form
    // accepts, so the model learns it from its own transcript. Only shown when
    // *no* edit anchored (`matched_lines` then lines up with `edits`); if the
    // model already anchored, it needs no reminder.
    let matched_note = if edits.iter().any(|e| e.start_line.is_some()) {
        String::new()
    } else {
        match matched_lines {
            [] => String::new(),
            [l] => format!(" (matched at {shown}:{l})"),
            lines if lines.len() <= 3 => {
                let list = lines
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" (matched lines {list})")
            }
            lines => format!(" (matched {} lines)", lines.len()),
        }
    };
    format!(
        "Replaced {} block(s) in {}{}",
        edits.len(),
        shown,
        matched_note
    )
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
