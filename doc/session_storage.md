# Session Storage — Design & Implementation Plan

## Motivation

prior currently has **zero persistent session storage**. The conversation
history lives only in a `Vec<Message>` (`history` in `run_interactive`,
`src/agent.rs:1108`) that is in-memory and lost on restart. There is no way to:

- Resume a previous session
- Name or label a session
- Fork/branch from an earlier point in the conversation
- Inspect past sessions

This document specifies a session storage layer that follows the
[pi agent](https://github.com/badlogic/pi-mono) model closely, aiming for
**JSONL on-disk format compatibility**. Every design decision is evaluated
against the goal that a pi session file and a prior session file should be
interchangeable (modulo trivial serde renaming).

---

## pi's Session Storage Architecture

### Overview

pi stores sessions as **JSONL (JSON Lines) files**, one per session, in
`~/.pi/agent/sessions/`. Each session file is an append-only log of structured
entries representing every state change in the session.

### File Layout (pi-compatible)

```text
~/.prior/sessions/
  └── <cwd-encoded>/
       └── <timestamp>_<uuid>.jsonl
```

### CWD Encoding (exact pi format)

The working directory is encoded into a single directory name.
**Pi's actual `encodeCwd`** (`jsonl-repo.ts:34`):

```typescript
function encodeCwd(cwd: string): string {
    return `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`;
}
```

The algorithm:

1. Strip a leading `/` or `\`
2. Replace every `/`, `\`, and `:` with a single hyphen `-`
3. Wrap with `--` prefix and `--` suffix

> **Ambiguity is a real, accepted limitation.** Path components containing
> hyphens produce collisions:
>
> | Path                       | Encoded                               |
> | -------------------------- | ------------------------------------- |
> | `/home/gabriel/my-project` | `--home-gabriel-my-project--`         |
> | `/home/gabriel/my/project` | `--home-gabriel-my-project--` (same!) |
>
> Pi **does not provide a `decodeCwd` function** — the directory name is
> write-only. The ground truth CWD is stored in the session header's `cwd`
> field. When listing sessions, pi reads every `.jsonl` file under every
> `--...--` directory and extracts the real CWD from each file's first JSON
> line. The directory name is purely a grouping heuristic (sessions started
> in the same directory appear in the same folder) and is never decoded back
> to a path.
>
> This means `cp -r ~/.pi/agent/sessions/* ~/.prior/sessions/` works
> verbatim for import — the directory names match exactly, and prior
> reads CWD from the same header field.

Examples:

|Original path                       |Encoded directory name                   |
|------------------------------------|-----------------------------------------|
|`/home/gabriel/src/prior`        |`--home-gabriel-src-prior--`          |
|`/Users/badlogic/workspaces/pi-mono`|`--Users-badlogic-workspaces-pi-mono--`  |
|`/home/my-project/src`              |`--home-my-project-src--`                |
|`C:\Users\me\project`         |`--C-Users-me-project--` (`\` → `-`, `:` → `-`)|
|`/`                   |`----` (strip → `""` → wrap → `----`; edge case, valid)|

**Rust implementation (exact pi match):**

```rust
/// Encode a CWD path into a pi-compatible directory name.
///
/// Pi's exact algorithm: strip leading `/` or `\`, replace `/`, `\`, `:` with
/// `-`, wrap with `--...--`. Ambiguous for path components containing hyphens
/// (e.g. `my-project` vs `my/project` collide), but the directory name is
/// write-only — the real CWD lives in the session header JSON.
fn encode_cwd(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let stripped = s.trim_start_matches(|c| c == '/' || c == '\\');
    let encoded = stripped.replace(|c: char| c == '/' || c == '\\' || c == ':', "-");
    format!("--{}--", encoded)
}

/// Decode a CWD directory name back to a path.
///
/// **Lossy!** This is inherently ambiguous (see note above) so the result is
/// best-effort. In practice, prior reads `cwd` from the session header
/// instead of decoding the directory name. This function exists for display
/// and debugging — never for resolving the session's working directory.
fn decode_cwd(encoded: &str) -> Option<String> {
    let inner = encoded.strip_prefix("--")?.strip_suffix("--")?;
    // Split on `-` and rejoin with `/`. This is wrong for paths whose
    // components contain `-`, but there's no better heuristic without the
    // ground-truth header. Prefer reading `cwd` from the JSON header.
    let decoded = inner.replace('-', "/");
    Some(format!("/{}", decoded))
}
```

### Filename Format (exact pi format)

`<iso_timestamp>_<uuid>.jsonl`

The ISO 8601 timestamp has `:` characters replaced with `-` (since `:` is
invalid on Windows filenames):

```text
Input:  2026-07-20T22:22:21.906Z
Output: 2026-07-20T22-22-21-906Z
```

Full filename:
`2026-07-20T22-22-21-906Z_019f819f-80d2-7948-8ee8-c96d4785c6ec.jsonl`

**Rust implementation:**

```rust
/// Generate a session filename matching pi's convention.
fn session_filename(timestamp: &chrono::DateTime<chrono::Utc>) -> String {
    let ts = timestamp.format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
    let uuid = uuid::Uuid::now_v7();
    format!("{}_{}.jsonl", ts, uuid)
}

/// Parse timestamp and UUID from a session filename.
fn parse_session_filename(name: &str) -> Option<(String, String)> {
    let name = name.strip_suffix(".jsonl")?;
    let (ts, uuid) = name.split_once('_')?;
    Some((ts.to_string(), uuid.to_string()))
}
```

### Full Path Example

```text
~/.prior/sessions/--home-gabriel-src-prior--/2026-07-20T22-22-21-906Z_019f819f-80d2-7948-8ee8-c96d4785c6ec.jsonl
```

---

## File Format (pi JSONL)

Each file contains one JSON object per line,
**exactly matching pi's wire format** (camelCase field names, same `type` tags,
same field layout).

### Line 1 — Session Header

```json
{
  "type": "session",
  "version": 3,
  "id": "d703a1a9-1b7b-4fb1-b512-c9738b1fe617",
  "timestamp": "2025-11-20T23:33:50.805Z",
  "cwd": "/home/gabriel/src/prior",
  "parentSession": "<optional absolute path to parent session file>",
  "metadata": { ... }
}
```

### Subsequent Lines — Session Entries

Each entry has the base fields:

```json
{
  "type": "<entry-type>",
  "id": "a1b2c3d4",
  "parentId": "e5f6g7h8",
  "timestamp": "2025-11-20T23:33:50.805Z",
  "...type-specific fields..."
}
```

#### Entry Type Catalog

- **`type` value**: `"message"`
  - **Required fields**: `message: AgentMessage`
  - **pi compatibility notes**: See AgentMessage mapping below
- **`type` value**: `"thinking_level_change"`
  - **Required fields**: `thinkingLevel: string`
  - **pi compatibility notes**: Same field name
- **`type` value**: `"model_change"`
  - **Required fields**: `provider: string, modelId: string`
  - **pi compatibility notes**: Same field names
- **`type` value**: `"active_tools_change"`
  - **Required fields**: `activeToolNames: string[]`
  - **pi compatibility notes**: Same field name
- **`type` value**: `"compaction"`
  - **Required fields**:
    `summary: string, firstKeptEntryId: string, tokensBefore: number`
  - **pi compatibility notes**: Same names
- **`type` value**: `"branch_summary"`
  - **Required fields**: `fromId: string, summary: string`
  - **pi compatibility notes**: Same names. `fromId` is the
    **branch-from position** (pi's `branchWithSummary`), not the common
    ancestor; `parentId` is the same id. Written parented at the `/tree` switch
    target, so it is inline on the branch the conversation continues on and
    projects into context. pi also writes optional `details` / `fromHook`, which
    we omit. See `doc/branch_summary.md`.
- **`type` value**: `"label"`
  - **Required fields**: `targetId: string, label?: string`
  - **pi compatibility notes**: Same names
- **`type` value**: `"session_info"`
  - **Required fields**: `name?: string`
  - **pi compatibility notes**: Same field name
- **`type` value**: `"custom"`
  - **Required fields**: `customType: string, data: object`
  - **pi compatibility notes**: Same names
- **`type` value**: `"custom_message"`
  - **Required fields**: `customType: string, content: object`
  - **pi compatibility notes**: Same names
- **`type` value**: `"leaf"`
  - **Required fields**: `targetId: string \| null`
  - **pi compatibility notes**: Same field name

### AgentMessage Mapping (pi ↔ rig)

pi uses its own abstracted `AgentMessage` format with three roles and typed
content items. Mapping to rig's `Message` type:

**pi `AgentMessage` wire format:**

```jsonc
// user message
{ "role": "user", "content": [{"type":"text","text":"..."}], "timestamp": 1234567890 }

// assistant message
{ "role": "assistant",
  "content": [
    {"type":"thinking",  "thinking":"...", "thinkingSignature":"..."},
    {"type":"toolCall",  "id":"toolu_...",  "name":"bash",  "arguments":{...}},
    {"type":"text",      "text":"..."}
  ],
  "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
             "totalTokens": 0, "reasoning": 0 },
  "stopReason": "end_turn", "responseId": "msg_...", "timestamp": 1234567890 }

// tool result (one per result)
{ "role": "toolResult", "toolCallId":"toolu_...", "toolName":"bash",
  "content": [{"type":"text","text":"..."}], "isError": false, "timestamp": 1234567890 }
```

**Mapping to rig:**

- **pi role + content type**: `role:"user"`, `type:"text"`
  - **rig equivalent**: `Message::User { content: [UserContent::Text(text)] }`
- **pi role + content type**: `role:"assistant"`, `type:"text"`
  - **rig equivalent**:
    `Message::Assistant { content: [AssistantContent::Text(text)] }`
- **pi role + content type**: `type:"thinking"`, fields
  `thinking`/`thinkingSignature`
  - **rig equivalent**:
    `AssistantContent::Reasoning(Reasoning { content: [ReasoningContent::Text { text: thinking, signature: thinkingSignature }] })`
- **pi role + content type**: `type:"toolCall"`, fields `id`/`name`/`arguments`
  - **rig equivalent**:
    `AssistantContent::ToolCall(ToolCall { id, function: ToolFunction { name, arguments } })`
- **pi role + content type**: `role:"toolResult"` messages
  - **rig equivalent**:
    `UserContent::ToolResult(ToolResult { id: toolCallId, call_id: toolCallId, is_error, content: ... })`

> **pi toolResult batching:** pi stores one `"toolResult"` entry per tool call.
> In rig's message history (the `Vec<Message>` passed to the next LLM call), all
> results for a turn are committed together as a single `Message::User` with
> multiple `UserContent::ToolResult` items. The importer must scan ahead to
> group consecutive pi `"toolResult"` entries into one `User` message.

> **pi entry IDs:** pi uses 8-character hex strings for entry IDs (e.g.
> `"84c3124c"`); the session header uses a standard UUID. prior uses UUIDv7
> throughout. The importer accepts any string, not validating as UUID.

### Wire Compatibility Strategy

prior's *own* storage uses **pi's camelCase field names directly** — not
snake_case. This means:

- `parentId` not `parent_id`
- `modelId` not `model_id`
- `thinkingLevel` not `thinking_level`
- `activeToolNames` not `active_tool_names`
- `targetId` not `target_id`
- `customType` not `custom_type`
- `firstKeptEntryId` not `first_kept_entry_id`
- `tokensBefore` not `tokens_before`
- `parentSession` not `parent_session`

The serde attributes on our Rust types use `#[serde(rename_all = "camelCase")]`
at the container level, and explicit `#[serde(rename = "...")]` where pi uses a
different key name.

This means **prior JSONL files are directly readable by pi** and vice versa,
modulo the session header `version` field (both use `3`).

---

## Data Model (Rust Types)

```rust
// ==========================================================================
// src/session/types.rs
// ==========================================================================

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// The first line of every JSONL session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionHeader {
    #[serde(rename = "type")]
    pub(crate) kind: String,          // always "session"
    pub(crate) version: u32,          // always 3
    pub(crate) id: String,            // UUIDv7
    pub(crate) timestamp: String,     // ISO 8601
    pub(crate) cwd: String,           // the working directory at session start
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_session: Option<PathBuf>,  // absolute path to parent session file (for forks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<serde_json::Value>,
}

impl SessionHeader {
    pub(crate) fn new(cwd: &std::path::Path) -> Self {
        Self {
            kind: "session".to_string(),
            version: 3,
            id: Uuid::now_v7().to_string(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            cwd: cwd.to_string_lossy().to_string(),
            parent_session: None,
            metadata: None,
        }
    }
}

/// Metadata about a session (derived from header + index), used for listing.
#[derive(Debug, Clone)]
pub(crate) struct SessionMetadata {
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) cwd: String,
    pub(crate) path: PathBuf,             // absolute path to the .jsonl file
    pub(crate) name: Option<String>,      // from the most recent session_info entry
    pub(crate) parent_session: Option<PathBuf>,
    pub(crate) message_count: usize,
    pub(crate) user_message_count: usize,    // user-role messages (= turn count)
    pub(crate) token_count: Option<u64>,  // from usage on the leaf branch
}

/// A single entry in the session tree. `#[serde(flatten)]` merges the
/// type-specific fields into the same JSON object as the base fields,
/// producing the flat JSONL line pi expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionEntry {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,   // serialized as "parentId"
    pub(crate) timestamp: String,
    #[serde(flatten)]
    pub(crate) content: SessionEntryContent,
}

/// The type-specific payload. `#[serde(tag = "type")]` writes a `"type"` key
/// alongside the variant fields, matching pi's self-describing wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SessionEntryContent {
    Message {
        message: serde_json::Value,   // pi AgentMessage format (see mapping below)
    },
    #[serde(rename = "thinking_level_change")]
    ThinkingLevelChange {
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    #[serde(rename = "model_change")]
    ModelChange {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "active_tools_change")]
    ActiveToolsChange {
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
    },
    Compaction {
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
    #[serde(rename = "branch_summary")]
    BranchSummary {
        #[serde(rename = "fromId")]
        from_id: String,
        summary: String,
    },
    Label {
        #[serde(rename = "targetId")]
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    #[serde(rename = "session_info")]
    SessionInfo {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Custom {
        #[serde(rename = "customType")]
        custom_type: String,
        data: serde_json::Value,
    },
    #[serde(rename = "custom_message")]
    CustomMessage {
        #[serde(rename = "customType")]
        custom_type: String,
        content: serde_json::Value,
    },
    Leaf {
        #[serde(rename = "targetId")]
        target_id: Option<String>,
    },
}
```

> **Design note on `Message` as `serde_json::Value`:** We store the pi
> `AgentMessage` as a raw `serde_json::Value` rather than trying to map it to
> rig's `Message` type at storage time. This preserves pi wire compatibility
> exactly — we take whatever JSON pi produces and write it verbatim. The mapping
> to rig's type happens only in `build_context()` (session → LLM context) and in
> the import path. This means the storage layer is agnostic to rig's message
> schema versioning.

### In-Memory Index

On open, the full JSONL file is scanned into an in-memory index:

```rust
/// In-memory representation of a session's entry tree, rebuilt from JSONL on open.
#[derive(Debug, Clone)]
pub(crate) struct SessionIndex {
    pub(crate) header: SessionHeader,
    pub(crate) entries: HashMap<String, SessionEntry>,  // id → entry
    pub(crate) leaf_id: Option<String>,                  // current leaf pointer
    pub(crate) children: HashMap<String, Vec<String>>,   // parentId → child ids
    pub(crate) session_name: Option<String>,             // from latest session_info entry
}
```

---

## Storage Traits

```rust
// ==========================================================================
// src/session/storage.rs
// ==========================================================================

/// Low-level I/O for a single JSONL session file.
#[async_trait]
pub(crate) trait SessionStorage: Send + Sync {
    /// Read metadata from the session header (no full scan).
    async fn read_header(&self) -> crate::Result<SessionHeader>;

    /// Full index build: parse every line, build HashMap, find leaf.
    async fn build_index(&self) -> crate::Result<SessionIndex>;

    /// Create a UUIDv7 entry ID.
    fn create_entry_id(&self) -> String;

    /// Append an entry line to the JSONL file.
    async fn append_entry(&self, entry: &SessionEntry) -> crate::Result<()>;

    /// Update the leaf pointer by appending a new leaf entry.
    async fn set_leaf_id(&self, leaf_id: Option<String>) -> crate::Result<()>;

    /// Walk parentId chain from leaf to root.
    fn get_path_to_root(&self, index: &SessionIndex, leaf_id: &str) -> Vec<SessionEntry>;

    /// Get the messages for a branch (path to root, filtered to message entries).
    fn get_branch_messages(&self, index: &SessionIndex) -> Vec<SessionEntry>;
}

/// Repository managing multiple sessions in a directory tree.
#[async_trait]
pub(crate) trait SessionRepo: Send + Sync {
    /// Create a new session with the given CWD.
    async fn create(&self, cwd: &std::path::Path) -> crate::Result<SessionHandle>;

    /// Open an existing session by metadata.
    async fn open(&self, meta: &SessionMetadata) -> crate::Result<SessionHandle>;

    /// List all sessions for a given CWD (or all CWDs if None).
    async fn list(&self, cwd: Option<&std::path::Path>) -> crate::Result<Vec<SessionMetadata>>;

    /// Delete a session file.
    async fn delete(&self, meta: &SessionMetadata) -> crate::Result<()>;

    /// Fork a session from a specific entry.
    async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &std::path::Path,
    ) -> crate::Result<SessionHandle>;
}
```

### JsonlSessionStorage Implementation

```rust
pub(crate) struct JsonlSessionStorage {
    path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
}

impl JsonlSessionStorage {
    /// Open (or create) a session file at `path`.
    pub(crate) async fn open_or_create(path: PathBuf) -> crate::Result<Self> {
        // Create parent directories.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Open with append + read.
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

    /// Read the session header (first line) without full scan.
    pub(crate) async fn read_header(&self) -> crate::Result<SessionHeader> {
        use tokio::io::AsyncBufReadExt;
        let file = self.file.lock().await;
        // Seek to beginning (we use a separate read handle for scan operations).
        drop(file);
        // Implementation uses a separate read handle...
        todo!()
    }

    /// Build the in-memory index by scanning all lines.
    pub(crate) async fn build_index(&self) -> crate::Result<SessionIndex> {
        use tokio::io::AsyncBufReadExt;
        let mut entries = HashMap::new();
        let mut leaf_id = None;
        let mut session_name = None;
        let mut header: Option<SessionHeader> = None;
        let mut children: HashMap<String, Vec<String>> = HashMap::new();

        // Read the file from the beginning using a separate read handle.
        let content = tokio::fs::read_to_string(&self.path).await?;
        for (line_no, line) in content.lines().enumerate() {
            if line.trim().is_empty() { continue; }
            if line_no == 0 {
                header = Some(serde_json::from_str(line)?);
                continue;
            }
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => {
                    // Track parent-child relationships.
                    if let Some(ref pid) = entry.parent_id {
                        children.entry(pid.clone()).or_default().push(entry.id.clone());
                    }
                    // Track leaf.
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
                    // Truncated file: skip last line if it's a partial write.
                    // Any other parse error is fatal.
                    // (Detection: if it's the last line AND has no trailing newline, skip.)
                    eprintln!("Warning: skipping unparseable line {}: {}", line_no, e);
                }
            }
        }

        let header = header.ok_or_else(|| {
            crate::PriorError::Other("Session file has no header".into())
        })?;

        Ok(SessionIndex {
            header,
            entries,
            leaf_id,
            children,
            session_name,
        })
    }

    /// Append a JSON-serialized entry to the file (with trailing newline).
    pub(crate) async fn append_entry(&self, entry: &SessionEntry) -> crate::Result<()> {
        let mut file = self.file.lock().await;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        use tokio::io::AsyncWriteExt;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }
}
```

### JsonlSessionRepo Implementation

```rust
pub(crate) struct JsonlSessionRepo {
    base_dir: PathBuf,  // ~/.prior/sessions/
}

impl JsonlSessionRepo {
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Create a new session: generate path, write header, return handle.
    pub(crate) async fn create(&self, cwd: &Path) -> crate::Result<SessionHandle> {
        let cwd_encoded = encode_cwd(cwd);
        let session_dir = self.base_dir.join(&cwd_encoded);
        let filename = session_filename(&Utc::now());
        let path = session_dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone()).await?;

        // Write the header as the first line.
        let header = SessionHeader::new(cwd);
        let header_entry = SessionEntry {
            id: header.id.clone(),
            parent_id: None,
            timestamp: header.timestamp.clone(),
            content: SessionEntryContent::Custom {
                custom_type: "header".into(),
                data: serde_json::to_value(&header)?,
            },
        };
        // (Note: the header is written as a raw JSON line, not via append_entry,
        //  to avoid the flattened Entry overhead. See implementation notes.)
        storage.append_entry(&header_entry).await?; // FIXME: write raw header

        Ok(SessionHandle { path, storage, index: None })
    }

    /// List all sessions for a given CWD.
    pub(crate) async fn list(&self, cwd: Option<&Path>) -> crate::Result<Vec<SessionMetadata>> {
        let mut sessions = Vec::new();

        if let Some(cwd) = cwd {
            let encoded = encode_cwd(cwd);
            let dir = self.base_dir.join(&encoded);
            if !dir.exists() {
                return Ok(sessions);
            }
            let mut read_dir = tokio::fs::read_dir(&dir).await?;
            while let Some(entry) = read_dir.next_entry().await? {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "jsonl") {
                    if let Some(meta) = self.read_metadata(&path).await {
                        sessions.push(meta);
                    }
                }
            }
        } else {
            // List all CWD directories and gather sessions from each.
            let mut read_dir = tokio::fs::read_dir(&self.base_dir).await?;
            while let Some(dir_entry) = read_dir.next_entry().await? {
                let dir_path = dir_entry.path();
                if dir_path.is_dir() {
                    let mut sub_read = tokio::fs::read_dir(&dir_path).await?;
                    while let Some(entry) = sub_read.next_entry().await? {
                        let path = entry.path();
                        if path.extension().map_or(false, |e| e == "jsonl") {
                            if let Some(meta) = self.read_metadata(&path).await {
                                sessions.push(meta);
                            }
                        }
                    }
                }
            }
        }

        // Sort by timestamp descending (most recent first).
        sessions.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(sessions)
    }

    /// Quick metadata read: parse only the first line (header), stat the file.
    async fn read_metadata(&self, path: &Path) -> Option<SessionMetadata> {
        let content = tokio::fs::read_to_string(path).await.ok()?;
        let first_line = content.lines().next()?;
        let header: SessionHeader = serde_json::from_str(first_line).ok()?;

        // Count message entries by scanning for "type":"message" lines.
        let message_count = content.lines()
            .filter(|l| l.contains("\"type\":\"message\""))
            .count();
        // Count user-role messages (turns).
        let user_message_count = content.lines()
            .filter(|l| l.contains("\"type\":\"message\"") && l.contains("\"role\":\"user\""))
            .count();

        Some(SessionMetadata {
            id: header.id,
            timestamp: header.timestamp,
            cwd: header.cwd,
            path: path.to_path_buf(),
            name: None,  // populated on full index build
            parent_session: header.parent_session,
            message_count,
            user_message_count,
            token_count: None,
        })
    }

    /// Fork from a given entry. Copies the header + entries up to (and including)
    /// the fork entry into a new session file, with a new session header.
    pub(crate) async fn fork(
        &self,
        source: &SessionMetadata,
        fork_entry_id: &str,
        cwd: &Path,
    ) -> crate::Result<SessionHandle> {
        // Read source index.
        let source_storage = JsonlSessionStorage::open_or_create(source.path.clone()).await?;
        let source_index = source_storage.build_index().await?;

        // Collect entries from fork_entry_id to root.
        let path = source_storage.get_path_to_root(&source_index, fork_entry_id);

        // Create new session.
        let cwd_encoded = encode_cwd(cwd);
        let session_dir = self.base_dir.join(&cwd_encoded);
        let filename = session_filename(&Utc::now());
        let new_path = session_dir.join(&filename);

        let new_storage = JsonlSessionStorage::open_or_create(new_path.clone()).await?;

        // Write new header (with parent_session set).
        let mut header = SessionHeader::new(cwd);
        header.parent_session = Some(source.path.clone());

        // Write header as raw JSON line.
        let header_line = serde_json::to_string(&header)?;
        {
            let mut file = new_storage.file.lock().await;
            use tokio::io::AsyncWriteExt;
            file.write_all(header_line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }

        // Write entries (path is from leaf to root, reverse to root→leaf).
        for entry in path.iter().rev() {
            new_storage.append_entry(entry).await?;
        }

        Ok(SessionHandle {
            path: new_path,
            storage: new_storage,
            index: None,
        })
    }
}

/// A handle to an open session, wrapping the storage + lazily-built index.
pub(crate) struct SessionHandle {
    pub(crate) path: PathBuf,
    pub(crate) storage: JsonlSessionStorage,
    pub(crate) index: Option<SessionIndex>,
}
```

---

## Session Object (Context Building)

```rust
// ==========================================================================
// src/session/session.rs
// ==========================================================================

use rig_core::message::Message;

/// High-level session operations. Wraps a SessionHandle and provides
/// context building, append helpers, and forking.
pub(crate) struct Session {
    handle: SessionHandle,
    index: SessionIndex,
}

impl Session {
    /// Open a session and build its index.
    pub(crate) async fn open(handle: SessionHandle) -> crate::Result<Self> {
        let index = handle.storage.build_index().await?;
        Ok(Self { handle, index })
    }

    /// Walk from leaf to root, collecting entries.
    pub(crate) fn get_branch(&self) -> Vec<SessionEntry> {
        let leaf_id = match &self.index.leaf_id {
            Some(id) => id.clone(),
            None => return vec![],
        };
        self.handle.storage.get_path_to_root(&self.index, &leaf_id)
    }

    /// Build the LLM context from the current branch.
    ///
    /// 1. Walk leaf → root collecting entries
    /// 2. Filter to message entries only
    /// 3. Map pi AgentMessage format → rig Message format
    /// 4. Apply transforms (compaction, reasoning stripping)
    /// 5. Return context + metadata
    pub(crate) fn build_context(&self) -> crate::Result<SessionContext> {
        let branch = self.get_branch();
        let transformed = self.apply_transforms(branch);
        let messages = self.extract_messages(&transformed);
        Ok(SessionContext {
            messages,
            thinking_level: self.derive_thinking_level(&transformed),
            model: self.derive_model(&transformed),
        })
    }

    /// Apply compaction/filter transforms on the branch.
    fn apply_transforms(&self, entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
        // Find the most recent compaction entry. If found, skip all entries
        // before firstKeptEntryId. Replace the skipped range with a synthetic
        // user message carrying the summary text.
        //
        // For Phase 1 (no compaction yet): identity transform.
        entries
    }

    /// Extract rig Messages from a filtered branch.
    fn extract_messages(&self, entries: &[SessionEntry]) -> Vec<Message> {
        let mut messages = Vec::new();
        for entry in entries {
            if let SessionEntryContent::Message { message } = &entry.content {
                if let Some(msg) = pi_message_to_rig(message) {
                    messages.push(msg);
                }
            }
        }
        messages
    }

    fn derive_thinking_level(&self, entries: &[SessionEntry]) -> Option<String> {
        entries.iter()
            .rev()
            .find_map(|e| {
                if let SessionEntryContent::ThinkingLevelChange { thinking_level } = &e.content {
                    Some(thinking_level.clone())
                } else {
                    None
                }
            })
    }

    fn derive_model(&self, entries: &[SessionEntry]) -> Option<String> {
        entries.iter()
            .rev()
            .find_map(|e| {
                if let SessionEntryContent::ModelChange { provider: _, model_id } = &e.content {
                    Some(model_id.clone())
                } else {
                    None
                }
            })
    }

    // --- Append helpers ---

    pub(crate) async fn append_message(&self, msg: &Message) -> crate::Result<()> {
        let entry = SessionEntry {
            id: self.handle.storage.create_entry_id(),
            parent_id: self.index.leaf_id.clone(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Message {
                message: rig_message_to_pi(msg),
            },
        };
        self.handle.storage.append_entry(&entry).await?;
        Ok(())
    }

    pub(crate) async fn append_model_change(&self, provider: &str, model_id: &str) -> crate::Result<()> {
        let entry = SessionEntry {
            id: self.handle.storage.create_entry_id(),
            parent_id: self.index.leaf_id.clone(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::ModelChange {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
        };
        self.handle.storage.append_entry(&entry).await?;
        Ok(())
    }

    pub(crate) async fn append_thinking_level_change(&self, level: &str) -> crate::Result<()> {
        let entry = SessionEntry {
            id: self.handle.storage.create_entry_id(),
            parent_id: self.index.leaf_id.clone(),
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::ThinkingLevelChange {
                thinking_level: level.to_string(),
            },
        };
        self.handle.storage.append_entry(&entry).await?;
        Ok(())
    }

    pub(crate) async fn set_leaf(&self, target_id: Option<String>) -> crate::Result<()> {
        let entry = SessionEntry {
            id: self.handle.storage.create_entry_id(),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Leaf { target_id },
        };
        self.handle.storage.append_entry(&entry).await?;
        Ok(())
    }

    pub(crate) async fn set_session_name(&self, name: &str) -> crate::Result<()> {
        let entry = SessionEntry {
            id: self.handle.storage.create_entry_id(),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::SessionInfo {
                name: Some(name.to_string()),
            },
        };
        self.handle.storage.append_entry(&entry).await?;
        Ok(())
    }
}

/// The reconstructed LLM context derived from a session branch.
#[derive(Debug)]
pub(crate) struct SessionContext {
    /// Messages to pass as conversation history for the next LLM call.
    pub(crate) messages: Vec<Message>,
    /// Thinking level in effect at the leaf.
    pub(crate) thinking_level: Option<String>,
    /// Model in effect at the leaf.
    pub(crate) model: Option<String>,
}
```

### pi AgentMessage ↔ rig Message Conversion

```rust
// ==========================================================================
// src/session/pi_compat.rs  (or inline in session.rs)
// ==========================================================================

/// Convert a pi-format AgentMessage (serde_json::Value) to a rig Message.
fn pi_message_to_rig(value: &serde_json::Value) -> Option<Message> {
    let role = value.get("role")?.as_str()?;
    match role {
        "user" => {
            let content = pi_content_to_user_content(value.get("content")?)?;
            Some(Message::User {
                content: OneOrMany::many(content).ok()?,
            })
        }
        "assistant" => {
            let content = pi_content_to_assistant_content(value.get("content")?)?;
            Some(Message::Assistant {
                id: value.get("responseId").and_then(|v| v.as_str().map(String::from)),
                content: OneOrMany::many(content).ok()?,
            })
        }
        "toolResult" => {
            // pi toolResult messages become UserContent::ToolResult items.
            // They must be batched by the caller.
            let tr = pi_toolresult_to_rig(value)?;
            Some(Message::User {
                content: OneOrMany::one(UserContent::ToolResult(tr)),
            })
        }
        _ => None,
    }
}

/// Convert a rig Message to pi-format AgentMessage (serde_json::Value).
fn rig_message_to_pi(msg: &Message) -> serde_json::Value {
    match msg {
        Message::User { content } => {
            let mut pi_content = Vec::new();
            for c in content.iter() {
                match c {
                    UserContent::Text(t) => {
                        pi_content.push(serde_json::json!({
                            "type": "text",
                            "text": t.text
                        }));
                    }
                    UserContent::ToolResult(tr) => {
                        // Split into individual toolResult entries (pi format).
                        // These are handled at the caller level (write multiple lines).
                        todo!()
                    }
                    _ => {}
                }
            }
            serde_json::json!({
                "role": "user",
                "content": pi_content,
                "timestamp": Utc::now().timestamp_millis(),
            })
        }
        Message::Assistant { id, content } => {
            let mut pi_content = Vec::new();
            for c in content.iter() {
                match c {
                    AssistantContent::Text(t) => {
                        pi_content.push(serde_json::json!({"type":"text","text":t.text}));
                    }
                    AssistantContent::Reasoning(r) => {
                        for rc in &r.content {
                            match rc {
                                ReasoningContent::Text { text, signature } => {
                                    let mut obj = serde_json::json!({
                                        "type": "thinking",
                                        "thinking": text,
                                    });
                                    if let Some(sig) = signature {
                                        obj["thinkingSignature"] = serde_json::Value::String(sig.clone());
                                    }
                                    pi_content.push(obj);
                                }
                                _ => {}
                            }
                        }
                    }
                    AssistantContent::ToolCall(tc) => {
                        pi_content.push(serde_json::json!({
                            "type": "toolCall",
                            "id": tc.id,
                            "name": tc.function.name,
                            "arguments": tc.function.arguments,
                        }));
                    }
                    _ => {}
                }
            }
            let mut msg = serde_json::json!({
                "role": "assistant",
                "content": pi_content,
                "timestamp": Utc::now().timestamp_millis(),
            });
            if let Some(id) = id {
                msg["responseId"] = serde_json::Value::String(id.clone());
            }
            msg
        }
    }
}
```

---

## Implementation Plan — Phase 1: Storage Layer ✅

**Goal:** Types + JSONL I/O + repo + unit tests. No changes to `agent.rs`.

**Status: Complete.** All types, storage, and repo implemented. 30 unit tests
pass. 78 real pi agent session files (46,825 entries) read with zero errors.

### Checklist

- [x] **1.1 Add dependencies to `Cargo.toml`**
- [x] **1.2 Create `src/session/mod.rs`**
- [x] **1.3 Implement `src/session/types.rs`** — all types, encoding,
      convenience constructors, 13 tests
- [x] **1.4 Implement `src/session/storage.rs`** — JSONL I/O, index building,
      implicit leaf fallback, 8+2 tests
- [x] **1.5 Implement `src/session/repo.rs`** — create/open/list/delete/fork, 6
      tests
- [x] **1.6 Integration test: pi wire format compatibility** — manual pi-format
      JSONL, snake_case leak check
- [x] **1.7 Cross-compatibility test: read actual pi session store**
  - `src/session/pi_compat_test.rs`, gated with `#[ignore]`
  - Verified: **78 files, 46,825 entries, 78 trees, 0 errors**

### Lessons Learned from Phase 1

1. **Implicit leaf pointer.** pi never writes `type:"leaf"` entries. The leaf is
   implicitly the last entry's ID (pi's `leafIdAfterEntry()` returns `entry.id`
   for every non-leaf type). `build_index()` must track `last_entry_id` and fall
   back when no explicit leaf entry exists. A `saw_leaf_entry` flag ensures
   explicit leaf entries (including `None` clears) are still authoritative for
   prior's own sessions.

2. **Read-only mount needed.** pi's session directory (`~/.pi/agent/sessions/`)
   is often on a read-only filesystem. `JsonlSessionStorage` needs
   `open_readonly()` to inspect external stores without requesting write access.

3. **8-char entry IDs.** pi uses 8-character hex strings for entry IDs (e.g.
   `"84c3124c"`), not UUIDv7. Our `SessionEntry.id` is `String`, so this works
   without changes.

4. **Separate header write.** The session header must be written as a raw JSON
   line (not via `append_entry`), because it has a different structure from
   `SessionEntry`. pi does the same.

5. **No `decodeCwd` in pi.** pi has no `decode_cwd` function. The directory name
   is write-only; the real CWD is always read from the session header's `cwd`
   field.

**Estimated size:** ~1020 lines of Rust + ~450 lines of tests (actual)

---

## Implementation Plan — Phase 2: Session Object ✅

**Goal:** `Session` wrapper with context building (pi AgentMessage → rig
Message), append helpers, and forking.

**Status: Complete.** 13 unit tests pass. pi↔rig round-trip verified.
Tool-result batching verified.

### Lessons Learned from Phase 2

1. **`Session` must be `Send + Sync`.** Both `storage` and `index` are wrapped
   in `Arc<AsyncMutex<>>` so append helpers take `&self` not `&mut self`.
   Essential for Phase 3 where `Session` is shared by reference.

2. **`build_context()` is async.** Index access goes through an async mutex, so
   `get_branch()` and `build_context()` are async methods. This affects
   `CodeAgent::prompt()` and all callers.

3. **Leaf pointer updated automatically.** Every append helper writes a
   `type:"leaf"` entry to storage AND updates the in-memory index. No separate
   `set_leaf()` call needed.

4. **Tool-result batching works.** Forward scan in `extract_messages()`
   correctly groups consecutive pi `toolResult` entries into a single
   `Message::User` with multiple `UserContent::ToolResult` items.

5. **`rig_message_to_pi_entries()` returns `Vec<SessionEntry>`.** A single rig
   `Message::User` with `ToolResult` items produces multiple pi entries. The
   append helper iterates and writes each.

6. **`Message::assistant()` convenience doesn't capture `id`.** When the agent
   returns a `responseId`, construct `Message::Assistant` manually with
   `id: Some(...)`.

### Checklist

- [x] **2.1 Implement `src/session/session.rs`** — `Session::new()`,
      `get_branch()`, `build_context()`, `apply_transforms()`, all append
      helpers, in-memory leaf updates
- [x] **2.2 pi↔rig conversion** — `pi_message_to_rig()`,
      `rig_message_to_pi_entries()`, toolResult batching, 9 round-trip tests
- [x] **2.3 Integration tests** — branch→context, metadata derivation,
      append→re-read, session name

**Estimated size:** ~625 lines of Rust + ~550 lines of tests (actual)

---

## Implementation Plan — Phase 3: Wire Into Agent Loop ✅

**Goal:** Replace `history: Vec<Message>` with `Session`.

**Status: Complete.** 267 tests pass (1 pre-existing repl failure). 43 session
tests + 9 agent tests all pass. Session created at startup via
`JsonlSessionRepo`, all paths (success, cancel, error, `!`/`!!`) persist to
session.

### Checklist

- [x] **3.1 Session initialization** — `~/.prior/sessions/` at startup,
      `Session` via `repo.create(cwd)`, stored as `Option<Session>`
- [x] **3.2 `prompt()` signature** — `session: Option<&Session>`, prior context
      from `build_context().await`, stripping before LLM
- [x] **3.3 Persistence** — `session.append_message()` for all paths,
      `normalize_call_ids()` applied, pre-strip storage
- [x] **3.4 Slash commands** — `/model` → `append_model_change()`, `/thinking` →
      `append_thinking_level_change()`
- [x] **3.5 Fallback** — `run_once()` passes `None`; no session = empty
      context + no persistence
- [x] **3.6 Regression tests** — All 9 existing agent tests pass unchanged

> **Superseded in part (2026):** persistence is now *incremental* — the user
> prompt is written at dispatch, each completion turn is written by the start of
> the next call, and only the in-flight response waits for a turn exit. The
> transcript (`TurnTranscript`) is the single source of truth; `persist_turn`,
> `normalize_call_ids`, and the `final_history`/`cancelled_history` branches are
> gone. See `doc/261022_incremental_session_write.md`.

### Lessons Learned

1. **`prompt()` signature change was clean.** `&mut Vec<Message>` → `&Session`
   works because Session uses interior mutability (Arc<AsyncMutex<>>).
2. **Stripping moved to context time.** Previously `persist_turn()` stripped
   before storing. With sessions, `build_context()` returns pre-strip messages;
   stripping is applied before sending to LLM.
3. **All error paths persist.** Non-cancellation streaming errors and `Esc`
   cancellation both call `session.append_message()` for partial turns.
4. **`persist_turn()` kept for test compat** with `#[allow(dead_code)]`. Tests
   use it directly; will be removed in Phase 4.

**Estimated size:** ~127 lines changed in `agent.rs`

---

## Implementation Plan — Phase 4: `/session` Command ✅

**Goal:** A single `/session` entry point for listing, naming, resuming,
forking, and deleting sessions — TUI overlay only, no separate slash commands
for each action.

No extra confirmation messages. The input line tells the story.

**Status: Complete** via the v2.5 slash menu engine (not the original v2.0
overlay plan). The original Phase 4 checklist items (4.1–4.7) were implemented
through the v2.5 `SlashDotNode` tree: `SessionListNode` + action leaf nodes,
`AsyncOp` dispatch, v2.0 session overlay removal. Design:
`slashdot/session_design.md`. 19 unit tests.

### UI Flow

```text
┌──────────────────────────────────────────────────────────────┐
│ /session                              (commands filter)      │
├──────────────────────────────────────────────────────────────┤
│  ◉  today 14:32    * prior session storage   2 files      │
│     today 11:07      parent: rename experiments              │
│     yesterday         test-harness rewrite                   │
│     Jul 14            parent: initial design                 │
│     Jul 10            add readline support                   │
│     ...                                                      │
└──────────────────────────────────────────────────────────────┘
```

Typing a name after `/session` filters the list. Hitting Enter or Tab on a
session opens the per-session action menu:

```text
┌──────────────────────────────────────────────────────────────┐
│ /session "prior session storage"                          │
├──────────────────────────────────────────────────────────────┤
│  Session: prior session storage                           │
│  Working dir: /home/gabriel/src/prior2                    │
│  238 turns | created today 14:32 | parent: fork-target       │
│                                                              │
│  ◉  resume          (current session, greyed out)            │
│     fork                                                     │
│     rename                                                   │
│     delete         (greyed out if not current)               │
└──────────────────────────────────────────────────────────────┘
```

Selecting an action sets the input line to a full-sentence form and commits it
(no additional confirmation):

- **Action**: resume
  - **Input after Enter**: `/session "prior session storage" resume`
  - **Behaviour**: Switch to selected session
- **Action**: fork
  - **Input after Enter**: `/session "prior session storage" fork`
  - **Behaviour**: Fork from the leaf, create new session, switch
- **Action**: rename
  - **Input after Enter**: `/session "prior session storage" rename`
  - **Behaviour**: Pre-fills input for typing new name
- **Action**: delete
  - **Input after Enter**: `/session "prior session storage" delete`
  - **Behaviour**: Remove the file, remove from list

Typing these directly (without the overlay) also works:
`/session <name> resume`, `/session <name> fork`,
`/session <name> rename <newname>`, `/session <name> delete`.

### Implementation Details

**New overlay kind codes:**

- `10` — session list (top-level `/session`)
- `11` — per-session action menu

**`InputAction` changes:**
No new variants needed. Everything is expressed as text that flows through the
existing `Submit` path:

```rust
// What the user types → what run_interactive() dispatches
"/session"                     → overlay opens (kind 10)
"/session prior session storage resume"  → resume session
"/session prior session storage fork"    → fork session
"/session prior session storage rename"  → overlay closes, input set to
                                             `/session "prior session storage" rename `
                                             (user types new name, then submit)
"/session prior session storage rename my-new-name" → rename session
"/session prior session storage delete"  → delete session file
```

**`ReplCtx` new fields:**

```rust
pub sessions_list: Vec<SessionMetadata>,
pub current_session_id: String,
pub branch_messages: Vec<(String, String)>,  // (entry_id, preview) for fork
```

### Checklist

**Implemented via v2.5 slash menu engine** (`slashdot/session_design.md`).
All functional goals achieved through `SlashDotNode` tree rather than the
original v2.0 overlay plan below.

- [x] **4.1 Store repo in `run_interactive()`**
  - [x] Keep `repo: Option<JsonlSessionRepo>` alongside
        `session: Option<Session>`
  - [x] Populate session data from `repo` via `SessionListNode::data()`
  - [x] Session operations dispatch via `AsyncOp` enum on `MenuShared`

- [x] **4.2 `/session` in menu tree**
  - [x] Registered in `v25_menu.rs` root menu as `StaticListingNode`
  - [x] Session node + action children built in `v25_session.rs`

- [x] **4.3 Implement session list** (via `SessionListNode::data()`)
  - [x] Rows: `◉` = current via `active_ids`. Columns: description, turns, age
  - [x] Sort: current session first, then by timestamp descending
  - [x] Relative timestamps ("3h", "2d")
  - [x] Named sessions show name; unnamed show first prompt

- [x] **4.4 Implement per-session action menu** (via `SessionListNode` children)
  - [x] Header shows session info table + First/Last prompts
  - [x] Options: resume | fork | rename | delete (action leaf nodes)
  - [x] Resume hidden for current session (guard in `data()`)
  - [x] Delete guarded for current session (error in `prepare()`)

- [x] **4.5 Dispatch** (via `AsyncOp` on `MenuShared.pending_op`)
  - [x] Agent loop handles `Resume`, `Fork`, `Rename`, `Delete` variants
  - [x] Reuses existing `handle_session_command()` logic

- [x] **4.6 Direct text input** — handled by v2.5 engine section parsing

- [x] **4.7 Tests** — 19 unit tests (listing, traversal, submit, guards,
      helpers)

- [ ] **4.5 Dispatch in `run_interactive()` (`agent.rs`)**
  - [ ] Before the `Submit` handler, check if input matches `/session` pattern
  - [ ] Parse: `"/session <name> resume"` → name + action
  - [ ] Name may be quoted (`"..."`) to handle spaces; unquote if needed
  - [ ] `resume`:
    - [ ] Call `repo.find_by_name(name)` → get `SessionMetadata`
    - [ ] Call `repo.open(&meta)` → new `(storage, index)`
    - [ ] Replace current `session` with `Session::new(storage, index)`
    - [ ] Update `ReplCtx.current_session_id`
  - [ ] `fork`:
    - [ ] Get current `SessionMetadata` from repo's list for the selected
          session
    - [ ] Call `repo.fork(&source_meta, leaf_id, cwd)` → new storage
    - [ ] Build index, create `Session::new(...)`, replace current session
  - [ ] `rename <newname>`:
    - [ ] Find the session in repo's list by name
    - [ ] If it's the current session: `session.set_session_name(newname)`
    - [ ] If it's a different session: open it, rename, close (or defer — Phase
          5)
  - [ ] `delete`:
    - [ ] Find session by name, call `repo.delete(&meta)`
    - [ ] Refresh `ReplCtx.sessions_list`

- [ ] **4.6 Direct text input (no overlay)**
  - [ ] `/session <name> resume` — parse and dispatch as above
  - [ ] `/session <name> fork` — same
  - [ ] `/session <name> rename <newname>` — same
  - [ ] `/session <name> delete` — same

- [ ] **4.7 Update tests**
  - [ ] `slash.rs`: update `registry_has_eight_commands` → nine
  - [ ] Add `parse_slash` tests for `"session"`
  - [ ] `render_help` tests include `/session`

### Notes

- Session switching (resume) replaces the `Session` object in
  `run_interactive()`. The in-memory `index` is rebuilt via `repo.open()`. No
  in-flight state is lost because resume only happens between turns (not
  mid-prompt).
- `fork` creates a new session file and sets `parentSession` in the header to
  the source file path. The new session becomes the current session.
- `delete` removes the `.jsonl` file. If it's the current session, greyed out in
  the menu; the user must resume a different session first.
- Session name lookup is O(n) over the list (n < 100 in practice). For large
  stores, a name index could be added later.
- The overlay uses relative timestamps ("today", "yesterday", "Jul 14") to keep
  rows compact. Full timestamp is shown in the action menu header.

**Estimated size:** ~450 lines across `slash.rs`, `repl.rs`, `agent.rs`

---

## Implementation Plan — Phase 5: (Merged into Phase 4)

The `/session` command subsumes what was previously planned as a separate
"Session Selector UI" (Phase 5). The session list overlay (kind 10) and
per-session action menu (kind 11) together provide the browsing, selection,
naming, forking, and deletion functionality originally planned for Phase 5.

No separate Phase 5 is needed.

---

## Key Design Decisions (Updated)

### 1. pi Wire Compatibility

**Decision:** prior's JSONL files are **identical** to pi's (camelCase, same
tags, same field layout). This means:

- A pi session file can be opened in prior with zero transformation
- A prior session file can be opened in pi with zero transformation
- The serde attributes use `#[serde(rename_all = "camelCase")]` at all levels
- `Message` content is stored as raw `serde_json::Value` in the pi
  `AgentMessage` format, not rig's format

The only difference is entry ID length: pi uses 8-char hex strings, prior
uses full UUIDv7 strings. The importer accepts any string.

### 2. JSONL File Format (Reaffirmed)

Storage format remains JSONL (same as pi). See original document for the
comparison table.

### 3. Entry Tree Model (Reaffirmed)

Tree model with `parentId` chains, matching pi's design. Enables forking,
branching, labels, and compaction without ad-hoc hacks.

### 4. Pre-Strip Storage

Messages are stored **before** reasoning stripping. The stripping
(`maybe_strip_reasoning`) is applied only in `build_context()` before sending to
the LLM. This ensures:

- Full reasoning is available for re-display of past sessions
- `/fork` from a reasoning-rich point preserves the full content
- pi compatibility is maintained (pi stores full messages)

### 5. Leaf Pointer Updates

Every append operation that changes the current position updates the leaf
pointer. This is done by appending a new `leaf` entry after the content entry,
keeping the leaf history as part of the entry tree.

### 6. UUIDv7 for Entry IDs

Full UUIDv7 strings (not pi's abbreviated 8-char format). The `uuid` crate with
`v7` feature provides `Uuid::now_v7()`. A `uuid::timestamp::context::Context`
(monotonic counter) should be created at agent startup and shared across the
session to guard against clock rewinds.

### 7. Resume Semantics

When resuming a session:

- **Replacement, not overlay.** The current `Session` object is dropped and
  replaced with the resumed one. The session file is re-read and the in-memory
  `SessionIndex` is rebuilt from scratch.
- **LLM receives full context.** `build_context()` returns every message from
  leaf to root. The first prompt after resume sees the entire prior conversation
  as history.
- **No content re-displayed.** No messages are re-rendered to the user's
  terminal. Rationale:
  - Terminal scrollback is the user's visual record. If the terminal is still
    open, everything is there.
  - Re-rendering hundreds of markdown messages on resume is expensive and noisy.
  - The session name in the status line (future Phase 5) is sufficient visual
    feedback.
- **Next prompt continues normally.** The resumed session is indistinguishable
  from one that was never interrupted. Persistence via `append_message()` works
  the same way after resume as before.
- **Model prefill on resume.** After the default replay
  (`session_resume_replay_turns`, default 3), the next input prompt is
  pre-filled with `/model <provider>:<model_id>` whenever the resumed
  session has a `ModelChange` entry on its active leaf→root path, or the
  session header carries `provider`/`modelId`. Pressing Enter accepts
  (rebuilds the agent with that model); Esc dismisses; typing narrows the
  model picker. The session-running agent itself is *not* silently switched
  on resume — the user gets to choose. The thinking level is not yet
  surfaced via prefill (single input field; see `doc/todo.md`).

Resume is purely about restoring the LLM's context window. Visual continuity
is the terminal's responsibility.

---

## Open Questions (Updated)

### Q1: Compaction Integration

Deferred to the compaction design (`doc/compaction.md`). The session storage
provides `apply_transforms()` as the integration point. When compaction runs:

1. It identifies a range of entries to compact
2. Generates a `Compaction` entry with `firstKeptEntryId` pointing past the
   compacted range
3. Appends the compaction entry to the session file
4. `build_context()` skips entries before `firstKeptEntryId`

The existing entries remain in the file (no deletion) — compaction is purely a
context-building transform.

### Q2: Concurrent Sessions

No locking required. Each session writes to a uniquely-named JSONL file (UUID in
filename). The only shared resource is the session directory; `list()` is atomic
at the OS level.

### Q3: pi Import Path

Planned as a future `/import-pi-session <path>` command. Since the formats are
wire-compatible, this is almost trivial: copy the file into the prior
sessions directory, generate a new header line. The only non-trivial step is
mapping pi's 8-char entry IDs to full UUIDv7 — but since prior's
`SessionEntry.id` is `String` (not UUID), even that is straightforward.

### Q4: Error Recovery

On open, if the last line is not valid JSON (truncated write), skip it and log a
warning. All preceding lines are valid. This is handled in `build_index()`.

---

## Required Dependencies

```toml
# Add to Cargo.toml
uuid = { version = "1.0", features = ["v7", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
```

(Existing: `serde`, `serde_json`, `tokio`, `async-trait`)

---

## File Structure (Final)

```text
src/
  session/
    mod.rs          # Re-exports + module declarations
    types.rs        # SessionHeader, SessionEntry, SessionEntryContent,
                    #   SessionMetadata, SessionIndex, SessionContext,
                    #   encode_cwd, decode_cwd, session_filename
    storage.rs      # SessionStorage trait, JsonlSessionStorage
    repo.rs         # SessionRepo trait, JsonlSessionRepo, SessionHandle
    session.rs      # Session struct (context building, append helpers,
                    #   pi ↔ rig message conversion)
  agent.rs          # Modified: uses Session instead of Vec<Message>
  repl.rs           # Modified: session selector and fork picker overlays
  slash.rs          # Modified: new SlashKind variants
```

---

## Migration Path (Summary)

| Phase | What                  | Tests                    | Risk   |
| ----- | --------------------- | ------------------------ | ------ |
| ✅ 1  | Types + Storage       | 30 unit, 78 pi files     | Low    |
| ✅ 2  | Session Object        | 13 unit, full round-trip | Low    |
| ✅ 3  | Wire into agent loop  | 267 pass, 9 agent        | Medium |
| ✅ 4  | `/session` command    | 19 session, manual       | Medium |
| ✅ 5  | (merged into Phase 4) | —                        | —      |
