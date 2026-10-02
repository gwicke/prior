//! Repository managing multiple JSONL session files in a directory tree.
//!
//! Provides [`JsonlSessionRepo`] which mirrors pi's `JsonlSessionRepo`:
//! sessions are stored under `base_dir/<cwd-encoded>/<timestamp>_<uuid>.jsonl`.
//! The CWD-based directory structure groups sessions by working directory.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use memchr::memmem;
use memmap2::Mmap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

use crate::session::storage::*;
use crate::session::types::*;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Fork mode: controls which entries are copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMode {
    /// Full tree fork: copy all entries byte-for-byte, only rewrite header.
    /// Preserves all branches, compactions, labels, and the original leaf.
    /// This is the default for `/session <id> fork`.
    FullTree,
    /// Active path only: copy only the linear path from root to fork_entry_id.
    /// Used by `/tree` when forking at a specific entry.
    ActivePath,
}

/// Repository managing multiple sessions in a directory.
#[async_trait]
pub trait SessionRepo: Send + Sync {
    /// Create a new session for the given CWD.  Writes the session header and
    /// returns a storage handle.
    async fn create(&self, cwd: &Path) -> crate::Result<JsonlSessionStorage>;

    /// Open an existing session by metadata, building its full in-memory index.
    async fn open(
        &self,
        meta: &SessionMetadata,
    ) -> crate::Result<(JsonlSessionStorage, SessionIndex)>;

    /// List all sessions for a given CWD (or all CWDs if `None`).
    async fn list(&self, cwd: Option<&Path>) -> crate::Result<Vec<SessionMetadata>>;

    /// Delete a session file.
    async fn delete(&self, meta: &SessionMetadata) -> crate::Result<()>;

    /// Fork a session from a specific entry.
    ///
    /// - `fork_entry_id` empty → FullTree fork (entire session, all branches)
    /// - `fork_entry_id` set + `ForkMode::ActivePath` → path-only fork (for /tree)
    /// - `fork_entry_id` set + `ForkMode::FullTree` → full tree + new Leaf at fork_entry_id
    async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &Path,
        mode: ForkMode,
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
    /// Shared session metadata cache for the sync listing path (overlay).
    /// Clone shares the same `Arc`, so all copies see updates.
    cache: Arc<Mutex<Option<SessionMetaCache>>>,
}

// ---------------------------------------------------------------------------
// Session metadata cache (for sync listing path)
// ---------------------------------------------------------------------------

/// Cached metadata for one session directory (one encoded-CWD subdirectory).
#[derive(Clone)]
struct CachedDir {
    /// Per-file mtime for each `.jsonl` in this directory.
    file_mtimes: HashMap<PathBuf, SystemTime>,
    /// Sorted sessions (most recent first, by `last_prompt_timestamp`).
    sessions: Vec<SessionMetadata>,
}

/// Lightweight cache for session metadata listings.
///
/// Validates freshness by comparing directory and file mtimes.
/// A full re-scan is triggered at most once every 2 seconds.
struct SessionMetaCache {
    /// When the last validation completed (monotonic clock).
    last_validated_at: Instant,
    /// Per-directory cached state, keyed by the resolved directory path.
    dirs: HashMap<PathBuf, CachedDir>,
}

impl SessionMetaCache {
    fn new() -> Self {
        Self {
            // Start far enough in the past so the first call always validates.
            last_validated_at: Instant::now() - Duration::from_secs(10),
            dirs: HashMap::new(),
        }
    }

    /// Return cached sessions for the given directories, merged and sorted.
    fn get_sessions(&self, dirs: &[PathBuf]) -> Vec<SessionMetadata> {
        let mut all = Vec::new();
        for dir in dirs {
            if let Some(d) = self.dirs.get(dir) {
                all.extend(d.sessions.iter().cloned());
            }
        }
        // Sort by file mtime (most recent activity first),
        // falling back to session creation timestamp.
        all.sort_by(|a, b| {
            let a_ts = a
                .file_mtime
                .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let b_ts = b
                .file_mtime
                .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            b_ts.cmp(&a_ts)
        });
        all
    }
}

// ---------------------------------------------------------------------------
// JsonlSessionRepo implementation
// ---------------------------------------------------------------------------

impl JsonlSessionRepo {
    /// Create a new repo rooted at `base_dir` (typically
    /// `~/.picocode/sessions/`).
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    /// The absolute base directory for session storage.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Invalidate the metadata cache so the next listing re-reads from disk.
    pub fn invalidate_cache(&self) {
        if let Ok(mut guard) = self.cache.lock() {
            *guard = None;
        }
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

    /// Quick-read metadata from a single JSONL file (full scan for compat).
    /// Does NOT populate `last_prompt_text` — use `read_metadata_detail`
    /// for the full detail (called once when viewing a single session).
    /// This is kept for backward compatibility with tests.
    #[allow(dead_code)]
    async fn read_metadata(path: &Path) -> Option<SessionMetadata> {
        let content = tokio::fs::read_to_string(path).await.ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        // Count message entries, extract the first user prompt, and track name.
        let mut message_count = 0usize;
        let mut user_message_count = 0usize;
        let mut first_prompt_preview = None::<String>;
        let mut last_prompt_timestamp: Option<String> = None;
        let mut name: Option<String> = None;

        for line in content.lines().skip(1) {
            if line.contains("\"type\":\"message\"") {
                message_count += 1;
                if line.contains("\"role\":\"user\"") {
                    user_message_count += 1;
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

        let metadata = tokio::fs::metadata(path).await.ok()?;
        let file_size = metadata.len();
        let file_mtime = metadata.modified().ok();

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            first_prompt_preview,
            file_size,
            file_mtime,
            detail: Some(SessionDetail {
                message_count,
                user_message_count,
                token_count: None,
                last_prompt_text: None,
                last_prompt_timestamp,
                compaction_summary: None,
            }),
        })
    }
    /// Quick-read metadata from a single JSONL file (header only) — sync version.
    /// Does NOT populate `last_prompt_text` — use `read_metadata_detail`
    /// for the full detail (called once when viewing a single session).
    /// Test-only: the eval path uses the async `read_metadata`.
    #[cfg(test)]
    fn read_metadata_sync(path: &Path) -> Option<SessionMetadata> {
        let content = std::fs::read_to_string(path).ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        let mut message_count = 0usize;
        let mut user_message_count = 0usize;
        let mut first_prompt_preview = None::<String>;
        let mut last_prompt_timestamp: Option<String> = None;
        let mut name: Option<String> = None;
        let mut compaction_summary: Option<String> = None;

        for line in content.lines().skip(1) {
            if line.contains("\"type\":\"message\"") {
                message_count += 1;
                if line.contains("\"role\":\"user\"") {
                    user_message_count += 1;
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
            } else if line.contains("\"compaction\"") {
                // Flexible match for compaction type (handles "type":"compaction" or "type": "compaction")
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if val.get("type").and_then(|v| v.as_str()) == Some("compaction") {
                        if let Some(s) = val.get("summary").and_then(|v| v.as_str()) {
                            if !s.is_empty() {
                                compaction_summary = Some(s.to_string());
                            }
                        }
                    }
                }
            }
        }

        // Get name: if header has the `name` field (even empty), use it.
        // Empty string in header -> None in metadata.
        let name = if let Some(hname) = header.name {
            if hname.is_empty() {
                None
            } else {
                Some(hname)
            }
        } else {
            name // from session_info scan above
        };

        let metadata = std::fs::metadata(path).ok()?;
        let file_size = metadata.len();
        let file_mtime = metadata.modified().ok();

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            first_prompt_preview,
            file_size,
            file_mtime,
            detail: Some(SessionDetail {
                message_count,
                user_message_count,
                token_count: None,
                last_prompt_text: None,
                last_prompt_timestamp,
                compaction_summary,
            }),
        })
    }

    /// Efficient metadata read for session listing.
    ///
    /// Reads only the header and first user message line. Avoids scanning
    /// the full file. Gets `name` from header if present, otherwise falls
    /// back to memmap2 + reverse search for the last `session_info` entry.
    /// Populates `file_size` and `file_mtime` from filesystem metadata.
    /// Does NOT populate `detail` (message counts, last_prompt_text, etc.),
    /// which are only needed for the detail view.
    pub async fn read_metadata_listing(path: &Path) -> Option<SessionMetadata> {
        // Get file metadata (size, mtime) - fast syscall
        let metadata = tokio::fs::metadata(path).await.ok()?;
        let file_size = metadata.len();
        let file_mtime = metadata.modified().ok();

        // Read header + first few lines efficiently using tokio BufReader
        let file = tokio::fs::File::open(path).await.ok()?;
        let mut reader = tokio::io::BufReader::new(file);
        let mut header_line = String::new();
        if tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut header_line)
            .await
            .is_err()
            || header_line.is_empty()
        {
            return None;
        }
        let header: SessionHeader = serde_json::from_str(header_line.trim()).ok()?;

        // Read lines until first user message found (for first_prompt_preview)
        let mut first_prompt_preview = None::<String>;
        let mut _last_prompt_timestamp: Option<String> = None;
        let mut lines_read = 0;
        let mut line = String::new();

        while lines_read < 50 {
            // Limit lines read for listing
            line.clear();
            if tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .is_err()
                || line.is_empty()
            {
                break;
            }
            lines_read += 1;

            if line.contains("\"type\":\"message\"") && line.contains("\"role\":\"user\"") {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    let text = val
                        .pointer("/message/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let ts = val.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        if first_prompt_preview.is_none() {
                            first_prompt_preview = Some(text.chars().collect());
                        }
                        if !ts.is_empty() {
                            _last_prompt_timestamp = Some(ts.to_string());
                        }
                        break; // Found first user message, stop reading
                    }
                }
            }
        }

        // Get name: if header has the `name` field (even empty), use it.
        // Only fall back to reverse search for pre-migration sessions without the field.
        // Empty string in header -> None in metadata (UI doesn't need to check for empty string).
        let name = if let Some(hname) = header.name {
            if hname.is_empty() {
                None
            } else {
                Some(hname)
            }
        } else {
            Self::find_latest_session_info_name(path).await
        };

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            first_prompt_preview,
            file_size,
            file_mtime,
            detail: None,
        })
    }

    /// Ensure the session header has a `name` field populated.
    ///
    /// If the header already has a name, does nothing. Otherwise, searches for
    /// the latest session_info entry and writes the name to the header atomically
    /// (preserving mtime). This migrates existing sessions to include the name
    /// in the header for fast listing.
    ///
    /// Always writes the header with a `name` field (empty string if no name found)
    /// to avoid repeated searches for session_info entries on startup.
    async fn ensure_header_has_name(path: PathBuf, header: &SessionHeader, name: Option<String>) {
        // If header already has the same name, nothing to do
        let new_name = name.or_else(|| Some(String::new()));
        if header.name == new_name {
            return;
        }
        // Update the header with the name (empty string if none found)
        let mut new_header = header.clone();
        new_header.name = new_name;
        // Write to temp file and atomically rename (preserving mtime)
        let content = tokio::fs::read_to_string(&path).await.ok();
        if let Some(content) = content {
            let mut lines: Vec<&str> = content.lines().collect();
            if !lines.is_empty() {
                let new_header_line = serde_json::to_string(&new_header).ok();
                if let Some(new_header_line) = new_header_line {
                    lines[0] = &new_header_line;
                    if let Some(parent) = path.parent() {
                        let temp_path = parent.join(format!(".{}.tmp", uuid::Uuid::now_v7()));
                        if tokio::fs::write(&temp_path, lines.join("\n") + "\n")
                            .await
                            .is_ok()
                        {
                            // Preserve mtime
                            let mtime = tokio::fs::metadata(&path)
                                .await
                                .ok()
                                .and_then(|m| m.modified().ok());
                            if tokio::fs::rename(&temp_path, &path).await.is_ok() {
                                if let Some(mtime) = mtime {
                                    let path_for_blocking = path.clone();
                                    let _ = tokio::task::spawn_blocking(move || {
                                        let times = [
                                            libc::timespec {
                                                tv_sec: mtime
                                                    .duration_since(UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_secs()
                                                    as i64,
                                                tv_nsec: 0,
                                            },
                                            libc::timespec {
                                                tv_sec: mtime
                                                    .duration_since(UNIX_EPOCH)
                                                    .unwrap_or_default()
                                                    .as_secs()
                                                    as i64,
                                                tv_nsec: 0,
                                            },
                                        ];
                                        let path_cstr = std::ffi::CString::new(
                                            path_for_blocking.as_os_str().as_encoded_bytes(),
                                        )
                                        .ok()?;
                                        unsafe {
                                            libc::utimensat(
                                                libc::AT_FDCWD,
                                                path_cstr.as_ptr(),
                                                times.as_ptr(),
                                                libc::AT_SYMLINK_NOFOLLOW,
                                            );
                                        }
                                        Some(())
                                    })
                                    .await
                                    .ok();
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Find the latest session_info name using memmap2 + reverse search.
    ///
    /// Memory-maps the file and searches backwards for `"type":"session_info"`
    /// to extract the name from the most recent session_info entry.
    async fn find_latest_session_info_name(path: &Path) -> Option<String> {
        use std::fs::File;

        // Use blocking IO for mmap - it's fast and we're already in async context
        // with a small file section. Could use tokio::task::spawn_blocking if needed.
        let file = File::open(path).ok()?;
        let mmap = unsafe { Mmap::map(&file).ok()? };
        if mmap.is_empty() {
            return None;
        }

        // Search backwards for "type\":\"session_info\"
        let needle = b"\"type\":\"session_info\"";
        let finder = memmem::FinderRev::new(needle);
        let pos = finder.rfind(&mmap)?;

        // Find the line boundaries (newline before and after)
        let line_start = mmap[..pos]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let line_end = mmap[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| pos + p)
            .unwrap_or(mmap.len());

        let line_bytes = &mmap[line_start..line_end];
        let line_str = std::str::from_utf8(line_bytes).ok()?;

        // Parse the line and extract name
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line_str) {
            if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    return Some(n.to_string());
                }
            }
        }
        None
    }

    /// Extract the timestamp of the last entry in the session file.
    ///
    /// Scans backwards from the end of the file to find the last valid JSON line
    /// and extracts its `timestamp` field. Returns the timestamp as a SystemTime
    /// for use in mtime preservation during fork operations.
    fn extract_last_entry_timestamp(path: &Path) -> Option<SystemTime> {
        let content = std::fs::read_to_string(path).ok()?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= 1 {
            return None; // Only header
        }

        // Scan backwards from the end to find the last valid entry with a timestamp
        for line in lines.iter().rev().skip(1) {
            // Skip header (first line)
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(ts_str) = val.get("timestamp").and_then(|v| v.as_str()) {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
                        let system_time: SystemTime = dt.into();
                        return Some(system_time);
                    }
                }
            }
        }
        None
    }

    /// Full metadata read including `last_prompt_text`.
    ///
    /// Unlike `read_metadata_sync`, this loads the last user message text
    /// in full. Only call this when viewing a single session's detail
    /// (one file read, not N per keystroke).
    pub(crate) fn read_metadata_detail(path: &Path) -> Option<SessionMetadata> {
        let content = std::fs::read_to_string(path).ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        let mut message_count = 0usize;
        let mut user_message_count = 0usize;
        let mut first_prompt_preview = None::<String>;
        let mut last_prompt_text: Option<String> = None;
        let mut last_prompt_timestamp: Option<String> = None;
        let mut name: Option<String> = None;
        let mut compaction_summary: Option<String> = None;

        for line in content.lines().skip(1) {
            if line.contains("\"type\":\"message\"") {
                message_count += 1;
                if line.contains("\"role\":\"user\"") {
                    user_message_count += 1;
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
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                        if !n.is_empty() {
                            name = Some(n.to_string());
                        }
                    }
                }
            } else if line.contains("\"compaction\"") {
                // Flexible match for compaction type (handles "type":"compaction" or "type": "compaction")
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if val.get("type").and_then(|v| v.as_str()) == Some("compaction") {
                        if let Some(s) = val.get("summary").and_then(|v| v.as_str()) {
                            if !s.is_empty() {
                                compaction_summary = Some(s.to_string());
                            }
                        }
                    }
                }
            }
        }

        let metadata = std::fs::metadata(path).ok()?;
        let file_size = metadata.len();
        let file_mtime = metadata.modified().ok();

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name,
            parent_session: header.parent_session,
            first_prompt_preview,
            file_size,
            file_mtime,
            detail: Some(SessionDetail {
                message_count,
                user_message_count,
                token_count: None,
                last_prompt_text,
                last_prompt_timestamp,
                compaction_summary,
            }),
        })
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
                if path.extension().is_some_and(|e| e == "jsonl") {
                    if let Some(meta) = Self::read_metadata_listing(&path).await {
                        // Migrate: ensure header has name if we found one from session_info
                        // We need to re-read header to get the current state
                        let content = tokio::fs::read_to_string(&path).await.ok();
                        if let Some(content) = content {
                            if let Some(first_line) = content.lines().next() {
                                if let Ok(header) =
                                    serde_json::from_str::<SessionHeader>(first_line)
                                {
                                    Self::ensure_header_has_name(
                                        path.clone(),
                                        &header,
                                        meta.name.clone(),
                                    )
                                    .await;
                                }
                            }
                        }
                        sessions.push(meta);
                    }
                }
            }
        }

        // Sort by file mtime (most recent activity first),
        // falling back to session creation timestamp.
        sessions.sort_by(|a, b| {
            let a_ts = a
                .file_mtime
                .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let b_ts = b
                .file_mtime
                .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            b_ts.cmp(&a_ts)
        });
        Ok(sessions)
    }

    async fn delete(&self, meta: &SessionMetadata) -> crate::Result<()> {
        tokio::fs::remove_file(&meta.path).await?;
        // Drop the listing cache immediately: the `/session` overlay reads
        // through `list_sessions_overlay` (2s mtime-debounce cache), which
        // would otherwise keep showing the deleted session as a ghost for
        // up to 2 seconds — looking like the delete never happened.
        self.invalidate_cache();
        Ok(())
    }

    async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &Path,
        mode: ForkMode,
    ) -> crate::Result<JsonlSessionStorage> {
        // Read the entire source file content.
        let source_content = tokio::fs::read_to_string(&source.path).await?;
        let lines: Vec<&str> = source_content.lines().collect();
        if lines.is_empty() {
            return Err(crate::PicocodeError::Other(
                "Source session file is empty".into(),
            ));
        }

        // Parse the original header to carry forward metadata.
        let orig_header: SessionHeader = serde_json::from_str(lines[0])?;

        let new_path = self.session_file_path(cwd);
        let new_path_for_mtime = new_path.clone();
        // The target cwd's session dir may not exist yet (first fork into
        // a fresh directory) — `File::create` below would fail with ENOENT.
        if let Some(parent) = new_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        match mode {
            ForkMode::FullTree => {
                // Full tree fork: copy all entries byte-for-byte, only rewrite header.
                // If fork_entry_id is set, we also append a new Leaf pointing to it.
                let new_header = SessionHeader::forked(
                    cwd,
                    source.path.clone(),
                    orig_header.provider.clone(),
                    orig_header.model_id.clone(),
                    orig_header.thinking_level.clone(),
                );
                let new_header_line = serde_json::to_string(&new_header)?;

                let mut new_file = tokio::fs::File::create(&new_path).await?;
                new_file.write_all(new_header_line.as_bytes()).await?;
                new_file.write_all(b"\n").await?;

                // Copy ALL remaining lines unchanged (includes valid leaf entry).
                for line in &lines[1..] {
                    if !line.trim().is_empty() {
                        new_file.write_all(line.as_bytes()).await?;
                        new_file.write_all(b"\n").await?;
                    }
                }

                // If a specific fork_entry_id was given, append a new Leaf pointing to it.
                // This makes the forked session start at that entry explicitly.
                if !fork_entry_id.is_empty() {
                    let leaf = SessionEntry::new_leaf(Some(fork_entry_id.to_string()), None);
                    let leaf_line = serde_json::to_string(&leaf)?;
                    new_file.write_all(leaf_line.as_bytes()).await?;
                    new_file.write_all(b"\n").await?;
                }

                new_file.flush().await?;

                // Preserve mtime from source file's last entry timestamp (or file mtime)
                let mtime = Self::extract_last_entry_timestamp(&source.path);
                let mtime = if let Some(mtime) = mtime {
                    Some(mtime)
                } else {
                    tokio::fs::metadata(&source.path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                };
                if let Some(mtime) = mtime {
                    if let Ok(mut _file) = tokio::fs::File::options()
                        .write(true)
                        .open(&new_path_for_mtime)
                        .await
                    {
                        let new_path_clone = new_path_for_mtime.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let times = [
                                libc::timespec {
                                    tv_sec: mtime
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64,
                                    tv_nsec: 0,
                                },
                                libc::timespec {
                                    tv_sec: mtime
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64,
                                    tv_nsec: 0,
                                },
                            ];
                            let path_cstr = std::ffi::CString::new(
                                new_path_clone.as_os_str().as_encoded_bytes(),
                            )
                            .ok()?;
                            unsafe {
                                libc::utimensat(
                                    libc::AT_FDCWD,
                                    path_cstr.as_ptr(),
                                    times.as_ptr(),
                                    libc::AT_SYMLINK_NOFOLLOW,
                                );
                            }
                            Some(())
                        })
                        .await
                        .ok();
                    }
                }

                let new_storage = JsonlSessionStorage::open_or_create(new_path).await?;
                // New file on disk — drop the sync-listing cache so the
                // `/session` overlay lists it immediately (see `delete`).
                self.invalidate_cache();
                Ok(new_storage)
            }
            ForkMode::ActivePath => {
                // Active path only: build index and extract path-to-root.
                let source_storage =
                    JsonlSessionStorage::open_or_create(source.path.clone()).await?;
                let source_index = source_storage.build_index().await?;

                let effective_leaf = if fork_entry_id.is_empty() {
                    source_index.leaf_id.as_deref().unwrap_or("")
                } else {
                    fork_entry_id
                };

                let path_to_root = source_index.get_path_to_root(effective_leaf);

                // Clone new_path for later mtime preservation
                let new_path_for_mtime = new_path.clone();
                let new_storage = JsonlSessionStorage::open_or_create(new_path).await?;

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

                // Append a new Leaf pointing to the fork entry.
                let leaf = SessionEntry::new_leaf(Some(effective_leaf.to_string()), None);
                new_storage.append_entry(&leaf).await?;

                // Preserve mtime from source file's last entry timestamp (or file mtime)
                let mtime = Self::extract_last_entry_timestamp(&source.path);
                let mtime = if let Some(mtime) = mtime {
                    Some(mtime)
                } else {
                    tokio::fs::metadata(&source.path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                };
                if let Some(mtime) = mtime {
                    if let Ok(mut _file) = tokio::fs::File::options()
                        .write(true)
                        .open(&new_path_for_mtime)
                        .await
                    {
                        let new_path_clone = new_path_for_mtime.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let times = [
                                libc::timespec {
                                    tv_sec: mtime
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64,
                                    tv_nsec: 0,
                                },
                                libc::timespec {
                                    tv_sec: mtime
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64,
                                    tv_nsec: 0,
                                },
                            ];
                            let path_cstr = std::ffi::CString::new(
                                new_path_clone.as_os_str().as_encoded_bytes(),
                            )
                            .ok()?;
                            unsafe {
                                libc::utimensat(
                                    libc::AT_FDCWD,
                                    path_cstr.as_ptr(),
                                    times.as_ptr(),
                                    libc::AT_SYMLINK_NOFOLLOW,
                                );
                            }
                            Some(())
                        })
                        .await
                        .ok();
                    }
                }

                // New file on disk — drop the sync-listing cache so the
                // `/session` overlay lists it immediately (see `delete`).
                self.invalidate_cache();
                Ok(new_storage)
            }
        }
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

    /// Test-support sync listing (optionally filtered by CWD). Production
    /// eval uses [`Self::list_sessions_overlay`] — the same
    /// 2-second-debounced, mtime-validated cache — so tests and prod see
    /// identical results.
    #[cfg(test)]
    pub(crate) fn list_sessions_sync(&self, cwd: Option<&Path>) -> Vec<SessionMetadata> {
        // Resolve directories to scan.
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

        if dirs.is_empty() {
            return Vec::new();
        }

        // Lock cache and check debounce.
        let mut cache_guard = self.cache.lock().unwrap();
        if let Some(ref cache) = *cache_guard {
            if cache.last_validated_at.elapsed() < Duration::from_secs(2) {
                return cache.get_sessions(&dirs);
            }
        }

        // Validate (or initialise) the cache.
        let cache: &mut SessionMetaCache = cache_guard.get_or_insert_with(SessionMetaCache::new);
        cache.last_validated_at = Instant::now();

        for dir in &dirs {
            // Always re-scan files; directory mtime doesn't change when existing
            // files are modified (only on create/delete). We must stat each file
            // to detect content changes.
            let mut sessions = Vec::new();
            let mut file_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();

            if let Ok(read_dir) = std::fs::read_dir(dir) {
                for entry in read_dir.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if !path.extension().is_some_and(|e| e == "jsonl") {
                        continue;
                    }
                    let file_mtime = entry.metadata().ok().and_then(|m| m.modified().ok());

                    // Reuse cached metadata if file mtime matches.
                    if let Some(file_mt) = file_mtime {
                        let cached_mt = cache
                            .dirs
                            .get(dir)
                            .and_then(|c| c.file_mtimes.get(&path).copied());
                        if Some(file_mt) == cached_mt {
                            if let Some(old) = cache
                                .dirs
                                .get(dir)
                                .and_then(|c| c.sessions.iter().find(|s| s.path == path))
                            {
                                sessions.push(old.clone());
                                file_mtimes.insert(path, file_mt);
                                continue;
                            }
                        }
                    }

                    // Cache miss or changed file: re-read metadata.
                    if let Some(meta) = Self::read_metadata_sync(&path) {
                        if let Some(mt) = file_mtime {
                            file_mtimes.insert(path, mt);
                        }
                        sessions.push(meta);
                    }
                }
            }

            sessions.sort_by(|a, b| {
                // Sort by file mtime (most recent activity first),
                // falling back to session creation timestamp.
                let a_ts = a
                    .file_mtime
                    .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let b_ts = b
                    .file_mtime
                    .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                b_ts.cmp(&a_ts)
            });
            cache.dirs.insert(
                dir.clone(),
                CachedDir {
                    file_mtimes,
                    sessions,
                },
            );
        }

        cache.get_sessions(&dirs)
    }

    /// Async overlay listing for the eval path: consults the shared
    /// 2-second-debounced cache first (warm = zero IO, pure memory); a
    /// stale cache triggers a `tokio::fs` scan so the eval task never
    /// blocks a runtime worker on std::fs. Mirrors `list_sessions_sync`
    /// semantics (per-file mtime reuse). The cache lock is never held
    /// across an await (lock rule 3).
    pub async fn list_sessions_overlay(&self, cwd: Option<&Path>) -> Vec<SessionMetadata> {
        // Resolve directories (async).
        let dirs: Vec<PathBuf> = if let Some(cwd) = cwd {
            let dir = self.session_dir(cwd);
            if tokio::fs::try_exists(&dir).await.unwrap_or(false) {
                vec![dir]
            } else {
                return Vec::new();
            }
        } else {
            let mut dirs = Vec::new();
            if let Ok(mut read_dir) = tokio::fs::read_dir(&self.base_dir).await {
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    let path = entry.path();
                    if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                        dirs.push(path);
                    }
                }
            }
            dirs
        };
        if dirs.is_empty() {
            return Vec::new();
        }

        // Debounce fast path — cached, zero IO.
        {
            let cache = self.cache.lock().unwrap();
            if let Some(ref cache) = *cache {
                if cache.last_validated_at.elapsed() < Duration::from_secs(2) {
                    return cache.get_sessions(&dirs);
                }
            }
        }

        // Snapshot prior per-dir state for mtime-based reuse (short lock).
        let prior: HashMap<PathBuf, CachedDir> = {
            let cache = self.cache.lock().unwrap();
            cache.as_ref().map(|c| c.dirs.clone()).unwrap_or_default()
        };

        // Async scan — one tokio::fs pass per directory.
        let mut scanned: HashMap<PathBuf, CachedDir> = HashMap::new();
        for dir in &dirs {
            let mut sessions = Vec::new();
            let mut file_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();
            if let Ok(mut read_dir) = tokio::fs::read_dir(dir).await {
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    let path = entry.path();
                    if !path.extension().is_some_and(|e| e == "jsonl") {
                        continue;
                    }
                    let file_mtime = entry.metadata().await.ok().and_then(|m| m.modified().ok());

                    // Reuse cached metadata when the file mtime is unchanged.
                    if let Some(file_mt) = file_mtime {
                        let hit = prior.get(dir).and_then(|c| {
                            if c.file_mtimes.get(&path) == Some(&file_mt) {
                                c.sessions.iter().find(|s| s.path == path).cloned()
                            } else {
                                None
                            }
                        });
                        if let Some(old) = hit {
                            sessions.push(old);
                            file_mtimes.insert(path, file_mt);
                            continue;
                        }
                    }

                    // Cache miss or changed file: re-read metadata.
                    if let Some(meta) = Self::read_metadata_listing(&path).await {
                        if let Some(mt) = file_mtime {
                            file_mtimes.insert(path.clone(), mt);
                        }
                        // Migrate: ensure header has name if we found one from session_info
                        let path_clone = path.clone();
                        let content = tokio::fs::read_to_string(&path_clone).await.ok();
                        if let Some(content) = content {
                            if let Some(first_line) = content.lines().next() {
                                if let Ok(header) =
                                    serde_json::from_str::<SessionHeader>(first_line)
                                {
                                    Self::ensure_header_has_name(
                                        path_clone,
                                        &header,
                                        meta.name.clone(),
                                    )
                                    .await;
                                }
                            }
                        }
                        sessions.push(meta);
                    }
                }
            }
            sessions.sort_by(|a, b| {
                let a_ts = a
                    .file_mtime
                    .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let b_ts = b
                    .file_mtime
                    .and_then(|mt| mt.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                b_ts.cmp(&a_ts)
            });
            scanned.insert(
                dir.clone(),
                CachedDir {
                    file_mtimes,
                    sessions,
                },
            );
        }

        // Store + return (short lock).
        let mut cache_guard = self.cache.lock().unwrap();
        let cache: &mut SessionMetaCache = cache_guard.get_or_insert_with(SessionMetaCache::new);
        cache.last_validated_at = Instant::now();
        for (dir, cd) in scanned {
            cache.dirs.insert(dir, cd);
        }
        cache.get_sessions(&dirs)
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
    async fn create_persisted(
        repo: &JsonlSessionRepo,
        cwd: &Path,
    ) -> crate::session::storage::JsonlSessionStorage {
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

    /// Regression: `delete()` must invalidate the sync-listing cache.
    /// The `/session` overlay lists via `list_sessions_sync()`, which
    /// serves cached results for up to 2 seconds — without invalidation a
    /// just-deleted session stayed visible as a ghost, so the delete
    /// appeared to have done nothing.
    #[tokio::test]
    async fn test_delete_invalidates_sync_listing_cache() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        let _storage = create_persisted(&repo, cwd).await;
        // Prime the sync-listing cache.
        assert_eq!(repo.list_sessions_sync(Some(cwd)).len(), 1);

        let sessions = repo.list(None).await.unwrap();
        repo.delete(&sessions[0]).await.unwrap();

        // Immediately (well inside the 2s debounce window) the deleted
        // session must be gone from the sync listing too.
        assert!(
            repo.list_sessions_sync(Some(cwd)).is_empty(),
            "deleted session still visible via sync listing (stale cache)"
        );

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

        // Fork from e1 (before e2, so we get only the first message) - ActivePath mode
        let fork_storage = repo
            .fork(&sessions[0], "e1", cwd, ForkMode::ActivePath)
            .await
            .unwrap();
        let fork_index = fork_storage.build_index().await.unwrap();

        // Fork should have the header + e1 = 1 entry (header is separate).
        // Note: the new Leaf entry pointing to e1 is also in entries map.
        assert_eq!(fork_index.entries.len(), 2); // e1 + Leaf
        assert!(fork_index.entries.contains_key("e1"));

        // Fork should have parent_session set
        assert!(fork_index.header.parent_session.is_some());

        let sessions_after = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions_after.len(), 2); // original + fork

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_fork_session_full_tree() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        // Create source session with a few entries + a branch.
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

        // A branch from e1
        let entry3 = SessionEntry {
            id: "e3".into(),
            parent_id: Some("e1".into()),
            timestamp: "t3".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({"role":"user","content":[]}),
            },
        };
        source_storage.append_entry(&entry3).await.unwrap();

        // Leaf pointing to e2 (active branch)
        let leaf = SessionEntry::new_leaf(Some("e2".into()), Some("e1".into()));
        source_storage.append_entry(&leaf).await.unwrap();

        // Get source metadata
        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);

        // FullTree fork (empty fork_entry_id) - should preserve ALL entries including the branch
        let fork_storage = repo
            .fork(&sessions[0], "", cwd, ForkMode::FullTree)
            .await
            .unwrap();
        let fork_index = fork_storage.build_index().await.unwrap();

        // FullTree fork should have ALL 3 entries (e1, e2, e3) + Leaf = 4 entries
        assert_eq!(fork_index.entries.len(), 4);
        assert!(fork_index.entries.contains_key("e1"));
        assert!(fork_index.entries.contains_key("e2"));
        assert!(fork_index.entries.contains_key("e3"));

        // Fork should have parent_session set
        assert!(fork_index.header.parent_session.is_some());

        // Leaf should point to e2 (same as source)
        assert_eq!(fork_index.leaf_id, Some("e2".into()));

        let sessions_after = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions_after.len(), 2); // original + fork

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }

    #[tokio::test]
    async fn test_fork_session_full_tree_with_fork_point() {
        let (repo, base_dir) = create_test_repo().await;
        let cwd = Path::new("/home/test");

        // Create source session with a few entries.
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

        let sessions = repo.list(Some(cwd)).await.unwrap();

        // FullTree fork WITH a specific fork_entry_id - should preserve all entries
        // AND add a new Leaf pointing to e1
        let fork_storage = repo
            .fork(&sessions[0], "e1", cwd, ForkMode::FullTree)
            .await
            .unwrap();
        let fork_index = fork_storage.build_index().await.unwrap();

        // Should have all entries + original Leaf + new Leaf = 4 entries
        assert_eq!(fork_index.entries.len(), 4);
        assert!(fork_index.entries.contains_key("e1"));
        assert!(fork_index.entries.contains_key("e2"));

        // Leaf should point to e1 (the fork point)
        assert_eq!(fork_index.leaf_id, Some("e1".into()));

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

        // The efficient listing (read_metadata_listing) doesn't populate detail.
        // Use read_metadata_detail to get full metadata including message counts.
        let sessions = repo.list(Some(cwd)).await.unwrap();
        assert_eq!(sessions.len(), 1);
        let meta = &sessions[0];
        let detail_meta = JsonlSessionRepo::read_metadata_detail(&meta.path)
            .expect("read_metadata_detail should work");
        let detail = detail_meta
            .detail
            .as_ref()
            .expect("detail should be populated");
        assert_eq!(detail.message_count, 2);
        // 1 user message + 1 assistant message → 1 turn.
        assert_eq!(detail.user_message_count, 1);

        let _ = tokio::fs::remove_dir_all(&base_dir).await;
    }
}
