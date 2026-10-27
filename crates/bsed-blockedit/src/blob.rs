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
//!   prior). Accepted on ingress / replay only.
//!
//! A block's markers may arrive in either order — the canonical `@@@OLD` …
//! `@@@NEW` … `@@@END`, or the switched `@@@NEW` … `@@@OLD` … `@@@END` (new
//! text first). The opening marker fixes the region roles, so there is no
//! guessing; everything this module *emits* (rendering, canonicalization) is
//! the canonical order.
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
/// read here — that is pi's canonical form, never a blob — and neither is a
/// string that is really a stringified array: both belong to
/// [`crate::wire::EditArgs::from_wire`], which is where the two are told apart.
pub fn blob_text(args: &Value) -> Option<&str> {
    let obj = args.as_object()?;
    obj.get(BLOCKS_FIELD)
        .and_then(Value::as_str)
        .or_else(|| obj.get(LEGACY_BLOCKS_FIELD).and_then(Value::as_str))
}

/// The OLD-region marker (`@@@OLD`): opens a block in the canonical order, and
/// closes the second region of a new-first one.
pub const SEARCH_MARKER: &str = "@@@OLD";
/// The NEW-region marker (`@@@NEW`): the divider of a canonical block, and the
/// opener of a new-first one.
pub const DIVIDER: &str = "@@@NEW";
/// Closing marker of a block (`@@@END`), in either order.
pub const REPLACE_MARKER: &str = "@@@END";

/// The marker order a block was written in.
///
/// The canonical order opens with [`SEARCH_MARKER`]; the switched order opens
/// with [`DIVIDER`] and lists the new text first. Both parse to the same
/// `(old_text, new_text)` pair — the order is kept only so a failure can name
/// the block's *actual* opening marker instead of assuming the canonical one.
/// Not persisted; rendering always re-emits the canonical order.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum BlockOrder {
    /// `@@@OLD` … old text … `@@@NEW` … new text … `@@@END`
    OldFirst,
    /// `@@@NEW` … new text … `@@@OLD` … old text … `@@@END`
    NewFirst,
}

impl BlockOrder {
    /// The marker that opens a block in this order — also the line a
    /// `path:LINE` header must sit above.
    pub fn opener(self) -> &'static str {
        match self {
            BlockOrder::OldFirst => SEARCH_MARKER,
            BlockOrder::NewFirst => DIVIDER,
        }
    }

    /// `(name of the first region, the marker that closes it)`.
    fn first_region(self) -> (&'static str, &'static str) {
        match self {
            BlockOrder::OldFirst => (SEARCH_MARKER, DIVIDER),
            BlockOrder::NewFirst => (DIVIDER, SEARCH_MARKER),
        }
    }

    /// The name of the second region (always closed by [`REPLACE_MARKER`]).
    fn second_region(self) -> &'static str {
        match self {
            BlockOrder::OldFirst => DIVIDER,
            BlockOrder::NewFirst => SEARCH_MARKER,
        }
    }

    /// Map the two physical regions onto `(old, new)` by role.
    fn split_regions<'a>(
        self,
        first: Vec<&'a str>,
        second: Vec<&'a str>,
    ) -> (Vec<&'a str>, Vec<&'a str>) {
        match self {
            BlockOrder::OldFirst => (first, second),
            BlockOrder::NewFirst => (second, first),
        }
    }
}

/// The article a marker name takes in a message: `an @@@OLD`, `a @@@NEW`.
fn article(marker: &str) -> &'static str {
    if marker == SEARCH_MARKER {
        "an"
    } else {
        "a"
    }
}

/// One parsed `@@@OLD`/`@@@NEW`/`@@@END` block, in either marker order.
#[derive(Debug, PartialEq, Clone)]
pub struct ParsedEdit {
    pub old_text: String,
    pub new_text: String,
    /// 1-based line the block's header anchored to (`path:LINE`), if any. Used
    /// as a nearest-match hint by the backend, and persisted/rendered once the
    /// edit has resolved so replay stays both compact and unambiguous.
    pub start_line: Option<u64>,
    /// The file this block's header named, if it had one.
    ///
    /// Needed because a blob may legitimately span files — a recursive `bsed`
    /// preview emits one header per file — and the single-file agreement check
    /// in [`parse_blocks`] only *validates* headers, it does not keep them. The
    /// anchor is split off `header`; this is the path part. Not persisted.
    pub header: Option<String>,
    /// The line directly above this block's opening marker that was **not**
    /// taken as a header, when there was one and it was not empty.
    ///
    /// A header must be a single whitespace-free token, so a model that wrote
    /// prose, or a real path containing a space, silently produces a
    /// header-less block. That is unreportable from `header` alone (`None`
    /// either way), and reporting a bare absence is the failure P1 was written
    /// against — the caller has no way to learn *what it wrote* in the slot the
    /// error is about. Kept so the multi-file policy can quote it back. Not
    /// persisted.
    pub rejected_header: Option<String>,
    /// The block's header line **verbatim**, captured before
    /// [`split_line_anchor`] peeled any `:LINE`/`:LINE:COL` suffix off into
    /// `start_line`.
    ///
    /// Kept so a failure can quote the model back the exact string it wrote
    /// (`a/b.rs:100:5`, column included) rather than a reconstruction that
    /// would have silently dropped the column. `header` cannot serve this
    /// purpose: it holds the path part only. `None` when the block carried no
    /// header. Not persisted.
    pub raw_header: Option<String>,
    /// The marker order this block was written in. Kept so a failure can name
    /// the block's real opening marker; rendering always re-emits the
    /// canonical order. Not persisted.
    pub order: BlockOrder,
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
    [SEARCH_MARKER, DIVIDER, REPLACE_MARKER]
        .into_iter()
        .find(|&m| line != m && line.starts_with(m))
        .map(|v| v as _)
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
/// Recognizes blocks whose markers each occupy their own (trimmed) line, in
/// either marker order: the canonical `@@@OLD` … `@@@NEW` … `@@@END`, and the
/// switched `@@@NEW` … `@@@OLD` … `@@@END` (new text first). The opening
/// marker fixes which region is which — there is no guessing. The line
/// immediately preceding the *opening* marker is treated as an optional
/// filename header when it is a single whitespace-free token; a trailing
/// `:LINE` on that header supplies the block's line anchor.
pub fn parse_blocks(text: &str) -> Result<ParsedBlocks, ParseError> {
    let lines = strip_fence(text);
    let mut edits: Vec<ParsedEdit> = Vec::new();
    let mut distinct_headers: Vec<String> = Vec::new();
    let mut saw_any_header = false;

    let mut i = 0usize;
    // Start of the text not yet consumed by a block. A new-first block's
    // failures scan back to here for a near-miss marker: a garbled `@@@OLD>`
    // opener is exactly what makes the parser fall through to a later
    // `@@@NEW` and open a phantom new-first block, so blaming only the
    // region would name a consequence rather than the mistake.
    let mut scan_from = 0usize;
    while i < lines.len() {
        let Some(opener) = marker_of(&lines[i]) else {
            i += 1;
            continue;
        };
        let order = match opener {
            SEARCH_MARKER => BlockOrder::OldFirst,
            DIVIDER => BlockOrder::NewFirst,
            // `@@@END` closes a block; it never opens one.
            _ => {
                i += 1;
                continue;
            }
        };
        let (first_name, first_expected) = order.first_region();
        let second_name = order.second_region();
        // Where the near-miss scan for this block's first-region failures
        // starts (see `scan_from`): back to the previous block for a
        // new-first one, the block's own first region otherwise.
        let near_from = if order == BlockOrder::NewFirst {
            scan_from
        } else {
            i + 1
        };

        // Optional header = the immediately preceding line, if it is a path.
        // A `path:LINE` suffix supplies this block's anchor; the path part is
        // what participates in the (per-file) agreement check.
        let mut header_anchor: Option<u64> = None;
        let mut header_path: Option<String> = None;
        // The line this block's header would have been, when there was one and
        // it was not a usable header. Kept only to be quoted back in an error:
        // prose and a path-with-a-space are indistinguishable from `None`.
        let mut rejected_header: Option<String> = None;
        let mut raw_header: Option<String> = None;
        if i > 0 {
            if let Some(h) = header_candidate(&lines[i - 1]) {
                let (path_part, anchor) = split_line_anchor(&h);
                saw_any_header = true;
                header_anchor = anchor;
                // Verbatim, before peeling — `split_line_anchor` above already
                // discarded a `:COL` suffix, which a failure report must quote
                // back exactly.
                raw_header = Some(h.clone());
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

        // Collect the first region's content up to its closing marker (the
        // divider in the canonical order, `@@@OLD` in the switched one).
        // Fail fast if any unexpected delimiter is encountered.
        let mut j = i + 1;
        let mut first_lines: Vec<&str> = Vec::new();
        while j < lines.len() {
            if let Some(m) = marker_of(&lines[j]) {
                if m == first_expected {
                    break; // found the expected marker
                }
                // Any other delimiter is invalid here (e.g. `@@@END` before
                // the second region, or a repeated opener). Prefer a garbled
                // marker from the region just scanned — that is what cost us
                // the structure.
                if let Some((mk, bad)) = near_miss_in(&lines[near_from..j]) {
                    return Err(malformed_marker_error(mk, bad));
                }
                return Err(ParseError(format!(
                    "unexpected delimiter `{m}` in {first_name} section; \
                     expected `{first_expected}`"
                )));
            }
            first_lines.push(&lines[j]);
            j += 1;
        }
        if j >= lines.len() {
            // The block is being rejected either way, so naming a near-miss in
            // the region we just scanned costs nothing and beats a bare
            // "missing {first_expected}" — there is no content here to protect.
            if let Some((mk, bad)) = near_miss_in(&lines[near_from..j]) {
                return Err(malformed_marker_error(mk, bad));
            }
            return Err(ParseError(format!(
                "an edit block is missing its `{first_expected}` section"
            )));
        }

        // Collect the second region's content up to the closing marker.
        // Fail fast if any unexpected delimiter is encountered.
        let mut k = j + 1;
        let mut second_lines: Vec<&str> = Vec::new();
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
                    "unexpected delimiter `{m}` in {second_name} section; \
                     expected `{REPLACE_MARKER}`"
                )));
            }
            second_lines.push(&lines[k]);
            k += 1;
        }
        if k >= lines.len() {
            // As above: the block is already rejected, so a near-miss in the
            // second region is worth naming.
            if let Some((mk, bad)) = near_miss_in(&lines[j + 1..k]) {
                return Err(malformed_marker_error(mk, bad));
            }
            return Err(ParseError(format!(
                "{} `{second_name}` section is missing its `{REPLACE_MARKER}` marker",
                article(second_name)
            )));
        }

        // Map the two physical regions onto (old, new) by the block's order.
        let (old_lines, new_lines) = order.split_regions(first_lines, second_lines);
        let old_text = old_lines.join("\n");
        if old_text.is_empty() {
            // The empty-old rule is about the OLD *role*, not about position:
            // an insert with nothing to search for cannot be located, in
            // either order.
            return Err(ParseError(
                "an edit block has an empty `@@@OLD` section; the OLD text must be non-empty"
                    .to_string(),
            ));
        }
        let new_text = new_lines.join("\n");
        edits.push(ParsedEdit {
            old_text,
            new_text,
            start_line: header_anchor,
            header: header_path,
            rejected_header,
            raw_header,
            order,
        });

        i = k + 1;
        scan_from = k + 1;
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
        return Err(ParseError(
            "no `@@@OLD` / `@@@NEW` / `@@@END` blocks found".to_string(),
        ));
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

/// Render block pairs as a blob, with the path repeated as a header line above
/// **every** block (Aider-style). A block's `start_line`, when present, is
/// rendered as `path:LINE`.
///
/// Anchors are part of the round trip: if you change this, also check
/// `parse_blocks`, [`edits_to_canonical_value`], [`to_parsed`],
/// [`array_to_blob_value`] and [`canonicalize_edit_value`] — all of them must
/// preserve `startLine`/`path:LINE` together.
///
/// The path is repeated because the tool *requires* a header on every block —
/// the model-facing form has to be a form the tool accepts, or the agent is
/// being taught an invalid call. Writing it once above the first block was the
/// old, laxer layout; it is no longer expressible, and a repeated header costs
/// a few tokens per block against the clarity of one uniform rule.
pub fn render_blob(path: &str, edits: &[ParsedEdit]) -> String {
    let mut out = String::new();
    for (i, e) in edits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !path.is_empty() {
            out.push_str(path);
            if let Some(line) = e.start_line {
                out.push(':');
                out.push_str(&line.to_string());
            }
            out.push('\n');
        }
        out.push_str(SEARCH_MARKER);
        out.push('\n');
        out.push_str(&e.old_text);
        if !e.old_text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(DIVIDER);
        out.push('\n');
        out.push_str(&e.new_text);
        if !e.new_text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(REPLACE_MARKER);
        out.push('\n');
    }
    // Drop the single trailing newline so the blob is tidy and round-trips.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The execution form of one file's blocks: [`EditOp`]s computed against the
/// original file, anchors carried through as the matching hint.
///
/// The single conversion every consumer of a parsed blob needs. It lives here
/// rather than in each caller because there were already two copies (this
/// crate's `from_wire`, and a private one in `bsed`), which drift the moment a
/// field is added to either type.
pub fn to_ops(edits: &[ParsedEdit]) -> Vec<crate::types::EditOp> {
    edits
        .iter()
        .map(|e| crate::types::EditOp {
            old_text: e.old_text.clone(),
            new_text: e.new_text.clone(),
            start_line: e.start_line.map(crate::types::EditStartLine),
            origin: e.raw_header.clone(),
        })
        .collect()
}

/// The inverse of [`to_ops`]: the render form of an execution-form batch.
///
/// `origin` has no `ParsedEdit` field of its own — it is the header the model
/// spelled, which only `render_blob` reproduces through `path:LINE` — so it is
/// dropped here. Nothing that renders a batch needs it.
pub fn to_parsed(edits: &[crate::types::EditOp]) -> Vec<ParsedEdit> {
    edits
        .iter()
        .map(|e| ParsedEdit {
            old_text: e.old_text.clone(),
            new_text: e.new_text.clone(),
            start_line: e.start_line.map(|s| s.0),
            header: None,
            rejected_header: None,
            raw_header: None,
            // Rendering is canonical regardless of the order the call arrived
            // in, so the re-rendered block is always old-first.
            order: BlockOrder::OldFirst,
        })
        .collect()
}

/// The canonical-array entry for one edit.
///
/// `startLine` is included when known. pi's schema tolerates the extra field
/// (it does not set `additionalProperties: false`) and ignores it during
/// application, so this is both safe and what lets a later turn re-render the
/// same compact anchored block after the file has changed.
fn edit_to_value(edit: &crate::types::EditOp) -> Value {
    match edit.start_line {
        Some(line) => json!({
            "oldText": edit.old_text,
            "newText": edit.new_text,
            "startLine": line.0,
        }),
        None => json!({ "oldText": edit.old_text, "newText": edit.new_text }),
    }
}

/// The canonical array form of one file's edits: `{path, edits:[…]}`.
///
/// **The** writer of the canonical array, for both ingress directions: the
/// producing function (`prepare_edit_call`) calls it with the minimized,
/// relativized [`EditOp`]s it is about to persist **and** display, and
/// [`canonicalize_edit_value`] calls it after parsing a blob. One writer means
/// the stored array and the rendered one cannot disagree about `startLine`.
///
/// Keep the `startLine` round trip in sync with `render_blob`,
/// [`array_to_blob_value`], [`canonicalize_edit_value`], and `wire::from_wire`.
pub fn edits_to_canonical_value(path: &str, edits: &[crate::types::EditOp]) -> Value {
    json!({
        "path": path,
        "edits": Value::Array(edits.iter().map(edit_to_value).collect()),
    })
}

/// Canonicalize a raw edit-call `arguments` value to the canonical array form,
/// **best-effort**. Handles:
///
/// - `blocks` is a **string** (blob) → parse + merge → array;
/// - `edits` is already an **array** → unchanged (the canonical form itself);
/// - top-level `oldText`/`newText` → single-entry array;
/// - legacy `old`/`new` → single-entry array.
///
/// The array branches are **storage-side only**. The array is not advertised
/// on the wire — a model is pointed at `blocks` (a string) or the single-edit
/// scalars — so reaching those branches here means we are normalizing a value
/// we read back off disk, or one a model sent by reflex. The wire *accepts*
/// the array (see [`crate::wire`]) but never asks for it, which is why
/// canonicalization still runs in the other direction: a stored array is
/// rewritten into the advertised block form before the model sees it.
///
/// On a blob that does not satisfy the block policy (unparseable, a block that
/// names no file, or a multi-file blob) the value is returned unchanged so
/// replay degrades gracefully rather than erroring.
pub fn canonicalize_edit_value(args: &Value) -> Value {
    let Some(obj) = args.as_object() else {
        return args.clone();
    };

    // Blob string (the model-facing field).
    if let Some(text) = blob_text(args) {
        // Same policy as the wire (`from_wire`): every block names its own
        // file, and the `path` argument is not consulted. Canonicalization must
        // not be laxer than the tool, or it would resolve blobs the tool
        // rejects — and a persisted call has to mean what it meant on ingress.
        if let Ok(parsed) = parse_blocks(text) {
            if let Ok(files) = split_by_file(parsed) {
                // The canonical form is one path per call, so only a
                // single-file blob canonicalizes; a multi-file one is stored
                // raw (degrades gracefully) until the storage axis splits it.
                if let [one] = files.as_slice() {
                    return edits_to_canonical_value(&one.path, &to_ops(&one.edits));
                }
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
        let edit = match obj.get("startLine").and_then(Value::as_u64) {
            Some(line) => json!({ "oldText": o, "newText": n, "startLine": line }),
            None => json!({ "oldText": o, "newText": n }),
        };
        return json!({ "path": path, "edits": [edit] });
    }

    // Legacy `old`/`new`.
    if let Some(o) = obj.get("old") {
        let n = obj.get("new").cloned().unwrap_or(Value::Null);
        let path = obj.get("path").cloned().unwrap_or(Value::Null);
        let edit = match obj.get("startLine").and_then(Value::as_u64) {
            Some(line) => json!({ "oldText": o, "newText": n, "startLine": line }),
            None => json!({ "oldText": o, "newText": n }),
        };
        return json!({ "path": path, "edits": [edit] });
    }

    args.clone()
}

/// Inverse of [`canonicalize_edit_value`] for the model-facing form: rewrite a
/// canonical-array edit call into the blob form, under [`BLOCKS_FIELD`].
/// **Idempotent** — a value that is not a canonical array is returned unchanged,
///
/// `startLine` must survive in both directions; if you touch that mapping, also
/// touch [`edits_to_canonical_value`], [`render_blob`], [`to_parsed`] and
/// [`canonicalize_edit_value`].
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
            start_line: e.get("startLine").and_then(Value::as_u64),
            header: None,
            rejected_header: None,
            raw_header: None,
            // The array is order-less; rendering is canonical anyway.
            order: BlockOrder::OldFirst,
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
            // The header token as the model spelled it. Equal to `path` here
            // because these fixtures carry no `:LINE` suffix; an anchored
            // fixture would need the suffix kept.
            raw_header: Some(path.to_string()),
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
            raw_header: None,
            order: BlockOrder::OldFirst,
        }
    }

    /// A NEW-first (switched) block as it comes back from parsing: the same
    /// pair as [`plain`], recorded in the switched order.
    fn switched(old: &str, new: &str) -> ParsedEdit {
        ParsedEdit {
            order: BlockOrder::NewFirst,
            ..plain(old, new)
        }
    }

    /// A NEW-first block that carried a header, like [`headed`].
    fn switched_headed(path: &str, old: &str, new: &str) -> ParsedEdit {
        ParsedEdit {
            header: Some(path.to_string()),
            raw_header: Some(path.to_string()),
            ..switched(old, new)
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
        // Two files -> two batches, and the `path` argument is not consulted.
        let files = split_by_file(p).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a.rs");
        assert_eq!(files[1].path, "b.rs");
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

    #[test]
    fn canonicalize_blob_string() {
        let v = json!({"path": "src/x.rs", "blocks": "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"});
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
        let v = json!({"path": "src/x.rs", "edits": "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"});
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
        // Every block carries its own header, because the tool requires one.
        assert_eq!(
            parsed.edits,
            vec![headed("src/x.rs", "a", "b"), headed("src/x.rs", "c", "d")]
        );
    }

    #[test]
    fn the_model_facing_form_satisfies_the_tools_own_policy() {
        // The agent teaches the blob form by rewriting stored args into it, so
        // what it teaches must be a form the tool accepts. Under the strict
        // policy that means a header on *every* block — writing the path once
        // above the first block would be teaching an invalid call.
        let v = json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "a", "newText": "b"}, {"oldText": "c", "newText": "d"}]
        });
        let blob = array_to_blob_value(&v);
        let text = blob.get(BLOCKS_FIELD).and_then(|x| x.as_str()).unwrap();
        // It passes the policy the wire applies...
        let files = split_by_file(parse_blocks(text).unwrap()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].edits.len(), 2);
        // ...and still canonicalizes to what it came from.
        assert_eq!(canonicalize_edit_value(&blob), v);
    }

    #[test]
    fn array_to_blob_is_idempotent_on_blob() {
        let v = json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        assert_eq!(array_to_blob_value(&v), v);
    }

    #[test]
    fn anchors_round_trip_through_render_and_canonicalize() {
        let v = json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "b", "newText": "B", "startLine": 12}]
        });
        let blob = array_to_blob_value(&v);
        assert_eq!(
            blob["blocks"].as_str().unwrap(),
            "src/x.rs:12\n@@@OLD\nb\n@@@NEW\nB\n@@@END"
        );
        assert_eq!(canonicalize_edit_value(&blob), v);
        assert_eq!(
            parse_blocks(blob["blocks"].as_str().unwrap())
                .unwrap()
                .edits[0]
                .start_line,
            Some(12)
        );
    }

    #[test]
    fn render_blob_emits_an_anchor_only_when_present() {
        let mut edit = plain("b", "B");
        edit.start_line = Some(12);
        let blob = render_blob("src/x.rs", &[edit]);
        assert!(blob.starts_with("src/x.rs:12\n@@@OLD\n"), "{blob}");
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

    // -- switched order: `@@@NEW` … `@@@OLD` … `@@@END` --------------------

    #[test]
    fn parses_a_new_first_block_to_the_same_pair() {
        let text = "src/foo.py\n@@@NEW\nnew text\n@@@OLD\nold text\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.path.as_deref(), Some("src/foo.py"));
        assert_eq!(
            p.edits,
            vec![switched_headed("src/foo.py", "old text", "new text")]
        );
        assert_eq!(p.edits[0].order, BlockOrder::NewFirst);
    }

    #[test]
    fn the_header_sits_above_the_opening_marker_in_a_switched_block() {
        // The header is the line above the *first* marker, whichever it is;
        // a `:LINE` anchor comes along the same way.
        let text = "a.rs:3\n@@@NEW\nnew\n@@@OLD\nold\n@@@END\n\n\
                    b.rs:9\n@@@OLD\nx\n@@@NEW\nX\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert!(p.multi_file);
        assert_eq!(p.edits[0].start_line, Some(3));
        assert_eq!(p.edits[1].start_line, Some(9));
    }

    #[test]
    fn canonical_and_switched_blocks_mix_in_one_blob() {
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\na.rs\n@@@NEW\nY\n@@@OLD\ny\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert!(!p.multi_file);
        assert_eq!(
            p.edits,
            vec![headed("a.rs", "x", "X"), switched_headed("a.rs", "y", "Y")]
        );
    }

    #[test]
    fn a_switched_delete_block_parses_with_empty_new_text() {
        // Empty NEW is a delete in either order — only empty OLD is refused.
        let text = "@@@NEW\n@@@OLD\nbye\n@@@END";
        let p = parse_blocks(text).unwrap();
        assert_eq!(p.edits, vec![switched("bye", "")]);
    }

    #[test]
    fn a_switched_block_with_empty_old_text_is_rejected() {
        // The empty-old rule is about the OLD *role*, not about position.
        let text = "@@@NEW\nhi\n@@@OLD\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(err.0.contains("empty `@@@OLD` section"), "{err}");
    }

    #[test]
    fn a_new_first_block_missing_its_old_section_names_the_expected_marker() {
        let text = "@@@NEW\nhi\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0
                .contains("unexpected delimiter `@@@END` in @@@NEW section; expected `@@@OLD`"),
            "{err}"
        );
    }

    #[test]
    fn a_new_first_block_missing_its_end_names_the_old_section() {
        let text = "@@@NEW\nhi\n@@@OLD\nold";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0
                .contains("an `@@@OLD` section is missing its `@@@END` marker"),
            "{err}"
        );
    }

    #[test]
    fn a_stray_old_inside_a_new_first_block_is_reported() {
        let text = "@@@NEW\nhi\n@@@OLD\n@@@OLD\nold\n@@@END";
        let err = parse_blocks(text).unwrap_err();
        assert!(
            err.0
                .contains("unexpected delimiter `@@@OLD` in @@@OLD section; expected `@@@END`"),
            "{err}"
        );
    }

    #[test]
    fn a_bare_end_marker_never_opens_a_block() {
        let err = parse_blocks("@@@END").unwrap_err();
        assert!(
            err.0
                .contains("no `@@@OLD` / `@@@NEW` / `@@@END` blocks found"),
            "{err}"
        );
    }

    #[test]
    fn canonicalize_a_switched_blob_maps_regions_by_role() {
        // Persistence is canonical, but it must read a switched block by role,
        // not by position: newText comes from the first region here.
        let v = json!({"path": "src/x.rs", "blocks": "src/x.rs\n@@@NEW\nb\n@@@OLD\na\n@@@END"});
        assert_eq!(
            canonicalize_edit_value(&v),
            json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );
    }

    #[test]
    fn a_switched_block_re_renders_in_the_canonical_order() {
        // Rendering is canonical-only, so a switched block survives a
        // parse → render → parse round trip as the same pair.
        let p = parse_blocks("src/x.rs\n@@@NEW\nb\n@@@OLD\na\n@@@END").unwrap();
        let blob = render_blob("src/x.rs", &p.edits);
        let again = parse_blocks(&blob).unwrap();
        assert_eq!(again.edits, vec![headed("src/x.rs", "a", "b")]);
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
                     above every block",
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

/// The path each block names, keyed for grouping, in first-seen order.
///
/// `split_by_file` is **the** path policy for every block blob, single-file or
/// not. There is deliberately one rule rather than two:
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
/// 4. There is no cap on how many files a blob may name.
///
///    There was one, at 32, on the reasoning that a single string argument
///    should not fan out into unbounded work. That conflated two different
///    limits: how much *work* the call does, and how much of the model's
///    context it consumes. The second is the one that matters and it is not
///    this function's to bound — the caller already had to write the blob, so
///    the blob's size is a fact about the request, not a surprise to defend
///    against. The first is the sandbox's: work inside it runs under the same
///    timeout and against the same bounds as `bash`, which is no more
///    restricted, and nothing here is more dangerous than a shell.
///
///    The cost is that an over-large blob now fails later rather than at
///    ingress: more files means more reads and writes, bounded by the timeout
///    and reported per file by `edit_files`, which is fault-tolerant per file.
///
/// There is no fallback, and there is no single-file exemption either: an
/// earlier design let `path` supply the file for a header-less *single-file*
/// blob, and keeping it would have meant a model holding two rules for one
/// thing, on a point where one costs nothing (a block already carries its
/// `path`). This is also why the function is not a drop-in for
/// [`group_by_file`], which is header-authoritative *with a fallback* and
/// keys on the raw string — see the hazards its own tests pin.
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
            _ => {
                return Err(no_file_error(
                    i + 1,
                    edit.rejected_header.as_deref(),
                    edit.order,
                ))
            }
        };
        // Key on the normalized form; report and store the first spelling.
        let key = normalized_key(&spelled);
        if !batches.contains_key(&key) {
            order.push(spelled);
        }
        batches.entry(key).or_default().push(edit);
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
fn no_file_error(idx: usize, rejected: Option<&str>, order: BlockOrder) -> String {
    // The header sits above the block's *opening* marker — `@@@OLD` in the
    // canonical order, `@@@NEW` in the switched one — so the message names
    // the marker this block actually opened with.
    let marker = order.opener();
    let shape = "\n  src/foo.rs:12\n  @@@OLD\n  ...\n  @@@NEW\n  ...\n  @@@END";
    let msg = match rejected {
        // Quoting the line back is the whole point: a bare "names no file"
        // tells the model to add a line it may already have added. The two
        // causes need different fixes, so they are named separately.
        Some(line) if line.split_whitespace().count() > 1 => format!(
            "block {idx} of a multi-file edit names no file: the line above its `{marker}` \
             marker is `{line}`, which is not a header. A header is one path token with no \
             spaces. If that is a real filename containing a space, this block form cannot \
             address it — edit that file with the single-edit fields (path + oldText + \
             newText) instead.{shape}"
        ),
        Some(line) => format!(
            "block {idx} of a multi-file edit names no file: the line above its `{marker}` \
             marker is `{line}`, which is not a path. Replace it with a path to the file this \
             block edits, like the other blocks.{shape}"
        ),
        None => format!(
            "block {idx} of a multi-file edit names no file. Put a filename line above its \
             `{marker}` marker, like the other blocks:{shape}"
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
        let err = no_file_error(3, None, BlockOrder::OldFirst);
        assert!(err.contains("block 3"), "{err}");
        assert!(!err.contains("`path` argument"), "{err}");
    }

    #[test]
    fn a_rejected_header_line_is_quoted_back() {
        // The model wrote *something* in the header slot. Saying only "names
        // no file" tells it to add a line it already added; quoting the line
        // is what makes the error actionable.
        let err = no_file_error(2, Some("Update the parser."), BlockOrder::OldFirst);
        assert!(err.contains("`Update the parser.`"), "{err}");
        // A single whitespace-free token is a *wrong path*, not a spaced one.
        let err = no_file_error(2, Some("src/typo.rs"), BlockOrder::OldFirst);
        assert!(err.contains("which is not a path"), "{err}");
        assert!(!err.contains("no spaces"), "{err}");
    }

    #[test]
    fn a_multi_word_line_is_reported_as_a_possible_spaced_path() {
        // Ambiguous by nature — prose and a spaced filename look the same —
        // so the message covers both and routes the unrepresentable case to
        // the form that can express it.
        let err = no_file_error(1, Some("my notes.rs:4"), BlockOrder::OldFirst);
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

    // -- rule 4: no fan-out cap -------------------------------------------

    #[test]
    fn a_blob_may_name_as_many_files_as_it_carries() {
        // The old cap was 32 and rejected the whole blob past it, on the theory
        // that a string argument should not fan out into unbounded work. The
        // caller wrote the blob, so its size is a fact about the request rather
        // than a surprise, and the sandbox bounds the work. See rule 4.
        let wide: Vec<(String, &str)> = (0..500).map(|i| (format!("f{i}.rs"), "x")).collect();
        let spec: Vec<(&str, &str)> = wide.iter().map(|(p, b)| (p.as_str(), *b)).collect();
        let g = split_by_file(blob(&spec)).unwrap();
        assert_eq!(g.len(), 500);
        // Still one block each: the file count was never what limited blocks.
        assert!(g.iter().all(|b| b.edits.len() == 1));
    }

    #[test]
    fn many_blocks_in_one_file_are_never_capped() {
        let spec: Vec<(&str, &str)> = (0..500).map(|_| ("a.rs", "x")).collect();
        let g = split_by_file(blob(&spec)).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].edits.len(), 500);
    }

    // -- the hazard this whole function exists to avoid -------------------

    #[test]
    fn a_header_less_block_never_becomes_a_second_file() {
        // `group_by_file` with a `path` fallback turns exactly this blob into
        // two batches. Here it is one error, and no file is implied.
        let text = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        assert!(split_by_file(parse(text)).is_err());
    }

    // -- switched order -----------------------------------------------------

    #[test]
    fn a_switched_block_groups_by_its_own_header() {
        // A `@@@NEW`-first block names its file above the `@@@NEW`, and the
        // splitter reads it exactly as it reads a canonical block's header.
        let text = "a.rs\n@@@NEW\nX\n@@@OLD\nx\n@@@END\n\nb.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let g = split_by_file(parse(text)).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].path, "a.rs");
        assert_eq!(g[0].edits[0].old_text, "x");
        assert_eq!(g[0].edits[0].new_text, "X");
        assert_eq!(g[1].path, "b.rs");
    }

    #[test]
    fn the_missing_file_error_names_a_switched_blocks_real_opener() {
        // The header sits above `@@@NEW` in a new-first block, so the error
        // must point there — naming `@@@OLD` would send the model to the
        // wrong line.
        let text = "a.rs\n@@@NEW\nX\n@@@OLD\nx\n@@@END\n\n\
                    @@@NEW\nY\n@@@OLD\ny\n@@@END";
        let err = split_by_file(parse(text)).unwrap_err();
        assert!(err.contains("block 2"), "{err}");
        assert!(err.contains("above its `@@@NEW` marker"), "{err}");
    }
}
