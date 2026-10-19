# Implemented: Reorder Auto-Recorded Files by Last Access in Compaction

## Problem Statement

Currently, `merge_files_with_previous` maintains files in this order:
1. Files from previous summary (original order, custom descriptors preserved)
2. Newly touched files from current span (chronological append)
3. Re-touched files promoted in-place at their original position

**Desired behavior:**
- Custom-labeled files (descriptor ≠ "read"/"modified"/None) stay at front, preserving relative order
- Auto-recorded files (descriptor = "read"/"modified"/None) move to end, ordered by **last access in current compaction round** (most recent last)
- "Last access" = last appearance in assistant tool calls within `entries` being compacted

## Key Definitions

- **Custom-labeled file**: `TouchedFile.descriptor` is `Some(d)` where `d ∉ {"read", "modified", ""}`
- **Auto-recorded file**: `TouchedFile.descriptor` is `None`, `Some("")`, `Some("read")`, or `Some("modified")`
- **Current compaction round**: The `entries` slice passed to `merge_files_with_previous`

## Implementation Plan

### Algorithm: Single-Pass Move-to-End

Walk the assistant messages in chronological order. Maintain a single `files: Vec<TouchedFile>` list. For each file access in the current span:
- If the file has a **custom label** (descriptor ≠ "read"/"modified"/""/None): **leave in place**
- If the file is **auto-recorded** (descriptor = "read"/"modified"/""/None) **or not yet in the list**: **move/append to end**

This naturally produces:
- Custom-labeled files at front, original relative order preserved
- Auto-recorded files at end, ordered by last access (most recent last)

### 1. Parse Previous Summary (Unchanged)

```rust
let mut files: Vec<TouchedFile> = Vec::new();
let mut notes: Vec<String> = Vec::new();

if let Some(summary) = previous_summary {
    let (parsed_files, parsed_notes) =
        parse_summary_into_sections(summary, &["files", "documents"]);
    files.extend(parsed_files);
    notes = parsed_notes;
}
```

### 2. Build Fast Lookup: Path → Index in `files`

```rust
let mut path_to_index: HashMap<String, usize> = 
    files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
```

### 3. Walk Entries in Chronological Order

```rust
for entry in entries {
    if let SessionEntryContent::Message { message } = &entry.borrow().content {
        if message.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            // Extract all (path, modified) from tool calls in this message
            let touches = extract_touches_from_message(message, session_dir);
            for (path, modified) in touches {
                handle_touch(&mut files, &mut path_to_index, path, modified);
            }
        }
    }
}
```

### 4. Handle a Single Touch

```rust
fn handle_touch(
    files: &mut Vec<TouchedFile>,
    path_to_index: &mut HashMap<String, usize>,
    path: String,
    modified: bool,
) {
    let is_custom = |f: &TouchedFile| -> bool {
        matches!(f.descriptor.as_deref(), Some(d) if d != "read" && d != "modified" && !d.is_empty())
    };

    if let Some(&idx) = path_to_index.get(&path) {
        // File already in list
        if is_custom(&files[idx]) {
            // Custom-labeled: do nothing (stays in place)
            // But still promote read→modified if needed
            if modified && !files[idx].modified {
                files[idx].modified = true;
                if files[idx].descriptor.as_deref() == Some("read") {
                    files[idx].descriptor = Some("modified".to_string());
                }
            }
        } else {
            // Auto-recorded: move to end
            let mut file = files.remove(idx);
            // Update indices for shifted elements
            for i in idx..files.len() {
                path_to_index.insert(files[i].path.clone(), i);
            }
            // Promote read→modified
            if modified {
                file.modified = true;
                if file.descriptor.as_deref() == Some("read") {
                    file.descriptor = Some("modified".to_string());
                }
            }
            // Append to end
            let new_idx = files.len();
            files.push(file);
            path_to_index.insert(path, new_idx);
        }
    } else {
        // New file: append to end (auto-recorded by default)
        let new_file = TouchedFile {
            path: path.clone(),
            modified,
            descriptor: None,
        };
        let new_idx = files.len();
        files.push(new_file);
        path_to_index.insert(path, new_idx);
    }
}
```

### 5. Extract Touches from Message

```rust
fn extract_touches_from_message(
    message: &serde_json::Value,
    session_dir: &std::path::Path,
) -> Vec<(String, bool)> {
    let mut touches = Vec::new();
    if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
        for item in content {
            if item.get("type").and_then(|t| t.as_str()) != Some("toolCall") {
                continue;
            }
            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let modified = matches!(name, "write" | "edit");
            if !matches!(name, "read" | "write" | "edit") {
                continue;
            }
            if let Some(path) = item
                .get("arguments")
                .and_then(|a| a.get("path"))
                .and_then(|p| p.as_str())
            {
                let normalized = relativize_path(session_dir, path).unwrap_or_else(|| path.to_string());
                touches.push((normalized, modified));
            }
        }
    }
    touches
}
```

### 6. Files to Modify

- `src/session/session.rs`: Replace `merge_files_with_previous` with the single-pass version; remove `collect_touched_files` (or keep for other uses); add `extract_touches_from_message` and `handle_touch` helpers.

### 7. Tests to Update/Add

Same tests as before — they assert final ordering, which now comes from the move-to-end walk.

### 8. Backward Compatibility

- Custom-labeled files: **no change** (never moved)
- Auto-recorded files: **reordered** by last access (most recent last)
- Round-trip stability: Custom labels survive; auto-recorded order changes per compaction round (expected)

## Pseudocode Summary

```rust
fn merge_files_with_previous(...) -> (Vec<TouchedFile>, Vec<String>) {
    // 1. Parse previous summary
    let mut files = parse_previous_summary(...);
    
    // 2. Build path → index map
    let mut path_to_index = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        path_to_index.insert(f.path.clone(), i);
    }
    
    // 3. Walk entries chronologically
    for entry in entries {
        if let Some(touches) = extract_touches_from_assistant_message(entry) {
            for (path, modified) in touches {
                handle_touch(&mut files, &mut path_to_index, path, modified);
            }
        }
    }
    
    (files, notes)
}

fn handle_touch(files, path_to_index, path, modified) {
    if let Some(idx) = path_to_index.get(&path).copied() {
        if is_custom(&files[idx]) {
            // Custom: stay put, maybe promote read→modified
            promote_if_needed(&mut files[idx], modified);
        } else {
            // Auto: move to end
            let mut file = files.remove(idx);
            // Fix shifted indices
            for i in idx..files.len() { path_to_index.insert(files[i].path.clone(), i); }
            promote_if_needed(&mut file, modified);
            path_to_index.insert(path, files.len());
            files.push(file);
        }
    } else {
        // New file: append
        path_to_index.insert(path.clone(), files.len());
        files.push(TouchedFile { path, modified, descriptor: None });
    }
}
```

## Complexity

- Time: O(n × m) where n = entries, m = files (typically small; move is O(m) but m ≤ ~50)
- Space: O(m) for the index map
- No sorting step needed
- Single pass, streaming-friendly

## Risk Assessment

- **Low risk**: Purely presentational reordering in summary generation
- **No data loss**: All files still included, just reordered
- **Test updates needed**: ~3-5 tests assert specific ordering

## Implementation Status

**✅ COMPLETE** - All tests pass (1056 tests).

### Files Modified
- `src/session/session.rs`: Replaced `merge_files_with_previous` with single-pass move-to-end algorithm; added helpers `is_custom_labeled`, `promote_if_needed`, `handle_touch`, `extract_touches_from_message`.

### Behavior
- Custom-labeled files (descriptor ≠ "read"/"modified"/"") stay at front, original relative order preserved
- Auto-recorded files (descriptor = "read"/"modified"/""/None) moved to end, ordered by last access in current compaction span (most recent last)
- Read→modified promotion works for both custom and auto-recorded files
- Round-trip stability preserved for custom labels