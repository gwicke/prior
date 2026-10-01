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

use crate::wire;

/// Tunables. Constants for now; can become config later if there's call.
const MIN_ANCHOR_LEN: usize = 4;
/// If the anchor is shorter than this fraction of `oldText`, we say "no close
/// match" rather than point the model at a phantom.
const MIN_ANCHOR_RATIO: f64 = 0.5;
/// Lines of file context shown on each side of the divergent region.
const CONTEXT_PAD: usize = 2;
/// Cap on rendered context lines, split evenly around the divergence.
///
/// Unlike the diff cap this replaces, eliding is **non-destructive**: every
/// rendered line keeps its own true line number, so a truncated region is still
/// directly usable by the model. That property is why the region is worth
/// showing at all.
const MAX_CONTEXT_LINES: usize = 40;
/// Cap on the echoed `oldText` when it is the only thing identifying a block.
const MAX_ECHOED_OLD_LINES: usize = 20;

/// Skew rule: for an anchor beyond the context tolerance, accept the nearest
/// candidate only if it is at least this many times closer than the second
/// candidate (`d2 >= RATIO * d1`). Ties never satisfy this for `RATIO > 1`.
pub const ANCHOR_SKEW_RATIO: usize = 3;
/// Skew rule hard cap: never accept a nearest candidate farther than this
/// many lines from the anchor, however skewed the ratio is. Bounds the damage
/// of a stale/garbage anchor on a large file.
pub const ANCHOR_SKEW_CAP: usize = 60;

/// Should an anchored nearest-match choice of distance `d1` (with second
/// candidate at `d2`) be accepted, given the context-scaled `tolerance`?
///
/// Additive rule: inside the old tolerance window always; beyond it, only
/// when the nearest candidate is both decisively closer than the second
/// (`d2 >= [ANCHOR_SKEW_RATIO] * d1`) and within [`ANCHOR_SKEW_CAP`] lines —
/// this rescues stale anchors (file shifted since the model read it) while
/// refusing close calls at large deviation. Ties (`d1 == d2`) only pass via
/// the tolerance clause, and callers separately reject ties.
///
/// Only meaningful with ≥ 2 candidates; callers short-circuit unique matches.
pub fn anchor_accepts(d1: usize, d2: usize, tolerance: usize) -> bool {
    d1 <= tolerance || (d1 <= ANCHOR_SKEW_CAP && d2 >= ANCHOR_SKEW_RATIO * d1)
}

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
    /// The block's origin string exactly as the model spelled it, for naming
    /// the failure. `None` when the call could not supply one.
    pub origin: Option<String>,
    /// Whether the model supplied a line anchor for this block. Not derived
    /// from `origin` — a replayed session has no origin string at all, and
    /// guessing an anchor back out of text is exactly the reconstruction this
    /// field exists to avoid.
    pub anchored: bool,
    /// The block's `oldText`, carried so [`BatchEditFailure::build`] can decide
    /// whether it is needed. `build` holds `total` and the whole failure list,
    /// so it is the only place that can tell whether the edit index is
    /// ambiguous; the per-failure constructors cannot. Already in memory.
    pub old_text: String,
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
    ///
    /// Each failure is led by the block's **origin string**, verbatim — the
    /// `src/foo.rs:140` header line the model itself wrote. In a batch of ten
    /// that is a literal string the model can find in its own message, which
    /// is a far better identifier than an ordinal it would have to count out.
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
                lead_with_origin(&item.failure.model_view, item),
            ));
            // An ordinal plus a bare path is enough only when the model can
            // trust the ordinal. See `needs_old_text`.
            if needs_old_text(item, total) {
                model_view.push_str(&echo_old_text(&item.old_text));
            }
        }
        let ui_view = if items.len() == 1 {
            // Single failure: the per-edit summary IS the message. The
            // model_view in the same JSON envelope carries the diff /
            // line numbers for the model; no need to point the human at
            // it.
            format!("edit failed:\n  {}", items[0].failure.ui_view)
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

/// Prefix a per-edit body with the block's origin string.
///
/// `Your block \`src/foo.rs:140\` failed: …` — the string is the model's own,
/// reproduced unchanged, so the model can match it against the header line it
/// wrote rather than translate an ordinal. A failure with no origin (a replayed
/// session carries none) keeps its body as-is rather than inventing one.
fn lead_with_origin(body: &str, item: &BatchItem) -> String {
    match item.origin.as_deref() {
        Some(origin) => format!("Your block `{origin}` failed: {body}"),
        None => body.to_string(),
    }
}

/// Must this failure fall back to echoing the model's `oldText` to be
/// identified?
///
/// The origin string usually settles it — but only when the model supplied a
/// line anchor, because that is what makes the string unique within a file. With
/// several blocks in one file and no anchors, the best identifier left is
/// `edit #N`, and an ordinal the model must count out across a batch it may be
/// reading ten blocks of is not an identifier, it is a puzzle.
///
/// Keyed on `total`, the number of blocks in this file, **not** on how many
/// failed: a ten-block file with one failure is exactly the case where `edit #4`
/// forces the model to count. A single-block call never needs it, because
/// `edit #1` is unambiguous.
fn needs_old_text(item: &BatchItem, total: usize) -> bool {
    total > 1 && !item.anchored
}

/// Render the model's `oldText` as an identifier fallback: fenced, so it cannot
/// be confused with the `NNNN| ` numbered file lines it sits under.
fn echo_old_text(old_text: &str) -> String {
    let lines: Vec<&str> = old_text.split('\n').collect();
    let shown = lines.len().min(MAX_ECHOED_OLD_LINES);
    // Lead with a newline: the per-edit body does not end in one, and running
    // the two together would weld the last context line to this heading.
    let mut out = String::from("\nThe text you sent for this block was:\n```\n");
    for l in &lines[..shown] {
        out.push_str(l);
        out.push('\n');
    }
    if lines.len() > shown {
        out.push_str(&wire::elision_notice(lines.len() - shown));
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

/// Try to find a region of `file_text` that closely resembles `query`.
///
/// `query` is what the model sent as `oldText`. `file_text` is the current
/// file contents. On exact-match (the normal path) we never get here; this
/// is only called when `text.contains(query)` already returned `false`.
///
/// The whole file is searched: the anchor only disambiguates between *exact*
/// matches, so it plays no part in diagnosing a miss.
///
/// **Returns file content in the same format as `read`** — `   12| line` —
/// not a diff. The diff this replaced showed the model a padded window wearing
/// `-`/`+` markers, capped at 30 rows from the front, so the rows that differed
/// were usually the ones cut. A model can act on numbered lines; it cannot act
/// on a diff of a region it has to re-derive.
///
/// Returns `None` if no region is similar enough to be worth showing.
pub fn diagnose(query: &str, file_text: &str) -> Option<EditFailure> {
    if query.is_empty() || file_text.is_empty() {
        return None;
    }

    let search_text = file_text;

    // Stage 1: anchor via longest-common-substring. Its job is to seed the
    // locator below and to answer "is anything related here at all".
    let (anchor_start, anchor_len) = find_anchor(search_text, query, MIN_ANCHOR_LEN)?;

    // Stage 2: Myers over a window sized to the **query**, not to the file. The
    // window used to be padded by a fixed 256 bytes on each side, which could
    // only inflate the equal-line count — padding lines match nothing in the
    // query but do count as context, so a long near-match scored ~0.99 and was
    // then rendered as thirty identical lines. Sizing the window to `oldText`
    // keeps the window honest *and* still covers the whole query: the anchor is
    // itself a large fraction of it.
    let anchor_end = anchor_start + anchor_len;
    let win_start = line_start_at(search_text, anchor_start.saturating_sub(query.len()));
    let win_end = line_end_at(
        search_text,
        (anchor_end + query.len()).min(search_text.len()),
    );
    let window = &search_text[win_start..win_end];
    let offset = count_lines(&search_text[..win_start]) + 1;

    // Two signals, max-of. The anchor ratio catches the "one bad character in a
    // long block" case, where whole lines differ and no line is equal even
    // though most characters match. The line ratio catches indentation drift,
    // where the shared run is short but most lines still line up — the
    // dominant real-world failure, and one the anchor ratio alone rejects.
    // Neither alone is sufficient; the max is not an inflation risk once the
    // window holds nothing but query-sized context.
    //
    // The ratio is a gate only. It is deliberately **not reported**: a number
    // the model cannot act on, which was previously both misleading (0.99 for a
    // region that rendered as pure context) and the loudest thing in the
    // message.
    let diff = TextDiff::from_lines(query, window);
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let anchor_ratio = (anchor_len as f64) / (query.len() as f64);
    let span_lines = count_lines(query).max(count_lines(window)).max(1);
    let equal_lines = changes
        .iter()
        .filter(|c| matches!(c.tag(), ChangeTag::Equal))
        .count();
    let line_ratio = (equal_lines as f64) / span_lines as f64;
    if anchor_ratio.max(line_ratio) < MIN_ANCHOR_RATIO {
        return None;
    }

    // Stage 3: which of those window lines actually differ. Change indices come
    // from the *new* (window) side: `TextDiff::from_lines` puts the query on the
    // old side, so an `old_index` would name a line of what the model wrote
    // rather than a line of the file it needs to fix.
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    for change in &changes {
        if matches!(change.tag(), ChangeTag::Equal) {
            continue;
        }
        if let Some(i) = change.new_index() {
            lo = lo.min(i);
            hi = hi.max(i);
        }
    }

    let span = if lo == usize::MAX {
        // Nothing changed as far as Myers can tell. A true miss should never get
        // here; fall back to the anchor's own span rather than underflow.
        let first = count_lines(&search_text[..anchor_start]) + 1;
        let last = (count_lines(&search_text[..anchor_end]) + 1).min(total_lines(search_text));
        (first, last.max(first))
    } else {
        (
            (offset + lo).max(1),
            (offset + hi + 1).min(total_lines(search_text)),
        )
    };

    let first = span.0.saturating_sub(CONTEXT_PAD).max(1);
    let last = (span.1 + CONTEXT_PAD).min(total_lines(search_text));
    let last = last.max(first);

    let body = elide_middle(
        &render_numbered(search_text, first, last),
        MAX_CONTEXT_LINES,
    );

    let model_view = format!(
        "@@@OLD lines not found. Closest match at file lines {}-{}:\n{}",
        first, last, body,
    );
    let ui_view = format!("Closest match at file lines {first}-{last}.");

    Some(EditFailure {
        ui_view,
        model_view,
    })
}

/// Total lines in `text`, counting a final line with no trailing newline.
fn total_lines(text: &str) -> usize {
    count_lines(text) + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

/// Render `text` over the 1-based inclusive line span `[first, last]` in `read`'s
/// exact format: `format!("{:4}| {}", line_no, content)`.
///
/// Byte-identical to `read` on purpose (`src/tools.rs`), so the model can lift
/// these lines straight into a `@@@OLD` block and reuse the numbers as a
/// `path:LINE` anchor. That also means it inherits `read`'s `\r`-stripping: on a
/// CRLF file these lines cannot be copied byte-exactly. Consistency with `read`
/// is worth more than a fix applied in one of the two places.
fn render_numbered(text: &str, first: usize, last: usize) -> String {
    let mut out = String::new();
    for (i, line) in text
        .lines()
        .enumerate()
        .skip(first.saturating_sub(1))
        .take(last.saturating_sub(first) + 1)
    {
        // `i + 1`, not `first + i`: `enumerate` numbers the whole stream, so
        // after `skip` its index is already the absolute file line.
        out.push_str(&format!("{:4}| {}\n", i + 1, line));
    }
    out
}

/// Keep the first and last halves of `body` when it exceeds `cap`, collapsing
/// the run between them into one notice.
///
/// Splitting around the middle rather than truncating the tail matters because
/// the divergent lines are usually in the middle of the region — a tail-only cap
/// would drop exactly the lines the model came for.
fn elide_middle(body: &str, cap: usize) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= cap {
        return body.to_string();
    }
    let head = cap / 2;
    let tail = cap - head;
    let elided = lines.len() - cap;
    let mut out = String::new();
    for l in &lines[..head] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&wire::elision_notice(elided));
    out.push('\n');
    for l in &lines[lines.len() - tail..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// The byte offset of the start of the line containing `at`.
///
/// The window is aligned to line boundaries, not merely to char boundaries. The
/// diff this replaced aligned to `floor_char_boundary`, which let the window
/// begin mid-line and emit a torn fragment as its first diff row.
fn line_start_at(text: &str, at: usize) -> usize {
    match text[..at].rfind('\n') {
        Some(i) => i + 1,
        None => 0,
    }
}

/// The byte offset of the first newline at or after `at`, or the end of `text`.
///
/// Exclusive end of a line-aligned span.
fn line_end_at(text: &str, at: usize) -> usize {
    match text[at..].find('\n') {
        Some(i) => at + i,
        None => text.len(),
    }
}

/// Render a 1-based inclusive line span for messages: `line 7` when the span
/// is a single line, `lines 7-9` otherwise.
fn render_line_span((first, last): (usize, usize)) -> String {
    if first == last {
        format!("line {first}")
    } else {
        format!("lines {first}-{last}")
    }
}

impl EditFailure {
    /// Construct a fallback for "oldText not found and no close match".
    ///
    /// `ui_view` is the **fragment only** — `"No similar region in file."` — with
    /// no `edit #N` prefix and no trailing period of its own. The caller supplies
    /// the leading `edit #N oldText not found.`, which every not-found failure
    /// shares; baking it in here as well produced a doubled prefix on the
    /// one path that reaches this constructor.
    pub fn not_found_no_match() -> Self {
        let ui_view = "No similar region in file.".to_string();
        let model_view = "@@@OLD lines not found. No close-enough region in the file; \
             please re-read the file and re-anchor your edit."
            .to_string();
        EditFailure {
            ui_view,
            model_view,
        }
    }

    /// Construct a failure for an anchored choice that is neither within the
    /// context tolerance nor decisively nearer than the second candidate (see
    /// [`anchor_accepts`]).
    ///
    /// `candidates` are `(line, distance-from-anchor)` pairs sorted by
    /// distance (≤10); at least two are expected (unique matches never get
    /// here). Reporting `d1`/`d2` lets the model see exactly which clause of
    /// the rule failed.
    pub fn anchor_out_of_tolerance(
        edit_index_1based: usize,
        anchor: usize,
        tolerance: usize,
        count: usize,
        candidates: &[(usize, usize)],
    ) -> Self {
        let (nearest_line, d1) = candidates[0];
        let second_clause = match candidates.get(1) {
            Some(&(line2, d2)) => format!(
                ", next-nearest at line {line2} is {d2} line(s) away \
                 (needs ≥{ratio}× nearer to decide)",
                ratio = ANCHOR_SKEW_RATIO,
            ),
            None => String::new(),
        };
        let line_list = candidates
            .iter()
            .map(|(l, _)| l.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let shown_more = if count > candidates.len() {
            format!(" (...and {} more)", count - candidates.len())
        } else {
            String::new()
        };
        let ui_view = format!(
            "edit #{edit_index_1based} oldText appears {count} times; nearest at line \
             {nearest_line} is {d1} line(s) from anchor {anchor} (tol {tolerance}), not \
             decisive{second_clause}.",
        );
        let model_view = format!(
            "oldText appears {count} times. Anchor line {anchor}: the nearest occurrence \
             at line {nearest_line} is {d1} line(s) away; the next-nearest is {d2_clause}. \
             An anchor is accepted only when the nearest is within {tolerance} line(s), or \
             when it is at least {ratio}× closer than the next-nearest and within {cap} \
             lines — this pick satisfies neither. All occurrences at lines: {line_list}\
             {shown_more}. The anchor is a near-line hint, not a range. Re-read the file \
             (`read` gives line numbers) and either correct the `path:LINE`/`startLine` \
             anchor or include more surrounding context in oldText so the match is \
             unambiguous.",
            d2_clause = candidates
                .get(1)
                .map(|(l, d)| format!("{d} line(s) (at line {l})"))
                .unwrap_or_else(|| "not far enough behind".to_string()),
            ratio = ANCHOR_SKEW_RATIO,
            cap = ANCHOR_SKEW_CAP,
        );
        EditFailure {
            ui_view,
            model_view,
        }
    }

    /// Construct a failure for two matches whose byte ranges overlap. Both
    /// edits are named with their file line ranges so the model can see which
    /// blocks to merge or reword. All matches in a batch are computed against
    /// the original file, so overlapping regions would corrupt each other.
    pub fn overlap_conflict(
        edit_index_1based: usize,
        my_lines: (usize, usize),
        other_edit_index_1based: usize,
        other_lines: (usize, usize),
    ) -> Self {
        let mine = render_line_span(my_lines);
        let theirs = render_line_span(other_lines);
        let ui_view = format!(
            "edit #{edit_index_1based} ({mine}) overlaps edit #{other_edit_index_1based} \
             ({theirs}); batch edits must not overlap.",
        );
        let model_view = format!(
            "This edit's match ({mine}) overlaps edit #{other_edit_index_1based}'s match \
             ({theirs}). Edits in one batch are all computed against the original \
             file, so their regions must be disjoint — overlapping matches would corrupt \
             each other. Merge the overlapping changes into a single block, or adjust \
             oldText so the regions are disjoint.",
        );
        EditFailure {
            ui_view,
            model_view,
        }
    }

    /// Construct a failure for an un-anchored block whose every candidate
    /// overlaps a match already claimed by another edit in the same batch
    /// (typically the same/overlapping oldText as an earlier block).
    ///
    /// `candidate_lines` are the occurrence lines (≤10), `claimer_edits` the
    /// 1-based indices of the edits whose matches consumed them (deduped).
    pub fn all_candidates_claimed(
        edit_index_1based: usize,
        count: usize,
        candidate_lines: &[usize],
        claimer_edits: &[usize],
    ) -> Self {
        let line_list = candidate_lines
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let claimer_list = claimer_edits
            .iter()
            .map(|n| format!("#{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let ui_view = if count == 1 {
            format!(
                "edit #{edit_index_1based} oldText's only occurrence (line {line_list}) \
                 already matched by edit(s) {claimer_list} in this batch.",
            )
        } else {
            format!(
                "edit #{edit_index_1based} oldText: all {count} occurrence(s) \
                 (lines: {line_list}) already matched by edit(s) {claimer_list} in this batch.",
            )
        };
        let model_view = format!(
            "oldText occurs {count} time(s) in the file (lines: {line_list}), but every \
             occurrence overlaps a match already claimed by edit(s) {claimer_list} in this \
             batch — two blocks cannot target the same region (matches are computed against \
             the original file). Either this block duplicates what the other edit already \
             replaces — drop it — or give it distinct oldText/anchors. ",
        );
        EditFailure {
            ui_view,
            model_view,
        }
    }

    /// Construct a failure for "two or more occurrences are equidistant from
    /// the anchor line", so nearest-match cannot pick one safely.
    pub fn anchor_ambiguous(
        edit_index_1based: usize,
        anchor: usize,
        line_numbers: &[usize],
    ) -> Self {
        let line_list = line_numbers
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let ui_view = format!(
            "edit #{edit_index_1based} oldText ambiguous near anchor line {anchor} \
             (occurrences at lines: {line_list}).",
        );
        let model_view = format!(
            "oldText has multiple occurrences equally close to the anchor line \
             {anchor} (lines: {line_list}). Include more surrounding context in \
             oldText so exactly one occurrence matches.",
        );
        EditFailure {
            ui_view,
            model_view,
        }
    }

    /// Construct a failure for "oldText appears N times and is still
    /// ambiguous after excluding claimed regions", listing the *free*
    /// candidate lines so the model can disambiguate.
    ///
    /// `count` is the total occurrence count in the file; `excluded` how many
    /// were already claimed by other matches in this batch (0 = no claims);
    /// `line_numbers` the free candidate lines (≤10).
    pub fn non_unique(
        edit_index_1based: usize,
        count: usize,
        line_numbers: &[usize],
        excluded: usize,
    ) -> Self {
        let free = count.saturating_sub(excluded);
        let shown = line_numbers.len();
        let line_list = if free > shown {
            format!(
                "{}, (...and {} more)",
                line_numbers
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                free - shown,
            )
        } else {
            line_numbers
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let excluded_note = if excluded > 0 {
            format!(" ({excluded} occurrence(s) already matched by other edits in this batch were excluded)")
        } else {
            String::new()
        };
        let ui_view = format!(
            "edit #{edit_index_1based} oldText appears {count} times{excluded_note} \
             (free lines: {line_list}); anchor it with a `path:LINE` header.",
        );
        let model_view = format!(
            "oldText appears {count} times{excluded_note}, so it is ambiguous. \
             Free occurrences at lines: {line_list}. To pick one, put a `path:LINE` \
             header line just above that block's `{marker}` marker — e.g. use \
             `...:{first}` to target the first free occurrence — which anchors the \
             block to the nearest occurrence (line numbers come from `read`). \
             Alternatively, include more surrounding context in oldText (the \
             line(s) immediately before/after) so only one occurrence matches. \
            ",
            marker = crate::blob::SEARCH_MARKER,
            first = line_numbers.first().copied().unwrap_or(0),
        );
        EditFailure {
            ui_view,
            model_view,
        }
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

/// One file's entry in a multi-file `edit` result.
///
/// `result` is that file's **single-file** result verbatim: a success-note
/// string, or a `{ui_view, model_view}` failure envelope. Reusing the existing
/// result is the point — a per-file report needs no new vocabulary, and a file
/// the model called alone and a file inside a batch are reported identically.
///
/// `path` is the one field the array must add. A result inside an array is no
/// longer sitting beside the assistant call's `path` argument; that association
/// existed only on the wire, and the failure envelope does not carry it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileResult {
    pub path: String,
    /// Verbatim single-file result: a JSON string (success) or object
    /// (failure envelope). Held as a `Value` so the two shapes round-trip
    /// without re-encoding.
    pub result: serde_json::Value,
}

/// Build a multi-file result from per-file `(path, result)` pairs.
///
/// This is a **bare JSON array**, `[{path, result}, …]` — deliberately not
/// wrapped in an object and carrying **no top-level `ui_view`**. Every durable
/// representation of a multi-file call is per-file (model history via W7, disk
/// via W8), so an aggregate roll-up has nowhere durable to live, and the two
/// readers that do see it in flight both want per-file detail. The array is the
/// transport between the tool and those two boundaries.
///
/// It must reach `on_completion_call` and `append_message` **intact**; see the
/// "result must survive to history" section of `doc/batch_editing_v2.md` for why
/// rewriting it earlier is not a shortcut.
pub fn render_file_results(files: &[FileResult]) -> String {
    serde_json::to_string(files).expect("FileResult is always serializable")
}

/// Recognize a multi-file result and read it back, for the display split.
///
/// Returns `None` for anything that is not an array of `{path, result}` — in
/// particular `None` for an array of plain strings, which other tools emit and
/// which the caller renders by its own existing rule.
pub fn parse_file_results(result: &str) -> Option<Vec<FileResult>> {
    let v: serde_json::Value = serde_json::from_str(result).ok()?;
    let arr = v.as_array()?;
    if arr.is_empty() {
        return None;
    }
    arr.iter()
        .map(|e| {
            Some(FileResult {
                path: e.get("path")?.as_str()?.to_string(),
                result: e.get("result")?.clone(),
            })
        })
        .collect()
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
    fn the_multi_file_result_is_a_bare_array_of_per_file_results() {
        // No wrapper, no top-level ui_view: the array IS the result.
        let files = vec![
            FileResult {
                path: "src/a.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in ./a.rs"),
            },
            FileResult {
                path: "src/b.rs".into(),
                result: serde_json::json!({"ui_view": "edit failed: x", "model_view": "y"}),
            },
        ];
        let out = render_file_results(&files);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.is_array(), "must be a bare array: {out}");
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert!(v.get("ui_view").is_none(), "no aggregate view: {out}");
        // Each per-file result is the existing shape, verbatim.
        assert_eq!(
            v[0]["result"],
            serde_json::json!("Replaced 1 block(s) in ./a.rs")
        );
        assert_eq!(v[1]["result"]["ui_view"], "edit failed: x");
        // Round-trips.
        assert_eq!(parse_file_results(&out).unwrap(), files);
    }

    #[test]
    fn parse_file_results_rejects_other_shapes() {
        // A single-file envelope is not a multi-file result.
        assert!(parse_file_results(r#"{"ui_view":"a","model_view":"b"}"#).is_none());
        // An array of plain strings is another tool's list, not ours.
        assert!(parse_file_results(r#"["a","b"]"#).is_none());
        // An empty array is not a report.
        assert!(parse_file_results("[]").is_none());
        // Non-JSON.
        assert!(parse_file_results("Replaced 1 block(s) in ./a.rs").is_none());
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
        let f = EditFailure::not_found_no_match();
        // ui_view is the bare fragment: `not_found_failure` supplies the
        // `edit #N oldText not found.` prefix every not-found failure shares,
        // so this must not carry one of its own.
        assert_eq!(f.ui_view, "No similar region in file.");
        assert!(f.model_view.contains("No close-enough region"));
        // The model already has its own text in its own tool call; echoing it
        // back costs tokens and adds no information.
        assert!(!f.model_view.contains("Original oldText"));
    }

    #[test]
    fn non_unique_lists_line_numbers() {
        let f = EditFailure::non_unique(1, 3, &[1, 3, 5], 0);
        assert!(f.ui_view.contains("lines: 1, 3, 5"));
        assert!(f.model_view.contains("lines: 1, 3, 5"));
        assert!(!f.model_view.contains("foo"), "oldText must not be echoed");
    }

    #[test]
    fn non_unique_caps_listing_with_truncation_notice() {
        let f = EditFailure::non_unique(1, 15, &(1..=10).collect::<Vec<_>>(), 0);
        assert!(f.ui_view.contains("(...and 5 more)"));
        assert!(f.ui_view.contains("1, 2, 3, 4, 5, 6, 7, 8, 9, 10"));
    }

    #[test]
    fn non_unique_reports_excluded_claims() {
        // 5 total, 2 already claimed → 3 free; free = 5 - 2, shown = 3 → no
        // truncation note, but the excluded count is called out.
        let f = EditFailure::non_unique(2, 5, &[10, 20, 30], 2);
        assert!(f.ui_view.contains("appears 5 times"));
        assert!(f.ui_view.contains("2 occurrence(s) already matched"));
        assert!(f.ui_view.contains("free lines: 10, 20, 30"));
        assert!(!f.ui_view.contains("more"));
    }

    #[test]
    fn non_unique_truncation_counts_only_free_candidates() {
        // 15 total, 5 excluded → 10 free, all shown → no "...and more" note
        // even though total (15) > shown (10).
        let f = EditFailure::non_unique(1, 15, &(1..=10).collect::<Vec<_>>(), 5);
        assert!(!f.ui_view.contains("more)"));
    }

    #[test]
    fn overlap_conflict_names_both_edits_and_ranges() {
        let f = EditFailure::overlap_conflict(2, (140, 143), 1, (141, 145));
        assert!(f.ui_view.contains("edit #2"));
        assert!(f.ui_view.contains("edit #1"));
        assert!(f.ui_view.contains("140-143"));
        assert!(f.ui_view.contains("141-145"));
        assert!(f.model_view.contains("disjoint"));
        // The line ranges already say which region; the oldText added nothing.
        assert!(!f.model_view.contains("the text"));
    }

    #[test]
    fn all_candidates_claimed_lists_claimers() {
        let f = EditFailure::all_candidates_claimed(3, 2, &[7, 12], &[1, 2]);
        assert!(f.ui_view.contains("edit #3"));
        assert!(f.ui_view.contains("#1, #2"));
        assert!(f.ui_view.contains("lines: 7, 12"));
        assert!(f.model_view.contains("drop it"));
        assert!(
            !f.model_view.contains("dup text"),
            "oldText must not be echoed"
        );
    }

    #[test]
    fn anchor_accepts_within_tolerance_or_decisive_skew() {
        // Fast path: inside the context tolerance regardless of skew.
        assert!(anchor_accepts(2, 3, 3));
        assert!(anchor_accepts(0, 0, 0));
        // Beyond tolerance, decisive skew and within cap → accept.
        assert!(anchor_accepts(10, 40, 1));
        assert!(anchor_accepts(1, 3, 0));
        // Beyond tolerance, near-tie → reject.
        assert!(!anchor_accepts(10, 20, 1));
        // Ties never pass the skew clause (d2 == d1, ratio > 1).
        assert!(!anchor_accepts(5, 5, 4));
        // Far beyond the cap → reject even with huge skew.
        assert!(!anchor_accepts(61, 10_000, 5));
        assert!(anchor_accepts(60, 180, 5));
    }

    #[test]
    fn batch_build_aggregates_all_failures() {
        let items = vec![
            BatchItem {
                edit_index: 0,
                origin: None,
                anchored: false,
                old_text: String::new(),
                failure: EditFailure::not_found_no_match(),
            },
            BatchItem {
                edit_index: 2,
                origin: None,
                anchored: false,
                old_text: String::new(),
                failure: EditFailure::non_unique(3, 2, &[4, 8], 0),
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
            origin: None,
            anchored: false,
            old_text: String::new(),
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
            origin: None,
            anchored: false,
            old_text: String::new(),
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
            origin: None,
            anchored: false,
            old_text: String::new(),
            failure: EditFailure::not_found_no_match(),
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
            origin: None,
            anchored: false,
            old_text: String::new(),
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

    #[test]
    fn diagnose_handles_multibyte_char_boundaries() {
        // Regression test for: start byte index is not a char boundary;
        // it is inside '─' (or any multi-byte UTF-8 character).
        // The fix aligns win_start/win_end to char boundaries using
        // floor_char_boundary/ceil_char_boundary.
        let file_text = "start ─── middle ─── end"; // '─' is 3 bytes each
                                                    // Query that will match near a multi-byte char boundary
        let query = "─── midd"; // This overlaps the 3-byte chars
        let f = diagnose(query, file_text).expect("should diagnose");
        assert!(f.model_view.contains("Closest match"));
        // The window slice must not panic, and must still be read-format
        // lines: selecting whole lines makes a split char impossible.
        assert!(f.model_view.contains("| "));
    }

    #[test]
    fn diagnose_handles_multibyte_at_file_edges() {
        // Test when the anchor is near the start/end of file with multi-byte chars
        let file_text = "───start"; // 3-byte chars at start
        let query = "───sta";
        let f = diagnose(query, file_text).expect("should diagnose");
        assert!(f.model_view.contains("Closest match"));

        let file_text2 = "end───"; // 3-byte chars at end
        let query2 = "nd───";
        let f2 = diagnose(query2, file_text2).expect("should diagnose");
        assert!(f2.model_view.contains("Closest match"));
    }

    #[test]
    fn diagnose_handles_large_offset_with_multibyte() {
        // Test the exact panic scenario: large file with multi-byte char
        // at position where adding WINDOW_PAD + query.len() crosses a
        // multi-byte boundary.
        let file_text = "a".repeat(1016) + "─" + &"b".repeat(100); // '─' at byte 1016 (3 bytes)
                                                                   // Query matching a region that includes the multi-byte char
        let query = &file_text[1000..1030]; // Includes the '─'
        let f = diagnose(query, &file_text).expect("should diagnose");
        assert!(f.model_view.contains("Closest match"));
        assert!(f.model_view.contains("| "));
    }
}

#[cfg(test)]
mod render_tests {
    //! Tests for the read-format context render that replaced the diff.
    //!
    //! These pin the properties the diff could not hold: that the region shown
    //! actually *contains* the difference, that every line number is the file's
    //! own, and that nothing the model sent comes back at it.

    use crate::apply::{apply_to_text, ApplyOutcome};
    use crate::types::EditOp;

    fn op(old: &str, origin: Option<&str>, anchored: bool) -> EditOp {
        EditOp {
            old_text: old.into(),
            new_text: "NEW".into(),
            start_line: anchored.then_some(crate::types::EditStartLine(1)),
            origin: origin.map(str::to_string),
        }
    }

    /// Run one failing edit and return its `model_view`.
    fn model_view(text: &str, edits: Vec<EditOp>) -> String {
        match apply_to_text(text, &edits) {
            ApplyOutcome::Applied { .. } => panic!("expected the edit to fail"),
            ApplyOutcome::Failed { report, .. } => {
                let v: serde_json::Value = serde_json::from_str(&report).unwrap();
                v["model_view"].as_str().unwrap().to_string()
            }
        }
    }

    /// The `NNNN| ` rows of a rendered body, as `(line_no, content)`.
    fn numbered(mv: &str) -> Vec<(usize, String)> {
        mv.lines()
            .filter_map(|l| {
                let (n, rest) = l.split_once('|')?;
                let n: usize = n.trim().parse().ok()?;
                // Exactly one space, per `read`. Anything more would hide a
                // change to the leading indentation — which is content.
                Some((n, rest.strip_prefix(' ')?.to_string()))
            })
            .collect()
    }

    /// Assert the numbered rows are exactly what `read` would emit for those
    /// lines. Spelled as `read`'s own expression rather than a near-copy, so a
    /// divergence between the two renders fails here.
    #[test]
    fn the_body_is_byte_identical_to_reads_format() {
        let file = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n    let gamma = 3;\n}\n";
        let q = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n    let delta = 3;\n}\n";
        let mv = model_view(file, vec![op(q, Some("f.rs:2"), true)]);
        let rows = numbered(&mv);
        assert!(!rows.is_empty(), "no numbered rows in:\n{mv}");
        let expect: Vec<(usize, String)> = rows
            .iter()
            .map(|&(n, _)| (n, file.lines().nth(n - 1).unwrap().to_string()))
            .collect();
        assert_eq!(rows, expect, "body does not match `read`'s rendering");
        // And the row really is `read`'s expression.
        let rendered: Vec<String> = file
            .lines()
            .enumerate()
            .filter(|(i, _)| rows.iter().any(|(n, _)| *n == i + 1))
            .map(|(i, l)| format!("{:4}| {}\n", i + 1, l))
            .collect();
        for (row, want) in rows.iter().zip(rendered.iter()) {
            assert!(
                mv.contains(want.trim_end()),
                "expected read's exact row {want:?} for line {}",
                row.0
            );
        }
    }

    #[test]
    fn the_body_carries_no_diff_markers() {
        let file = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n}\n";
        let q = "fn main() {\n    let alpha = 1;\n    let bxta = 2;\n}\n";
        let mv = model_view(file, vec![op(q, Some("f.rs:2"), true)]);
        assert!(!mv.contains("Diff"), "diff header survived:\n{mv}");
        // Scope to the rendered region: the envelope's own `--- edit #N ---`
        // separator legitimately starts with `-`.
        let body = mv
            .split_once("Closest match at file lines ")
            .and_then(|(_, b)| b.split_once('\n'))
            .map(|(_, b)| b)
            .expect("rendered body");
        assert!(
            !body.lines().any(|l| l.starts_with("@@")),
            "hunk marker survived:\n{body}"
        );
        assert!(
            !body
                .lines()
                .any(|l| l.starts_with('+') || l.starts_with('-')),
            "diff rows survived:\n{body}"
        );
        assert!(
            !body.contains("... ("),
            "truncation notice survived:\n{body}"
        );
    }

    /// The diff rendered a padded window from the front, so a change late in a
    /// long block was cut before it was shown. This is the case it got wrong.
    #[test]
    fn a_typo_on_the_last_line_of_a_long_block_is_shown() {
        let mut file: String = (1..=120)
            .map(|i| format!("line{i:03} original\n"))
            .collect();
        file.push_str("fn main() { let alpha = 1; }\n");
        let mut q: String = (1..=120)
            .map(|i| format!("line{i:03} original\n"))
            .collect();
        q.push_str("fn main() { let alhpa = 1; }\n");

        let mv = model_view(&file, vec![op(&q, Some("big.rs:121"), true)]);
        assert!(
            mv.contains("let alpha = 1;"),
            "the changed line must be shown:\n{mv}"
        );
        assert!(mv.contains("121| fn main() { let alpha = 1; }"), "{mv}");
    }

    /// The other end of the block: the anchor is the longest common *substring*,
    /// so a change on the first line must not push the region past it.
    #[test]
    fn a_typo_on_the_first_line_of_a_block_is_shown() {
        let file = (1..=10)
            .map(|i| format!("line{i:02} original\n"))
            .collect::<String>();
        let q = format!("LINE01 wrong\n{}", &file[12..]);
        let mv = model_view(&file, vec![op(&q, Some("f.rs:1"), true)]);
        assert!(
            mv.contains("line01 original"),
            "changed line missing:\n{mv}"
        );
        assert!(mv.contains("   1|"), "line 1 missing:\n{mv}");
    }

    /// Pins the double-count that made the header claim line 347 in a 202-line
    /// file: the reported span and the rendered rows must agree.
    #[test]
    fn reported_line_numbers_are_the_files_own() {
        let mut file = String::new();
        for i in 1..=200 {
            file.push_str(&format!("a{i:03}\n"));
        }
        file.push_str("target here\nmore code\n");
        let mv = model_view(
            &file,
            vec![op("target HERE\nmore code\n", Some("f.rs:201"), true)],
        );

        let (first, last) = parse_span(&mv);
        assert!(
            first <= 201 && last <= 202,
            "span {first}-{last} must lie inside the {}-line file",
            file.lines().count()
        );
        // Every row's number must index the file's real content.
        for (n, content) in numbered(&mv) {
            assert_eq!(
                file.lines().nth(n - 1).unwrap(),
                content,
                "line {n} does not match the file"
            );
        }
    }

    fn parse_span(mv: &str) -> (usize, usize) {
        let marker = "Closest match at file lines ";
        let rest = &mv[mv.find(marker).expect("span in header") + marker.len()..];
        let end = rest.find(':').expect("span terminator");
        let mut it = rest[..end].split('-').map(|s| s.parse().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    }

    #[test]
    fn elision_keeps_true_line_numbers_on_both_sides() {
        // 80 unchanged lines around one changed line: past the 40-line cap.
        let mut file = String::new();
        for i in 1..=80 {
            file.push_str(&format!("same line {i:02}\n"));
        }
        let mut q = String::new();
        for i in 1..=80 {
            q.push_str(&format!("same line {i:02}\n"));
        }
        q.push_str("the changed line\n");
        let mv = model_view(&file, vec![op(&q, Some("f.rs:81"), true)]);

        assert!(mv.contains("lines elided"), "no elision notice:\n{mv}");
        let rows = numbered(&mv);
        assert!(rows.len() <= 42, "cap not applied: {} rows", rows.len());
        // The elision is non-destructive: numbers stay real, so the head and
        // the tail are each independently usable.
        assert!(rows.first().unwrap().0 < 40, "head not near the start");
        assert!(rows.last().unwrap().0 > 45, "tail not near the end");
        for (n, content) in &rows {
            assert_eq!(file.lines().nth(n - 1).unwrap(), content, "line {n}");
        }
    }

    #[test]
    fn a_wrong_anchor_is_not_blamed_and_the_region_is_still_shown() {
        // A line number is a disambiguator, not a validity check, so a wildly
        // wrong anchor cannot be why this failed and must not be named as such.
        let file = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n}\n";
        let q = "fn main() {\n    let alpha = 1;\n    let beta = 9999;\n}\n";
        let mv = model_view(file, vec![op(q, Some("f.rs:99999"), true)]);
        assert!(mv.contains("Closest match"), "{mv}");
        assert!(!mv.contains("anchored near"), "anchor blamed:\n{mv}");
    }
}

#[cfg(test)]
mod origin_tests {
    //! The identifier that tells the model *which* of its blocks failed.

    use crate::apply::{apply_to_text, ApplyOutcome};
    use crate::types::EditOp;

    fn op(old: &str, origin: Option<&str>, anchored: bool) -> EditOp {
        EditOp {
            old_text: old.into(),
            new_text: "NEW".into(),
            start_line: anchored.then_some(crate::types::EditStartLine(1)),
            origin: origin.map(str::to_string),
        }
    }

    fn model_view(text: &str, edits: Vec<EditOp>) -> String {
        match apply_to_text(text, &edits) {
            ApplyOutcome::Applied { .. } => panic!("expected the edit to fail"),
            ApplyOutcome::Failed { report, .. } => {
                let v: serde_json::Value = serde_json::from_str(&report).unwrap();
                v["model_view"].as_str().unwrap().to_string()
            }
        }
    }

    const FILE: &str = "fn main() {\n    let alpha = 1;\n    let beta = 2;\n}\n";
    const Q: &str = "fn main() {\n    let alpha = 1;\n    let bxta = 2;\n}\n";

    #[test]
    fn an_anchored_block_is_named_by_its_own_header_string() {
        let mv = model_view(FILE, vec![op(Q, Some("src/foo.rs:140"), true)]);
        assert!(mv.contains("Your block `src/foo.rs:140` failed:"), "{mv}");
    }

    /// The oldText is the fallback for when the origin cannot identify the
    /// block: several blocks in one file and no anchor to tell them apart.
    #[test]
    fn oldtext_is_echoed_only_for_unanchored_multi_block_calls() {
        // total == 1, unanchored: `edit #1` is unambiguous, so no echo.
        let single = model_view(FILE, vec![op(Q, Some("src/foo.rs"), false)]);
        assert!(
            !single.contains("The text you sent"),
            "echoed needlessly:\n{single}"
        );

        // total == 3, unanchored: the model would have to count, so echo.
        let multi = model_view(
            FILE,
            vec![
                op(
                    "fn main() {\n    let alpha = 1;\n",
                    Some("src/foo.rs"),
                    false,
                ),
                op(
                    "    let beta = 2;\n    let gamma = 3;\n",
                    Some("src/foo.rs"),
                    false,
                ),
                op(Q, Some("src/foo.rs"), false),
            ],
        );
        assert!(
            multi.contains("The text you sent for this block"),
            "{multi}"
        );
        assert!(
            multi.contains("let bxta = 2;"),
            "wrong text echoed:\n{multi}"
        );
    }

    #[test]
    fn an_anchor_suppresses_the_echo_even_in_a_multi_block_call() {
        let mv = model_view(
            FILE,
            vec![
                op(
                    "fn main() {\n    let alpha = 1;\n",
                    Some("src/foo.rs:1"),
                    true,
                ),
                op(Q, Some("src/foo.rs:2"), true),
            ],
        );
        assert!(
            !mv.contains("The text you sent"),
            "echoed needlessly:\n{mv}"
        );
    }

    #[test]
    fn the_echo_is_capped_at_twenty_lines_and_elided_at_the_end() {
        let mut file = String::new();
        for i in 1..=200 {
            file.push_str(&format!("filler line {i:03}\n"));
        }
        file.push_str("const WANTED = 1;\n");
        let q: String = (1..=40).map(|i| format!("q line {i:03}\n")).collect();
        let mut bad = q.clone();
        bad.push_str("const WANTED = 2;\n");

        // Two blocks in the file, so the ordinal alone cannot identify one.
        let mv = model_view(
            &file,
            vec![
                op("filler line 001\n", Some("f.rs:1"), false),
                op(&bad, Some("f.rs"), false),
            ],
        );
        let fence = mv
            .split("The text you sent")
            .nth(1)
            .unwrap_or_else(|| panic!("no echo in:\n{mv}"));
        let echoed: Vec<&str> = fence
            .lines()
            .skip_while(|l| !l.contains("```"))
            .skip(1)
            .take_while(|l| *l != "```")
            .collect();
        // 20 lines plus the elision notice.
        assert!(echoed.len() <= 21, "echo too long: {}", echoed.len());
        assert!(
            echoed.iter().any(|l| l.contains("lines elided")),
            "no elision notice: {echoed:?}"
        );
        // The notice is last: elided at the end, not in the middle.
        assert!(
            echoed.last().unwrap().contains("lines elided"),
            "not elided at the end: {echoed:?}"
        );
    }

    #[test]
    fn every_failure_kind_leads_with_the_origin_string() {
        // `non_unique`: two occurrences of the same text, both unanchored.
        let dup = "run();\n";
        let text = "run();\nkeep();\nrun();\n";
        let mv = model_view(text, vec![op(dup, Some("a/b.rs:1"), false)]);
        assert!(mv.contains("Your block `a/b.rs:1` failed:"), "{mv}");
    }
}
