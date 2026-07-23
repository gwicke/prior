//! Repository managing multiple JSONL session files in a directory tree.
//!
//! Provides [`JsonlSessionRepo`] which mirrors pi's `JsonlSessionRepo`:
//! sessions are stored under `base_dir/<cwd-encoded>/<timestamp>_<uuid>.jsonl`.
//! The CWD-based directory structure groups sessions by working directory.

use std::path::{Path, PathBuf};
use async_trait::async_trait;
use chrono::Utc;

use crate::session::types::*;
use crate::session::storage::*;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Repository managing multiple sessions in a directory.
#[async_trait]
pub trait SessionRepo: Send + Sync {
    /// Create a new session for the given CWD.  Writes the session header and
    /// returns a storage handle.
    async fn create(&self, cwd: &Path) -> crate::Result<JsonlSessionStorage>;

    /// Open an existing session by metadata, building its full in-memory index.
    async fn open(&self, meta: &SessionMetadata) -> crate::Result<(JsonlSessionStorage, SessionIndex)>;

    /// List all sessions for a given CWD (or all CWDs if `None`).
    async fn list(&self, cwd: Option<&Path>) -> crate::Result<Vec<SessionMetadata>>;

    /// Delete a session file.
    async fn delete(&self, meta: &SessionMetadata) -> crate::Result<()>;

    /// Fork a session from a specific entry.  Copies the header + entries up to
    /// (and including) the fork entry into a new session file, linked via
    /// `parentSession`.
    async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &Path,
    ) -> crate::Result<JsonlSessionStorage>;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Concrete [`SessionRepo`] backed by a directory of JSONL files.
///
/// Session file layout:
/// ```ignore
/// <base_dir>
///   └── <cwd-encoded>/
///        └── <timestamp>_<uuid>.jsonl
/// ```
#[derive(Clone)]
pub struct JsonlSessionRepo {
    base_dir: PathBuf,
}

impl JsonlSessionRepo {
    /// Create a new repo rooted at `base_dir` (typically
    /// `~/.picocode/sessions/`).
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// The absolute base directory for session storage.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Resolve the encoded CWD directory path.
    fn session_dir(&self, cwd: &Path) -> PathBuf {
        self.base_dir.join(encode_cwd(cwd))
    }

    /// Generate a new session file path for the given CWD.
    fn session_file_path(&self, cwd: &Path) -> PathBuf {
        let dir = self.session_dir(cwd);
        let filename = session_filename(&Utc::now());
        dir.join(filename)
    }

    /// Quick-read metadata from a single JSONL file (header only).
    async fn read_metadata(path: &Path) -> Option<SessionMetadata> {
        let content = tokio::fs::read_to_string(path).await.ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        // Count message entries, extract the first user prompt, and track name.
        let mut message_count = 0usize;
        let mut first_prompt_preview = None::<String>;
        let mut last_prompt_text: Option<String> = None;
        let mut last_prompt_timestamp: Option<String> = None;
        let mut name: Option<String> = None;

        for line in content.lines().skip(1) {
            if line.contains("\"type\":\"message\"") {
                message_count += 1;
                if line.contains("\"role\":\"user\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                        let text = val
                            .pointer("/message/content/0/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let ts = val.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            if first_prompt_preview.is_none() {
                                let preview: String = text.chars().collect();
                                first_prompt_preview = Some(preview);
                            }
                            last_prompt_text = Some(text.chars().collect());
                            if !ts.is_empty() {
                                last_prompt_timestamp = Some(ts.to_string());
                            }
                        }
                    }
                }
            } else if line.contains("\"type\":\"session_info\"") {
                // Extract the name from the most recent SessionInfo entry.
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                        if !n.is_empty() {
                            name = Some(n.to_string());
                        }
                    }
                }
            }
        }

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            message_count,
            token_count: None,
            first_prompt_preview,
            last_prompt_text,
            last_prompt_timestamp,
        })
    }
    /// Quick-read metadata from a single JSONL file (header only) — sync version.
    fn read_metadata_sync(path: &Path) -> Option<SessionMetadata> {
        let content = std::fs::read_to_string(path).ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        let mut message_count = 0usize;
        let mut first_prompt_preview = None::<String>;
        let mut last_prompt_text: Option<String> = None;
        let mut last_prompt_timestamp: Option<String> = None;
        let mut name: Option<String> = None;

        for line in content.lines().skip(1) {
            if line.contains("\"type\":\"message\"") {
                message_count += 1;
                if line.contains("\"role\":\"user\"") {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                        let text = val
                            .pointer("/message/content/0/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let ts = val.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            if first_prompt_preview.is_none() {
                                let preview: String = text.chars().collect();
                                first_prompt_preview = Some(preview);
                            }
                            last_prompt_text = Some(text.chars().collect());
                            if !ts.is_empty() {
                                last_prompt_timestamp = Some(ts.to_string());
                            }
                        }
                    }
                }
            } else if line.contains("\"type\":\"session_info\"") {
                // Extract the name from the most recent SessionInfo entry.
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                        if !n.is_empty() {
                            name = Some(n.to_string());
                        }
                    }
                }
            }
        }

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            message_count,
            token_count: None,
            first_prompt_preview,
            last_prompt_text,
            last_prompt_timestamp,
        })
    }

    /// Synchronously list all session files under a given directory.
    /// Used by the overlay (avoids async machinery on each keystroke).
    fn list_sessions_in_dir(dir: &Path) -> Vec<SessionMetadata> {
        let mut sessions = Vec::new();
        let read_dir = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return sessions,
        };
        for entry in read_dir.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "jsonl") {
                if let Some(meta) = Self::read_metadata_sync(&path) {
                    sessions.push(meta);
                }
            }
        }
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sessions
    }
}

#[async_trait]
impl SessionRepo for JsonlSessionRepo {
    async fn create(&self, cwd: &Path) -> crate::Result<JsonlSessionStorage> {
        let path = self.session_file_path(cwd);
        let storage = JsonlSessionStorage::open_or_create(path).await?;
        // Header is NOT written here — it's deferred to Session::ensure_persisted
        // which fires on the first append_message(). This avoids creating empty
        // .jsonl files for sessions where no turn is ever taken.
        Ok(storage)
    }

    async fn open(
        &self,
        meta: &SessionMetadata,
    ) -> crate::Result<(JsonlSessionStorage, SessionIndex)> {
        let storage = JsonlSessionStorage::open_or_create(meta.path.clone()).await?;
        let index = storage.build_index().await?;
        Ok((storage, index))
    }

    async fn list(&self, cwd: Option<&Path>) -> crate::Result<Vec<SessionMetadata>> {
        let dirs: Vec<PathBuf> = if let Some(cwd) = cwd {
            let dir = self.session_dir(cwd);
            if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                vec![dir]
            } else {
                return Ok(Vec::new());
            }
        } else {
            self.list_session_dirs().await?
        };

        let mut sessions = Vec::new();
        for dir in &dirs {
            let mut read_dir = match tokio::fs::read_dir(dir).await {
                Ok(r) => r,
                Err(_) => continue,
            };
            while let Some(entry) = read_dir.next_entry().await? {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "jsonl") {
                    if let Some(meta) = Self::read_metadata(&path).await {
                        sessions.push(meta);
                    }
                }
            }
        }

        // Sort by timestamp descending (most recent first).
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(sessions)
    }

    async fn delete(&self, meta: &SessionMetadata) -> crate::Result<()> {
        tokio::fs::remove_file(&meta.path).await?;
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &Path,
    ) -> crate::Result<JsonlSessionStorage> {
        // Open the source session and build its index.
        let source_storage =
            JsonlSessionStorage::open_or_create(source.path.clone()).await?;
        let source_index = source_storage.build_index().await?;

        // Empty fork_entry_id means "fork the entire session" (root to leaf).
        let effective_leaf = if fork_entry_id.is_empty() {
            source_index.leaf_id.as_deref().unwrap_or("")
        } else {
            fork_entry_id
        };

        // Collect entries from root to fork_entry_id (path-to-root reversed).
        let path_to_root = source_index.get_path_to_root(effective_leaf);

        // Create the new session file manually (don't use self.create() because
        // we need to set parent_session in the header).
        let path = self.session_file_path(cwd);
        let new_storage = JsonlSessionStorage::open_or_create(path).await?;

        let header = SessionHeader::forked(
            cwd,
            source.path.clone(),
            source_index.header.provider.clone(),
            source_index.header.model_id.clone(),
            source_index.header.thinking_level.clone(),
        );
        new_storage.write_header(&header).await?;

        // Write the forked entries in root→leaf order.
        for entry in &path_to_root {
            new_storage.append_entry(entry).await?;
        }

        Ok(new_storage)
    }
}

// Internal helpers
impl JsonlSessionRepo {
    /// List all CWD-encoded directories under the base directory.
    async fn list_session_dirs(&self) -> crate::Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        if !tokio::fs::try_exists(&self.base_dir).await.unwrap_or(false) {
            return Ok(dirs);
        }
        let mut read_dir = tokio::fs::read_dir(&self.base_dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        Ok(dirs)
    }

    /// Synchronously list all session metadata, optionally filtered by CWD.
    /// Mirrors the async `SessionRepo::list` but uses std::fs for direct use
    /// in the overlay (avoiding async machinery on each keystroke).
    pub(crate) fn list_sessions_sync(&self, cwd: Option<&Path>) -> Vec<SessionMetadata> {
        let dirs: Vec<PathBuf> = if let Some(cwd) = cwd {
            let dir = self.session_dir(cwd);
            if dir.try_exists().unwrap_or(false) {
                vec![dir]
            } else {
                return Vec::new();
            }
        } else {
            let mut dirs = Vec::new();
            if let Ok(read_dir) = std::fs::read_dir(&self.base_dir) {
                for entry in read_dir.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.is_dir() {
                        dirs.push(path);
                    }
                }
            }
            dirs
        };

        let mut sessions = Vec::new();
        for dir in &dirs {
            sessions.extend(Self::list_sessions_in_dir(dir));
        }
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        sessions
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Create a session via repo.create() and immediately persist it (write header).
    /// repo.create() no longer writes the header — it's deferred to Session's
    /// first append. Tests that need listing/reading must persist explicitly.
    async fn create_persisted(repo: &JsonlSessionRepo, cwd: &Path) -> crate::session::storage::JsonlSessionStorage {
        let storage = repo.create(cwd).await.unwrap();
        let header = crate::session::SessionHeader::new(cwd);
        storage.write_header(&header).await.unwrap();
        storage
    }

    /// Create a temporary repo for testing.
    async fn create_test_repo() -> (JsonlSessionRepo, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "picocode-repo-test-{}-{}",
            std::process::id(),
            rand_name(),
        ));
        let repo = JsonlSessionRepo::new(dir.clone());
        (repo, dir)
    }

    fn rand_name() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        format!("{:09x}", nanos)
    }

    #[tokio::test]
    async fn test_create_and_list() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test/project");

        // Create a session and write header explicitly (repo.create no longer
        // writes the header — it's deferred to the first append).
        let storage = create_persisted(&repo, cwd).await;
        let header = storage.read_header().await.unwrap();
        assert_eq!(header.cwd, "/home/test/project");

        // List sessions for this CWD
        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, header.id);
        assert_eq!(sessions[0].cwd, "/home/test/project");

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_list_all_cwds() {
        let (repo, base_dir) = create_test_repo().await;

        // Create sessions in different CWDs
        let _ = create_persisted(&repo, Path::new("/home/test/a")).await;
        let _ = create_persisted(&repo, Path::new("/home/test/b")).await;

        // List all sessions (no CWD filter)
        let sessions = repo.list(None).await.unwrap();
        assert_eq!(sessions.len(), 2);

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_list_non_existent_cwd_returns_empty() {
        let (repo, base_dir) = create_test_repo().await;
        let sessions = repo.list(Some(Path::new("/nonexistent"))).await.unwrap();
        assert!(sessions.is_empty());
        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_delete_session() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        let storage = create_persisted(&repo, cwd).await;
        let _header_meta = storage.read_header().await.unwrap();

        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);

        repo.delete(&sessions[0]).await.unwrap();

        let sessions_after = repo.list(Some(cwd)).await.unwrap();
        assert!(sessions_after.is_empty());

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_fork_session() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        // Create source session with a few entries.
        // Must write header explicitly since repo.create defers it.
        let source_storage = repo.create(cwd).await.unwrap();
        let header = crate::session::SessionHeader::new(cwd);
        source_storage.write_header(&header).await.unwrap();

        let entry1 = SessionEntry {
            id: "e1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        source_storage.append_entry(&entry1).await.unwrap();

        let entry2 = SessionEntry {
            id: "e2".into(),
            parent_id: Some("e1".into()),
            timestamp: "t2".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[]}),
            },
        };
        source_storage.append_entry(&entry2).await.unwrap();

        // Leaf pointing to e2
        let leaf = SessionEntry::new_leaf(Some("e2".into()), Some("e1".into()));
        source_storage.append_entry(&leaf).await.unwrap();

        // Get source metadata
        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);

        // Fork from e1 (before e2, so we get only the first message)
        let fork_storage = repo.fork(&sessions[0], "e1", cwd).await.unwrap();
        let fork_index = fork_storage.build_index().await.unwrap();

        // Fork should have the header + e1 = 1 entry (header is separate)
        assert_eq!(fork_index.entries.len(), 1);
        assert!(fork_index.entries.contains_key("e1"));

        // Fork should have parent_session set
        assert!(fork_index.header.parent_session.is_some());

        let sessions_after = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions_after.len(), 2); // original + fork

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_metadata_includes_message_count() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        let storage = create_persisted(&repo, cwd).await;

        // Write a couple message entries
        let entry = SessionEntry {
            id: "m1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        storage.append_entry(&entry).await.unwrap();

        let entry = SessionEntry {
            id: "m2".into(),
            parent_id: Some("m1".into()),
            timestamp: "t2".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[]}),
            },
        };
        storage.append_entry(&entry).await.unwrap();

        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].message_count, 2);

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }
}
