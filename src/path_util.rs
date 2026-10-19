//! Re-export of the `bsed_blockedit` crate's path helpers.
//!
//! Moved out so the extracted block-edit core is self-contained (the blob
//! parser's path comparison needs lexical normalization). This shim keeps
//! every `crate::path_util::…` call site in prior working.

pub use bsed_blockedit::path::*;
