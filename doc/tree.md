# `/tree` Menu — Session Tree Navigator

## Overview

The `/tree` overlay shows the current session's entry tree (a branchable conversation
history), lets the user select a point to continue from, and optionally summarizes
the abandoned branch via the LLM.

This mirrors pi's tree navigator (`packages/coding-agent/src/modes/interactive/components/tree-selector.ts`)
but adapted to picocode's architectural constraints: no TypeScript async, no extension
hooks, no separate agent-session layer.

## Critique of this plan (read before implementing)

The plan below is directionally correct but contains several concrete errors that
would not compile or would corrupt the session tree. Fix these while implementing:

1. **Missing enum variant.** `SessionEntryContent` also has an
   `ActiveToolsChange { active_tool_names }` variant (see `types.rs:161`). The
   overview list and the `entry_display_label` `match` both omit it, so the match
   is non-exhaustive and will not compile. Every `match &entry.content` in this
   doc must handle `ActiveToolsChange` (render as meta / skip).
2. **`Leaf` entries must not become tree nodes.** The existing append helpers
   (`append_message`, `append_model_change`, …) write the leaf pointer with
   `SessionEntry::new_leaf(Some(id), None)` — parent `None` — and **never** insert
   it into `index.entries` or `index.children`; they only set `index.leaf_id`.
   The `move_leaf`/`append_*` code below instead gives the Leaf a parent and
   inserts it into the tree, which pollutes `build_tree()` with phantom nodes.
   Follow the existing pattern exactly (corrected code inline below).
3. **Navigation drops thinking level and model.** `build_context()` already
   returns `thinking_level` and `model` for the branch. Phase 3.2 only copies
   `ctx.messages` into the agent, so after navigating to a branch with a different
   model/thinking level the agent keeps the old ones. Apply all three.
4. **`build_tree()` is O(n²).** Both `subtree_contains` (called per node during
   sort) and the per-node `entries.values().find_map(...)` label scan re-walk the
   whole map. Precompute a `target_id -> label` map once and a single
   `active_path: HashSet<&str>` from `get_path_to_root(leaf)` before recursing.
5. **Naming/signature drift.** The REPL meta type is `SessionMeta` (not
   `ReplSessionMeta`), and `build_session_ctx` currently returns a 2-tuple
   `(Vec<SessionMeta>, Vec<(String,String)>)`. Extend it consistently.
6. **Phase 3.2 does not compile as written:** `common_ancestor_id` is never
   defined; `target_id` is moved into `move_leaf` then used again in the final
   `format!`; and `old_leaf_id.unwrap_or_default()` moves a value read earlier.
   Corrected sketch inline below.
7. **Emoji width.** 🏷️/📝/📦 include variation selectors and render at
   ambiguous widths; since rows are pre-rendered and later width-measured, prefer
   ASCII markers (or verify against the existing width helper) to avoid cursor
   drift in the overlay.

## Current state

The session storage layer already supports a tree structure:

- `SessionEntry` has `id`, `parent_id`, `timestamp`, and a `SessionEntryContent` enum
  with `Message`, `ThinkingLevelChange`, `ModelChange`, `ActiveToolsChange`,
  `Compaction`, `BranchSummary`, `Label`, `SessionInfo`, `Custom`, `CustomMessage`,
  and `Leaf` variants (verify against `types.rs:139` — the set must be handled
  exhaustively everywhere)
- `SessionIndex` has an `entries: HashMap<String, SessionEntry>`, a
  `children: HashMap<String, Vec<String>>`, and a `leaf_id: Option<String>`
- `SessionIndex::get_path_to_root()` walks from leaf to root (used by context building)
- `Session::get_branch()` and `Session::build_context()` already traverse the tree

What's missing:

- A `build_tree()` method to produce a recursive tree structure from the flat index
- Navigation methods (`move_leaf`, `get_entry`, `get_tree`)
- Entry constructors for `BranchSummary`, `Compaction`, `Label`
- The REPL overlay (display, selection, keyboard navigation)
- A command handler that modifies the session's leaf pointer
- Optional LLM summarization of abandoned branches

## Phase 1 — Tree building & scaffolding

### 1.1 SessionTreeNode type + build_tree()

**File:** `src/session/types.rs`

Add a recursive tree node type:

```rust
/// A node in the session entry tree.
#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
    /// Resolved label text, if any Label entry targets this node.
    pub label: Option<String>,
}
```

Add a `build_tree()` method on `SessionIndex`:

```rust
impl SessionIndex {
    /// Build the full entry tree, rooted at entries with no parent.
    /// Children are sorted so the branch containing `preferred_id`
    //  (typically the current leaf) appears first.
    pub fn build_tree(&self, preferred_id: Option<&str>) -> Vec<SessionTreeNode> {
        // 1. Find root entries (parent_id is None or not in entries map)
        // 2. For each root, build_subtree()
        // 3. Sort so the subtree containing `preferred_id` is first
    }

    fn build_subtree(
        &self,
        entry_id: &str,
        preferred_id: Option<&str>,
    ) -> Option<SessionTreeNode> {
        let entry = self.entries.get(entry_id)?.clone();
        let mut children: Vec<SessionTreeNode> = self.children
            .get(entry_id)
            .map(|child_ids| {
                child_ids.iter()
                    .filter_map(|cid| self.build_subtree(cid, preferred_id))
                    .collect()
            })
            .unwrap_or_default();

        // Sort: branch containing preferred_id first
        if let Some(pref) = preferred_id {
            children.sort_by_key(|c| {
                if subtree_contains(&self.entries, &self.children, &c.entry.id, pref) {
                    0
                } else {
                    1
                }
            });
        }

        // Resolve label for this node.
        // NOTE (perf, critique #4): this per-node `entries.values()` scan makes
        // build_tree O(n²). Precompute a `HashMap<&str, &str>` of
        // target_id -> label once in `build_tree()` and pass it down instead.
        let label = self.entries.values()
            .find_map(|e| match &e.content {
                SessionEntryContent::Label { target_id, label }
                    if target_id == entry_id => label.clone(),
                _ => None,
            });

        Some(SessionTreeNode { entry, children, label })
    }
}
```

Helper function `subtree_contains`:

```rust
fn subtree_contains(
    entries: &HashMap<String, SessionEntry>,
    children: &HashMap<String, Vec<String>>,
    root_id: &str,
    target_id: &str,
) -> bool {
    if root_id == target_id { return true; }
    if let Some(child_ids) = children.get(root_id) {
        child_ids.iter().any(|cid| subtree_contains(entries, children, cid, target_id))
    } else {
        false
    }
}
```

### 1.2 Entry constructors

**File:** `src/session/types.rs`

Add constructors that already exist as enum variants but lack builder methods:

```rust
impl SessionEntry {
    // Existing:
    // pub fn new_leaf(target_id, parent_id)
    // pub fn new_session_info(name, parent_id)
    // pub fn new_model_change(provider, model_id, parent_id)
    // pub fn new_thinking_level_change(level, parent_id)

    // New:
    pub fn new_branch_summary(from_id: String, summary: String, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::BranchSummary { from_id, summary },
        }
    }

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

    pub fn new_label(target_id: String, label: Option<String>, parent_id: Option<String>) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            parent_id,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            content: SessionEntryContent::Label { target_id, label },
        }
    }
}
```

### 1.3 Navigation methods on Session

**File:** `src/session/session.rs`

Add methods that the REPL and command handler need:

```rust
impl Session {
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
    /// Writes a `Leaf` pointer record to storage and updates `leaf_id` only.
    ///
    /// IMPORTANT: matches the existing append helpers — the `Leaf` entry is a
    /// pointer record, NOT a tree node. It is written with parent `None` and is
    /// deliberately never inserted into `entries`/`children`.
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
            from_id.to_string(), summary.to_string(), parent_id,
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
            summary.to_string(), first_kept_entry_id.to_string(), tokens_before, parent_id,
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
            target_id.to_string(), Some(label.to_string()), parent_id,
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
}
```

## Phase 2 — `/tree` REPL overlay

### 2.1 Register the slash command

**File:** `src/slash.rs`

```rust
pub enum SlashKind {
    Help,
    Thinking,
    Model,
    Settings,
    Persona,
    Stat,
    Quit,
    Sandbox,
    Session,
    Tree,    // new
}
```

Add to `registry()`:

```rust
SlashCommand {
    name: "/tree",
    help: "Navigate session history: select a point to continue from, optionally summarize pruned branches",
    kind: SlashKind::Tree,
},
```

Also register `/tree` in the `from_buffer` parsing. Currently the parser in
`from_buffer` (line 414) matches `"session" | "sessions"` and then checks for
`"tree"` as a sub-command of session. We can either:

- **Option A:** Make `/tree` a standalone top-level slash command (parallel to
  `/session`). Clearer UX, simpler routing.
- **Option B:** Keep it as `/session <name> tree`. More discoverable via the
  `/session` action menu.

**Recommendation:** Option A (standalone `/tree` operating on the current session).
The `/session` action menu can gain a `tree` action that sets the editor to `/tree`.

Update the test `registry_has_nine_commands` → `registry_has_ten_commands`.

### 2.2 Pass tree snapshot in ReplCtx

**File:** `src/repl.rs`

The `ReplCtx` struct needs access to the session tree so the overlay can render
it without holding a lock or doing async work in the hot loop. Follow the pattern
established by `sessions` and `branch_messages`: pre-build the tree snapshot
in `run_interactive` and pass it in.

```rust
pub struct ReplCtx {
    // existing fields...
    pub sessions: Vec<SessionMeta>,
    pub current_session_id: String,
    pub branch_messages: Vec<(String, String)>,

    // new:
    /// Pre-built session entry tree (empty if no session).
    pub tree: Vec<session::SessionTreeNode>,
    /// Current leaf entry ID (for highlighting).
    pub leaf_id: Option<String>,
}
```

Build it in `run_interactive` (`src/agent.rs`, alongside `build_session_ctx`):

Do NOT use `blocking_lock`/`now_or_never` here. `build_session_ctx` is already
`async` and runs before `read_input`, so build the tree and leaf id cleanly there
and thread them through `ReplCtx`:

```rust
let (tree, leaf_id) = match session {
    Some(s) => (s.get_tree().await, s.get_leaf_id().await),
    None => (Vec::new(), None),
};
```

(`get_tree()` already sorts the current leaf's branch first, so `leaf_id` is only
needed for highlighting.) Extend `build_session_ctx` to also return tree data:

```rust
// Current signature (agent.rs:1103) returns a 2-tuple:
//   (Vec<SessionMeta>, Vec<(String, String)>)
// Extend it (note the type is `SessionMeta`, not `ReplSessionMeta`):
async fn build_session_ctx(
    session: &Option<Session>,
    repo: &Option<JsonlSessionRepo>,
) -> (
    Vec<SessionMeta>,
    Vec<(String, String)>,
    Vec<SessionTreeNode>,  // new
    Option<String>,         // new: leaf_id
) { ... }
```

### 2.3 Add ActiveOverlay::Tree variant

**File:** `src/repl.rs`

```rust
pub(crate) enum ActiveOverlay {
    // existing variants...
    Tree {
        rows: Vec<String>,    // pre-rendered display lines
        targets: Vec<String>, // entry IDs (1:1 with rows, for Enter resolution)
        active: Option<usize>,
        _filter: String,
    },
}
```

The `rows` vector stores fully rendered lines (with indentation, tree connectors,
and entry labels). Each visual line maps 1:1 to a `targets` entry ID; the Enter
handler uses `targets[sel]` to determine where to navigate.

### 2.4 from_buffer for `/tree`

**File:** `src/repl.rs`, in `ActiveOverlay::from_buffer()`

Add a match arm (in the same ladder that handles `"session"`):

```rust
"tree" | "t" => {
    let filter = rest.trim().to_string();
    let mut rows = Vec::new();
    let mut targets = Vec::new();
    let mut active = None;
    Repl::tree_overlay_entries(&ctx.tree, ctx.leaf_id.as_deref(), &filter,
        &mut rows, &mut targets, &mut active);
    ActiveOverlay::Tree { rows, targets, active, _filter: filter }
}
```

Also register `"tree"` in the "bare word" inline check around line 404 so
the overlay opens immediately after typing `/tree` (before a space):

```rust
after,
"" | "model" | "thinking" | ... | "tree"
```

### 2.5 Render tree rows

**File:** `src/repl.rs`, new method

```rust
fn tree_overlay_entries(
    tree: &[SessionTreeNode],
    leaf_id: Option<&str>,
    filter: &str,
    rows: &mut Vec<String>,
    targets: &mut Vec<String>,
    active: &mut Option<usize>,
) {
    if tree.is_empty() {
        rows.push("(no session)".into());
        return;
    }

    // Flatten tree into display rows with indentation + connectors.
    // Build a set of entry IDs on the active path (root → leaf).
    let active_path: std::collections::HashSet<&str> = /* ... */;

    let mut index = 0usize; // tracks position in the flat list
    flatten_tree_rows(tree, 0, false, true, leaf_id, &active_path, rows, targets, &mut index);

    // Apply text filter
    if !filter.is_empty() {
        let f = filter.to_lowercase();
        let filtered: Vec<usize> = rows.iter().enumerate()
            .filter(|(_, r)| r.to_lowercase().contains(&f))
            .map(|(i, _)| i)
            .collect();
        // Rebuild rows/targets from filtered indices
        // ...
    }

    // Highlight current leaf
    if let Some(lid) = leaf_id {
        *active = targets.iter().position(|t| t == lid);
    }
}
```

The `flatten_tree_rows` recursive helper:

```rust
fn flatten_tree_rows(
    nodes: &[SessionTreeNode],
    indent: usize,
    show_connector: bool,
    is_last: bool,
    leaf_id: Option<&str>,
    active_path: &HashSet<&str>,
    rows: &mut Vec<String>,
    targets: &mut Vec<String>,
    index: &mut usize,
) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last_child = i == nodes.len() - 1;

        // Build the tree-drawing prefix
        let connector = if show_connector {
            if is_last_child { "└─ " } else { "├─ " }
        } else {
            "  "
        };
        let prefix = format!("{:indent$}{}", "", connector, indent = indent);

        // Build the entry label
        let label = entry_display_label(&node.entry, node.label.as_deref());

        // Highlight active path
        let is_active = active_path.contains(node.entry.id.as_str());
        let is_current = leaf_id == Some(node.entry.id.as_str());
        let marker = if is_current { " ◉" } else { "" };
        let row = if is_active {
            format!("{}{}{}", prefix, label, marker)
        } else {
            format!("\x1b[2m{}{}{}\x1b[0m", prefix, label, marker)
        };

        rows.push(row);
        targets.push(node.entry.id.clone());

        // Recurse into children
        flatten_tree_rows(
            &node.children,
            indent + 2,
            true,
            is_last_child,
            leaf_id,
            active_path,
            rows,
            targets,
            index,
        );
    }
}
```

Entry display labels:

```rust
fn entry_display_label(entry: &SessionEntry, label: Option<&str>) -> String {
    match &entry.content {
        SessionEntryContent::Message { message } => {
            match message.get("role").and_then(|r| r.as_str()) {
                Some("user") => {
                    let text = message.pointer("/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(user)");
                    // Single-line sanitised, use table fit for truncation
                    sanitise_single_line(text)
                }
                Some("assistant") => {
                    let text = message.pointer("/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        "assistant".into()
                    } else {
                        sanitise_single_line(text)
                    }
                }
                Some("toolResult") => "(tool result)".into(),
                _ => "(unknown)".into(),
            }
        }
        SessionEntryContent::ThinkingLevelChange { thinking_level } => {
            format!("thinking: {}", thinking_level)
        }
        SessionEntryContent::ModelChange { provider, model_id } => {
            format!("model: {}/{}", provider, model_id)
        }
        // Required for exhaustiveness (see critique #1). Treat as meta / skip.
        SessionEntryContent::ActiveToolsChange { .. } => return String::new(),
        SessionEntryContent::BranchSummary { summary, .. } => {
            let s = sanitise_single_line(summary);
            format!("📝 {}", if s.is_empty() { "(branch summary)" } else { &s })
        }
        SessionEntryContent::Compaction { summary, .. } => {
            let s = sanitise_single_line(summary);
            format!("📦 {}", if s.is_empty() { "(compacted)" } else { &s })
        }
        SessionEntryContent::Label { target_id, label } => {
            let l = label.as_deref().unwrap_or("(label)");
            format!("🏷️ {} → {}", l, target_id)
        }
        // Meta entries: skip (don't add to rows)
        SessionEntryContent::SessionInfo { .. } => return String::new(),
        SessionEntryContent::Leaf { .. } => return String::new(),
        SessionEntryContent::Custom { custom_type, .. } => {
            format!("custom: {}", custom_type)
        }
        SessionEntryContent::CustomMessage { custom_type, .. } => {
            format!("custom msg: {}", custom_type)
        }
    }
}
```

**Important:** The function must skip `SessionInfo` and `Leaf` entries completely
(they are meta entries, not part of the conversation timeline). The `flatten_tree_rows`
helper should skip nodes whose entry display returns empty.

### 2.6 Handle Enter on Tree overlay

**File:** `src/repl.rs`, in `handle_enter`

```rust
ActiveOverlay::Tree { targets, .. } => {
    if let Some(target_id) = targets.get(sel) {
        // Look up the target entry to determine behaviour:
        // - User message: set editor text to the user's prompt, submit
        // - Other entry: just navigate there without editor text
        let line = format!("/tree {}", target_id);
        Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
        return OverlayAction::Action(InputAction::Submit(line));
    }
    OverlayAction::Continue
}
```

The `/tree <target_id>` command is then parsed in the main loop (Phase 3).

### 2.7 Handle Tab on Tree overlay

Tab copies the selected entry's text (following pi's `app.message.copy` pattern):

```rust
ActiveOverlay::Tree { targets, .. } => {
    if let Some(target_id) = targets.get(sel) {
        // Extract text and put in editor as a draft
        // editor.set(&extracted_text);
    }
}
```

### 2.8 Handle Esc on Tree overlay

Esc dismisses the tree overlay and returns to bare prompt (like Esc on `/session`):

```rust
// In handle_esc:
ActiveOverlay::Tree { .. } => {
    editor.set("");
    path_completions.clear();
}
```

### 2.9 overlay_lines display

**File:** `src/repl.rs`, in `Repl::overlay_lines`

The Tree overlay uses the same rendering path as other overlays. The `rows`
vector from `ActiveOverlay::Tree` is passed through `Self::overlay_lines`
with an appropriate title ("tree"). The `active` index points to the current
leaf.

## Phase 3 — Command handler

### 3.1 InputAction variant

**File:** `src/repl.rs` (or `src/agent.rs`)

Add a new `InputAction` variant:

```rust
pub(crate) enum InputAction {
    // existing...
    Submit(String),
    NavigateTree {
        target_id: String,
        // Decided at parse time, not typed by user
    },
}
```

### 3.2 Handle `/tree` in the main loop

**File:** `src/agent.rs`, in `run_interactive`

In the `InputAction::Submit(input)` handler, before the existing `/session`
check, add:

```rust
if input.starts_with("/tree") {
    let rest = input["/tree".len()..].trim();
    let (target_id, summarize) = parse_tree_args(rest);
    if target_id.is_empty() {
        // No target: just open the overlay (already done via from_buffer)
        continue;
    }

    if let Some(ref s) = session {
        let old_leaf_id = s.get_leaf_id().await;

        if old_leaf_id.as_deref() == Some(target_id.as_str()) {
            continue; // no-op
        }

        // Collect the abandoned branch (old_leaf .. common ancestor) and the
        // common-ancestor id, if summarizing. Note `collect_...` returns the
        // ancestor id so we don't invent an undefined `common_ancestor_id`.
        if summarize {
            if let Some(old_leaf) = old_leaf_id.as_deref() {
                let (abandoned, common_ancestor_id) = {
                    let index = s.index().lock().await;
                    collect_entries_for_branch_summary(&index, old_leaf, &target_id)
                }; // drop the index lock before any await

                if !abandoned.is_empty() {
                    if let Some(agent) = agent.as_mut() {
                        if let Ok(summary) = summarize_branch(agent, &abandoned).await {
                            let _ = s.append_branch_summary(&common_ancestor_id, &summary).await;
                        }
                    }
                }
            }
        }

        // Move the leaf pointer.
        let _ = s.move_leaf(Some(target_id.clone())).await;

        // Rebuild context so the next turn sees the new position. Apply ALL of
        // messages, thinking level, and model (critique #3) — build_context()
        // returns all three and they can differ per branch.
        if let Ok(ctx) = s.build_context().await {
            if let Some(agent) = agent.as_mut() {
                agent.state.messages = ctx.messages;
                if let Some(level) = ctx.thinking_level {
                    agent.set_thinking_level(&level); // or equivalent
                }
                if let Some(model) = ctx.model {
                    agent.set_model(&model); // or equivalent
                }
            }
        }

        config.output.display_plain(&format!(
            "/tree {} ({})",
            if summarize { "navigated + summarized" } else { "navigated" },
            target_id
        ));
    }
    continue;
}
```

### 3.3 Parse tree arguments

```rust
fn parse_tree_args(rest: &str) -> (String, bool) {
    let mut summarize = false;
    let mut target = String::new();
    for part in rest.split_whitespace() {
        match part {
            "--summarize" | "-s" => summarize = true,
            id => target = id.to_string(),
        }
    }
    (target, summarize)
}
```

### 3.4 Collect entries for branch summary

**File:** `src/agent.rs` (or new module)

```rust
/// Walk from `old_leaf` to root and from `new_target` to root, find the common
/// ancestor, and return `(abandoned_entries, common_ancestor_id)` where
/// `abandoned_entries` are the entries after the common ancestor on the old
/// branch. `get_path_to_root` is a method on `SessionIndex` (not `Session`) and
/// returns entries in root→leaf order.
fn collect_entries_for_branch_summary(
    index: &SessionIndex,
    old_leaf: &str,
    new_target: &str,
) -> (Vec<SessionEntry>, String) {
    let old_path = index.get_path_to_root(old_leaf);
    let new_path = index.get_path_to_root(new_target);

    // Number of shared entries from the root.
    let common_len = old_path.iter()
        .zip(new_path.iter())
        .take_while(|(a, b)| a.id == b.id)
        .count();

    // The common ancestor is the last shared entry (index common_len - 1).
    let common_ancestor_id = common_len
        .checked_sub(1)
        .and_then(|i| old_path.get(i))
        .map(|e| e.id.clone())
        .unwrap_or_default();

    let abandoned = old_path.into_iter().skip(common_len).collect();
    (abandoned, common_ancestor_id)
}
```

## Phase 4 — Summarization (deferred)

This phase depends on having the `agent` reference available in the tree handler.

### 4.1 Summary prompt

Build a system prompt that asks the LLM to summarize the abandoned branch:

```rust
fn build_summary_prompt(entries: &[SessionEntry]) -> String {
    let mut text = String::from("Summarize the following conversation branch concisely:\n\n");
    for entry in entries {
        if let SessionEntryContent::Message { message } = &entry.content {
            if let Some(role) = message.get("role").and_then(|r| r.as_str()) {
                let content = message.get("content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                text.push_str(&format!("[{}] {}\n", role, content));
            }
        }
    }
    text
}
```

### 4.2 LLM call

Use `agent.completion()` or `agent.stream()` to generate the summary. The
summarization should use a small token budget (e.g., `max_tokens: 500`).

```rust
let summary = agent.completion(
    "system", "You are a conversation summarizer. Keep summaries concise.",
    "user", &build_summary_prompt(&entries_to_summarize),
    Some(500), // max_tokens
).await?;
```

### 4.3 Write compaction entry

After summarization, optionally write a `Compaction` entry that marks older
entries as replaced by the summary:

```rust
let _ = s.append_compaction(
    &summary,
    &first_kept_entry_id, // first entry after compaction boundary
    tokens_before,         // token count of removed entries
).await;
```

The existing `apply_transforms` on `Session` will then filter out entries
before the compaction boundary and replace them with a synthetic user message
carrying the summary.

## Phase 5 — Integrate with `/session` action menu

**File:** `src/repl.rs`, in `session_action_overlay_entries`

Add a "tree" action (index 7) to the session action menu:

```rust
opts.push("  tree".into());
```

And in the Enter handler for `SessionAction`:

```rust
7 => ("tree", true),
```

Then in `handle_session_command`, add a `"tree"` action that sets the editor
to `/tree` so the tree overlay opens for the selected session:

```rust
"tree" => {
    config.output.display_plain(&format!("/session {} tree", name));
    SessionCommandResult::Handled
}
```

The next loop iteration picks up `/tree` from `from_buffer`. For switching to
a *different* session's tree, the tree snapshot in `ReplCtx` would need to be
rebuilt — simplest is to require that the tree overlay only works for the
current session (the `/session action menu's "tree" option for a non-current
session would just switch to that session first).

## Implementation order

```
Phase 1.1  SessionTreeNode type + build_tree()       │ types.rs
Phase 1.2  Entry constructors                         │ types.rs
Phase 1.3  Navigation methods on Session              │ session.rs
Phase 2.1  Register /tree slash command               │ slash.rs
Phase 2.2  Pre-build tree snapshot in ReplCtx         │ agent.rs, repl.rs
Phase 2.3  ActiveOverlay::Tree variant                │ repl.rs
Phase 2.4  from_buffer for /tree                      │ repl.rs
Phase 2.5  Tree row rendering                         │ repl.rs
Phase 2.6  Enter handler                              │ repl.rs
Phase 2.7  Tab handler                                │ repl.rs
Phase 2.8  Esc handler                                │ repl.rs
Phase 3.1  InputAction variant                        │ repl.rs
Phase 3.2  Handle /tree in main loop                  │ agent.rs
Phase 3.3  Parse tree args                            │ agent.rs
Phase 5    Session action menu integration             │ repl.rs, agent.rs
Phase 4    Summarization (deferred)                   │ agent.rs
```

## Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Standalone vs sub-command | Standalone `/tree` | Clearer UX, simpler routing |
| Tree data flow | Pre-built snapshot in `ReplCtx` | Avoids async locking in hot overlay loop |
| Enter action | Emits `/tree <id>` command string | Follows existing `/session` pattern |
| Active branch sorting | Current leaf's branch first | Same as pi, makes active path easy to find |
| Meta entries | Skipped (`SessionInfo`, `Leaf`) | Not part of conversation timeline |
| Filter | Text match on display labels | Same as other overlays |
| Summarization | LLM call with collected branch entries | Reuses agent's model, no extra config |
| Compaction | `Compaction` entry + `apply_transforms` | Already partially supported |

## Files modified

| File | Change |
|---|---|
| `src/session/types.rs` | `SessionTreeNode`, `build_tree()`, `subtree_contains()`, entry constructors |
| `src/session/session.rs` | `get_entry()`, `get_leaf_id()`, `get_tree()`, `move_leaf()`, `append_branch_summary()`, `append_compaction()`, `append_label()` |
| `src/slash.rs` | `SlashKind::Tree`, `/tree` command, test updates |
| `src/repl.rs` | `ReplCtx` tree fields, `ActiveOverlay::Tree`, `from_buffer`, `tree_overlay_entries()`, `flatten_tree_rows()`, `entry_display_label()`, Enter/Tab/Esc handlers |
| `src/agent.rs` | `ReplCtx` construction with tree, `/tree` command handler, `parse_tree_args()`, `collect_entries_for_branch_summary()`, summarization |
