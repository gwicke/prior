//! Type definitions for the session storage layer.
//!
//! Follows pi's JSONL format exactly: camelCase field names, `type`-tagged
//! entry variants, and the pi `AgentMessage` wire format for message content.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CWD encoding (pi-compatible)
// ---------------------------------------------------------------------------

/// Encode a CWD path into a pi-compatible directory name.
///
/// Pi's exact algorithm (`jsonl-repo.ts:encodeCwd`): strip leading `/` or `\`,
/// replace `/`, `\`, `:` with `-`, wrap with `--...--`. Ambiguous for path
/// components containing hyphens, but the directory name is write-only — the
/// real CWD lives in the session header JSON.
pub fn encode_cwd(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let stripped = s.trim_start_matches(['/', '\\']);
    let encoded = stripped.replace(['/', '\\', ':'], "-");
    format!("--{}--", encoded)
}

/// Decode a CWD directory name back to a path.
///
/// **Lossy!** Inherently ambiguous (see `encode_cwd`). Exists for display and
/// debugging — never for resolving the session's working directory. Prefer
/// reading `cwd` from the session header.
pub fn decode_cwd(encoded: &str) -> Option<String> {
    let inner = encoded.strip_prefix("--")?.strip_suffix("--")?;
    let decoded = inner.replace('-', "/");
    Some(format!("/{}", decoded))
}

/// Generate a session filename matching pi's convention.
///
/// Format: `<iso-timestamp>_<uuid>.jsonl` where `:` and `.` in the timestamp
/// are replaced with `-` for cross-platform filename safety.
pub fn session_filename(timestamp: &chrono::DateTime<chrono::Utc>) -> String {
    let ts = timestamp.format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let uuid = Uuid::now_v7();
    format!("{}_{}.jsonl", ts, uuid)
}

/// Parse timestamp and UUID from a session filename.
pub fn parse_session_filename(name: &str) -> Option<(String, String)> {
    let name = name.strip_suffix(".jsonl")?;
    let (ts, uuid) = name.split_once('_')?;
    Some((ts.to_string(), uuid.to_string()))
}

// ---------------------------------------------------------------------------
// Session header (first line of every JSONL file)
// ---------------------------------------------------------------------------

/// The first line of every JSONL session file. Matches pi's `SessionHeader`
/// interface exactly (camelCase, version 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    /// Always `"session"` — serialised as the JSON key `"type"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Schema version; currently `3`.
    pub version: u32,
    /// UUIDv7 identifying this session.
    pub id: String,
    /// ISO 8601 timestamp of session creation.
    pub timestamp: String,
    /// The working directory at session start.
    pub cwd: String,
    /// Absolute path to the parent session file (when this session was forked).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<PathBuf>,
    /// Optional metadata blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Provider name (pi-compatible top-level metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model ID (pi-compatible top-level metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Thinking level (pi-compatible top-level metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// User-defined display name (for fast listing without scanning for session_info).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SessionHeader {
    /// Create a new session header for the given CWD. Generates a UUIDv7 and
    /// uses the current UTC time as the timestamp.
    pub fn new(cwd: &std::path::Path) -> Self {
        Self {
            kind: "session".to_string(),
            version: 3,
            id: Uuid::now_v7().to_string(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cwd: cwd.to_string_lossy().to_string(),
            parent_session: None,
            metadata: None,
            provider: None,
            model_id: None,
            thinking_level: None,
            name: Some(String::new()),
        }
    }

    /// Create a new session header with optional model context metadata.
    pub fn with_model_context(
        cwd: &std::path::Path,
        provider: Option<String>,
        model_id: Option<String>,
        thinking_level: Option<String>,
    ) -> Self {
        Self {
            provider,
            model_id,
            thinking_level,
            ..Self::new(cwd)
        }
    }

    /// Create a new session header with a parent session link (for forks),
    /// carrying forward the parent's provider/model/thinking metadata.
    pub fn forked(
        cwd: &std::path::Path,
        parent: PathBuf,
        provider: Option<String>,
        model_id: Option<String>,
        thinking_level: Option<String>,
    ) -> Self {
        Self {
            parent_session: Some(parent),
            provider,
            model_id,
            thinking_level,
            ..Self::new(cwd)
        }
    }
}

// ---------------------------------------------------------------------------
// Session entry (subsequent lines)
// ---------------------------------------------------------------------------

/// A single entry in the session tree. `#[serde(flatten)]` merges the
/// type-specific fields into the same JSON object as the base fields,
/// producing the flat JSONL line pi expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEntry {
    /// Unique entry ID (UUIDv7 string).
    pub id: String,
    /// ID of the parent entry in the tree. `None` for the first entry after
    /// the header (root of the entry tree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Type-specific payload. Flattened so that e.g. a `Message` variant
    /// produces `{"type":"message","message":{...}, "id":"...", ...}`.
    #[serde(flatten)]
    pub content: SessionEntryContent,
}

/// The type-specific payload for a session entry.
///
/// `#[serde(tag = "type")]` writes a `"type"` key alongside the variant
/// fields, making the JSON self-describing and matching pi's wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryContent {
    /// A user or assistant message. The `message` field stores the pi
    /// `AgentMessage` format as raw JSON (not rig's `Message` type), ensuring
    /// wire compatibility with pi's JSONL files.
    Message {
        /// The pi-format `AgentMessage` (raw JSON value).
        message: serde_json::Value,
    },
    /// Thinking level change (e.g., `"off"`, `"low"`, `"high"`).
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    /// Model/provider change.
    #[serde(rename = "model_change")]
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    /// Active tools change.
    #[serde(rename = "active_tools_change")]
    ActiveToolsChange {
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
    },
    /// Context compaction record.
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
    /// Fork/branch annotation.
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
    },
    /// Named label on an entry.
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Session name/metadata.
    #[serde(rename = "session_info")]
    SessionInfo {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Custom entry (does NOT participate in LLM context).
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        data: serde_json::Value,
    },
    /// Custom message entry (DOES participate in LLM context, rendered as user
    /// message).
    #[serde(rename = "custom_message")]
    CustomMessage {
        #[serde(rename = "customType")]
        custom_type: String,
        content: serde_json::Value,
    },
    /// Current position pointer. The latest `Leaf` entry determines which node
    /// in the tree is the "current position" for context building.
    Leaf {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Session metadata (for listing)
// ---------------------------------------------------------------------------

/// Detailed metadata populated only when viewing a session's detail view.
#[derive(Debug, Clone)]
pub struct SessionDetail {
    /// Number of message entries in the session (user + assistant + toolResult).
    pub message_count: usize,
    /// Number of user-role message entries (i.e. user prompts, one per
    /// logical turn). Excludes assistant and `toolResult` entries. This is the
    /// canonical "turn count" used by the `/stat` and `/session <id>` panels.
    pub user_message_count: usize,
    /// Total token count (from usage on the leaf branch, if tracked).
    pub token_count: Option<u64>,
    /// Full text of the last user message (for detail display).
    pub last_prompt_text: Option<String>,
    /// Timestamp of the last user message.
    pub last_prompt_timestamp: Option<String>,
    /// Summary from the last compaction entry (for detail display).
    pub compaction_summary: Option<String>,
}

/// Lightweight metadata about a session, derived from the header + quick scan.
/// Used for session listing and selection.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    /// Session UUID.
    pub id: String,
    /// ISO 8601 creation timestamp.
    pub timestamp: String,
    /// Working directory at session creation.
    pub cwd: String,
    /// Absolute path to the `.jsonl` file.
    pub path: PathBuf,
    /// User-assigned display name (from header or latest `SessionInfo` entry).
    pub name: Option<String>,
    /// Absolute path to the parent session file (if forked).
    pub parent_session: Option<PathBuf>,
    /// A short preview of the first user message in the session.
    pub first_prompt_preview: Option<String>,
    /// File size in bytes (for display and sorting in listing).
    pub file_size: u64,
    /// File modification time (for cache validation and age display).
    pub file_mtime: Option<SystemTime>,
    /// Detailed metadata (message counts, last message, etc.) - only populated
    /// on demand for the detail view via `read_metadata_detail`.
    pub detail: Option<SessionDetail>,
}

// ---------------------------------------------------------------------------
// In-memory index (built by scanning a JSONL file)
// ---------------------------------------------------------------------------

/// In-memory representation of a session's entry tree, rebuilt from JSONL
/// on open. The full entry map is needed for tree operations (fork picker,
/// label lookup, path-to-root walks).
#[derive(Debug, Clone)]
pub struct SessionIndex {
    /// Parsed session header.
    pub header: SessionHeader,
    /// All entries keyed by ID.
    pub entries: HashMap<String, SessionEntry>,
    /// Current leaf pointer (the `targetId` of the most recent `Leaf` entry).
    pub leaf_id: Option<String>,
    /// Child entries keyed by parent ID. `parent_id -> [child_id, ...]`.
    pub children: HashMap<String, Vec<String>>,
    /// User-assigned session name (from the most recent `SessionInfo` entry).
    pub session_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Session tree (for `/tree` overlay)
// ---------------------------------------------------------------------------

/// A node in the session entry tree, used for the `/tree` overlay.
#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    /// Resolved label text, if any Label entry targets this node.
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// Session context (build_context output)
// ---------------------------------------------------------------------------

/// The reconstructed LLM context derived from a session branch.
#[derive(Debug)]
pub struct SessionContext {
    /// Messages to pass as conversation history for the next LLM call.
    pub messages: Vec<rig_core::message::Message>,
    /// Thinking level in effect at the leaf (latest `ThinkingLevelChange` on
    /// the path to root; `None` = use config default).
    pub thinking_level: Option<String>,
    /// Model/provider in effect at the leaf (latest `ModelChange`).
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl SessionEntry {
    /// Create a new leaf entry pointing to `target_id`.
    pub fn new_leaf(target_id: Option<String>, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Leaf { target_id },
        }
    }

    /// Create a new session_info entry.
    pub fn new_session_info(name: Option<String>, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::SessionInfo { name },
        }
    }

    /// Create a new model_change entry.
    pub fn new_model_change(provider: String, model_id: String, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::ModelChange { provider, model_id },
        }
    }

    /// Create a new thinking_level_change entry.
    pub fn new_thinking_level_change(thinking_level: String, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::ThinkingLevelChange { thinking_level },
        }
    }

    /// Create a new branch_summary entry.
    pub fn new_branch_summary(from_id: String, summary: String, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::BranchSummary { from_id, summary },
        }
    }

    /// Create a new compaction entry.
    pub fn new_compaction(
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        parent_id: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            },
        }
    }

    /// Create a new label entry.
    pub fn new_label(target_id: String, label: Option<String>, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Label { target_id, label },
        }
    }
}

impl SessionIndex {
    /// Walk the `parent_id` chain from `leaf_id` to root, returning entries in
    /// root→leaf order (suitable for LLM context building).
    ///
    /// Returns an empty vec if `leaf_id` is `None` or not found.
    pub fn get_path_to_root(&self, leaf_id: &str) -> Vec<SessionEntry> {
        let mut path: Vec<SessionEntry> = Vec::new();
        let mut current_id = leaf_id.to_string();
        while let Some(entry) = self.entries.get(&current_id) {
            path.push(entry.clone());
            match &entry.parent_id {
                Some(pid) => current_id = pid.clone(),
                None => break,
            }
        }
        path.reverse();
        path
    }

    /// Build the full entry tree, rooted at entries with no parent.
    /// Children are sorted so the branch containing `preferred_id`
    /// (typically the current leaf) appears first.
    ///
    /// Performance notes (ref critique #4 in doc/tree.md):
    /// - `labels` is a pre-computed `target_id → label` map to avoid O(n²)
    /// - `active_path` is built once via `get_path_to_root`
    pub fn build_tree(&self, preferred_id: Option<&str>) -> Vec<SessionTreeNode> {
        // Precompute label map: target_id -> label
        let mut labels: std::collections::HashMap<&str, Option<&str>> =
            std::collections::HashMap::new();
        for entry in self.entries.values() {
            if let SessionEntryContent::Label { target_id, label } = &entry.content {
                labels.insert(target_id.as_str(), label.as_deref());
            }
        }

        // Precompute active-path set from preferred_id → root
        let active_path: std::collections::HashSet<String> = preferred_id
            .map(|leaf| {
                let path = self.get_path_to_root(leaf);
                path.iter().map(|e| e.id.clone()).collect()
            })
            .unwrap_or_default();

        // Identify root entries: those whose parent_id is None
        // or references an entry not in the map.
        let roots: Vec<&str> = self
            .entries
            .keys()
            .filter(|id| {
                self.entries
                    .get(id.as_str())
                    .and_then(|e| e.parent_id.as_deref())
                    .map(|pid| !self.entries.contains_key(pid))
                    .unwrap_or(true)
            })
            .map(|s| s.as_str())
            .collect();

        let mut out: Vec<SessionTreeNode> = roots
            .iter()
            .filter_map(|id| self.build_subtree(id, &active_path, &labels))
            .collect();

        // Sort: subtree containing preferred_id first
        if let Some(_pref) = preferred_id {
            out.sort_by_key(|n| {
                if active_path.contains(n.entry.id.as_str()) {
                    0
                } else {
                    1
                }
            });
        }

        out
    }

    /// Build a subtree rooted at `entry_id` iteratively (no recursion) to
    /// avoid stack overflow on deeply nested session trees.
    ///
    /// Skips the entry itself if it is a meta entry (Leaf, SessionInfo)
    /// — they don't show as rows in the tree list — but **still walks
    /// into their children**. A `SessionInfo` entry that sits in the
    /// middle of the active path must not strand its descendants: the
    /// tree must keep the chain from the root to the leaf intact, even
    /// if one of the hops is a meta entry the user doesn't need to
    /// see. (The flat list filters by visibility separately, so even
    /// if the meta node is in the tree, no row is produced for it.)
    fn build_subtree(
        &self,
        root_entry_id: &str,
        _active_path: &std::collections::HashSet<String>,
        labels: &std::collections::HashMap<&str, Option<&str>>,
    ) -> Option<SessionTreeNode> {
        // Stack frames for iterative post-order traversal.
        // Each frame: (entry_id, state)
        // state = 0: visiting, state = 1: processing children results
        #[derive(Debug)]
        struct Frame {
            entry_id: String,
            state: u8,
            entry: SessionEntry,
            children_results: Vec<SessionTreeNode>,
        }

        let root_entry = self.entries.get(root_entry_id)?.clone();
        let mut stack = vec![Frame {
            entry_id: root_entry_id.to_string(),
            state: 0,
            entry: root_entry,
            children_results: Vec::new(),
        }];

        while let Some(frame) = stack.last_mut() {
            if frame.state == 0 {
                frame.state = 1;
                // Push children onto stack for post-order processing
                if let Some(child_ids) = self.children.get(&frame.entry_id) {
                    for cid in child_ids.iter().rev() {
                        if let Some(child_entry) = self.entries.get(cid) {
                            stack.push(Frame {
                                entry_id: cid.clone(),
                                state: 0,
                                entry: child_entry.clone(),
                                children_results: Vec::new(),
                            });
                        }
                    }
                }
            } else {
                // Pop the frame and build its node
                let frame = stack.pop().unwrap();
                let entry = frame.entry;
                let children = frame.children_results;

                // Meta entries (Leaf, SessionInfo) — the entry itself is
                // invisible in the list, but its children must still be
                // reachable. If the meta node has visible descendants,
                // return them wrapped under the meta entry so the tree
                // shape mirrors the JSONL exactly. The flat list filters
                // these out via `entry_display_label`, so the user never
                // sees the SessionInfo row.
                let node = if matches!(
                    entry.content,
                    SessionEntryContent::Leaf { .. } | SessionEntryContent::SessionInfo { .. }
                ) {
                    if children.is_empty() {
                        None
                    } else {
                        Some(SessionTreeNode {
                            entry,
                            children,
                            label: None,
                        })
                    }
                } else {
                    let label = labels
                        .get(entry.id.as_str())
                        .copied()
                        .flatten()
                        .map(String::from);
                    Some(SessionTreeNode {
                        entry,
                        children,
                        label,
                    })
                };

                // If there's a parent frame, pass the result up
                if let Some(parent_frame) = stack.last_mut() {
                    if let Some(node) = node {
                        parent_frame.children_results.push(node);
                    }
                } else {
                    // This was the root frame
                    return node;
                }
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encode_cwd_unix_path() {
        assert_eq!(
            encode_cwd(Path::new("/home/gabriel/src/picocode")),
            "--home-gabriel-src-picocode--"
        );
    }

    #[test]
    fn encode_cwd_with_hyphen_component() {
        assert_eq!(
            encode_cwd(Path::new("/home/gabriel/my-project")),
            "--home-gabriel-my-project--"
        );
    }

    #[test]
    fn encode_cwd_root() {
        assert_eq!(encode_cwd(Path::new("/")), "----");
    }

    #[test]
    fn encode_cwd_windows_path() {
        // pi encodes each of `/`, `\`, `:` individually, so `C:\Users`
        // produces `C--Users` (two dashes: one for `:`, one for `\`).
        assert_eq!(
            encode_cwd(Path::new("C:\\Users\\me\\project")),
            "--C--Users-me-project--"
        );
    }

    #[test]
    fn decode_cwd_roundtrip_simple() {
        let encoded = encode_cwd(Path::new("/home/gabriel/src"));
        // decode_cwd is lossy but for paths without hyphens it round-trips
        assert_eq!(decode_cwd(&encoded).as_deref(), Some("/home/gabriel/src"));
    }

    #[test]
    fn decode_cwd_returns_none_for_bad_encoding() {
        assert!(decode_cwd("no-prefix").is_none());
        assert!(decode_cwd("--no-suffix").is_none());
    }

    #[test]
    fn session_filename_format() {
        let dt = chrono::DateTime::parse_from_rfc3339("2026-07-20T22:22:21.906Z")
            .unwrap()
            .with_timezone(&Utc);
        let filename = session_filename(&dt);
        // The UUID part varies, but the prefix should match
        assert!(filename.starts_with("2026-07-20T22-22-21-906Z_"));
        assert!(filename.ends_with(".jsonl"));
        assert!(filename.len() > 40); // timestamp + uuid + ext
    }

    #[test]
    fn parse_session_filename_roundtrip() {
        let dt = Utc::now();
        let filename = session_filename(&dt);
        let (ts, uuid) = parse_session_filename(&filename).unwrap();
        assert_eq!(ts, dt.format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string());
        // uuid is a valid UUIDv7
        assert!(uuid::Uuid::try_parse(&uuid).is_ok());
    }

    #[test]
    fn session_header_serialization() {
        let header = SessionHeader::new(Path::new("/home/test"));
        let json = serde_json::to_string(&header).unwrap();
        // Must contain camelCase fields
        assert!(json.contains("\"type\":\"session\""));
        assert!(json.contains("\"version\":3"));
        assert!(json.contains("\"cwd\":\"/home/test\""));
        assert!(json.contains("\"id\":\""));
        // parent_session and metadata should be absent when None
        assert!(!json.contains("parentSession"));
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn session_header_deserialization() {
        let json = r#"{"type":"session","version":3,"id":"abc-123","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/home/test"}"#;
        let header: SessionHeader = serde_json::from_str(json).unwrap();
        assert_eq!(header.kind, "session");
        assert_eq!(header.version, 3);
        assert_eq!(header.id, "abc-123");
        assert_eq!(header.cwd, "/home/test");
        assert!(header.parent_session.is_none());
    }

    #[test]
    fn session_entry_roundtrip_message() {
        let entry = SessionEntry {
            id: "entry-1".into(),
            parent_id: Some("root".into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}]
                }),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        // Must contain type tag at top level
        assert!(json.contains("\"type\":\"message\""));
        // Must contain camelCase parentId
        assert!(json.contains("\"parentId\":\"root\""));
        // Must contain the message payload
        assert!(json.contains("\"role\":\"user\""));

        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "entry-1");
        assert_eq!(parsed.parent_id, Some("root".into()));
        match &parsed.content {
            SessionEntryContent::Message { message } => {
                assert_eq!(message["role"], "user");
            }
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn session_entry_roundtrip_leaf() {
        let entry = SessionEntry {
            id: "leaf-1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Leaf {
                target_id: Some("entry-42".into()),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"leaf\""));
        assert!(json.contains("\"targetId\":\"entry-42\""));

        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        match &parsed.content {
            SessionEntryContent::Leaf { target_id } => {
                assert_eq!(target_id.as_deref(), Some("entry-42"));
            }
            _ => panic!("expected Leaf variant"),
        }
    }

    #[test]
    fn session_entry_roundtrip_all_variants() {
        // Test that every variant serializes without error
        let variants: Vec<SessionEntryContent> = vec![
            SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
            SessionEntryContent::ThinkingLevelChange {
                thinking_level: "high".into(),
            },
            SessionEntryContent::ModelChange {
                provider: "anthropic".into(),
                model_id: "claude-3-5-sonnet".into(),
            },
            SessionEntryContent::ActiveToolsChange {
                active_tool_names: vec!["bash".into(), "read".into()],
            },
            SessionEntryContent::Compaction {
                summary: "summarized".into(),
                first_kept_entry_id: "entry-10".into(),
                tokens_before: 5000,
            },
            SessionEntryContent::BranchSummary {
                from_id: "entry-5".into(),
                summary: "forked for experiment".into(),
            },
            SessionEntryContent::Label {
                target_id: "entry-3".into(),
                label: Some("important".into()),
            },
            SessionEntryContent::SessionInfo {
                name: Some("my-session".into()),
            },
            SessionEntryContent::Custom {
                custom_type: "my-ext".into(),
                data: serde_json::json!({"key": "val"}),
            },
            SessionEntryContent::CustomMessage {
                custom_type: "my-ext".into(),
                content: serde_json::json!("plain text"),
            },
            SessionEntryContent::Leaf { target_id: None },
        ];

        for variant in variants {
            let entry = SessionEntry {
                id: Uuid::now_v7().to_string(),
                parent_id: None,
                timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                content: variant,
            };
            let json = serde_json::to_string(&entry).unwrap();
            let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
            // Verify entry ID round-trips
            assert_eq!(parsed.id, entry.id);
            // Verify JSON has a "type" field
            let parsed_json: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert!(parsed_json.get("type").and_then(|t| t.as_str()).is_some());
        }
    }

    #[test]
    fn session_index_path_to_root() {
        let mut index = SessionIndex {
            header: SessionHeader::new(Path::new("/test")),
            entries: HashMap::new(),
            leaf_id: Some("entry-3".into()),
            children: HashMap::new(),
            session_name: None,
        };

        // Build a simple chain: root -> entry-1 -> entry-2 -> entry-3
        let root = SessionEntry {
            id: "root".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        let e1 = SessionEntry {
            id: "entry-1".into(),
            parent_id: Some("root".into()),
            timestamp: "t2".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[]}),
            },
        };
        let e2 = SessionEntry {
            id: "entry-2".into(),
            parent_id: Some("entry-1".into()),
            timestamp: "t3".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        let e3 = SessionEntry {
            id: "entry-3".into(),
            parent_id: Some("entry-2".into()),
            timestamp: "t4".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[]}),
            },
        };

        for e in [root, e1, e2, e3] {
            let pid = e.parent_id.clone();
            let entry_id = e.id.clone();
            index.entries.insert(entry_id.clone(), e);
            if let Some(pid) = pid {
                index.children.entry(pid).or_default().push(entry_id);
            }
        }

        let path = index.get_path_to_root("entry-3");
        assert_eq!(path.len(), 4);
        assert_eq!(path[0].id, "root");
        assert_eq!(path[1].id, "entry-1");
        assert_eq!(path[2].id, "entry-2");
        assert_eq!(path[3].id, "entry-3");
    }

    #[test]
    fn session_index_path_to_root_empty_for_missing_leaf() {
        let index = SessionIndex {
            header: SessionHeader::new(Path::new("/test")),
            entries: HashMap::new(),
            leaf_id: None,
            children: HashMap::new(),
            session_name: None,
        };
        assert!(index.get_path_to_root("nonexistent").is_empty());
    }
}
