//! Aider-style SEARCH/REPLACE "blob" edit format: parser and value-level
//! transforms.
//!
//! See `doc/batch_editing_v2.md`. Three representations are in play:
//!
//! - **Blob** — the model-facing text format, a filename line followed by one
//!   or more `SEARCH_MARKER` / `DIVIDER` / `REPLACE_MARKER` blocks.
//! - **Canonical array** — the internal/persisted form, pi-compatible:
//!   `{"path": ..., "edits": [{"oldText": ..., "newText": ...}, ...]}`.
//! - **Legacy** — `oldText`/`newText` (single) and `old`/`new` (very old
//!   picocode). Accepted on ingress / replay only.
//!
//! This module is pure: it operates on `&str` and [`serde_json::Value`] and
//! knows nothing about rig types (the `Message`-level rewrite loops live at
//! the call sites).

use serde_json::{json, Value};

/// Opening marker of a SEARCH region.
pub const SEARCH_MARKER: &str = "@@@OLD";
/// Divider between the SEARCH and REPLACE regions.
pub const DIVIDER: &str = "@@@DIV";
/// Closing marker of a REPLACE region.
pub const REPLACE_MARKER: &str = "@@@NEW";

/// One parsed SEARCH/REPLACE block.
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

/// Whether a line is one of the three structural markers (trimmed comparison,
/// so trailing whitespace from the model does not defeat detection).
///
/// For SEARCH only, be lenient with a single hallucinated closing bracket/brace
/// (`>`, `]`, `}`, `)`) that some models emit — e.g. `@@@OLD>`
/// or `@@@OLD]`. The prefix must match exactly; only a single such
/// char is allowed and nothing else may follow. REPLACE and DIVIDER remain
/// strict.
fn marker_of(line: &str) -> Option<&'static str> {
    let t = line.trim();
    // Lenient SEARCH marker: exact `<<<<<<< SEARCH` or with exactly one
    // trailing char from the set `>]})`.
    if let Some(after) = t.strip_prefix(SEARCH_MARKER) {
        if after.is_empty() {
            return Some(SEARCH_MARKER);
        }
        if after.chars().count() == 1
            && matches!(after.chars().next(), Some('>') | Some(']') | Some('}') | Some(')'))
        {
            return Some(SEARCH_MARKER);
        }
    }
    if t == DIVIDER {
        return Some(DIVIDER);
    }
    if t == REPLACE_MARKER {
        return Some(REPLACE_MARKER);
    }
    None
}

/// Strip a single surrounding triple-backtick fence, if present.
fn strip_fence(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();

    // Split any `@@@OLD>content...` (or `]`, `}`, `)`) line into the
    // marker line and the content line. Models occasionally hallucinate the
    // bracket without a following newline. The prefix `@@@OLD` must
    // match exactly; the first char of `rest` must be one of `>]})`.
    let mut i = 0;
    while i < lines.len() {
        // Determine whether to split, extracting any needed data without
        // keeping a borrow across the mutation.
        let should_split = {
            let line = &lines[i];
            if let Some(rest) = line.trim_start().strip_prefix(SEARCH_MARKER) {
                let first = rest.chars().next();
                if matches!(first, Some('>') | Some(']') | Some('}') | Some(')')) {
                    let content_after = rest[1..].trim_start();
                    if !content_after.is_empty() {
                        Some(content_after.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(content_after) = should_split {
            lines[i] = SEARCH_MARKER.to_string();
            lines.insert(i + 1, content_after);
            i += 2;
            continue;
        }
        i += 1;
    }

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
/// line immediately preceding a `<<<<<<< SEARCH` is treated as an optional
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

        // Collect SEARCH content up to the divider.
        let mut j = i + 1;
        let mut search_lines: Vec<&str> = Vec::new();
        while j < lines.len() && marker_of(&lines[j]) != Some(DIVIDER) {
            search_lines.push(&lines[j]);
            j += 1;
        }
        if j >= lines.len() {
            return Err(ParseError(format!(
                "a {SEARCH_MARKER} block is missing its `{DIVIDER}` divider"
            )));
        }

        // Collect REPLACE content up to the closing marker.
        let mut k = j + 1;
        let mut replace_lines: Vec<&str> = Vec::new();
        while k < lines.len() && marker_of(&lines[k]) != Some(REPLACE_MARKER) {
            replace_lines.push(&lines[k]);
            k += 1;
        }
        if k >= lines.len() {
            return Err(ParseError(format!(
                "a {SEARCH_MARKER} block is missing its `{REPLACE_MARKER}` closing marker"
            )));
        }

        let old_text = search_lines.join("\n");
        if old_text.is_empty() {
            return Err(ParseError(
                "a {SEARCH_MARKER} block has empty search text".to_string(),
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
        return Err(ParseError(format!("no `{SEARCH_MARKER}` blocks found")));
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
             `<<<<<<< SEARCH` marker"
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
        let text = "src/foo.py\n@@@OLD\ndef foo():\n    return 1\n@@@DIV\ndef foo():\n    return 2\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![plain("def foo():\n    return 1", "def foo():\n    return 2")]
        );
        assert_eq!(p.edits[0].start_line, None);
    }

    #[test]
    fn parses_two_blocks_same_file_second_inherits() {
        let text = "src/foo.py\n@@@OLD\na\n@@@DIV\nA\n@@@NEW\n\n@@@OLD\nb\n@@@DIV\nB\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![plain("a", "A"), plain("b", "B")]
        );
    }

    #[test]
    fn header_line_anchor_is_captured_per_block() {
        // `path:LINE` on a header supplies that block's anchor; differing
        // anchors on the same file do NOT count as a multi-file attempt.
        let text = "src/foo.py:10\n@@@OLD\na\n@@@DIV\nA\n@@@NEW\n\nsrc/foo.py:42\n@@@OLD\nb\n@@@DIV\nB\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(p.edits.len(), 2);
        assert_eq!(p.edits[0].start_line, Some(10));
        assert_eq!(p.edits[1].start_line, Some(42));
    }

    #[test]
    fn header_line_anchor_only_on_first_block() {
        let text = "src/foo.py:7\n@@@OLD\na\n@@@DIV\nA\n@@@NEW\n\n@@@OLD\nb\n@@@DIV\nB\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits[0].start_line, Some(7));
        assert_eq!(p.edits[1].start_line, None);
    }

    #[test]
    fn lenient_search_marker_accepts_trailing_bracket() {
        // The model occasionally hallucinates a closing bracket/brace after
        // SEARCH (e.g. `@@@OLD>` or `@@@OLD]`). Be lenient
        // for SEARCH only — the prefix must match exactly, only one char from
        // the set `>]})` is allowed, and nothing else may follow.
        let text = "src/foo.py\n@@@OLD>\nold\n@@@DIV\nnew\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert_eq!(p.edits, vec![plain("old", "new")]);

        let text2 = "src/foo.py\n@@@OLD]\nold\n@@@DIV\nnew\n@@@NEW";
        let p2 = parse_blocks(text2).unwrap();
        assert_eq!(p2.edits, vec![plain("old", "new")]);

        let text3 = "src/foo.py\n@@@OLD}\nold\n@@@DIV\nnew\n@@@NEW";
        let p3 = parse_blocks(text3).unwrap();
        assert_eq!(p3.edits, vec![plain("old", "new")]);

        let text4 = "src/foo.py\n@@@OLD)\nold\n@@@DIV\nnew\n@@@NEW";
        let p4 = parse_blocks(text4).unwrap();
        assert_eq!(p4.edits, vec![plain("old", "new")]);
    }

    #[test]
    fn lenient_search_marker_rejects_invalid_brackets() {
        // A char outside the allowed set, or a space before the bracket,
        // must NOT be accepted.
        assert!(parse_blocks("@@@OLDX\nold\n@@@DIV\nnew\n@@@NEW").is_err());
        assert!(parse_blocks("@@@OLD ]\nold\n@@@DIV\nnew\n@@@NEW").is_err());
    }

    #[test]
    fn lenient_search_marker_splits_hallucinated_bracket_from_content() {
        // `<<<<<<< SEARCH>>` → treat as `<<<<<<< SEARCH\n>` (marker + content line ">")
        let text = "@@@OLD>>\nold\n@@@DIV\nnew\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![plain(">\nold", "new")]);
    }

    #[test]
    fn lenient_only_for_search_not_replace() {
        // The closing marker must remain exact — no leniency for REPLACE.
        assert!(parse_blocks("@@@OLD\nold\n@@@DIV\nnew\n@@@NEW>").is_err());
        assert!(parse_blocks("@@@OLD\nold\n@@@DIV\nnew\n@@@NEW]").is_err());
    }

    #[test]
    fn lenient_search_marker_accepts_content_on_same_line() {
        // When the bracket is followed directly by content (no newline), the
        // line is split and the content becomes the first SEARCH line.
        let text = "src/foo.py\n@@@OLD>old\n@@@DIV\nnew\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert_eq!(p.edits, vec![plain("old", "new")]);

        let text2 = "src/foo.py\n@@@OLD]old\n@@@DIV\nnew\n@@@NEW";
        let p2 = parse_blocks(text2).unwrap();
        assert_eq!(p2.edits, vec![plain("old", "new")]);

        // Multi-line content on the same line after the bracket.
        let text3 = "src/foo.py\n@@@OLD>line 1\nline 2\n@@@DIV\nnew\n@@@NEW";
        let p3 = parse_blocks(text3).unwrap();
        assert_eq!(p3.edits, vec![plain("line 1\nline 2", "new")]);
    }

    #[test]
    fn lenient_search_marker_newline_style_still_works() {
        // The original `<<<<<<< SEARCH>\n` style still parses correctly.
        let text = "src/foo.py\n@@@OLD>\nold\n@@@DIV\nnew\n@@@NEW";
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
        let text = "a.rs\n@@@OLD\nx\n@@@DIV\ny\n@@@NEW\n\nb.rs\n@@@OLD\np\n@@@DIV\nq\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        assert!(p.multi_file);
        assert!(merge_path(Some("a.rs"), p).is_err());
    }

    #[test]
    fn strips_fences() {
        let text = "```\nsrc/x.rs\n@@@OLD\na\n@@@DIV\nb\n@@@NEW\n```";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert_eq!(p.edits, vec![plain("a", "b")]);
    }

    #[test]
    fn prose_before_block_is_not_a_header() {
        let text = "Sure, here is the change:\n\n@@@OLD\na\n@@@DIV\nb\n@@@NEW";
        let p = parse_blocks(text).unwrap();
        // The immediate predecessor of SEARCH is blank → no header.
        assert_eq!(p.path, None);
        assert_eq!(p.edits, vec![plain("a", "b")]);
    }

    #[test]
    fn missing_divider_errors() {
        let text = "@@@OLD\na\n@@@NEW";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn missing_replace_errors() {
        let text = "@@@OLD\na\n@@@DIV\nb";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn empty_search_errors() {
        let text = "@@@OLD\n@@@DIV\nb\n@@@NEW";
        assert!(parse_blocks(text).is_err());
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
        let v =
            json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@DIV\nb\n@@@NEW"});
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
        let v = json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW"});
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
}
