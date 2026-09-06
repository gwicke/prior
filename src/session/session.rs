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
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolCallId,
    ToolFunction, ToolResult, ToolResultContent, UserContent,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::session::storage::{JsonlSessionStorage, SessionStorage};
use crate::session::types::*;

// ---------------------------------------------------------------------------
// Compaction summary message (pi-compatible wrapper)
// ---------------------------------------------------------------------------

/// pi's compaction summary message wrapper (core/messages.ts).
/// The compaction entry is never sent to the LLM as itself; when its `summary`
/// is non-empty it is projected into a synthetic user message wrapped as below.
const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Build the synthetic user-message entry that stands in for a `Compaction`
/// entry in the LLM context (pi `compactionSummary` message, converted by
/// `convertToLlm`). Called with a non-empty summary only — an empty summary
/// emits nothing in the context ("if available" per the design doc).
fn compaction_summary_message(summary: &str) -> SessionEntry {
    SessionEntry {
        id: uuid::Uuid::now_v7().to_string(),
        parent_id: None,
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        content: SessionEntryContent::Message {
            message: serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("{}{}{}", COMPACTION_SUMMARY_PREFIX, summary, COMPACTION_SUMMARY_SUFFIX)
                }]
            }),
        },
    }
}

/// A single file touched during the compacted span, for the text-only
/// summary's file list.
#[derive(Debug, PartialEq)]
pub(crate) struct TouchedFile {
    pub path: String,
    /// `true` when the file was written/edited (vs only read).
    pub modified: bool,
}

/// Markdown / documentation file. Compaction summaries separate these from
/// other touched files so the user (or an LLM reading the summary) can
/// quickly find narrative context vs. code/config artifacts.
///
/// The check is the basename suffix `.md` / `.markdown` (case-insensitive).
/// We intentionally keep this conservative — no MIME sniffing, no
/// "looking inside the file" — because the input here is a tool-call
/// path the user already used, so the file's role is what the path
/// declares.
pub(crate) fn is_document_path(path: &str) -> bool {
    // Take the last path component (after the final `/` or `\`).
    let base = match path.rfind(['/', '\\']) {
        Some(i) => &path[i + 1..],
        None => path,
    };
    let lower = base.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Serialize one pi user/assistant AgentMessage into the text-only summary.
/// Returns `None` for messages with no renderable text (tool results,
/// reasoning-only assistant messages, empty content).
fn summary_message_text(message: &serde_json::Value) -> Option<String> {
    let role = message.get("role").and_then(|r| r.as_str())?;
    let content = message.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|item| {
            let ty = item.get("type").and_then(|t| t.as_str())?;
            match (role, ty) {
                ("user", "text") | ("assistant", "text") => {
                    item.get("text").and_then(|t| t.as_str())
                }
                // Deliberately excluded: toolCall, thinking/reasoning,
                // toolResult content, images.
                _ => None,
            }
        })
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Strip `session_dir` prefix from `path`, returning a compact relative path
/// (no leading `./`). Both sides are canonicalized (symlink-resolved) before
/// comparison to handle path portability. Returns `None` if `path` is not
/// a sub-path of `session_dir`, or if canonicalization fails.
pub(crate) fn relativize_path(session_dir: &std::path::Path, path: &str) -> Option<String> {
    let session_dir = session_dir.canonicalize().ok()?;
    let abs_path = std::path::Path::new(path).canonicalize().ok()?;
    let rel = abs_path.strip_prefix(&session_dir).ok()?.to_str()?;
    // strip_prefix succeeds for equal paths, returning "". Treat as outside.
    if rel.is_empty() {
        return None;
    }
    Some(rel.to_string())
}

/// Extract touched files from one pi assistant message's `toolCall` blocks.
/// Appends into `files`, promoting existing read-only entries to modified
/// when a later write/edit touches the same path. Only `read`, `write`, and
/// `edit` calls are considered; all other tools are ignored.
///
/// Paths below `session_dir` are stored as compact relative paths
/// (no leading `./`). Paths outside `session_dir` are stored as-is.
fn collect_touched_files(
    message: &serde_json::Value,
    files: &mut Vec<TouchedFile>,
    session_dir: &std::path::Path,
) {
    let Some(content) = message.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
            continue;
        }
        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let modified = match name {
            "write" | "edit" => true,
            "read" => false,
            _ => continue, // bash, grep, … do not touch files directly
        };
        let Some(path) = item
            .get("arguments")
            .and_then(|a| a.get("path"))
            .and_then(|p| p.as_str())
        else {
            continue;
        };
        let normalized = relativize_path(session_dir, path).unwrap_or_else(|| path.to_string());
        if let Some(existing) = files.iter_mut().find(|f| f.path == normalized) {
            if modified {
                existing.modified = true; // read-then-write → modified
            }
        } else {
            files.push(TouchedFile {
                path: normalized,
                modified,
            });
        }
    }
}

/// Like `collect_touched_files` but skips paths already in `seen_paths`.
/// Adds new entries to `files` and records their paths in `seen_paths`.
/// De-duplication handles both relative and absolute paths by normalising
/// all extracted paths via `relativize_path` before lookup.
fn collect_touched_files_filtered(
    message: &serde_json::Value,
    files: &mut Vec<TouchedFile>,
    seen_paths: &mut std::collections::HashSet<String>,
    session_dir: &std::path::Path,
) {
    let Some(content) = message.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    for item in content {
        if item.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
            continue;
        }
        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let modified = match name {
            "write" | "edit" => true,
            "read" => false,
            _ => continue,
        };
        let Some(path) = item
            .get("arguments")
            .and_then(|a| a.get("path"))
            .and_then(|p| p.as_str())
        else {
            continue;
        };
        // Normalise to relative form for de-dup key; fall back to absolute.
        let normalized = relativize_path(session_dir, path).unwrap_or_else(|| path.to_string());

        // Skip already-seen paths (preserve earlier entry's order).
        if seen_paths.contains(&normalized) {
            // If the new entry is "modified" and the existing is "read", upgrade it.
            if modified {
                if let Some(existing) = files.iter_mut().find(|f| f.path == normalized) {
                    existing.modified = true;
                }
            }
            continue;
        }
        seen_paths.insert(normalized.clone());
        files.push(TouchedFile {
            path: normalized,
            modified,
        });
    }
}
/// entries a compaction drops (everything before `firstKeptEntryId`).
///
/// Contains, in chronological order:
/// - every **user prompt** (full text),
/// - the assistant's **text responses** for that turn (one block per turn;
///   reasoning and tool-call syntax are skipped),
/// and ends with a **touched-files list** (`read` / `write` / `edit` paths,
/// modified files marked) — but no reasoning content, no tool calls, and no
/// tool output.
///
/// Deterministic (no LLM). Used as the `summary` of a `Compaction` entry
/// ("text only" mode). Empty when the dropped span has no renderable text.
pub(crate) fn build_text_only_summary(dropped: &[SessionEntry], session_dir: &std::path::Path) -> String {
    let mut out = String::new();
    let mut files: Vec<TouchedFile> = Vec::new();
    // Assistant text accumulates until the next user prompt closes the turn.
    let mut turn_assistant: Vec<String> = Vec::new();

    // Each turn is introduced by a horizontal rule + speaker header, so the
    // markdown rendering gives clear visual separation between turns:
    //
    //   ---
    //   # [user]
    //   <prompt>
    //
    //   ---
    //   # [assistant]
    //   <response text>
    let flush_turn = |out: &mut String, assistant: &mut Vec<String>| {
        if assistant.is_empty() {
            return;
        }
        out.push_str("\n---\n# [assistant]\n");
        out.push_str(&assistant.join("\n\n"));
        assistant.clear();
    };

    for entry in dropped {
        match &entry.content {
            SessionEntryContent::Message { message } => {
                match message.get("role").and_then(|r| r.as_str()) {
                    Some("user") => {
                        // A new user prompt closes the previous turn.
                        flush_turn(&mut out, &mut turn_assistant);
                        if let Some(text) = summary_message_text(message) {
                            out.push_str("\n---\n# [user]\n");
                            out.push_str(&text);
                        }
                    }
                    Some("assistant") => {
                        collect_touched_files(message, &mut files, session_dir);
                        if let Some(text) = summary_message_text(message) {
                            turn_assistant.push(text);
                        }
                    }
                    // toolResult entries: excluded (no tool output in the
                    // summary).
                    _ => {}
                }
            }
            // Meta entries (model/thinking changes, earlier compactions,
            // labels, …): excluded from the text-only summary.
            _ => {}
        }
    }
    flush_turn(&mut out, &mut turn_assistant);

    if !files.is_empty() {
        // Split into Documents (markdown) and Files (everything else) so the
        // reader can quickly find narrative context vs. code/config. Within
        // each group, modified first then reads; both groups alphabetical
        // for stability across runs. A group that is empty is omitted.
        render_files_section(&mut out, &files, "Documents");
        render_files_section(&mut out, &files, "Files");
    }
    out.trim_start_matches('\n').to_string()
}

/// Build a deterministic, user-editable template for `manual`-mode
/// compaction. Unlike `text only`, this contains NO conversation
/// content (user prompts or assistant text) — it is purely a
/// hand-edited set of pointers the user can populate or wipe before
/// confirming the compaction.
///
/// The template is seeded with:
/// - The **first user prompt** of the session (if no previous compaction
///   summary exists), which usually establishes significant context; or
/// - The **previous compaction summary's text** (if one exists), which the
///   user has been iteratively refining over incremental compactions.
///
/// The seed text is placed above the file sections so it survives the
/// parse → render round-trip. File sections from the previous summary are
/// also carried forward and merged with freshly-touched files.
///
/// Shape (when the dropped span touches at least one file):
///
///   # Compaction notes
///
///   <seed text or previous summary notes paragraph>
///
///   ---
///   # [Documents]
///   - modified: path.md   # *.md files touched
///   - read: path.md
///
///   # [Files]
///   - modified: path.rs
///   - read: path.rs
///
/// Within each group: modified first, then reads; each group alphabetical.
/// Empty groups (no `.md` files or no other files) are omitted. A span
/// that touches no files returns the empty string — i.e. manual-mode
/// compaction remains a no-op when there is nothing to point at.
pub(crate) fn build_manual_template(dropped: &[SessionEntry], previous_summary: Option<&str>, session_dir: &std::path::Path) -> String {
    // Build the file list: previous-summary files first, then newly-touched
    // files from the dropped span, de-duplicated. De-dup is by path; the earlier
    // entry wins (previous-summary files are "authoritative").
    let mut files: Vec<TouchedFile> = Vec::new();
    let mut seen_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seed_text: Vec<String> = Vec::new();

    // Carry-forward files from the previous summary first.
    if let Some(summary) = previous_summary {
        let (parsed_files, notes) = parse_summary_into_sections(summary, &["files", "documents"]);
        for pf in parsed_files {
            seen_paths.insert(pf.path.clone());
            files.push(pf);
        }
        seed_text = notes;
    }

    // Collect newly-touched files from the dropped span (skip duplicates).
    for entry in dropped {
        if let SessionEntryContent::Message { message } = &entry.content {
            if message.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                collect_touched_files_filtered(message, &mut files, &mut seen_paths, session_dir);
            }
        }
    }

    // No previous summary: seed with the first user prompt.
    if seed_text.is_empty() {
        if let Some(prompt) = first_user_prompt(dropped) {
            seed_text.push(prompt);
        }
    }

    // Nothing to show: no touched files, no seed text. Return empty so
    // the caller skips the review gate (preserves original no-op behavior).
    if files.is_empty() && seed_text.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    out.push_str("# Compaction notes\n\n");
    for (i, para) in seed_text.iter().enumerate() {
        if i > 0 {
            out.push_str("\n");
        }
        out.push_str(para);
    }
    render_files_section(&mut out, &files, "Documents");
    render_files_section(&mut out, &files, "Files");
    out.trim_end_matches('\n').to_string()
}

/// Append a single labeled files group (`# [label]`, then bullet lines) to
/// `out`. The split key (`is_document_path`) routes `.md` files to one
/// group and everything else to another; this function picks one side per
/// call. Pass the same `files` list to two calls with two different labels
/// to get the split Documents / Files rendering.
///
/// A group with no matching files emits nothing — the caller can then
/// decide whether to omit the section header entirely. This keeps the
/// output compact: if no `.md` was touched, the Documents section simply
/// doesn't appear, and the summary stays single-section.
/// Parse a compaction summary into its constituent sections for re-use and
/// merging across incremental compactions. Designed to be fault-tolerant:
/// non-conforming input is preserved as-is so the user can manually fix it.
///
/// Recognised shapes (case-insensitive, any heading level):
///
///   # [Documents]    # Documents    ## documents
///   - path.md
///   - modified: path.md
///   - read: path.md
///   - Some descriptor: path.md
///   - /absolute/path.md
///   * read: /path.rs
///   * Some sentence: path.md
///
/// Rules:
/// - `-` and `*` list prefixes accepted; non-list lines are preserved.
/// - `Descriptor: path` (colon-space split on the right-most `:`) is parsed
///   when the part after the last `:` is a valid file path (has a `.` extension).
/// - `read` / `modified` descriptors are tracked for upgrade logic; any other
///   descriptor is preserved verbatim.
/// - Horizontal rules (`---`, `***`, `---`) are skipped.
/// - Entries not ending in a valid file path are left in `unparsed`.
///
/// Returns `(files, documents, unparsed)` where:
/// - `files`: non-`.md` paths extracted from the original sections
/// - `documents`: `.md`/`.markdown` paths extracted from the original sections
/// - `unparsed`: lines that couldn't be parsed, emitted verbatim below the
///   file sections so the user can fix them manually
#[derive(Debug, Default)]
struct Extractor<'a> {
    files: Vec<(&'a str, bool)>,
    documents: Vec<(&'a str, bool)>,
    unparsed: Vec<&'a str>,
}

impl<'a> Extractor<'a> {
    fn new() -> Self {
        Self::default()
    }

    /// Feed one non-empty, non-horizontal-rule line into the extractor.
    fn ingest(&mut self, line: &'a str) {
        let line = line.trim();
        if line.is_empty() || line == "--- " || line == "---" || line == "***" {
            return;
        }

        // Only list lines are parsed; everything else is preserved as-is.
        let Some(stripped) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) else {
            self.unparsed.push(line);
            return;
        };

        // Split on the right-most `: ` separator. If the part after it looks
        // like a file path (contains a `.` before any `/` from the end), treat
        // the part before as the optional descriptor.
        let (descriptor, path_candidate) = match stripped.rsplit_once(": ") {
            Some((desc, rest)) => {
                // Accept when the "rest" portion contains a file extension.
                // Path may be bare or absolute.
                let path = rest.trim();
                if path.contains('.') && !path.starts_with('.') {
                    (Some(desc.trim()), path)
                } else {
                    (None, stripped.trim())
                }
            }
            None => (None, stripped.trim()),
        };

        // Normalize: strip trailing `#` comments and whitespace.
        let path = path_candidate.trim_end().trim_end_matches('`');

        // Accept if it looks like a file (contains a dot, isn't a URL, isn't
        // just whitespace). Skip URLs, email addresses, dotted words without
        // separators.
        if path.is_empty()
            || path.starts_with("http")
            || path.starts_with('<')  // email / markup
            || !path.contains('.')     // no extension → not a file
            || path.ends_with('.')     // trailing dot
            || path.contains("://")    // URL
        {
            self.unparsed.push(line);
            return;
        }

        // Classify by extension.
        let is_doc = is_document_path(path);

        // Descriptor logic: "read" → modified=false, "modified" → modified=true,
        // anything else (including absent) → preserve as-is (emit with no
        // read/modified hint).
        let modified = descriptor == Some("modified");
        let preserve_unmodified = descriptor.is_none()
            || (descriptor != Some("read") && descriptor != Some("modified"));

        let target = if is_doc { &mut self.documents } else { &mut self.files };

        if preserve_unmodified {
            // Descriptor is absent or non-standard → add verbatim (no hint).
            target.push((path, modified));
        } else {
            // Descriptor is "read" or "modified" → add with status hint.
            target.push((path, modified));
        }
    }

    fn take_unparsed(&mut self) -> Vec<&'a str> {
        std::mem::take(&mut self.unparsed)
    }
}

/// Split a multi-line string into heading-delimited sections. Returns a tuple of
/// `(preamble, sections)` where:
/// - `preamble`: text appearing before the first heading (may be empty)
/// - `sections`: map from normalized label → body text (everything up to the next
///   heading or horizontal rule, or end-of-input)
///
/// Label normalization: lowercased, stripped of `[`/`]`. Any heading level
/// (1–6 `#`) is accepted. Horizontal rules (`---`, `***`) end a section body.
fn split_sections(input: &str) -> (String, HashMap<String, String>) {
    let mut sections = HashMap::new();
    let mut preamble = String::new();
    let mut current_label: Option<String> = None;
    let mut current_body = String::new();

    for line in input.lines() {
        // Heading detection: any line starting with 1–6 `#` chars followed by
        // whitespace or `[`.
        if let Some(rest) = line.strip_prefix('#') {
            let rest_stripped = rest.trim_start_matches('#').trim_start();
            if !rest_stripped.is_empty()
                && (rest_stripped.starts_with('[')
                    || rest_stripped
                        .chars()
                        .next()
                        .map_or(false, |c| c.is_alphabetic()))
            {
                // Flush previous section.
                if let Some(label) = current_label.take() {
                    sections.insert(label, current_body.trim().to_string());
                    current_body.clear();
                } else if current_body.trim().is_empty() {
                    // No active section — this is the first heading we hit.
                    // Save what we've accumulated as the preamble.
                    preamble = current_body.trim().to_string();
                    current_body.clear();
                } else {
                    // Preamble text was accumulated before any heading.
                    preamble = current_body.trim().to_string();
                    current_body.clear();
                }
                // Normalise label.
                let label_raw =
                    rest_stripped.trim_start_matches('[').trim_end_matches(']');
                current_label = Some(label_raw.to_lowercase());
                continue;
            }
        }
        if current_label.is_some() {
            let trimmed = line.trim();
            // Horizontal rules end the section body.
            if trimmed == "---" || trimmed == "***" {
                continue;
            }
            current_body.push_str(line);
            current_body.push('\n');
        } else {
            // Before any heading: accumulate into preamble.
            let trimmed = line.trim();
            // Horizontal rules are section separators in the preamble too — skip them.
            if trimmed == "---" || trimmed == "***" {
                continue;
            }
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    // Flush last section.
    if let Some(label) = current_label {
        sections.insert(label, current_body.trim().to_string());
    } else if current_body.trim().is_empty() && preamble.is_empty() {
        // No heading ever seen, and the body has preamble text.
        preamble = current_body.trim().to_string();
    } else if !current_body.trim().is_empty() {
        // No heading seen at all — all content is preamble.
        preamble = current_body.trim().to_string();
    }
    (preamble, sections)
}

/// Parse a previously-stored compaction summary and extract the file sections
/// into a `(TouchedFileVec, preserved_non_file_lines)` pair. The returned
/// `Vec<TouchedFile>` can be merged with freshly-collected files from the
/// current span.
///
/// Non-file list lines (e.g. free-text notes) are returned verbatim so they
/// can be re-appended below the unified file section. Horizontal rules are
/// dropped. This function never errors — malformed input is preserved as-is.
///
/// For the `manual` mode, only the first section-level paragraph (before any
/// heading) is kept as the "previous notes" context. For `text-only`, the
/// whole body (minus headings) is treated as the notes to keep.
fn parse_summary_into_sections(
    summary: &str,
    file_section_labels: &[&str],
) -> (Vec<TouchedFile>, Vec<String>) {
    let (preamble, sections) = split_sections(summary);
    let mut all_files: Vec<TouchedFile> = Vec::new();
    let mut non_file_lines: Vec<String> = Vec::new();

    // The preamble (text before the first heading) is always preserved as notes.
    if !preamble.is_empty() {
        non_file_lines.push(preamble);
    }

    for (label, body) in &sections {
        let is_file_section = file_section_labels.iter().any(|l| *l == label);
        if is_file_section {
            let mut extractor = Extractor::new();
            for line in body.lines() {
                extractor.ingest(line);
            }
            for (path, modified) in extractor.files.drain(..).chain(extractor.documents.drain(..)) {
                all_files.push(TouchedFile {
                    path: path.to_string(),
                    modified,
                });
            }
            non_file_lines.extend(extractor.take_unparsed().iter().map(|s| s.to_string()));
        } else if !body.is_empty() {
            // Non-file-section body (e.g. extra notes below the file sections)
            // is preserved verbatim so the user can fix malformed input.
            non_file_lines.push(body.clone());
        }
    }

    (all_files, non_file_lines)
}

/// Extract the first user prompt text from a sequence of session entries.
/// Returns `None` if the span has no user message (e.g. tool-result-only
/// entries). This is used to seed the manual template when there is no
/// previous compaction summary — the first prompt usually establishes a
/// significant amount of context.
fn first_user_prompt(entries: &[SessionEntry]) -> Option<String> {
    for entry in entries.iter() {
        if let SessionEntryContent::Message { message } = &entry.content {
            if message.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(text) = summary_message_text(message) {
                    return Some(text.trim().to_string());
                }
            }
        }
    }
    None
}

fn render_files_section(out: &mut String, files: &[TouchedFile], label: &str) {
    // Group + sort: modified first, then read; alphabetical within each.
    let mut modified: Vec<&TouchedFile> = files
        .iter()
        .filter(|f| f.modified && is_document_path(&f.path) == (label == "Documents"))
        .collect();
    let mut read: Vec<&TouchedFile> = files
        .iter()
        .filter(|f| !f.modified && is_document_path(&f.path) == (label == "Documents"))
        .collect();
    if modified.is_empty() && read.is_empty() {
        return;
    }
    modified.sort_by(|a, b| a.path.cmp(&b.path));
    read.sort_by(|a, b| a.path.cmp(&b.path));
    out.push_str(&format!("\n---\n# [{}]\n", label));
    let mut lines: Vec<String> = Vec::new();
    for f in modified {
        lines.push(format!("- modified: {}", f.path));
    }
    for f in read {
        lines.push(format!("- read: {}", f.path));
    }
    out.push_str(&lines.join("\n"));
}

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
        // derive_thinking_level / derive_model run on the FULL branch (pi's
        // `getSessionContextSettings` walks the whole path), NOT on the
        // compaction-scoped set, so a model/thinking change recorded before
        // the compaction boundary still applies after compaction.
        let thinking_level = derive_thinking_level(&branch);
        let model = derive_model(&branch);
        let transformed = self.apply_transforms(branch);
        let messages = extract_messages(&transformed);

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
    /// Mirrors pi's `buildContextEntries` (`session-manager.ts`): the latest
    /// compaction entry on the branch scopes the context. The result is
    /// `[summary from the latest compaction (if any)] + [entries from
    /// firstKeptEntryId (inclusive) up to the compaction entry] + [entries
    /// after the compaction entry]`. The compaction entry itself is NEVER
    /// included (an empty summary emits no summary message at all); older
    /// compaction entries that fall inside the kept range are projected to
    /// their own summary messages (pi's repeated-compaction behavior).
    ///
    /// An empty summary scopes the context to simply
    /// `<first message>…<last message>`.
    fn apply_transforms(&self, entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
        apply_compaction_transforms(entries)
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
                    map.insert(tc.id.clone().into_string(), tc.function.name.clone());
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
            // Mirror the new name into the in-memory index so a caller that
            // reads `s.index().session_name` immediately after
            // `set_session_name` sees the updated value. The JSONL entry is
            // the source of truth on disk (re-read by `build_index`), but
            // until that rebuild runs, the in-memory view would otherwise
            // lag — which is a footgun for any UI path that reads the name
            // straight off the live index (e.g. the `/new <name>` and
            // `/clear <name>` handlers in `agent.rs`).
            index.session_name = Some(name.to_string());
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

    /// Count how many genuine *user prompt* messages are currently persisted
    /// on the active leaf→root branch.
    ///
    /// This is the persistence source-of-truth for the retry de-dup guard
    /// (`TurnPersistence`): it counts pi `role: "user"` entries, which are
    /// only ever written for a plain, non-empty user prompt (see
    /// [`rig_message_to_pi_entries`] — tool results are stored as
    /// `role: "toolResult"`, never `"user"`). So it measures how many
    /// user turns have really been committed to the session, independently of
    /// any REPL-side counter. Compaction/`model_change`/`thinking_level`
    /// entries are ignored.
    pub async fn count_persisted_user_prompts(&self) -> usize {
        let index = self.index.lock().await;
        let leaf_id = match &index.leaf_id {
            Some(id) => id.clone(),
            None => return 0,
        };
        let path = index.get_path_to_root(&leaf_id);
        path.iter().fold(0usize, |count, entry| {
            if let crate::session::types::SessionEntryContent::Message { message } =
                &entry.content
            {
                if message.get("role").and_then(|r| r.as_str()) == Some("user") {
                    return count + 1;
                }
            }
            count
        })
    }

    /// Pivot the session to `predecessor_id` and return the scoped context.
    ///
    /// Moves the leaf to `predecessor_id`, calls `build_context()` (which
    /// applies `apply_compaction_transforms`), and returns the resulting
    /// `SessionContext` along with the original leaf ID so the caller can
    /// restore it. This is the same tree-pivot used by LLM compaction:
    /// `build_context()` re-scopes to the prefix `root..predecessor_id`,
    /// activating any compaction entry at or before that point.
    ///
    /// Returns `None` if the leaf cannot be saved or moved.
    pub async fn get_pivot_context(
        &self, predecessor_id: &str,
    ) -> Option<(SessionContext, String)> {
        let old_leaf = self.get_leaf_id().await?;
        self.move_leaf(Some(predecessor_id.to_string())).await.ok()?;
        let ctx = self.build_context().await.ok()?;
        self.move_leaf(Some(old_leaf.clone())).await.ok()?;
        Some((ctx, old_leaf))
    }

    /// Extract the text of the active compaction's summary from a pivot
    /// context, if present.
    ///
    /// After a tree pivot to `predecessor_id`, `build_context()` prepends
    /// the active compaction's summary as a synthetic user message (the
    /// `<summary>` wrapper from `compaction_summary_message`). This function
    /// extracts that leading message's text and strips the XML wrapper so
    /// callers get just the summary content. Returns `None` when there is
    /// no active compaction or its summary is empty.
    pub(crate) fn extract_seed_from_context(ctx: &SessionContext) -> Option<String> {
        let first = ctx.messages.first()?;
        let Message::User { content } = first else {
            return None;
        };
        let Some(UserContent::Text(t)) = content.first() else {
            return None;
        };
        let raw = t.text.trim();
        // Strip the COMPACTION_SUMMARY_PREFIX / SUFFIX wrapper.
        let inner = raw
            .strip_prefix(COMPACTION_SUMMARY_PREFIX)?
            .strip_suffix(COMPACTION_SUMMARY_SUFFIX)
            .unwrap_or(raw)
            .trim()
            .to_string();
        if inner.is_empty() {
            None
        } else {
            Some(inner)
        }
    }

    /// Find the first user prompt on the active branch.
    ///
    /// Walks root→leaf and returns the text of the first `role: "user"`
    /// message found — skipping any preceding compaction summary messages that
    /// were projected from earlier compactions. This correctly handles
    /// resumed sessions: the first real prompt is always an actual user
    /// message, never a synthetic summary message.
    pub async fn get_first_user_prompt(&self) -> Option<String> {
        let branch = self.get_branch().await;
        for entry in &branch {
            if let SessionEntryContent::Message { message } = &entry.content {
                if message.get("role").and_then(|r| r.as_str()) == Some("user") {
                    // `summary_message_text` filters to text-only content and
                    // skips tool/results — safe to use here.
                    if let Some(text) = summary_message_text(message) {
                        let text = text.trim();
                        return Some(text.trim().to_string());
                    }
                }
            }
        }
        None
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

/// Compaction scoping core (pi `buildContextEntries` parity). Exposed as a
/// free function so tests can exercise it without a `Session` instance.
///
/// Latest compaction on the branch (closest to leaf) scopes the context to:
/// `[summary (if non-empty)] + [firstKeptEntryId .. compaction) +
/// [post-compaction]`. The compaction entry itself is never included;
/// **all** other `Compaction` entries in the kept range are stepped over
/// silently (no summary message emitted for them either). This deliberately
/// diverges from pi (which emits each older compaction's summary as its own
/// `compactionSummary` user message) to avoid confusing redundant
/// compaction content mid-turn. See `doc/nested_compactions.md` for the
/// rationale.
pub(crate) fn apply_compaction_transforms(entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
    // Find the most recent compaction entry (closest to leaf) — pi keeps
    // the latest compaction on the path active.
    let compaction = entries.iter().rev().find_map(|entry| match &entry.content {
        SessionEntryContent::Compaction {
            summary,
            first_kept_entry_id,
            ..
        } => Some((
            entry.id.clone(),
            summary.clone(),
            first_kept_entry_id.clone(),
        )),
        _ => None,
    });

    let Some((_compaction_id, summary, first_kept_id)) = compaction else {
        return entries; // no compaction → full branch
    };

    // Find the index of the first kept entry.
    let keep_idx = match entries.iter().position(|e| e.id == first_kept_id) {
        Some(i) => i,
        None => return entries, // defensive fallback: full branch
    };

    let mut result: Vec<SessionEntry> = Vec::new();

    // The latest compaction's summary goes FIRST (pi: contextEntries =
    // [compaction] + …). Empty summary → emit nothing ("if available").
    if !summary.is_empty() {
        result.push(compaction_summary_message(&summary));
    }

    // Kept range: firstKeptEntryId .. end, with **every** compaction
    // entry dropped silently — the latest (already represented by the
    // summary message at the front), and any nested ones in the kept
    // range. Nested compactions represent superseded boundaries; their
    // summary text is intentionally invisible to the runtime LLM context
    // to avoid mid-turn redundancy. Information from them is preserved
    // via the new compaction's own `previousSummary`-injected
    // summarization step (see `run_compaction_at_point`).
    for entry in &entries[keep_idx..] {
        match &entry.content {
            SessionEntryContent::Compaction { .. } => {
                // Skip the latest compaction (already represented above)
                // and any older nested compactions (step over silently).
            }
            _ => result.push(entry.clone()),
        }
    }

    result
}

/// Convert a single pi AgentMessage (user or assistant) to a rig Message.
pub(crate) fn pi_message_to_rig(value: &serde_json::Value) -> Option<Message> {
    let role = value.get("role")?.as_str()?;
    match role {
        "user" => {
            let content = pi_content_to_user_content(value.get("content")?)?;
            Some(Message::User {
                content,
            })
        }
        "assistant" => {
            let content = pi_content_to_assistant_content(value.get("content")?)?;
            Some(Message::Assistant {
                id: value
                    .get("responseId")
                    .and_then(|v| v.as_str().map(String::from)),
                content,
            })
        }
        _ => None,
    }
}

/// Convert a batch of pi toolResult entries into a single rig Message::User.
pub(crate) fn pi_toolresults_to_rig_message(results: Vec<serde_json::Value>) -> Option<Message> {
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
        content: tool_results,
    })
}

/// Convert a pi toolResult entry to a rig ToolResult.
fn pi_toolresult_to_rig(value: &serde_json::Value) -> Option<ToolResult> {
    let id = value.get("toolCallId")?.as_str()?.to_string();
    let content = pi_content_to_toolresult_content(value.get("content")?)?;

    Some(ToolResult {
        call: ToolCallId::new(id)?,
        provider: None,
        name: value
            .get("toolName")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "tool".to_string()),
        content,
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
                    id: ToolCallId::new(id)?,
                    provider: None,
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
                                    "toolCallId": tr.call.as_str(),
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

/// Find the most recent `ModelChange` on the branch (closest to leaf),
/// returning both the provider and the model id. Used by the resume
/// prefill, which formats `provider:model_id` so the `/model` overlay's
/// fuzzy filter matches a single catalog entry directly.
pub(crate) fn derive_model_change(
    entries: &[SessionEntry],
) -> Option<(String, String)> {
    entries.iter().rev().find_map(|e| {
        if let SessionEntryContent::ModelChange { provider, model_id } = &e.content {
            Some((provider.clone(), model_id.clone()))
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
                match content.first().unwrap() {
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
                match content.first().unwrap() {
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
                match content.first().unwrap() {
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
            if let UserContent::Text(t) = content.first().unwrap() {
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
            match content.first().unwrap() {
                UserContent::ToolResult(tr) => assert_eq!(tr.call.as_str(), "tc_1"),
                _ => panic!("expected ToolResult"),
            }
            match content.iter().nth(1).unwrap() {
                UserContent::ToolResult(tr) => assert_eq!(tr.call.as_str(), "tc_2"),
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
    fn test_derive_model_change_returns_provider_and_model_id() {
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
                content: SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
        ];
        // Latest ModelChange wins (closest to leaf in rev() iteration).
        assert_eq!(
            derive_model_change(&entries),
            Some(("openai".into(), "gpt-4o".into()))
        );

        // No ModelChange → None.
        let empty: Vec<SessionEntry> = vec![];
        assert_eq!(derive_model_change(&empty), None);

        // Other entry types in between don't break the leaf-first walk.
        let mixed = vec![
            SessionEntry {
                id: "e1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-sonnet-4-5".into(),
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
        ];
        assert_eq!(
            derive_model_change(&mixed),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
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
                if let UserContent::Text(t) = content.first().unwrap() {
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
            content: vec![
                AssistantContent::Reasoning(Reasoning::new_with_signature(
                    "step by step",
                    Some("sig".into()),
                )),
                AssistantContent::Text(Text::new("done")),
            ],
        };

        let entries = rig_message_to_pi_entries(&assistant, "a1", &None);
        assert_eq!(entries.len(), 1);

        let converted = extract_messages(&entries);
        assert_eq!(converted.len(), 1);
        match &converted[0] {
            Message::Assistant { id, content } => {
                assert_eq!(id.as_deref(), Some("resp-1"));
                assert_eq!(content.len(), 2);
                match content.first().unwrap() {
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
            content: vec![UserContent::ToolResult(ToolResult {
                call: ToolCallId::new("tc_1").unwrap(),
                provider: None,
                name: "tool".into(),
                content: vec![ToolResultContent::Text(Text::new("output"))],
            })],
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
                match content.first().unwrap() {
                    UserContent::ToolResult(tr) => assert_eq!(tr.call.as_str(), "tc_1"),
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
            content: vec![
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: ToolCallId::new("tc_0").unwrap(), provider: None,
                    function: rig_core::message::ToolFunction::new("read".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: ToolCallId::new("tc_1").unwrap(), provider: None,
                    function: rig_core::message::ToolFunction::new("read".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
                AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: ToolCallId::new("tc_2").unwrap(), provider: None,
                    function: rig_core::message::ToolFunction::new("ls".into(), serde_json::json!({})),
                    signature: None, additional_params: None,
                }),
            ],
        };
        session.append_message(&assistant_with_tools).await.unwrap();

        // Three tool results in one User message.
        let tool_results = Message::User {
            content: vec![
                UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new("tc_0").unwrap(), provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("result 0"))],
                }),
                UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new("tc_1").unwrap(), provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("result 1"))],
                }),
                UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new("tc_2").unwrap(), provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("result 2"))],
                }),
            ],
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
            UserContent::ToolResult(tr) => tr.call.as_str(),
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
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId::new("call_abc").unwrap(),
                provider: None,
                function: ToolFunction::new("get_weather".to_string(), serde_json::json!({"city": "NYC"})),
                signature: None,
                additional_params: None,
            })],
        }).await.unwrap();
        session.append_message(&Message::User {
            content: vec![UserContent::ToolResult(RigToolResult {
                call: ToolCallId::new("call_abc").unwrap(),
                provider: None,
                name: "tool".into(),
                content: vec![ToolResultContent::Text(Text::new("72F"))],
            })],
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

    // ── Phase 1: compaction scoping (pi `buildContextEntries` parity) ──────

    /// Helper: create a Compaction entry.
    fn compaction_entry(id: &str, summary: &str, first_kept: &str, tokens: u64, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(String::from),
            timestamp: "tc".into(),
            content: SessionEntryContent::Compaction {
                summary: summary.into(),
                first_kept_entry_id: first_kept.into(),
                tokens_before: tokens,
            },
        }
    }

    /// Helper: extract the text of a scoped context's first message.
    fn first_user_text(messages: &[Message]) -> &str {
        match &messages[0] {
            Message::User { content } => match &content[0] {
                UserContent::Text(t) => t.text.as_str(),
                _ => "",
            },
            _ => "",
        }
    }

    #[test]
    fn test_apply_compaction_transforms_empty_summary_truncates_prefix() {
        // [m0 m1 m2 C{firstKept:m1, summary:""} m3] → [m1 m2 m3]
        let entries = vec![
            user_msg("m0", "intro", None),
            user_msg("m1", "kept-a", Some("m0")),
            user_msg("m2", "kept-b", Some("m1")),
            compaction_entry("c1", "", "m1", 128, Some("m2")),
            user_msg("m3", "after", Some("c1")),
        ];
        let out = apply_compaction_transforms(entries);
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        // Empty summary → no summary message; m0 dropped; compaction entry never included.
        assert_eq!(ids, ["m1", "m2", "m3"]);
    }

    #[test]
    fn test_apply_compaction_transforms_nonempty_summary_wraps_pi_message() {
        // [m0 m1 m2 C{firstKept:m1, summary:"S"} m3] → [summary("S") m1 m2 m3]
        let entries = vec![
            user_msg("m0", "intro", None),
            user_msg("m1", "kept-a", Some("m0")),
            user_msg("m2", "kept-b", Some("m1")),
            compaction_entry("c1", "S", "m1", 128, Some("m2")),
            user_msg("m3", "after", Some("c1")),
        ];
        let out = apply_compaction_transforms(entries);
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        // First is the pi-wrapped summary user message (fresh uuid, not m0/m1/m2/m3/c1).
        assert_eq!(ids[1..], ["m1", "m2", "m3"]);
        assert_eq!(
            first_user_text(&extract_messages(&out)),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nS\n</summary>"
        );
    }

    #[test]
    fn test_apply_compaction_transforms_no_compaction_is_identity() {
        let entries = vec![
            user_msg("m0", "intro", None),
            user_msg("m1", "kept-a", Some("m0")),
            user_msg("m2", "kept-b", Some("m1")),
        ];
        let out = apply_compaction_transforms(entries.clone());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, "m0");
        assert_eq!(out[1].id, "m1");
        assert_eq!(out[2].id, "m2");
        assert_eq!(
            extract_messages(&out),
            extract_messages(&entries),
            "no compaction → context identical to the full branch"
        );
    }

    #[test]
    fn test_apply_compaction_transforms_nested_compaction_steps_over_older_summary() {
        // Two compactions: the latest (c2) scopes from m3; the older (c1)
        // with a non-empty summary lives INSIDE the kept range (between m3
        // and c2). Under the new design (doc/nested_compactions.md), c1 is
        // stepped over silently — its summary text is NOT re-emitted as a
        // mid-turn user message. c2's summary is empty so no front
        // summary message either. Result: just [m3, m4, m5].
        //
        // Branch:
        //   [m0 m1 m2 m3 C1{firstKept:m0, summary:"early"} m4 C2{firstKept:m3, summary:""} m5]
        // Result:
        //   [m3, m4, m5]
        let entries = vec![
            user_msg("m0", "a", None),
            user_msg("m1", "b", Some("m0")),
            user_msg("m2", "c", Some("m1")),
            user_msg("m3", "d", Some("m2")),
            compaction_entry("c1", "early", "m0", 64, Some("m3")),
            user_msg("m4", "e", Some("c1")),
            compaction_entry("c2", "", "m3", 96, Some("m4")),
            user_msg("m5", "f", Some("c2")),
        ];
        let out = apply_compaction_transforms(entries);
        let ids: Vec<&str> = out.iter().map(|e| e.id.as_str()).collect();
        // Only the kept-range messages; both compaction entries (c1, c2)
        // are absent.
        assert_eq!(ids, ["m3", "m4", "m5"]);
        // Defensive: ensure c1's "early" text did NOT leak into the result
        // as a summary message.
        let msgs = extract_messages(&out);
        assert_eq!(msgs.len(), 3);
        for m in &msgs {
            if let Message::User { content } = m {
                for c in content {
                    if let UserContent::Text(t) = c {
                        assert!(
                            !t.text.contains("early"),
                            "older compaction summary text leaked into runtime context: {}",
                            t.text
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_apply_compaction_transforms_triple_nested_only_latest_summary_emitted() {
        // Three nested compactions. The latest (c3) scopes from m5; c1 and
        // c2 live inside the kept range with non-empty summaries. Under the
        // new design, only c3's summary is emitted at the front; c1 and c2
        // are stepped over silently.
        //
        // Branch:
        //   [m0 m1 m2 m3 C1{firstKept:m0, summary:"s1"} m4 C2{firstKept:m2, summary:"s2"} m5 C3{firstKept:m5, summary:"s3"} m6]
        // Result:
        //   [summary("s3"), m5, m6]   — c1 and c2 stepped over
        let entries = vec![
            user_msg("m0", "a", None),
            user_msg("m1", "b", Some("m0")),
            user_msg("m2", "c", Some("m1")),
            user_msg("m3", "d", Some("m2")),
            compaction_entry("c1", "s1", "m0", 64, Some("m3")),
            user_msg("m4", "e", Some("c1")),
            compaction_entry("c2", "s2", "m2", 64, Some("m4")),
            user_msg("m5", "f", Some("c2")),
            compaction_entry("c3", "s3", "m5", 96, Some("m5")),
            user_msg("m6", "g", Some("c3")),
        ];
        let out = apply_compaction_transforms(entries);
        let msgs = extract_messages(&out);
        assert_eq!(msgs.len(), 3, "summary(s3) + m5 + m6");
        // First message is the c3 summary wrapper.
        assert!(first_user_text(&msgs).contains("s3"));
        // Defensive: c1 and c2 summary text must not leak.
        assert!(!first_user_text(&msgs).contains("s1"));
        assert!(!first_user_text(&msgs).contains("s2"));
    }

    #[test]
    fn test_apply_compaction_transforms_missing_first_kept_falls_back() {
        // firstKeptEntryId not on the branch → defensive fallback (full branch).
        let entries = vec![
            user_msg("m0", "intro", None),
            user_msg("m1", "kept", Some("m0")),
            compaction_entry("c1", "", "missing-id", 128, Some("m1")),
            user_msg("m2", "after", Some("c1")),
        ];
        let out = apply_compaction_transforms(entries.clone());
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].id, "m0");
        assert_eq!(out[3].id, "m2");
        assert_eq!(
            extract_messages(&out),
            extract_messages(&entries),
            "missing firstKeptEntryId → full branch"
        );
    }

    #[tokio::test]
    async fn test_build_context_scopes_to_compaction_on_resume() {
        // End-to-end: append a compaction entry (empty summary, firstKept=m1),
        // then a follow-up message, and verify build_context() on a RE-OPENED
        // session (simulating resume) exposes only the scoped prefix.
        let dir = std::env::temp_dir().join(format!(
            "picocode-cmp-resume-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone()).await.unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        session.append_message(&Message::user("intro")).await.unwrap();
        session.append_message(&Message::user("kept-a")).await.unwrap();
        session.append_message(&Message::user("kept-b")).await.unwrap();

        // find the id of "kept-a" (second message) to use as firstKeptEntryId
        let branch = session.get_branch().await;
        let kept_id = branch[1].id.clone();
        session.append_compaction("", &kept_id, 200).await.unwrap();
        session.append_message(&Message::user("after")).await.unwrap();

        // Re-open (resume) and verify the scoped context: [kept-a, kept-b, after]
        let ro = JsonlSessionStorage::open_readonly(path.clone()).await.unwrap();
        let index2 = ro.build_index().await.unwrap();
        let session2 = Session::new_persisted(ro, index2);
        let ctx = session2.build_context().await.unwrap();

        let texts: Vec<&str> = ctx.messages.iter().map(|m| match m {
            Message::User { content } => match &content[0] {
                UserContent::Text(t) => t.text.as_str(),
                _ => "",
            },
            _ => "",
        }).collect();
        assert_eq!(texts, ["kept-a", "kept-b", "after"]);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_context_at_pre_compaction_point_reactivates_prior_summary() {
        // The cache/prefix property the `compact llm` leaf dance relies on:
        // move the leaf to the entry just BEFORE the compaction point and
        // `build_context()` must produce EXACTLY the context a normal prompt
        // at that position would send — including re-activating a prior
        // compaction's summary as the leading `<summary>` wrapper.
        //
        // Session: u0 a0 c1{firstKept:u0, summary:"S1"} u1 a1 u2 (leaf).
        // Compacting at target=u2 -> predecessor = a1. Branch to a1 is
        // [u0 a0 c1 u1 a1]; latest compaction on that prefix is c1, so the
        // context must be [summary(S1)] + [u1 a1] (c1 itself stepped over,
        // u0 a0 covered by the summary).
        let dir = std::env::temp_dir().join(format!(
            "picocode-cmp-prefix-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone()).await.unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        session.append_message(&Message::user("u0")).await.unwrap();
        session.append_message(&Message::assistant("a0")).await.unwrap();
        // c1 summaries everything before u1 (firstKept=u0's successor). Use
        // the message id of u1 as firstKept.
        let branch = session.get_branch().await;
        let u0_id = branch[0].id.clone();
        session.append_compaction("S1", &u0_id, 100).await.unwrap();
        session.append_message(&Message::user("u1")).await.unwrap();
        session.append_message(&Message::assistant("a1")).await.unwrap();
        session.append_message(&Message::user("u2")).await.unwrap();

        // Leaf dance: move to the predecessor of a hypothetical new
        // compaction at u2 (i.e. a1). Find a1 by its content.
        let branch = session.get_branch().await;
        let a1_id = branch
            .iter()
            .find(|e| {
                if let crate::session::SessionEntryContent::Message { message } = &e.content {
                    message
                        .get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr[0].get("text"))
                        .and_then(|t| t.as_str())
                        == Some("a1")
                } else {
                    false
                }
            })
            .expect("a1 entry")
            .id
            .clone();
        session.move_leaf(Some(a1_id.clone())).await.unwrap();

        let ctx = session.build_context().await.unwrap();
        let texts: Vec<String> = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => match &content[0] {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        // Context at a1 = the prefix a normal prompt at a1 would receive:
        // [summary(S1)] + [u0] + [u1]. The prior compaction c1 re-activates
        // (S1 leading wrapper); c1 itself is stepped over; the kept range is
        // firstKept(u0) .. leaf(a1), which includes u0/a0 (pre-compaction
        // messages, summarized into S1) and u1/a1 (post-compaction). The
        // compaction entries never reach the wire.
        assert_eq!(texts.len(), 3, "user texts at a1: {texts:?}");
        assert!(texts[0].contains("S1"), "summary wrapper first: {texts:?}");
        assert!(texts[0].contains("<summary>"), "pi wrapper format: {texts:?}");
        assert_eq!(texts[1], "u0");
        assert_eq!(texts[2], "u1");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_context_at_exact_compaction_boundary_excludes_that_compaction() {
        // The strict-before rule, now emergent from the tree: a compaction AT
        // the target boundary is a DESCENDANT of the predecessor, so moving
        // the leaf to the predecessor excludes it from context. The new
        // summary therefore re-derives from raw messages — exactly what the
        // original compaction at that point saw.
        //
        // Session: u0 a0 c1{firstKept:u0} (leaf=c1). Re-compact at target=u0
        // -> predecessor = (root, none?) u0 has no predecessor -> guard
        // triggers. Use the MEANINGFUL case: target = a0 -> predecessor = u0.
        let dir = std::env::temp_dir().join(format!(
            "picocode-cmp-boundary-{}",
            std::process::id()
        ));
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone()).await.unwrap();
        let header = SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        let index = storage.build_index().await.unwrap();
        let session = Session::new_persisted(storage, index);

        session.append_message(&Message::user("u0")).await.unwrap();
        session.append_message(&Message::assistant("a0")).await.unwrap();
        let u0_id = session.get_branch().await[0].id.clone();
        session.append_compaction("S1", &u0_id, 50).await.unwrap();

        // Leaf dance: move to u0 (predecessor of a0, the new boundary).
        session.move_leaf(Some(u0_id.clone())).await.unwrap();

        let ctx = session.build_context().await.unwrap();
        let texts: Vec<String> = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => match &content[0] {
                    UserContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        // No compaction is on the [u0] prefix -> NO summary wrapper, just u0.
        assert_eq!(texts, vec!["u0"], "context at u0: {texts:?}");

        let _ = tokio::fs::remove_file(&path).await;
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
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId::new("tc_read").unwrap(),
                provider: None,
                function: ToolFunction::new("read".into(), serde_json::json!({"path": "x.rs"})),
                signature: None,
                additional_params: None,
            })],
        }).await.unwrap();
        session.append_message(&Message::User {
            content: vec![UserContent::ToolResult(RigToolResult {
                call: ToolCallId::new("tc_read").unwrap(),
                provider: None,
                name: "tool".into(),
                content: vec![ToolResultContent::Text(Text::new("file contents"))],
            })],
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
            content: vec![
                AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::new("tc_bash").unwrap(),
                    provider: None,
                    function: ToolFunction::new("bash".into(), serde_json::json!({"command": "ls"})),
                    signature: None,
                    additional_params: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::new("tc_read").unwrap(),
                    provider: None,
                    function: ToolFunction::new("read".into(), serde_json::json!({"path": "x.rs"})),
                    signature: None,
                    additional_params: None,
                }),
            ],
        }).await.unwrap();
        session.append_message(&Message::User {
            content: vec![
                UserContent::ToolResult(RigToolResult {
                    call: ToolCallId::new("tc_bash").unwrap(),
                    provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("file1.txt"))],
                }),
                UserContent::ToolResult(RigToolResult {
                    call: ToolCallId::new("tc_read").unwrap(),
                    provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("content"))],
                }),
            ],
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

    #[tokio::test]
    async fn test_count_persisted_user_prompts_counts_only_user_prompts() {
        // `count_persisted_user_prompts` is the session-side source of truth
        // for the retry de-dup guard: it must count genuine user prompts
        // (role "user") and EXCLUDE tool-result batches (role
        // "toolResult"), assistant messages, and non-message entries.
        let dir = std::env::temp_dir().join(format!(
            "picocode-count-user-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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

        use rig_core::message::{
            ToolCall, ToolFunction, ToolResult as RigToolResult, ToolResultContent,
        };

        // Empty session: 0 user prompts.
        assert_eq!(session.count_persisted_user_prompts().await, 0);

        // One genuine user prompt.
        session.append_message(&Message::user("hello")).await.unwrap();
        assert_eq!(session.count_persisted_user_prompts().await, 1);

        // An assistant turn does not add a user prompt.
        session
            .append_message(&Message::assistant("world"))
            .await
            .unwrap();
        assert_eq!(session.count_persisted_user_prompts().await, 1);

        // A tool-result batch (role "toolResult") must NOT be counted.
        session
            .append_message(&Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::new("tc_read").unwrap(),
                    provider: None,
                    function: ToolFunction::new("read".into(), serde_json::json!({})),
                    signature: None,
                    additional_params: None,
                })],
            })
            .await
            .unwrap();
        session
            .append_message(&Message::User {
                content: vec![UserContent::ToolResult(RigToolResult {
                    call: ToolCallId::new("tc_read").unwrap(),
                    provider: None,
                    name: "tool".into(),
                    content: vec![ToolResultContent::Text(Text::new("file contents"))],
                })],
            })
            .await
            .unwrap();
        assert_eq!(session.count_persisted_user_prompts().await, 1);

        // A second user prompt now makes 2.
        session.append_message(&Message::user("again")).await.unwrap();
        assert_eq!(session.count_persisted_user_prompts().await, 2);

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

    // ------------------------------------------------------------------
    // Text-only compaction summary
    // ------------------------------------------------------------------

    fn sum_user(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(|p| p.into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn sum_assistant(id: &str, parent: Option<&str>, content: serde_json::Value) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(|p| p.into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": content
                }),
            },
        }
    }

    #[test]
    fn text_only_summary_user_prompts_and_assistant_text() {
        let session_dir = std::path::PathBuf::from("/test");
        let dropped = vec![
            sum_user("m0", None, "first prompt"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "reasoning", "text": "deep thoughts"},
                {"type": "text", "text": "first answer"},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "src/lib.rs"}}
            ])),
            // Tool result — must be excluded entirely.
            SessionEntry {
                id: "t0".into(),
                parent_id: Some("a0".into()),
                timestamp: "2025-01-01T00:00:01.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "toolResult",
                        "content": [{"type": "text", "text": "file contents here"}]
                    }),
                },
            },
            sum_user("m1", Some("t0"), "second prompt"),
            sum_assistant("a1", Some("m1"), serde_json::json!([
                {"type": "text", "text": "second answer"}
            ])),
        ];
        let s = build_text_only_summary(&dropped, &session_dir);
        assert!(s.contains("# [user]\nfirst prompt"));
        assert!(s.contains("# [assistant]\nfirst answer"));
        assert!(s.contains("# [user]\nsecond prompt"));
        assert!(s.contains("# [assistant]\nsecond answer"));
        // No reasoning, no tool syntax, no tool output.
        assert!(!s.contains("deep thoughts"));
        assert!(!s.contains("toolCall"));
        assert!(!s.contains("read("));
        assert!(!s.contains("file contents here"));
        // Turn order preserved.
        let first = s.find("first prompt").unwrap();
        let second = s.find("second prompt").unwrap();
        assert!(first < second);
        // HR separators before every section: 2 user turns, 2 assistant
        // responses, and 1 files section (the .rs read in this span
        // belongs to "Files", not "Documents", so the split is a single
        // group here). (The very first separator has its leading newline
        // trimmed off.)
        assert_eq!(s.matches("---\n# [").count(), 5);
        assert!(s.starts_with("---\n# [user]\nfirst prompt"));
    }

    #[test]
    fn text_only_summary_lists_touched_files() {
        let session_dir = std::path::PathBuf::from("/test");
        let dropped = vec![
            sum_user("m0", None, "do work"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "text", "text": "working"},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "b.txt", "offset": 0, "limit": 10}},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "a.txt"}},
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "a.txt", "edits": []}},
                {"type": "toolCall", "name": "bash",
                 "arguments": {"command": "ls"}},
            ])),
        ];
        let s = build_text_only_summary(&dropped, &session_dir);
        // No .md files in this span, so only the "Files" section appears.
        assert!(s.contains("# [Files]"));
        assert!(!s.contains("# [Documents]"));
        // Modified files listed as modified (read-then-edit promotes).
        assert!(s.contains("- modified: a.txt"));
        assert!(s.contains("- read: b.txt"));
        // Non-file tools ignored; a.txt appears once in the read list and
        // once as modified, but NOT as a plain read (promoted).
        let read_lines: Vec<&str> = s.lines().filter(|l| l.starts_with("- read: ")).collect();
        assert_eq!(read_lines, vec!["- read: b.txt"]);
        // No tool-call syntax or arguments leaked.
        assert!(!s.contains("toolCall"));
        assert!(!s.contains("arguments"));
        // Modified before reads.
        let m = s.find("- modified:").unwrap();
        let r = s.find("- read:").unwrap();
        assert!(m < r);
    }

    #[test]
    fn text_only_summary_empty_for_empty_span() {
        let session_dir = std::path::PathBuf::from("/test");
        assert_eq!(build_text_only_summary(&[], &session_dir), "");
        // Tool results only → still empty.
        let dropped = vec![SessionEntry {
            id: "t".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "toolResult",
                    "content": [{"type": "text", "text": "output"}]
                }),
            },
        }];
        assert_eq!(build_text_only_summary(&dropped, &session_dir), "");
    }

    #[test]
    fn text_only_summary_end_of_compacted_context_check() {
        let session_dir = std::path::PathBuf::from("/test");
        // The summary is written into a Compaction entry; applying the
        // compaction transforms must project it as the FIRST context message,
        // before the kept messages (pi contract).
        let dropped = vec![
            sum_user("m0", None, "early task"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "text", "text": "early answer"},
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "src/lib.rs", "edits": []}}
            ])),
        ];
        let summary = build_text_only_summary(&dropped, &session_dir);
        assert!(!summary.is_empty());
        assert!(summary.contains("- modified: src/lib.rs"));

        // Branch: dropped span + compaction entry (with summary) + kept span.
        let branch = vec![
            sum_user("m0", None, "early task"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "text", "text": "early answer"}
            ])),
            SessionEntry {
                id: "cmp".into(),
                parent_id: Some("a0".into()),
                timestamp: "2025-01-01T00:00:02.000Z".into(),
                content: SessionEntryContent::Compaction {
                    summary: summary.clone(),
                    first_kept_entry_id: "m1".into(),
                    tokens_before: 12_345,
                },
            },
            sum_user("m1", Some("cmp"), "kept prompt"),
        ];
        let transformed = apply_compaction_transforms(branch);
        // [summary user message, kept user message]
        assert_eq!(transformed.len(), 2);
        let first = &transformed[0];
        let text = match &first.content {
            SessionEntryContent::Message { message } => {
                message.pointer("/content/0/text").unwrap().as_str().unwrap()
            }
            other => panic!("expected Message, got {:?}", std::mem::discriminant(other)),
        };
        // pi wrapper + our summary as the FIRST message of the context.
        assert!(text.starts_with(COMPACTION_SUMMARY_PREFIX));
        assert!(text.contains("# [user]\nearly task"));
        assert!(text.contains("[assistant]"));
        // src/lib.rs is non-markdown → it lands in the Files section.
        assert!(text.contains("# [Files]"));
        assert!(!text.contains("# [Documents]"));
        assert!(text.contains("- modified: src/lib.rs"));
        let second_text = match &transformed[1].content {
            SessionEntryContent::Message { message } => {
                message.pointer("/content/0/text").unwrap().as_str().unwrap()
            }
            _ => panic!("expected Message"),
        };
        assert_eq!(second_text, "kept prompt");
    }

    #[test]
    fn is_document_path_classifies_markdown_only() {
        // Plain `.md` / `.markdown` (case-insensitive) are documents.
        assert!(is_document_path("README.md"));
        assert!(is_document_path("readme.MD"));
        assert!(is_document_path("notes.markdown"));
        assert!(is_document_path("Notes.Markdown"));
        // Subpaths preserved (only the suffix matters).
        assert!(is_document_path("docs/spec/plan.md"));
        assert!(is_document_path("a\\b\\c.md"));
        // Non-markdown is not a document, even if it looks "document-y".
        assert!(!is_document_path("main.rs"));
        assert!(!is_document_path("data.json"));
        assert!(!is_document_path("notes.mdx"));
        assert!(!is_document_path("notes.md5"));
        // Edge: file literally named `.md` is a document.
        assert!(is_document_path(".md"));
        // Trailing dot does NOT match (we only treat the final suffix).
        assert!(!is_document_path("notes.md."));
    }

    #[test]
    fn text_only_summary_splits_documents_and_files() {
        let session_dir = std::path::PathBuf::from("/test");
        // The dropped span touches both markdown and non-markdown files;
        // the text-only summary must render them under separate headers,
        // with each group sorted (modified before reads, alphabetical
        // within).
        let dropped = vec![
            sum_user("m0", None, "do work"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "text", "text": "working"},
                // Non-markdown (code) — goes to "Files".
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "src/lib.rs", "edits": []}},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "README.md"}},
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "docs/plan.md", "edits": []}},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "CHANGELOG.md"}},
            ])),
        ];
        let s = build_text_only_summary(&dropped, &session_dir);
        // Both headers present.
        assert!(s.contains("# [Documents]"), "missing Documents section: {s}");
        assert!(s.contains("# [Files]"), "missing Files section: {s}");
        // Documents section: only .md files, modified (docs/plan.md) before
        // read (README.md, CHANGELOG.md), alphabetical within each.
        let docs_start = s.find("# [Documents]").unwrap();
        let files_start = s.find("# [Files]").unwrap();
        assert!(docs_start < files_start);
        let docs_block = &s[docs_start..files_start];
        assert!(docs_block.contains("- modified: docs/plan.md"));
        assert!(docs_block.contains("- read: CHANGELOG.md"));
        assert!(docs_block.contains("- read: README.md"));
        // No non-.md in Documents.
        assert!(!docs_block.contains("src/lib.rs"));
        // Files section: only non-.md, modified (src/lib.rs).
        let files_block = &s[files_start..];
        assert!(files_block.contains("- modified: src/lib.rs"));
        assert!(!files_block.contains("README.md"));
        assert!(!files_block.contains("docs/plan.md"));
        // Modified still comes before reads in each group.
        let docs_m = docs_block.find("- modified:").unwrap();
        let docs_r = docs_block.find("- read:").unwrap();
        assert!(docs_m < docs_r);
    }

    #[test]
    fn text_only_summary_omits_empty_files_group() {
        let session_dir = std::path::PathBuf::from("/test");
        // If the dropped span touches ONLY markdown files, the Files
        // section is empty and must be omitted (no header, no empty
        // bullets). Same for the converse.
        let only_md = vec![
            sum_user("m0", None, "read docs"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "README.md"}},
            ])),
        ];
        let s = build_text_only_summary(&only_md, &session_dir);
        assert!(s.contains("# [Documents]"));
        assert!(!s.contains("# [Files]"));

        let only_rs = vec![
            sum_user("m0", None, "read code"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "src/lib.rs"}},
            ])),
        ];
        let s = build_text_only_summary(&only_rs, &session_dir);
        assert!(s.contains("# [Files]"));
        assert!(!s.contains("# [Documents]"));
    }

    #[test]
    fn build_manual_template_is_empty_when_no_files_touched() {
        let session_dir = std::path::PathBuf::from("/test");
        // No file-touching tool calls in the dropped span → no file section,
        // but the seed text (first user prompt) is always included.
        let dropped = vec![
            sum_user("m0", None, "no files"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "text", "text": "just text"},
                {"type": "toolCall", "name": "bash",
                 "arguments": {"command": "ls"}},
            ])),
        ];
        let t = build_manual_template(&dropped, None, &session_dir);
        // Seed text is always present; no file sections since bash doesn't touch files.
        assert_eq!(
            t,
            "# Compaction notes\n\nno files"
        );
    }

    #[test]
    fn build_manual_template_splits_documents_and_files() {
        let session_dir = std::path::PathBuf::from("/test");
        // The manual template is a user-editable scaffold: file lists
        // preceded by the first user prompt as seed text.
        let dropped = vec![
            sum_user("m0", None, "do work"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "src/lib.rs", "edits": []}},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "README.md"}},
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "docs/plan.md", "edits": []}},
            ])),
        ];
        let t = build_manual_template(&dropped, None, &session_dir);
        // Title section ends with the first file section heading.
        assert!(t.contains("# Compaction notes\n\n"));
        assert!(t.contains("\n---\n# [Documents]"));
        // Seed text (first user prompt) appears after the title.
        assert!(t.contains("do work"));
        // Both sections present, Documents before Files.
        let docs_i = t.find("# [Documents]").unwrap();
        let files_i = t.find("# [Files]").unwrap();
        assert!(docs_i < files_i);
        // Documents: only .md files, in modified-then-read order.
        let docs_block = &t[docs_i..files_i];
        assert!(docs_block.contains("- modified: docs/plan.md"));
        assert!(docs_block.contains("- read: README.md"));
        assert!(!docs_block.contains("src/lib.rs"));
        // Files: only non-.md files.
        let files_block = &t[files_i..];
        assert!(files_block.contains("- modified: src/lib.rs"));
        assert!(!files_block.contains("README.md"));
        // No tool-call syntax / arguments leaked.
        assert!(!t.contains("toolCall"));
        assert!(!t.contains("arguments"));
    }

    #[test]
    fn build_manual_template_omits_empty_group() {
        let session_dir = std::path::PathBuf::from("/test");
        // Only non-.md touched: no Documents section, only Files.
        let dropped = vec![
            sum_user("m0", None, "code work"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "toolCall", "name": "edit",
                 "arguments": {"path": "src/lib.rs", "edits": []}},
            ])),
        ];
        let t = build_manual_template(&dropped, None, &session_dir);
        assert!(!t.contains("# [Documents]"));
        assert!(t.contains("# [Files]"));
        assert!(t.contains("- modified: src/lib.rs"));
        // Seed text from first prompt is included.
        assert!(t.contains("code work"));

        // Only .md touched: no Files section, only Documents.
        let dropped = vec![
            sum_user("m0", None, "doc work"),
            sum_assistant("a0", Some("m0"), serde_json::json!([
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "README.md"}},
            ])),
        ];
        let t = build_manual_template(&dropped, None, &session_dir);
        assert!(t.contains("# [Documents]"));
        assert!(!t.contains("# [Files]"));
        assert!(t.contains("- read: README.md"));
        assert!(t.contains("doc work"));
    }

    // ------------------------------------------------------------------
    // Summary parsing and section extraction
    // ------------------------------------------------------------------

    #[test]
    fn split_sections_parses_standard_sections() {
        let input = "preamble text\n\n---\n# [Documents]\n- path.md\n\n---\n# [Files]\n- path.rs";
        let (preamble, sections) = split_sections(input);
        assert_eq!(preamble, "preamble text");
        assert_eq!(sections.get("documents").unwrap(), "- path.md");
        assert_eq!(sections.get("files").unwrap(), "- path.rs");
    }

    #[test]
    fn split_sections_ignores_heading_levels_and_brackets() {
        let input = "## documents\n- a.md\n### [Files]\n- b.rs\n# FILES\n- c.txt\n## Documents\n- d.md";
        let (preamble, sections) = split_sections(input);
        // Preamble is empty (no text before first heading).
        assert!(preamble.is_empty());
        // Last occurrence wins per label.
        assert_eq!(sections.get("documents").unwrap(), "- d.md");
        assert_eq!(sections.get("files").unwrap(), "- c.txt");
    }

    #[test]
    fn extractor_parses_standard_bullets() {
        let input = "- modified: path.rs\n- read: path.md\n- /absolute/path.js\n* read: other.rs";
        let mut ex = Extractor::new();
        for line in input.lines() {
            ex.ingest(line);
        }
        assert_eq!(ex.documents.len(), 1);
        assert_eq!(ex.documents[0].0, "path.md");
        assert!(!ex.documents[0].1); // read
        assert_eq!(ex.files.len(), 3);
        assert_eq!(ex.files[0].0, "path.rs");
        assert!(ex.files[0].1); // modified
        assert_eq!(ex.files[1].0, "/absolute/path.js");
        assert_eq!(ex.files[2].0, "other.rs");
        assert!(!ex.files[2].1); // read (from "* read:")
    }

    #[test]
    fn extractor_parses_descriptor_variations() {
        let input = "- Design doc: path.md\n- Progress doc; update after each turn: another.md\n- Some sentence: /path/file.rs\n- text with `some` [markup](https://example.com): path.js";
        let mut ex = Extractor::new();
        for line in input.lines() {
            ex.ingest(line);
        }
        // .md files go to documents; everything else to files.
        assert_eq!(ex.documents.len(), 2);
        assert_eq!(ex.documents[0].0, "path.md");
        assert!(!ex.documents[0].1); // read/modified hint absent → false
        assert_eq!(ex.documents[1].0, "another.md");
        assert_eq!(ex.files.len(), 2);
        assert_eq!(ex.files[0].0, "/path/file.rs");
        assert!(!ex.files[0].1);
        assert_eq!(ex.files[1].0, "path.js");
        assert!(!ex.files[1].1);
    }

    #[test]
    fn extractor_ignores_non_file_lines() {
        let input = "Some prose that is not a list\n- modified: path.rs\n\n---\n  regular text after a list";
        let mut ex = Extractor::new();
        for line in input.lines() {
            ex.ingest(line);
        }
        assert_eq!(ex.files.len(), 1);
        assert_eq!(ex.unparsed.len(), 2);
        assert!(ex.unparsed.contains(&"Some prose that is not a list"));
    }

    #[test]
    fn extractor_skips_urls_and_incomplete_paths() {
        let input = "- https://example.com\n- user@host\n- no-extension\n- modified: path.rs";
        let mut ex = Extractor::new();
        for line in input.lines() {
            ex.ingest(line);
        }
        assert_eq!(ex.files.len(), 1); // only path.rs
        assert_eq!(ex.unparsed.len(), 3); // url, email, no-ext
    }

    #[test]
    fn parse_summary_carries_forward_files_and_notes() {
        // Files from both Documents and Files sections are merged (both contribute
        // to the returned list; `build_manual_template` re-splits by extension
        // when rendering). Non-file-section content before the first file section
        // heading is returned as notes.
        let summary = "Context about the project.\n\n---\n# [Documents]\n- read: old.md\n\n---\n# [Files]\n- modified: old.rs";
        let (all_files, notes) = parse_summary_into_sections(summary, &["files", "documents"]);
        // old.md (document) + old.rs (file) = 2 total.
        assert_eq!(all_files.len(), 2);
        assert!(all_files.iter().any(|f| f.path == "old.md" && !f.modified));
        assert!(all_files.iter().any(|f| f.path == "old.rs" && f.modified));
        // Notes = pre-file-section text.
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("Context about the project"));
    }

    #[test]
    fn parse_summary_handles_malformed_input_gracefully() {
        // Non-list content gets preserved as unparsed.
        let summary = "Some notes.\n\n---\n# [Files]\nThis is not a list item\n- good: path.rs";
        let (files, notes) = parse_summary_into_sections(summary, &["files", "documents"]);
        assert_eq!(files.len(), 1);
        // Non-file line preserved in notes.
        assert!(notes.iter().any(|n| n.contains("not a list")));
    }

    // ------------------------------------------------------------------
    // Pivot context and seed extraction
    // ------------------------------------------------------------------

    #[test]
    fn extract_seed_from_context_returns_none_for_empty_context() {
        use crate::session::types::SessionContext;
        let ctx = SessionContext {
            messages: vec![],
            thinking_level: None,
            model: None,
        };
        assert!(crate::session::Session::extract_seed_from_context(&ctx).is_none());
    }

    #[test]
    fn extract_seed_from_context_strips_prefix_and_suffix() {
        use crate::session::types::SessionContext;
        use rig_core::message::{Message, UserContent, Text};
        let summary_text = "Summary body content";
        // Reconstruct the exact wrapper used by compaction_summary_message.
        let prefix = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
        let suffix = "\n</summary>";
        let wrapped = format!("{}{}{}", prefix, summary_text, suffix);
        let ctx = SessionContext {
            messages: vec![Message::User {
                content: vec![UserContent::Text(Text::new(wrapped))],
            }],
            thinking_level: None,
            model: None,
        };
        assert_eq!(
            crate::session::Session::extract_seed_from_context(&ctx).as_deref(),
            Some("Summary body content")
        );
    }

    #[test]
    fn extract_seed_from_context_returns_none_for_non_user_message() {
        use crate::session::types::SessionContext;
        use rig_core::message::Message;
        let ctx = SessionContext {
            messages: vec![Message::Assistant {
                id: None,
                content: vec![],
            }],
            thinking_level: None,
            model: None,
        };
        assert!(crate::session::Session::extract_seed_from_context(&ctx).is_none());
    }

    // ------------------------------------------------------------------
    // Relativize path
    // ------------------------------------------------------------------

    #[test]
    fn relativize_path_returns_relative_for_sub_path() {
        // Paths below session_dir become compact relative (no leading ./)
        // Use a temp dir + file so canonicalize works on both sides.
        let tmp = std::path::PathBuf::from(std::env::temp_dir())
            .join("picocode_relpath_test");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("marker"), b"").unwrap();
        // Create the nested file so canonicalize resolves the path.
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/main.rs"), b"").unwrap();
        let result = crate::session::session::relativize_path(
            &tmp,
            tmp.join("src/main.rs").to_str().unwrap(),
        );
        assert_eq!(result, Some("src/main.rs".into()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn relativize_path_returns_none_for_parent_path() {
        let tmp = std::path::PathBuf::from(std::env::temp_dir())
            .join("picocode_relpath_test2");
        std::fs::create_dir_all(&tmp).unwrap();
        // The parent of session_dir is not a sub-path.
        std::fs::write(tmp.join("marker"), b"").unwrap();
        let result = crate::session::session::relativize_path(
            &tmp,
            std::env::temp_dir().to_str().unwrap(),
        );
        assert_eq!(result, None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn relativize_path_returns_none_for_equal_path() {
        let tmp = std::path::PathBuf::from(std::env::temp_dir())
            .join("picocode_relpath_test3");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("marker"), b"").unwrap();
        // The path itself is the session_dir; strip_prefix succeeds but yields "".
        let result = crate::session::session::relativize_path(&tmp, tmp.to_str().unwrap());
        assert_eq!(result, None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn relativize_path_returns_none_for_nonexistent() {
        let tmp = std::path::PathBuf::from(std::env::temp_dir())
            .join("picocode_relpath_test4");
        std::fs::create_dir_all(&tmp).unwrap();
        // A nonexistent file inside the session_dir -> canonicalize fails -> None.
        std::fs::write(tmp.join("marker"), b"").unwrap();
        let result = crate::session::session::relativize_path(
            &tmp,
            tmp.join("nonexistent.rs").to_str().unwrap(),
        );
        assert_eq!(result, None);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
