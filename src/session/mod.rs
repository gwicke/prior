//! Persistent session storage layer.
//!
//! Follows the pi agent's JSONL session format for on-disk compatibility.
//! See `doc/session_storage.md` for the full design.

pub mod types;
pub mod storage;
pub mod repo;

pub use types::{
    encode_cwd, decode_cwd, session_filename, parse_session_filename,
    SessionHeader, SessionEntry, SessionEntryContent,
    SessionMetadata, SessionIndex, SessionContext,
};
pub use storage::{SessionStorage, JsonlSessionStorage};
pub use repo::{SessionRepo, JsonlSessionRepo};
