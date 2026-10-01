//! Re-export of the `blockedit` crate's blob parser, plus the split primitives
//! both surfaces of a multi-file call share.
//!
//! The implementation moved to `crates/blockedit/src/blob.rs` so the `bsed`
//! binary can emit and consume the exact same format without depending on
//! picocode. This module stays as the in-crate path so the ~15 existing
//! `crate::edit_blocks::…` call sites are untouched.

use serde_json::Value;

pub use blockedit::blob::*;

/// The id of file `i` of `n` in a split of `orig`.
///
/// Derived, never minted — and deliberately defined *once*, here, because the
/// two surfaces that split a multi-file call never see each other: the
/// model-facing transform (`agent.rs`) and the persist transform
/// (`session.rs`) each split their own half of the conversation, and both must
/// independently arrive at the same id for the same file. Minting a random id
/// on either side would pair a call with the wrong result, and re-deriving a
/// fresh one would break correlation with an already-stored half. The `~` is
/// not a character any provider issues, so a derived id cannot collide with a
/// real one — which is what lets a synthetic call carry no `provider`.
pub fn split_id(
    orig: &rig_core::message::ToolCallId,
    i: usize,
    n: usize,
) -> rig_core::message::ToolCallId {
    rig_core::message::ToolCallId::new(format!("{orig}~f{i}n{n}"))
        .expect("a derived id is never empty")
}

/// Split one multi-file `edit` call's `blocks` argument into one blob per file,
/// or `None` if the call is not a well-formed multi-file blob.
///
/// `None` covers the single-file case (which is the overwhelmingly common one
/// and must stay byte-identical), a blob that does not parse, and one beyond
/// anything the tool itself handles and reports — so there is nothing for a
/// surface to split.
///
/// The per-file blobs are the *model-facing* form, so a split call is
/// indistinguishable from N real single-file calls; each surface then applies
/// its own canonicalisation (or leaves the block form) as it would have for a
/// genuine single-file call.
pub fn per_file_blocks(args: &Value) -> Option<Vec<String>> {
    let parsed = parse_blocks(blob_text(args)?).ok()?;
    let files = split_by_file(parsed).ok()?;
    (files.len() > 1).then(|| {
        files
            .iter()
            .map(|f| render_blob(&f.path, &f.edits))
            .collect()
    })
}
