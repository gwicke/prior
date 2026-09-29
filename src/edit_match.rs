//! Re-export of the `blockedit` crate's match/diagnose pipeline.
//!
//! The implementation moved to `crates/blockedit/src/diagnose.rs` so `bsed`
//! shares one matcher with the agent. See `src/edit_blocks.rs` for why the
//! shim exists.

pub use blockedit::diagnose::*;
