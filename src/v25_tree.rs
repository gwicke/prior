//! v2.5 `SlashDotNode` implementations for `/tree`.
//!
//! `TreeListNode`: dynamic rows (one per visible entry from the session
//! tree). When drilled into an entry, `header()` renders the full message
//! body and the static action children (switch, compact) are shown.
//!
//! `TreeSwitchNode`: leaf node that moves the session leaf pointer to
//! the selected entry via the `tree_switch` control target.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::app_state::{CompactMode, AppState};
use crate::session::{SessionEntry, SessionEntryContent, SessionTreeNode};
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};
use crate::slashdot_menu::fields::ColumnDef;
use crate::slashdot_menu::value::Value;

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

/// Strip carriage returns and control characters, **keeping `\n` and
/// `\t`** — row descriptions carry full multi-line text (design: "Full
/// message text as the description"). The listing's wrap-disabled table
/// flattens `\n` to spaces and ellipsizes for display; fuzzy rank and
/// `header()` see the raw multi-line string.
fn sanitise_multiline(text: &str) -> String {
    text.replace('\r', "")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// Cap a compaction "from" context line at `max` characters (char-safe),
/// appending an ellipsis when cut — enough to identify the line in the
/// listing without flooding it.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
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

/// One row in the flattened tree. The row *is* the data: listing
/// display, fuzzy rank, filters, and `header()` all read it (design:
/// "header() → entry detail from the same cached row data").
struct FlatRow {
    indent: usize,
    /// Full row text (markers + body, newlines kept). The listing's
    /// wrap-disabled table flattens it to one line for display.
    desc: String,
    entry_id: String,
    on_active_path: bool,
    is_current: bool,
    /// Role for listing metadata (`entry_role`).
    role: String,
    /// Entry timestamp (ISO 8601) — shown by `header()`, filterable.
    timestamp: String,
    /// Terminal endpoint of a branch: no visible entries below.
    is_terminal: bool,
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

/// Ids of visible entries with no visible entries anywhere below them —
/// the branch endpoints shown in the listing. Invisible meta entries
/// (tool results, session info) don't count as descendants, so a
/// visible message above a dead-end tool result is the endpoint you
/// actually see.
///
/// Iterative post-order (deep linear sessions overflow recursion):
/// `(node, expanded)` state stack; `has_vis` = "some visible entry
/// exists in this node's subtree".
fn visible_terminals(
    nodes: &[SessionTreeNode],
    labels: &HashMap<String, String>,
) -> HashSet<String> {
    let visible = |n: &SessionTreeNode| -> bool {
        !entry_display_label_ext(&n.entry, n.label.as_deref(), labels).is_empty()
    };
    let mut terminals = HashSet::new();
    let mut has_vis: HashMap<String, bool> = HashMap::new();
    let mut stack: Vec<(&SessionTreeNode, bool)> = Vec::new();
    for n in nodes.iter().rev() {
        stack.push((n, false));
    }
    while let Some((node, expanded)) = stack.pop() {
        if !expanded {
            stack.push((node, true));
            for child in node.children.iter().rev() {
                stack.push((child, false));
            }
            continue;
        }
        let mut has = false;
        for child in &node.children {
            if visible(child)
                || has_vis.get(&child.entry.id).copied().unwrap_or(false)
            {
                has = true;
                break;
            }
        }
        if visible(node) && !has {
            terminals.insert(node.entry.id.clone());
        }
        has_vis.insert(node.entry.id.clone(), has);
    }
    terminals
}

/// Recursively flatten the tree into rows.
fn flatten_turns(
    nodes: &[SessionTreeNode],
    indent: usize,
    leaf_id: Option<&str>,
    active_path: &HashSet<String>,
    labels: &HashMap<String, String>,
    terminals: &HashSet<String>,
    out: &mut Vec<FlatRow>,
) {
    // Iterative DFS to avoid stack overflow on deep linear sessions.
    // Push root nodes in reverse so the first sibling is processed first.
    let mut stack: Vec<(&SessionTreeNode, usize)> = Vec::new();
    for node in nodes.iter().rev() {
        stack.push((node, indent));
    }

    while let Some((node, cur_indent)) = stack.pop() {
        let label =
            entry_display_label_ext(&node.entry, node.label.as_deref(), labels);

        // Children indent: visible multi-child nodes get +4 indent;
        // everything else keeps current level.
        let is_branch = node.children.len() > 1;
        let child_indent = if is_branch { cur_indent + 4 } else { cur_indent };

        if label.is_empty() {
            // Meta entry: skip visual row but descend into children.
            for child in node.children.iter().rev() {
                stack.push((child, cur_indent));
            }
            continue;
        }

        // Indent rule: assistant → +2, everything else → base indent.
        let row_indent = if is_assistant(&node.entry) { cur_indent + 2 } else { cur_indent };

        let is_active = active_path.contains(&node.entry.id);
        let is_current = leaf_id == Some(node.entry.id.as_str());
        let role = entry_role(&node.entry);
        let desc = entry_row_desc(&node.entry, node.label.as_deref(), labels);
        let timestamp = node.entry.timestamp.clone();
        let is_terminal = terminals.contains(&node.entry.id);

        out.push(FlatRow {
            indent: row_indent,
            desc,
            entry_id: node.entry.id.clone(),
            on_active_path: is_active,
            is_current,
            role,
            timestamp,
            is_terminal,
        });

        // Push children in reverse so first child is processed first.
        for child in node.children.iter().rev() {
            stack.push((child, child_indent));
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

/// Full row description for a *visible* entry — what fuzzy rank and
/// `header()` see (design: "Full message text as the description").
/// Differs from the single-line listing label in two ways: messages
/// keep their newlines, and compactions carry both the truncated
/// "from" context (identifying line) and the summary text.
fn entry_row_desc(
    entry: &SessionEntry,
    label: Option<&str>,
    labels: &HashMap<String, String>,
) -> String {
    match &entry.content {
        SessionEntryContent::Message { .. } => sanitise_multiline(&entry_full_text(entry)),
        SessionEntryContent::Compaction {
            first_kept_entry_id,
            summary,
            ..
        } => {
            let kept = labels
                .get(first_kept_entry_id)
                .map(|t| sanitise_single_line(t))
                .or_else(|| label.map(|l| sanitise_single_line(l)));
            let head = match kept {
                Some(t) if !t.is_empty() => format!("from {}", truncate_chars(&t, 300)),
                _ => format!("before {}", first_kept_entry_id),
            };
            format!("[COMPACT] {}\n{}", head, sanitise_multiline(summary))
        }
        SessionEntryContent::BranchSummary { summary, .. } => {
            let s = sanitise_multiline(summary);
            if s.is_empty() {
                "[SUMMARY] (branch summary)".into()
            } else {
                format!("[SUMMARY] {}", s)
            }
        }
        // Model/thinking/label/custom keep their marker labels; meta
        // entries never reach here (invisible, no row).
        _ => entry_display_label_ext(entry, label, labels),
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
    let mut tags = FieldSpec::new("tags", false);
    tags.description = Some("leaf = branch endpoint".to_string());
    vec![
        FieldSpec::new("role", false),
        FieldSpec::new("timestamp", false),
        tags,
    ]
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
    pub app_state: Arc<AppState>,
    /// Cached data response — reused via Rc::ptr_eq when the tree hasn't
    /// changed.
    cached_data: Option<Rc<DataResponse>>,
    /// The Arc we last built rows from (for freshness check).
    cached_tree_rc: Option<Arc<Vec<SessionTreeNode>>>,
    cached_leaf_id: Option<String>,
}

impl TreeListNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
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

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeListNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        // Read the app_state tree (Arc — O(1) clone, no deep copy).
        let tree_rc = self.app_state.raw_tree();
        let leaf = self.app_state.leaf_id();

        // Freshness check: same Arc pointer + same leaf = cached data is valid.
        let same_tree = self
            .cached_tree_rc
            .as_ref()
            .map_or(false, |old| Arc::ptr_eq(old, &tree_rc));
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
        let terminals = visible_terminals(&tree_rc, &labels);
        let mut flat = Vec::new();
        flatten_turns(
            &tree_rc,
            0,
            leaf.as_deref(),
            &active_path,
            &labels,
            &terminals,
            &mut flat,
        );

        let rows: Vec<Row> = flat
            .iter()
            .map(|r| {
                let padded = format!("{:indent$}{}", "", r.desc, indent = r.indent);
                let mut row = Row::new(r.entry_id.clone())
                    .desc(padded)
                    .field("role", Value::Str(r.role.clone()))
                    .field("timestamp", Value::Str(r.timestamp.clone()));
                if r.on_active_path {
                    row = row.field("on_path", Value::Str("1".into()));
                }
                if r.is_current {
                    row = row.field("leaf", Value::Str("1".into()));
                }
                // Optional tag: present only on branch endpoints, so
                // `.tags:leaf` (missing field ⇒ row dropped) filters to
                // terminal nodes.
                if r.is_terminal {
                    row = row.field("tags", Value::Str("leaf".into()));
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

    async fn header(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> Vec<String> {
        let Some(entry_id) = Self::drilled_entry_id(path_args) else {
            return vec!["Select a tree entry, then choose an action.".to_string()];
        };

        // Refresh the cache (cheap Rc::ptr_eq when unchanged) and render
        // from the row — the same data the listing and fuzzy search use
        // (design: "header() → entry detail from the same cached row data").
        let Ok(resp) = self.data(0, path_args).await else {
            return vec![format!("Entry not found: {}", entry_id)];
        };
        let Some(row) = resp.rows.iter().find(|r| r.id == entry_id) else {
            return vec![format!("Entry not found: {}", entry_id)];
        };
        let field_str = |name: &str| -> String {
            match row.fields.iter().find(|(k, _)| k == name) {
                Some((_, Value::Str(s))) => s.clone(),
                _ => String::new(),
            }
        };
        let role = field_str("role");
        let ts = field_str("timestamp");
        // Full text incl. markers; drop the tree indent for display.
        let body = row.description.as_deref().unwrap_or_default().trim_start();
        let tw = render_ctx.term_width.max(1).saturating_sub(2);
        match role.as_str() {
            "compaction" => {
                let text = body.strip_prefix("[COMPACT] ").unwrap_or(body);
                wrap_with_prefix(&format!("[COMPACT {}] ", ts), text, tw)
            }
            "summary" => {
                let text = body.strip_prefix("[SUMMARY] ").unwrap_or(body);
                wrap_with_prefix(&format!("[SUMMARY {}] ", ts), text, tw)
            }
            // Marker already in the body; timestamp goes in parens.
            "model" | "thinking" => vec![format!("{} ({})", body, ts)],
            "user" | "assistant" => wrap_with_prefix(
                &format!("[{} {}] ", role.to_uppercase(), ts),
                body,
                tw,
            ),
            // Label/custom/meta: terse legacy form.
            r => vec![format!("[{}] {}", r, ts)],
        }
    }
}

// ---------------------------------------------------------------------------
// TreeSwitchNode
// ---------------------------------------------------------------------------

/// Leaf node: moves the session leaf pointer to the selected entry.
pub struct TreeSwitchNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeSwitchNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("switch")
                .desc("continue from this entry")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Move the session leaf to this entry.".to_string(),
            "The next prompt continues from this point.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
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
        // Data-plane inline: move the leaf directly — the session is
        // reachable via the slot. No session → nothing to move (same as
        // the old applier's silent no-op).
        if let Some(s) = self.app_state.session() {
            let old_leaf_id = s.get_leaf_id().await;
            if old_leaf_id.as_deref() != Some(target_id.as_str()) {
                let _ = s.move_leaf(Some(target_id.clone())).await;
            }
        }
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
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for CompactNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("compact").desc("summarize conversation up to the current point")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Compact: summarize entries before the current point.".into(),
            "Redirects to /tree <id> compact.".into(),
        ]
    }

    async fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
        // Look up the current session leaf ID and redirect.
        let leaf_id = self.app_state.leaf_id()?;
        Some(format!("tree {} compact ", leaf_id))
    }

    async fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        // Enter (without drill) should also redirect — treat as drill.
        match self.app_state.leaf_id() {
            Some(leaf_id) => SubmitResult::NewPath(format!("tree {} compact ", leaf_id)),
            None => SubmitResult::Done(Some("No active session leaf to compact.".to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeCompactManualNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with manual (touched-files) mode.
/// `prepare()` opens $EDITOR with a template; `submit()` writes the
/// `tree_compact` control target (`mode: Manual`).
pub struct TreeCompactManualNode {
    pub app_state: Arc<AppState>,
    edited: std::cell::RefCell<Option<String>>,
}

impl TreeCompactManualNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state, edited: std::cell::RefCell::new(None) }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeCompactManualNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("manual").desc("user-editable file list")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
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

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let draft = self.edited.borrow_mut().take().unwrap_or_default();
        // Data-plane inline: session/agent/guard/output are all reachable
        // via the slot; errors come back as the stay-open message.
        match crate::agent::compact_inline(
            &self.app_state,
            &target_id,
            CompactMode::Manual,
            &draft,
        )
        .await
        {
            None => SubmitResult::Done(None),
            Some(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeCompactTextNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with text-only mode.
/// `prepare()` opens $EDITOR with extracted conversation text; `submit()`
/// writes the `tree_compact` control target (`mode: TextOnly`).
pub struct TreeCompactTextNode {
    pub app_state: Arc<AppState>,
    edited: std::cell::RefCell<Option<String>>,
}

impl TreeCompactTextNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state, edited: std::cell::RefCell::new(None) }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeCompactTextNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("text-only").desc("extracted conversation text")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Text-only compaction: extract user prompts + assistant text.".into(),
            "No reasoning, tool calls, or tool output. Edit in editor.".into(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let draft = self.edited.borrow_mut().take().unwrap_or_default();
        match crate::agent::compact_inline(
            &self.app_state,
            &target_id,
            CompactMode::TextOnly,
            &draft,
        )
        .await
        {
            None => SubmitResult::Done(None),
            Some(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeCompactPromptNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with LLM prompt mode.
/// `submit()` reads the custom prompt from `path_args.suffix` and
/// writes the `tree_compact` control target (`mode: Prompt`).
/// The LLM call happens in the agent loop's applier.
pub struct TreeCompactPromptNode {
    pub app_state: Arc<AppState>,
}

impl TreeCompactPromptNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeCompactPromptNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("prompt").desc("LLM-generated summary")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
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

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Pre-fill the default compaction prompt.
        // ids are ["tree", "<id>", "compact", "prompt"].
        Some(path_args.prompt_with("Summarize the conversation up to this point."))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        let prompt = path_args.suffix.trim().to_string();
        match crate::agent::compact_inline(
            &self.app_state,
            &target_id,
            CompactMode::Prompt,
            &prompt,
        )
        .await
        {
            None => SubmitResult::Done(None),
            Some(msg) => SubmitResult::Done(Some(msg)),
        }
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
    app_state: &Arc<AppState>,
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
        TreeListNode::new(app_state.clone()).into();
    tree_node.add_child(TreeSwitchNode {
        app_state: app_state.clone(),
    }
    .into());

    // Compact subtree: a listing node with three mode children.
    let mut compact_node: SlashDotMenuNode = StaticListingNode::new(&[StaticRow::new(
        "compact",
        "summarize entries before this point",
    )])
    .into();
    compact_node.add_child(TreeCompactManualNode::new(app_state.clone()).into());
    compact_node.add_child(TreeCompactTextNode::new(app_state.clone()).into());
    compact_node.add_child(TreeCompactPromptNode::new(app_state.clone()).into());

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

    /// Fresh app state for tests (binding kept as `app_state` in tests below).
    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(test_settings())
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

    /// Build app state with a pre-populated tree.
    fn app_state_with_tree(tree: Vec<SessionTreeNode>) -> Arc<AppState> {
        let app_state = test_app_state();
        app_state.set_raw_tree(Arc::new(tree));
        app_state
    }

    fn build_tree_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_tree_node(app_state));
        SlashDotMenu::from_root_node(root)
    }

    // --- Basic listing ---

    #[test]
    fn empty_tree_shows_empty_listing() {
        let app_state = test_app_state();
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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

    // --- §5.1 restore ladder ---

    fn expect_continue(result: MenuResult) -> (crate::slashdot_menu::render::RenderPlan, String) {
        match result {
            MenuResult::Continue(plan, prompt) => (plan, prompt),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// Esc drill-out must restore the stored `view_args` (the filter)
    /// *and* pin the cursor on the drilled row — the reported bug had
    /// the restore firing behind the scenes while the prompt kept the
    /// bare `<id>`, so the next cursor move dropped back to the full
    /// listing. Exercises the full sequence: `/tree .tags:leaf` →
    /// MoveCursor → Tab (drill) → Esc (restore).
    #[test]
    fn esc_drill_out_restores_filter_and_cursor() {
        let tree = vec![node(
            make_entry("u1", None, "user", "parent"),
            vec![
                node(make_entry("u2", Some("u1"), "user", "child a"), vec![]),
                node(make_entry("u3", Some("u1"), "user", "child b"), vec![]),
            ],
        )];
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();

        // 1. Filtered leaf overview: u1 is a branch parent (no `tags`
        //    field → dropped); only u2/u3 survive.
        let (plan, prompt) =
            expect_continue(futures::executor::block_on(handle.eval_prompt_resolved("tree .tags:leaf", HostKey::None, 80)));
        assert_eq!(plan.rows.len(), 2, "filter must show only leaves: {:?}", plan.rows);
        assert_eq!(prompt, "tree .tags:leaf");

        // 2. Cursor to the second leaf, Tab drills into it.
        let _ = futures::executor::block_on(handle.eval_prompt_resolved("tree .tags:leaf", HostKey::MoveCursor(1), 80));
        let (_, prompt) =
            expect_continue(futures::executor::block_on(handle.eval_prompt_resolved("tree .tags:leaf", HostKey::Tab, 80)));
        assert_eq!(prompt, "tree u3 ", "drill appends cursored id + space");

        // 3. Esc drills out: §5.1 step 1 restores the stored filter in
        //    the prompt (not a bare id) with the cursor back on u3.
        let (plan, prompt) =
            expect_continue(futures::executor::block_on(handle.eval_prompt_resolved(&prompt, HostKey::Esc, 80)));
        assert_eq!(prompt, "tree .tags:leaf",
            "Esc must restore the filter view_args into the prompt");
        assert_eq!(plan.rows.len(), 2,
            "restored view is the filtered listing: {:?}", plan.rows);
        let selected = plan.selected.unwrap_or(0);
        assert!(plan.rows[selected].0.contains("child b"),
            "cursor must be pinned on the drilled row (u3): {:?}", plan.rows[selected]);

        // 4. A cursor move after the restore stays in the filtered
        //    view (the bug: it fell back to the full listing).
        let (plan, _) =
            expect_continue(futures::executor::block_on(handle.eval_prompt_resolved(&prompt, HostKey::MoveCursor(-1), 80)));
        assert_eq!(plan.rows.len(), 2,
            "filtered view must survive cursor moves: {:?}", plan.rows);
    }

    // --- Caching ---

    #[test]
    fn tree_listing_caching_skips_rebuild() {
        let tree = vec![node(
            make_entry("u1", None, "user", "test"),
            vec![],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };

        let r1 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();
        let r2 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();
        assert!(Rc::ptr_eq(&r1, &r2),
            "same tree + same leaf should return same Rc");
    }

    #[test]
    fn tree_listing_caching_rebuilds_on_change() {
        let tree = vec![node(
            make_entry("u1", None, "user", "test"),
            vec![],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };

        let r1 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();

        // Change the tree.
        let new_tree = vec![
            node(make_entry("u1", None, "user", "test"), vec![]),
            node(make_entry("u2", None, "user", "new entry"), vec![]),
        ];
        app_state.set_raw_tree(Arc::new(new_tree));

        let r2 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();
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
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into()],
            suffix: String::new(),
            args: Default::default(),
        };

        // Trigger data() first to populate the cache.
        let _ = futures::executor::block_on(tree_node.data(0, &path_args));

        let header = futures::executor::block_on(tree_node.header(&path_args, &RenderContext { term_width: 80 }));
        let joined = header.join("\n");
        assert!(joined.contains("hello"), "header should contain message text: {}", joined);
        assert!(joined.contains("world"), "header should contain full text: {}", joined);
        assert!(joined.contains("USER"), "header should contain role: {}", joined);
    }

    fn row_field(row: &Row, name: &str) -> Option<String> {
        row.fields.iter().find(|(k, _)| k == name).and_then(|(_, v)| {
            match v {
                crate::slashdot_menu::value::Value::Str(s) => Some(s.clone()),
                _ => None,
            }
        })
    }

    fn tree_path(ids: &[&str]) -> PathArgs {
        PathArgs {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            suffix: String::new(),
            args: Default::default(),
        }
    }

    #[test]
    fn tree_row_desc_keeps_newlines_and_carries_fields() {
        let tree = vec![node(
            make_entry("u1", None, "user", "hello\nworld"),
            vec![],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());
        let resp = futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let row = resp
            .rows
            .iter()
            .find(|r| r.id == "u1")
            .expect("u1 row");
        let desc = row.description.clone().unwrap_or_default();
        assert!(desc.contains("hello\nworld"),
            "desc must keep newlines for header/fuzzy: {:?}", desc);
        assert_eq!(row_field(row, "role").as_deref(), Some("user"));
        assert_eq!(
            row_field(row, "timestamp").as_deref(),
            Some("2025-01-01T00:00:00.000Z")
        );
    }

    #[test]
    fn tree_terminal_nodes_get_leaf_tag() {
        let tree = vec![node(
            make_entry("u1", None, "user", "parent"),
            vec![
                node(make_entry("u2", Some("u1"), "user", "child a"), vec![]),
                node(make_entry("u3", Some("u1"), "user", "child b"), vec![]),
            ],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());
        let resp = futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let tags = |id: &str| {
            resp.rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| row_field(r, "tags"))
        };
        assert_eq!(tags("u1"), None,
            "parent has visible children — not a leaf endpoint");
        assert_eq!(tags("u2"), Some("leaf".to_string()));
        assert_eq!(tags("u3"), Some("leaf".to_string()));
    }

    #[test]
    fn tree_terminal_hides_behind_visible_descendants() {
        // u1 → u2 (invisible tool result) → u3 (visible): u1 is NOT a
        // terminal (visible descendant below), u3 is.
        let tree = vec![node(
            make_entry("u1", None, "user", "start"),
            vec![node(
                make_entry("u2", Some("u1"), "toolResult", ""),
                vec![node(
                    make_entry("u3", Some("u2"), "user", "after tools"),
                    vec![],
                )],
            )],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());
        let resp = futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let tags = |id: &str| {
            resp.rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| row_field(r, "tags"))
        };
        assert_eq!(tags("u1"), None, "visible descendant below");
        assert_eq!(tags("u3"), Some("leaf".to_string()));
        assert!(resp.rows.iter().all(|r| r.id != "u2"),
            "invisible tool result has no row");
    }

    #[test]
    fn tree_compaction_header_from_cached_row() {
        let compaction = SessionEntry {
            id: "c1".into(),
            parent_id: Some("u1".into()),
            timestamp: "2025-01-02T00:00:00.000Z".into(),
            content: SessionEntryContent::Compaction {
                summary: "summarised body".into(),
                first_kept_entry_id: "u1".into(),
                tokens_before: 100,
            },
        };
        let tree = vec![node(
            make_entry("u1", None, "user", "kept context line"),
            vec![node(compaction, vec![])],
        )];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        // Desc carries the truncated "from" context AND the summary.
        let resp = futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let row = resp.rows.iter().find(|r| r.id == "c1").expect("c1 row");
        let desc = row.description.clone().unwrap_or_default();
        assert!(desc.contains("[COMPACT] from kept context line"),
            "desc from-context: {:?}", desc);
        assert!(desc.contains("summarised body"), "desc summary: {:?}", desc);
        assert_eq!(row_field(row, "tags").as_deref(), Some("leaf"));

        // Header renders from that row: marker with timestamp, then
        // from-context, then summary.
        let header = futures::executor::block_on(tree_node.header(&tree_path(&["tree", "c1"]),
            &RenderContext { term_width: 120 }));
        let joined = header.join("\n");
        assert!(joined.contains("[COMPACT 2025-01-02"),
            "header timestamped marker: {}", joined);
        assert!(joined.contains("from kept context line"),
            "header from-context: {}", joined);
        assert!(joined.contains("summarised body"),
            "header summary: {}", joined);
    }

    // --- Switch ---

    #[test]
    fn tree_switch_submit_moves_leaf() {
        let app_state = test_app_state();
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "some-entry",
            None,
            "user",
            "hi",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut tree_node = TreeSwitchNode { app_state: app_state.clone() };
        // Action node path: ["tree", "<entry-id>", "switch"]
        let path_args = PathArgs {
            ids: vec!["tree".into(), "some-entry".into(), "switch".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let _ = futures::executor::block_on(tree_node.submit(1, &path_args));
        let leaf = futures::executor::block_on(session.get_leaf_id());
        assert_eq!(
            leaf.as_deref(),
            Some("some-entry"),
            "submit must move the session leaf directly"
        );
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
        let app_state = app_state_with_tree(tree);
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "u1",
            None,
            "user",
            "first message",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();

        // 1. Listing shows entries.
        let listing = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
        match &listing {
            MenuResult::Continue(plan, _) => {
                assert!(!plan.rows.is_empty(), "tree listing should have rows");
            }
            other => panic!("expected Continue at listing, got {:?}", std::mem::discriminant(other)),
        }

        // 2. Drill into u1.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into entry");

        // 3. Drill into switch.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 switch ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into switch");

        // 4. Enter → submit.
        let _ = futures::executor::block_on(handle.eval_prompt("tree u1 switch ", HostKey::Enter, 80));
        let leaf = futures::executor::block_on(session.get_leaf_id());
        assert_eq!(
            leaf.as_deref(),
            Some("u1"),
            "switch must move the session leaf to the entry ID"
        );
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        app_state.set_leaf_id(Some("u2".into()));
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();
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
        let app_state = app_state_with_tree(tree);
        app_state.set_leaf_id(Some("u2".into()));
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();
        assert_eq!(resp.active_ids, vec!["u2".to_string()],
            "active_ids should find leaf deep in tree");
    }

    // --- Compact: mode submit tests ---

    #[test]
    fn compact_manual_submit_appends_compaction() {
        let app_state = test_app_state();
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "u1",
            None,
            "user",
            "hi",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactManualNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "manual".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        let appended = crate::agent::block_on_test(async {
            let idx = session.index().lock().await;
            idx.entries.values().any(|e| {
                matches!(
                    &e.content,
                    crate::session::SessionEntryContent::Compaction { .. }
                )
            })
        });
        assert!(appended, "manual compact must append a Compaction entry");
    }

    #[test]
    fn compact_text_only_submit_appends_compaction() {
        let app_state = test_app_state();
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "u1",
            None,
            "user",
            "hi",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactTextNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "text-only".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        let appended = crate::agent::block_on_test(async {
            let idx = session.index().lock().await;
            idx.entries.values().any(|e| {
                matches!(
                    &e.content,
                    crate::session::SessionEntryContent::Compaction { .. }
                )
            })
        });
        assert!(appended, "text-only compact must append a Compaction entry");
    }

    #[test]
    fn compact_prompt_unknown_entry_errors() {
        let app_state = test_app_state();
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "other",
            None,
            "user",
            "hi",
        )]);
        app_state.set_session(Some(session));
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "prompt".into()],
            suffix: "summarize the key decisions".into(),
            args: Default::default(),
        };
        // Guarded before any agent/LLM work: unknown target stays open
        // with the message (the Llm path itself needs a live model).
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("unknown tree entry"), "msg: {msg}")
            }
            other => panic!("expected stay-open unknown-entry error, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn compact_prompt_without_session_reports_error() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into(), "compact".into(), "prompt".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("no active session"), "msg: {msg}")
            }
            other => panic!("expected stay-open no-session error, got {:?}", std::mem::discriminant(&other)),
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
        let app_state = app_state_with_tree(tree);
        let session = crate::agent::test_session_with_entries(vec![make_entry(
            "u1",
            None,
            "user",
            "first message",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();

        // 1. Listing.
        let listing = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
        assert!(matches!(listing, MenuResult::Continue(_, _)));

        // 2. Drill into u1.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 3. Drill into compact.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 compact ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 4. Drill into manual.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 compact manual ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 5. Enter → submit (inline compact drives session IO — tokio
        // driver; review is None without a term guard → non-interactive
        // append).
        let _ = crate::agent::block_on_test(handle.eval_prompt("tree u1 compact manual ", HostKey::Enter, 80));
        let appended = crate::agent::block_on_test(async {
            let idx = session.index().lock().await;
            idx.entries.values().any(|e| {
                matches!(
                    &e.content,
                    crate::session::SessionEntryContent::Compaction { .. }
                )
            })
        });
        assert!(appended, "traversal must end in an appended Compaction");
    }

    // --- Compact: header tests ---

    #[test]
    fn compact_manual_header_shows_description() {
        let app_state = test_app_state();
        let mut node = TreeCompactManualNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let header = futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
        assert!(!header.is_empty());
        assert!(header[0].contains("Manual"), "header mentions manual: {}", header[0]);
    }

    #[test]
    fn compact_prompt_header_shows_prompt_when_present() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: "my custom prompt".into(),
            args: Default::default(),
        };
        let header = futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
        assert!(header.iter().any(|l| l.contains("my custom prompt")),
            "header should contain the prompt text: {:?}", header);
    }

    #[test]
    fn compact_prompt_header_shows_default_when_empty() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let header = futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
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
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec!["tree".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();
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
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
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
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();

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
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state);
        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };
        let resp = futures::executor::block_on(tree_node.data(0, &path_args)).unwrap();
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
        let app_state = test_app_state();
        app_state.set_leaf_id(Some("msg-42".into()));
        let mut node = CompactNode { app_state: app_state.clone() };
        let path = vec!["compact".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some("tree msg-42 compact ".to_string()));
    }

    #[test]
    fn compact_node_no_leaf_returns_none() {
        let app_state = test_app_state();
        // leaf_id is None (no session).
        let mut node = CompactNode { app_state: app_state.clone() };
        let path = vec!["compact".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, None);
    }

    #[test]
    fn compact_prompt_drill_rewrite_fills_default() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path = vec!["tree".to_string(), "msg-1".to_string(),
                        "compact".to_string(), "prompt".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some(
            "tree msg-1 compact prompt Summarize the conversation up to this point.".to_string()
        ));
    }
}
