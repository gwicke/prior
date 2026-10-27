//! Low-level JSONL session file I/O.
//!
//! Provides [`JsonlSessionStorage`], which wraps a single `.jsonl` file and
//! implements [`SessionStorage`].  The file is append-only; the in-memory index
//! is rebuilt on each open by scanning the full file.

use async_trait::async_trait;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::session::types::*;

// ---------------------------------------------------------------------------
// In-place normalisation helpers (called during build_index)
// ---------------------------------------------------------------------------

/// Canonical tool-name mappings.  Every consumer (replay, tree overlay,
/// context building) sees these names; no downstream normalisation needed.
const CANONICAL_NAMES: &[(&str, &str)] = &[
    ("read_file", "read"),
    ("write_file", "write"),
    ("edit_file", "edit"),
    ("grep_text", "grep"),
    ("list_dir", "ls"),
    ("glob_files", "find"),
    ("sed_preview", "sed"),
];

/// Rename argument fields in `args` from old/pi names to canonical names.
fn normalize_args_in_place(name: &str, args: &mut serde_json::Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    match name {
        "bash" => {
            // Pi uses "command"; old prior used "cmd".
            if let Some(cmd) = obj.remove("cmd") {
                obj.insert("command".into(), cmd);
            }
            // Drop a redundant `cd <cwd> && …` prefix, the same rewrite the
            // execution path and the model-facing history apply, so a replayed
            // call reads like the compact form that actually ran. A `cd` to a
            // different directory is left alone.
            if let Some(stripped) =
                obj.get("command")
                    .and_then(|c| c.as_str())
                    .and_then(|command| {
                        std::env::current_dir()
                            .ok()
                            .and_then(|cwd| crate::bash_cmd::strip_cwd_prefix(command, &cwd))
                    })
            {
                obj.insert("command".into(), serde_json::Value::String(stripped));
            }
        }
        "grep" => {
            // Canonical is "pattern". Accept both "pat" (old) and "pattern" (pi).
            if let Some(pat) = obj.remove("pat") {
                obj.insert("pattern".into(), pat);
            }
            obj.remove("glob");
            obj.remove("ignoreCase");
            obj.remove("literal");
            obj.remove("context");
            obj.remove("limit");
        }
        "ls" => {
            obj.remove("limit");
        }
        "find" => {
            if let Some(pat) = obj.remove("pat") {
                obj.insert("pattern".into(), pat);
            }
            obj.remove("limit");
        }
        "sed" => {
            // No legacy field names, but drop the `context` we normalize away
            // so a replayed call does not look like a different edit.
            obj.remove("context");
        }
        "edit" => {
            // Canonical is `edits:[{oldText,newText}]` (pi-compatible). Fold the
            // model's block form (a string), the legacy single `oldText`/`newText`
            // shape, and the very old `old`/`new` shape all into it. Best-effort:
            // an unparseable blob is left as-is so replay degrades gracefully.
            //
            // This is the reload counterpart of `Session::append_message` and
            // `LoggingHook::on_tool_call`; all three must preserve `startLine`
            // via `canonicalize_edit_value` so an anchored edit survives a
            // round trip through disk.
            *args = crate::edit_blocks::canonicalize_edit_value(args);
        }
        _ => {}
    }
}

/// Normalise tool-call entries in a `SessionEntry` in place, so that the
/// in-memory representation uses canonical tool names and argument fields.
/// Operates on the raw `serde_json::Value` inside `Message` entries.
fn normalize_entry_in_place(entry: &mut SessionEntry) {
    let message = match &mut entry.content {
        SessionEntryContent::Message { message } => message,
        _ => return,
    };
    let Some(arr) = message.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for item in arr.iter_mut() {
        if item.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
            continue;
        }
        let name = match item.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let canonical = CANONICAL_NAMES
            .iter()
            .find(|(old, _)| *old == name.as_str())
            .map(|(_, new)| *new)
            .unwrap_or(name.as_str());
        item["name"] = serde_json::Value::String(canonical.to_string());
        if let Some(args) = item.get_mut("arguments") {
            normalize_args_in_place(canonical, args);
        }
    }
}

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
    fn get_path_to_root(&self, index: &SessionIndex, leaf_id: &str) -> Vec<Arc<SessionEntry>>;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Concrete [`SessionStorage`] that reads/writes a single JSONL file.
///
/// The file is opened lazily — the first write call creates the file and
/// parent directories.  A `tokio::sync::Mutex` guards the write handle so
/// that concurrent async consumers do not interleave writes.
pub struct JsonlSessionStorage {
    path: PathBuf,
    /// Mutex-protected optional write handle.  `None` until the first write
    /// operation opens (or creates) the file.
    file: tokio::sync::Mutex<Option<tokio::fs::File>>,
}

impl JsonlSessionStorage {
    /// Open (or create) a session file at `path`.
    ///
    /// The file is NOT opened or created immediately — that happens on the
    /// first write call.  This avoids creating empty files for sessions that
    /// never receive a message.
    pub async fn open_or_create(path: PathBuf) -> crate::Result<Self> {
        // Parent directories ARE created eagerly so that the repo's directory
        // structure is visible for listing even before any writes.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(Self {
            path,
            file: tokio::sync::Mutex::new(None),
        })
    }

    /// Open an existing session file in read-only mode.
    ///
    /// Unlike `open_or_create`, this does not create the file or parent
    /// directories, and does not request write access.  Useful for inspecting
    /// external session stores (e.g. pi sessions on a read-only mount).
    pub async fn open_readonly(path: PathBuf) -> crate::Result<Self> {
        let file = tokio::fs::OpenOptions::new().read(true).open(&path).await?;
        Ok(Self {
            path,
            file: tokio::sync::Mutex::new(Some(file)),
        })
    }

    /// Ensure the write handle is open, creating the file if needed.
    ///
    /// Also checks if the file's inode has changed (e.g., due to atomic rename
    /// from another process or our own `update_session_header`). If the inode
    /// differs from the cached handle's inode, the handle is closed and the
    /// file is reopened.
    async fn ensure_open(
        &self,
    ) -> crate::Result<tokio::sync::MutexGuard<'_, Option<tokio::fs::File>>> {
        let mut guard = self.file.lock().await;

        // Check if file exists and get its current inode
        let current_inode = tokio::fs::metadata(&self.path).await.ok().map(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                m.ino()
            }
            #[cfg(not(unix))]
            {
                None
            }
        });

        let mut should_reopen = guard.is_none();

        if let Some(file) = guard.as_ref() {
            // Compare inode of cached handle with current file
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(meta) = file.metadata().await {
                    should_reopen = current_inode != Some(meta.ino());
                } else {
                    should_reopen = true; // Can't stat handle, reopen to be safe
                }
            }
            #[cfg(not(unix))]
            {
                // On non-unix, can't check inode; rely on explicit invalidation
            }
        }

        if should_reopen {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)
                .await?;
            *guard = Some(file);
        }
        Ok(guard)
    }

    /// Close the cached file handle so the next write reopens the file.
    ///
    /// Call this after an atomic file replacement (e.g., `update_session_header`)
    /// to ensure subsequent appends write to the new file, not the old inode.
    pub async fn invalidate_file_handle(&self) {
        let mut guard = self.file.lock().await;
        *guard = None;
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
        let mut guard = self.ensure_open().await?;
        if let Some(ref mut file) = *guard {
            tokio::io::AsyncWriteExt::write_all(file, line.as_bytes()).await?;
            tokio::io::AsyncWriteExt::flush(file).await?;
        }
        Ok(())
    }

    /// Atomically update the session header while preserving file mtime.
    ///
    /// This reads the full file, replaces the first line (header) with the new
    /// header, writes to a temp file in the same directory, preserves the
    /// original file's mtime (which should correspond to the last message's
    /// timestamp), and atomically renames the temp file to the session file.
    ///
    /// This is used for updating `name` and `cwd` in the header without
    /// rewriting the full session history.
    pub async fn update_session_header(&self, new_header: &SessionHeader) -> crate::Result<()> {
        // Read the full current content
        let content = tokio::fs::read_to_string(&self.path).await?;

        // Split into lines, replace the first line (header)
        let mut lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Err(crate::PriorError::Other(
                "Session file is empty, cannot update header".into(),
            ));
        }

        // Replace header line
        let new_header_line = serde_json::to_string(new_header)?;
        lines[0] = &new_header_line;

        // Write to temp file in same directory (for atomic rename on same filesystem)
        let parent_dir = self.path.parent().ok_or_else(|| {
            crate::PriorError::Other("Session file has no parent directory".into())
        })?;

        let temp_path = parent_dir.join(format!(".{}.tmp", Uuid::now_v7()));

        // Write new content to temp file
        {
            let mut temp_file = fs::File::create(&temp_path)?;
            for line in lines.iter() {
                temp_file.write_all(line.as_bytes())?;
                temp_file.write_all(b"\n")?;
            }
            temp_file.flush()?;
        }

        // Get the original file's mtime to preserve it
        let original_metadata = fs::metadata(&self.path)?;
        let original_mtime = original_metadata.mtime();
        let original_mtime_nsec = original_metadata.mtime_nsec();

        // Atomically rename temp file to session file
        fs::rename(&temp_path, &self.path)?;

        // Restore the original mtime (using utimensat for nanosecond precision)
        let times = [
            libc::timespec {
                tv_sec: original_mtime,
                tv_nsec: original_mtime_nsec,
            },
            libc::timespec {
                tv_sec: original_mtime,
                tv_nsec: original_mtime_nsec,
            },
        ];

        // Use unsafe to call utimensat for precise mtime restoration
        let path_cstr = std::ffi::CString::new(self.path.as_os_str().as_encoded_bytes())
            .map_err(|e| crate::PriorError::Other(format!("Invalid path for CString: {}", e)))?;
        unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                path_cstr.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            );
        }

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
            .ok_or_else(|| crate::PriorError::Other("Session file is empty".into()))?;
        let header: SessionHeader = serde_json::from_str(first_line)?;
        Ok(header)
    }

    async fn build_index(&self) -> crate::Result<SessionIndex> {
        let content = self.read_all().await?;
        let mut entries: HashMap<String, Arc<SessionEntry>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut leaf_id: Option<String> = None;
        let mut session_name: Option<String> = None;
        // Track whether ANY leaf entry was encountered. If at least one leaf
        // entry exists, its value (even `None`) is authoritative. If no leaf
        // entry exists, fall back to the last entry's ID (pi's sessions don't
        // write leaf entries — the leaf is implicitly the last entry).
        let mut saw_leaf_entry = false;
        let mut last_entry_id: Option<String> = None;

        let lines: Vec<&str> = content.lines().collect();
        if lines.is_empty() {
            return Err(crate::PriorError::Other("Session file is empty".into()));
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
                Ok(mut entry) => {
                    // Normalise tool call names and arguments so that every
                    // downstream consumer sees canonical names.  This happens
                    // at the storage boundary (JSONL -> in-memory) so there is
                    // exactly one obvious place for the transformation.
                    normalize_entry_in_place(&mut entry);

                    // Chronological position: the file is append-only, so the
                    // scan order is the display order.
                    order.push(entry.id.clone());

                    // Track parent-child relationships.
                    if let Some(ref pid) = entry.parent_id {
                        children
                            .entry(pid.clone())
                            .or_default()
                            .push(entry.id.clone());
                    }
                    let entry_id = entry.id.clone();

                    // Track leaf pointer.
                    if let SessionEntryContent::Leaf { target_id } = &entry.content {
                        leaf_id = target_id.clone();
                        saw_leaf_entry = true;
                    } else {
                        // Non-leaf entries update the fallback tracker.
                        // We only update last_entry_id for non-leaf entries
                        // so that a trailing leaf entry doesn't poison the
                        // fallback.
                        last_entry_id = Some(entry_id.clone());
                    }
                    // Track session name.
                    if let SessionEntryContent::SessionInfo { name } = &entry.content {
                        session_name = name.clone();
                    }
                    entries.insert(entry.id.clone(), Arc::new(entry));
                }
                Err(e) => {
                    if is_last {
                        // Truncated last line — skip it (crash during write).
                        // This is explicitly called out in the design doc:
                        // "On open, skip the last line if it doesn't parse as
                        //  valid JSON."
                        continue;
                    }
                    return Err(crate::PriorError::Other(format!(
                        "Failed to parse session entry at line {}: {}",
                        i + 2,
                        e
                    )));
                }
            }
        }

        // If no explicit leaf entry was found, fall back to the last non-leaf
        // entry's ID (pi's sessions don't write leaf entries — the leaf is
        // implicitly the last entry in the file).
        if !saw_leaf_entry {
            leaf_id = last_entry_id;
        }

        Ok(SessionIndex {
            header,
            order,
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
        let mut guard = self.ensure_open().await?;
        if let Some(ref mut file) = *guard {
            tokio::io::AsyncWriteExt::write_all(file, line.as_bytes()).await?;
            tokio::io::AsyncWriteExt::flush(file).await?;
        }
        Ok(())
    }

    fn get_path_to_root(&self, index: &SessionIndex, leaf_id: &str) -> Vec<Arc<SessionEntry>> {
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

    #[test]
    fn normalize_edit_args_handles_all_forms() {
        // Block string -> canonical array.
        let mut v = serde_json::json!({
            "path": "src/x.rs",
            "blocks": "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        });
        normalize_args_in_place("edit", &mut v);
        assert_eq!(
            v,
            serde_json::json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );

        // Legacy `old`/`new` -> canonical array.
        let mut v = serde_json::json!({"path": "src/x.rs", "old": "a", "new": "b"});
        normalize_args_in_place("edit", &mut v);
        assert_eq!(
            v,
            serde_json::json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]})
        );

        // Already-array form is unchanged (idempotent).
        let orig =
            serde_json::json!({"path": "src/x.rs", "edits": [{"oldText": "a", "newText": "b"}]});
        let mut v = orig.clone();
        normalize_args_in_place("edit", &mut v);
        assert_eq!(v, orig);

        // Malformed blob is left as-is (graceful degradation).
        let orig = serde_json::json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@END"});
        let mut v = orig.clone();
        normalize_args_in_place("edit", &mut v);
        assert_eq!(v, orig);
    }

    #[test]
    fn normalize_bash_args_strips_a_redundant_cd_prefix() {
        let cwd = std::env::current_dir().unwrap();
        // A `cd` into the working directory is dropped, as on the live path.
        let mut v = serde_json::json!({
            "command": format!("cd {} && cargo test", cwd.display())
        });
        normalize_args_in_place("bash", &mut v);
        assert_eq!(v, serde_json::json!({"command": "cargo test"}));

        // A `cd` elsewhere is a real directory change and survives replay.
        let mut v = serde_json::json!({"command": "cd /tmp && ls"});
        normalize_args_in_place("bash", &mut v);
        assert_eq!(v, serde_json::json!({"command": "cd /tmp && ls"}));
    }

    /// Helper: create a JsonlSessionStorage backed by a temp file.
    async fn create_temp_storage() -> (tempfile::TempDir, JsonlSessionStorage, PathBuf) {
        // A `TempDir` (returned first so the caller keeps it alive past the
        // storage, which re-opens by path). Removed on drop instead of leaking
        // an empty directory.
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let filename = session_filename(&Utc::now());
        let path = dir.join(&filename);
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        (dir_guard, storage, path)
    }

    #[tokio::test]
    async fn test_write_and_read_header() {
        let (_guard, storage, path) = create_temp_storage().await;
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
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
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
        let (_guard, storage, path) = create_temp_storage().await;

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
        // Leaf falls back to the last entry's ID when no explicit leaf entry
        // exists (pi-compatible behaviour).
        assert_eq!(index.leaf_id, Some("e2".into()));

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_leaf_tracking() {
        let (_guard, storage, path) = create_temp_storage().await;

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
        let (_guard, storage, path) = create_temp_storage().await;

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
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
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
        let (_guard, storage, path) = create_temp_storage().await;

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

    // ------------------------------------------------------------------
    // 1.6 Integration test: pi wire format compatibility
    // ------------------------------------------------------------------

    /// A manually crafted pi-format JSONL session string. Uses pi's exact
    /// wire format: camelCase, `type`-tagged entries, `"type":"session"`
    /// header with `"version":3`.
    const PI_FORMAT_JSONL: &str = r#"{"type":"session","version":3,"id":"pi-session-1","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/home/test"}
{"type":"message","id":"m1","parentId":null,"timestamp":"2025-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"thinking_level_change","id":"m2","parentId":"m1","timestamp":"2025-01-01T00:00:02.000Z","thinkingLevel":"high"}
{"type":"model_change","id":"m3","parentId":"m2","timestamp":"2025-01-01T00:00:03.000Z","provider":"anthropic","modelId":"claude-3-5-sonnet"}
{"type":"message","id":"m4","parentId":"m3","timestamp":"2025-01-01T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi there"},{"type":"thinking","thinking":"step by step"}]}}
{"type":"leaf","id":"l1","parentId":"m4","timestamp":"2025-01-01T00:00:05.000Z","targetId":"m4"}"#;

    #[tokio::test]
    async fn test_read_pi_format_jsonl() {
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let path = dir.join("pi-format.jsonl");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(&path, PI_FORMAT_JSONL).await.unwrap();

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let index = storage.build_index().await.unwrap();

        // Verify header
        assert_eq!(index.header.kind, "session");
        assert_eq!(index.header.version, 3);
        assert_eq!(index.header.id, "pi-session-1");
        assert_eq!(index.header.cwd, "/home/test");

        // Verify entries
        assert_eq!(index.entries.len(), 5);
        assert!(index.entries.contains_key("m1"));
        assert!(index.entries.contains_key("m4"));

        // Verify message entry content (pi-format AgentMessage)
        let m1 = &index.entries["m1"];
        match &m1.content {
            SessionEntryContent::Message { message } => {
                assert_eq!(message["role"], "user");
                assert_eq!(message["content"][0]["type"], "text");
                assert_eq!(message["content"][0]["text"], "hello");
            }
            _ => panic!("expected Message variant"),
        }

        // Verify thinking_level_change
        let m2 = &index.entries["m2"];
        match &m2.content {
            SessionEntryContent::ThinkingLevelChange { thinking_level } => {
                assert_eq!(thinking_level, "high");
            }
            _ => panic!("expected ThinkingLevelChange"),
        }

        // Verify model_change
        let m3 = &index.entries["m3"];
        match &m3.content {
            SessionEntryContent::ModelChange { provider, model_id } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-3-5-sonnet");
            }
            _ => panic!("expected ModelChange"),
        }

        // Verify assistant message with reasoning
        let m4 = &index.entries["m4"];
        match &m4.content {
            SessionEntryContent::Message { message } => {
                assert_eq!(message["role"], "assistant");
                assert_eq!(message["content"][1]["type"], "thinking");
                assert_eq!(message["content"][1]["thinking"], "step by step");
            }
            _ => panic!("expected Message variant"),
        }

        // Verify leaf
        let l1 = &index.entries["l1"];
        match &l1.content {
            SessionEntryContent::Leaf { target_id } => {
                assert_eq!(target_id.as_deref(), Some("m4"));
            }
            _ => panic!("expected Leaf variant"),
        }
        assert_eq!(index.leaf_id, Some("m4".into()));

        // Verify parent-child relationships
        assert_eq!(index.children.get("m1").unwrap(), &vec!["m2"]);
        assert_eq!(index.children.get("m3").unwrap(), &vec!["m4"]);

        // Verify path-to-root
        let branch = storage.get_path_to_root(&index, "m4");
        assert_eq!(branch.len(), 4); // m1 -> m2 -> m3 -> m4 (4 entries, leaf is separate)
        assert_eq!(branch[0].id, "m1");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_prior_output_is_pi_compatible() {
        let (_guard, storage, path) = create_temp_storage().await;

        // Write header
        let header = SessionHeader::new(std::path::Path::new("/test"));
        storage.write_header(&header).await.unwrap();

        // Write an entry
        let entry = SessionEntry {
            id: "e1".into(),
            parent_id: Some("p0".into()),
            timestamp: "t1".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[{"type":"text","text":"hello"}]}),
            },
        };
        storage.append_entry(&entry).await.unwrap();

        // Read back raw content and verify no snake_case leaked
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2); // header + entry

        // Verify header is pi-format (camelCase)
        assert!(lines[0].contains("\"type\":\"session\""));
        assert!(lines[0].contains("\"version\":3"));
        assert!(lines[0].contains("\"cwd\":\"/test\""));
        // Must NOT contain snake_case fields
        assert!(!lines[0].contains("parent_session"));
        assert!(!lines[0].contains("kind"));

        // Verify entry is pi-format (camelCase, type-tagged)
        assert!(lines[1].contains("\"type\":\"message\""));
        assert!(lines[1].contains("\"parentId\":\"p0\""));
        assert!(lines[1].contains("\"role\":\"user\""));
        // Must NOT contain snake_case
        assert!(!lines[1].contains("parent_id"));
        assert!(!lines[1].contains("thinking_level"));

        let _ = tokio::fs::remove_file(&path).await;
    }
}
