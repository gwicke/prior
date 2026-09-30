//! Aider-style `@@@OLD`/`@@@NEW`/`@@@END` "blob" edit format: parser and
//! value-level transforms.
//!
//! See `doc/batch_editing_v2.md`. Three representations are in play:
//!
//! - **Blob** — the model-facing text format, a filename line followed by one
//!   or more `@@@OLD` / `@@@NEW` / `@@@END` blocks.
//! - **Canonical array** — the internal/persisted form, pi-compatible:
//!   `{"path": ..., "edits": [{"oldText": ..., "newText": ...}, ...]}`.
//! - **Legacy** — `oldText`/`newText` (single) and `old`/`new` (very old
//!   picocode). Accepted on ingress / replay only.
//!
//! This module is pure: it operates on `&str` and [`serde_json::Value`] and
//! knows nothing about rig types (the `Message`-level rewrite loops live at
//! the call sites).

use serde_json::{json, Value};

/// The **model-facing** field carrying the block blob.
///
/// Renamed from `edits`, which meant two different things at once: this string
/// blob, and the canonical array. The split removes the overloading — `blocks`
/// is what we advertise, and `edits` is only the canonical array pi requires
/// on disk.
pub const BLOCKS_FIELD: &str = "blocks";

/// The pre-rename name for [`BLOCKS_FIELD`], still honoured on ingress.
pub const LEGACY_BLOCKS_FIELD: &str = "edits";

/// The blob string a raw `edit` call carries, or `None` when it carries none.
///
/// Reads [`BLOCKS_FIELD`], then falls back to a **string**
/// [`LEGACY_BLOCKS_FIELD`]. `blocks` is the name we *ask* for, not one we
/// *require*: a harness, a model, or a session written before the rename all
/// still send the old one, and dead-ending those calls would break the very
/// shape this tool exists to serve. An `edits` *array* is deliberately not
/// read here — that is pi's canonical form, never a blob, and `from_wire`
/// rejects it by name.
pub fn blob_text(args: &Value) -> Option<&str> {
    let obj = args.as_object()?;
    obj.get(BLOCKS_FIELD)
        .and_then(Value::as_str)
        .or_else(|| obj.get(LEGACY_BLOCKS_FIELD).and_then(Value::as_str))
}

/// Opening marker of an OLD region (`@@@OLD`).
pub const SEARCH_MARKER: &str = "@@@OLD";
/// Divider between the OLD and NEW regions (`@@@NEW`).
pub const DIVIDER: &str = "@@@NEW";
/// Closing marker of a NEW region (`@@@END`).
pub const REPLACE_MARKER: &str = "@@@END";

/// One parsed `@@@OLD`/`@@@NEW`/`@@@END` block.
#[derive(Debug, PartialEq, Clone)]
pub struct ParsedEdit {
    pub old_text: String,
    pub new_text: String,
    /// 1-based line the block's header anchored to (`path:LINE`), if any. Used
    /// as a nearest-match hint by the backend; never persisted or rendered.
    pub start_line: Option<u64>,
    /// The file this block's header named, if it had one.
    ///
    /// Needed because a blob may legitimately span files — a recursive `bsed`
    /// preview emits one header per file — and the single-file agreement check
    /// in [`parse_blocks`] only *validates* headers, it does not keep them. The
    /// anchor is split off `header`; this is the path part. Not persisted.
    pub header: Option<String>,
    /// The line directly above this block's `@@@OLD` that was **not** taken
    /// as a header, when there was one and it was not empty.
    ///
    /// A header must be a single whitespace-free token, so a model that wrote
    /// prose, or a real path containing a space, silently produces a
    /// header-less block. That is unreportable from `header` alone (`None`
    /// either way), and reporting a bare absence is the failure P1 was written
    /// against — the caller has no way to learn *what it wrote* in the slot the
    /// error is about. Kept so the multi-file policy can quote it back. Not
    /// persisted.
    pub rejected_header: Option<String>,
}

/// Result of parsing a blob.
#[derive(Debug, PartialEq)]
pub struct ParsedBlocks {
    /// The inferred single file path, when every present header agrees (and at
    /// least one header is present). `None` when no header was found.
    pub path: Option<String>,
    /// True when two blocks carry *differing* header paths (multi-file attempt).
    pub multi_file: bool,
    /// Parsed blocks in text order.
    pub edits: Vec<ParsedEdit>,
}

/// A blob parse failure. Rendered into the tool's self-correcting error.
#[derive(Debug, PartialEq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether a line is exactly one of the three structural markers.
///
/// The line comes from `str::lines`, so any trailing newline — and a CRLF's
/// `\r` — is already stripped. A marker must therefore be the whole line: no
/// leading whitespace, no trailing characters.
fn marker_of(line: &str) -> Option<&'static str> {
    if line == SEARCH_MARKER {
        return Some(SEARCH_MARKER);
    }
    if line == DIVIDER {
        return Some(DIVIDER);
    }
    if line == REPLACE_MARKER {
        return Some(REPLACE_MARKER);
    }
    None
}

/// A marker the line *almost* is: it starts with one of the three markers but
/// carries trailing characters, so [`marker_of`] does not recognize it — e.g.
/// `@@@OLD>`, `@@@OLD the old text`, or `@@@END ` (with a trailing space).
///
/// Returns the marker the line appears to start with.
///
/// Used *only* when no block parsed at all, to explain a garbled opening
/// marker. Inside a block body a near miss is deliberately left alone: it may
/// be a formatting slip, but it may equally be the literal text the caller
/// means to write (`@@@END>` is a closed block and harms nothing), and the
/// `oldText` match fails loudly if it was a slip. Rejecting it there would
/// block a legitimate insert without preventing any real damage.
fn near_miss_marker(line: &str) -> Option<&'static str> {
    for m in [SEARCH_MARKER, DIVIDER, REPLACE_MARKER] {
        if line != m && line.starts_with(m) {
            return Some(m);
        }
    }
    None
}

/// The error for a line that starts a marker but does not end there.
fn malformed_marker_error(marker: &str, line: &str) -> ParseError {
    ParseError(format!(
        "malformed `{marker}` marker `{line}`: a marker must be alone on its line, \
         with nothing after it. To write the literal text `{marker}` into the file, \
         use the single-edit fields (path + oldText + newText) for that file instead."
    ))
}

/// The first near-miss marker in a region already scanned, if any.
///
/// Used only on paths that are rejecting the edit outright. Naming the garbled
/// marker there is free — nothing is being preserved — and it is usually the
/// root cause: a garbled divider makes the parser run on and trip over some
/// later valid marker, so reporting that later marker alone points at a
/// consequence rather than the mistake.
fn near_miss_in<S: AsRef<str>>(region: &[S]) -> Option<(&'static str, &str)> {
    region
        .iter()
        .find_map(|l| near_miss_marker(l.as_ref()).map(|m| (m, l.as_ref())))
}

/// Strip a single surrounding triple-backtick fence, if present.
fn strip_fence(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();

    // Drop a leading fence line (``` or ```lang).
    if let Some(first) = lines.iter().position(|l| !l.trim().is_empty()) {
        if lines[first].trim_start().starts_with("```") {
            lines.remove(first);
        }
    }
    // Drop a trailing fence line (exactly ```).
    if let Some(last) = lines.iter().rposition(|l| !l.trim().is_empty()) {
        if lines[last].trim() == "```" {
            lines.remove(last);
        }
    }
    lines
}

/// A header candidate must be a single token (no ASCII whitespace): this is
/// what distinguishes a filename line from surrounding prose (which has
/// spaces). A path with embedded spaces is not recognized as a header — such
/// calls still work when the `path` argument is supplied.
pub(crate) fn header_candidate(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) || marker_of(t).is_some() {
        return None;
    }
    if t.starts_with("```") {
        return None;
    }
    Some(t.to_string())
}

/// Split a header token into `(path, optional 1-based line anchor)`.
///
/// Recognizes the `file:LINE` idiom (as used by compiler diagnostics, stack
/// traces, and `grep -n`), and tolerates the `file:LINE:COL` form by ignoring
/// the column. The suffix is only peeled off when it is a trailing `:` followed
/// by **only ASCII digits**, so a colon-bearing filename (`weird:name.rs`) or a
/// Windows drive path (`C:\foo\bar.rs`) is left intact — the anchor is taken
/// from the *rightmost* `:digits` group. A line number of `0` is not a valid
/// 1-based anchor and is treated as part of the path.
///
/// Shared by the blob header parser and the `path` argument parser.
pub fn split_line_anchor(token: &str) -> (String, Option<u64>) {
    fn digits(s: &str) -> Option<u64> {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<u64>().ok().filter(|n| *n >= 1)
        } else {
            None
        }
    }

    if let Some(idx) = token.rfind(':') {
        let (head, tail) = (&token[..idx], &token[idx + 1..]);
        if let Some(n) = digits(tail) {
            // `path:LINE:COL` — the trailing group is a column; prefer the
            // preceding numeric group as the line.
            if let Some(idx2) = head.rfind(':') {
                if let Some(line) = digits(&head[idx2 + 1..]) {
                    return (head[..idx2].to_string(), Some(line));
                }
            }
            return (head.to_string(), Some(n));
        }
    }
    (token.to_string(), None)
}

/// Parse a blob into [`ParsedEdit`] blocks plus the inferred path.
///
/// Recognizes blocks whose markers each occupy their own (trimmed) line. The
/// line immediately preceding an `@@@OLD` is treated as an optional
/// filename header when it is a single whitespace-free token; a trailing
/// `:LINE` on that header supplies the block's line anchor.
pub fn parse_blocks(text: &str) -> Result<ParsedBlocks, ParseError> {
    let lines = strip_fence(text);
    let mut edits: Vec<ParsedEdit> = Vec::new();
    let mut distinct_headers: Vec<String> = Vec::new();
    let mut saw_any_header = false;

    let mut i = 0usize;
    while i < lines.len() {
        if marker_of(&lines[i]) != Some(SEARCH_MARKER) {
            i += 1;
            continue;
        }

        // Optional header = the immediately preceding line, if it is a path.
        // A `path:LINE` suffix supplies this block's anchor; the path part is
        // what participates in the (per-file) agreement check.
        let mut header_anchor: Option<u64> = None;
        let mut header_path: Option<String> = None;
        // The line this block's header would have been, when there was one and
        // it was not a usable header. Kept only to be quoted back in an error:
        // prose and a path-with-a-space are indistinguishable from `None`.
        let mut rejected_header: Option<String> = None;
        if i > 0 {
            if let Some(h) = header_candidate(&lines[i - 1]) {
                let (path_part, anchor) = split_line_anchor(&h);
                saw_any_header = true;
                header_anchor = anchor;
                header_path = Some(path_part.clone());
                if !distinct_headers.iter().any(|d| same_path(d, &path_part)) {
                    distinct_headers.push(path_part);
                }
            } else {
                // A marker is never a header candidate — an adjacent block
                // has the previous `@@@END` sitting in this slot, and
                // reporting that as a rejected header would be nonsense.
                let prev = lines[i - 1].trim();
                if !prev.is_empty() && !prev.starts_with("```") && marker_of(prev).is_none() {
                    rejected_header = Some(prev.to_string());
                }
            }
        }

        // Collect OLD content up to the divider.
        // Fail fast if any unexpected delimiter is encountered.
        let mut j = i + 1;
        let mut search_lines: Vec<&str> = Vec::new();
        while j < lines.len() {
            if let Some(m) = marker_of(&lines[j]) {
                if m == DIVIDER {
                    break; // found the expected divider
                }
                // Any other delimiter (@@@OLD or @@@END) is invalid in OLD
                // section. Prefer a garbled marker from the region just
                // scanned — that is what cost us the structure.
                if let Some((mk, bad)) = near_miss_in(&lines[i + 1..j]) {
                    return Err(malformed_marker_error(mk, bad));
                }
                return Err(ParseError(format!(
                    "unexpected delimiter `{m}` in @@@OLD section; expected `@@@NEW`"
                )));
            }
            search_lines.push(&lines[j]);
            j += 1;
        }
        if j >= lines.len() {
            // The block is being rejected either way, so naming a near-miss in
            // the region we just scanned costs nothing and beats a bare
            // "missing @@@NEW" — there is no content here to protect.
            if let Some((mk, bad)) = near_miss_in(&lines[i + 1..j]) {
                return Err(malformed_marker_error(mk, bad));
            }
            return Err(ParseError(format!(
                "an edit block is missing its `@@@NEW` section"
            )));
        }

        // Collect NEW content up to the closing marker.
        // Fail fast if any unexpected delimiter is encountered.
        let mut k = j + 1;
        let mut replace_lines: Vec<&str> = Vec::new();
        while k < lines.len() {
            if let Some(m) = marker_of(&lines[k]) {
                if m == REPLACE_MARKER {
                    break; // found the expected closing marker
                }
                // As above, prefer a garbled marker from the scanned region.
                if let Some((mk, bad)) = near_miss_in(&lines[j + 1..k]) {
                    return Err(malformed_marker_error(mk, bad));
                }
                return Err(ParseError(format!(
                    "unexpected delimiter `{m}` in @@@NEW section; expected `@@@END`"
                )));
            }
            replace_lines.push(&lines[k]);
            k += 1;
        }
        if k >= lines.len() {
            // As above: the block is already rejected, so a near-miss in the
            // NEW region is worth naming.
            if let Some((mk, bad)) = near_miss_in(&lines[j + 1..k]) {
                return Err(malformed_marker_error(mk, bad));
            }
            return Err(ParseError(format!(
                "an `@@@OLD` section is missing its `@@@END` marker"
            )));
        }

        let old_text = search_lines.join("\n");
        if old_text.is_empty() {
            return Err(ParseError(
                "an `@@@OLD` block has empty OLD text".to_string(),
            ));
        }
        let new_text = replace_lines.join("\n");
        edits.push(ParsedEdit {
            old_text,
            new_text,
            start_line: header_anchor,
            header: header_path,
            rejected_header,
        });

        i = k + 1;
    }

    if edits.is_empty() {
        // No block parsed. If some line merely *looked* like a marker, say so —
        // otherwise the model is told "no blocks found" for what is really a
        // formatting slip on its opening `@@@OLD`. Like the in-loop checks,
        // this only runs on a path that is already rejecting the edit.
        if let Some((marker, line)) = lines
            .iter()
            .find_map(|l| near_miss_marker(l).map(|m| (m, l)))
        {
            return Err(malformed_marker_error(marker, line));
        }
        return Err(ParseError(format!("no `@@@OLD` / `@@@NEW` blocks found")));
    }

    let multi_file = distinct_headers.len() > 1;
    let path = if distinct_headers.len() == 1 {
        Some(distinct_headers.pop().unwrap())
    } else if saw_any_header {
        // Two or more differing headers (multi_file) — leave path unset here;
        // the caller reports the conflict.
        None
    } else {
        None
    };

    Ok(ParsedBlocks {
        path,
        multi_file,
        edits,
    })
}

/// Compare two path strings the way the rest of the codebase does: resolve
/// both against the current directory (tilde expansion, relative join,
/// lexical `..`/`.` cleanup) and compare. Falls back to a raw string compare
/// only if `current_dir` is unavailable.
fn same_path(a: &str, b: &str) -> bool {
    match std::env::current_dir() {
        Ok(cwd) => {
            crate::path::resolve_normalized(a, &cwd) == crate::path::resolve_normalized(b, &cwd)
        }
        Err(_) => a == b,
    }
}

/// Apply the path policy and produce the resolved single-file target plus the
/// block pairs.
///
/// `json_path` is the trimmed, non-empty `path` argument (or `None`).
pub fn merge_path(
    json_path: Option<&str>,
    parsed: ParsedBlocks,
) -> Result<(String, Vec<ParsedEdit>), String> {
    if parsed.multi_file {
        return Err(
            "multi-file edits are not supported in this form; make one `edit` call per file"
                .to_string(),
        );
    }
    match (json_path, parsed.path) {
        (Some(p), Some(h)) => {
            if same_path(p, &h) {
                Ok((p.to_string(), parsed.edits))
            } else {
                Err(format!(
                    "path mismatch: the `path` argument is `{p}` but the edit text names `{h}`; \
                     they must agree"
                ))
            }
        }
        (Some(p), None) => Ok((p.to_string(), parsed.edits)),
        (None, Some(h)) => Ok((h, parsed.edits)),
        (None, None) => Err(
            "no file path: set the `path` argument or include a filename line before the \
             `@@@OLD` marker"
                .to_string(),
        ),
    }
}

/// Render block pairs as a blob, with the path as a header line above the
/// first block (Aider-style). Anchors (`startLine`/`endLine`) are a transient
/// matching aid and are not represented.
pub fn render_blob(path: &str, edits: &[ParsedEdit]) -> String {
    let mut out = String::new();
    if !path.is_empty() {
        out.push_str(path);
        out.push('\n');
    }
    for (i, e) in edits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(SEARCH_MARKER);
        out.push('\n');
        out.push_str(&e.old_text);
        out.push('\n');
        out.push_str(DIVIDER);
        out.push('\n');
        out.push_str(&e.new_text);
        out.push('\n');
        out.push_str(REPLACE_MARKER);
        out.push('\n');
    }
    // Drop the single trailing newline so the blob is tidy and round-trips.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Build the canonical `edits` array value from parsed blocks. Anchors are
/// transient and deliberately omitted (the canonical array is the persisted,
/// pi-compatible form).
fn edits_to_value(edits: &[ParsedEdit]) -> Value {
    Value::Array(
        edits
            .iter()
            .map(|e| json!({ "oldText": e.old_text, "newText": e.new_text }))
            .collect(),
    )
}

/// Canonicalize a raw edit-call `arguments` value to the canonical array form,
/// **best-effort**. Handles:
///
/// - `blocks` is a **string** (blob) → parse + merge → array;
/// - `edits` is already an **array** → unchanged (the canonical form itself);
/// - top-level `oldText`/`newText` → single-entry array;
/// - legacy `old`/`new` → single-entry array.
///
/// The array branches are **storage-side only**. The array is not a wire shape
/// any more — a model sends `blocks` (a string) or the single-edit scalars —
/// so reaching those branches here means we are normalizing something we read
/// back off disk, which is exactly what they are for.
///
/// On a malformed blob (unparseable, or a conflicting path) the value is
/// returned unchanged so replay degrades gracefully rather than erroring.
pub fn canonicalize_edit_value(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let json_path = obj
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Blob string (the model-facing field).
    if let Some(text) = blob_text(args) {
        if let Ok(parsed) = parse_blocks(text) {
            if let Ok((path, pairs)) = merge_path(json_path, parsed) {
                return json!({ "path": path, "edits": edits_to_value(&pairs) });
            }
        }
        return args.clone();
    }

    // Already an array — leave as-is.
    if obj.get("edits").map(Value::is_array).unwrap_or(false) {
        return args.clone();
    }

    // Single `oldText`/`newText`.
    if let (Some(o), Some(n)) = (
        obj.get("oldText").and_then(|v| v.as_str()),
        obj.get("newText").and_then(|v| v.as_str()),
    ) {
        let path = obj
            .get("path")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        return json!({ "path": path, "edits": [{ "oldText": o, "newText": n }] });
    }

    // Legacy `old`/`new`.
    if let Some(o) = obj.get("old") {
        let n = obj.get("new").cloned().unwrap_or(Value::Null);
        let path = obj.get("path").cloned().unwrap_or(Value::Null);
        return json!({ "path": path, "edits": [{ "oldText": o, "newText": n }] });
    }

    args.clone()
}

/// Inverse of [`canonicalize_edit_value`] for the model-facing form: rewrite a
/// canonical-array edit call into the blob form, under [`BLOCKS_FIELD`].
/// **Idempotent** — a value that is not a canonical array is returned unchanged,
/// so a value already in blob form passes straight through.
pub fn array_to_blob_value(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };
    let Some(edits) = obj.get("edits").and_then(|v| v.as_array()) else {
        return args.clone();
    };
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let parsed: Vec<ParsedEdit> = edits
        .iter()
        .map(|e| ParsedEdit {
            old_text: e
                .get("oldText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            new_text: e
                .get("newText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            start_line: None,
            header: None,
            rejected_header: None,
        })
        .collect();
    let blob = render_blob(path, &parsed);
    json!({ "path": path, "blocks": blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block as it comes back from parsing a blob that named `path` on its
    /// header line.
    fn headed(path: &str, old: &str, new: &str) -> ParsedEdit {
        ParsedEdit {
            header: Some(path.to_string()),
            ..plain(old, new)
        }
    }

    /// A block with no anchor and no header, for terse comparisons.
    fn plain(old: &str, new: &str) -> ParsedEdit {
        ParsedEdit {
            old_text: old.to_string(),
            new_text: new.to_string(),
            start_line: None,
            header: None,
            rejected_header: None,
        }
    }

    #[test]
    fn parses_single_block_with_path() {
        let text = "src/foo.py\n@@@OLD\ndef foo():\n    return 1\n@@@NEW\ndef foo():\n    return 2\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![headed(
                "src/foo.py",
                "def foo():\n    return 1",
                "def foo():\n    return 2"
            )]
        );
        assert_eq!(p.edits[0].start_line, None);
    }

    #[test]
    fn parses_two_blocks_same_file_second_inherits() {
        let text = "src/foo.py\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        // The first block carries the header; the second inherits the file but
        // not the header line, so only the first keeps it.
        assert_eq!(
            p.edits,
            vec![headed("src/foo.py", "a", "A"), plain("b", "B")]
        );
    }

    #[test]
    fn header_line_anchor_is_captured_per_block() {
        // `path:LINE` on a header supplies that block's anchor; differing
        // anchors on the same file do NOT count as a multi-file attempt.
        let text = "src/foo.py:10\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\nsrc/foo.py:42\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(p.edits.len(), 2);
        assert_eq!(p.edits[0].start_line, Some(10));
        assert_eq!(p.edits[1].start_line, Some(42));
    }

    #[test]
    fn header_line_anchor_only_on_first_block() {
        let text = "src/foo.py:7\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits[0].start_line, Some(7));
        assert_eq!(p.edits[1].start_line, None);
    }

    #[test]
    fn strict_markers_no_trailing_chars() {
        // Delimiters must be exact — no trailing characters accepted
        assert!(parse_blocks("@@@OLD>\nold\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD]\nold\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD}\nold\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD)\nold\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLDX\nold\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD \nold\n@@@NEW\nnew\n@@@END").is_err()); // trailing space
    }

    #[test]
    fn strict_markers_no_content_on_same_line() {
        // Delimiters cannot have content on the same line
        assert!(parse_blocks("@@@OLD>old\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD]old\n@@@NEW\nnew\n@@@END").is_err());
        assert!(parse_blocks("@@@OLD old\n@@@NEW\nnew\n@@@END").is_err()); // space + content
    }

    #[test]
    fn strict_end_marker_no_trailing_chars() {
        // @@@END must also be exact
        assert!(parse_blocks("@@@OLD\nold\n@@@NEW\nnew\n@@@END>").is_err());
        assert!(parse_blocks("@@@OLD\nold\n@@@NEW\nnew\n@@@END]").is_err());
        assert!(parse_blocks("@@@OLD\nold\n@@@NEW\nnew\n@@@ENDX").is_err());
    }

    #[test]
    fn exact_markers_still_work() {
        // The strict exact format should still parse correctly
        let text = "src/foo.py\n@@@OLD\nold\n@@@NEW\nnew\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert_eq!(p.edits, vec![headed("src/foo.py", "old", "new")]);
    }

    #[test]
    fn split_line_anchor_forms() {
        assert_eq!(split_line_anchor("a/b.rs"), ("a/b.rs".into(), None));
        assert_eq!(
            split_line_anchor("a/b.rs:100"),
            ("a/b.rs".into(), Some(100))
        );
        // `line:col` — column ignored.
        assert_eq!(
            split_line_anchor("a/b.rs:100:5"),
            ("a/b.rs".into(), Some(100))
        );
        // Colon-bearing filename is left intact.
        assert_eq!(
            split_line_anchor("weird:name.rs"),
            ("weird:name.rs".into(), None)
        );
        // Windows drive path preserved; trailing anchor still split.
        assert_eq!(
            split_line_anchor("C:\\foo\\bar.rs"),
            ("C:\\foo\\bar.rs".into(), None)
        );
        assert_eq!(
            split_line_anchor("C:\\foo\\bar.rs:12"),
            ("C:\\foo\\bar.rs".into(), Some(12))
        );
        // Non-numeric / zero suffixes are not anchors.
        assert_eq!(split_line_anchor("a.rs:x"), ("a.rs:x".into(), None));
        assert_eq!(split_line_anchor("a.rs:0"), ("a.rs:0".into(), None));
    }

    #[test]
    fn flags_multi_file_on_differing_headers() {
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\ny\n@@@END\n\nb.rs\n@@@OLD\np\n@@@NEW\nq\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert!(p.multi_file);
        assert!(merge_path(Some("a.rs"), p).is_err());
    }

    #[test]
    fn strips_fences() {
        let text = "```\nsrc/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END\n```";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert_eq!(p.edits, vec![headed("src/x.rs", "a", "b")]);
    }

    #[test]
    fn prose_before_block_is_not_a_header() {
        let text = "Sure, here is the change:\n\n@@@OLD\na\n@@@NEW\nb\n@@@END";
        let p = parse_blocks(text).unwrap();
        // The immediate predecessor of @@@OLD is blank → no header.
        assert_eq!(p.path, None);
        assert_eq!(p.edits, vec![plain("a", "b")]);
    }

    #[test]
    fn missing_divider_errors() {
        let text = "@@@OLD\na\n@@@END";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn missing_replace_errors() {
        let text = "@@@OLD\na\n@@@NEW\nb";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn empty_search_errors() {
        let text = "@@@OLD\n@@@NEW\nb\n@@@END";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn near_miss_detection_basics() {
        assert_eq!(near_miss_marker("@@@OLD>"), Some(SEARCH_MARKER));
        assert_eq!(near_miss_marker("@@@OLD the old text"), Some(SEARCH_MARKER));
        assert_eq!(near_miss_marker("@@@NEWx"), Some(DIVIDER));
        assert_eq!(near_miss_marker("@@@END "), Some(REPLACE_MARKER));
        // Exact markers are not near misses.
        assert_eq!(near_miss_marker("@@@OLD"), None);
        assert_eq!(near_miss_marker("@@@NEW"), None);
        assert_eq!(near_miss_marker("@@@END"), None);
        // Indented, or mid-line, is content — not a near miss.
        assert_eq!(near_miss_marker("  @@@OLD>"), None);
        assert_eq!(near_miss_marker("see @@@OLD> here"), None);
    }

    #[test]
    fn near_miss_inside_old_section_is_ordinary_content() {
        // A near miss is not a delimiter, so inside a body it is content. It
        // may be exactly the text the caller means to write; the oldText match
        // is what decides whether it was a slip.
        let text = "@@@OLD\n@@@OLD>\n@@@NEW\nb\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![plain("@@@OLD>", "b")]);
    }

    #[test]
    fn near_miss_inside_new_section_is_insertable() {
        // The case that motivated relaxing the in-content check: a closed
        // block whose newText is literally `@@@END>` must be writable.
        let text = "some/path\n@@@OLD\nsome text\n@@@NEW\n@@@END>\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("some/path"));
        assert_eq!(p.edits, vec![headed("some/path", "some text", "@@@END>")]);
    }

    #[test]
    fn near_miss_multiline_new_body_is_insertable() {
        let text = "@@@OLD\nold\n@@@NEW\nkeep\n@@@OLD>\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![plain("old", "keep\n@@@OLD>")]);
    }

    #[test]
    fn malformed_opening_marker_reported_instead_of_no_blocks_found() {
        // The opening `@@@OLD` has trailing garbage, so no block parses at all.
        // We want the specific complaint, not "no blocks found".
        let text = "@@@OLD>\na\n@@@NEW\nb\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@OLD` marker `@@@OLD>`"),
            "{err}"
        );
        assert!(!err.0.contains("no `@@@OLD`"), "{err}");
    }

    #[test]
    fn malformed_divider_reports_missing_garbage_precisely() {
        // `@@@NEW>` does not close the OLD section, so the scan runs on and
        // trips over the later `@@@END`. The garbled divider is the root
        // cause, so that is what gets reported.
        let text = "@@@OLD\na\n@@@NEW>\nb\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@NEW` marker `@@@NEW>`"),
            "{err}"
        );
    }

    #[test]
    fn garbled_closing_marker_beats_the_error_it_causes() {
        // Same shape in the NEW section: a garbled `@@@END` swallows what
        // follows and eventually strands a valid marker.
        let text = "@@@OLD\nold\n@@@NEW\nnew\n@@@END>\n@@@OLD";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@END` marker `@@@END>`"),
            "{err}"
        );
    }

    #[test]
    fn unexpected_delimiter_still_reported_when_no_near_miss() {
        // No garbled marker in the region, so the structural error stands.
        let text = "@@@OLD\nold\n@@@OLD\n@@@NEW\nnew\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0
                .contains("unexpected delimiter `@@@OLD` in @@@OLD section"),
            "{err}"
        );
    }

    fn one_block(path: Option<&str>) -> ParsedBlocks {
        ParsedBlocks {
            path: path.map(str::to_string),
            multi_file: false,
            edits: vec![plain("a", "b")],
        }
    }

    #[test]
    fn merge_path_infers_from_header() {
        let (path, edits) = merge_path(None, one_block(Some("src/x.rs"))).unwrap();
        assert_eq!(path, "src/x.rs");
        assert_eq!(edits, vec![plain("a", "b")]);
    }

    #[test]
    fn merge_path_rejects_conflict() {
        assert!(merge_path(Some("a.rs"), one_block(Some("b.rs"))).is_err());
    }

    #[test]
    fn merge_path_accepts_equivalent_spellings() {
        // `./src/x.rs` and `src/x.rs` normalize equal.
        assert!(merge_path(Some("./src/x.rs"), one_block(Some("src/x.rs"))).is_ok());
    }

    #[test]
    fn merge_path_requires_a_path() {
        assert!(merge_path(None, one_block(None)).is_err());
    }

    #[test]
    fn canonicalize_blob_string() {
        let v = json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        let c = canonicalize_edit_value(&v);
        assert_eq!(
            c,
            json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );
    }

    #[test]
    fn canonicalize_single_oldtext() {
        let v = json!({"path": "src/x.rs", "oldText": "a", "newText": "b"});
        let c = canonicalize_edit_value(&v);
        assert_eq!(
            c,
            json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );
    }

    #[test]
    fn canonicalize_array_is_identity() {
        let v = json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]});
        assert_eq!(canonicalize_edit_value(&v), v);
    }

    #[test]
    fn a_blob_under_the_old_field_name_is_still_read() {
        // The rename must not break callers that have not seen it. `edits` as a
        // *string* is the pre-rename blob, and it still parses and canonicalizes
        // — a session recorded before the rename now normalizes on load, which
        // is a small recovery the rename had thrown away.
        let v = json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        assert!(blob_text(&v).is_some());
        assert_eq!(
            canonicalize_edit_value(&v),
            json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );
    }

    #[test]
    fn blob_text_reads_only_the_blocks_field() {
        let v = json!({"path": "p", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        assert!(blob_text(&v).is_some());
        // The array is the canonical form, never a blob.
        let arr = json!({"path": "p", "edits": [{"oldText": "a", "newText": "b"}]});
        assert_eq!(blob_text(&arr), None);
    }

    #[test]
    fn array_to_blob_emits_blocks_not_edits() {
        // The rename's whole point: the two fields are no longer confusable.
        let v = json!({"path": "p", "edits": [{"oldText": "a", "newText": "b"}]});
        let out = array_to_blob_value(&v);
        assert!(out.get("blocks").is_some(), "{out}");
        assert!(out.get("edits").is_none(), "{out}");
        assert_eq!(BLOCKS_FIELD, "blocks");
    }

    #[test]
    fn canonicalize_malformed_blob_is_identity() {
        let v = json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@END"});
        assert_eq!(canonicalize_edit_value(&v), v);
    }

    #[test]
    fn array_to_blob_round_trips() {
        let v = json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}, {"oldText": "c", "newText": "d"}]});
        let blob = array_to_blob_value(&v);
        let text = blob.get(BLOCKS_FIELD).and_then(|x| x.as_str()).unwrap();
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("src/x.rs"));
        assert_eq!(
            parsed.edits,
            vec![headed("src/x.rs", "a", "b"), plain("c", "d")]
        );
    }

    #[test]
    fn array_to_blob_is_idempotent_on_blob() {
        let v = json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        assert_eq!(array_to_blob_value(&v), v);
    }

    #[test]
    fn render_blob_multiline_round_trips() {
        let edits = vec![plain("fn a() {\n    x\n}", "fn a() {\n    y\n}")];
        let blob = render_blob("src/m.rs", &edits);
        let parsed = parse_blocks(&blob).unwrap();
        // render_blob writes the path once, above the first block, so the parse
        // recovers the same texts plus that header.
        assert_eq!(
            parsed.edits,
            vec![headed(
                "src/m.rs",
                "fn a() {\n    x\n}",
                "fn a() {\n    y\n}"
            )]
        );
    }

    #[test]
    fn rejects_old_delimiter_at_line_start_in_new_text() {
        // @@@OLD at start of line in newText should fail
        let text = "@@@OLD\nold\n@@@NEW\n@@@OLD\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(err
            .0
            .contains("unexpected delimiter `@@@OLD` in @@@NEW section"));
    }

    #[test]
    fn rejects_new_delimiter_at_line_start_in_old_text() {
        // @@@NEW at start of line in oldText should fail
        // Need actual content between @@@OLD and first @@@NEW (the divider)
        let text = "@@@OLD\nactual content\n@@@NEW\n@@@NEW\nnew\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(err
            .0
            .contains("unexpected delimiter `@@@NEW` in @@@NEW section"));
    }

    #[test]
    fn rejects_end_delimiter_at_line_start_in_old_text() {
        // @@@END at start of line in oldText should fail
        let text = "@@@OLD\n@@@END\n@@@NEW\nnew\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(err
            .0
            .contains("unexpected delimiter `@@@END` in @@@OLD section"));
    }

    #[test]
    fn allows_delimiter_in_middle_of_line() {
        // Delimiters NOT at start of line should be allowed (mentioned in content)
        let text = "@@@OLD\nold\n@@@NEW\nnew @@@OLD\n@@@END";
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.edits[0].new_text, "new @@@OLD");
    }

    #[test]
    fn allows_delimiter_with_leading_whitespace() {
        // Indented delimiters should be allowed (not at start of line)
        let text = "@@@OLD\nold\n@@@NEW\n  @@@OLD\n@@@END";
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.edits[0].new_text, "  @@@OLD");
    }

    #[test]
    fn end_marker_accepts_trailing_newlines() {
        // `str::lines` strips the terminator (and a CRLF's \r), so a closing
        // @@@END followed by any number of newlines must still match.
        for text in [
            "@@@OLD\nold\n@@@NEW\nnew\n@@@END",
            "@@@OLD\nold\n@@@NEW\nnew\n@@@END\n",
            "@@@OLD\nold\n@@@NEW\nnew\n@@@END\n\n\n",
            "@@@OLD\nold\n@@@NEW\nnew\n@@@END\r\n",
        ] {
            let parsed = parse_blocks(text).unwrap();
            assert_eq!(parsed.edits, vec![plain("old", "new")], "text: {text:?}");
        }
    }

    #[test]
    fn multi_block_adjacent_without_blank_line() {
        // A block may be followed immediately by the next @@@OLD.
        let text = "@@@OLD\na\n@@@NEW\nA\n@@@END\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![plain("a", "A"), plain("b", "B")]);
    }

    #[test]
    fn an_adjacent_marker_is_not_recorded_as_a_rejected_header() {
        // The previous block's `@@@END` sits in the header slot of an adjacent
        // block. Recording it would make the missing-file error quote a marker
        // back as if the model had written a bad header.
        let text = "@@@OLD\na\n@@@NEW\nA\n@@@END\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits[1].rejected_header, None);
    }

    #[test]
    fn multi_block_second_gets_blank_line_separator() {
        // Blank lines between blocks are not swallowed into the next OLD text.
        let text = "src/x.rs\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert_eq!(p.edits, vec![headed("src/x.rs", "a", "A"), plain("b", "B")]);
    }

    #[test]
    fn multi_block_second_may_carry_its_own_header() {
        // Each block may name the file; agreement is what makes it single-file.
        let text =
            "src/x.rs:1\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\nsrc/x.rs:40\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert!(!p.multi_file);
        assert_eq!(p.edits[0].start_line, Some(1));
        assert_eq!(p.edits[1].start_line, Some(40));
    }

    #[test]
    fn multi_block_trailing_newlines_after_last_end() {
        // Trailing newlines after the final @@@END do not create a phantom block.
        let text = "@@@OLD\na\n@@@NEW\nA\n@@@END\n\n\n@@@OLD\nb\n@@@NEW\nB\n@@@END\n\n";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![plain("a", "A"), plain("b", "B")]);
    }

    #[test]
    fn valid_block_still_works() {
        // Ensure we didn't break normal operation
        let text = "@@@OLD\nold\n@@@NEW\nnew\n@@@END";
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].old_text, "old");
        assert_eq!(parsed.edits[0].new_text, "new");
    }
}

/// One file's worth of blocks, in blob order.
#[derive(Debug, Clone, PartialEq)]
pub struct FileBatch {
    /// The path, as the header spelled it (or the caller's `path` argument).
    pub path: String,
    pub edits: Vec<ParsedEdit>,
}

/// Split a parsed blob into one batch per file, in first-seen order.
///
/// This is the seam between "a blob may span files" and "a write is atomic per
/// file": a recursive `bsed preview` emits headers for many files, and both
/// `bsed apply` and the agent's multi-file path need to turn that into one
/// batch per file rather than one giant batch nobody can apply safely.
///
/// Blocks with no header of their own go to `fallback` — the caller's `path`
/// argument — which is the single-file case, and the reason a header-less blob
/// still works. Errors only when a block names no file and there is no fallback
/// to attribute it to.
pub fn group_by_file(
    parsed: ParsedBlocks,
    fallback: Option<&str>,
) -> Result<Vec<FileBatch>, String> {
    let mut order: Vec<String> = Vec::new();
    let mut batches: std::collections::HashMap<String, Vec<ParsedEdit>> =
        std::collections::HashMap::new();

    for (idx, edit) in parsed.edits.into_iter().enumerate() {
        let path = match edit.header.as_deref().or(fallback) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => {
                return Err(format!(
                    "block {idx} names no file: set the `path` argument, or put a filename line \
                     above every `@@@OLD`",
                    idx = idx + 1
                ))
            }
        };
        if !batches.contains_key(&path) {
            order.push(path.clone());
        }
        batches.entry(path).or_default().push(edit);
    }

    Ok(order
        .into_iter()
        .map(|path| {
            let edits = batches.remove(&path).unwrap_or_default();
            FileBatch { path, edits }
        })
        .collect())
}

/// Upper bound on files in one call. Beyond it the blob is rejected whole,
/// before any file is touched.
///
/// This bounds the work a single string argument can fan out into. The
/// single-file form caps *blocks* not at all, so there is no precedent to copy
/// here; 32 is chosen because a cross-file refactor fits under it comfortably
/// and the bound is about boundedness, not about taste.
pub const MAX_FILES_PER_CALL: usize = 32;

/// The path each block names, keyed for grouping, in first-seen order.
///
/// `split_by_file` is the *multi-file* path policy, and it is deliberately
/// stricter than [`group_by_file`] and than [`merge_path`]:
///
/// 1. **Every** block must carry its own header. No defaulting — not to the
///    `path` argument, not to the previous block. A header-less block in a
///    blob that spans files has two or more plausible homes, and any rule that
///    picks one is guessing at a write. An error costs one round trip.
/// 2. The `path` argument is *not consulted at all*. The headers are
///    authoritative: a call that names its files in the blob has already
///    answered the question, and reconciling a redundant `path` against N
///    headers is a second, conflicting source of truth. (`path` remains the
///    single-edit form's file argument; see the tool's `description()`.)
/// 3. Grouping is keyed on the **normalized** path, so `./a.rs` and `a.rs`
///    are one file and one batch — two all-or-nothing applies for one file
///    would be a bug, not a feature.
/// 4. More than [`MAX_FILES_PER_CALL`] distinct files is rejected whole.
///
/// Unlike `merge_path` there is no fallback: a single-file blob still goes
/// through `merge_path` and still defaults freely, because there is only one
/// candidate and a default cannot be wrong. That is also why this function is
/// not a drop-in for `group_by_file` — see the hazards its own tests pin.
pub fn split_by_file(parsed: ParsedBlocks) -> Result<Vec<FileBatch>, String> {
    if parsed.edits.is_empty() {
        return Err("no edit blocks found".to_string());
    }

    let mut order: Vec<String> = Vec::new();
    let mut batches: std::collections::HashMap<String, Vec<ParsedEdit>> =
        std::collections::HashMap::new();

    for (i, edit) in parsed.edits.into_iter().enumerate() {
        let spelled = match edit.header.as_deref() {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => return Err(no_file_error(i + 1, edit.rejected_header.as_deref())),
        };
        // Key on the normalized form; report and store the first spelling.
        let key = normalized_key(&spelled);
        if !batches.contains_key(&key) {
            order.push(spelled);
        }
        batches.entry(key).or_default().push(edit);
    }

    if order.len() > MAX_FILES_PER_CALL {
        return Err(format!(
            "this edit names {} files; the limit is {MAX_FILES_PER_CALL}. Split it into \
             several `edit` calls",
            order.len()
        ));
    }

    Ok(order
        .into_iter()
        .map(|path| {
            let edits = batches.remove(&normalized_key(&path)).unwrap_or_default();
            FileBatch { path, edits }
        })
        .collect())
}

/// The grouping key for a spelled path: the normalized form when a cwd is
/// available, so equivalent spellings collapse; the spelling itself otherwise.
///
/// Mirrors [`same_path`]'s fallback, so grouping and agreement can never
/// disagree about what "the same file" means.
fn normalized_key(path: &str) -> String {
    match std::env::current_dir() {
        Ok(cwd) => crate::path::resolve_normalized(path, &cwd)
            .to_string_lossy()
            .into_owned(),
        Err(_) => path.to_string(),
    }
}

/// A block that named no file, in a blob that spans files.
///
/// Named by index, quoted back what was actually in the header slot when there
/// was something there, and fixed by the one edit that repairs it. The `path`
/// argument is deliberately *not* offered as a fix: this blob already spans
/// files, so a single `path` cannot name the missing block's file, and
/// defaulting it is the wrong-file write this policy exists to prevent.
fn no_file_error(idx: usize, rejected: Option<&str>) -> String {
    let shape = "\n  src/foo.rs:12\n  @@@OLD\n  ...\n  @@@NEW\n  ...\n  @@@END";
    let msg = match rejected {
        // Quoting the line back is the whole point: a bare "names no file"
        // tells the model to add a line it may already have added. The two
        // causes need different fixes, so they are named separately.
        Some(line) if line.split_whitespace().count() > 1 => format!(
            "block {idx} of a multi-file edit names no file: the line above its `@@@OLD` \
             marker is `{line}`, which is not a header. A header is one path token with no \
             spaces. If that is a real filename containing a space, this block form cannot \
             address it — edit that file with the single-edit fields (path + oldText + \
             newText) instead.{shape}"
        ),
        Some(line) => format!(
            "block {idx} of a multi-file edit names no file: the line above its `@@@OLD` \
             marker is `{line}`, which is not a path. Replace it with a path to the file this \
             block edits, like the other blocks.{shape}"
        ),
        None => format!(
            "block {idx} of a multi-file edit names no file. Put a filename line above its \
             `@@@OLD` marker, like the other blocks:{shape}"
        ),
    };
    msg
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn parse(text: &str) -> ParsedBlocks {
        parse_blocks(text).unwrap()
    }

    #[test]
    fn a_recursive_blob_groups_by_header_in_first_seen_order() {
        // What `bsed preview -r` emits: headers interleaved per file.
        let text = "a.rs:1\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\nb.rs:4\n@@@OLD\ny\n@@@NEW\nY\n@@@END\n\na.rs:9\n@@@OLD\nz\n@@@NEW\nZ\n@@@END";
        let g = group_by_file(parse(text), None).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].path, "a.rs");
        assert_eq!(g[0].edits.len(), 2);
        assert_eq!(g[0].edits[0].start_line, Some(1));
        assert_eq!(g[0].edits[1].start_line, Some(9));
        assert_eq!(g[1].path, "b.rs");
        assert_eq!(g[1].edits.len(), 1);
    }

    #[test]
    fn header_less_blocks_fall_back_to_the_given_path() {
        let text = "@@@OLD\nx\n@@@NEW\nX\n@@@END";
        let g = group_by_file(parse(text), Some("src/x.rs")).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].path, "src/x.rs");
    }

    #[test]
    fn a_header_beats_the_fallback() {
        // The blob names its own file; `path` only fills gaps.
        let text = "b.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let g = group_by_file(parse(text), Some("a.rs")).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].path, "b.rs");
        assert_eq!(g[1].path, "a.rs");
    }

    #[test]
    fn no_header_and_no_fallback_is_an_error() {
        let text = "@@@OLD\nx\n@@@NEW\nX\n@@@END";
        let err = group_by_file(parse(text), None).unwrap_err();
        assert!(err.contains("names no file"), "{err}");
    }

    #[test]
    fn the_error_names_the_offending_block_not_the_group_count() {
        // Blocks 1-3 all name `a.rs`, so there is one *group*; the header-less
        // block is block 4. Counting groups reported "block 2", pointing the
        // model at a block that is fine.
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    @@@OLD\ny\n@@@NEW\nY\n@@@END";
        let err = group_by_file(parse(text), None).unwrap_err();
        assert!(err.contains("block 4"), "{err}");
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;

    fn parse(text: &str) -> ParsedBlocks {
        parse_blocks(text).unwrap()
    }

    /// A blob whose blocks each name their own file, as `split_by_file` requires.
    fn blob(spec: &[(&str, &str)]) -> ParsedBlocks {
        let mut text = String::new();
        for (path, body) in spec {
            text.push_str(path);
            text.push('\n');
            text.push_str(SEARCH_MARKER);
            text.push('\n');
            text.push_str(body);
            text.push('\n');
            text.push_str(DIVIDER);
            text.push('\n');
            text.push_str("new");
            text.push('\n');
            text.push_str(REPLACE_MARKER);
            text.push('\n');
            text.push('\n');
        }
        parse(&text)
    }

    #[test]
    fn splits_into_one_batch_per_file_in_first_seen_order() {
        let g = split_by_file(blob(&[("b.rs", "1"), ("a.rs", "2"), ("b.rs", "3")])).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].path, "b.rs");
        assert_eq!(g[0].edits.len(), 2);
        assert_eq!(g[1].path, "a.rs");
        assert_eq!(g[1].edits.len(), 1);
    }

    #[test]
    fn a_single_file_blob_is_one_batch() {
        let g = split_by_file(blob(&[("src/a.rs", "1"), ("src/a.rs", "2")])).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].path, "src/a.rs");
        assert_eq!(g[0].edits.len(), 2);
    }

    // -- rule 1: every block names its own file, with no defaulting ---------

    #[test]
    fn a_header_less_block_is_an_error_even_when_other_blocks_are_named() {
        // The hazard `group_by_file` has and this must not: a header-less
        // block silently attributed to whatever came before (or to `path`).
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let err = split_by_file(parse(text)).unwrap_err();
        assert!(err.contains("block 2"), "{err}");
    }

    #[test]
    fn the_missing_file_error_names_the_index_and_shows_the_shape() {
        // The self-correcting style: name the one problem, show the one
        // accepted shape, and do not offer a fix that would be wrong here.
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let err = split_by_file(parse(text)).unwrap_err();
        assert!(
            err.contains("block 2 of a multi-file edit names no file"),
            "{err}"
        );
        assert!(err.contains(SEARCH_MARKER), "must show the shape: {err}");
        assert!(err.contains("src/foo.rs:12"), "must show a header: {err}");
        assert!(
            !err.contains("`path` argument"),
            "must not offer `path` as the fix: {err}"
        );
    }

    #[test]
    fn the_missing_file_error_never_offers_the_path_argument() {
        // `path` cannot name the missing block's file in a blob that spans
        // files, so suggesting it would send the model in a circle.
        let err = no_file_error(3, None);
        assert!(err.contains("block 3"), "{err}");
        assert!(!err.contains("`path` argument"), "{err}");
    }

    #[test]
    fn a_rejected_header_line_is_quoted_back() {
        // The model wrote *something* in the header slot. Saying only "names
        // no file" tells it to add a line it already added; quoting the line
        // is what makes the error actionable.
        let err = no_file_error(2, Some("Update the parser."));
        assert!(err.contains("`Update the parser.`"), "{err}");
        // A single whitespace-free token is a *wrong path*, not a spaced one.
        let err = no_file_error(2, Some("src/typo.rs"));
        assert!(err.contains("which is not a path"), "{err}");
        assert!(!err.contains("no spaces"), "{err}");
    }

    #[test]
    fn a_multi_word_line_is_reported_as_a_possible_spaced_path() {
        // Ambiguous by nature — prose and a spaced filename look the same —
        // so the message covers both and routes the unrepresentable case to
        // the form that can express it.
        let err = no_file_error(1, Some("my notes.rs:4"));
        assert!(err.contains("no spaces"), "{err}");
        assert!(err.contains("oldText"), "{err}");
    }

    #[test]
    fn a_path_with_a_space_is_reported_and_routed_to_the_single_edit_form() {
        // A header must be one whitespace-free token, so a real filename with
        // a space cannot be expressed this way. With no `path` argument to
        // fall back on, the single-edit fields are the only way through.
        let text = "my notes.rs:4\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    src/b.rs:3\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let err = split_by_file(parse(text)).unwrap_err();
        assert!(err.contains("`my notes.rs:4`"), "{err}");
        assert!(err.contains("oldText"), "{err}");
    }

    #[test]
    fn an_empty_blob_is_an_error() {
        assert!(split_by_file(ParsedBlocks {
            path: None,
            multi_file: true,
            edits: vec![],
        })
        .is_err());
    }

    // -- rule 2: headers are authoritative; `path` is never consulted ------

    #[test]
    fn block_paths_are_authoritative_and_the_path_argument_is_not_consulted() {
        // The function takes no `path` at all, so there is no code path by
        // which a redundant argument could re-target a file.
        let g = split_by_file(blob(&[("a.rs", "1"), ("b.rs", "2")])).unwrap();
        assert_eq!(
            g.iter().map(|b| b.path.as_str()).collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"]
        );
    }

    // -- rule 3: grouping keys on the normalized path ----------------------

    #[test]
    fn equivalent_spellings_are_one_batch() {
        // `group_by_file` keys on the raw string, so this is two batches and
        // two applies for one file — the bug the keying rule closes.
        let g = split_by_file(blob(&[("./a.rs", "1"), ("a.rs", "2")])).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].edits.len(), 2);
        // Reported with the first spelling the model used.
        assert_eq!(g[0].path, "./a.rs");
    }

    #[test]
    fn the_batch_path_is_the_first_spelling_not_the_normalized_one() {
        let g = split_by_file(blob(&[("./a.rs", "1")])).unwrap();
        assert_eq!(g[0].path, "./a.rs");
    }

    // -- rule 4: the fan-out cap -------------------------------------------

    #[test]
    fn exactly_the_cap_is_accepted_and_one_more_is_rejected() {
        let at_cap: Vec<(String, &str)> = (0..MAX_FILES_PER_CALL)
            .map(|i| (format!("f{i}.rs"), "x"))
            .collect();
        let spec: Vec<(&str, &str)> = at_cap.iter().map(|(p, b)| (p.as_str(), *b)).collect();
        assert_eq!(
            split_by_file(blob(&spec)).unwrap().len(),
            MAX_FILES_PER_CALL
        );

        let over: Vec<(String, &str)> = (0..=MAX_FILES_PER_CALL)
            .map(|i| (format!("f{i}.rs"), "x"))
            .collect();
        let spec: Vec<(&str, &str)> = over.iter().map(|(p, b)| (p.as_str(), *b)).collect();
        let err = split_by_file(blob(&spec)).unwrap_err();
        assert!(
            err.contains(&format!("the limit is {MAX_FILES_PER_CALL}")),
            "{err}"
        );
    }

    #[test]
    fn the_cap_counts_files_not_blocks() {
        // Many blocks, one file: not a fan-out, so not capped.
        let spec: Vec<(&str, &str)> = (0..MAX_FILES_PER_CALL + 5).map(|_| ("a.rs", "x")).collect();
        let g = split_by_file(blob(&spec)).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].edits.len(), MAX_FILES_PER_CALL + 5);
    }

    // -- the hazard this whole function exists to avoid -------------------

    #[test]
    fn a_header_less_block_never_becomes_a_second_file() {
        // `group_by_file` with a `path` fallback turns exactly this blob into
        // two batches. Here it is one error, and no file is implied.
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        assert!(split_by_file(parse(text)).is_err());
    }
}
