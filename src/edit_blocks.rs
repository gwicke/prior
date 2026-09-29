//! Re-export of the `blockedit` crate's blob parser.
//!
//! The implementation moved to `crates/blockedit/src/blob.rs` so the `bsed`
//! binary can emit and consume the exact same format without depending on
//! picocode. This module stays as the in-crate path so the ~15 existing
//! `crate::edit_blocks::…` call sites are untouched.

pub use blockedit::blob::*;
