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

use crate::app_state::{AppState, CompactMode};
use crate::session::{SessionEntry, SessionEntryContent, SessionTreeNode};
use crate::slashdot_menu::fields::ColumnDef;
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};
use crate::slashdot_menu::value::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
                                        Some("text") => item.get("text").and_then(|t| t.as_str()),
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
        SessionEntryContent::SessionInfo { .. } | SessionEntryContent::Leaf { .. } => String::new(),
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
fn collect_active_path(nodes: &[SessionTreeNode], leaf_id: &str, out: &mut HashSet<String>) {
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
fn find_entry<'a>(nodes: &'a [SessionTreeNode], id: &str) -> Option<&'a SessionTreeNode> {
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
    /// The rendered tree prefix: connector + bar cell + reply step
    /// (doc/tree_visual_affordances.md §5). Carries its own SGR, so it is
    /// kept out of `desc` — the fuzzy haystack and `header()` read that.
    prefix: String,
    /// Spur level; `0` = main line. Replaces the old space-padding indent.
    depth: usize,
    /// Branch ordinal for colour: `0` = the spine (main line), `1..n` = the
    /// n-th spur of the parent in descending-height order.
    branch: u8,
    /// First visible row of a spur — the row that draws the connector.
    spur_start: bool,
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
                Some("user") => message
                    .pointer("/content/0/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(user)")
                    .to_string(),
                Some("assistant") => message
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                                Some("text") => item.get("text").and_then(|t| t.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            }
        }
        SessionEntryContent::Compaction { summary, .. } => summary.clone(),
        SessionEntryContent::ThinkingLevelChange { thinking_level } => {
            format!("reasoning: {}", thinking_level)
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
        SessionEntryContent::Message { message } => message
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown")
            .to_string(),
        SessionEntryContent::Compaction { .. } => "compaction".into(),
        SessionEntryContent::ThinkingLevelChange { .. } => "reasoning".into(),
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
            if visible(child) || has_vis.get(&child.entry.id).copied().unwrap_or(false) {
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

/// Each node's children ranked by `(height, write order)`, deepest first.
///
/// `ranked[node][0]` is therefore the **spine** — the child that continues
/// the parent's line — and the rest are spurs, tallest first, which is the
/// order hues are handed out in (§6.1). Heights are measured over the
/// *visible* projection: invisible entries (tool results, tool-call-only
/// assistant segments) must not make a spur look deep.
///
/// A purely linear tree ranks every child first, so every child is a spine.
///
/// Iterative post-order — a 1000-entry linear session overflows recursion
/// (same constraint as `visible_terminals`). Write positions are distinct,
/// so the ranking is a total order and the layout is a pure function of the
/// tree.
fn ranked_children<'a>(
    nodes: &'a [SessionTreeNode],
    labels: &HashMap<String, String>,
) -> HashMap<&'a str, Vec<&'a str>> {
    let visible = |n: &SessionTreeNode| -> bool {
        !entry_display_label_ext(&n.entry, n.label.as_deref(), labels).is_empty()
    };
    let mut height: HashMap<&'a str, usize> = HashMap::new();
    let mut ranked: HashMap<&'a str, Vec<&'a str>> = HashMap::new();

    let mut stack: Vec<(&'a SessionTreeNode, bool)> = Vec::new();
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
        let vis = usize::from(visible(node));
        let mut h = vis;
        let mut children: Vec<(usize, usize, &'a str)> = Vec::with_capacity(node.children.len());
        for (i, child) in node.children.iter().enumerate() {
            let id = child.entry.id.as_str();
            let ch = height.get(id).copied().unwrap_or(0);
            h = h.max(vis + ch);
            children.push((ch, i, id));
        }
        // Descending, so index 0 is the spine; ties in height go to the
        // newer (later-written) branch, as in the spine rule.
        children.sort_by(|a, b| b.cmp(a));
        height.insert(node.entry.id.as_str(), h);
        ranked.insert(
            node.entry.id.as_str(),
            children.into_iter().map(|(_, _, id)| id).collect(),
        );
    }
    ranked
}

// ---------------------------------------------------------------------------
// Row prefix (doc/tree_visual_affordances.md §5-§6)
// ---------------------------------------------------------------------------
//
// The prefix is pure presentation: depth, branch ordinal and the glyph are
// decided by the data layer, and this section turns them into cells. It
// lives here for now so `/tree` is reviewable end to end; the generic
// gutter work owns this in the end, and `row_prefix` is the seam — it is
// the only function that knows about glyphs, colour or column arithmetic.

/// Columns per spur level. The bar occupies the *last* column of its
/// branch's indent block, so the text column is exactly where the
/// space-only layout put it: `text_col(d) = d * SPUR_STEP`.
const SPUR_STEP: usize = 4;

/// Column of a branch's bar. The main line has no indent, so its bar is at
/// column 0; a spur's bar is the last column of its indent.
fn bar_col(depth: usize) -> usize {
    if depth == 0 {
        0
    } else {
        depth * SPUR_STEP - 1
    }
}

/// What the bar cell draws. The vertical is per *branch*: it runs through
/// the branch's rows and stops at its end, so a spur that forks in a new
/// turn still shows one continuous pipe.
///
/// Heavy box-drawing throughout — over a saturated wash the thin strokes
/// (U+2502 and friends) read as too faint, so the bar is carried by the
/// stroke rather than by the colour behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bar {
    /// `┃` — the branch continues below.
    Line,
    /// `┣` — a reply whose branch continues below (the arm points at it).
    Reply,
    /// `┗` — a reply on the branch's last row: the ladder turns off.
    ReplyEnd,
    /// `┻` — a non-reply on the branch's last row: the pipe ends.
    End,
    /// `┓` — a spur's first visible row: the connector arrives from the
    /// left and turns down into this branch.
    SpurStart,
}

impl Bar {
    fn glyph(self) -> char {
        match self {
            Bar::Line => '\u{2503}',      // ┃ HEAVY VERTICAL
            Bar::Reply => '\u{2523}',     // ┣ HEAVY VERTICAL AND RIGHT
            Bar::ReplyEnd => '\u{2517}',  // ┗ HEAVY UP AND RIGHT
            Bar::End => '\u{253b}',       // ┻ HEAVY UP AND HORIZONTAL
            Bar::SpurStart => '\u{2513}', // ┓ HEAVY DOWN AND LEFT
        }
    }
}

/// Main-line anchor: warm orange, fully saturated. Red would read as an
/// error in most terminals; orange carries the same warmth without it.
const ANCHOR_RGB: (u8, u8, u8) = (0xf9, 0x73, 0x16);

/// Spur hues, in the order they are handed out. Starts just after the
/// anchor and warms back up at the end, so the common two-or-three-spur
/// case gets warm, clearly distinct hues.
const SPUR_RGB: [(u8, u8, u8); 8] = [
    (0xf4, 0x72, 0xb6), // rose
    (0xa7, 0x8b, 0xfa), // violet
    (0x60, 0xa5, 0xfa), // azure
    (0x22, 0xd3, 0xee), // cyan
    (0x2d, 0xd4, 0xbf), // teal
    (0x6e, 0xe7, 0xa8), // green
    (0xa3, 0xe6, 0x35), // lime
    (0xfb, 0xbf, 0x24), // amber
];

/// Unselected row background (`48;5;235`) — the wash fades toward it.
const ROW_BG_RGB: (u8, u8, u8) = (0x26, 0x26, 0x26);

/// Connector ink: neutral and dim, never a branch hue. Weight (light
/// `└`/`─` against the heavy verticals) and ink together keep two adjacent
/// sub-trees from reading as one pipe.
const CONNECTOR_RGB: (u8, u8, u8) = (0x6e, 0x6e, 0x6e);

/// Wash strength by spur depth. The main line is the most saturated thing
/// on screen (it is the anchor); each level in recedes. The ink stays
/// black, so the lines remain crisp however far out a branch is.
const FADE: [f64; 6] = [1.00, 0.75, 0.58, 0.45, 0.36, 0.30];

fn fade_at(depth: usize) -> f64 {
    *FADE.get(depth).unwrap_or_else(|| FADE.last().unwrap())
}

/// Blend `c` toward the row background by `f` (1.0 = untouched).
fn blend(c: (u8, u8, u8), f: f64) -> (u8, u8, u8) {
    let mix = |from: u8, to: u8| -> u8 {
        (to as f64 + (from as f64 - to as f64) * f)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (
        mix(c.0, ROW_BG_RGB.0),
        mix(c.1, ROW_BG_RGB.1),
        mix(c.2, ROW_BG_RGB.2),
    )
}

/// The hue for a branch ordinal: 0 is the anchor, `n` the n-th spur in
/// cycle order.
fn branch_rgb(branch: u8) -> (u8, u8, u8) {
    if branch == 0 {
        ANCHOR_RGB
    } else {
        SPUR_RGB[(branch as usize - 1) % SPUR_RGB.len()]
    }
}

/// Build the prefix for one row: the connector (on a spur's first visible
/// row), the bar cell, and the assistant step.
///
/// The spur connector is not decorative: pre-order DFS guarantees a node's
/// first child is emitted on the row immediately below it, so the row above
/// a spur's first row *is* its parent.
fn row_prefix(depth: usize, branch: u8, bar: Bar, assistant: bool) -> String {
    let bar_at = bar_col(depth);
    let mut out = String::new();

    // Connector: a heavy elbow in the parent's bar column, then a heavy run
    // to our bar. The block has no heavy *arc* (`╰` is a light-arc only), so
    // the connector is told apart from the bar by its neutral ink rather
    // than by its weight.
    if bar == Bar::SpurStart {
        let parent_at = bar_col(depth.saturating_sub(1));
        out.push_str(&" ".repeat(parent_at));
        let run = bar_at.saturating_sub(parent_at + 1);
        out.push_str(&format!(
            "\x1b[38;2;{};{};{}m\u{2517}{}\x1b[0m",
            CONNECTOR_RGB.0,
            CONNECTOR_RGB.1,
            CONNECTOR_RGB.2,
            "\u{2501}".repeat(run),
        ));
    } else {
        out.push_str(&" ".repeat(bar_at));
    }

    // The bar cell: wash (hue faded by depth) with black ink, so the wash
    // recedes while the line stays legible.
    let wash = blend(branch_rgb(branch), fade_at(depth));
    out.push_str(&format!(
        "\x1b[48;2;{};{};{}m\x1b[38;2;0;0;0m{}\x1b[0m",
        wash.0,
        wash.1,
        wash.2,
        bar.glyph()
    ));

    // A reply sits one column right of the bar. The junction's arm already
    // points right, so the step carries a grey `╸` that continues that
    // horizontal into the text and hooks up to the row above: with the
    // reply indent down to a single column, the mark is what keeps replies
    // scannable.
    if assistant {
        out.push_str(&format!(
            "\x1b[38;2;{};{};{}m\u{2578}\x1b[0m",
            CONNECTOR_RGB.0, CONNECTOR_RGB.1, CONNECTOR_RGB.2,
        ));
    }
    out
}

/// Recursively flatten the tree into rows.
fn flatten_turns(
    nodes: &[SessionTreeNode],
    leaf_id: Option<&str>,
    active_path: &HashSet<String>,
    labels: &HashMap<String, String>,
    terminals: &HashSet<String>,
    out: &mut Vec<FlatRow>,
) {
    let ranked = ranked_children(nodes, labels);
    // Global spur counter: hues are handed out once per spur, tallest
    // first within each fork, so two different level-1 excursions get
    // different colours (a per-parent ordinal would make every single-spur
    // branch the same colour, which says nothing about identity). Starts at
    // 1 — 0 is the spine/anchor.
    let mut next_spur: u8 = 1;

    // A frame carries the branch state of the row it will emit:
    // `(node, depth, branch, spur_start)`. `spur_start` is set on a spur's
    // first child and *propagated* through invisible nodes to the first
    // visible row of that spur, so a marker never lands on a hidden entry.
    type Frame<'a> = (&'a SessionTreeNode, usize, u8, bool);

    // Iterative DFS to avoid stack overflow on deep linear sessions.
    // Push root nodes in reverse so the first sibling is processed first.
    let mut stack: Vec<Frame> = Vec::new();
    for node in nodes.iter().rev() {
        stack.push((node, 0, 0, false));
    }

    while let Some((node, depth, branch, spur_start)) = stack.pop() {
        let label = entry_display_label_ext(&node.entry, node.label.as_deref(), labels);

        let mut child_frames = |pending: bool| -> Vec<Frame> {
            // Hand out hues in the node's ranked order: rank 0 is the spine
            // (branch 0), the rest take the next colours.
            let order = ranked
                .get(node.entry.id.as_str())
                .map(|ids| ids.as_slice())
                .unwrap_or(&[]);
            // Returns (branch, is_spur). "Spur" is a *rank*, not a colour:
            // a spine inside a spur inherits a non-zero branch but does not
            // step right and does not open a new connector.
            let mut branch_of = |child: &SessionTreeNode| -> (u8, bool) {
                match order.iter().position(|id| *id == child.entry.id.as_str()) {
                    // The spine continues the current branch, so it
                    // inherits its hue — a spur's replies are the same
                    // colour as the spur's first row.
                    None | Some(0) => (branch, false),
                    Some(_) => {
                        let n = next_spur;
                        next_spur = next_spur.saturating_add(1);
                        (n, true)
                    }
                }
            };
            let mut frames: Vec<Frame> = Vec::with_capacity(node.children.len());
            for (i, child) in node.children.iter().enumerate() {
                let (b, spur) = branch_of(child);
                // A spur's own first row is its start. `pending` (this
                // node's flag) is only alive because the node is invisible
                // and emitted nothing, and it travels to the *first* child
                // — the only one whose subtree can emit the next row.
                frames.push((
                    child,
                    if spur { depth + 1 } else { depth },
                    b,
                    spur || (pending && i == 0),
                ));
            }
            frames
        };

        if label.is_empty() {
            // Meta entry: skip visual row but descend into children. The
            // spine rule applies here too — a meta entry can be a fork
            // point even though it has no row of its own.
            stack.extend(child_frames(spur_start).into_iter().rev());
            continue;
        }

        let assistant = is_assistant(&node.entry);
        // The bar glyph (§5.1): the vertical runs per branch, so the
        // ladder turns off / the pipe ends on the branch's last row, not
        // at the end of each turn.
        let bar = if spur_start {
            Bar::SpurStart
        } else if terminals.contains(&node.entry.id) {
            if assistant {
                Bar::ReplyEnd
            } else {
                Bar::End
            }
        } else if assistant {
            Bar::Reply
        } else {
            Bar::Line
        };

        out.push(FlatRow {
            prefix: row_prefix(depth, branch, bar, assistant),
            depth,
            branch,
            spur_start,
            desc: entry_row_desc(&node.entry, node.label.as_deref(), labels),
            entry_id: node.entry.id.clone(),
            on_active_path: active_path.contains(&node.entry.id),
            is_current: leaf_id == Some(node.entry.id.as_str()),
            role: entry_role(&node.entry),
            timestamp: node.entry.timestamp.clone(),
            is_terminal: terminals.contains(&node.entry.id),
        });

        // Push children in reverse so first child is processed first.
        stack.extend(child_frames(false).into_iter().rev());
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
                .or_else(|| label.map(sanitise_single_line));
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
                .or_else(|| label.map(sanitise_single_line));
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
    vec![ColumnDef::new("description", "")
        .min(20)
        .prio(3)
        .wrap(false)]
}

/// Field metadata for the tree listing.
pub fn tree_fields() -> Vec<FieldSpec> {
    let mut tags = FieldSpec::new("tags", false);
    tags.description = Some("leaf = branch endpoint".to_string());
    let mut depth = FieldSpec::new("depth", false);
    depth.description = Some("spur level (0 = main line)".to_string());
    let mut branch = FieldSpec::new("branch", false);
    branch.description = Some("colour ordinal: 0 = main line".to_string());
    let mut spur_start = FieldSpec::new("spur_start", false);
    spur_start.description = Some("1 = first row of a sub-tree".to_string());
    vec![
        FieldSpec::new("role", false),
        FieldSpec::new("timestamp", false),
        depth,
        branch,
        spur_start,
        tags,
    ]
}

/// Split `line` into chunks of at most `max` display columns, breaking at a
/// space where one is available and hard-splitting a word too long to fit on
/// its own. Returns slices of `line`, so every chunk is valid UTF-8 by
/// construction — `line` is arbitrary user/model text and may carry
/// multi-byte chars at any offset, so a byte-indexed cut can land inside one.
fn split_to_width(line: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0; // byte offset where the current chunk begins
    let mut col = 0; // display columns occupied by line[start..i]
    let mut last_space = None; // byte offset of the last breakable space
    for (i, ch) in line.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > max && i > start {
            let at = match last_space {
                Some(s) if s > start => s,
                _ => i,
            };
            out.push(&line[start..at]);
            start = if at == i { i } else { at + 1 }; // consume the break space
            col = UnicodeWidthStr::width(&line[start..i]);
            last_space = None;
        }
        if ch == ' ' {
            last_space = Some(i);
        }
        col += w;
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

/// Wrap `text` to `width` columns, prefixing the first line with `prefix`.
/// Subsequent continuation lines are indented to align with the text start.
///
/// `width` is a budget in display *columns*, not bytes, and the text is
/// whatever the session contains — so both the measurement and the cut are
/// column/char aware.
fn wrap_with_prefix(prefix: &str, text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![format!("{}(empty)", prefix)];
    }
    // Budgets are never zero: with a zero budget no cut can advance, so the
    // wrap loop would spin forever. That is reachable here — a full ISO
    // timestamp in the prefix is 32 columns, so any terminal narrower than
    // that would starve the first line of its budget entirely.
    let first_width = width.saturating_sub(prefix.width()).max(1);
    let cont_width = width.saturating_sub(2).max(1); // 2-space indent
    let mut result = Vec::new();

    for (i, line) in text.split('\n').enumerate() {
        let (pfx, eff_width) = if i == 0 {
            (prefix, first_width)
        } else {
            ("  ", cont_width)
        };
        if line.is_empty() {
            result.push(pfx.to_string());
            continue;
        }
        for chunk in split_to_width(line, eff_width) {
            result.push(format!("{pfx}{chunk}"));
        }
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
            leaf.as_deref(),
            &active_path,
            &labels,
            &terminals,
            &mut flat,
        );

        let rows: Vec<Row> = flat
            .iter()
            .map(|r| {
                // The listing cell is prefix + text. It carries SGR, which
                // is why `desc` stays bare: it is the fuzzy haystack
                // (cursor.rs) and the `header()` source, neither of which
                // wants escape bytes or box-drawing glyphs in it.
                let mut row = Row::new(r.entry_id.clone())
                    .desc(format!("{}{}", r.prefix, r.desc))
                    .field("text", Value::Str(r.desc.clone()))
                    .field("depth", Value::Num(r.depth as f64))
                    .field("branch", Value::Num(r.branch as f64))
                    .field("role", Value::Str(r.role.clone()))
                    .field("timestamp", Value::Str(r.timestamp.clone()));
                if r.spur_start {
                    row = row.field("spur_start", Value::Str("1".into()));
                }
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

        let response = Rc::new(DataResponse { rows, active_ids });

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

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx();
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
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
        // The bare text (no prefix): the listing cell is `prefix + text`,
        // and the header shows only the text.
        let body: String = row
            .fields
            .iter()
            .find(|(k, _)| k == "text")
            .and_then(|(_, v)| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .or_else(|| row.description.clone())
            .unwrap_or_default();
        let body = body.as_str();
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
            "model" | "reasoning" => vec![format!("{} ({})", body, ts)],
            "user" | "assistant" => {
                wrap_with_prefix(&format!("[{} {}] ", role.to_uppercase(), ts), body, tw)
            }
            // Label/custom/meta: terse legacy form.
            r => vec![format!("[{}] {}", r, ts)],
        }
    }
}

// ---------------------------------------------------------------------------
// TreeSwitchNode
// ---------------------------------------------------------------------------

/// Leaf node: moves the session leaf pointer to the selected entry — the
/// `cursor` row of the `/tree <id> switch` sub-menu. Carries no summary; the
/// sibling `summarize` sub-menu is the two-phase variant
/// (`TreeSwitchSummarizeNode`).
pub struct TreeSwitchCursorNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeSwitchCursorNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("cursor").desc("move the session cursor to this entry")],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx();
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Move the session leaf to this entry.".to_string(),
            "The next prompt continues from this point.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        // The path is ["tree", "<entry-id>", "switch", "cursor"]; the entry ID
        // is always at index 1 — the same slot every `/tree` sub-menu reads, so
        // the rule does not shift as rows are added (`cursor` here,
        // `summarize <mode>` below).
        let target_id = extract_target_id(path_args);
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

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx();
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
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

/// Leaf node: compact with manual (touched-files) mode — the shared
/// two-phase compaction flow (doc/editor_cleanup.md §6): `prepare()`
/// generates the template and opens $EDITOR, NodeEntry's confirm
/// (`Prepared` → Enter/Esc) decides, `submit()` appends the compaction
/// from the stashed draft.
pub struct TreeCompactManualNode {
    pub app_state: Arc<AppState>,
    stash: std::cell::RefCell<Option<CompactStash>>,
}

impl TreeCompactManualNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
            stash: std::cell::RefCell::new(None),
        }
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

    async fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::Manual,
                payload: "",
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            false,
        )
        .await
    }

    async fn edit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::Manual,
                payload: "",
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            true,
        )
        .await
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Manual compaction: scan touched files, open in editor.".into(),
            "Enter opens $EDITOR; then Enter writes, Esc aborts.".into(),
        ]
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_submit(
            &self.app_state,
            epoch,
            path_args,
            SummaryAction::Compact,
            &mut self.stash,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// TreeCompactTextNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with text-only mode — same two-phase flow as the
/// manual node: generate → $EDITOR → confirm → apply.
pub struct TreeCompactTextNode {
    pub app_state: Arc<AppState>,
    stash: std::cell::RefCell<Option<CompactStash>>,
}

impl TreeCompactTextNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
            stash: std::cell::RefCell::new(None),
        }
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

    async fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::TextOnly,
                payload: "",
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            false,
        )
        .await
    }

    async fn edit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::TextOnly,
                payload: "",
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            true,
        )
        .await
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Text-only compaction: extract user prompts + assistant text.".into(),
            "No reasoning, tool calls, or tool output.".into(),
            "Enter opens $EDITOR; then Enter writes, Esc aborts.".into(),
        ]
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_submit(
            &self.app_state,
            epoch,
            path_args,
            SummaryAction::Compact,
            &mut self.stash,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// TreeCompactPromptNode
// ---------------------------------------------------------------------------

/// Leaf node: compact with LLM prompt mode — same two-phase flow: the
/// agent generates against the pivoted context (custom instruction from
/// `path_args.suffix`, else the setting), the draft opens in $EDITOR,
/// confirm, apply.
pub struct TreeCompactPromptNode {
    pub app_state: Arc<AppState>,
    stash: std::cell::RefCell<Option<CompactStash>>,
}

impl TreeCompactPromptNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
            stash: std::cell::RefCell::new(None),
        }
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

    async fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::Prompt,
                payload: &path_args.suffix,
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            false,
        )
        .await
    }

    async fn edit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: CompactMode::Prompt,
                payload: &path_args.suffix,
                action: SummaryAction::Compact,
            },
            &mut self.stash,
            true,
        )
        .await
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let prompt = &path_args.suffix;
        if prompt.is_empty() {
            vec![
                "LLM compaction: generate summary via agent, then edit.".into(),
                "Type a custom instruction after 'prompt', or use the default.".into(),
                "Enter generates + opens $EDITOR; then Enter writes, Esc aborts.".into(),
            ]
        } else {
            vec![
                "LLM compaction with custom prompt:".into(),
                format!("  {}", prompt),
                "Enter generates + opens $EDITOR; then Enter writes, Esc aborts.".into(),
            ]
        }
    }

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Pre-fill the default compaction prompt.
        // ids are ["tree", "<id>", "compact", "prompt"].
        Some(path_args.prompt_with("Summarize the conversation up to this point."))
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_submit(
            &self.app_state,
            epoch,
            path_args,
            SummaryAction::Compact,
            &mut self.stash,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// TreeEditNode
// ---------------------------------------------------------------------------

/// Leaf node: `/tree <id> edit` — re-ask a user prompt. pi's user-message
/// re-edit (`agent-session.ts:2950`): the leaf moves to the prompt's **parent**
/// and its text is pre-filled into the input buffer, so the next turn replaces
/// the prompt instead of following it. The sibling `switch` row moves the leaf
/// *to* the entry, which is the other (and also wanted) behavior.
///
/// The row only appears when the drilled entry is actually re-askable — a user
/// message or a custom message — so `edit` is never a row that fails on submit.
pub struct TreeEditNode {
    pub app_state: Arc<AppState>,
}

impl TreeEditNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeEditNode {
    async fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        let target_id = extract_target_id(path_args);
        let Some(session) = self.app_state.session() else {
            return Ok(Rc::new(DataResponse {
                rows: Vec::new(),
                active_ids: Vec::new(),
            }));
        };
        if !crate::agent::compaction::is_reaskable_entry(&session, &target_id).await {
            return Ok(Rc::new(DataResponse {
                rows: Vec::new(),
                active_ids: Vec::new(),
            }));
        }
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("edit").desc("re-ask this prompt (pre-filled)")],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx();
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Re-ask this prompt: the session moves to its parent and the".to_string(),
            "text is pre-filled into the input for editing.".to_string(),
            "The original prompt leaves the active branch.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let target_id = extract_target_id(path_args);
        match crate::agent::compaction::reask_from_entry(&self.app_state, &target_id).await {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// TreeSwitchSummarizeNode
// ---------------------------------------------------------------------------

/// Leaf node: `/tree <id> switch summarize <mode>` — summarize the spur left
/// behind by the switch, then switch. One struct, three instances (manual /
/// text-only / prompt): the modes differ only in how the draft is generated,
/// so they share the whole two-phase flow with the compact nodes
/// (`compact_prepare` / `edit_draft` / `compact_submit`) and differ only in
/// [`SummaryAction::SwitchSummarize`] — the submit that writes a
/// `BranchSummary` entry and moves the leaf, rather than a `Compaction` entry.
///
/// Path shape: `["tree", "<entry-id>", "switch", "summarize", "<mode>"]`, so
/// the entry ID is at index 1 — the same slot `extract_target_id` reads.
pub struct TreeSwitchSummarizeNode {
    pub app_state: Arc<AppState>,
    mode: CompactMode,
    stash: std::cell::RefCell<Option<CompactStash>>,
}

impl TreeSwitchSummarizeNode {
    /// One instance per mode of `/tree <id> switch summarize `.
    pub fn new(app_state: Arc<AppState>, mode: CompactMode) -> Self {
        Self {
            app_state,
            mode,
            stash: std::cell::RefCell::new(None),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for TreeSwitchSummarizeNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let (row, desc) = match self.mode {
            CompactMode::Manual => ("manual", "touched files of the abandoned branch"),
            CompactMode::TextOnly => ("text-only", "extracted text of the abandoned branch"),
            CompactMode::Prompt => ("prompt", "LLM-generated digest of the abandoned branch"),
        };
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new(row).desc(desc)],
            active_ids: Vec::new(),
        }))
    }

    async fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: self.mode,
                // Only the LLM mode takes a trailing instruction.
                payload: match self.mode {
                    CompactMode::Prompt => path_args.suffix.as_str(),
                    _ => "",
                },
                action: SummaryAction::SwitchSummarize,
            },
            &mut self.stash,
            false,
        )
        .await
    }

    async fn edit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_prepare(
            &self.app_state,
            epoch,
            path_args,
            SummaryPlan {
                mode: self.mode,
                // Only the LLM mode takes a trailing instruction.
                payload: match self.mode {
                    CompactMode::Prompt => path_args.suffix.as_str(),
                    _ => "",
                },
                action: SummaryAction::SwitchSummarize,
            },
            &mut self.stash,
            true,
        )
        .await
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let mut out = vec![
            "Summarize the branch you are leaving, then continue from the".to_string(),
            "selected entry. The digest is written at the switch target, so the".to_string(),
            "next prompt sees it (pi's branch summary).".to_string(),
            "Enter opens $EDITOR; then Enter writes and switches, Esc aborts.".to_string(),
        ];
        if self.mode == CompactMode::Prompt {
            let prompt = &path_args.suffix;
            if prompt.is_empty() {
                out.push("Type a custom focus after 'prompt', or use the default.".into());
            } else {
                out.push(format!("Custom focus: {}", prompt));
            }
        }
        out
    }

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Only the LLM mode takes a trailing instruction — the other two have
        // nothing to parameterize.
        if self.mode != CompactMode::Prompt {
            return None;
        }
        // ids are ["tree", "<id>", "switch", "summarize", "prompt"].
        Some(path_args.prompt_with("What was left unfinished, and what should happen next."))
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        compact_submit(
            &self.app_state,
            epoch,
            path_args,
            SummaryAction::SwitchSummarize,
            &mut self.stash,
        )
        .await
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

/// The session's authoritative leaf.
///
/// Deliberately **not** `AppState::leaf_id()`: that is the menu's cached view of
/// the leaf, refreshed by the loop's convergence step, so it lags a switch the
/// menu itself just performed (and is `None` in-process). The branch-summary
/// stash key must name the leaf its spur was measured from, and the apply half
/// compares against the same value — so both sides read the session.
async fn session_leaf_id(app_state: &Arc<AppState>) -> Option<String> {
    let session = app_state.session()?;
    session.get_leaf_id().await
}

/// Which summary a shared two-phase node writes on `submit()`. The generate /
/// edit / confirm half is identical (doc/editor_cleanup.md §6); only the
/// effect differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryAction {
    /// `/tree <id> compact <mode>` — replace the prefix before the target with
    /// a `Compaction` entry (the leaf stays put).
    Compact,
    /// `/tree <id> switch summarize <mode>` — write a `BranchSummary` entry
    /// for the spur being left behind, parented to the switch target, and move
    /// the leaf there (pi `branchWithSummary`).
    SwitchSummarize,
}

/// What one two-phase node generates and writes: the draft flavor, the LLM
/// instruction (`""` = the built-in default), and the effect on submit. Grouped
/// into one value because the three always travel together — the six node
/// instances are exactly the six combinations.
struct SummaryPlan<'a> {
    mode: CompactMode,
    payload: &'a str,
    action: SummaryAction,
}

/// Stashed result of a compact node's `prepare()`: the reviewed draft,
/// keyed by the target it was generated for *and* the engine epoch it
/// was made at (doc/editor_cleanup.md §4 — reused only while both
/// match). The action and, for a branch summary, the leaf the spur was
/// measured from are part of the key: the same draft means different
/// things to different applies, and a spur summary is only valid for the
/// leaf it was generated against.
struct CompactStash {
    epoch: u64,
    action: SummaryAction,
    target_id: String,
    /// The session leaf when the draft was generated — `apply_branch_summary`
    /// refuses a moved leaf rather than summarizing the wrong span.
    source_leaf_id: Option<String>,
    draft: crate::agent::compaction::CompactionDraft,
}

/// `prepare()` / `edit()` body shared by the compact and switch-summarize mode
/// nodes: generate the draft (data layer), open it in $EDITOR (UI layer),
/// stash the reviewed text, return `Prepared` — NodeEntry's two-phase
/// confirm takes it from there (Enter → [`compact_submit`], Esc →
/// abort; the stash then just waits for the next `prepare()` to
/// overwrite it, so no abort hook is needed).
///
/// `reuse_stash` (`edit()`, Ctrl-E): re-open the stashed draft while
/// its key still matches, regenerate otherwise (§4).
///
/// The stash is taken **`&mut`, not `&`** — and not because of the writes,
/// which the `RefCell` would cover on its own. `RefCell<T>` is *unconditionally*
/// `!Sync` (its borrow flag is a non-atomic `Cell`), so `&RefCell<T>` is never
/// `Send`, and this borrow lives across `generate_compaction(...).await`: a `&`
/// here would make this future `!Send` whatever `CompactStash` contained.
/// `&mut` asks only for `T: Send`, which it is. The same holds for
/// [`compact_submit`] and `edit_draft`.
async fn compact_prepare(
    app_state: &Arc<AppState>,
    epoch: u64,
    path_args: &PathArgs,
    plan: SummaryPlan<'_>,
    stash: &mut std::cell::RefCell<Option<CompactStash>>,
    reuse_stash: bool,
) -> SubmitResult {
    let target_id = extract_target_id(path_args);
    let source_leaf_id = session_leaf_id(app_state).await;
    if reuse_stash {
        let fresh = stash.borrow().as_ref().and_then(|s| {
            (s.epoch == epoch
                && s.action == plan.action
                && s.target_id == target_id
                && s.source_leaf_id == source_leaf_id)
                .then(|| s.draft.clone())
        });
        if let Some(draft) = fresh {
            return edit_draft(
                app_state,
                stash,
                epoch,
                plan.action,
                target_id,
                source_leaf_id,
                draft,
            );
        }
    }
    let summary_mode = match plan.mode {
        CompactMode::Manual => crate::agent::compaction::CompactSummaryMode::Manual,
        CompactMode::TextOnly => crate::agent::compaction::CompactSummaryMode::TextOnly,
        CompactMode::Prompt => crate::agent::compaction::CompactSummaryMode::Llm,
    };
    let generated = match plan.action {
        SummaryAction::Compact => {
            crate::agent::compaction::generate_compaction(
                app_state,
                &target_id,
                summary_mode,
                plan.payload,
            )
            .await
        }
        SummaryAction::SwitchSummarize => {
            crate::agent::compaction::generate_branch_summary(
                app_state,
                &target_id,
                summary_mode,
                plan.payload,
            )
            .await
        }
    };
    match generated {
        Ok(draft) => edit_draft(
            app_state,
            stash,
            epoch,
            plan.action,
            target_id,
            source_leaf_id,
            draft,
        ),
        // Validation/generation failures close the menu with the
        // data-level message (the pre-split compact_inline shape).
        Err(msg) => SubmitResult::Done(Some(msg)),
    }
}

/// Open `draft` in $EDITOR and stash the outcome. Without an
/// interactive tty (quiet/pipe/tests) the draft is accepted as-is —
/// the pre-split review closure behaved the same outside the REPL.
fn edit_draft(
    app_state: &Arc<AppState>,
    stash: &mut std::cell::RefCell<Option<CompactStash>>,
    epoch: u64,
    action: SummaryAction,
    target_id: String,
    source_leaf_id: Option<String>,
    mut draft: crate::agent::compaction::CompactionDraft,
) -> SubmitResult {
    match app_state.edit_text(&draft.summary, crate::editor::EditorMode::Edit) {
        Ok(text) => {
            draft.summary = text;
        }
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            // No interactive tty: accept the generated draft as-is.
        }
        Err(e) => return SubmitResult::Done(Some(format!("editor error: {}", e))),
    }
    *stash.borrow_mut() = Some(CompactStash {
        epoch,
        action,
        target_id,
        source_leaf_id,
        draft,
    });
    // The arms name what accepting does — and they differ by action: a
    // compaction rewrites history at the current position, a switch-summarize
    // writes the digest of the abandoned spur and moves there.
    let accept = match action {
        SummaryAction::Compact => "Write this summary to the session",
        SummaryAction::SwitchSummarize => "Write this summary and switch to the entry",
    };
    SubmitResult::Prepared(Some((
        accept.into(),
        "Discard it (regenerate with Enter)".into(),
    )))
}

/// `submit()` body: apply the stashed draft. The stash must still be
/// keyed to this (action, target, source leaf, epoch) — a stale one means
/// the data epoch or the session moved under us; error out and let the next
/// Enter regenerate.
async fn compact_submit(
    app_state: &Arc<AppState>,
    epoch: u64,
    path_args: &PathArgs,
    action: SummaryAction,
    stash: &mut std::cell::RefCell<Option<CompactStash>>,
) -> SubmitResult {
    let target_id = extract_target_id(path_args);
    let source_leaf_id = session_leaf_id(app_state).await;
    let prepared = stash.borrow().as_ref().and_then(|s| {
        (s.epoch == epoch
            && s.action == action
            && s.target_id == target_id
            && s.source_leaf_id == source_leaf_id)
            .then(|| s.draft.clone())
    });
    let Some(draft) = prepared else {
        return SubmitResult::Error(
            "No fresh prepared summary — press Enter to generate one.".to_string(),
        );
    };
    let applied = match action {
        SummaryAction::Compact => {
            crate::agent::compaction::apply_compaction(app_state, &target_id, &draft).await
        }
        SummaryAction::SwitchSummarize => {
            let source = source_leaf_id.unwrap_or_default();
            crate::agent::compaction::apply_branch_summary(app_state, &target_id, &source, &draft)
                .await
        }
    };
    match applied {
        Ok(()) => {
            *stash.borrow_mut() = None;
            // Silent: the user just reviewed the summary, and the
            // committed line documents the write.
            SubmitResult::Done(None)
        }
        Err(msg) => SubmitResult::Done(Some(msg)),
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build the `/tree` subtree as a `SlashDotMenuNode`.
pub fn build_tree_node(app_state: &Arc<AppState>) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::menu::SlashDotMenuNode;
    use crate::slashdot_menu::node_impl::StaticRow;
    use crate::v25_listing_node::ListingNode;

    let mut listing: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new(
            "tree",
            "navigate session history — select a point to continue from",
        )],
    )
    .with_columns(tree_listing_columns())
    .with_fields(tree_fields())
    .into();

    let mut tree_node: SlashDotMenuNode = TreeListNode::new(app_state.clone()).into();

    // `switch` sub-menu: `cursor` (today's behavior) or `summarize` (digest
    // the abandoned spur, then move there). Same shape as `compact` — a
    // listing level whose rows come from its children.
    let mut switch_node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new(
            "switch",
            "move the session cursor to this entry",
        )],
    )
    .into();
    switch_node.add_child(
        TreeSwitchCursorNode {
            app_state: app_state.clone(),
        }
        .into(),
    );
    let mut summarize_node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new(
            "summarize",
            "summarize the abandoned branch, then switch",
        )],
    )
    .into();
    for mode in [
        CompactMode::Manual,
        CompactMode::TextOnly,
        CompactMode::Prompt,
    ] {
        summarize_node.add_child(TreeSwitchSummarizeNode::new(app_state.clone(), mode).into());
    }
    switch_node.add_child(summarize_node);
    tree_node.add_child(switch_node);

    // `edit` (re-ask a user prompt) — a sibling of `switch`, not a mode of it:
    // it moves the leaf to the prompt's parent, where `switch` moves to the
    // prompt. Its row only shows up for user/custom-message entries.
    tree_node.add_child(TreeEditNode::new(app_state.clone()).into());

    // Compact subtree: a listing node with three mode children.
    let mut compact_node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new(
            "compact",
            "summarize entries before this point",
        )],
    )
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
    use crate::session::{SessionEntry, SessionEntryContent, SessionTreeNode};
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, PathArgs, StaticListingNode};
    use crate::slashdot_menu::HostKey;

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            models: Default::default(),
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
        let tree = vec![node(
            make_entry("u1", None, "user", "hello world"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "hi there"),
                vec![],
            )],
        )];
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("tree ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 2, "should have user + assistant rows");
                assert!(
                    plan.rows[0].0.contains("hello world"),
                    "first row should contain user text: {}",
                    plan.rows[0].0
                );
                assert!(
                    plan.rows[1].0.contains("hi there"),
                    "second row should contain assistant text: {}",
                    plan.rows[1].0
                );
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

    /// The leading id of each rendered row: a plan row is a `LogicalLine`
    /// holding the whole cell (`<id>  <description>`, SGR-stripped by the
    /// table), so the first whitespace-delimited token is the id.
    fn row_ids(plan: &crate::slashdot_menu::render::RenderPlan) -> Vec<String> {
        plan.rows
            .iter()
            .map(|line| {
                line.0
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
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
        let (plan, prompt) = expect_continue(futures::executor::block_on(
            handle.eval_prompt_resolved("tree .tags:leaf", HostKey::None, 80),
        ));
        assert_eq!(
            plan.rows.len(),
            2,
            "filter must show only leaves: {:?}",
            plan.rows
        );
        assert_eq!(prompt, "tree .tags:leaf");

        // 2. Cursor to the second leaf, Tab drills into it.
        let _ = futures::executor::block_on(handle.eval_prompt_resolved(
            "tree .tags:leaf",
            HostKey::MoveCursor(1),
            80,
        ));
        let (_, prompt) = expect_continue(futures::executor::block_on(
            handle.eval_prompt_resolved("tree .tags:leaf", HostKey::Tab, 80),
        ));
        assert_eq!(prompt, "tree u3 ", "drill appends cursored id + space");

        // 3. Esc drills out: §5.1 step 1 restores the stored filter in
        //    the prompt (not a bare id) with the cursor back on u3.
        let (plan, prompt) = expect_continue(futures::executor::block_on(
            handle.eval_prompt_resolved(&prompt, HostKey::Esc, 80),
        ));
        assert_eq!(
            prompt, "tree .tags:leaf",
            "Esc must restore the filter view_args into the prompt"
        );
        assert_eq!(
            plan.rows.len(),
            2,
            "restored view is the filtered listing: {:?}",
            plan.rows
        );
        let selected = plan.selected.unwrap_or(0);
        assert!(
            plan.rows[selected].0.contains("child b"),
            "cursor must be pinned on the drilled row (u3): {:?}",
            plan.rows[selected]
        );

        // 4. A cursor move after the restore stays in the filtered
        //    view (the bug: it fell back to the full listing).
        let (plan, _) = expect_continue(futures::executor::block_on(handle.eval_prompt_resolved(
            &prompt,
            HostKey::MoveCursor(-1),
            80,
        )));
        assert_eq!(
            plan.rows.len(),
            2,
            "filtered view must survive cursor moves: {:?}",
            plan.rows
        );
    }

    // --- Caching ---

    #[test]
    fn tree_listing_caching_skips_rebuild() {
        let tree = vec![node(make_entry("u1", None, "user", "test"), vec![])];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        let path_args = PathArgs {
            ids: vec![],
            suffix: String::new(),
            args: Default::default(),
        };

        let r1 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();
        let r2 = futures::executor::block_on(tree_node.data(1, &path_args)).unwrap();
        assert!(
            Rc::ptr_eq(&r1, &r2),
            "same tree + same leaf should return same Rc"
        );
    }

    #[test]
    fn tree_listing_caching_rebuilds_on_change() {
        let tree = vec![node(make_entry("u1", None, "user", "test"), vec![])];
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
        assert!(!Rc::ptr_eq(&r1, &r2), "changed tree should return new Rc");
        assert_eq!(r2.rows.len(), 2, "new tree has two entries");
    }

    // --- Header ---

    #[test]
    fn tree_entry_header_shows_message_body() {
        let tree = vec![node(make_entry("u1", None, "user", "hello\nworld"), vec![])];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());

        let path_args = PathArgs {
            ids: vec!["tree".into(), "u1".into()],
            suffix: String::new(),
            args: Default::default(),
        };

        // Trigger data() first to populate the cache.
        let _ = futures::executor::block_on(tree_node.data(0, &path_args));

        let header = futures::executor::block_on(
            tree_node.header(&path_args, &RenderContext { term_width: 80 }),
        );
        let joined = header.join("\n");
        assert!(
            joined.contains("hello"),
            "header should contain message text: {}",
            joined
        );
        assert!(
            joined.contains("world"),
            "header should contain full text: {}",
            joined
        );
        assert!(
            joined.contains("USER"),
            "header should contain role: {}",
            joined
        );
    }

    fn row_field(row: &Row, name: &str) -> Option<String> {
        row.fields
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| match v {
                crate::slashdot_menu::value::Value::Str(s) => Some(s.clone()),
                _ => None,
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
        let tree = vec![node(make_entry("u1", None, "user", "hello\nworld"), vec![])];
        let app_state = app_state_with_tree(tree);
        let mut tree_node = TreeListNode::new(app_state.clone());
        let resp =
            futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let row = resp.rows.iter().find(|r| r.id == "u1").expect("u1 row");
        let desc = row.description.clone().unwrap_or_default();
        assert!(
            desc.contains("hello\nworld"),
            "desc must keep newlines for header/fuzzy: {:?}",
            desc
        );
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
        let resp =
            futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let tags = |id: &str| {
            resp.rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| row_field(r, "tags"))
        };
        assert_eq!(
            tags("u1"),
            None,
            "parent has visible children — not a leaf endpoint"
        );
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
        let resp =
            futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let tags = |id: &str| {
            resp.rows
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| row_field(r, "tags"))
        };
        assert_eq!(tags("u1"), None, "visible descendant below");
        assert_eq!(tags("u3"), Some("leaf".to_string()));
        assert!(
            resp.rows.iter().all(|r| r.id != "u2"),
            "invisible tool result has no row"
        );
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
        let resp =
            futures::executor::block_on(tree_node.data(0, &tree_path(&["tree"]))).expect("data");
        let row = resp.rows.iter().find(|r| r.id == "c1").expect("c1 row");
        let desc = row.description.clone().unwrap_or_default();
        assert!(
            desc.contains("[COMPACT] from kept context line"),
            "desc from-context: {:?}",
            desc
        );
        assert!(desc.contains("summarised body"), "desc summary: {:?}", desc);
        assert_eq!(row_field(row, "tags").as_deref(), Some("leaf"));

        // Header renders from that row: marker with timestamp, then
        // from-context, then summary.
        let header = futures::executor::block_on(tree_node.header(
            &tree_path(&["tree", "c1"]),
            &RenderContext { term_width: 120 },
        ));
        let joined = header.join("\n");
        assert!(
            joined.contains("[COMPACT 2025-01-02"),
            "header timestamped marker: {}",
            joined
        );
        assert!(
            joined.contains("from kept context line"),
            "header from-context: {}",
            joined
        );
        assert!(
            joined.contains("summarised body"),
            "header summary: {}",
            joined
        );
    }

    // --- Switch ---

    #[test]
    fn tree_switch_cursor_submit_moves_leaf() {
        let app_state = test_app_state();
        let (_guard, session) = crate::agent::test_session_with_entries(vec![make_entry(
            "some-entry",
            None,
            "user",
            "hi",
        )]);
        app_state.set_session(Some(session.clone()));
        let mut tree_node = TreeSwitchCursorNode {
            app_state: app_state.clone(),
        };
        // Leaf path: ["tree", "<entry-id>", "switch", "cursor"] — the entry ID
        // is the second-to-last element.
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "some-entry".into(),
                "switch".into(),
                "cursor".into(),
            ],
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
        let tree = vec![node(
            make_entry("u1", None, "user", "first message"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "response"),
                vec![],
            )],
        )];
        let app_state = app_state_with_tree(tree);
        let (_guard, session) = crate::agent::test_session_with_entries(vec![make_entry(
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
            other => panic!(
                "expected Continue at listing, got {:?}",
                std::mem::discriminant(other)
            ),
        }

        // 2. Drill into u1.
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into entry");

        // 3. Drill into switch → the cursor/summarize sub-menu.
        let r =
            futures::executor::block_on(handle.eval_prompt("tree u1 switch ", HostKey::None, 80));
        match &r {
            MenuResult::Continue(plan, _) => {
                assert_eq!(
                    row_ids(plan),
                    vec!["cursor", "summarize"],
                    "switch sub-menu rows"
                );
            }
            other => panic!(
                "expected Continue at switch, got {:?}",
                std::mem::discriminant(other)
            ),
        }

        // 4. Drill into cursor, then Enter → submit.
        let r = futures::executor::block_on(handle.eval_prompt(
            "tree u1 switch cursor ",
            HostKey::None,
            80,
        ));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into cursor");
        let _ = futures::executor::block_on(handle.eval_prompt(
            "tree u1 switch cursor ",
            HostKey::Enter,
            80,
        ));
        let leaf = futures::executor::block_on(session.get_leaf_id());
        assert_eq!(
            leaf.as_deref(),
            Some("u1"),
            "switch must move the session leaf to the entry ID"
        );
    }

    // --- switch summarize ---

    /// A spur session shaped
    ///
    /// ```text
    /// u1 ─┬─ a1 ─ a2 ─ a3   (a3 is the leaf)
    ///     └─ u2
    /// ```
    ///
    /// Switching to `u1` abandons `a1` + `a2` (a spur); switching to `a3` (or
    /// the leaf itself) abandons nothing, because `a3` is *ahead* on the
    /// current branch. The entry order sets the leaf (`a3` is last) — the same
    /// convention `test_session_with_entries` uses.
    fn spur_app_state() -> (Arc<AppState>, Arc<crate::session::Session>) {
        let tree = vec![node(
            make_entry("u1", None, "user", "first message"),
            vec![
                node(
                    make_entry("a1", Some("u1"), "assistant", "response"),
                    vec![node(
                        make_entry("a2", Some("a1"), "assistant", "more work"),
                        vec![node(
                            make_entry("a3", Some("a2"), "assistant", "final turn"),
                            vec![],
                        )],
                    )],
                ),
                node(make_entry("u2", Some("u1"), "user", "other path"), vec![]),
            ],
        )];
        let app_state = app_state_with_tree(tree);
        let (_guard, session) = crate::agent::test_session_with_entries(vec![
            make_entry("u1", None, "user", "first message"),
            make_entry("a1", Some("u1"), "assistant", "response"),
            make_entry("u2", Some("u1"), "user", "other path"),
            make_entry("a2", Some("a1"), "assistant", "more work"),
            make_entry("a3", Some("a2"), "assistant", "final turn"),
        ]);
        app_state.set_session(Some(session.clone()));
        (app_state, session)
    }

    /// The `BranchSummary` entries in `session` as
    /// `(parent_id, from_id, summary)`.
    fn branch_summaries(
        session: &std::sync::Arc<crate::session::Session>,
    ) -> Vec<(String, String, String)> {
        let idx = session.index().blocking_lock();
        let mut out: Vec<(String, String, String)> = idx
            .entries
            .values()
            .filter_map(|e| match &e.content {
                SessionEntryContent::BranchSummary { from_id, summary } => Some((
                    e.parent_id.clone().unwrap_or_default(),
                    from_id.clone(),
                    summary.clone(),
                )),
                _ => None,
            })
            .collect();
        out.sort();
        out
    }

    // --- /tree <id> edit (pi's user-message re-edit) ---

    #[test]
    fn edit_row_appears_only_for_prompt_entries() {
        let (app_state, _session) = spur_app_state();
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        // `u1` is a user message → the re-ask row is offered, next to switch.
        let (plan, _) = expect_continue(futures::executor::block_on(handle.eval_prompt(
            "tree u1 ",
            HostKey::None,
            80,
        )));
        assert_eq!(
            row_ids(&plan),
            vec!["switch", "edit", "compact"],
            "u1 is a prompt"
        );
        // `a1` is an assistant reply → no re-ask row (switch still is).
        let (plan, _) = expect_continue(futures::executor::block_on(handle.eval_prompt(
            "tree a1 ",
            HostKey::None,
            80,
        )));
        assert_eq!(row_ids(&plan), vec!["switch", "compact"], "a1 is a reply");
    }

    #[test]
    fn edit_traversal_moves_to_the_parent_and_prefills() {
        let (app_state, session) = spur_app_state();
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let r = futures::executor::block_on(handle.eval_prompt("tree u1 edit ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into edit");
        // A tokio runtime: the submit writes the Leaf pointer record.
        let r =
            crate::agent::block_on_test(handle.eval_prompt("tree u1 edit ", HostKey::Enter, 80));
        assert!(matches!(r, MenuResult::DoneSuccess(..)));
        // u1's parent is the root, so re-asking it starts a fresh branch and
        // the prompt comes back for editing.
        assert_eq!(futures::executor::block_on(session.get_leaf_id()), None);
        assert_eq!(
            app_state.take_prefill().as_deref(),
            Some("first message"),
            "the prompt is pre-filled into the input"
        );
    }

    #[test]
    fn switch_summarize_lists_three_modes() {
        let (app_state, _session) = spur_app_state();
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let r = futures::executor::block_on(handle.eval_prompt(
            "tree u1 switch summarize ",
            HostKey::None,
            80,
        ));
        match &r {
            MenuResult::Continue(plan, _) => {
                assert_eq!(
                    row_ids(plan),
                    vec!["manual", "text-only", "prompt"],
                    "the same three modes as /tree <id> compact"
                );
            }
            other => panic!(
                "expected Continue at summarize, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    fn switch_summarize_full_traversal_writes_entry_at_the_target_and_moves_leaf() {
        let (app_state, session) = spur_app_state();
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();

        // Enter #1 → prepare(): generate + editor (accepted as-is without a
        // tty) + stash → the engine's confirm is armed. Nothing is written.
        let r = crate::agent::block_on_test(handle.eval_prompt_resolved(
            "tree u1 switch summarize text-only ",
            HostKey::Enter,
            80,
        ));
        assert!(matches!(r, MenuResult::Continue(_, _)), "confirm armed");
        assert!(
            branch_summaries(&session).is_empty(),
            "the first Enter must only arm the confirm"
        );

        // Enter #2 → submit(): append the digest and move the leaf.
        let r = crate::agent::block_on_test(handle.eval_prompt_resolved(
            "tree u1 switch summarize text-only ",
            HostKey::Enter,
            80,
        ));
        assert!(matches!(r, MenuResult::DoneSuccess(..)));

        let written = branch_summaries(&session);
        assert_eq!(written.len(), 1, "exactly one BranchSummary written");
        let (parent, from_id, summary) = &written[0];
        // pi's `branchWithSummary`: parentId = fromId = the switch target, and
        // the entry is the new leaf.
        assert_eq!(
            parent, "u1",
            "parented at the switch target, not the abandoned spur"
        );
        assert_eq!(from_id, "u1", "fromId is the branch-from position");
        assert!(
            summary.contains("more work"),
            "the spur is what got summarized: {:?}",
            summary
        );
        let idx = session.index().blocking_lock();
        let summary_entry = idx
            .entries
            .values()
            .find(|e| matches!(e.content, SessionEntryContent::BranchSummary { .. }))
            .unwrap();
        assert_eq!(idx.leaf_id.as_deref(), Some(summary_entry.id.as_str()));
    }

    #[test]
    fn switch_summarize_context_carries_the_digest() {
        // The point of the pi placement: the very next turn sees the digest.
        let (app_state, session) = spur_app_state();
        let draft = crate::agent::compaction::CompactionDraft {
            summary: "Tried the wrong approach".into(),
            tokens_before: 0,
        };
        crate::agent::block_on_test(crate::agent::compaction::apply_branch_summary(
            &app_state, "u1", "a3", &draft,
        ))
        .expect("apply");
        let ctx = crate::agent::block_on_test(session.build_context()).expect("context");
        let joined = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                rig_core::message::Message::User { content } => match content.first() {
                    Some(rig_core::message::UserContent::Text(t)) => Some(t.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Tried the wrong approach"),
            "digest must reach the LLM context: {:?}",
            joined
        );
    }

    #[test]
    fn switch_summarize_refuses_a_stale_leaf() {
        let (app_state, session) = spur_app_state();
        let draft = crate::agent::compaction::CompactionDraft {
            summary: "digest".into(),
            tokens_before: 0,
        };
        // The leaf moved since generation (here: we assert against a leaf that
        // is not the current one) — the apply refuses rather than summarizing
        // the wrong span.
        let err = crate::agent::block_on_test(crate::agent::compaction::apply_branch_summary(
            &app_state,
            "u1",
            "some-other-leaf",
            &draft,
        ))
        .unwrap_err();
        assert!(
            err.contains("moved"),
            "expected a staleness error, got: {}",
            err
        );
        assert!(branch_summaries(&session).is_empty(), "nothing written");
        assert_eq!(
            futures::executor::block_on(session.get_leaf_id()),
            Some("a3".to_string()),
            "the leaf must not move on a rejected apply"
        );
    }

    #[test]
    fn switch_summarize_without_spur_reports_an_error() {
        // Target ahead on the current branch: nothing was abandoned, so there
        // is nothing to summarize (pi skips the entry in the same case).
        let (app_state, _session) = spur_app_state();
        let err = crate::agent::block_on_test(crate::agent::compaction::generate_branch_summary(
            &app_state,
            "a3",
            crate::agent::compaction::CompactSummaryMode::TextOnly,
            "",
        ))
        .unwrap_err();
        assert!(
            err.contains("ahead") || err.contains("nothing"),
            "got: {}",
            err
        );
    }

    #[test]
    fn switch_summarize_at_the_current_leaf_reports_an_error() {
        let (app_state, _session) = spur_app_state();
        let err = crate::agent::block_on_test(crate::agent::compaction::generate_branch_summary(
            &app_state,
            "a3",
            crate::agent::compaction::CompactSummaryMode::TextOnly,
            "",
        ))
        .unwrap_err();
        assert!(err.contains("Already at this entry"), "got: {}", err);
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
                assert!(
                    plan.rows[0].0.contains("[MODEL]"),
                    "model change should have [MODEL] prefix: {}",
                    plan.rows[0].0
                );
                assert!(
                    plan.rows[0].0.contains("openai/gpt-4o"),
                    "model change should show provider/model: {}",
                    plan.rows[0].0
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
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
                assert!(
                    plan.rows.is_empty(),
                    "tool-call-only assistant should be hidden"
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
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
        assert_eq!(
            resp.active_ids,
            vec!["u2".to_string()],
            "active_ids should contain the leaf entry"
        );
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
                vec![node(make_entry("u2", Some("a1"), "user", "second"), vec![])],
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
        assert_eq!(
            resp.active_ids,
            vec!["u2".to_string()],
            "active_ids should find leaf deep in tree"
        );
    }

    // --- Compact: two-phase prepare/submit tests ---

    /// True when the session has any `Compaction` entry.
    fn compaction_appended(session: &std::sync::Arc<crate::session::Session>) -> bool {
        let idx = session.index().blocking_lock();
        idx.entries.values().any(|e| {
            matches!(
                &e.content,
                crate::session::SessionEntryContent::Compaction { .. }
            )
        })
    }

    #[test]
    fn compact_manual_prepare_then_submit_appends_compaction() {
        // The native NodeEntry flow: prepare() generates + stashes the
        // draft and arms the confirm (Prepared — nothing written yet);
        // submit() applies the stashed draft.
        let app_state = test_app_state();
        let (_guard, session) =
            crate::agent::test_session_with_entries(vec![make_entry("u1", None, "user", "hi")]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactManualNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "manual".into(),
            ],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        assert!(
            matches!(result, SubmitResult::Prepared(_)),
            "prepare must arm the confirm with Prepared"
        );
        assert!(
            !compaction_appended(&session),
            "prepare must not write — only the confirm's submit does"
        );

        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        assert!(
            compaction_appended(&session),
            "manual compact must append a Compaction entry"
        );
    }

    #[test]
    fn compact_text_only_prepare_then_submit_appends_compaction() {
        let app_state = test_app_state();
        let (_guard, session) =
            crate::agent::test_session_with_entries(vec![make_entry("u1", None, "user", "hi")]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactTextNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "text-only".into(),
            ],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        assert!(matches!(result, SubmitResult::Prepared(_)));
        assert!(!compaction_appended(&session), "prepare must not write");

        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        assert!(
            compaction_appended(&session),
            "text-only compact must append a Compaction entry"
        );
    }

    #[test]
    fn compact_prompt_unknown_entry_errors_on_prepare() {
        // Validation lives in prepare(): the unknown target closes the
        // menu with the data-level message before any agent/LLM work.
        let app_state = test_app_state();
        let (_guard, session) =
            crate::agent::test_session_with_entries(vec![make_entry("other", None, "user", "hi")]);
        app_state.set_session(Some(session));
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "prompt".into(),
            ],
            suffix: "summarize the key decisions".into(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("unknown tree entry"), "msg: {msg}")
            }
            other => panic!(
                "expected unknown-entry error from prepare, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn compact_prompt_without_session_reports_error_on_prepare() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "prompt".into(),
            ],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("no active session"), "msg: {msg}")
            }
            other => panic!(
                "expected no-session error from prepare, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn compact_submit_without_prepare_is_rejected() {
        // submit() applies a *stashed* draft: with nothing prepared the
        // stash key misses and nothing is written.
        let app_state = test_app_state();
        let (_guard, session) =
            crate::agent::test_session_with_entries(vec![make_entry("u1", None, "user", "hi")]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactManualNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "manual".into(),
            ],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(
            matches!(result, SubmitResult::Error(_)),
            "submit without a prepared stash must error, got {:?}",
            std::mem::discriminant(&result)
        );
        assert!(!compaction_appended(&session), "nothing may be written");
    }

    #[test]
    fn compact_submit_rejects_stale_epoch() {
        // The stash is keyed by (target, epoch): a prepare at epoch 1
        // cannot feed a submit at epoch 2 — the data moved under us, so
        // submit errors and lets the next Enter regenerate.
        let app_state = test_app_state();
        let (_guard, session) =
            crate::agent::test_session_with_entries(vec![make_entry("u1", None, "user", "hi")]);
        app_state.set_session(Some(session.clone()));
        let mut node = TreeCompactManualNode::new(app_state.clone());
        let path_args = PathArgs {
            ids: vec![
                "tree".into(),
                "u1".into(),
                "compact".into(),
                "manual".into(),
            ],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        assert!(matches!(result, SubmitResult::Prepared(_)));

        let result = crate::agent::block_on_test(node.submit(2, &path_args));
        assert!(
            matches!(result, SubmitResult::Error(_)),
            "stale-epoch submit must error, got {:?}",
            std::mem::discriminant(&result)
        );
        assert!(!compaction_appended(&session), "stale draft not applied");
    }

    // --- Compact: full traversal ---

    #[test]
    fn compact_full_traversal_manual() {
        let tree = vec![node(
            make_entry("u1", None, "user", "first message"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "response"),
                vec![],
            )],
        )];
        let app_state = app_state_with_tree(tree);
        let (_guard, session) = crate::agent::test_session_with_entries(vec![make_entry(
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
        let r =
            futures::executor::block_on(handle.eval_prompt("tree u1 compact ", HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 4. Drill into manual.
        let r = futures::executor::block_on(handle.eval_prompt(
            "tree u1 compact manual ",
            HostKey::None,
            80,
        ));
        assert!(matches!(r, MenuResult::Continue(_, _)));

        // 5. Enter #1 → prepare(): generate (the editor accepts the
        //    draft as-is without a tty) + stash → Prepared arms the
        //    engine's confirm. Nothing is written yet.
        let r = crate::agent::block_on_test(handle.eval_prompt_resolved(
            "tree u1 compact manual ",
            HostKey::Enter,
            80,
        ));
        assert!(matches!(r, MenuResult::Continue(_, _)));
        assert!(
            !compaction_appended(&session),
            "the first Enter must only arm the confirm"
        );

        // 6. Enter #2 → submit(): apply the stashed draft.
        let r = crate::agent::block_on_test(handle.eval_prompt_resolved(
            "tree u1 compact manual ",
            HostKey::Enter,
            80,
        ));
        assert!(matches!(r, MenuResult::DoneSuccess(..)));
        assert!(
            compaction_appended(&session),
            "traversal must end in an appended Compaction"
        );
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
        let header =
            futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
        assert!(!header.is_empty());
        assert!(
            header[0].contains("Manual"),
            "header mentions manual: {}",
            header[0]
        );
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
        let header =
            futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
        assert!(
            header.iter().any(|l| l.contains("my custom prompt")),
            "header should contain the prompt text: {:?}",
            header
        );
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
        let header =
            futures::executor::block_on(node.header(&path_args, &RenderContext { term_width: 80 }));
        assert!(
            header.iter().any(|l| l.contains("default")),
            "header should mention default: {:?}",
            header
        );
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

        // All on the main line, so depth 0 and the bar at column 0; the
        // last reply turns the ladder off (doc/tree_visual_affordances.md
        // §5.1: the vertical runs per branch, ending at its last row).
        assert!(
            row_depths(&resp).iter().all(|(_, d)| *d == 0),
            "a linear session is all main line: {:?}",
            row_depths(&resp)
        );
        let rows = plain_rows(&resp);
        for (id, want) in [
            ("u1", "┃"),
            ("a1", "┣╸"),
            ("u2", "┃"),
            ("a2", "┣╸"),
            ("u3", "┃"),
            ("a3", "┗╸"),
        ] {
            assert_eq!(prefix_of(&plain_of(&rows, id)), want, "prefix of {id}");
        }
    }

    #[test]
    fn linear_session_indent_render_plan() {
        // The prefix must survive the whole pipeline into the render plan,
        // SGR and all.
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
                assert_eq!(prefix_of(&strip_sgr(&plan.rows[0].0)), "┃");
                assert_eq!(prefix_of(&strip_sgr(&plan.rows[1].0)), "┗╸");
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
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
                // Main line all the way: the bar is at column 0 for every
                // row, and only the final reply turns the ladder off.
                let expected = ["┃", "┣╸", "┃", "┣╸", "┃", "┗╸"];
                for (i, want) in expected.iter().enumerate() {
                    let got = prefix_of(&strip_sgr(&plan.rows[i].0));
                    assert_eq!(got, *want, "row {} prefix: {got:?}", i);
                }
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn indent_survives_table_render() {
        // Check that leading-space indent survives through the Table
        // rendering pipeline used by build_render_plan.
        use crate::slashdot_menu::table::{Align, Table};

        let cells = vec![
            "hello",     // indent 0
            "  world",   // indent 2
            "    model", // indent 4
        ];
        let mut t = Table::new(&[""])
            .with_aligns(&[Align::Left])
            .with_min_widths(&[20])
            .with_priorities(&[3])
            .set_gap(1);
        for c in &cells {
            t.add_row(&[c.to_string()]);
        }
        let rendered = t.render(80);
        assert_eq!(rendered.len(), 3);
        for (i, (cell, line)) in cells.iter().zip(rendered.iter()).enumerate() {
            let indent = line.len() - line.trim_start().len();
            let expected_indent = cell.len() - cell.trim_start().len();
            assert_eq!(
                indent, expected_indent,
                "row {} cell={:?} rendered={:?}: indent lost in table render",
                i, cell, line
            );
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
            eprintln!(
                "  row {}: id={:?} indent={} desc={:?}",
                i, row.id, indent, desc
            );
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
                // Spine rule: one child of a fork continues the parent's
                // line at the same column, the rest are spurs. mc1's three
                // children are all leaves, so heights tie and the recency
                // tie-break picks the newest (mc4) for the margin; mc2/mc3
                // open connectors and step right.
                let expected = [
                    "┃",    // u1
                    "┣╸",   // a1
                    "┃",    // u2
                    "┣╸",   // a2
                    "┃",    // mc1, a non-reply marker on the main line
                    "┗━━┓", // mc2, spur
                    "┗━━┓", // mc3, spur
                    "┻",    // mc4, the spine, and the branch's last row
                ];
                for (i, want) in expected.iter().enumerate() {
                    let got = prefix_of(&strip_sgr(&plan.rows[i].0));
                    assert_eq!(got, *want, "row {} prefix: {got:?}", i);
                }
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
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
        let rows = plain_rows(&resp);
        // A model-change marker is a non-reply row: it keeps the bar on the
        // main line, and the reply after it still gets its junction.
        for (id, want) in [
            ("u1", "┃"),
            ("a1", "┣╸"),
            ("mc1", "┃"),
            ("u2", "┃"),
            ("a2", "┗╸"),
        ] {
            assert_eq!(prefix_of(&plain_of(&rows, id)), want, "prefix of {id}");
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
        let rows = plain_rows(&resp);
        // The invisible tool-call/tool-result chain must not break the turn:
        // both rows are still the main line, and the reply still gets the
        // junction arm.
        assert_eq!(prefix_of(&plain_of(&rows, "u1")), "┃");
        assert_eq!(prefix_of(&plain_of(&rows, "a_text")), "┗╸");
    }

    // --- Spine indentation (doc/tree_indent_refinement.md) ---

    /// (entry id, spur depth) for every row, in listing order. The prefix
    /// owns the columns now, so depth is the structural fact to assert on.
    fn row_depths(resp: &DataResponse) -> Vec<(String, usize)> {
        resp.rows
            .iter()
            .map(|r| {
                let depth = match r.field_value("depth") {
                    Some(Value::Num(n)) => *n as usize,
                    other => panic!("depth field missing: {other:?}"),
                };
                (r.id.clone(), depth)
            })
            .collect()
    }

    /// (entry id, listing cell with SGR removed) — for asserting on the
    /// visible prefix.
    fn plain_rows(resp: &DataResponse) -> Vec<(String, String)> {
        resp.rows
            .iter()
            .map(|r| {
                (
                    r.id.clone(),
                    strip_sgr(&r.description.clone().unwrap_or_default()),
                )
            })
            .collect()
    }

    /// Drop SGR sequences from a styled cell. Fragments with no escape in
    /// them (the text before the first one) must be kept.
    fn strip_sgr(s: &str) -> String {
        s.split('\x1b')
            .map(|part| match part.split_once('m') {
                Some((_, rest)) => rest.to_string(),
                None => part.to_string(),
            })
            .collect()
    }

    /// The visible prefix of a row: the leading run of spaces and
    /// box-drawing glyphs before its text.
    fn prefix_of(plain: &str) -> String {
        plain
            .chars()
            .take_while(|c| *c == ' ' || ('\u{2500}'..='\u{257f}').contains(c))
            .collect()
    }

    /// The plain listing text of one row.
    fn plain_of(rows: &[(String, String)], id: &str) -> String {
        rows.iter()
            .find(|(rid, _)| rid == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| panic!("no row for {id}"))
    }

    fn depth_of(rows: &[(String, usize)], id: &str) -> usize {
        rows.iter()
            .find(|(rid, _)| rid == id)
            .map(|(_, i)| *i)
            .unwrap_or_else(|| panic!("no row for {id}"))
    }

    /// The `branch` ordinal of a row (0 = main line).
    fn branch_of(resp: &DataResponse, id: &str) -> u8 {
        let row = resp.rows.iter().find(|r| r.id == id).expect("row");
        match row.field_value("branch") {
            Some(Value::Num(n)) => *n as u8,
            other => panic!("branch field missing on {id}: {other:?}"),
        }
    }

    /// True if the row carries the `spur_start` marker field.
    fn is_spur_start(resp: &DataResponse, id: &str) -> bool {
        resp.rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.field_value("spur_start").is_some())
            .unwrap_or(false)
    }

    /// (id, depth) for every row of a tree, with the given leaf.
    fn depths_for(tree: Vec<SessionTreeNode>, leaf: Option<&str>) -> Vec<(String, usize)> {
        let app_state = app_state_with_tree(tree);
        app_state.set_leaf_id(leaf.map(|s| s.to_string()));
        let mut node = TreeListNode::new(app_state);
        let resp = futures::executor::block_on(node.data(0, &tree_path(&["tree"]))).expect("data");
        row_depths(&resp)
    }

    /// The raw `DataResponse` for a tree with the given leaf.
    fn data_for(tree: Vec<SessionTreeNode>, leaf: Option<&str>) -> Rc<DataResponse> {
        let app_state = app_state_with_tree(tree);
        app_state.set_leaf_id(leaf.map(|s| s.to_string()));
        let mut node = TreeListNode::new(app_state);
        futures::executor::block_on(node.data(0, &tree_path(&["tree"]))).expect("data")
    }

    /// The §1 session: a 6-row main flow, an experiment spur off `a1` and a
    /// 2-row side quest off `u2`. `spur_turns` grows the experiment spur so
    /// a test can check that the main flow does not move when it does.
    fn main_flow_with_spur(spur_turns: usize) -> Vec<SessionTreeNode> {
        // x1 → x2a → x2 → x3a → … one user/assistant pair per turn, built
        // back to front.
        let mut spur = node(
            make_entry("x1", Some("a1"), "user", "experiment prompt"),
            vec![node(
                make_entry("x2a", Some("x1"), "assistant", "experiment reply 1"),
                vec![],
            )],
        );
        for turn in 2..=spur_turns {
            let prev = spur.entry.id.clone();
            let cur = format!("x{turn}");
            spur = node(
                make_entry(&cur, Some(&prev), "user", &format!("experiment {turn}")),
                vec![node(
                    make_entry(
                        &format!("x{}a", turn),
                        Some(&cur),
                        "assistant",
                        &format!("experiment reply {turn}"),
                    ),
                    vec![],
                )],
            );
        }
        vec![node(
            make_entry("u1", None, "user", "kick off"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply 1"),
                vec![
                    spur,
                    node(
                        make_entry("u2", Some("a1"), "user", "second message"),
                        vec![
                            node(
                                make_entry("y1", Some("u2"), "user", "side quest"),
                                vec![node(
                                    make_entry("y2", Some("y1"), "assistant", "side reply"),
                                    vec![],
                                )],
                            ),
                            node(
                                make_entry("a2", Some("u2"), "assistant", "reply 2"),
                                vec![node(
                                    make_entry("u3", Some("a2"), "user", "third message"),
                                    vec![node(
                                        make_entry("a3", Some("u3"), "assistant", "reply 3"),
                                        vec![],
                                    )],
                                )],
                            ),
                        ],
                    ),
                ],
            )],
        )]
    }

    /// A tool-call-only assistant segment: an entry with no displayable text.
    fn tool_call_entry(id: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(|s| s.into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "tool_use", "name": "bash", "input": {}}]
                }),
            },
        }
    }

    /// A long fork-free chain — the pre-refinement rule and the spine rule
    /// agree on it, and building it exercises the iterative traversals.
    fn linear_chain(n: usize) -> Vec<SessionTreeNode> {
        let mut acc = node(make_entry("e0", None, "user", "last"), vec![]);
        for i in 1..n {
            let id = format!("e{i}");
            acc = node(
                make_entry(&id, None, "user", &format!("msg {i}")),
                vec![acc],
            );
        }
        vec![acc]
    }

    #[test]
    fn linear_session_indent_unchanged() {
        // No forks ⇒ every node is its parent's spine ⇒ column 0 forever,
        // at any length. 1000 entries also covers the stack-safety
        // constraint on the post-order pass.
        let rows = depths_for(linear_chain(1000), Some("e0"));
        assert_eq!(rows.len(), 1000);
        assert!(
            rows.iter().all(|(_, depth)| *depth == 0),
            "a fork-free chain must stay at depth 0"
        );
    }

    #[test]
    fn longest_branch_stays_at_left_margin() {
        // The main flow keeps the bar at column 0; both spurs draw their
        // connector from the bar above and step right. Text columns are
        // unchanged from the space-only layout: depth d puts text at
        // d * SPUR_STEP.
        let resp = data_for(main_flow_with_spur(1), Some("a3"));
        assert_eq!(resp.rows.len(), 10);
        let depths = row_depths(&resp);
        for (id, depth) in [
            ("u1", 0),
            ("u2", 0),
            ("a2", 0),
            ("u3", 0),
            ("a3", 0),
            ("x1", 1),
            ("x2a", 1),
            ("y1", 1),
            ("y2", 1),
        ] {
            assert_eq!(depth_of(&depths, id), depth, "depth of {id}");
        }
        let rows = plain_rows(&resp);
        for (id, want) in [
            ("u1", "┃"),
            ("a1", "┣╸"),
            ("x1", "┗━━┓"),   // spur: connector from the bar above
            ("x2a", "   ┗╸"), // …its reply turns the ladder off
            ("u2", "┃"),
            ("y1", "┗━━┓"),
            ("y2", "   ┗╸"),
            ("a2", "┣╸"),
            ("u3", "┃"),
            ("a3", "┗╸"),
        ] {
            assert_eq!(prefix_of(&plain_of(&rows, id)), want, "prefix of {id}");
        }
    }

    #[test]
    fn short_spur_does_not_outrank_long_flow() {
        // Growing a spur must not move the main flow — the spur is still
        // the shorter path, however many turns it gains.
        let flow = ["u1", "a1", "u2", "a2", "u3", "a3"];
        let short = plain_rows(&data_for(main_flow_with_spur(1), Some("a3")));
        for turns in 2..6 {
            let longer = plain_rows(&data_for(main_flow_with_spur(turns), Some("a3")));
            for id in flow {
                assert_eq!(
                    prefix_of(&plain_of(&longer, id)),
                    prefix_of(&plain_of(&short, id)),
                    "main-flow row {id} moved when the spur grew to {turns} turns"
                );
            }
        }
    }

    #[test]
    fn invisible_children_do_not_fork() {
        // The parent has two children but only one is visible: the
        // pre-spine rule counted `children.len() > 1` and indented by 4.
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![node(
                tool_call_entry("tc1", Some("u1")),
                vec![node(
                    make_entry("u2", Some("tc1"), "user", "second"),
                    vec![],
                )],
            )],
        )];
        let resp = data_for(tree, Some("u2"));
        let rows = plain_rows(&resp);
        let depths = row_depths(&resp);
        // Still the main line: the bar is at column 0 and no connector is
        // drawn. `╵` because it is the branch's last visible row.
        assert_eq!(depth_of(&depths, "u2"), 0, "{rows:?}");
        assert_eq!(prefix_of(&plain_of(&rows, "u2")), "┻", "{rows:?}");
    }

    #[test]
    fn spine_prefers_newest_on_height_tie() {
        // u2→a2 (older, 2 rows deep) vs v1 with two children (newer, also
        // 2 deep, 3 rows). Heights tie, so recency decides: the newer
        // branch holds the margin even though the older one is narrower.
        let tree = vec![node(
            make_entry("u1", None, "user", "kick off"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply 1"),
                vec![
                    node(
                        make_entry("u2", Some("a1"), "user", "older branch"),
                        vec![node(
                            make_entry("a2", Some("u2"), "assistant", "older reply"),
                            vec![],
                        )],
                    ),
                    node(
                        make_entry("v1", Some("a1"), "user", "newer branch"),
                        vec![
                            node(make_entry("w1", Some("v1"), "user", "first try"), vec![]),
                            node(make_entry("x1", Some("v1"), "user", "second try"), vec![]),
                        ],
                    ),
                ],
            )],
        )];
        let resp = data_for(tree, Some("v1"));
        let rows = plain_rows(&resp);
        assert_eq!(
            prefix_of(&plain_of(&rows, "v1")),
            "┃",
            "newest tie-winner keeps the margin"
        );
        assert_eq!(
            prefix_of(&plain_of(&rows, "w1")),
            "┗━━┓",
            "its spur connects from the bar above and steps right"
        );
        assert_eq!(
            prefix_of(&plain_of(&rows, "x1")),
            "┻",
            "its own spine stays on the line, and it is the branch's last row"
        );
        assert_eq!(
            prefix_of(&plain_of(&rows, "u2")),
            "┗━━┓",
            "older equal-depth branch is a spur"
        );
        assert_eq!(
            prefix_of(&plain_of(&rows, "a2")),
            "   ┗╸",
            "spur keeps the reply cue"
        );
    }

    #[test]
    fn layout_survives_navigation() {
        // The spine key does not read the leaf, so moving the leaf (a
        // `/tree <id> switch`) must not re-lay-out the listing.
        let with_leaf = depths_for(main_flow_with_spur(1), Some("a3"));
        let in_spur = depths_for(main_flow_with_spur(1), Some("x2a"));
        let at_root = depths_for(main_flow_with_spur(1), None);
        assert_eq!(with_leaf, in_spur, "navigation into a spur moved columns");
        assert_eq!(with_leaf, at_root, "no-leaf layout differs");

        // …and neither does it move the colours.
        let branches = |leaf: &str| -> Vec<(String, String)> {
            data_for(main_flow_with_spur(1), Some(leaf))
                .rows
                .iter()
                .map(|r| (r.id.clone(), r.description.clone().unwrap_or_default()))
                .collect()
        };
        assert_eq!(
            branches("a3"),
            branches("x2a"),
            "branch hues moved on switch"
        );
    }

    #[test]
    fn chain_beats_bush_recursively() {
        // §4.3: a 5-row chain vs a 7-row bush forking off the same parent.
        // The chain is deeper, so it holds the margin and no row to its
        // right is deeper than the row beside it.
        let tree = vec![node(
            make_entry("u1", None, "user", "kick off"),
            vec![node(
                make_entry("a1", Some("u1"), "assistant", "reply 1"),
                vec![
                    node(
                        make_entry("u2", Some("a1"), "user", "main 1"),
                        vec![node(
                            make_entry("a2", Some("u2"), "assistant", "main reply 1"),
                            vec![node(
                                make_entry("u3", Some("a2"), "user", "main 2"),
                                vec![node(
                                    make_entry("a3", Some("u3"), "assistant", "main reply 2"),
                                    vec![node(
                                        make_entry("u4", Some("a3"), "user", "main 3"),
                                        vec![],
                                    )],
                                )],
                            )],
                        )],
                    ),
                    node(
                        make_entry("v1", Some("a1"), "user", "explore"),
                        vec![
                            node(
                                make_entry("b1", Some("v1"), "user", "try 1"),
                                vec![node(
                                    make_entry("b2", Some("b1"), "assistant", "tried 1"),
                                    vec![],
                                )],
                            ),
                            node(
                                make_entry("c1", Some("v1"), "user", "try 2"),
                                vec![node(
                                    make_entry("c2", Some("c1"), "assistant", "tried 2"),
                                    vec![],
                                )],
                            ),
                            node(
                                make_entry("d1", Some("v1"), "user", "try 3"),
                                vec![node(
                                    make_entry("d2", Some("d1"), "assistant", "tried 3"),
                                    vec![],
                                )],
                            ),
                        ],
                    ),
                ],
            )],
        )];
        let resp = data_for(tree, Some("u4"));
        let depths = row_depths(&resp);
        let rows = plain_rows(&resp);
        // The chain holds the margin.
        for id in ["u2", "a2", "u3", "a3", "u4"] {
            assert_eq!(depth_of(&depths, id), 0, "chain row {id}");
        }
        // The bush is a spur: its root connects from the bar above, and two
        // of its own three children are spurs of *it* (a third level).
        assert_eq!(
            prefix_of(&plain_of(&rows, "v1")),
            "┗━━┓",
            "bush steps right"
        );
        for id in ["d1", "d2"] {
            assert_eq!(depth_of(&depths, id), 1, "bush spine {id}");
        }
        for id in ["b1", "b2", "c1", "c2"] {
            assert_eq!(depth_of(&depths, id), 2, "bush fork {id}");
        }
        // The chain is the deeper branch, which is why it won the margin.
        assert_eq!(depth_of(&depths, "u4"), 0);
    }

    #[test]
    fn layout_is_deterministic() {
        // Same tree ⇒ same layout (and same hues), from a cold cache.
        let render = |leaf: Option<&str>| -> Vec<String> {
            data_for(main_flow_with_spur(1), leaf)
                .rows
                .iter()
                .map(|r| r.description.clone().unwrap_or_default())
                .collect()
        };
        assert_eq!(render(Some("a3")), render(Some("a3")));
    }

    #[test]
    fn branch_ordinals_identify_each_spur() {
        // Each spur gets its own colour ordinal, and its replies inherit
        // it — a per-parent ordinal would make every single-spur branch the
        // same colour, which says nothing about identity.
        let resp = data_for(main_flow_with_spur(1), Some("a3"));
        assert_eq!(branch_of(&resp, "u1"), 0, "main line is the anchor");
        assert_eq!(branch_of(&resp, "a1"), 0);
        assert_eq!(branch_of(&resp, "u2"), 0);
        assert_eq!(branch_of(&resp, "a3"), 0);
        // Two spurs, two ordinals — and the reply inside a spur keeps it.
        assert_eq!(branch_of(&resp, "x1"), 1);
        assert_eq!(branch_of(&resp, "x2a"), 1, "reply inherits its spur's hue");
        assert_eq!(branch_of(&resp, "y1"), 2);
        assert_eq!(branch_of(&resp, "y2"), 2);
    }

    #[test]
    fn spur_start_marks_only_the_first_visible_row() {
        // The marker rides on the spur's first *visible* row; every other
        // row of the spur, and every main-line row, is unmarked.
        let resp = data_for(main_flow_with_spur(1), Some("a3"));
        assert!(is_spur_start(&resp, "x1"));
        assert!(is_spur_start(&resp, "y1"));
        for id in ["u1", "a1", "x2a", "u2", "a2", "u3", "a3", "y2"] {
            assert!(!is_spur_start(&resp, id), "{id} must not be a spur start");
        }
    }

    #[test]
    fn spur_start_skips_invisible_entries() {
        // A spur whose root is an invisible tool-call segment: the connector
        // must land on the first *visible* row of that spur, not on the
        // hidden one (which emits no row at all). The spur is the older
        // child, so the main line is chosen as the spine.
        let tree = vec![node(
            make_entry("u1", None, "user", "first"),
            vec![
                node(
                    tool_call_entry("tc1", Some("u1")),
                    vec![node(
                        make_entry("x1", Some("tc1"), "user", "spur row"),
                        vec![],
                    )],
                ),
                node(make_entry("u2", Some("u1"), "user", "main line"), vec![]),
            ],
        )];
        let resp = data_for(tree, Some("x1"));
        let rows = plain_rows(&resp);
        assert!(
            is_spur_start(&resp, "x1"),
            "the connector must land on the first visible spur row: {rows:?}"
        );
        assert_eq!(prefix_of(&plain_of(&rows, "x1")), "┗━━┓");
    }

    #[test]
    fn wash_fades_with_depth_and_ink_stays_black() {
        // Saturation is depth; the glyph is always black, so the lines stay
        // crisp however far out a branch is.
        let row = |depth: usize, branch: u8, bar: Bar| row_prefix(depth, branch, bar, false);
        let (deep, shallow) = (row(3, 1, Bar::Line), row(1, 1, Bar::Line));
        assert_ne!(
            deep, shallow,
            "a deeper spur must fade relative to its parent"
        );
        for r in [deep, shallow] {
            assert!(r.contains("38;2;0;0;0m"), "ink must stay black: {r:?}");
        }
        // Main line: the anchor, unfaded.
        assert!(row(0, 0, Bar::Line).contains(&format!(
            "48;2;{};{};{}m",
            ANCHOR_RGB.0, ANCHOR_RGB.1, ANCHOR_RGB.2
        )));
    }

    #[test]
    fn connector_is_neutral() {
        // Two adjacent sub-trees must not read as one pipe — the connector is
        // heavy box-drawing in a neutral ink, never a branch hue, and it
        // carries no wash of its own — the ink is now the only thing that
        // separates a connector from the bar it runs into.
        let prefix = row_prefix(1, 1, Bar::SpurStart, false);
        assert!(prefix.contains('\u{2517}'), "heavy elbow: {prefix:?}");
        assert!(prefix.contains('\u{2501}'), "heavy run: {prefix:?}");
        assert_eq!(
            prefix.matches("48;2;").count(),
            1,
            "only the bar cell may be washed: {prefix:?}"
        );
        assert!(prefix.contains(&format!(
            "38;2;{};{};{}m",
            CONNECTOR_RGB.0, CONNECTOR_RGB.1, CONNECTOR_RGB.2
        )));
        // The elbow sits in the parent's bar column, so it lines up with the
        // bar directly above it. Measured in *visible* columns: the SGR
        // bytes carry no width.
        let col_of = |needle: char| {
            let at = prefix.find(needle).expect("glyph");
            crate::slashdot_menu::table::display_width(&prefix[..at])
        };
        assert_eq!(col_of('\u{2517}'), bar_col(0), "elbow column");
        assert_eq!(col_of('\u{2513}'), bar_col(1), "bar column");
    }

    #[test]
    fn spine_indent_survives_render_plan() {
        // The prefix must reach the terminal through the table, SGR and all
        // (the generic leading-space case is covered by
        // `indent_survives_table_render` — this is the spine shape, where
        // the margin and the spur columns coexist).
        let app_state = app_state_with_tree(main_flow_with_spur(1));
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        let (plan, _) = expect_continue(futures::executor::block_on(handle.eval_prompt(
            "tree ",
            HostKey::None,
            72,
        )));
        let got: Vec<String> = plan
            .rows
            .iter()
            .map(|r| prefix_of(&strip_sgr(&r.0)))
            .collect();
        let want = vec![
            "┃",
            "┣╸",
            "┗━━┓",
            "   ┗╸",
            "┃",
            "┗━━┓",
            "   ┗╸",
            "┣╸",
            "┃",
            "┗╸",
        ];
        assert_eq!(got, want, "{plan:?}");
        // The wash must survive to the terminal too, not just the glyph —
        // and it is the *faded* hue (depth 1), not the raw palette entry.
        let wash = blend(SPUR_RGB[0], fade_at(1));
        assert!(
            plan.rows[2]
                .0
                .contains(&format!("48;2;{};{};{}m", wash.0, wash.1, wash.2)),
            "spur wash missing in the rendered row: {:?}",
            plan.rows[2].0
        );
    }

    /// Guard for the highlight path (found while scoping the follow-up
    /// visual work). Today the fuzzy path pre-truncates via
    /// `extract_snippet`, so `Table::fit` never sees an overflowing styled
    /// cell and this passes either way — it is here so that stays true,
    /// and so the follow-up colour work (which *will* put SGR in cells
    /// the snippet path can see) has a tripwire. The `fit`-level
    /// truncation bug and its fix are covered in `slashdot_menu::table`.
    #[test]
    fn fuzzy_filtered_row_is_not_cut_inside_a_style() {
        const OPEN: &str = "\x1b[1;35m";
        let tree = vec![node(
            make_entry(
                "u1",
                None,
                "user",
                "please refactor the tree indentation logic today",
            ),
            vec![node(
                make_entry(
                    "a1",
                    Some("u1"),
                    "assistant",
                    "done, the spine rule is in place",
                ),
                vec![],
            )],
        )];
        let app_state = app_state_with_tree(tree);
        let mut menu = build_tree_menu(&app_state);
        let mut handle = menu.get_handle();
        // Narrow terminal ⇒ the description column must ellipsize.
        let (plan, _) = expect_continue(futures::executor::block_on(handle.eval_prompt(
            "tree refactor",
            HostKey::None,
            40,
        )));
        let styled: Vec<&String> = plan
            .rows
            .iter()
            .map(|r| &r.0)
            .filter(|text| text.contains('\x1b'))
            .collect();
        assert!(!styled.is_empty(), "expected a highlighted row: {plan:?}");
        for text in styled {
            assert!(
                !text.trim_end().ends_with(OPEN),
                "row was cut inside the highlight, so the style bleeds: {text:?}"
            );
            assert!(
                text.contains("the tree indentation"),
                "escape bytes must not eat the column budget: {text:?}"
            );
        }
    }

    // --- drill_rewrite tests ---

    #[test]
    fn compact_node_drill_rewrite_redirects_to_tree() {
        let app_state = test_app_state();
        app_state.set_leaf_id(Some("msg-42".into()));
        let mut node = CompactNode {
            app_state: app_state.clone(),
        };
        let path = vec!["compact".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some("tree msg-42 compact ".to_string()));
    }

    #[test]
    fn compact_node_no_leaf_returns_none() {
        let app_state = test_app_state();
        // leaf_id is None (no session).
        let mut node = CompactNode {
            app_state: app_state.clone(),
        };
        let path = vec!["compact".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, None);
    }

    #[test]
    fn compact_prompt_drill_rewrite_fills_default() {
        let app_state = test_app_state();
        let mut node = TreeCompactPromptNode::new(app_state.clone());
        let path = vec![
            "tree".to_string(),
            "msg-1".to_string(),
            "compact".to_string(),
            "prompt".to_string(),
        ];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(
            result,
            Some(
                "tree msg-1 compact prompt Summarize the conversation up to this point."
                    .to_string()
            )
        );
    }

    /// Regression for two ways this render used to take the TUI down, both
    /// reachable by ordinary session text in the `/tree` entry detail:
    ///
    /// 1. It cut by byte offset against a *column* budget, so a multi-byte
    ///    char (em dash, `§`, CJK) could put the cut inside the char — panic.
    /// 2. A zero budget could not make progress (`&remaining[0..]`), so the
    ///    wrap loop never terminated and grew a `Vec` until the process was
    ///    OOM-killed. The prefix is 32 columns with a full ISO timestamp, so
    ///    any narrow terminal starved the first line of its budget.
    ///
    /// Stated as properties over a spread of inputs, so it covers the cases
    /// nobody thought to enumerate — not just the one that happened to fire.
    #[test]
    fn wrap_survives_multibyte_text() {
        const PREFIX: &str = "[USER 2025-01-01T00:00:00.000Z] ";
        const TEXTS: &[&str] = &[
            "—x",
            "—",
            "a — b",
            "alpha beta — gamma delta epsilon zeta eta theta iota kappa",
            "§6b A3.2 — no row in the API either",
            "plain ascii only, with a good many words to wrap around",
            "日本語のテキストはここにあります，折り返しの確認",
            "emoji 🎉 in the middle of a fairly long sentence here",
            "mixed — ascii 日本語 🎉 § text that runs on and on for a while",
            // No spaces at all: forces the hard-split path.
            "—a—b—c—d—e—f—g—h—i—j—k—l—m—n—o—p",
            "a\n—b\nccc — ddd",
            "",
        ];
        let pfx_width = UnicodeWidthStr::width(PREFIX);
        for text in TEXTS {
            // From narrower than the prefix itself (budget of zero/one) up to
            // comfortably wide, so the degenerate budgets are covered too.
            for width in 1..=60usize {
                // The call returning at all is the termination half of the
                // regression: the old loop span here, allocating without end.
                let out = wrap_with_prefix(PREFIX, text, width);
                assert!(!out.is_empty(), "empty output for {text:?}@{width}");
                // The empty-body `(empty)` marker is a sentinel, not wrappable
                // content, and may outgrow a narrow terminal.
                if !text.is_empty() {
                    // Width is only enforceable when the prefix leaves room
                    // for content; below that the row is prefix plus a sliver.
                    if width > pfx_width + 2 {
                        for line in &out {
                            assert!(
                                UnicodeWidthStr::width(line.as_str()) <= width,
                                "row overflowed width {width}: {line:?} (from {text:?})"
                            );
                        }
                    }
                    // No data loss: the text survives, modulo the prefix and
                    // the spaces a wrap point is allowed to consume.
                    let strip = |l: &str| {
                        l.strip_prefix(PREFIX)
                            .unwrap_or(l)
                            .trim_start_matches("  ")
                            .replace(' ', "")
                    };
                    let joined: String = out.iter().map(|l| strip(l)).collect();
                    let want: String = text
                        .split('\n')
                        .collect::<Vec<_>>()
                        .join("")
                        .replace(' ', "");
                    assert_eq!(joined, want, "text lost for {text:?}@{width}");
                }
            }
        }
    }
}
