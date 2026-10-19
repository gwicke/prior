//! Re-export of the `bsed_blockedit` crate's match/diagnose pipeline.
//!
//! The implementation moved to `crates/bsed_blockedit/src/diagnose.rs` so `bsed`
//! shares one matcher with the agent. See `src/edit_blocks.rs` for why the
//! shim exists.

pub use bsed_blockedit::diagnose::*;
