//! Re-export of the `bsed_blockedit` crate's blob parser, plus `split_id`, the
//! derived per-file call id every `edit` split uses.
//!
//! The implementation moved to `crates/bsed_blockedit/src/blob.rs` so the `bsed`
//! binary can emit and consume the exact same format without depending on
//! prior. This module stays as the in-crate path so the ~15 existing
//! `crate::edit_blocks::…` call sites are untouched.

pub use bsed_blockedit::blob::*;

/// The id of file `i` of `n` in a split of `orig`.
///
/// Derived, never minted, so the id is stable across runs: a replayed session,
/// a re-prepared call and the stored pair all reach the same id for the same
/// file. `prepare_edit_call` is now the only caller — persist and display read
/// its output — so this is a stable-id convention rather than a correctness
/// requirement, but keeping it derived costs nothing and keeps old sessions
/// pairing.
pub fn split_id(orig: &rig_core::message::CallId, i: usize, n: usize) -> rig_core::message::CallId {
    // Use the original tool call ID for the first file, only assign derived IDs for additional
    // files. This ensures that the original tool call ID has a valid matching result, which some
    // providers (e.g. deepseek) require within a turn.
    if i == 0 {
        orig.clone()
    } else {
        // The wire form of the derived id is never empty (`orig_` is always a
        // prefix), so it is always a provider-shaped id.
        rig_core::message::CallId::from_wire(format!("{orig}_f{i}n{n}"))
    }
}
