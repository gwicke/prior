//! High-level `Session` wrapper with context building and append helpers.
//!
//! The `Session` struct wraps a `(JsonlSessionStorage, SessionIndex)` pair and
//! provides:
//!
//! - `build_context()` — walks the leaf→root branch, converts pi-format messages
//!   to rig `Message` types, applies transforms (compaction), and derives
//!   thinking/model metadata.
//! - Append helpers: `append_message()`, `append_model_change()`, etc.
//! - `pi_message_to_rig()` / `rig_message_to_pi()` — wire-format conversion.
//!
//! Messages are stored as raw `serde_json::Value` in pi's `AgentMessage` format.
//! The conversion to rig's `Message` happens only in `build_context()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::Utc;
use rig_core::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall,
    ToolFunction, ToolResult, ToolResultContent, UserContent,
};
use rig_core::OneOrMany;
use tokio::sync::Mutex as AsyncMutex;

use crate::session::storage::{JsonlSessionStorage, SessionStorage};
use crate::session::types::*;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// High-level session operations. Wraps a storage handle and an in-memory index.
pub struct Session {
    /// Append-only JSONL storage (thread-safe).
    pub(crate) storage: Arc<AsyncMutex<JsonlSessionStorage>>,
    /// In-memory index (rebuilt on open). Wrapped in Arc+Mutex so append
    /// helpers can update the leaf pointer through `&self`.
    pub(crate) index: Arc<AsyncMutex<SessionIndex>>,
    /// Cached session ID, set on construction — avoids a lock for the common
    /// case of reading the session identifier.
    pub(crate) session_id: String,
    /// Whether the session has been persisted to disk (header written).
    /// Set `true` on the first append. Avoids empty `.jsonl` files for
    /// sessions where no turn was ever taken.
    persisted: AtomicBool,
    /// Tracks `call_id → tool_name` across `append_message` calls so that
    /// tool result entries written to JSONL carry the correct `"toolName"`
    /// field (pi-compatible) instead of the placeholder `"tool"`.
    tool_name_by_call_id: std::sync::Mutex<HashMap<String, String>>,
}

impl Session {
    /// Construct a `Session` from an existing storage handle and index.
    ///
    /// Typically returned by `JsonlSessionRepo::open()`. The session is NOT
    /// persisted to disk until the first `append_message()` call (prevents
    /// empty `.jsonl` files for sessions with no turns).
    pub fn new(storage: JsonlSessionStorage, index: SessionIndex) -> Self {
        let session_id = index.header.id.clone();
        Self {
            storage: Arc::new(AsyncMutex::new(storage)),
            index: Arc::new(AsyncMutex::new(index)),
            session_id,
            persisted: AtomicBool::new(false),
            tool_name_by_call_id: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Construct a `Session` that is already persisted (e.g. after resuming
    /// an existing session or loading from a file). The header write is
    /// skipped on the first `append_message()`.
    pub fn new_persisted(storage: JsonlSessionStorage, index: SessionIndex) -> Self {
        let session_id = index.header.id.clone();
        Self {
            storage: Arc::new(AsyncMutex::new(storage)),
            index: Arc::new(AsyncMutex::new(index)),
            session_id,
            persisted: AtomicBool::new(true),
            tool_name_by_call_id: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Ensure the session header is written to disk. No-op after the first
    /// call. Called automatically by `append_message()` — callers do not
    /// need to invoke this manually.
    pub async fn ensure_persisted(&self) -> crate::Result<()> {
        if self.persisted.load(Ordering::Acquire) {
            return Ok(());
        }
        let storage = self.storage.lock().await;
        let index = self.index.lock().await;
        storage.write_header(&index.header).await?;
        self.persisted.store(true, Ordering::Release);
        Ok(())
    }

    /// The session's unique identifier (UUIDv7 from the header).
    pub fn get_session_id(&self) -> &str {
        &self.session_id
    }

    /// Rebuild the in-memory index from the storage file.
    pub async fn refresh_index(&self) -> crate::Result<()> {
        let storage = self.storage.lock().await;
        let mut index = self.index.lock().await;
        *index = storage.build_index().await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Context building
    // ------------------------------------------------------------------

    /// Walk from leaf to root, collecting entries in root→leaf order.
    pub async fn get_branch(&self) -> Vec<SessionEntry> {
        let index = self.index.lock().await;
        let leaf_id = match &index.leaf_id {
            Some(id) => id.clone(),
            None => return vec![],
        };
        index.get_path_to_root(&leaf_id)
    }

    /// Build the LLM context from the current branch.
    ///
    /// 1. Walk leaf → root collecting entries
    /// 2. Apply transforms (compaction filtering)
    /// 3. Convert pi AgentMessage entries → rig Message types
    /// 4. Derive thinking level and model from the branch
    pub async fn build_context(&self) -> crate::Result<SessionContext> {
        let branch = self.get_branch().await;
        let transformed = self.apply_transforms(branch);
        let messages = extract_messages(&transformed);
        let thinking_level = derive_thinking_level(&transformed);
        let model = derive_model(&transformed);

        // If deriving from entries yielded nothing, fall back to the session
        // header's provider/model/thinking metadata (pi stores these at the
        // top level of the header, not as separate entries).
        let index = self.index.lock().await;
        let thinking_level = thinking_level.or_else(|| index.header.thinking_level.clone());
        let model = model.or_else(|| index.header.model_id.clone());
        Ok(SessionContext {
            messages,
            thinking_level,
            model,
        })
    }

    /// Apply context-level transforms on the branch.
    ///
    /// Currently supports compaction: when a `Compaction` entry is on the
    /// branch, all entries before `first_kept_entry_id` are removed and
    /// replaced with a synthetic user message carrying the summary text.
    ///
    /// For Phase 2, this is an identity transform (no compaction entries
    /// exist yet).
    fn apply_transforms(&self, entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
        // Find the most recent compaction entry (closest to leaf).
        let compaction = entries.iter().rev().find_map(|entry| {
            if let SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } = &entry.content
            {
                Some((summary.clone(), first_kept_entry_id.clone()))
            } else {
                None
            }
        });

        let Some((summary, first_kept_id)) = compaction else {
            return entries;
        };

        // Find the index of the first kept entry.
        let keep_idx = entries.iter().position(|e| e.id == first_kept_id);
        let keep_idx = match keep_idx {
            Some(i) => i,
            None => return entries, // fallback: no transform
        };

        // Build the result: summary message + kept entries.
        let mut result: Vec<SessionEntry> = Vec::new();

        // Create a synthetic message entry carrying the summary text.
        let summary_entry = SessionEntry {
            id: uuid::Uuid::now_v7().to_string(),
            parent_id: None,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": format!("[Compacted: {}]", summary)}]
                }),
            },
        };
        result.push(summary_entry);

        // Append entries from the keep point onwards.
        result.extend_from_slice(&entries[keep_idx..]);

        result
    }

    // ------------------------------------------------------------------
    // Append helpers
    // ------------------------------------------------------------------

    /// Append a user or assistant message to the session.
    ///
    /// Converts the rig `Message` to pi's `AgentMessage` format before storage.
    pub async fn append_message(&self, msg: &Message) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let entry_id = uuid::Uuid::now_v7().to_string();
        let parent_id = {
            let index = self.index.lock().await;
            index.leaf_id.clone()
        };

        // Before serialising, extract tool call names from an Assistant message
        // so tool result entries can carry the correct "toolName".
        if let Message::Assistant { content, .. } = msg {
            let mut map = self.tool_name_by_call_id.lock().unwrap();
            for item in content.iter() {
                if let AssistantContent::ToolCall(tc) = item {
                    map.insert(tc.id.clone(), tc.function.name.clone());
                }
            }
        }

        let mut pi_entries = rig_message_to_pi_entries(msg, &entry_id, &parent_id);

        // Post-process tool result entries: set "toolName" from the call_id
        // lookup map instead of the placeholder "tool".
        {
            let map = self.tool_name_by_call_id.lock().unwrap();
            for pi_entry in &mut pi_entries {
                if let SessionEntryContent::Message { message } = &mut pi_entry.content {
                    if message.get("role").and_then(|v| v.as_str()) == Some("toolResult") {
                        if let Some(call_id) =
                            message.get("toolCallId").and_then(|v| v.as_str())
                        {
                            if let Some(tool_name) = map.get(call_id) {
                                message["toolName"] =
                                    serde_json::Value::String(tool_name.clone());
                            }
                        }
                    }
                }
            }
        }

        let storage = self.storage.lock().await;
        for pi_entry in &pi_entries {
            storage.append_entry(pi_entry).await?;
        }

        // Update leaf pointer to the last entry written.
        let last_id = pi_entries.last().map(|e| e.id.clone());
        let leaf_entry = SessionEntry::new_leaf(last_id.clone(), None);
        storage.append_entry(&leaf_entry).await?;

        // Update in-memory index: insert entries, track parent-child
        // relationships, and advance the leaf pointer.  Without this the
        // in-memory `entries` map becomes stale and `get_branch()` /
        // `build_context()` cannot follow the parent chain — every
        // subsequent turn would see an empty conversation history.
        {
            let mut index = self.index.lock().await;
            for pi_entry in &pi_entries {
                index.entries.insert(pi_entry.id.clone(), pi_entry.clone());
                if let Some(ref pid) = pi_entry.parent_id {
                    index.children.entry(pid.clone()).or_default().push(pi_entry.id.clone());
                }
            }
            index.leaf_id = last_id.clone();
        }

        Ok(last_id.unwrap_or(entry_id))
    }

    /// Append a model/provider change entry.
    pub async fn append_model_change(&self, provider: &str, model_id: &str) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = {
            let index = self.index.lock().await;
            index.leaf_id.clone()
        };
        let entry = SessionEntry::new_model_change(
            provider.to_string(),
            model_id.to_string(),
            parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;

        let leaf = SessionEntry::new_leaf(Some(id.clone()), None);
        storage.append_entry(&leaf).await?;

        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
            index.leaf_id = Some(id.clone());
        }
        Ok(id)
    }

    /// Append a thinking level change entry.
    pub async fn append_thinking_level_change(&self, level: &str) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = {
            let index = self.index.lock().await;
            index.leaf_id.clone()
        };
        let entry = SessionEntry::new_thinking_level_change(
            level.to_string(),
            parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;

        let leaf = SessionEntry::new_leaf(Some(id.clone()), None);
        storage.append_entry(&leaf).await?;

        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
            index.leaf_id = Some(id.clone());
        }
        Ok(id)
    }

    /// Write a session name entry.
    pub async fn set_session_name(&self, name: &str) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = {
            let index = self.index.lock().await;
            index.leaf_id.clone()
        };
        let entry = SessionEntry::new_session_info(
            Some(name.to_string()),
            parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;

        let leaf = SessionEntry::new_leaf(Some(id.clone()), None);
        storage.append_entry(&leaf).await?;

        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
            index.leaf_id = Some(id.clone());
        }
        Ok(id)
    }

    // ------------------------------------------------------------------
    // Tree navigation
    // ------------------------------------------------------------------

    /// Look up a single entry by ID.
    pub async fn get_entry(&self, id: &str) -> Option<SessionEntry> {
        let index = self.index.lock().await;
        index.entries.get(id).cloned()
    }

    /// Return the current leaf entry ID.
    pub async fn get_leaf_id(&self) -> Option<String> {
        let index = self.index.lock().await;
        index.leaf_id.clone()
    }

    /// Build a recursive tree from the in-memory index.
    pub async fn get_tree(&self) -> Vec<SessionTreeNode> {
        let index = self.index.lock().await;
        let leaf_id = index.leaf_id.clone();
        index.build_tree(leaf_id.as_deref())
    }

    /// Move the leaf pointer to a new entry.
    ///
    /// Writes a `Leaf` pointer record to storage and updates `leaf_id` only.
    /// Matches the existing append helpers: the `Leaf` entry is a pointer
    /// record, NOT a tree node — written with parent `None` and deliberately
    /// never inserted into `entries`/`children`.
    pub async fn move_leaf(&self, new_leaf_id: Option<String>) -> crate::Result<()> {
        self.ensure_persisted().await?;
        let leaf_entry = SessionEntry::new_leaf(new_leaf_id.clone(), None);
        {
            let storage = self.storage.lock().await;
            storage.append_entry(&leaf_entry).await?;
        }
        {
            let mut index = self.index.lock().await;
            index.leaf_id = new_leaf_id;
        }
        Ok(())
    }

    /// Write a BranchSummary entry and advance leaf.
    pub async fn append_branch_summary(
        &self, from_id: &str, summary: &str,
    ) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = { self.index.lock().await.leaf_id.clone() };
        let entry = SessionEntry::new_branch_summary(
            from_id.to_string(), summary.to_string(), parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;
        let leaf = SessionEntry::new_leaf(Some(id.clone()), None);
        storage.append_entry(&leaf).await?;
        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
            index.leaf_id = Some(id.clone());
        }
        Ok(id)
    }

    /// Write a Compaction entry and advance leaf.
    pub async fn append_compaction(
        &self, summary: &str, first_kept_entry_id: &str, tokens_before: u64,
    ) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = { self.index.lock().await.leaf_id.clone() };
        let entry = SessionEntry::new_compaction(
            summary.to_string(), first_kept_entry_id.to_string(), tokens_before, parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;
        let leaf = SessionEntry::new_leaf(Some(id.clone()), None);
        storage.append_entry(&leaf).await?;
        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
            index.leaf_id = Some(id.clone());
        }
        Ok(id)
    }

    /// Write a Label entry attached to a target entry.
    /// Does NOT advance the leaf (labels are metadata, not navigation points).
    pub async fn append_label(
        &self, target_id: &str, label: &str,
    ) -> crate::Result<String> {
        self.ensure_persisted().await?;
        let parent_id = { self.index.lock().await.leaf_id.clone() };
        let entry = SessionEntry::new_label(
            target_id.to_string(), Some(label.to_string()), parent_id.clone(),
        );
        let id = entry.id.clone();
        let storage = self.storage.lock().await;
        storage.append_entry(&entry).await?;
        {
            let mut index = self.index.lock().await;
            index.entries.insert(entry.id.clone(), entry);
            if let Some(ref pid) = parent_id {
                index.children.entry(pid.clone()).or_default().push(id.clone());
            }
        }
        Ok(id)
    }

    /// Access the storage (for repo-level operations like forking).
    pub fn storage(&self) -> Arc<AsyncMutex<JsonlSessionStorage>> {
        self.storage.clone()
    }

    /// Access the index.
    pub fn index(&self) -> &Arc<AsyncMutex<SessionIndex>> {
        &self.index
    }
}

// ---------------------------------------------------------------------------
// pi AgentMessage → rig Message conversion
// ---------------------------------------------------------------------------

/// Convert a branch of session entries to rig Messages.
///
/// pi stores each `toolResult` as a separate entry. rig batches all tool
/// results for a turn into a single `Message::User` with multiple
/// `UserContent::ToolResult` items. This function scans ahead to group
/// consecutive `toolResult` entries.
pub(crate) fn extract_messages(entries: &[SessionEntry]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut tool_results: Vec<serde_json::Value> = Vec::new();

    for entry in entries {
        let content = match &entry.content {
            SessionEntryContent::Message { message } => message,
            _ => continue,
        };

        let role = content.get("role").and_then(|r| r.as_str());
        match role {
            Some("toolResult") => {
                tool_results.push(content.clone());
            }
            Some("user") | Some("assistant") => {
                // Flush any buffered tool results first.
                if !tool_results.is_empty() {
                    if let Some(msg) =
                        pi_toolresults_to_rig_message(std::mem::take(&mut tool_results))
                    {
                        messages.push(msg);
                    }
                }

                if let Some(msg) = pi_message_to_rig(content) {
                    messages.push(msg);
                }
            }
            _ => {}
        }
    }

    // Flush any trailing tool results.
    if !tool_results.is_empty() {
        if let Some(msg) = pi_toolresults_to_rig_message(tool_results) {
            messages.push(msg);
        }
    }

    messages
}

/// Convert a single pi AgentMessage (user or assistant) to a rig Message.
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
                id: value
                    .get("responseId")
                    .and_then(|v| v.as_str().map(String::from)),
                content: OneOrMany::many(content).ok()?,
            })
        }
        _ => None,
    }
}

/// Convert a batch of pi toolResult entries into a single rig Message::User.
fn pi_toolresults_to_rig_message(results: Vec<serde_json::Value>) -> Option<Message> {
    let mut tool_results: Vec<UserContent> = Vec::new();
    for result in &results {
        if let Some(tr) = pi_toolresult_to_rig(result) {
            tool_results.push(UserContent::ToolResult(tr));
        }
    }
    if tool_results.is_empty() {
        return None;
    }
    Some(Message::User {
        content: OneOrMany::many(tool_results).ok()?,
    })
}

/// Convert a pi toolResult entry to a rig ToolResult.
fn pi_toolresult_to_rig(value: &serde_json::Value) -> Option<ToolResult> {
    let id = value.get("toolCallId")?.as_str()?.to_string();
    let content = pi_content_to_toolresult_content(value.get("content")?)?;

    Some(ToolResult {
        id,
        call_id: None,
        content: OneOrMany::many(content).ok()?,
    })
}

/// Convert pi content array to rig UserContent items.
fn pi_content_to_user_content(content: &serde_json::Value) -> Option<Vec<UserContent>> {
    let array = content.as_array()?;
    let mut items = Vec::new();
    for item in array {
        let type_str = item.get("type")?.as_str()?;
        match type_str {
            "text" => {
                let text = item.get("text")?.as_str()?;
                items.push(UserContent::Text(Text::new(text)));
            }
            _ => {}
        }
    }
    Some(items)
}

/// Convert pi content array to rig AssistantContent items.
fn pi_content_to_assistant_content(
    content: &serde_json::Value,
) -> Option<Vec<AssistantContent>> {
    let array = content.as_array()?;
    let mut items = Vec::new();
    for item in array {
        let type_str = item.get("type")?.as_str()?;
        match type_str {
            "text" => {
                let text = item.get("text")?.as_str()?;
                items.push(AssistantContent::Text(Text::new(text)));
            }
            "thinking" => {
                let thinking = item.get("thinking")?.as_str()?;
                let signature = item
                    .get("thinkingSignature")
                    .and_then(|s| s.as_str().map(String::from));
                items.push(AssistantContent::Reasoning(
                    Reasoning::new_with_signature(&thinking, signature),
                ));
            }
            "toolCall" => {
                let id = item.get("id")?.as_str()?.to_string();
                let name = item.get("name")?.as_str()?.to_string();
                let arguments = item.get("arguments")?.clone();
                let tc = ToolCall {
                    id,
                    call_id: None,
                    function: ToolFunction::new(name, arguments),
                    signature: None,
                    additional_params: None,
                };
                items.push(AssistantContent::ToolCall(tc));
            }
            _ => {}
        }
    }
    Some(items)
}

/// Convert pi content array to rig ToolResultContent items.
fn pi_content_to_toolresult_content(
    content: &serde_json::Value,
) -> Option<Vec<ToolResultContent>> {
    let array = content.as_array()?;
    let mut items = Vec::new();
    for item in array {
        let type_str = item.get("type")?.as_str()?;
        match type_str {
            "text" => {
                let text = item.get("text")?.as_str()?;
                items.push(ToolResultContent::Text(Text::new(text)));
            }
            _ => {}
        }
    }
    Some(items)
}

// ---------------------------------------------------------------------------
// rig Message → pi AgentMessage conversion
// ---------------------------------------------------------------------------

/// ID for the i-th content entry within a multi-item `Message::User`.
/// i=0 uses `base_id` directly; subsequent entries are `{base_id}-{i}`.
fn content_entry_id(base_id: &str, i: usize) -> String {
    if i == 0 {
        base_id.to_string()
    } else {
        format!("{}-{}", base_id, i)
    }
}

/// `parent_id` for the i-th content entry within a multi-item `Message::User`.
///
/// Entries must form a **linear chain** so that `get_path_to_root` visits
/// every one of them:
///   i=0 → outer `parent_id`   (the preceding leaf in the session tree)
///   i=1 → `base_id`            (the i=0 entry)
///   i≥2 → `{base_id}-{i-1}`   (the immediately preceding entry)
///
/// The previous code always used `base_id` for every i>0, which created a
/// star/tree topology.  `get_path_to_root` then only followed one branch
/// (the leaf → root path), silently dropping all intermediate siblings.
fn content_entry_parent(base_id: &str, i: usize, outer_parent: &Option<String>) -> Option<String> {
    match i {
        0 => outer_parent.clone(),
        1 => Some(base_id.to_string()),
        _ => Some(format!("{}-{}", base_id, i - 1)),
    }
}

/// Convert a rig `Message` to one or more pi `AgentMessage` entries.
///
/// Most messages produce a single entry, but a `Message::User` containing
/// `ToolResult` items produces one pi entry per tool result (pi's format
/// stores each tool result as a separate entry).
pub(crate) fn rig_message_to_pi_entries(
    msg: &Message,
    base_id: &str,
    parent_id: &Option<String>,
) -> Vec<SessionEntry> {
    match msg {
        Message::User { content } => {
            let mut entries = Vec::new();

            for (i, c) in content.iter().enumerate() {
                match c {
                    UserContent::Text(t) => {
                        let entry = SessionEntry {
                            id: content_entry_id(base_id, i),
                            parent_id: content_entry_parent(base_id, i, parent_id),
                            timestamp: Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                            content: SessionEntryContent::Message {
                                message: serde_json::json!({
                                    "role": "user",
                                    "content": [{"type": "text", "text": t.text}]
                                }),
                            },
                        };
                        entries.push(entry);
                    }
                    UserContent::ToolResult(tr) => {
                        let content_items: Vec<serde_json::Value> = tr
                            .content
                            .iter()
                            .map(|c| match c {
                                ToolResultContent::Text(t) => {
                                    serde_json::json!({"type":"text","text":t.text})
                                }
                                ToolResultContent::Image(_) => {
                                    serde_json::json!({"type":"text","text":"[image]"})
                                }
                                ToolResultContent::Json { value } => {
                                    serde_json::json!({"type":"text","text":value.to_string()})
                                }
                            })
                            .collect();

                        let entry = SessionEntry {
                            id: content_entry_id(base_id, i),
                            parent_id: content_entry_parent(base_id, i, parent_id),
                            timestamp: Utc::now()
                                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                            content: SessionEntryContent::Message {
                                message: serde_json::json!({
                                    "role": "toolResult",
                                    "toolCallId": tr.id,
                                    "toolName": "tool",
                                    "content": content_items,
                                    "isError": false,
                                }),
                            },
                        };
                        entries.push(entry);
                    }
                    _ => {}
                }
            }
            entries
        }
        Message::Assistant { id, content } => {
            let mut pi_content: Vec<serde_json::Value> = Vec::new();
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
                                        obj["thinkingSignature"] =
                                            serde_json::Value::String(sig.clone());
                                    }
                                    pi_content.push(obj);
                                }
                                ReasoningContent::Encrypted(data) => {
                                    pi_content.push(serde_json::json!({
                                        "type": "thinking",
                                        "thinking": format!("[encrypted: {}]", data),
                                    }));
                                }
                                ReasoningContent::Redacted { data } => {
                                    pi_content.push(serde_json::json!({
                                        "type": "thinking",
                                        "thinking": format!("[redacted: {}]", data),
                                    }));
                                }
                                ReasoningContent::Summary(s) => {
                                    pi_content.push(serde_json::json!({
                                        "type": "thinking",
                                        "thinking": s,
                                    }));
                                }
                                _ => {
                                    // Future ReasoningContent variants — skip.
                                }
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
                    AssistantContent::Image(_) => {}
                }
            }

            let mut message = serde_json::json!({
                "role": "assistant",
                "content": pi_content,
            });
            if let Some(aid) = id {
                message["responseId"] = serde_json::Value::String(aid.clone());
            }

            let entry = SessionEntry {
                id: base_id.to_string(),
                parent_id: parent_id.clone(),
                timestamp: Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                content: SessionEntryContent::Message { message },
            };
            vec![entry]
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Metadata derivation
// ---------------------------------------------------------------------------

/// Find the most recent `ThinkingLevelChange` on the branch (closest to leaf).
fn derive_thinking_level(entries: &[SessionEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|e| {
            if let SessionEntryContent::ThinkingLevelChange { thinking_level } = &e.content {
                Some(thinking_level.clone())
            } else {
                None
            }
        })
}

/// Find the most recent `ModelChange` on the branch (closest to leaf).
fn derive_model(entries: &[SessionEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|e| {
            if let SessionEntryContent::ModelChange { model_id, .. } = &e.content {
                Some(model_id.clone())
            } else {
                None
            }
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::storage::JsonlSessionStorage;
    use crate::session::types::session_filename;
    use std::path::Path;
    use std::path::PathBuf;

    /// Write entries to a temp session file, then open it read-only and build
    /// a Session for context-building tests.
    async fn build_session_for_test(
        entries: Vec<SessionEntry>,
    ) -> (Session, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-test-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        // Write the file
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        for entry in &entries {
            storage.append_entry(entry).await.unwrap();
        }
        drop(storage);

        // Open read-only and build index
        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index = ro.build_index().await.unwrap();

        let session = Session::new_persisted(ro, index);

        // Cleanup marker
        (session, path)
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    /// Helper: create a quick user text message entry.
    fn user_msg(id: &str, text: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(String::from),
            timestamp: "t".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    /// Helper: create a quick assistant message entry.
    fn assistant_msg(id: &str, text: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(String::from),
            timestamp: "t".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    #[tokio::test]
    async fn test_build_context_empty_session() {
        let (session, path) = build_session_for_test(vec![]).await;
        let ctx = session.build_context().await.unwrap();
        assert!(ctx.messages.is_empty());
        assert!(ctx.thinking_level.is_none());
        assert!(ctx.model.is_none());
        cleanup(&path);
    }

    #[test]
    fn test_pi_message_to_rig_user_text() {
        let pi = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hello world"}]
        });
        let msg = pi_message_to_rig(&pi).unwrap();
        match msg {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                match content.first_ref() {
                    UserContent::Text(t) => assert_eq!(t.text, "hello world"),
                    _ => panic!("expected Text"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn test_pi_message_to_rig_assistant_text_and_reasoning() {
        let pi = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "let me think", "thinkingSignature": "sig123"},
                {"type": "text", "text": "the answer is 42"}
            ],
            "responseId": "msg_001"
        });
        let msg = pi_message_to_rig(&pi).unwrap();
        match msg {
            Message::Assistant { id, content } => {
                assert_eq!(id, Some("msg_001".into()));
                assert_eq!(content.len(), 2);
                match content.first_ref() {
                    AssistantContent::Reasoning(r) => {
                        match &r.content[0] {
                            ReasoningContent::Text { text, signature } => {
                                assert_eq!(text, "let me think");
                                assert_eq!(signature.as_deref(), Some("sig123"));
                            }
                            _ => panic!("expected ReasoningContent::Text"),
                        }
                    }
                    _ => panic!("expected Reasoning"),
                }
                match content.iter().nth(1).unwrap() {
                    AssistantContent::Text(t) => assert_eq!(t.text, "the answer is 42"),
                    _ => panic!("expected Text"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn test_pi_message_to_rig_assistant_tool_call() {
        let pi = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "toolCall", "id": "tc_001", "name": "bash", "arguments": {"cmd": "ls"}}
            ]
        });
        let msg = pi_message_to_rig(&pi).unwrap();
        match msg {
            Message::Assistant { content, .. } => {
                assert_eq!(content.len(), 1);
                match content.first_ref() {
                    AssistantContent::ToolCall(tc) => {
                        assert_eq!(tc.id, "tc_001");
                        assert_eq!(tc.function.name, "bash");
                        // pi_message_to_rig is a raw conversion, not a
                        // normalisation step.  The "cmd" field is preserved
                        // as-is; storage-layer build_index() normalises it.
                        assert_eq!(tc.function.arguments["cmd"], "ls");
                    }
                    _ => panic!("expected ToolCall"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn test_toolresult_batching() {
        let entries = vec![
            user_msg("e1", "list files", None),
            SessionEntry {
                id: "e2".into(),
                parent_id: Some("e1".into()),
                timestamp: "t2".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type":"toolCall","id":"tc_1","name":"bash","arguments":{"cmd":"ls"}}]
                    }),
                },
            },
            SessionEntry {
                id: "e3".into(),
                parent_id: Some("e2".into()),
                timestamp: "t3".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "toolResult",
                        "toolCallId": "tc_1",
                        "toolName": "bash",
                        "content": [{"type":"text","text":"file1.txt"}],
                        "isError": false
                    }),
                },
            },
            SessionEntry {
                id: "e4".into(),
                parent_id: Some("e3".into()),
                timestamp: "t4".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "toolResult",
                        "toolCallId": "tc_2",
                        "toolName": "read",
                        "content": [{"type":"text","text":"file content"}],
                        "isError": false
                    }),
                },
            },
            user_msg("e5", "next question", Some("e4")),
        ];

        let messages = extract_messages(&entries);
        assert_eq!(messages.len(), 4);

        // 1: user "list files"
        assert!(matches!(&messages[0], Message::User { .. }));
        if let Message::User { content } = &messages[0] {
            if let UserContent::Text(t) = content.first_ref() {
                assert_eq!(t.text, "list files");
            } else {
                panic!("expected Text");
            }
        }

        // 2: assistant with tool call
        assert!(matches!(&messages[1], Message::Assistant { .. }));

        // 3: user with 2 tool results (batched)
        assert!(matches!(&messages[2], Message::User { .. }));
        if let Message::User { content } = &messages[2] {
            assert_eq!(content.len(), 2);
            match content.first_ref() {
                UserContent::ToolResult(tr) => assert_eq!(tr.id, "tc_1"),
                _ => panic!("expected ToolResult"),
            }
            match content.iter().nth(1).unwrap() {
                UserContent::ToolResult(tr) => assert_eq!(tr.id, "tc_2"),
                _ => panic!("expected ToolResult"),
            }
        }

        // 4: user "next question"
        assert!(matches!(&messages[3], Message::User { .. }));
    }

    #[test]
    fn test_derive_thinking_level_and_model() {
        let entries = vec![
            SessionEntry {
                id: "e1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            SessionEntry {
                id: "e2".into(),
                parent_id: Some("e1".into()),
                timestamp: "t2".into(),
                content: SessionEntryContent::ThinkingLevelChange {
                    thinking_level: "high".into(),
                },
            },
            SessionEntry {
                id: "e3".into(),
                parent_id: Some("e2".into()),
                timestamp: "t3".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
        ];

        assert_eq!(derive_thinking_level(&entries), Some("high".into()));
        // Latest model change wins (closest to leaf)
        assert_eq!(derive_model(&entries), Some("gpt-4o".into()));
    }

    #[test]
    fn test_rig_message_to_pi_user_text_roundtrip() {
        let rig_msg = Message::user("hello from rig");
        let entries = rig_message_to_pi_entries(&rig_msg, "test-id", &None);
        assert_eq!(entries.len(), 1);

        let converted = extract_messages(&entries);
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            Message::User { content } => {
                if let UserContent::Text(t) = content.first_ref() {
                    assert_eq!(t.text, "hello from rig");
                } else {
                    panic!("expected Text");
                }
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn test_rig_assistant_with_reasoning_roundtrip() {
        let assistant = Message::Assistant {
            id: Some("resp-1".into()),
            content: OneOrMany::many(vec![
                AssistantContent::Reasoning(Reasoning::new_with_signature(
                    "step by step",
                    Some("sig".into()),
                )),
                AssistantContent::Text(Text::new("done")),
            ])
            .unwrap(),
        };

        let entries = rig_message_to_pi_entries(&assistant, "a1", &None);
        assert_eq!(entries.len(), 1);

        let converted = extract_messages(&entries);
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            Message::Assistant { id, content } => {
                assert_eq!(id.as_deref(), Some("resp-1"));
                assert_eq!(content.len(), 2);
                match content.first_ref() {
                    AssistantContent::Reasoning(r) => {
                        match &r.content[0] {
                            ReasoningContent::Text { text, signature } => {
                                assert_eq!(text, "step by step");
                                assert_eq!(signature.as_deref(), Some("sig"));
                            }
                            _ => panic!("expected ReasoningContent::Text"),
                        }
                    }
                    _ => panic!("expected Reasoning"),
                }
                match content.iter().nth(1).unwrap() {
                    AssistantContent::Text(t) => assert_eq!(t.text, "done"),
                    _ => panic!("expected Text"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn test_rig_tool_result_roundtrip() {
        let tool_result = Message::User {
            content: OneOrMany::many(vec![UserContent::ToolResult(ToolResult {
                id: "tc_1".into(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(Text::new("output"))),
            })])
            .unwrap(),
        };

        let entries = rig_message_to_pi_entries(&tool_result, "tr-id", &None);
        assert_eq!(entries.len(), 1);

        // Verify pi format
        if let SessionEntryContent::Message { message } = &entries[0].content {
            assert_eq!(message["role"], "toolResult");
            assert_eq!(message["toolCallId"], "tc_1");
        } else {
            panic!("expected Message variant");
        }

        // Round-trip back
        let converted = extract_messages(&entries);
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            Message::User { content } => {
                assert_eq!(content.len(), 1);
                match content.first_ref() {
                    UserContent::ToolResult(tr) => assert_eq!(tr.id, "tc_1"),
                    _ => panic!("expected ToolResult"),
                }
            }
            _ => panic!("expected User"),
        }
    }

    #[tokio::test]
    async fn test_build_context_from_branch() {
        // Build a real session tree and verify build_context produces
        // the correct message sequence.
        let entries = vec![
            user_msg("e1", "hello", None),
            assistant_msg("e2", "hi there", Some("e1")),
            user_msg("e3", "how are you?", Some("e2")),
            assistant_msg("e4", "I'm fine", Some("e3")),
            // Leaf points to e4
            SessionEntry::new_leaf(Some("e4".into()), Some("e3".into())),
        ];

        let (session, path) = build_session_for_test(entries).await;
        let ctx = session.build_context().await.unwrap();

        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(ctx.messages[0], Message::user("hello"));
        assert_eq!(ctx.messages[1], Message::assistant("hi there"));
        assert_eq!(ctx.messages[2], Message::user("how are you?"));
        assert_eq!(ctx.messages[3], Message::assistant("I'm fine"));

        cleanup(&path);
    }

    #[tokio::test]
    async fn test_append_and_reread() {
        // Create a fresh session, append a message, then re-open and verify.
        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-append-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();

        let session = Session::new_persisted(storage, index);
        let _ = session.append_message(&Message::user("append test")).await.unwrap();

        // Re-open read-only to verify
        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index2 = ro.build_index().await.unwrap();
        let session2 = Session::new_persisted(ro, index2);
        let ctx = session2.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0], Message::user("append test"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_append_model_and_thinking() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-metadata-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();

        let session = Session::new_persisted(storage, index);
        session.append_message(&Message::user("q1")).await.unwrap();
        session.append_model_change("anthropic", "claude-3-5-sonnet").await.unwrap();
        session.append_thinking_level_change("high").await.unwrap();

        // Re-open and verify metadata is derived correctly
        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index2 = ro.build_index().await.unwrap();
        let session2 = Session::new_persisted(ro, index2);
        let ctx = session2.build_context().await.unwrap();
        // Should have the user message
        assert_eq!(ctx.messages.len(), 1);
        // Thinking level should be "high"
        assert_eq!(ctx.thinking_level.as_deref(), Some("high"));
        // Model should be claude
        assert_eq!(ctx.model.as_deref(), Some("claude-3-5-sonnet"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_set_session_name() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-name-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();

        let session = Session::new_persisted(storage, index);
        session.set_session_name("my-analysis").await.unwrap();

        // Re-open and verify session name
        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index2 = ro.build_index().await.unwrap();
        assert_eq!(index2.session_name.as_deref(), Some("my-analysis"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_in_memory_index_updated_after_append() {
        // Regression test: after appending messages and metadata entries,
        // build_context() must return the correct messages using the
        // IN-MEMORY index (without re-opening from disk). The bug was that
        // append operations wrote to disk and updated leaf_id but never
        // inserted entries into index.entries, so get_branch() returned empty.
        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-inmem-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        // --- Turn 1: append a user + assistant message ---
        session.append_message(&Message::user("hello")).await.unwrap();
        session.append_message(&Message::assistant("hi there")).await.unwrap();

        // Verify context without re-opening from disk
        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2, "should have both turn-1 messages");
        assert_eq!(ctx.messages[0], Message::user("hello"));
        assert_eq!(ctx.messages[1], Message::assistant("hi there"));

        // --- Model change ---
        session.append_model_change("anthropic", "claude-3-5-sonnet").await.unwrap();

        // Verify context still includes the original messages
        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2, "model change must not drop messages");
        assert_eq!(ctx.messages[0], Message::user("hello"));
        assert_eq!(ctx.messages[1], Message::assistant("hi there"));
        assert_eq!(ctx.model.as_deref(), Some("claude-3-5-sonnet"));

        // --- Thinking level change ---
        session.append_thinking_level_change("high").await.unwrap();

        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 2, "thinking change must not drop messages");
        assert_eq!(ctx.thinking_level.as_deref(), Some("high"));

        // --- Turn 2: another user message ---
        session.append_message(&Message::user("follow up")).await.unwrap();

        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 3, "should accumulate across turns");
        assert_eq!(ctx.messages[0], Message::user("hello"));
        assert_eq!(ctx.messages[1], Message::assistant("hi there"));
        assert_eq!(ctx.messages[2], Message::user("follow up"));

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Regression test: a `Message::User` with N tool results must survive a
    /// full roundtrip (store → `build_context`) with **all** results present and
    /// in the original order.
    ///
    /// The previous bug set `parent_id = base_id` for every i>0 entry,
    /// creating a star topology.  `get_path_to_root` then only visited the
    /// last entry's branch, silently dropping all intermediate siblings (i=1,
    /// i=2, … i=N-2).  With three tool results the middle one (i=1) was lost,
    /// which caused the API to reject the next request with
    /// "insufficient tool messages following tool_calls message".
    #[tokio::test]
    async fn test_multiple_tool_results_all_preserved() {
        use rig_core::message::{ToolResult, ToolResultContent};

        let dir = std::env::temp_dir().join(format!(
            "picocode-session2-toolresults-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        // User prompt.
        session.append_message(&Message::user("summarise the project")).await.unwrap();

        // Assistant turn with three tool calls.
        let assistant_with_tools = Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: "tc_0".into(), call_id: None,
                    function: rig_core::message::ToolFunction::new("read".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: "tc_1".into(), call_id: None,
                    function: rig_core::message::ToolFunction::new("read".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: "tc_2".into(), call_id: None,
                    function: rig_core::message::ToolFunction::new("ls".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
            ]).unwrap(),
        };
        session.append_message(&assistant_with_tools).await.unwrap();

        // Three tool results in one User message.
        let tool_results = Message::User {
            content: OneOrMany::many(vec![
                UserContent::ToolResult(ToolResult {
                    id: "tc_0".into(), call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(Text::new("result 0"))),
                }),
                UserContent::ToolResult(ToolResult {
                    id: "tc_1".into(), call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(Text::new("result 1"))),
                }),
                UserContent::ToolResult(ToolResult {
                    id: "tc_2".into(), call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(Text::new("result 2"))),
                }),
            ]).unwrap(),
        };
        session.append_message(&tool_results).await.unwrap();

        // Final assistant response.
        session.append_message(&Message::assistant("here is the summary")).await.unwrap();

        // --- Verify roundtrip ---
        let ctx = session.build_context().await.unwrap();
        assert_eq!(ctx.messages.len(), 4, "user + assistant(tools) + user(results) + assistant(final)");

        // The tool-result user message must contain all three results.
        let tool_result_msg = &ctx.messages[2];
        let Message::User { content } = tool_result_msg else {
            panic!("expected User message at index 2, got {:?}", tool_result_msg);
        };
        assert_eq!(content.len(), 3, "all three tool results must be present after roundtrip");

        let ids: Vec<&str> = content.iter().map(|c| match c {
            UserContent::ToolResult(tr) => tr.id.as_str(),
            _ => panic!("expected ToolResult"),
        }).collect();
        assert_eq!(ids, ["tc_0", "tc_1", "tc_2"], "tool results must be in original order");

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Verify that a tool-calling turn round-trips through the session
    /// without creating dangling tool_calls on the next turn.
    #[tokio::test]
    async fn test_tool_call_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-session-toolrt-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        use rig_core::message::{ToolCall, ToolFunction, ToolResult as RigToolResult, ToolResultContent};

        // --- Turn 1: user asks about weather, assistant responds with tool
        // call, tool executes, assistant gives final answer ---
        session.append_message(&Message::user("weather")).await.unwrap();
        session.append_message(&Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: "call_abc".into(),
                call_id: None,
                function: ToolFunction::new("get_weather".to_string(), serde_json::json!({"city": "NYC"})),
                signature: None,
                additional_params: None,
            })),
        }).await.unwrap();
        session.append_message(&Message::User {
            content: OneOrMany::one(UserContent::ToolResult(RigToolResult {
                id: "call_abc".into(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(Text::new("72F"))),
            })),
        }).await.unwrap();
        session.append_message(&Message::assistant("It's 72F in NYC")).await.unwrap();

        // Verify context is correct after turn 1
        let ctx1 = session.build_context().await.unwrap();
        assert_eq!(ctx1.messages.len(), 4, "turn 1 should have 4 messages");

        // Check alternation: user -> assistant(toolCall) -> user(toolResult) -> assistant(text)
        assert!(matches!(&ctx1.messages[0], Message::User { .. }));
        assert!(matches!(&ctx1.messages[1], Message::Assistant { .. }));
        assert!(matches!(&ctx1.messages[2], Message::User { .. }));
        assert!(matches!(&ctx1.messages[3], Message::Assistant { .. }));

        // Verify the tool call is present in message[1]
        if let Message::Assistant { content, .. } = &ctx1.messages[1] {
            let has_tool_call = content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)));
            assert!(has_tool_call, "assistant message 1 should contain a tool call");
        }

        // Verify the tool result is in message[2]
        if let Message::User { content } = &ctx1.messages[2] {
            let has_tool_result = content.iter().any(|c| matches!(c, UserContent::ToolResult(_)));
            assert!(has_tool_result, "user message 2 should contain a tool result");
        }

        // --- Turn 2: simulate a subsequent user message ---
        session.append_message(&Message::user("forecast")).await.unwrap();

        let ctx2 = session.build_context().await.unwrap();
        // Should have 5 messages: 4 from turn 1 + 1 new user message
        assert_eq!(ctx2.messages.len(), 5, "turn 2 should have 5 accumulated messages");

        // Verify no dangling tool call: the last assistant(toolCall) must be
        // followed by a tool result or another assistant message.
        for i in 0..ctx2.messages.len().saturating_sub(1) {
            let is_assistant_with_tool_call = match &ctx2.messages[i] {
                Message::Assistant { content, .. } => {
                    content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)))
                }
                _ => false,
            };
            if is_assistant_with_tool_call {
                let next_is_tool_result = match &ctx2.messages[i + 1] {
                    Message::User { content } => {
                        content.iter().any(|c| matches!(c, UserContent::ToolResult(_)))
                    }
                    _ => false,
                };
                assert!(
                    next_is_tool_result,
                    "assistant message {} with tool_calls must be followed by tool result (message {})",
                    i, i + 1
                );
            }
        }

        let _ = tokio::fs::remove_file(&path).await;
    }
}

    #[tokio::test]
    async fn test_toolname_in_toolresult() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-toolname-test-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(std::path::Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        use rig_core::message::{ToolCall, ToolFunction, ToolResult as RigToolResult, ToolResultContent};

        // Turn: user -> assistant(read tool call) -> user(tool result) -> assistant
        session.append_message(&Message::user("read file")).await.unwrap();
        session.append_message(&Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::ToolCall(ToolCall {
                id: "tc_read".into(),
                call_id: None,
                function: ToolFunction::new("read".into(), serde_json::json!({"path": "x.rs"})),
                signature: None,
                additional_params: None,
            })),
        }).await.unwrap();
        session.append_message(&Message::User {
            content: OneOrMany::one(UserContent::ToolResult(RigToolResult {
                id: "tc_read".into(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::Text(Text::new("file contents"))),
            })),
        }).await.unwrap();
        session.append_message(&Message::assistant("done")).await.unwrap();

        // Read back the JSONL file and check toolName in tool result entries.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let mut found_tool_result = false;
        for line in raw.lines().skip(1) {
            if line.contains("\"role\":\"toolResult\"") {
                found_tool_result = true;
                assert!(
                    line.contains("\"toolName\":\"read\""),
                    "expected toolName 'read' in tool result, got: {}",
                    line
                );
            }
        }
        assert!(found_tool_result, "should have found a tool result entry");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_toolname_multiple_tools() {
        let dir = std::env::temp_dir().join(format!(
            "picocode-multitool-test-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = SessionHeader::new(std::path::Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        use rig_core::message::{ToolCall, ToolFunction, ToolResult as RigToolResult, ToolResultContent};

        // Turn: assistant calls two different tools, then user provides results.
        session.append_message(&Message::user("do things")).await.unwrap();
        session.append_message(&Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![
                AssistantContent::ToolCall(ToolCall {
                    id: "tc_bash".into(),
                    call_id: None,
                    function: ToolFunction::new("bash".into(), serde_json::json!({"command": "ls"})),
                    signature: None,
                    additional_params: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "tc_read".into(),
                    call_id: None,
                    function: ToolFunction::new("read".into(), serde_json::json!({"path": "x.rs"})),
                    signature: None,
                    additional_params: None,
                }),
            ]).unwrap(),
        }).await.unwrap();
        session.append_message(&Message::User {
            content: OneOrMany::many(vec![
                UserContent::ToolResult(RigToolResult {
                    id: "tc_bash".into(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(Text::new("file1.txt"))),
                }),
                UserContent::ToolResult(RigToolResult {
                    id: "tc_read".into(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::Text(Text::new("content"))),
                }),
            ]).unwrap(),
        }).await.unwrap();
        session.append_message(&Message::assistant("all done")).await.unwrap();

        // Read back and verify toolName values.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let mut bash_ok = false;
        let mut read_ok = false;
        for line in raw.lines().skip(1) {
            if line.contains("\"role\":\"toolResult\"") {
                if line.contains("\"toolName\":\"bash\"") {
                    bash_ok = true;
                } else if line.contains("\"toolName\":\"read\"") {
                    read_ok = true;
                } else {
                    panic!("unexpected toolName in tool result: {}", line);
                }
            }
        }
        assert!(bash_ok, "should have a bash tool result with toolName 'bash'");
        assert!(read_ok, "should have a read tool result with toolName 'read'");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[test]
    fn test_header_metadata_roundtrip() {
        // Create a header with provider/model/thinking set.
        let cwd = std::path::Path::new("/test");
        let header = SessionHeader::with_model_context(
            cwd,
            Some("anthropic".into()),
            Some("claude-sonnet-4-5".into()),
            Some("off".into()),
        );
        assert_eq!(header.provider.as_deref(), Some("anthropic"));
        assert_eq!(header.model_id.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(header.thinking_level.as_deref(), Some("off"));

        // Serialise and deserialise to verify roundtrip.
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains("\"provider\":\"anthropic\""));
        assert!(json.contains("\"modelId\":\"claude-sonnet-4-5\""));
        assert!(json.contains("\"thinkingLevel\":\"off\""));

        let deserialised: SessionHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialised.provider.as_deref(), Some("anthropic"));
        assert_eq!(deserialised.model_id.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(deserialised.thinking_level.as_deref(), Some("off"));
    }

    #[test]
    fn test_header_metadata_absent_by_default() {
        // A default header should have no model metadata.
        let cwd = std::path::Path::new("/test");
        let header = SessionHeader::new(cwd);
        assert!(header.provider.is_none());
        assert!(header.model_id.is_none());
        assert!(header.thinking_level.is_none());

        let json = serde_json::to_string(&header).unwrap();
        assert!(!json.contains("\"provider\""));
        assert!(!json.contains("\"modelId\""));
        assert!(!json.contains("\"thinkingLevel\""));
    }
