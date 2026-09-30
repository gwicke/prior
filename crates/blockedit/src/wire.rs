//! Wire-format decoding for the `edit` tool: the raw `arguments` JSON into a
//! resolved path plus a batch of [`EditOp`]s.
//!
//! # The error text is model-facing
//!
//! The self-correcting error built by [`EditArgs::invalid`] is the *only* signal
//! the model gets about a malformed block, and it is passed through verbatim —
//! rig's arg-parse path does not redact it, and the agent's result hook
//! forwards it unchanged. So this is copy written for the model, not for a
//! human log: state the one problem, show the one correct shape, echo the
//! offending structure. Anything decorative (the tool name, an apology about
//! sed, three near-identical JSON examples) costs tokens and buries the fix.
//!
//! The advertised shape is a **flat bag of optional fields** (see the tool's
//! `parameters()`), so variant selection happens here, on the raw JSON, and no
//! `anyOf` union is ever advertised. Accepted shapes — and only these two:
//!
//! - single edit: `{path, oldText, newText, startLine?}` — `startLine` is the
//!   anchor.
//! - block edit:  `{path?, blocks: "<@@@OLD/@@@NEW blocks>"}` — anchors come
//!   from each block's `path:LINE` header; a top-level `startLine` is ignored
//!   in this shape.
//!
//! # The `edits` array is not a wire shape
//!
//! `{path, edits: [{oldText, newText}]}` was accepted here for pi parity, and
//! is gone: the whole reason this module exists is that weak tool-calling
//! harnesses mangle nested arrays — most often by *stringifying* them, which
//! turns a batch into a JSON string and loses the edit entirely. `edits` is now
//! storage-only, where nothing has to encode it for a provider: pi's canonical
//! array, written to and read back from the session file. A model that still
//! sends one gets a named correction rather than a generic "no recognizable
//! edit arguments", because pi-shaped training makes that a likely slip.

use serde::Deserialize;
use serde_json::Value;

use crate::blob::{self, BLOCKS_FIELD, LEGACY_BLOCKS_FIELD};
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

/// The accepted shape, rendered the way the model should write it: line-based
/// block syntax, one line per marker, with the anchor shown. A single anchored
/// block covers every case the tool takes (the header is optional when `path` is
/// given as an argument), and line-based form is far easier to get right than a
/// JSON string full of `\n` escapes. Kept short on purpose — this is the
/// model-facing correction, and a wall of examples buries the one thing that
/// matters (which marker is malformed).
const EDIT_ARGS_EXAMPLE: &str = "some/path:1234\n@@@OLD\nold text\n@@@NEW\nnew text\n@@@END";

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

/// Render the `edits` blob the way the model should have written it: real
/// newlines, one marker per line, and every content line collapsed to a bare
/// `…`.
///
/// The point is comparability. `Actual:` and `Expected:` are then the same *kind*
/// of thing — two line-based blocks stacked vertically — so the model's eye goes
/// straight to the structural difference (a marker that is not alone on its
/// line) instead of mentally un-escaping a JSON string full of `\n` to find it.
/// Only headers (`path:LINE`) and markers are kept verbatim; *every* other
/// non-empty run of content collapses to a single `…`. The bad marker — the one
/// thing the model must see — is a marker, so it always survives; the file text
/// around it is the part that is noise.
fn render_blob_structure(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run = 0usize;
    for line in text.lines() {
        let t = line.trim();
        let keep = is_structural_line(line) || is_path_header(line);
        if keep {
            if run > 0 {
                out.push(elision_notice(run));
                run = 0;
            }
            out.push(t.to_string());
        } else if !t.is_empty() {
            run += 1;
        }
    }
    if run > 0 {
        out.push(elision_notice(run));
    }
    out.join("\n")
}

/// Whether a line is a block header (`some/path.rs`, `some/path.rs:123`), kept
/// in the skeleton so the model can see which file and anchor it aimed at.
///
/// Deliberately stricter than [`blob::header_candidate`], which accepts *any*
/// whitespace-free token — the real parser only *considers* a token a header at
/// particular positions, but here we are scanning every line, so a permissive
/// test would present ordinary content words (`return`, `more`) and braces as
/// if they were headers. Requiring an extension or a directory separator is
/// enough to recognize the header and nothing else.
fn is_path_header(line: &str) -> bool {
    blob::header_candidate(line).is_some_and(|h| h.contains('.') || h.contains('/'))
}

/// The `Actual:` echo: the offending `edits` blob rendered line-based (see
/// [`render_blob_structure`]) so it is directly comparable to the `Expected:`
/// block above it. Any other shape (the single-edit fields, a non-object) is
/// short, so it is shown as-is via [`truncate_wire_repr`].
fn actual_summary(raw: &Value) -> String {
    if let Some(text) = blob::blob_text(raw)
        .filter(|_| raw.get("oldText").is_none() && raw.get("newText").is_none())
    {
        return char_truncate(&render_blob_structure(text), MAX_ECHOED_ARGS);
    }
    truncate_wire_repr(raw)
}

impl EditArgs {
    /// Build the self-correcting error: what went wrong, the one accepted shape
    /// (line-based block syntax), and the structure actually received.
    ///
    /// Deliberately terse. This string is the *model's* correction, and every
    /// token spent on ceremony — restating the tool name, listing the
    /// single-edit JSON shape, apologizing about sed — competes with the one
    /// fact the model must act on (which marker is malformed). The model-facing
    /// text is a raw, unredacted copy of this (see the note in the module
    /// docs); keep it free of anything the model does not need to self-correct.
    fn invalid(raw: &Value, why: &str) -> String {
        format!(
            "{why}\n\nExpected:\n{EDIT_ARGS_EXAMPLE}\n\nActual:\n{}",
            actual_summary(raw),
        )
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

        // Block form: `blocks` is a string of @@@OLD/@@@NEW blocks.
        // Anchors come solely from each block's `path:LINE` header.
        if let Some(text) = blob::blob_text(raw) {
            let parsed = blob::parse_blocks(text)
                .map_err(|e| Self::invalid(raw, &format!("Invalid block: {e}")))?;
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

        // `edits` as an array is the canonical *storage* form, not a wire one.
        // A model that sends it (pi-shaped training makes that likely) gets a
        // correction naming the field it should use instead of a dead end.
        if obj.get("edits").map(Value::is_array).unwrap_or(false) {
            return Err(Self::invalid(
                raw,
                "`edits` is a list; it is not a form this tool accepts. Use `blocks` \
                 (a block string) or the single-edit fields `oldText` + `newText`",
            ));
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

        // A blob field that is neither a string nor an array is a *specific*
        // fault — a client that serialized the call wrongly — and it is worth
        // more than the generic absence below, which is all we can say when we
        // genuinely do not recognize the shape. Seen in the wild: a harness that
        // wrapped the whole argument object into one field, so the blob arrived
        // as `{"edits": {"path": "<blob>"}}`. Without this, that caller gets
        // "no recognizable edit arguments" and no idea which of the three
        // accepted shapes it failed to produce.
        for field in [BLOCKS_FIELD, LEGACY_BLOCKS_FIELD] {
            if let Some(v) = obj.get(field) {
                if !v.is_string() && !v.is_array() {
                    return Err(Self::invalid(
                        raw,
                        &format!(
                            "`{field}` is {}; it must be a string of `@@@OLD`/`@@@NEW`/`@@@END` \
                             blocks, or the canonical `edits` array",
                            match v {
                                Value::Object(_) => "an object",
                                Value::Array(_) => "a list",
                                _ => "a non-string value",
                            }
                        ),
                    ));
                }
            }
        }

        Err(Self::invalid(
            raw,
            "no recognizable edit arguments (expected `oldText`+`newText`, or `blocks`)",
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
        let v = json!({ "path": "p", "blocks": blob });
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
        let v = json!({ "path": "p", "blocks": blob });
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
            "blocks": "src/x.rs:7\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.edits[0].start_line, Some(EditStartLine(7)));
    }

    #[test]
    fn path_argument_anchor_is_peeled_in_block_form_too() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs:12",
            "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.path, "src/x.rs");
        assert_eq!(a.edits[0].start_line, None);
    }

    /// The shape this tool advertised for its whole life, still arriving from a
    /// harness or a model that has not seen the rename. It must keep working:
    /// `blocks` is the name we *ask* for, not a name we *require*.
    #[test]
    fn a_legacy_edits_string_is_still_accepted() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.path, "src/x.rs");
        assert_eq!(a.edits.len(), 1);
        assert_eq!(a.edits[0].old_text, "a");
        assert_eq!(a.edits[0].new_text, "b");
    }

    /// `blocks` wins when both are present, and a malformed `blocks` must not
    /// silently fall through to a healthy `edits` — that would apply an edit the
    /// model did not ask for.
    #[test]
    fn blocks_wins_over_a_legacy_edits_string() {
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs",
            "blocks": "@@@OLD\nfrom-blocks\n@@@NEW\nB\n@@@END",
            "edits":  "@@@OLD\nfrom-edits\n@@@NEW\nE\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.edits[0].old_text, "from-blocks");

        // ...and a malformed `blocks` is an error, not a fallback.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs",
            "blocks": "@@@OLD\nno end",
            "edits":  "@@@OLD\nfrom-edits\n@@@NEW\nE\n@@@END"
        }))
        .unwrap_err();
        assert!(e.starts_with("Invalid block:"), "{e}");
    }

    #[test]
    fn the_edits_array_is_rejected_with_a_named_correction() {
        // The array is pi's storage form, not a wire one. A model that sends it
        // must be told which field to use, not left at "unrecognized args".
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "a", "newText": "b", "startLine": 3}]
        }))
        .unwrap_err();
        assert!(e.contains("`edits` is a list"), "{e}");
        assert!(e.contains("`blocks`"), "{e}");
        // The self-correcting envelope is intact around the correction.
        assert!(e.contains("Expected:"), "{e}");
        assert!(e.contains("Actual:"), "{e}");
    }

    #[test]
    fn the_two_field_names_are_not_interchangeable() {
        // `blocks` is the blob and is not a list; `edits` is the canonical list
        // and is not a blob. Neither reading may be reached through the other's
        // field, or the rename has bought nothing.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.path, "src/x.rs");
        assert_eq!(a.edits.len(), 1);

        // `blocks` as an array is not the block form, and not the single-edit
        // form either, so it is simply unrecognised.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "blocks": [{"oldText": "a", "newText": "b"}]
        }))
        .unwrap_err();
        assert!(e.contains("no recognizable edit arguments"), "{e}");
    }

    /// The malformation a real client produced: the whole argument object
    /// wrapped into the blob field, so the blob arrived as an object rather than
    /// a string. Refused exactly as before — nothing is accepted that was not
    /// already — but the error now names the fault instead of reporting an
    /// absence the caller cannot act on.
    #[test]
    fn a_blob_field_holding_an_object_is_diagnosed_rather_than_ignored() {
        let e = EditArgs::from_wire(&json!({
            "edits": { "path": "@@@OLD\na\n@@@NEW\nb\n@@@END" },
            "path": "src/x.rs"
        }))
        .unwrap_err();
        assert!(e.contains("is an object"), "{e}");
        assert!(
            e.contains(BLOCKS_FIELD) || e.contains(LEGACY_BLOCKS_FIELD),
            "{e}"
        );
        // Still a self-correcting envelope, and still not a batch failure.
        assert!(e.contains("Expected:"), "{e}");

        // The same for the current name, and for a scalar.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "blocks": 42
        }))
        .unwrap_err();
        assert!(e.contains("non-string value"), "{e}");

        // A list is the canonical form and keeps its own, different correction.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]
        }))
        .unwrap_err();
        assert!(e.contains("`edits` is a list"), "{e}");

        // And the object case does not swallow a *valid* single edit.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "oldText": "a", "newText": "b"
        }))
        .unwrap();
        assert_eq!(a.edits.len(), 1);
    }

    /// Every shape this tool has *ever* accepted on the wire, and what each one
    /// does now.
    ///
    /// This test exists because the rename broke one of them silently. Three
    /// shapes were accepted before it, and the suite passed with two of the
    /// three gone: the array went on purpose, and the blob string went by
    /// accident — the tests did not miss that, they *specified* it
    /// (`a_blob_under_the_old_field_name_is_inert` asserted that the break was
    /// correct). Nothing then asked the only question a rename has to ask:
    /// *did every caller I never migrated stop working?*
    ///
    /// So the contract is a table here rather than something re-derived per
    /// change. The rule is narrow and worth stating: a field name this tool
    /// has published is either still **accepted**, or **refused by name** —
    /// never silently unrecognised. The array is the only shape allowed to be
    /// removed, and it paid for that with a correction naming the field to use.
    #[test]
    fn every_wire_shape_this_tool_has_ever_accepted_is_accounted_for() {
        // (1) `edits` as a blob string — the name from before the rename.
        // Retired, but still working: `blocks` is what we ask for, not what we
        // require.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .expect("a pre-rename blob must still be accepted");
        assert_eq!(a.edits.len(), 1);

        // (2) `edits` as an array — removed on purpose, refused by name.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]
        }))
        .unwrap_err();
        assert!(e.contains("`blocks`"), "a removal must name the field: {e}");

        // (3) the single-edit scalars — never touched.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "oldText": "a", "newText": "b"
        }))
        .unwrap();
        assert_eq!(a.edits.len(), 1);

        // (4) the current name.
        let a = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"
        }))
        .unwrap();
        assert_eq!(a.edits.len(), 1);
    }

    #[test]
    fn an_empty_edits_array_is_rejected_too() {
        // Belt and braces: a mangled array must never produce zero EditOps and
        // a vacuous "success".
        let e = EditArgs::from_wire(&json!({ "path": "src/x.rs", "edits": [] })).unwrap_err();
        assert!(e.contains("`blocks`"), "{e}");
    }

    #[test]
    fn malformed_blob_reports_the_marker_problem() {
        // The blob never closes its OLD section, so the parser runs on and
        // trips over the later @@@END — and says so.
        let e = EditArgs::from_wire(&json!({
            "path": "src/x.rs", "blocks": "@@@OLD\na\n@@@END"
        }))
        .unwrap_err();
        assert!(e.contains("@@@OLD section"), "{e}");
        assert!(e.contains("Actual:"), "{e}");
    }

    #[test]
    fn model_error_is_terse_and_line_based() {
        // The message below is what the *model* reads. It must lead with the
        // one problem, show the block format line-based, and show the offending
        // blob the same way — no JSON string full of `\n` escapes, no ceremony.
        let e = EditArgs::from_wire(&json!({
            "blocks": "src/x.rs\n@@@OLD\nold\n@@@NEWEND@@@OLD2@@@NEW2"
        }))
        .unwrap_err();
        assert!(e.starts_with("Invalid block:"), "{e}");
        // The bad marker is quoted verbatim and called out.
        assert!(e.contains("@@@NEWEND@@@OLD2@@@NEW2"), "{e}");
        // Line-based Expected, one marker per line.
        assert!(
            e.contains("Expected:\nsome/path:1234\n@@@OLD\nold text\n@@@NEW\nnew text\n@@@END"),
            "{e}"
        );
        // Line-based Actual: markers on their own lines, content elided.
        assert!(e.contains("Actual:\nsrc/x.rs\n@@@OLD\n"), "{e}");
        // No escaped newlines, no JSON blob examples, no sed/python lecture.
        // (The guidance prose may name the single-edit fields, so target the
        // JSON-example shape specifically, not the word "oldText".)
        assert!(!e.contains("\\n"), "no escaped newlines: {e}");
        assert!(!e.contains("\"path\""), "no JSON example: {e}");
        assert!(!e.contains("Expected one of these"), "no format list: {e}");
        assert!(!e.contains("sed or python"), "no sed/python lecture: {e}");
        // Bounded: a huge blob cannot blow up the message.
        let big = format!("@@@OLD\n{}\n@@@NEWEND", "x".repeat(50_000));
        let e2 = EditArgs::from_wire(&json!({ "blocks": big })).unwrap_err();
        assert!(e2.len() < 4 * MAX_ECHOED_ARGS, "len {}", e2.len());
    }
}
