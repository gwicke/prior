//! Wire-format decoding for the `edit` tool: the raw `arguments` JSON into a
//! resolved path plus a batch of [`EditOp`]s.
//!
//! The advertised shape is a **flat bag of optional fields** (see the tool's
//! `parameters()`), so variant selection happens here, on the raw JSON, and no
//! `anyOf` union is ever advertised. Accepted shapes:
//!
//! - single edit: `{path, oldText, newText, startLine?}` — `startLine` is the
//!   anchor.
//! - block edit:  `{path?, edits: "<SEARCH/REPLACE blocks>"}` — anchors come
//!   from each block's `path:LINE` header; a top-level `startLine` is ignored
//!   in this shape.
//! - legacy batch (replay only): `{path, edits: [{oldText, newText, startLine?}]}`

use serde::Deserialize;
use serde_json::Value;

use crate::blob;
use crate::types::{EditOp, EditStartLine};

/// Cap on the JSON echoed back in an invalid-parameters error, so a bad call
/// carrying a huge blob (e.g. file contents pasted into the wrong field)
/// can't blow up the tool message.
pub const MAX_ECHOED_ARGS: usize = 400;

/// A string field shorter than this is echoed verbatim; longer ones are
/// elided, so small fields (a `path`, a short `oldText`) survive intact.
const ELIDE_MIN_CHARS: usize = 120;

/// Per-field budget for a long string that carries no structural markers —
/// nothing worth keeping line-by-line, so a prefix stands in for it.
const ELIDE_FIELD_BUDGET: usize = 160;

/// Lines of context kept either side of a structural (marker) line when
/// eliding a blob, so the block shape reads clearly.
const ELIDE_CONTEXT: usize = 1;

const EDIT_ARGS_EXAMPLE: &str = r#"Single edit: {"path": "some/path", "oldText": "original text", "newText": "replacement text"}
Multiple edits: {"path": "some/path", "edits": "@@@OLD\noriginal text\n@@@NEW\nreplacement text\n@@@END"}
Anchored block (when the text repeats): {"path": "some/path", "edits": "some/path:120\n@@@OLD\noriginal text\n@@@NEW\nreplacement text\n@@@END"}"#;

/// Arguments accepted by the `edit` tool, after normalization to the execution
/// form (a resolved single-file `path` plus one or more [`EditOp`]s).
#[derive(Debug)]
pub struct EditArgs {
    /// File to edit (from the `path` argument, or inferred from a block header).
    pub path: String,
    /// The edits to apply, computed against the original file.
    pub edits: Vec<EditOp>,
}

/// Truncate `s` to `max` bytes on a char boundary, appending ` …` when cut.
fn char_truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} …", &s[..cut])
}

/// A line worth keeping when eliding a SEARCH/REPLACE blob: the structural
/// markers. Prefix matching (not exact equality) so near-miss markers the
/// model invents — e.g. a stray `@@@OLD>` — are still shown.
fn is_structural_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with(blob::SEARCH_MARKER)
        || t.starts_with(blob::DIVIDER)
        || t.starts_with(blob::REPLACE_MARKER)
}

/// The marker emitted in place of a run of elided lines.
fn elision_notice(lines: usize) -> String {
    format!(
        "… ({lines} line{} elided) …",
        if lines == 1 { "" } else { "s" }
    )
}

/// Shrink a long string field by keeping only its structural lines (plus
/// [`ELIDE_CONTEXT`] lines of context) and collapsing every other run of lines
/// into a single [`elision_notice`]. A string with no structural lines degrades
/// to a plain prefix. Short strings pass through untouched.
fn elide_string(s: &str) -> String {
    if s.len() < ELIDE_MIN_CHARS {
        return s.to_string();
    }
    let lines: Vec<&str> = s.split('\n').collect();
    let structural: Vec<bool> = lines.iter().map(|l| is_structural_line(l)).collect();
    if !structural.iter().any(|&b| b) {
        return char_truncate(s, ELIDE_FIELD_BUDGET);
    }

    let mut keep = vec![false; lines.len()];
    for (i, &is_sep) in structural.iter().enumerate() {
        if !is_sep {
            continue;
        }
        let lo = i.saturating_sub(ELIDE_CONTEXT);
        let hi = (i + ELIDE_CONTEXT).min(lines.len() - 1);
        for k in lo..=hi {
            keep[k] = true;
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut run = 0usize;
    for (i, &k) in keep.iter().enumerate() {
        if k {
            if run > 0 {
                out.push(elision_notice(run));
                run = 0;
            }
            out.push(lines[i].to_string());
        } else {
            run += 1;
        }
    }
    if run > 0 {
        out.push(elision_notice(run));
    }
    out.join("\n")
}

/// Recursively replace long string values in `value` with their elided form.
fn elide_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(elide_string(s)),
        Value::Array(a) => Value::Array(a.iter().map(elide_value).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), elide_value(v))).collect())
        }
        other => other.clone(),
    }
}

/// Render the received parameters for the error echo, elided to fit
/// [`MAX_ECHOED_ARGS`]. A blob is shown by its *structure* — the marker lines
/// and a line of context around each — rather than by a raw byte prefix, so the
/// model can see the shape of what it sent.
fn truncate_wire_repr(raw: &Value) -> String {
    let s = raw.to_string();
    if s.len() <= MAX_ECHOED_ARGS {
        return s;
    }
    let elided = elide_value(raw).to_string();
    // Only prefer the elided form when it actually shrinks the payload.
    let picked = if elided.len() < s.len() { elided } else { s };
    char_truncate(&picked, MAX_ECHOED_ARGS)
}

impl EditArgs {
    /// Build the self-correcting error: what went wrong, the accepted shapes,
    /// and the (truncated) parameters actually received.
    fn invalid(raw: &Value, why: &str) -> String {
        format!(
            "Invalid edit tool arguments: {why}\n\nExpected one of these formats:\n{}\nActual parameters received: {}\nPlease do NOT use sed or python for editing, they lack the protections of this tool.",
            EDIT_ARGS_EXAMPLE,
            truncate_wire_repr(raw),
        )
    }

    /// Parse one legacy `edits[]` entry.
    fn op_from_value(e: &Value) -> Option<EditOp> {
        Some(EditOp {
            old_text: e.get("oldText")?.as_str()?.to_string(),
            new_text: e.get("newText")?.as_str()?.to_string(),
            start_line: e
                .get("startLine")
                .and_then(|v| v.as_u64())
                .map(EditStartLine),
        })
    }

    /// Parse the wire JSON. The `path` argument is authoritative; a filename
    /// line inside the blocks may agree with it (or supply it when absent), but
    /// a disagreement is a hard error (never silently write the wrong file).
    ///
    /// The `path` argument also accepts the `file:LINE` idiom; the trailing
    /// `:LINE` is peeled off before path resolution in **every** shape (so
    /// `f.rs:12` resolves to `f.rs`, not a nonexistent file named `f.rs:12`).
    ///
    /// Anchors are **per shape, never mixed**:
    ///
    /// - single edit: anchors come from `path:LINE` and/or `startLine` (they
    ///   must agree if both are present);
    /// - blob form: anchors come *only* from each block's `path:LINE` header —
    ///   the top-level `startLine` and any `path:LINE` are ignored, so a stray
    ///   call-wide anchor cannot silently aim an unrelated block.
    pub fn from_wire(raw: &Value) -> Result<Self, String> {
        let obj = match raw.as_object() {
            Some(o) => o,
            None => return Err(Self::invalid(raw, "arguments must be a JSON object")),
        };
        let raw_path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // Peel a `file:LINE` suffix off the `path` argument; the path part is
        // what resolves to a file.
        let (json_path, path_anchor): (Option<String>, Option<u64>) = match raw_path {
            Some(p) => {
                let (path, anchor) = blob::split_line_anchor(p);
                (Some(path), anchor)
            }
            None => (None, None),
        };
        let json_path = json_path.as_deref();
        let start_line = obj
            .get("startLine")
            .and_then(|v| v.as_u64())
            .map(EditStartLine);

        // Block form: `edits` is a string of Aider SEARCH/REPLACE blocks.
        // Anchors come solely from each block's `path:LINE` header.
        if let Some(text) = obj.get("edits").and_then(|v| v.as_str()) {
            let parsed = blob::parse_blocks(text)
                .map_err(|e| Self::invalid(raw, &format!("Invalid @@@OLD / @@@NEW block: {e}")))?;
            let (path, blocks) =
                blob::merge_path(json_path, parsed).map_err(|e| Self::invalid(raw, &e))?;
            let edits = blocks
                .into_iter()
                .map(|b| EditOp {
                    old_text: b.old_text,
                    new_text: b.new_text,
                    start_line: b.start_line.map(EditStartLine),
                })
                .collect();
            return Ok(EditArgs { path, edits });
        }

        // Legacy batch form: `edits` is an array.
        if let Some(arr) = obj.get("edits").and_then(|v| v.as_array()) {
            let path =
                json_path.ok_or_else(|| Self::invalid(raw, "missing required field 'path'"))?;
            let mut edits = Vec::with_capacity(arr.len());
            for e in arr {
                edits.push(Self::op_from_value(e).ok_or_else(|| {
                    Self::invalid(
                        raw,
                        "each `edits[]` entry needs string 'oldText' and 'newText'",
                    )
                })?);
            }
            return Ok(EditArgs {
                path: path.to_string(),
                edits,
            });
        }

        // Single form: top-level oldText/newText. Both `path:LINE` and
        // `startLine` may anchor it; if both are present they must agree.
        if let (Some(o), Some(n)) = (
            obj.get("oldText").and_then(|v| v.as_str()),
            obj.get("newText").and_then(|v| v.as_str()),
        ) {
            let path =
                json_path.ok_or_else(|| Self::invalid(raw, "missing required field 'path'"))?;
            let anchor = match (start_line, path_anchor) {
                (Some(s), Some(p)) if s.0 != p => {
                    return Err(Self::invalid(
                        raw,
                        &format!(
                            "the `path` argument anchors line {p} but `startLine` is {}; \
                             they must agree",
                            s.0
                        ),
                    ));
                }
                (Some(s), _) => Some(s),
                (None, Some(p)) => Some(EditStartLine(p)),
                (None, None) => None,
            };
            return Ok(EditArgs {
                path: path.to_string(),
                edits: vec![EditOp {
                    old_text: o.to_string(),
                    new_text: n.to_string(),
                    start_line: anchor,
                }],
            });
        }

        Err(Self::invalid(
            raw,
            "no recognizable edit arguments (expected `oldText`+`newText`, or `edits`)",
        ))
    }
}

impl<'de> Deserialize<'de> for EditArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        Self::from_wire(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn echoed_blob_keeps_markers_and_elides_bulk_content() {
        // A big, well-formed blob: the echo must keep the *structure* (the
        // marker lines) and elide the bulky middle, rather than chopping the
        // blob at an arbitrary byte offset.
        let old: String = (0..40).map(|i| format!("old line {i}\n")).collect();
        let new: String = (0..40).map(|i| format!("new line {i}\n")).collect();
        let blob = format!("@@@OLD\n{old}@@@NEW\n{new}@@@END");
        let v = json!({ "path": "p", "edits": blob });
        let out = truncate_wire_repr(&v);

        assert!(out.contains("@@@OLD"), "opening marker lost: {out}");
        assert!(out.contains("@@@NEW"), "divider lost: {out}");
        assert!(out.contains("@@@END"), "closing marker lost: {out}");
        assert!(out.contains("lines elided"), "no elision notice: {out}");
        assert!(!out.contains("old line 20"), "bulk not elided: {out}");
        assert!(out.len() <= MAX_ECHOED_ARGS, "echo exceeds cap: {out}");
    }

    #[test]
    fn echoed_blob_recognizes_near_miss_markers() {
        // Prefix matching keeps a hallucinated marker (trailing `>`) visible,
        // which is exactly the failure the echo should surface.
        let body: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let blob = format!("@@@OLD>\n{body}");
        let v = json!({ "path": "p", "edits": blob });
        let out = truncate_wire_repr(&v);
        assert!(out.contains("@@@OLD>"), "marker lost: {out}");
        assert!(out.contains("elided"), "no elision notice: {out}");
    }

    #[test]
    fn short_fields_are_not_elided() {
        // Under the cap: everything is echoed verbatim.
        let v = json!({ "path": "src/main.rs", "oldText": "a", "newText": "b" });
        assert_eq!(truncate_wire_repr(&v), v.to_string());
    }

    #[test]
    fn single_edit_shape_anchors_from_path_line() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs:12", "oldText": "a", "newText": "b"
        }))
        .unwrap();
        assert_eq!(a.path, "src/x.rs");
        assert_eq!(a.edits[0].start_line, Some(EditStartLine(12)));
    }

    #[test]
    fn single_edit_rejects_disagreeing_anchors() {
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs:12", "startLine": 20, "oldText": "a", "newText": "b"
        }))
        .unwrap_err();
        assert!(e.contains("they must agree"), "{e}");
    }

    #[test]
    fn block_form_takes_anchors_only_from_headers() {
        // A top-level startLine must not aim a header-less block.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs",
            "startLine": 20,
            "edits": "src/x.rs:7\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.edits[0].start_line, Some(EditStartLine(7)));
    }

    #[test]
    fn path_argument_anchor_is_peeled_in_block_form_too() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs:12",
            "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.path, "src/x.rs");
        assert_eq!(a.edits[0].start_line, None);
    }

    #[test]
    fn legacy_array_shape_still_parses() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "a", "newText": "b", "startLine": 3}]
        }))
        .unwrap();
        assert_eq!(a.edits[0].start_line, Some(EditStartLine(3)));
    }

    #[test]
    fn malformed_blob_reports_the_marker_problem() {
        // The blob never closes its OLD section, so the parser runs on and
        // trips over the later @@@END — and says so.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "edits": "@@@OLD\na\n@@@END"
        }))
        .unwrap_err();
        assert!(e.contains("@@@OLD section"), "{e}");
        assert!(e.contains("Actual parameters received:"), "{e}");
    }
}
