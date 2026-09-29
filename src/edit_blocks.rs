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
/// This is a common model slip, and `marker_of` alone reports it only
/// indirectly (usually as a missing-divider or missing-marker error, or by
/// silently folding the line into the file content). Returns the marker the
/// line appears to start with.
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
        "malformed `{marker}` marker: `{line}`. A marker must occupy its whole line, \
         with nothing after it. If you meant to write the literal text `{marker}` \
         into the file, use the OpenAI single-edit format (path/oldText/newText) \
         instead, and use that format for this file going forward."
    ))
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
fn header_candidate(line: &str) -> Option<String> {
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
        if i > 0 {
            if let Some(h) = header_candidate(&lines[i - 1]) {
                let (path_part, anchor) = split_line_anchor(&h);
                saw_any_header = true;
                header_anchor = anchor;
                if !distinct_headers.iter().any(|d| same_path(d, &path_part)) {
                    distinct_headers.push(path_part);
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
                // Any other delimiter (@@@OLD or @@@END) is invalid in OLD section
                return Err(ParseError(format!(
                    "unexpected delimiter `{m}` in @@@OLD section; expected `@@@NEW`"
                )));
            }
            // A line that *starts* a marker but trails garbage is a near miss
            // for it, not content — report it as the malformed marker it is.
            if let Some(m) = near_miss_marker(&lines[j]) {
                return Err(malformed_marker_error(m, &lines[j]));
            }
            search_lines.push(&lines[j]);
            j += 1;
        }
        if j >= lines.len() {
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
                // Any other delimiter (@@@OLD or @@@NEW) is invalid in NEW section
                return Err(ParseError(format!(
                    "unexpected delimiter `{m}` in @@@NEW section; expected `@@@END`"
                )));
            }
            if let Some(m) = near_miss_marker(&lines[k]) {
                return Err(malformed_marker_error(m, &lines[k]));
            }
            replace_lines.push(&lines[k]);
            k += 1;
        }
        if k >= lines.len() {
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
        });

        i = k + 1;
    }

    if edits.is_empty() {
        // No block parsed. If some line merely *looked* like a marker, say so —
        // otherwise the model is told "no blocks found" for what is really a
        // formatting slip on its opening `@@@OLD`. Scanned only here, so a
        // near miss in ordinary prose outside any block stays harmless.
        if let Some(line) = lines.iter().find(|l| near_miss_marker(l).is_some()) {
            let m = near_miss_marker(line).expect("just checked");
            return Err(malformed_marker_error(m, line));
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
            crate::path_util::resolve_normalized(a, &cwd)
                == crate::path_util::resolve_normalized(b, &cwd)
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
/// - `edits` is a **string** (blob) → parse + merge → array;
/// - `edits` is already an **array** → unchanged (legacy/replay);
/// - top-level `oldText`/`newText` → single-entry array;
/// - legacy `old`/`new` → single-entry array.
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

    // Blob string.
    if let Some(text) = obj.get("edits").and_then(|v| v.as_str()) {
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
/// canonical-array edit call into the blob form. **Idempotent** — a value that
/// is not a canonical array is returned unchanged.
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
        })
        .collect();
    let blob = render_blob(path, &parsed);
    json!({ "path": path, "edits": blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block with no anchor, for terse comparisons.
    fn plain(old: &str, new: &str) -> ParsedEdit {
        ParsedEdit {
            old_text: old.to_string(),
            new_text: new.to_string(),
            start_line: None,
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
            vec![plain(
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
        assert_eq!(p.edits, vec![plain("a", "A"), plain("b", "B")]);
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
        assert_eq!(p.edits, vec![plain("old", "new")]);
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
        assert_eq!(p.edits, vec![plain("a", "b")]);
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
    fn malformed_marker_in_old_section_names_the_line() {
        let text = "@@@OLD\n@@@OLD>\n@@@NEW\nb\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@OLD` marker: `@@@OLD>`"),
            "{err}"
        );
        assert!(err.0.contains("single-edit format"), "{err}");
    }

    #[test]
    fn malformed_marker_in_new_section_names_the_line() {
        let text = "@@@OLD\na\n@@@NEW\n@@@END>\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@END` marker: `@@@END>`"),
            "{err}"
        );
    }

    #[test]
    fn malformed_opening_marker_reported_instead_of_no_blocks_found() {
        // The opening `@@@OLD` has trailing garbage, so no block parses at all.
        // We want the specific complaint, not "no blocks found".
        let text = "@@@OLD>\na\n@@@NEW\nb\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@OLD` marker: `@@@OLD>`"),
            "{err}"
        );
        assert!(!err.0.contains("no `@@@OLD`"), "{err}");
    }

    #[test]
    fn malformed_divider_reports_missing_garbage_precisely() {
        // `@@@NEW>` no longer closes the OLD section, so this is a near miss
        // rather than a bare "missing @@@NEW divider".
        let text = "@@@OLD\na\n@@@NEW>\nb\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0.contains("malformed `@@@NEW` marker: `@@@NEW>`"),
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
        let v = json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
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
    fn canonicalize_malformed_blob_is_identity() {
        let v = json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@END"});
        assert_eq!(canonicalize_edit_value(&v), v);
    }

    #[test]
    fn array_to_blob_round_trips() {
        let v = json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}, {"oldText": "c", "newText": "d"}]});
        let blob = array_to_blob_value(&v);
        let text = blob.get("edits").and_then(|x| x.as_str()).unwrap();
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("src/x.rs"));
        assert_eq!(parsed.edits, vec![plain("a", "b"), plain("c", "d")]);
    }

    #[test]
    fn array_to_blob_is_idempotent_on_blob() {
        let v =
            json!({"path": "src/x.rs", "edits": "<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE"});
        assert_eq!(array_to_blob_value(&v), v);
    }

    #[test]
    fn render_blob_multiline_round_trips() {
        let edits = vec![plain("fn a() {\n    x\n}", "fn a() {\n    y\n}")];
        let blob = render_blob("src/m.rs", &edits);
        let parsed = parse_blocks(&blob).unwrap();
        assert_eq!(parsed.edits, edits);
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
    fn multi_block_second_gets_blank_line_separator() {
        // Blank lines between blocks are not swallowed into the next OLD text.
        let text = "src/x.rs\n@@@OLD\na\n@@@NEW\nA\n@@@END\n\n@@@OLD\nb\n@@@NEW\nB\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert_eq!(p.edits, vec![plain("a", "A"), plain("b", "B")]);
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
