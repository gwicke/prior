//! Persistent session storage layer.
//!
//! Follows the pi agent's JSONL session format for on-disk compatibility.
//! See `doc/session_storage.md` for the full design.

pub mod repo;
// `session::session` is not a mistake: the module mirrors the on-disk session
// layout described in `doc/session_storage.md`, so the path a reader expects
// for session files is the path the types live at. Renaming it would churn
// every `use` to buy a lint that has no bearing on the code being correct.
#[allow(clippy::module_inception)]
pub mod session;
pub mod storage;
pub mod types;

#[cfg(test)]
pub mod pi_compat_test;

pub use repo::{JsonlSessionRepo, SessionRepo};
pub(crate) use session::build_manual_template;
pub(crate) use session::build_text_only_summary;
pub(crate) use session::collect_touched_files;
pub(crate) use session::entries_since_last_compaction;
pub use session::Session;
pub(crate) use session::TouchedFile;
pub(crate) use session::{
    branch_summary_message_text, render_files_section, user_prompt_text, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX,
};
pub(crate) use session::{pi_message_to_rig, pi_toolresults_to_rig_message};
pub use storage::{JsonlSessionStorage, SessionStorage};
pub use types::{
    decode_cwd, encode_cwd, parse_session_filename, session_filename, SessionContext, SessionEntry,
    SessionEntryContent, SessionHeader, SessionIndex, SessionMetadata, SessionTreeNode,
};
