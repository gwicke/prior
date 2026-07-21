//! Low-level JSONL session file I/O.
//!
//! Provides [`JsonlSessionStorage`], which wraps a single `.jsonl` file and
//! implements [`SessionStorage`].  The file is append-only; the in-memory index
//! is rebuilt on each open by scanning the full file.

use std::collections::HashMap;
use std::path::PathBuf;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::session::types::*;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Low-level I/O for a single JSONL session file.
#[async_trait]
pub trait SessionStorage: Send + Sync {
    /// Read the session header (first line) without building the full index.
    async fn read_header(&self) -> crate::Result<SessionHeader>;

    /// Build the full in-memory index by scanning every line of the JSONL file.
    async fn build_index(&self) -> crate::Result<SessionIndex>;

    /// Create a new UUIDv7 entry ID.
    fn create_entry_id(&self) -> String;

    /// Append an entry line to the JSONL file and update the in-memory state.
    async fn append_entry(&self, entry: &SessionEntry) -> crate::Result<()>;

    /// Walk the `parent_id` chain from `leaf_id` to root, returning entries in
    /// root→leaf order.
    fn get_path_to_root(&self, index: &SessionIndex, leaf_id: &str) -> Vec<SessionEntry>;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Concrete [`SessionStorage`] that reads/writes a single JSONL file.
///
/// The file is opened with both read and append access on creation.  A
/// `tokio::sync::Mutex` guards the write handle so that concurrent async
/// consumers do not interleave writes.
pub struct JsonlSessionStorage {
    path: PathBuf,
    /// Mutex-protected write handle.  We keep one handle for writes and
    /// re-open a separate read handle for full-scan operations.
    file: tokio::sync::Mutex<tokio::fs::File>,
}

impl JsonlSessionStorage {
    /// Open (or create) a session file at `path`.
    ///
    /// Creates parent directories if they do not exist.  The file is created
    /// lazily — an empty file will lack a header and `build_index` will error.
    pub async fn open_or_create(path: PathBuf) -> crate::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .await?;
        Ok(Self {
            path,
            file: tokio::sync::Mutex::new(file),
        })
    }

    /// The path to the session file.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Write the session header as the first line of the file.
    ///
    /// This must be called before any `append_entry` calls.  The header is
    /// written raw (not as a `SessionEntry`) to match pi's file format.
    pub async fn write_header(&self, header: &SessionHeader) -> crate::Result<()> {
        let mut line = serde_json::to_string(header)?;
        line.push('\n');
        let mut file = self.file.lock().await;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Read the full file content into a string.
    async fn read_all(&self) -> crate::Result<String> {
        Ok(tokio::fs::read_to_string(&self.path).await?)
    }
}

#[async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn read_header(&self) -> crate::Result<SessionHeader> {
        let content = self.read_all().await?;
        let first_line = content
            .lines()
            .next()
            .ok_or_else(|| crate::PicocodeError::Other("Session file is empty".into()))?;
        let header: SessionHeader = serde_json::from_str(first_line)?;
        Ok(header)
    }

    async fn build_index(&self) -> crate::Result<SessionIndex> {
        let content = self.read_all().await?;
        let mut entries: HashMap<String, SessionEntry> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut leaf_id: Option<String> = None;
        let mut session_name: Option<String> = None;

        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Err(crate::PicocodeError::Other("Session file is empty".into()));
        }

        // First line is always the header.
        let header: SessionHeader = serde_json::from_str(lines[0])?;

        // Subsequent lines are entries.
        let line_count = lines.len();
        for (i, line) in lines[1..].iter().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // If this is the last line and looks truncated (no trailing
            // newline in source), skip gracefully.
            let is_last = (i + 2) == line_count; // +2 because we skip index 0
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => {
                    // Track parent-child relationships.
                    if let Some(ref pid) = entry.parent_id {
                        children
                            .entry(pid.clone())
                            .or_default()
                            .push(entry.id.clone());
                    }
                    // Track leaf pointer.
                    if let SessionEntryContent::Leaf { target_id } = &entry.content {
                        leaf_id = target_id.clone();
                    }
                    // Track session name.
                    if let SessionEntryContent::SessionInfo { name } = &entry.content {
                        session_name = name.clone();
                    }
                    entries.insert(entry.id.clone(), entry);
                }
                Err(e) => {
                    if is_last {
                        // Truncated last line — skip it (crash during write).
                        // This is explicitly called out in the design doc:
                        // "On open, skip the last line if it doesn't parse as
                        //  valid JSON."
                        continue;
                    }
                    return Err(crate::PicocodeError::Other(format!(
                        "Failed to parse session entry at line {}: {}",
                        i + 2,
                        e
                    )));
                }
            }
        }

        Ok(SessionIndex {
            header,
            entries,
            leaf_id,
            children,
            session_name,
        })
    }

    fn create_entry_id(&self) -> String {
        Uuid::now_v7().to_string()
    }

    async fn append_entry(&self, entry: &SessionEntry) -> crate::Result<()> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = self.file.lock().await;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    fn get_path_to_root(&self, index: &SessionIndex, leaf_id: &str) -> Vec<SessionEntry> {
        index.get_path_to_root(leaf_id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::session_filename;
    use chrono::Utc;
    use std::path::Path;

    /// Helper: create a JsonlSessionStorage backed by a temp file.
    async fn create_temp_storage() -> (JsonlSessionStorage, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session-test-{}",
            std::process::id()
        ));
        let filename = session_filename(&Utc::now());
        let path = dir.join(&filename);
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        (storage, path)
    }

    #[tokio::test]
    async fn test_write_and_read_header() {
        let (storage, path) = create_temp_storage().await;
        let header = SessionHeader::new(Path::new("/home/test"));
        storage.write_header(&header).await.unwrap();

        let read_back = storage.read_header().await.unwrap();
        assert_eq!(read_back.cwd, "/home/test");
        assert_eq!(read_back.version, 3);
        assert_eq!(read_back.id, header.id);

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_build_index_empty_file_errors() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session-test-{}",
            std::process::id()
        ));
        let path = dir.join("empty.jsonl");
        // Create empty file
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, "").await.unwrap();

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let result = storage.build_index().await;
        assert!(result.is_err(), "expected error for empty file");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_write_entries_and_build_index() {
        let (storage, path) = create_temp_storage().await;

        // Write header
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();

        // Write entries
        let entry1 = SessionEntry {
            id: "e1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
            },
        };
        storage.append_entry(&entry1).await.unwrap();

        let entry2 = SessionEntry {
            id: "e2".into(),
            parent_id: Some("e1".into()),
            timestamp: "2025-01-01T00:00:01.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[{"type":"text","text":"hello"}]}),
            },
        };
        storage.append_entry(&entry2).await.unwrap();

        // Build index
        let index = storage.build_index().await.unwrap();
        assert_eq!(index.header.id, header.id);
        assert_eq!(index.entries.len(), 2);
        assert!(index.entries.contains_key("e1"));
        assert!(index.entries.contains_key("e2"));
        assert!(index.leaf_id.is_none()); // no leaf entry yet

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_leaf_tracking() {
        let (storage, path) = create_temp_storage().await;

        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();

        // Write a message entry
        let msg = SessionEntry {
            id: "m1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        storage.append_entry(&msg).await.unwrap();

        // Write a leaf pointing to m1
        let leaf = SessionEntry::new_leaf(Some("m1".into()), None);
        storage.append_entry(&leaf).await.unwrap();

        // Build index and verify leaf
        let index = storage.build_index().await.unwrap();
        assert_eq!(index.leaf_id, Some("m1".into()));

        // Update leaf
        let leaf2 = SessionEntry::new_leaf(None, None);
        storage.append_entry(&leaf2).await.unwrap();

        let index2 = storage.build_index().await.unwrap();
        assert_eq!(index2.leaf_id, None);

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_session_name_tracking() {
        let (storage, path) = create_temp_storage().await;

        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();

        // Write session name
        let info = SessionEntry::new_session_info(Some("my-session".into()), None);
        storage.append_entry(&info).await.unwrap();

        let index = storage.build_index().await.unwrap();
        assert_eq!(index.session_name, Some("my-session".into()));

        // Overwrite name
        let info2 = SessionEntry::new_session_info(Some("renamed".into()), None);
        storage.append_entry(&info2).await.unwrap();

        let index2 = storage.build_index().await.unwrap();
        assert_eq!(index2.session_name, Some("renamed".into()));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_truncated_last_line_skipped() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session-test-{}",
            std::process::id()
        ));
        let filename = session_filename(&Utc::now());
        let path = dir.join(&filename);
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Write a valid header + one valid entry + a truncated last line.
        let header = SessionHeader::new(Path::new("/test"));
        let mut content = serde_json::to_string(&header).unwrap();
        content.push('\n');

        let entry = SessionEntry {
            id: "e1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        content.push_str(&serde_json::to_string(&entry).unwrap());
        content.push('\n');
        // Truncated line (missing closing brace)
        content.push_str(r#"{"type":"message","id":"partial","parentId"#);

        tokio::fs::write(&path, &content).await.unwrap();

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let index = storage.build_index().await.unwrap();
        // Only the valid entry should be in the index
        assert_eq!(index.entries.len(), 1);
        assert!(index.entries.contains_key("e1"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_path_to_root() {
        let (storage, path) = create_temp_storage().await;

        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();

        // Build a chain: m1 -> m2 -> m3
        let m1 = SessionEntry {
            id: "m1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        storage.append_entry(&m1).await.unwrap();

        let m2 = SessionEntry {
            id: "m2".into(),
            parent_id: Some("m1".into()),
            timestamp: "t2".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"assistant","content":[]}),
            },
        };
        storage.append_entry(&m2).await.unwrap();

        let m3 = SessionEntry {
            id: "m3".into(),
            parent_id: Some("m2".into()),
            timestamp: "t3".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        storage.append_entry(&m3).await.unwrap();

        // Leaf pointing to m3
        let leaf = SessionEntry::new_leaf(Some("m3".into()), Some("m2".into()));
        storage.append_entry(&leaf).await.unwrap();

        let index = storage.build_index().await.unwrap();
        let branch = storage.get_path_to_root(&index, "m3");
        assert_eq!(branch.len(), 3);
        assert_eq!(branch[0].id, "m1");
        assert_eq!(branch[1].id, "m2");
        assert_eq!(branch[2].id, "m3");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
