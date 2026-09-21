//! Aider-style SEARCH/REPLACE "blob" edit format: parser and value-level
//! transforms.
//!
//! See `doc/batch_editing_v2.md`. Three representations are in play:
//!
//! - **Blob** — the model-facing text format, a filename line followed by one
//!   or more `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` blocks.
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
pub const SEARCH_MARKER: &str = "<<<<<<< SEARCH";
/// Divider between the SEARCH and REPLACE regions.
pub const DIVIDER: &str = "=======";
/// Closing marker of a REPLACE region.
pub const REPLACE_MARKER: &str = ">>>>>>> REPLACE";

/// Result of parsing a blob.
#[derive(Debug, PartialEq)]
pub struct ParsedBlocks {
    /// The inferred single file path, when every present header agrees (and at
    /// least one header is present). `None` when no header was found.
    pub path: Option<String>,
    /// True when two blocks carry *differing* header paths (multi-file attempt).
    pub multi_file: bool,
    /// `(old_text, new_text)` in text order.
    pub edits: Vec<(String, String)>,
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
fn marker_of(line: &str) -> Option<&'static str> {
    let t = line.trim();
    if t == SEARCH_MARKER {
        Some(SEARCH_MARKER)
    } else if t == DIVIDER {
        Some(DIVIDER)
    } else if t == REPLACE_MARKER {
        Some(REPLACE_MARKER)
    } else {
        None
    }
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

/// Parse a blob into `(old, new)` blocks plus the inferred path.
///
/// Recognizes blocks whose markers each occupy their own (trimmed) line. The
/// line immediately preceding a `<<<<<<< SEARCH` is treated as an optional
/// filename header when it is a single whitespace-free token.
pub fn parse_blocks(text: &str) -> Result<ParsedBlocks, ParseError> {
    let lines = strip_fence(text);
    let mut edits: Vec<(String, String)> = Vec::new();
    let mut distinct_headers: Vec<String> = Vec::new();
    let mut saw_any_header = false;

    let mut i = 0usize;
    while i < lines.len() {
        if marker_of(&lines[i]) != Some(SEARCH_MARKER) {
            i += 1;
            continue;
        }

        // Optional header = the immediately preceding line, if it is a path.
        if i > 0 {
            if let Some(h) = header_candidate(&lines[i - 1]) {
                saw_any_header = true;
                if !distinct_headers.iter().any(|d| same_path(d, &h)) {
                    distinct_headers.push(h);
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
                "a SEARCH block is missing its `{DIVIDER}` divider"
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
                "a SEARCH block is missing its `{REPLACE_MARKER}` closing marker"
            )));
        }

        let old_text = search_lines.join("\n");
        if old_text.is_empty() {
            return Err(ParseError(
                "a SEARCH block has empty search text".to_string(),
            ));
        }
        let new_text = replace_lines.join("\n");
        edits.push((old_text, new_text));

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
) -> Result<(String, Vec<(String, String)>), String> {
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
/// first block (Aider-style). `startLine`/`endLine` are not represented.
pub fn render_blob(path: &str, edits: &[(String, String)]) -> String {
    let mut out = String::new();
    if !path.is_empty() {
        out.push_str(path);
        out.push('\n');
    }
    for (i, (old, new)) in edits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(SEARCH_MARKER);
        out.push('\n');
        out.push_str(old);
        out.push('\n');
        out.push_str(DIVIDER);
        out.push('\n');
        out.push_str(new);
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

/// Build the canonical `edits` array value from block pairs.
fn edits_to_value(pairs: &[(String, String)]) -> Value {
    Value::Array(
        pairs
            .iter()
            .map(|(o, n)| json!({ "oldText": o, "newText": n }))
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
    let pairs: Vec<(String, String)> = edits
        .iter()
        .map(|e| {
            (
                e.get("oldText")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                e.get("newText")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        })
        .collect();
    let blob = render_blob(path, &pairs);
    json!({ "path": path, "edits": blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_block_with_path() {
        let text = "src/foo.py\n<<<<<<< SEARCH\ndef foo():\n    return 1\n=======\ndef foo():\n    return 2\n>>>>>>> REPLACE";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![(
                "def foo():\n    return 1".to_string(),
                "def foo():\n    return 2".to_string()
            )]
        );
    }

    #[test]
    fn parses_two_blocks_same_file_second_inherits() {
        let text = "src/foo.py\n<<<<<<< SEARCH\na\n=======\nA\n>>>>>>> REPLACE\n\n<<<<<<< SEARCH\nb\n=======\nB\n>>>>>>> REPLACE";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![("a".into(), "A".into()), ("b".into(), "B".into())]
        );
    }

    #[test]
    fn flags_multi_file_on_differing_headers() {
        let text = "a.rs\n<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n\nb.rs\n<<<<<<< SEARCH\np\n=======\nq\n>>>>>>> REPLACE";
        let p = parse_blocks(text).unwrap();
        assert!(p.multi_file);
        assert!(merge_path(Some("a.rs"), p).is_err());
    }

    #[test]
    fn strips_fences() {
        let text = "```\nsrc/x.rs\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/x.rs"));
        assert_eq!(p.edits, vec![("a".into(), "b".into())]);
    }

    #[test]
    fn prose_before_block_is_not_a_header() {
        let text = "Sure, here is the change:\n\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE";
        let p = parse_blocks(text).unwrap();
        // The immediate predecessor of SEARCH is blank → no header.
        assert_eq!(p.path, None);
        assert_eq!(p.edits, vec![("a".into(), "b".into())]);
    }

    #[test]
    fn missing_divider_errors() {
        let text = "<<<<<<< SEARCH\na\n>>>>>>> REPLACE";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn missing_replace_errors() {
        let text = "<<<<<<< SEARCH\na\n=======\nb";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn empty_search_errors() {
        let text = "<<<<<<< SEARCH\n=======\nb\n>>>>>>> REPLACE";
        assert!(parse_blocks(text).is_err());
    }

    #[test]
    fn merge_path_infers_from_header() {
        let p = ParsedBlocks {
            path: Some("src/x.rs".into()),
            multi_file: false,
            edits: vec![("a".into(), "b".into())],
        };
        let (path, edits) = merge_path(None, p).unwrap();
        assert_eq!(path, "src/x.rs");
        assert_eq!(edits, vec![("a".into(), "b".into())]);
    }

    #[test]
    fn merge_path_rejects_conflict() {
        let p = ParsedBlocks {
            path: Some("b.rs".into()),
            multi_file: false,
            edits: vec![("a".into(), "b".into())],
        };
        assert!(merge_path(Some("a.rs"), p).is_err());
    }

    #[test]
    fn merge_path_accepts_equivalent_spellings() {
        let p = ParsedBlocks {
            path: Some("src/x.rs".into()),
            multi_file: false,
            edits: vec![("a".into(), "b".into())],
        };
        // `./src/x.rs` and `src/x.rs` normalize equal.
        assert!(merge_path(Some("./src/x.rs"), p).is_ok());
    }

    #[test]
    fn merge_path_requires_a_path() {
        let p = ParsedBlocks {
            path: None,
            multi_file: false,
            edits: vec![("a".into(), "b".into())],
        };
        assert!(merge_path(None, p).is_err());
    }

    #[test]
    fn canonicalize_blob_string() {
        let v =
            json!({"path": "src/x.rs", "edits": "<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE"});
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
        let v = json!({"path": "src/x.rs", "edits": "<<<<<<< SEARCH\na\n>>>>>>> REPLACE"});
        assert_eq!(canonicalize_edit_value(&v), v);
    }

    #[test]
    fn array_to_blob_round_trips() {
        let v = json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}, {"oldText": "c", "newText": "d"}]});
        let blob = array_to_blob_value(&v);
        let text = blob.get("edits").and_then(|x| x.as_str()).unwrap();
        let parsed = parse_blocks(text).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("src/x.rs"));
        assert_eq!(
            parsed.edits,
            vec![("a".into(), "b".into()), ("c".into(), "d".into())]
        );
    }

    #[test]
    fn array_to_blob_is_idempotent_on_blob() {
        let v =
            json!({"path": "src/x.rs", "edits": "<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE"});
        assert_eq!(array_to_blob_value(&v), v);
    }

    #[test]
    fn render_blob_multiline_round_trips() {
        let edits = vec![(
            "fn a() {\n    x\n}".to_string(),
            "fn a() {\n    y\n}".to_string(),
        )];
        let blob = render_blob("src/m.rs", &edits);
        let parsed = parse_blocks(&blob).unwrap();
        assert_eq!(parsed.edits, edits);
    }
}
