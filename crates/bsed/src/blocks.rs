//! Rendering blocks as the `@@@OLD`/`@@@NEW`/`@@@END` blob the `edit` tool
//! consumes, plus the budget and banner that go with it.
//!
//! The output is deliberately two things in one string: a first line the model
//! must read, and the blob it can paste verbatim. Prose before a block is
//! ignored by the blob parser (a header must be a single whitespace-free token),
//! so the banner is safe to sit in front of the payload.

use bsed_blockedit::blob::{DIVIDER, REPLACE_MARKER, SEARCH_MARKER};

use crate::hunks::Block;

/// Render one block, header included, terminated by a newline.
fn render_block(path: &str, b: &Block) -> String {
    let mut s = String::with_capacity(b.old_text.len() + b.new_text.len() + 64);
    s.push_str(path);
    s.push(':');
    s.push_str(&b.anchor.to_string());
    s.push('\n');
    s.push_str(SEARCH_MARKER);
    s.push('\n');
    s.push_str(&b.old_text);
    s.push('\n');
    s.push_str(DIVIDER);
    s.push('\n');
    s.push_str(&b.new_text);
    s.push('\n');
    s.push_str(REPLACE_MARKER);
    s.push('\n');
    s
}

/// A line inside OLD or NEW that the blob parser would read as structure.
///
/// Only the three *exact* markers are structural. A near miss such as
/// `@@@END>` is content and round-trips fine (see `bsed_blockedit::blob`), so this
/// check is narrow on purpose — a false positive here would refuse an edit that
/// would have applied cleanly.
pub fn structural_marker_in(text: &str) -> Option<&'static str> {
    for line in text.lines() {
        if line == SEARCH_MARKER {
            return Some(SEARCH_MARKER);
        }
        if line == DIVIDER {
            return Some(DIVIDER);
        }
        if line == REPLACE_MARKER {
            return Some(REPLACE_MARKER);
        }
    }
    None
}

/// Check that the blocks we built can be expressed in the blob format, and say
/// precisely why not when they cannot.
///
/// The in-memory check comes first on purpose: a marker line inside a block body
/// makes the blob *unparseable*, so asking the parser first would report its
/// generic "unexpected delimiter" instead of naming the block and the line the
/// caller has to deal with. Parsing afterwards is the belt to that pair of
/// braces: if the round-trip does not recover our own blocks, something else is
/// wrong and we want to hear about it now rather than at apply time.
pub fn verify(blocks: &[Block], blob: &str) -> Result<(), String> {
    for (i, b) in blocks.iter().enumerate() {
        let found = structural_marker_in(&b.old_text).or_else(|| structural_marker_in(&b.new_text));
        if let Some(marker) = found {
            return Err(format!(
                "block {} contains the line `{marker}`, which the edit tool reads as block \
                 structure rather than content. Re-run against a file without that line, or \
                 apply the change with the edit tool's oldText/newText form instead of block \
                 syntax.",
                i + 1
            ));
        }
    }

    let parsed = bsed_blockedit::parse_blocks(blob).map_err(|e| {
        format!("the blocks could not be rendered in the edit tool's block format: {e}")
    })?;
    if parsed.edits.len() != blocks.len() {
        return Err(format!(
            "internal: rendered {} blocks but the edit tool's parser recovered {}",
            blocks.len(),
            parsed.edits.len()
        ));
    }
    for (i, (e, b)) in parsed.edits.iter().zip(blocks.iter()).enumerate() {
        if e.old_text != b.old_text || e.new_text != b.new_text {
            return Err(format!(
                "internal: block {} did not survive the round-trip through the edit tool's \
                 block format",
                i + 1
            ));
        }
    }
    Ok(())
}

/// Render `blocks` as a blob, stopping before `max_bytes` on a block boundary.
#[derive(Debug)]
pub struct Rendered {
    pub blob: String,
    /// Blocks actually emitted.
    pub shown: usize,
    /// Blocks that existed before the budget was applied.
    pub total: usize,
    /// True when the budget cut the list short.
    pub truncated: bool,
}

/// Render only as many blocks as fit, never splitting one.
///
/// A partial block is worse than no block: it would either fail to apply or, if
/// the model trimmed it by hand, apply something sed never said.
pub fn render(path: &str, blocks: &[Block], max_bytes: usize) -> Rendered {
    let total = blocks.len();
    let mut blob = String::new();
    let mut shown = 0usize;
    for (i, b) in blocks.iter().enumerate() {
        let chunk = render_block(path, b);
        if i > 0 && blob.len() + chunk.len() + 1 > max_bytes {
            break;
        }
        if i > 0 {
            blob.push('\n');
        }
        blob.push_str(&chunk);
        shown += 1;
    }
    // Tidy the trailing newline so the payload round-trips as typed.
    if blob.ends_with('\n') {
        blob.pop();
    }
    Rendered {
        blob,
        shown,
        total,
        truncated: shown < total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(anchor: usize, old: &str, new: &str) -> Block {
        Block {
            anchor,
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn renders_anchored_header() {
        let r = render("src/x.rs", &[b(12, "a", "b")], 50_000);
        assert_eq!(r.blob, "src/x.rs:12\n@@@OLD\na\n@@@NEW\nb\n@@@END");
        assert_eq!((r.shown, r.total, r.truncated), (1, 1, false));
    }

    #[test]
    fn empty_new_renders_as_an_empty_section() {
        let r = render("src/x.rs", &[b(6, "gone", "")], 50_000);
        assert!(r.blob.contains("@@@NEW\n\n@@@END"), "{}", r.blob);
        assert!(verify(&[b(6, "gone", "")], &r.blob).is_ok());
    }

    #[test]
    fn budget_stops_on_a_block_boundary() {
        let blocks: Vec<Block> = (1..=5)
            .map(|i| b(i, "old text here", "new text here"))
            .collect();
        // Room for two blocks only.
        let one = render("p", &blocks[..1], 200);
        let r = render("p", &blocks, one.blob.len() + 10);
        assert!(r.shown < r.total, "{r:?}");
        assert!(r.truncated);
        // Whatever survived is still whole and parseable.
        let parsed = bsed_blockedit::parse_blocks(&r.blob).unwrap();
        assert_eq!(parsed.edits.len(), r.shown);
        assert_eq!(parsed.edits[0].old_text, "old text here");
    }

    #[test]
    fn round_trip_preserves_texts_and_anchors() {
        let blocks = vec![b(3, "a\nb", "A\nB"), b(9, "x", "y")];
        let r = render("src/x.rs", &blocks, 50_000);
        let parsed = bsed_blockedit::parse_blocks(&r.blob).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("src/x.rs"));
        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].old_text, "a\nb");
        assert_eq!(parsed.edits[0].new_text, "A\nB");
        assert_eq!(parsed.edits[0].start_line, Some(3));
        assert_eq!(parsed.edits[1].start_line, Some(9));
    }

    #[test]
    fn exact_marker_lines_are_flagged() {
        assert_eq!(structural_marker_in("ok\n@@@OLD\nok"), Some("@@@OLD"));
        assert_eq!(structural_marker_in("@@@END"), Some("@@@END"));
        // Near misses are content, not structure.
        assert_eq!(structural_marker_in("@@@END>"), None);
        assert_eq!(structural_marker_in("  @@@OLD"), None);
        assert_eq!(structural_marker_in("see @@@OLD here"), None);
    }
}
