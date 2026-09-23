//! v2.5 `SlashDotNode` implementations for `/tree`.
//!
//! `TreeListNode`: dynamic rows (one per visible entry from the session
//! tree). When drilled into an entry, `header()` renders the full message
//! body and the static action children (switch, compact) are shown.
//!
//! `TreeSwitchNode`: leaf node that moves the session leaf pointer to
//! the selected entry via `AsyncOp::TreeSwitch`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::slash_menu_bridge::{AsyncOp, CompactMode, MenuShared};
use crate::session::{SessionEntry, SessionEntryContent, SessionTreeNode};
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};
use crate::slashdot_menu::provider::ColumnDef;

// ---------------------------------------------------------------------------
// Entry display helpers (ported from repl.rs entry_display_label)
// ---------------------------------------------------------------------------

/// Extract a display label from a `SessionEntry`. Returns empty string
/// for meta entries that should be invisible in the listing.
fn entry_display_label(entry: &SessionEntry, label: Option<&str>) -> String {
    match &entry.content {
        SessionEntryContent::Message { message } => {
            match message.get("role").and_then(|r| r.as_str()) {
                Some("user") => {
                    let text = message
                        .pointer("/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(user)");
                    sanitise_single_line(text)
                }
                Some("assistant") => {
                    let text = message
                        .get("content")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| {
                                    match item.get("type").and_then(|t| t.as_str()) {
                                        Some("text") => {
                                            item.get("text").and_then(|t| t.as_str())
                                        }
                                        _ => None,
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    if text.is_empty() {
                        String::new()
                    } else {
                        sanitise_single_line(&text)
                    }
                }
                Some("toolResult") => String::new(),
                _ => String::new(),
            }
        }
        SessionEntryContent::Compaction {
            first_kept_entry_id,
            ..
        } => {
            // Show the compaction marker with the kept entry's text if
            // available. For the listing we use a simplified label since the
            // full tree is not available here — the label map is built in
            // `build_rows` below.
            if let Some(kept) = label {
                format!("[COMPACT] {}", sanitise_single_line(kept))
            } else {
                format!("[COMPACT] before {}", first_kept_entry_id)
            }
        }
        SessionEntryContent::ThinkingLevelChange { thinking_level } => {
            format!("[THINK] {}", thinking_level)
        }
        SessionEntryContent::ModelChange { provider, model_id } => {
            format!("[MODEL] {}/{}", provider, model_id)
        }
        SessionEntryContent::BranchSummary { summary, .. } => {
            let s = sanitise_single_line(summary);
            if s.is_empty() {
                "[SUMMARY] (branch summary)".into()
            } else {
                format!("[SUMMARY] {}", s)
            }
        }
        SessionEntryContent::Label { target_id, label } => {
            let l = label.as_deref().unwrap_or("(label)");
            format!("[LABEL] {} -> {}", l, target_id)
        }
        // Meta entries: invisible.
        SessionEntryContent::SessionInfo { .. } | SessionEntryContent::Leaf { .. } => {
            String::new()
        }
        SessionEntryContent::ActiveToolsChange { .. } => String::new(),
        SessionEntryContent::Custom { custom_type, .. } => {
            format!("[custom] {}", custom_type)
        }
        SessionEntryContent::CustomMessage { custom_type, .. } => {
            format!("[custom msg] {}", custom_type)
        }
    }
}

/// Replace newlines and control characters with spaces; collapse runs.
fn sanitise_single_line(text: &str) -> String {
    text.replace('\n', " ")
        .replace('\r', "")
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Active-path computation
// ---------------------------------------------------------------------------

/// Walk from `leaf_id` up through the tree to collect the set of entry
/// IDs on the active branch.
fn collect_active_path(
    nodes: &[SessionTreeNode],
    leaf_id: &str,
    out: &mut HashSet<String>,
) {
    // Iterative DFS: walk the tree to find leaf_id, then record the
    // path from root to leaf.  Uses an explicit stack so deep linear
    // sessions (1000s of entries) don't overflow the call stack.
    let mut stack: Vec<(&[SessionTreeNode], Vec<String>)> = Vec::new();
    stack.push((nodes, Vec::new()));
    while let Some((current, mut path)) = stack.pop() {
        for node in current {
            if node.entry.id == leaf_id {
                path.push(node.entry.id.clone());
                for id in &path {
                    out.insert(id.clone());
                }
                return;
            }
            let mut child_path = path.clone();
            child_path.push(node.entry.id.clone());
            stack.push((&node.children, child_path));
        }
    }
}

// ---------------------------------------------------------------------------
// Tree flattening
// ---------------------------------------------------------------------------

/// True if this entry is a visible assistant message (for indent rule).
fn is_assistant(entry: &SessionEntry) -> bool {
    matches!(&entry.content,
        SessionEntryContent::Message { message }
        if message.get("role").and_then(|r| r.as_str()) == Some("assistant"))
}

/// Find a node by entry ID. Iterative DFS to avoid stack overflow.
fn find_entry<'a>(
    nodes: &'a [SessionTreeNode],
    id: &str,
) -> Option<&'a SessionTreeNode> {
    let mut stack: Vec<&'a [SessionTreeNode]> = Vec::new();
    stack.push(nodes);
    while let Some(current) = stack.pop() {
        for n in current {
            if n.entry.id == id {
                return Some(n);
            }
            stack.push(&n.children);
        }
    }
    None
}

/// One row in the flattened tree.
struct FlatRow {
    indent: usize,
    label: String,
    entry_id: String,
    on_active_path: bool,
    is_current: bool,
    branch_depth: usize,
    /// Full text for header() — may be multi-line.
    full_text: String,
    /// Role for header metadata.
    role: String,
    /// Timestamp for header metadata.
    timestamp: String,
}

/// Build a label map: entry ID → display label (for compaction rows that
/// reference ancestors).
fn build_label_map(nodes: &[SessionTreeNode]) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    // Iterative DFS.
    let mut stack: Vec<&[SessionTreeNode]> = Vec::new();
    stack.push(nodes);
    while let Some(current) = stack.pop() {
        for n in current {
            let t = entry_display_label(&n.entry, n.label.as_deref());
            if !t.is_empty() {
                labels.insert(n.entry.id.clone(), t);
            }
            stack.push(&n.children);
        }
    }
    labels
}

/// Extract full text (multi-line) from an entry for the header.
fn entry_full_text(entry: &SessionEntry) -> String {
    match &entry.content {
        SessionEntryContent::Message { message } => {
            match message.get("role").and_then(|r| r.as_str()) {
                Some("user") => {
                    message
                        .pointer("/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(user)")
                        .to_string()
                }
                Some("assistant") => {
                    message
                        .get("content")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| {
                                    match item.get("type").and_then(|t| t.as_str()) {
                                        Some("text") => {
                                            item.get("text").and_then(|t| t.as_str())
                                        }
                                        _ => None,
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                }
                _ => String::new(),
            }
        }
        SessionEntryContent::Compaction {
            summary, ..
        } => {
            summary.clone()
        }
        SessionEntryContent::ThinkingLevelChange { thinking_level } => {
            format!("thinking: {}", thinking_level)
        }
        SessionEntryContent::ModelChange { provider, model_id } => {
            format!("model: {}/{}", provider, model_id)
        }
        SessionEntryContent::BranchSummary { summary, .. } => summary.clone(),
        _ => String::new(),
    }
}

/// Extract the role string for metadata.
fn entry_role(entry: &SessionEntry) -> String {
    match &entry.content {
        SessionEntryContent::Message { message } => {
            message
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown")
                .to_string()
        }
        SessionEntryContent::Compaction { .. } => "compaction".into(),
        SessionEntryContent::ThinkingLevelChange { .. } => "thinking".into(),
        SessionEntryContent::ModelChange { .. } => "model".into(),
        SessionEntryContent::BranchSummary { .. } => "summary".into(),
        SessionEntryContent::Label { .. } => "label".into(),
        _ => "meta".into(),
    }
}

/// Recursively flatten the tree into rows.
fn flatten_turns(
    nodes: &[SessionTreeNode],
    indent: usize,
    branch_depth: usize,
    leaf_id: Option<&str>,
    active_path: &HashSet<String>,
    labels: &HashMap<String, String>,
    out: &mut Vec<FlatRow>,
) {
    // Iterative DFS to avoid stack overflow on deep linear sessions.
    // Push root nodes in reverse so the first sibling is processed first.
    let mut stack: Vec<(&SessionTreeNode, usize, usize)> = Vec::new();
    for node in nodes.iter().rev() {
        stack.push((node, indent, branch_depth));
    }

    while let Some((node, cur_indent, cur_bd)) = stack.pop() {
        let label =
            entry_display_label_ext(&node.entry, node.label.as_deref(), labels);

        // Children indent/branch_depth: visible multi-child nodes get +4
        // indent and +1 branch_depth; everything else keeps current level.
        let is_branch = node.children.len() > 1;
        let child_indent = if is_branch { cur_indent + 4 } else { cur_indent };
        let child_bd = if is_branch { cur_bd + 1 } else { cur_bd };

        if label.is_empty() {
            // Meta entry: skip visual row but descend into children.
            for child in node.children.iter().rev() {
                stack.push((child, cur_indent, child_bd));
            }
            continue;
        }

        // Indent rule: assistant → +2, everything else → base indent.
        let row_indent = if is_assistant(&node.entry) { cur_indent + 2 } else { cur_indent };

        let is_active = active_path.contains(&node.entry.id);
        let is_current = leaf_id == Some(node.entry.id.as_str());
        let full_text = entry_full_text(&node.entry);
        let role = entry_role(&node.entry);
        let timestamp = node.entry.timestamp.clone();

        out.push(FlatRow {
            indent: row_indent,
            label,
            entry_id: node.entry.id.clone(),
            on_active_path: is_active,
            is_current,
            branch_depth: cur_bd,
            full_text,
            role,
            timestamp,
        });

        // Push children in reverse so first child is processed first.
        for child in node.children.iter().rev() {
            stack.push((child, child_indent, child_bd));
        }
    }
}

/// Variant of `entry_display_label` that resolves compaction entries via the
/// whole-tree label map.
fn entry_display_label_ext(
    entry: &SessionEntry,
    label: Option<&str>,
    labels: &HashMap<String, String>,
) -> String {
    match &entry.content {
        SessionEntryContent::Compaction {
            first_kept_entry_id,
            ..
        } => {
            let kept = labels
                .get(first_kept_entry_id)
                .map(|t| sanitise_single_line(t))
                .or_else(|| label.map(|l| sanitise_single_line(l)));
            match kept {
                Some(t) if !t.is_empty() => format!("[COMPACT] {}", t),
                _ => format!("[COMPACT] before {}", first_kept_entry_id),
            }
        }
        _ => entry_display_label(entry, label),
    }
}

// ---------------------------------------------------------------------------
// Column definitions
// ---------------------------------------------------------------------------

/// Column definitions for the `/tree` listing.
pub fn tree_listing_columns() -> Vec<ColumnDef> {
    // Empty label suppresses the table header row (no "DESCRIPTION" line).
    vec![ColumnDef::new("description", "").min(20).prio(3).wrap(false)]
}

/// Field metadata for the tree listing.
pub fn tree_fields() -> Vec<FieldSpec> {
    vec![FieldSpec::new("role", false)]
}

/// Wrap `text` to `width` columns, prefixing the first line with `prefix`.
/// Subsequent continuation lines are indented to align with the text start.
fn wrap_with_prefix(prefix: &str, text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![format!("{}(empty)", prefix)];
    }
    let first_width = width.saturating_sub(prefix.len());
    let cont_width = width.saturating_sub(2); // 2-space indent for continuations
    let mut result = Vec::new();
    let mut first = true;

    for line in text.split('\n') {
        let eff_width = if first { first_width } else { cont_width };
        let indent = if first { String::new() } else { "  ".into() };
        if line.len() <= eff_width {
            result.push(format!("{}{}", if first { prefix } else { &indent }, line));
        } else {
            let mut remaining = line;
            let mut first_chunk = true;
            while remaining.len() > eff_width {
                let pfx = if first && first_chunk { prefix } else { &indent };
                if let Some(break_at) = remaining[..eff_width].rfind(' ') {
                    result.push(format!("{}{}", pfx, &remaining[..break_at]));
                    remaining = &remaining[break_at + 1..];
                } else {
                    result.push(format!("{}{}", pfx, &remaining[..eff_width]));
                    remaining = &remaining[eff_width..];
                }
                first_chunk = false;
            }
            if !remaining.is_empty() {
                let pfx = if first && first_chunk { prefix } else { &indent };
                result.push(format!("{}{}", pfx, remaining));
            }
        }
        first = false;
    }
    result
}

// ---------------------------------------------------------------------------
// TreeListNode
// ---------------------------------------------------------------------------

/// Dynamic listing node for `/tree`. `data()` returns one row per visible
/// session entry from the raw tree. When drilled into (an entry selected),
/// `header()` renders the full message body and the static action children
/// are shown.
pub struct TreeListNode {
    pub shared: Rc<MenuShared>,
    /// Cached data response — reused via Rc::ptr_eq when the tree hasn't
    /// changed.
    cached_data: Option<Rc<DataResponse>>,
    /// The Rc we last built rows from (for freshness check).
    cached_tree_rc: Option<Rc<Vec<SessionTreeNode>>>,
    cached_leaf_id: Option<String>,
}

impl TreeListNode {
    pub fn new(shared: Rc<MenuShared>) -> Self {
        Self {
            shared,
            cached_data: None,
            cached_tree_rc: None,
            cached_leaf_id: None,
        }
    }

    /// Resolve the entry ID from `path_args.ids` — the last element in
    /// the listing context (path `["tree", "<entry-id>"]`).
    fn drilled_entry_id(path_args: &PathArgs) -> Option<&str> {
        path_args.ids.last().map(|s| s.as_str())
    }
}

impl SlashDotNode for TreeListNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        use crate::slashdot_menu::value::Value;

        // Read the shared tree (Rc — O(1) clone, no deep copy).
        let tree_rc = self.shared.raw_tree.borrow().clone();
        let leaf = self.shared.leaf_id.borrow().clone();

        // Freshness check: same Rc pointer + same leaf = cached data is valid.
        let same_tree = self
            .cached_tree_rc
            .as_ref()
            .map_or(false, |old| Rc::ptr_eq(old, &tree_rc));
        let same_leaf = self.cached_leaf_id == leaf;

        if same_tree && same_leaf {
            if let Some(ref cached) = self.cached_data {
                return Ok(cached.clone());
            }
        }

        // Build the rows from the raw tree.
        let active_path: HashSet<String> = leaf
            .as_deref()
            .map(|lid| {
                let mut p = HashSet::new();
                collect_active_path(&tree_rc, lid, &mut p);
                p
            })
            .unwrap_or_default();

        let labels = build_label_map(&tree_rc);
        let mut flat = Vec::new();
        flatten_turns(
            &tree_rc,
            0,
            0,
            leaf.as_deref(),
            &active_path,
            &labels,
            &mut flat,
        );

        let rows: Vec<Row> = flat
            .iter()
            .map(|r| {
                let padded_label = format!("{:indent$}{}", "", r.label, indent = r.indent);
                let mut row = Row::new(r.entry_id.clone())
                    .desc(padded_label)
                    .field("role", Value::Str(r.role.clone()));
                if r.on_active_path {
                    row = row.field("on_path", Value::Str("1".into()));
                }
                if r.is_current {
                    row = row.field("leaf", Value::Str("1".into()));
                }
                row
            })
            .collect();

        let active_ids = leaf
            .as_ref()
            .filter(|l| find_entry(&tree_rc, l).is_some())
            .cloned()
            .map(|l| vec![l])
            .unwrap_or_default();

        let response = Rc::new(DataResponse {
            rows,
            active_ids,
        });

        // Cache for next call.
        self.cached_data = Some(response.clone());
        self.cached_tree_rc = Some(tree_rc);
        self.cached_leaf_id = leaf;

        Ok(response)
    }

    fn columns(&self) -> Vec<ColumnDef> {
        // Empty = default id + description rendering for child rows
        // (switch, compact, etc.).
        Vec::new()
    }

    fn fields(&self) -> Vec<FieldSpec> {
        tree_fields()
    }

    fn header(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> Vec<String> {
        let Some(entry_id) = Self::drilled_entry_id(path_args) else {
            return vec!["Select a tree entry, then choose an action.".to_string()];
        };

        // Call self.data() to ensure the cache is populated (cheap Rc::ptr_eq
        // return if the tree hasn't changed).
        let _ = self.data(0, path_args);

        // Find the entry in the raw tree for full details.
        let tree_borrow = self.shared.raw_tree.borrow();
        let tree: &[SessionTreeNode] = &tree_borrow;

        let Some(node) = find_entry(tree, entry_id) else {
            return vec![format!("Entry not found: {}", entry_id)];
        };

        let entry = &node.entry;
        let role = entry_role(entry);
        let ts = &entry.timestamp;
        let tw = render_ctx.term_width.max(1).saturating_sub(2);
        let lines: Vec<String> = match &entry.content {
            SessionEntryContent::Message { message: _ } => {
                let full = entry_full_text(entry);
                let prefix = format!("[{} {}] ", role.to_uppercase(), ts);
                wrap_with_prefix(&prefix, &full, tw)
            }
            SessionEntryContent::Compaction { summary, .. } => {
                let prefix = format!("[COMPACT {}] ", ts);
                wrap_with_prefix(&prefix, summary, tw)
            }
            SessionEntryContent::ModelChange { provider, model_id } => {
                vec![format!("[MODEL] {}/{} ({})", provider, model_id, ts)]
            }
            SessionEntryContent::ThinkingLevelChange { thinking_level } => {
                vec![format!("[THINK] {} ({})", thinking_level, ts)]
            }
            SessionEntryContent::BranchSummary { summary, .. } => {
                let prefix = format!("[SUMMARY {}] ", ts);
                wrap_with_prefix(&prefix, summary, tw)
            }
            _ => vec![format!("[{}] {}", role, ts)],
        };
        lines
    }
}

// ---------------------------------------------------------------------------
// TreeSwitchNode
// ---------------------------------------------------------------------------

/// Leaf node: moves the session leaf pointer to the selected entry.
pub struct TreeSwitchNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for TreeSwitchNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("switch")
                .desc("continue from this entry")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Move the session leaf to this entry.".to_string(),
            "The next prompt continues from this point.".to_string(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        // In the action node's own context, the path is
        // ["tree", "<entry-id>", "switch"]. The entry ID is the
        // second-to-last element (matching SessionResumeNode's pattern).
        let target_id = {
            let ids = &path_args.ids;
            if ids.len() >= 2 {
                ids.get(ids.len() - 2).cloned().unwrap_or_default()
            } else {
                String::new()
            }
        };
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::TreeSwitch { target_id });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// CompactNode (root-level /compact shortcut)
// ---------------------------------------------------------------------------

/// Root-level `/compact` leaf node. On drill, looks up the current
/// session leaf (message ID) and redirects to `/tree <id> compact `
/// so the compact modes are listed.
pub struct CompactNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for CompactNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("compact").desc("summarize conversation up to the current point")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Compact: summarize entries before the current point.".into(),
            "Redirects to /tree <id> compact.".into(),
        ]
    }

    fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
        // Look up the current session leaf ID and redirect.
        let leaf_id = self.shared.leaf_id.borrow().clone()?;
        Some(format!("tree {} compact ", leaf_id))
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        // Enter (without drill) should also redirect — treat as drill.
        match self.shared.leaf_id.borrow().clone() {
            Some(leaf_id) => SubmitResult::NewPath(format!("tree {} compact ", leaf_id)),
            None => SubmitResult::Done(Some("No active session leaf to compact.".to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeCompactManualNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with manual (touched-files) mode.
/// `prepare()` opens $EDITOR with a template; `submit()` dispatches
/// `AsyncOp::TreeCompact { mode: Manual }`.
pub struct TreeCompactManualNode {
    pub shared: Rc<MenuShared>,
    edited: std::cell::RefCell<Option<String>>,
}

impl TreeCompactManualNode {
    pub fn new(shared: Rc<MenuShared>) -> Self {
        Self { shared, edited: std::cell::RefCell::new(None) }
    }
}

impl SlashDotNode for TreeCompactManualNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("manual").desc("user-editable file list")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Manual compaction: scan touched files, open in editor.".into(),
            "Save and close to confirm; discard to cancel.".into(),
        ]
    }

    // Default prepare() delegates to submit() (single-phase).
    // Future: override prepare() to open $EDITOR for draft editing,
    // then return Prepared → confirm → submit. Requires session access
    // to compute drafts, which the node doesn't have.

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let draft = self.edited.borrow_mut().take().unwrap_or_default();
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::TreeCompact { target_id, mode: CompactMode::Manual, payload: draft });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// TreeCompactTextNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with text-only mode.
/// `prepare()` opens $EDITOR with extracted conversation text; `submit()`
/// dispatches `AsyncOp::TreeCompact { mode: TextOnly }`.
pub struct TreeCompactTextNode {
    pub shared: Rc<MenuShared>,
    edited: std::cell::RefCell<Option<String>>,
}

impl TreeCompactTextNode {
    pub fn new(shared: Rc<MenuShared>) -> Self {
        Self { shared, edited: std::cell::RefCell::new(None) }
    }
}

impl SlashDotNode for TreeCompactTextNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("text-only").desc("extracted conversation text")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Text-only compaction: extract user prompts + assistant text.".into(),
            "No reasoning, tool calls, or tool output. Edit in editor.".into(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let draft = self.edited.borrow_mut().take().unwrap_or_default();
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::TreeCompact { target_id, mode: CompactMode::TextOnly, payload: draft });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// TreeCompactPromptNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with LLM prompt mode.
/// `submit()` reads the custom prompt from `path_args.suffix` and
/// dispatches `AsyncOp::TreeCompact { mode: Prompt, payload: suffix }`.
/// The LLM call happens in the agent loop handler.
pub struct TreeCompactPromptNode {
    pub shared: Rc<MenuShared>,
}

impl TreeCompactPromptNode {
    pub fn new(shared: Rc<MenuShared>) -> Self {
        Self { shared }
    }
}

impl SlashDotNode for TreeCompactPromptNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("prompt").desc("LLM-generated summary")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let prompt = &path_args.suffix;
        if prompt.is_empty() {
            vec![
                "LLM compaction: generate summary via agent.".into(),
                "Type a custom instruction after 'prompt', or use the default.".into(),
            ]
        } else {
            vec![
                "LLM compaction with custom prompt:".into(),
                format!("  {}", prompt),
            ]
        }
    }

    fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Pre-fill the default compaction prompt.
        // ids are ["tree", "<id>", "compact", "prompt"].
        Some(path_args.prompt_with("Summarize the conversation up to this point."))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let prompt = path_args.suffix.trim().to_string();
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::TreeCompact { target_id, mode: CompactMode::Prompt, payload: prompt });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

/// Extract the entry ID from a compact node's path.
/// Path format: `["tree", "<entry-id>", "compact", "<mode>"]`.
/// Target ID is at index 1 (second element).
fn extract_target_id(path_args: &PathArgs) -> String {
    let ids = &path_args.ids;
    if ids.len() >= 2 {
        ids.get(1).cloned().unwrap_or_default()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build the `/tree` subtree as a `SlashDotMenuNode`.
pub fn build_tree_node(
    shared: &Rc<MenuShared>,
) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::menu::SlashDotMenuNode;
    use crate::slashdot_menu::node_impl::{StaticListingNode, StaticRow};

    let mut listing: SlashDotMenuNode = StaticListingNode::new(&[StaticRow::new(
        "tree",
        "navigate session history — select a point to continue from",
    )])
    .with_columns(tree_listing_columns())
    .with_fields(tree_fields())
    .into();

    let mut tree_node: SlashDotMenuNode =
        TreeListNode::new(shared.clone()).into();
    tree_node.add_child(TreeSwitchNode {
        shared: shared.clone(),
    }
    .into());

    // Compact subtree: a listing node with three mode children.
    let mut compact_node: SlashDotMenuNode = StaticListingNode::new(&[StaticRow::new(
        "compact",
        "summarize entries before this point",
    )])
    .into();
    compact_node.add_child(TreeCompactManualNode::new(shared.clone()).into());
    compact_node.add_child(TreeCompactTextNode::new(shared.clone()).into());
    compact_node.add_child(TreeCompactPromptNode::new(shared.clone()).into());

    tree_node.add_child(compact_node);

    listing.add_child(tree_node);
    listing
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_menu_bridge::MenuShared;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{
        MenuResult, PathArgs, StaticListingNode,
    };
    use crate::slashdot_menu::HostKey;
    use crate::session::{SessionEntry, SessionEntryContent, SessionTreeNode};

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_thinking_level: String::new(),
            show_reasoning: String::new(),
            reasoning_in_context: false,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            path: None,
        }
    }

    fn make_shared() -> Rc<MenuShared> {
        Rc::new(MenuShared::new(test_settings(), None))
    }

    fn make_entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(|s| s.into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn node(entry: SessionEntry, children: Vec<SessionTreeNode>) -> SessionTreeNode {
        SessionTreeNode {
            entry,
            children,
            label: None,
        }
    }

    /// Build a shared with a pre-populated tree.
    fn shared_with_tree(tree: Vec<SessionTreeNode>) -> Rc<MenuShared> {
        let shared = make_shared();
        *shared.raw_tree.borrow_mut() = Rc::new(tree);
        shared
    }

    fn build_tree_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_tree_node(shared));
        SlashDotMenu::from_root_node(root)
    }

    // --- Basic listing ---

    #[test]
    fn empty_tree_shows_empty_listing() {
        let shared = make_shared();
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert!(plan.rows.is_empty(), "empty tree → empty listing");
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn tree_listing_shows_entries() {
        let tree = vec![
            node(
                make_entry("u1", None, "user", "hello world"),
                vec![node(
                    make_entry("a1", Some("u1"), "assistant", "hi there"),
                    vec![],
                )],
            ),
        ];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 2, "should have user + assistant rows");
                assert!(plan.rows[0].0.contains("hello world"),
                    "first row should contain user text: {}", plan.rows[0].0);
                assert!(plan.rows[1].0.contains("hi there"),
                    "second row should contain assistant text: {}", plan.rows[1].0);
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // --- Caching ---

    #[test]
    fn tree_listing_caching_skips_rebuild() {
        let tree = vec![node(
            make_entry("u1", None, "user", "test"),
            vec![],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared.clone());

        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };

        let r1 = tree_node.data(1, &path_args).unwrap();
        let r2 = tree_node.data(1, &path_args).unwrap();
        assert!(Rc::ptr_eq(&r1, &r2),
            "same tree + same leaf should return same Rc");
    }

    #[test]
    fn tree_listing_caching_rebuilds_on_change() {
        let tree = vec![node(
            make_entry("u1", None, "user", "test"),
            vec![],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared.clone());

        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };

        let r1 = tree_node.data(1, &path_args).unwrap();

        // Change the tree.
        let new_tree = vec![
            node(make_entry("u1", None, "user", "test"), vec![]),
            node(make_entry("u2", None, "user", "new entry"), vec![]),
        ];
        *shared.raw_tree.borrow_mut() = Rc::new(new_tree);

        let r2 = tree_node.data(1, &path_args).unwrap();
        assert!(!Rc::ptr_eq(&r1, &r2),
            "changed tree should return new Rc");
        assert_eq!(r2.rows.len(), 2, "new tree has two entries");
    }

    // --- Header ---

    #[test]
    fn tree_entry_header_shows_message_body() {
        let tree = vec![node(
            make_entry("u1", None, "user", "hello\nworld"),
            vec![],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared.clone());

        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into()],
            suffix: String::new(),
            args: Default::default(),
        };

        // Trigger data() first to populate the cache.
        let _ = tree_node.data(0, &path_args);

        let header = tree_node.header(&path_args, &RenderContext { term_width: 80 });
        let joined = header.join("\n");
        assert!(joined.contains("hello"), "header should contain message text: {}", joined);
        assert!(joined.contains("world"), "header should contain full text: {}", joined);
        assert!(joined.contains("USER"), "header should contain role: {}", joined);
    }

    // --- Switch ---

    #[test]
    fn tree_switch_submit_sets_async_op() {
        let shared = make_shared();
        let mut tree_node = TreeSwitchNode { shared: shared.clone() };
        // Action node path: ["tree", "<entry-id>", "switch"]
        let path_args = PathArgs {
            ids: vec!["tree".into(), "some-entry".into(), "switch".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let _ = tree_node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeSwitch { target_id }) => {
                assert_eq!(target_id, "some-entry");
            }
            other => panic!("expected TreeSwitch, got {:?}", other),
        }
    }

    // --- Full traversal ---

    #[test]
    fn tree_switch_full_traversal() {
        let tree = vec![
            node(
                make_entry("u1", None, "user", "first message"),
                vec![node(
                    make_entry("a1", Some("u1"), "assistant", "response"),
                    vec![],
                )],
            ),
        ];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();

        // 1. Listing shows entries.
        let listing = handle.eval_prompt("tree ", HostKey::None, 80);
        match &listing {
            MenuResult::Continue(plan, _) => {
                assert!(!plan.rows.is_empty(), "tree listing should have rows");
            }
            other => panic!("expected Continue at listing, got {:?}", std::mem::discriminant(other)),
        }

        // 2. Drill into u1.
        let r = handle.eval_prompt("tree u1 ", HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into entry");

        // 3. Drill into switch.
        let r = handle.eval_prompt("tree u1 switch ", HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into switch");

        // 4. Enter → submit.
        let _ = handle.eval_prompt("tree u1 switch ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeSwitch { target_id }) => {
                assert_eq!(target_id, "u1", "switch op must carry the entry ID");
            }
            other => panic!("expected TreeSwitch, got {:?}", other),
        }
    }

    // --- Label rendering ---

    #[test]
    fn tree_model_change_shows_prefix() {
        let tree = vec![SessionTreeNode {
            entry: SessionEntry {
                id: "mc1".into(),
                parent_id: None,
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
            children: vec![],
            label: None,
        }];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 1);
                assert!(plan.rows[0].0.contains("[MODEL]"),
                    "model change should have [MODEL] prefix: {}", plan.rows[0].0);
                assert!(plan.rows[0].0.contains("openai/gpt-4o"),
                    "model change should show provider/model: {}", plan.rows[0].0);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn tree_empty_entry_hidden() {
        // A tool-call-only assistant segment should produce no row.
        let tree = vec![SessionTreeNode {
            entry: SessionEntry {
                id: "tc1".into(),
                parent_id: None,
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "tool_use", "name": "bash", "input": {}}]
                    }),
                },
            },
            children: vec![],
            label: None,
        }];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert!(plan.rows.is_empty(),
                    "tool-call-only assistant should be hidden");
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn active_entry_marked() {
        // Leaf is a root-level sibling — active_ids must contain it.
        let tree = vec![
            node(make_entry("u1", None, "user", "first"), vec![]),
            node(make_entry("u2", None, "user", "second"), vec![]),
        ];
        let shared = shared_with_tree(tree);
        *shared.leaf_id.borrow_mut() = Some("u2".into());
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();
        assert_eq!(resp.active_ids, vec!["u2".to_string()],
            "active_ids should contain the leaf entry");
    }

    #[test]
    fn active_entry_marked_deep() {
        // Leaf is deep in the tree (not root-level) — active_ids must
        // still find it. This was broken when active_ids only checked
        // root-level entries via tree_rc.iter().
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply"),
                vec![node(
                    make_entry("u2", Some("a1"), "user", "second"),
                    vec![],
                )],
            )],
        )];
        let shared = shared_with_tree(tree);
        *shared.leaf_id.borrow_mut() = Some("u2".into());
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();
        assert_eq!(resp.active_ids, vec!["u2".to_string()],
            "active_ids should find leaf deep in tree");
    }

    // --- Compact: mode submit tests ---

    #[test]
    fn compact_manual_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = TreeCompactManualNode::new(shared.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "manual".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = node.submit(1, &path_args);
        assert!(matches!(result, SubmitResult::Done(None)));
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeCompact { target_id, mode, payload }) => {
                assert_eq!(target_id, "u1");
                assert_eq!(*mode, CompactMode::Manual);
                assert!(payload.is_empty(), "manual draft is empty by default");
            }
            other => panic!("expected TreeCompact, got {:?}", other),
        }
    }

    #[test]
    fn compact_text_only_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = TreeCompactTextNode::new(shared.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "text-only".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = node.submit(1, &path_args);
        assert!(matches!(result, SubmitResult::Done(None)));
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeCompact { target_id, mode, payload }) => {
                assert_eq!(target_id, "u1");
                assert_eq!(*mode, CompactMode::TextOnly);
                assert!(payload.is_empty());
            }
            other => panic!("expected TreeCompact, got {:?}", other),
        }
    }

    #[test]
    fn compact_prompt_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = TreeCompactPromptNode::new(shared.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "prompt".into()],
            suffix: "summarize the key decisions".into(),
            args: Default::default(),
        };
        let result = node.submit(1, &path_args);
        assert!(matches!(result, SubmitResult::Done(None)));
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeCompact { target_id, mode, payload }) => {
                assert_eq!(target_id, "u1");
                assert_eq!(*mode, CompactMode::Prompt);
                assert_eq!(payload, "summarize the key decisions");
            }
            other => panic!("expected TreeCompact, got {:?}", other),
        }
    }

    #[test]
    fn compact_prompt_empty_suffix_uses_default() {
        let shared = make_shared();
        let mut node = TreeCompactPromptNode::new(shared.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "prompt".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = node.submit(1, &path_args);
        assert!(matches!(result, SubmitResult::Done(None)));
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeCompact { payload, mode, .. }) => {
                assert_eq!(*mode, CompactMode::Prompt);
                assert!(payload.is_empty(), "empty suffix = empty payload");
            }
            other => panic!("expected TreeCompact, got {:?}", other),
        }
    }

    // --- Compact: full traversal ---

    #[test]
    fn compact_full_traversal_manual() {
        let tree = vec![
            node(
                make_entry("u1", None, "user", "first message"),
                vec![node(
                    make_entry("a1", Some("u1"), "assistant", "response"),
                    vec![],
                )],
            ),
        ];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();

        // 1. Listing.
        let listing = handle.eval_prompt("tree ", HostKey::None, 80);
        assert!(matches!(listing, MenuResult::Continue(_, _)));

        // 2. Drill into u1.
        let r = handle.eval_prompt("tree u1 ", HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 3. Drill into compact.
        let r = handle.eval_prompt("tree u1 compact ", HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 4. Drill into manual.
        let r = handle.eval_prompt("tree u1 compact manual ", HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 5. Enter → submit.
        let _ = handle.eval_prompt("tree u1 compact manual ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::TreeCompact { target_id, mode, .. }) => {
                assert_eq!(target_id, "u1");
                assert_eq!(*mode, CompactMode::Manual);
            }
            other => panic!("expected TreeCompact, got {:?}", other),
        }
    }

    // --- Compact: header tests ---

    #[test]
    fn compact_manual_header_shows_description() {
        let shared = make_shared();
        let mut node = TreeCompactManualNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let header = node.header(&path_args, &RenderContext { term_width: 80 });
        assert!(!header.is_empty());
        assert!(header[0].contains("Manual"), "header mentions manual: {}", header[0]);
    }

    #[test]
    fn compact_prompt_header_shows_prompt_when_present() {
        let shared = make_shared();
        let mut node = TreeCompactPromptNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: "my custom prompt".into(),
            args: Default::default(),
        };
        let header = node.header(&path_args, &RenderContext { term_width: 80 });
        assert!(header.iter().any(|l| l.contains("my custom prompt")),
            "header should contain the prompt text: {:?}", header);
    }

    #[test]
    fn compact_prompt_header_shows_default_when_empty() {
        let shared = make_shared();
        let mut node = TreeCompactPromptNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let header = node.header(&path_args, &RenderContext { term_width: 80 });
        assert!(header.iter().any(|l| l.contains("default")),
            "header should mention default: {:?}", header);
    }

    // --- Indentation ---

    #[test]
    fn linear_session_indent_user_assistant() {
        // Linear tree: u1 → a1 → u2 → a2 → u3 → a3
        // Expected: user=0, assistant=2, user=0, assistant=2, ...
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply 1"),
                vec![node(
                    make_entry("u2", Some("a1"), "user", "second"),
                    vec![node(
                        make_entry("a2", Some("u2"), "assistant", "reply 2"),
                        vec![node(
                            make_entry("u3", Some("a2"), "user", "third"),
                            vec![node(
                                make_entry("a3", Some("u3"), "assistant", "reply 3"),
                                vec![],
                            )],
                        )],
                    )],
                )],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();
        assert_eq!(resp.rows.len(), 6, "should have 6 visible entries");

        let mut flat = Vec::new();
        for row in resp.rows.iter() {
            let desc = row.description.clone().unwrap_or_default();
            let indent = desc.len() - desc.trim_start().len();
            flat.push((row.id.clone(), indent));
        }

        // u1=0, a1=2, u2=0, a2=2, u3=0, a3=2
        let expected = vec![
            ("u1", 0),
            ("a1", 2),
            ("u2", 0),
            ("a2", 2),
            ("u3", 0),
            ("a3", 2),
        ];
        assert_eq!(flat.len(), expected.len());
        for (i, (exp_id, exp_indent)) in expected.iter().enumerate() {
            assert_eq!(flat[i].0, *exp_id, "row {} id", i);
            assert_eq!(flat[i].1, *exp_indent,
                "row {} ({}) indent: expected {}, got {}",
                i, exp_id, exp_indent, flat[i].1);
        }
    }

    #[test]
    fn linear_session_indent_render_plan() {
        // Verify the full render plan preserves indent spacing.
        let tree = vec![node(
            make_entry("u1", None, "user", "hello"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "world"),
                vec![],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 2);
                let u1_text = &plan.rows[0].0;
                let a1_text = &plan.rows[1].0;
                let u1_indent = u1_text.len() - u1_text.trim_start().len();
                let a1_indent = a1_text.len() - a1_text.trim_start().len();
                assert_eq!(u1_indent, 0, "user row should have no indent: {:?}", u1_text);
                assert_eq!(a1_indent, 2, "assistant row should have 2-space indent: {:?}", a1_text);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn linear_session_indent_render_plan_long() {
        // 6-entry linear tree through the full render pipeline.
        let tree = vec![node(
            make_entry("u1", None, "user", "first message"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "first reply"),
                vec![node(
                    make_entry("u2", Some("a1"), "user", "second message"),
                    vec![node(
                        make_entry("a2", Some("u2"), "assistant", "second reply"),
                        vec![node(
                            make_entry("u3", Some("a2"), "user", "third message"),
                            vec![node(
                                make_entry("a3", Some("u3"), "assistant", "third reply"),
                                vec![],
                            )],
                        )],
                    )],
                )],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 6);
                let expected = vec![
                    ("u1", 0),
                    ("a1", 2),
                    ("u2", 0),
                    ("a2", 2),
                    ("u3", 0),
                    ("a3", 2),
                ];
                for (i, (exp_id, exp_indent)) in expected.iter().enumerate() {
                    let text = &plan.rows[i].0;
                    let indent = text.len() - text.trim_start().len();
                    assert_eq!(indent, *exp_indent,
                        "row {} ({}) indent wrong in rendered output: {:?}",
                        i, exp_id, text);
                }
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn indent_survives_table_render() {
        // Check that leading-space indent survives through the Table
        // rendering pipeline used by build_render_plan.
        use crate::slashdot_menu::table::{Table, Align};

        let cells = vec![
            "hello",           // indent 0
            "  world",         // indent 2
            "    model",       // indent 4
        ];
        let mut t = Table::new(&[""])
            .with_aligns(&[Align::Left])
            .with_min_widths(&[20])
            .with_priorities(&[3])
            .set_gap(2);
        for c in &cells {
            t.add_row(&[c.to_string()]);
        }
        let rendered = t.render(80);
        assert_eq!(rendered.len(), 3);
        for (i, (cell, line)) in cells.iter().zip(rendered.iter()).enumerate() {
            let indent = line.len() - line.trim_start().len();
            let expected_indent = cell.len() - cell.trim_start().len();
            assert_eq!(indent, expected_indent,
                "row {} cell={:?} rendered={:?}: indent lost in table render",
                i, cell, line);
        }
    }

    #[test]
    fn debug_cell_text_from_build_render_plan() {
        // Dump the actual cell text that build_render_plan feeds to the table.
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply1"),
                vec![node(
                    make_entry("u2", Some("a1"), "user", "second"),
                    vec![node(
                        make_entry("a2", Some("u2"), "assistant", "reply2"),
                        vec![],
                    )],
                )],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec!["tree".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();
        eprintln!("=== DataResponse rows ===");
        for (i, row) in resp.rows.iter().enumerate() {
            let desc = row.description.clone().unwrap_or_default();
            let indent = desc.len() - desc.trim_start().len();
            eprintln!("  row {}: id={:?} indent={} desc={:?}", i, row.id, indent, desc);
        }
    }

    #[test]
    fn tree_with_model_changes_indent_render() {
        // Realistic tree: user/assistant turns with model changes at the end.
        // u1 → a1 → u2 → a2 → mc1 → mc2, mc3, mc4
        let mc2 = SessionTreeNode {
            entry: SessionEntry {
                id: "mc2".into(),
                parent_id: Some("mc1".into()),
                timestamp: "2025-01-01T00:00:01.000Z".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
            children: vec![],
            label: None,
        };
        let mc3 = SessionTreeNode {
            entry: SessionEntry {
                id: "mc3".into(),
                parent_id: Some("mc1".into()),
                timestamp: "2025-01-01T00:00:02.000Z".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3".into(),
                },
            },
            children: vec![],
            label: None,
        };
        let mc4 = SessionTreeNode {
            entry: SessionEntry {
                id: "mc4".into(),
                parent_id: Some("mc1".into()),
                timestamp: "2025-01-01T00:00:03.000Z".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
            children: vec![],
            label: None,
        };
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply1"),
                vec![node(
                    make_entry("u2", Some("a1"), "user", "second"),
                    vec![node(
                        make_entry("a2", Some("u2"), "assistant", "reply2"),
                        vec![SessionTreeNode {
                            entry: SessionEntry {
                                id: "mc1".into(),
                                parent_id: Some("a2".into()),
                                timestamp: "2025-01-01T00:00:00.000Z".into(),
                                content: SessionEntryContent::ModelChange {
                                    provider: "openai".into(),
                                    model_id: "gpt-4o".into(),
                                },
                            },
                            children: vec![mc2, mc3, mc4],
                            label: None,
                        }],
                    )],
                )],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut menu = build_tree_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("tree ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                // Print for debugging
                for (i, row) in plan.rows.iter().enumerate() {
                    let indent = row.0.len() - row.0.trim_start().len();
                    eprintln!("  row {}: indent={} {:?}", i, indent, row.0);
                }
                assert_eq!(plan.rows.len(), 8);
                let expected = vec![
                    ("u1", 0),
                    ("a1", 2),
                    ("u2", 0),
                    ("a2", 2),
                    ("mc1", 0),  // model change: Boundary at base
                    ("mc2", 4),  // branch: mc1 has 3 children → indent+4
                    ("mc3", 4),
                    ("mc4", 4),
                ];
                for (i, (exp_id, exp_indent)) in expected.iter().enumerate() {
                    let text = &plan.rows[i].0;
                    let indent = text.len() - text.trim_start().len();
                    assert_eq!(indent, *exp_indent,
                        "row {} ({}) indent wrong: {:?}", i, exp_id, text);
                }
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn linear_session_with_model_change_indent() {
        // u1 → a1 → mc1 (model change) → u2 → a2
        // mc1 is a Boundary: indent=0, in_turn=false
        // u2 is User: indent=0, in_turn=true
        // a2 is Assistant: indent=2 (in_turn)
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply 1"),
                vec![SessionTreeNode {
                    entry: SessionEntry {
                        id: "mc1".into(),
                        parent_id: Some("a1".into()),
                        timestamp: "2025-01-01T00:00:00.000Z".into(),
                        content: SessionEntryContent::ModelChange {
                            provider: "openai".into(),
                            model_id: "gpt-4o".into(),
                        },
                    },
                    children: vec![node(
                        make_entry("u2", Some("mc1"), "user", "second"),
                        vec![node(
                            make_entry("a2", Some("u2"), "assistant", "reply 2"),
                            vec![],
                        )],
                    )],
                    label: None,
                }],
            )],
        )];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();

        let mut flat = Vec::new();
        for row in resp.rows.iter() {
            let desc = row.description.clone().unwrap_or_default();
            let indent = desc.len() - desc.trim_start().len();
            flat.push((row.id.clone(), indent));
        }

        let expected = vec![
            ("u1", 0),   // user
            ("a1", 2),   // assistant (+2)
            ("mc1", 0),  // model change: back to base
            ("u2", 0),   // user: base indent
            ("a2", 2),   // assistant (+2)
        ];
        assert_eq!(flat.len(), expected.len(),
            "rows: {:?}", flat.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>());
        for (i, (exp_id, exp_indent)) in expected.iter().enumerate() {
            assert_eq!(flat[i].0, *exp_id, "row {} id", i);
            assert_eq!(flat[i].1, *exp_indent,
                "row {} ({}) indent: expected {}, got {}",
                i, exp_id, exp_indent, flat[i].1);
        }
    }

    #[test]
    fn assistant_after_tool_result_chain_gets_indent() {
        // Real-world pattern: user → [assistant(tool_call) → toolResult] × N → assistant(text)
        // The invisible entries must NOT break the turn grouping so the
        // visible assistant gets +2 indent.
        let tree = vec![SessionTreeNode {
            entry: make_entry("u1", None, "user", "Please critique"),
            children: vec![
                // First assistant segment: tool-call-only (invisible)
                SessionTreeNode {
                    entry: SessionEntry {
                        id: "a_tool1".into(),
                        parent_id: Some("u1".into()),
                        timestamp: "2025-01-01T00:00:00.000Z".into(),
                        content: SessionEntryContent::Message {
                            message: serde_json::json!({
                                "role": "assistant",
                                "content": [{"type": "tool_use", "name": "bash", "input": {}}]
                            }),
                        },
                    },
                    children: vec![
                        // Tool result (invisible)
                        SessionTreeNode {
                            entry: SessionEntry {
                                id: "tr1".into(),
                                parent_id: Some("a_tool1".into()),
                                timestamp: "2025-01-01T00:00:00.000Z".into(),
                                content: SessionEntryContent::Message {
                                    message: serde_json::json!({
                                        "role": "toolResult",
                                        "content": [{"type": "text", "text": "result1"}]
                                    }),
                                },
                            },
                            children: vec![
                                // Second assistant segment: tool-call-only (invisible)
                                SessionTreeNode {
                                    entry: SessionEntry {
                                        id: "a_tool2".into(),
                                        parent_id: Some("tr1".into()),
                                        timestamp: "2025-01-01T00:00:00.000Z".into(),
                                        content: SessionEntryContent::Message {
                                            message: serde_json::json!({
                                                "role": "assistant",
                                                "content": [{"type": "tool_use", "name": "bash", "input": {}}]
                                            }),
                                        },
                                    },
                                    children: vec![
                                        // Tool result (invisible)
                                        SessionTreeNode {
                                            entry: SessionEntry {
                                                id: "tr2".into(),
                                                parent_id: Some("a_tool2".into()),
                                                timestamp: "2025-01-01T00:00:00.000Z".into(),
                                                content: SessionEntryContent::Message {
                                                    message: serde_json::json!({
                                                        "role": "toolResult",
                                                        "content": [{"type": "text", "text": "result2"}]
                                                    }),
                                                },
                                            },
                                            children: vec![
                                                // Final assistant text (VISIBLE)
                                                SessionTreeNode {
                                                    entry: make_entry(
                                                        "a_text",
                                                        Some("tr2"),
                                                        "assistant",
                                                        "Now I have a comprehensive understanding",
                                                    ),
                                                    children: vec![],
                                                    label: None,
                                                },
                                            ],
                                            label: None,
                                        },
                                    ],
                                    label: None,
                                },
                            ],
                            label: None,
                        },
                    ],
                    label: None,
                },
            ],
            label: None,
        }];
        let shared = shared_with_tree(tree);
        let mut tree_node = TreeListNode::new(shared);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = tree_node.data(0, &path_args).unwrap();
        // Only 2 visible rows: user + assistant
        assert_eq!(resp.rows.len(), 2, "should have 2 visible entries");
        let u1_desc = resp.rows[0].description.clone().unwrap_or_default();
        let a_desc = resp.rows[1].description.clone().unwrap_or_default();
        let u1_indent = u1_desc.len() - u1_desc.trim_start().len();
        let a_indent = a_desc.len() - a_desc.trim_start().len();
        assert_eq!(u1_indent, 0, "user should have indent 0: {:?}", u1_desc);
        assert_eq!(a_indent, 2, "assistant should have indent 2 after tool chain: {:?}", a_desc);
    }

    // --- drill_rewrite tests ---

    #[test]
    fn compact_node_drill_rewrite_redirects_to_tree() {
        let shared = make_shared();
        *shared.leaf_id.borrow_mut() = Some("msg-42".into());
        let mut node = CompactNode { shared: shared.clone() };
        let path = vec!["compact".to_string()];
        let result = node.drill_rewrite(&PathArgs::new(path, " "));
        assert_eq!(result, Some("tree msg-42 compact ".to_string()));
    }

    #[test]
    fn compact_node_no_leaf_returns_none() {
        let shared = make_shared();
        // leaf_id is None (no session).
        let mut node = CompactNode { shared: shared.clone() };
        let path = vec!["compact".to_string()];
        let result = node.drill_rewrite(&PathArgs::new(path, " "));
        assert_eq!(result, None);
    }

    #[test]
    fn compact_prompt_drill_rewrite_fills_default() {
        let shared = make_shared();
        let mut node = TreeCompactPromptNode::new(shared.clone());
        let path = vec!["tree".to_string(), "msg-1".to_string(),
                        "compact".to_string(), "prompt".to_string()];
        let result = node.drill_rewrite(&PathArgs::new(path, " "));
        assert_eq!(result, Some(
            "tree msg-1 compact prompt Summarize the conversation up to this point.".to_string()
        ));
    }
}
