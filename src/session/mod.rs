//! Persistent session storage layer.
//!
//! Follows the pi agent's JSONL session format for on-disk compatibility.
//! See `doc/session_storage.md` for the full design.

pub mod repo;
pub mod session;
pub mod storage;
pub mod types;

#[cfg(test)]
pub mod pi_compat_test;

pub use repo::{JsonlSessionRepo, SessionRepo};
pub(crate) use session::build_manual_template;
pub(crate) use session::build_text_only_summary;
pub use session::Session;
pub(crate) use session::{pi_message_to_rig, pi_toolresults_to_rig_message};
pub use storage::{JsonlSessionStorage, SessionStorage};
pub use types::{
    decode_cwd, encode_cwd, parse_session_filename, session_filename, SessionContext, SessionEntry,
    SessionEntryContent, SessionHeader, SessionIndex, SessionMetadata, SessionTreeNode,
};
