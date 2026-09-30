//! The `@@@OLD` / `@@@NEW` / `@@@END` block-edit core.
//!
//! One implementation, three consumers: picocode's `edit` tool, the `bsed`
//! binary (which previews GNU sed changes in exactly this format, and applies
//! them from a shell), and the session layer's canonicalization.
//!
//! Everything here is pure — it operates on `&str`, [`serde_json::Value`],
//! and returns text. There is no agent, no tool-call plumbing, and no path
//! *policy*: resolving a caller-supplied path against an allowlist stays with
//! the caller, so this crate is safe to run outside picocode.
//!
//! Module map:
//!
//! - [`blob`] — the model-facing text format: parse, render, canonicalize.
//! - [`types`] — [`EditOp`] and the 1-based [`EditStartLine`] anchor.
//! - [`wire`] — the `edit` tool's `arguments` JSON into a resolved batch.
//! - [`resolve`] — two-pass anchored-then-content match resolution.
//! - [`apply`] — all-or-nothing application to text (and, thinly, to a file).
//! - [`diagnose`] — fuzzy failure diagnosis for the model-and-UI split.
//! - [`path`] — lexical path helpers (the blob parser compares paths).

pub mod apply;
pub mod arg_error_view;
pub mod blob;
pub mod diagnose;
pub mod path;
pub mod resolve;
pub mod tool_errors;
pub mod types;
pub mod wire;

pub use apply::{apply_to_file, apply_to_text, success_note, ApplyOutcome};
pub use blob::{
    array_to_blob_value, canonicalize_edit_value, group_by_file, merge_path, parse_blocks,
    render_blob, split_by_file, split_line_anchor, FileBatch, ParseError, ParsedBlocks, ParsedEdit,
    DIVIDER, MAX_FILES_PER_CALL, REPLACE_MARKER, SEARCH_MARKER,
};
pub use tool_errors::strip_arg_error_prefixes;
pub use types::{EditOp, EditStartLine};
pub use wire::EditArgs;
