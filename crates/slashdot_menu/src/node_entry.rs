//! Internal node entry: wraps a host `SlashDotNode` with children,
//! content cache, epoch state, and the v2.5 traversal algorithm.
//! Not exposed to the host.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::fields::{row_field_value, FieldSpace};
use crate::filter::compile_query;
use crate::grammar::{parse_dot_token, ModifierToken};
use crate::node_impl::{
    Args, HostKey, MenuResult, PathArgs, SubmitResult,
    SlashDotNode,
};
use crate::render::{RenderLine, RenderPlan, Style};
use crate::value::Value;

// --- Handle to a row in a child's DataResponse ---

/// A lightweight handle (child_idx, row_offset) into a child's
/// `Rc<DataResponse>.rows` vec. Copy, 8 bytes. Created by `refresh_data`
/// and `ensure_view`; resolved via `RowRef::resolve`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowRef {
    pub child_idx: u16,
    pub row_offset: u32,
}

impl RowRef {
    /// Resolve the handle against the data_rc snapshot. Returns `None` on
    /// a stale/foreign handle (§3.6: degrade/skip, never unwrap).
    fn resolve<'a>(&self, data: &'a [Option<Rc<crate::node_impl::DataResponse>>]) -> Option<&'a crate::fields::Row> {
        let resp = data.get(self.child_idx as usize)?.as_ref()?;
        resp.rows.get(self.row_offset as usize)
    }
}

// --- Content cache (data-derived) ---

struct ContentCache {
    /// Per-child data responses, parallel to self.children.
    /// On DataError, slot is None (stale rows kept).
    data_rc: Vec<Option<Rc<crate::node_impl::DataResponse>>>,
    /// Rows marked active by any child (§5.2). HashSet for O(1) lookup.
    active_ids: HashSet<String>,
    /// Row ID → child index. Rebuilt when any child's Rc changes.
    row_index: HashMap<String, u16>,
    /// Ranked row handles. Rebuilt on dirty or view_args change.
    ranked_rows: Vec<RowRef>,
    /// True when data changed since last ensure_view.
    dirty: bool,
    /// Cached rendered lines + active row indices. Rebuilt only when
    /// content changes (dirty) or terminal width changes. MoveCursor
    /// clones the Rc and only updates markers.
    render: RenderCache,
}

struct RenderCache {
    /// Rc-shared rendered lines — zero-cost clone on cursor moves.
    lines: Rc<Vec<RenderLine>>,
    /// Resolved active row indices (computed from active_ids + ranked_rows).
    active_rows: Vec<usize>,
    /// Number of sticky (non-scrollable) lines at the top.
    sticky_count: usize,
}

impl Default for ContentCache {
    fn default() -> Self {
        ContentCache {
            data_rc: Vec::new(),
            active_ids: HashSet::new(),
            row_index: HashMap::new(),
            ranked_rows: Vec::new(),
            dirty: true,
            render: RenderCache {
                lines: Rc::new(Vec::new()),
                active_rows: Vec::new(),
                sticky_count: 0,
            },
        }
    }
}

// --- Epoch state (user-intent, reset on new activation) ---

struct EpochState {
    /// Cursor row ID (survives re-rank). Resolved to index lazily.
    cursor_id: Option<String>,
    /// Cached cursor index (O(1) display lookup).
    cursor_idx: usize,
    /// Last rendered section for this node (view cache key + restore source).
    view_args: String,
    /// Set when this node drills into a child; consumed on bare-ID re-entry.
    should_restore_view_args: bool,
    /// Confirm dialog pending (§9).
    confirm_pending: bool,
    /// Terminal width for table layout (set per node_traverse call).
    term_width: usize,
}

impl Default for EpochState {
    fn default() -> Self {
        EpochState {
            cursor_id: None,
            cursor_idx: 0,
            view_args: String::new(),
            should_restore_view_args: false,
            confirm_pending: false,
            term_width: 80,
        }
    }
}

// --- Internal traversal result ---

// Re-use MenuResult directly (user confirmed: single type).
// The Continue variant carries (RenderPlan, new_prompt_string).

// --- Root node (no-op) ---

struct EmptyNode;

impl SlashDotNode for EmptyNode {}

// --- NodeEntry ---

pub(crate) struct NodeEntry {
    inner: Box<dyn SlashDotNode>,
    children: Vec<NodeEntry>,
    content: ContentCache,
    epoch: EpochState,
    /// Last epoch this node was traversed at. Triggers EpochState reset.
    cache_epoch: u64,
}

impl NodeEntry {
    /// Create a root node (metadata-only container, no data/submit).
    pub fn new_root() -> Self {
        NodeEntry {
            inner: Box::new(EmptyNode),
            children: Vec::new(),
            content: ContentCache::default(),
            epoch: EpochState::default(),
            cache_epoch: 0,
        }
    }

    /// Create a child node from a host `SlashDotNode` impl.
    pub fn new_child(inner: Box<dyn SlashDotNode>) -> Self {
        NodeEntry {
            inner,
            children: Vec::new(),
            content: ContentCache::default(),
            epoch: EpochState::default(),
            cache_epoch: 0,
        }
    }

    /// Append a child node. Returns `&mut Self` for chaining:
    /// ```ignore
    /// parent.add_child_node(a).add_child_node(b);
    /// ```
    pub fn add_child_node(&mut self, child: impl SlashDotNode + 'static) -> &mut Self {
        self.children.push(NodeEntry::new_child(Box::new(child)));
        self
    }

    /// Mutable access to the last child (for `ChildBuilder` in menu.rs).
    pub(crate) fn last_child_mut(&mut self) -> &mut NodeEntry {
        self.children.last_mut().expect("add_child_node must be called first")
    }

    // --- Main entry point ---

    /// Drive one key event through this node's subtree.
    ///
    /// `path` is the accumulated drill IDs (passed by value for cheap clone
    /// on drill). `prompt_suffix` is this node's section of the prompt
    /// (everything after the parent's matched ID, or the full prompt at root).
    pub fn node_traverse(
        &mut self,
        epoch: u64,
        key: HostKey,
        mut path: Vec<String>,
        prompt_suffix: &str,
        term_width: usize,
    ) -> MenuResult {
        // Phase 0: Epoch gate — reset per-node epoch state on new activation.
        if epoch != self.cache_epoch {
            self.epoch = EpochState::default();
            self.cache_epoch = epoch;
        }
        self.epoch.term_width = term_width;
        // eprintln!("In node_traverse {}", prompt_suffix);

        // Phase 1: Parse section (single parse pass).
        let trailing_space = prompt_suffix.ends_with(' ');
        let parsed = parse_section(prompt_suffix, &self.content.row_index);

        // Phase 2: Refresh data from children.
        let refresh_err = self.refresh_data(epoch);
        // If refresh rebuilt row_index, re-parse since IDs may have changed.
        // (Only matters on first call of a new epoch when row_index was
        // stale, or when dynamic data changes between calls.)
        let parsed = if refresh_err {
            parse_section(prompt_suffix, &self.content.row_index)
        } else {
            parsed
        };

        // Phase 3: Restore trigger (§5.1 step 1 — applies without Esc).
        if let Some((ref matched_id, _child_idx)) = parsed.matched_child {
            if parsed.tail.is_empty()
                && !trailing_space
                && self.epoch.should_restore_view_args
            {
                // Undo the drill: seed cursor on the matched ID's row,
                // replace the bare ID with stored view_args.
                let restore_to = self.epoch.view_args.clone();
                self.epoch.should_restore_view_args = false;
                // Seed cursor on the matched ID row.
                self.epoch.cursor_id = Some(matched_id.clone());
                self.seed_cursor_idx_on_id(matched_id);
                // Build view from restored section.
                let restore_args = parse_section(&restore_to, &self.content.row_index);
                self.ensure_view(epoch, &restore_args, &restore_to);
                let plan = self.build_render_plan();
                return MenuResult::Continue(plan, full_prompt(&path, &restore_to));
            }
        }

        // Phase 4: Dispatch.

        // Leaf node (no children).
        if self.children.is_empty() {
            return self.handle_leaf(epoch, key, &parsed, trailing_space, &path, prompt_suffix);
        }

        // Listing node: matched child with non-empty tail → recurse.
        // The key is forwarded — the child handles Tab/Enter as appropriate.
        if let Some((ref _matched_id, child_idx)) = parsed.matched_child {
            if !parsed.tail.is_empty() {
                path.push(_matched_id.clone());
                return self.children[child_idx].node_traverse(
                    epoch,
                    key,
                    path,
                    &parsed.tail,
                    term_width,
                );
            }
            // Matched child, empty tail, no restore → bare ID render (§2.5).
            // Fall through to show listing with cursor on the ID's row.
        }

        // Listing: show filtered listing, handle keys.
        self.handle_listing(epoch, key, &parsed, trailing_space, &path, prompt_suffix, term_width)
    }

    // --- Leaf handling ---

    fn handle_leaf(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        trailing_space: bool,
        path: &[String],
        prompt_suffix: &str,
    ) -> MenuResult {
        match key {
            HostKey::Enter => {
                // eprintln!("In leaf enter");
                if self.epoch.confirm_pending {
                    // Confirm dialog: yea → submit.
                    self.epoch.confirm_pending = false;
                    let path_args = build_path_args(path, parsed, prompt_suffix);
                    match self.inner.submit(epoch, &path_args) {
                        SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg),
                        SubmitResult::NewPath(p) => MenuResult::Continue(
                            RenderPlan::default(),
                            p,
                        ),
                        SubmitResult::Prepared => {
                            self.build_continue(path, prompt_suffix)
                        }
                    }
                } else {
                    let path_args = build_path_args(path, parsed, prompt_suffix);
                    match self.inner.prepare(epoch, &path_args) {
                        SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg),
                        SubmitResult::Prepared => {
                            self.epoch.confirm_pending = true;
                            self.build_continue(path, prompt_suffix)
                        }
                        SubmitResult::NewPath(p) => MenuResult::Continue(
                            RenderPlan::default(),
                            p,
                        ),
                    }
                }
            }
            HostKey::Esc => {
                if self.epoch.confirm_pending {
                    // Cancel confirm dialog.
                    self.epoch.confirm_pending = false;
                    self.build_continue(path, prompt_suffix)
                } else {
                    self.handle_esc(path, parsed, trailing_space, prompt_suffix)
                }
            }
            HostKey::None => self.build_continue(path, prompt_suffix),
            _ => self.build_continue(path, prompt_suffix),
        }
    }

    // --- Listing handling ---

    fn handle_listing(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        trailing_space: bool,
        path: &[String],
        prompt_suffix: &str,
        term_width: usize,
    ) -> MenuResult {
        // View args = everything before the matched tail (or entire section).
        let view_args_clean = if parsed.matched_child.is_some() {
            prompt_suffix[..prompt_suffix.len() - parsed.tail.len()]
                .trim_end()
                .to_string()
        } else {
            prompt_suffix.trim_end().to_string()
        };

        match key {
            HostKey::MoveCursor(delta) => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let n = self.content.ranked_rows.len();
                if n > 0 {
                    self.epoch.cursor_idx = (self.epoch.cursor_idx as isize + delta as isize)
                        .rem_euclid(n as isize) as usize;
                    if let Some(rref) = self.content.ranked_rows.get(self.epoch.cursor_idx) {
                        self.epoch.cursor_id = rref
                            .resolve(&self.content.data_rc)
                            .map(|row| row.id.clone());
                    }
                }
                let plan = self.plan_from_cache();
                MenuResult::Continue(plan, full_prompt(path, prompt_suffix))
            }
            HostKey::Tab => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let Some(active_id) = self.cursor_id_or_fallback() else {
                    return MenuResult::DoneError("cursor row vanished".into());
                };
                let child_idx = match self.content.row_index.get(&active_id) {
                    Some(&idx) => idx as usize,
                    None => return MenuResult::DoneError("cursor row not in index".into()),
                };
                self.epoch.view_args = view_args_clean;
                self.epoch.should_restore_view_args = true;
                let mut new_path = path.to_vec();
                new_path.push(active_id.clone());
                // Listing drill: child gets " " (the drill indicator).
                self.children[child_idx].node_traverse(
                    epoch,
                    HostKey::None,
                    new_path,
                    " ",
                    term_width,
                )
            }
            HostKey::Enter => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let Some(active_id) = self.cursor_id_or_fallback() else {
                    return MenuResult::DoneError("cursor row vanished".into());
                };
                let child_idx = match self.content.row_index.get(&active_id) {
                    Some(&idx) => idx as usize,
                    None => return MenuResult::DoneError("cursor row not in index".into()),
                };
                self.epoch.view_args = view_args_clean;
                self.epoch.should_restore_view_args = true;
                let mut new_path = path.to_vec();
                new_path.push(active_id.clone());
                // Listing drill: child gets " " (the drill indicator).
                self.children[child_idx].node_traverse(
                    epoch,
                    HostKey::Enter,
                    new_path,
                    " ",
                    term_width,
                )
            }
            HostKey::Esc => {
                self.handle_esc(path, parsed, trailing_space, prompt_suffix)
            }
            _ => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let plan = self.build_render_plan();
                MenuResult::Continue(plan, full_prompt(path, prompt_suffix))
            }
        }
    }

    // --- Esc ladder (§5.1) ---

    fn handle_esc(
        &mut self,
        path: &[String],
        parsed: &ParsedSection,
        trailing_space: bool,
        prompt_suffix: &str,
    ) -> MenuResult {
        // Step 2: clear refinement — terms, modifiers, selection dropped.
        if !parsed.positional.is_empty() || !parsed.modifiers.is_empty() || parsed.matched_child.is_some() {
            // Rewrite section to just the parent path (no terms/modifiers/selection).
            return MenuResult::Continue(RenderPlan::default(), full_prompt(path, ""));
        }

        // Step 3: strip trailing space → parent becomes responsible.
        if trailing_space {
            let trimmed = prompt_suffix.trim_end().to_string();
            return MenuResult::Continue(RenderPlan::default(), full_prompt(path, &trimmed));
        }

        // Step 4: nothing left at this level → close.
        MenuResult::DoneSuccess(None)
    }

    // --- Data refresh ---

    /// Refresh data from children. Returns true if any child's Rc changed.
    fn refresh_data(&mut self, epoch: u64) -> bool {
        let mut changed = false;
        let mut new_data: Vec<Option<Rc<crate::node_impl::DataResponse>>> =
            Vec::with_capacity(self.children.len());

        for (i, child) in self.children.iter_mut().enumerate() {
            let path_args = PathArgs {
                ids: Vec::new(),
                suffix: String::new(),
                args: Args::default(),
            };
            match child.inner.data(epoch, &path_args) {
                Ok(response) => {
                    let same = match self.content.data_rc.get(i) {
                        Some(Some(prev)) => Rc::ptr_eq(prev, &response),
                        _ => false,
                    };
                    if !same {
                        changed = true;
                    }
                    new_data.push(Some(response));
                }
                Err(_e) => {
                    // Keep stale rows; mark changed so view re-derives.
                    changed = true;
                    new_data.push(self.content.data_rc.get(i).cloned().flatten());
                }
            }
        }

        if changed {
            self.content.data_rc = new_data;
            // Rebuild row_index.
            self.content.row_index.clear();
            for (child_idx, slot) in self.content.data_rc.iter().enumerate() {
                if let Some(resp) = slot.as_ref() {
                    for row in &resp.rows {
                        // Row uniformity (§2.1): every row must have a
                        // non-empty id. Warn and skip in prod; debug_assert
                        // for extra safety in dev builds.
                        if row.id.is_empty() {
                            eprintln!(
                                "slashdot: skipping row with empty id at child {}",
                                child_idx
                            );
                            continue;
                        }
                        // Debug assert on duplicate insertion (dev catches bugs);
                        // in prod, last child wins (matches "later child
                        // overrides" convention).
                        if self.content.row_index.contains_key(&row.id) {
                            eprintln!(
                                "slashdot: duplicate row ID '{}' at child {}; \
                                 later child wins",
                                row.id, child_idx
                            );
                        }
                        self.content.row_index.insert(row.id.clone(), child_idx as u16);
                    }
                }
            }
            // Collect active IDs.
            self.content.active_ids.clear();
            for slot in self.content.data_rc.iter().filter_map(|s| s.as_ref()) {
                for id in &slot.active_ids {
                    self.content.active_ids.insert(id.clone());
                }
            }
            self.content.dirty = true;
        }

        changed
    }

    // --- View derivation (filter → fuzzy rank → sort) ---

    fn ensure_view(&mut self, _epoch: u64, parsed: &ParsedSection, view_args_str: &str) {
        if !self.content.dirty && self.epoch.view_args == view_args_str {
            return;
        }
        self.content.dirty = false;

        // Build field space (inferred from rows for now; §2.1 metadata
        // on parent comes in step 5).
        let field_space = self.field_space();

        // Step 1: Filter by modifiers.
        let modifier_tokens: Vec<ModifierToken> = parsed
            .modifiers
            .iter()
            .map(|(k, v)| ModifierToken {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect();
        let query = if modifier_tokens.is_empty() {
            crate::filter::Query::default()
        } else {
            compile_query(&modifier_tokens, &field_space)
        };

        // Build filtered RowRef handles. Mint handles alongside their rows
        // — the (RowRef, &Row) pair means no resolution needed in the
        // filter pass (§3.3). Rows with empty IDs are excluded.
        let filtered: Vec<(RowRef, &crate::fields::Row)> = self
            .content
            .data_rc
            .iter()
            .enumerate()
            .filter_map(|(ci, s)| s.as_ref().map(|rc| (ci as u16, rc)))
            .flat_map(|(ci, rc)| {
                rc.rows
                    .iter()
                    .enumerate()
                    .map(move |(off, row)| {
                        (
                            RowRef {
                                child_idx: ci,
                                row_offset: off as u32,
                            },
                            row,
                        )
                    })
            })
            .filter(|(_, row)| !row.id.is_empty())
            .filter(|(_, row)| {
                query
                    .filters
                    .iter()
                    .all(|(name, pred)| {
                        row_field_value(row, name)
                            .map(|v| pred.matches(&v))
                            .unwrap_or(false)
                    })
            })
            .collect();

        // Step 2: Fuzzy rank by positional terms.
        let terms = parsed.positional.join(" ");
        let mut ranked: Vec<RowRef> = crate::cursor::rank_row_refs(&filtered, &terms, None)
            .rows;

        // Step 3: Apply explicit sort (overrides fuzzy rank order).
        if let Some((ref sort_field, sort_dir)) = query.sort {
            self.sort_row_refs(&mut ranked, sort_field, sort_dir);
        }

        self.content.ranked_rows = ranked;
        self.epoch.view_args = view_args_str.to_owned();

        // Step 4: Seed / keep cursor (§3.4).
        self.seed_or_keep_cursor(parsed);
    }

    /// Sort ranked_rows by a field value. Missing-field rows go last.
    fn sort_row_refs(&self, rows: &mut Vec<RowRef>, field: &str, dir: crate::fields::SortDir) {
        // Partition into present/missing.
        let mut present: Vec<(Value, RowRef)> = Vec::new();
        let mut missing: Vec<RowRef> = Vec::new();
        for rref in rows.drain(..) {
            if let Some(row) = rref.resolve(&self.content.data_rc) {
                if let Some(val) = row_field_value(row, field) {
                    present.push((val, rref));
                } else {
                    missing.push(rref);
                }
            } else {
                missing.push(rref);
            }
        }
        present.sort_by(|(va, ra), (vb, rb)| {
            let ord = va.cmp_value(vb);
            if ord == std::cmp::Ordering::Equal {
                // Tiebreak by id.
                let id_a = ra.resolve(&self.content.data_rc).map(|r| r.id.as_str()).unwrap_or("");
                let id_b = rb.resolve(&self.content.data_rc).map(|r| r.id.as_str()).unwrap_or("");
                id_a.cmp(id_b)
            } else {
                ord
            }
        });
        if dir == crate::fields::SortDir::Desc {
            present.reverse();
        }
        *rows = present.into_iter().map(|(_, rref)| rref).collect();
        rows.extend(missing);
    }

    /// Seed or keep cursor after re-derive (§3.4).
    fn seed_or_keep_cursor(&mut self, parsed: &ParsedSection) {
        // If cursor_id is still present, keep it and refresh the index.
        if let Some(ref id) = self.epoch.cursor_id.clone() {
            if let Some(idx) = self.content.ranked_rows.iter().position(|r| {
                r.resolve(&self.content.data_rc)
                    .map_or(false, |row| row.id == *id)
            }) {
                self.epoch.cursor_idx = idx;
                return;
            }
        }

        // Re-seed: selection (if any) → first active → row 0.
        self.epoch.cursor_id = parsed
            .matched_child
            .as_ref()
            .map(|(id, _)| id.clone())
            .or_else(|| self.content.active_ids.iter().next().cloned())
            .or_else(|| {
                self.content
                    .ranked_rows
                    .first()
                    .and_then(|r| r.resolve(&self.content.data_rc))
                    .map(|row| row.id.clone())
            });
        self.epoch.cursor_idx = 0;
        // Re-resolve the new cursor_id to an index.
        if let Some(ref id) = self.epoch.cursor_id.clone() {
            self.seed_cursor_idx_on_id(id);
        }
    }

    /// Set cursor_idx to the position of the given ID in ranked_rows.
    fn seed_cursor_idx_on_id(&mut self, id: &str) {
        if let Some(idx) = self.content.ranked_rows.iter().position(|r| {
            r.resolve(&self.content.data_rc)
                .map_or(false, |row| row.id == id)
        }) {
            self.epoch.cursor_idx = idx;
        }
    }

    fn cursor_id_or_fallback(&self) -> Option<String> {
        self.content
            .ranked_rows
            .get(self.epoch.cursor_idx)
            .and_then(|r| r.resolve(&self.content.data_rc))
            .map(|row| row.id.clone())
    }

    // --- View helpers ---

    /// Build field space for this level: trait-declared fields if the
    /// node provides them, otherwise inferred from the row data.
    fn field_space(&self) -> FieldSpace {
        let declared = self.inner.fields();
        if !declared.is_empty() {
            return FieldSpace::new(declared);
        }
        self.infer_field_space()
    }

    /// Infer field space from the current data_rc.
    fn infer_field_space(&self) -> FieldSpace {
        let mut names: Vec<String> = Vec::new();
        for slot in self.content.data_rc.iter().filter_map(|s| s.as_ref()) {
            for row in &slot.rows {
                for (k, _) in &row.fields {
                    if !names.contains(k) {
                        names.push(k.clone());
                    }
                }
            }
        }
        let specs = names
            .iter()
            .map(|k| {
                let is_num = self
                    .content
                    .data_rc
                    .iter()
                    .filter_map(|s| s.as_ref())
                    .flat_map(|resp| &resp.rows)
                    .find_map(|r| r.field_value(k))
                    .map(|v| matches!(v, Value::Num(_)))
                    .unwrap_or(false);
                crate::fields::FieldSpec::new(k.clone(), is_num)
            })
            .collect();
        FieldSpace::new(specs)
    }

    fn build_continue(&self, path: &[String], prompt_suffix: &str) -> MenuResult {
        MenuResult::Continue(RenderPlan::default(), full_prompt(path, prompt_suffix))
    }

    // --- Render plan construction (§5) ---

    /// Rebuild lines and cache. Called when content is dirty or on
    /// non-cursor key events.
    ///
    /// Panel structure (§6):
    /// 1. header() lines — sticky, non-selectable
    /// 2. Table header row (if columns + data) — sticky
    /// 3. Data rows — scrollable, with > / * markers
    ///
    /// The host uses `sticky_count` to decide vertical truncation when
    /// the terminal is too short for all sticky + data lines.
    fn build_render_plan(&mut self) -> RenderPlan {
        let mut lines = Vec::new();
        let mut sticky_count = 0usize;

        // Pre-compute: do we have data rows and table columns?
        let has_data = !self.content.ranked_rows.is_empty();
        let columns = self.inner.columns();
        let field_space = self.field_space();
        let has_columns = !columns.is_empty() || !field_space.fields.is_empty();
        let has_table_header = has_columns && has_data;

        // 1. Header lines from the node's header() method (sticky).
        let empty_args = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Args::default(),
        };
        let header = self.inner.header(&empty_args);
        for h in &header {
            lines.push(RenderLine {
                text: h.clone(),
                style: Style::HEADER,
                is_header: true,
                is_marker_row: false,
                option: usize::MAX,
            });
            sticky_count += 1;
        }

        // 2. Table header row (if columns declared and there's data, sticky).
        // 2+3. Table rendering: header + data rows.
        let term_width: usize = self.epoch.term_width;
        {
            use crate::table::{Align, Table};
            // Column field names (what to read from each row).
            let col_field_names: Vec<String> = if !columns.is_empty() {
                columns.iter().map(|c| c.field.clone()).collect()
            } else {
                let mut names = vec!["id".to_string()];
                names.extend(field_space.fields.iter().map(|f| f.name.clone()));
                if names.len() == 1 {
                    names.push("description".to_string());
                }
                names
            };
            // Header labels (what to display).
            let header_labels: Vec<&str> = if !columns.is_empty() {
                columns.iter().map(|c| c.header.as_str()).collect()
            } else {
                col_field_names.iter().map(|s| s.as_str()).collect()
            };
            let mut t = Table::new(&header_labels).set_gap(2);
            if !columns.is_empty() {
                let aligns: Vec<Align> = columns.iter().map(|c| c.align).collect();
                let mins: Vec<usize> = columns.iter().map(|c| c.min_width).collect();
                let prios: Vec<usize> = columns.iter().map(|c| c.priority).collect();
                t = t.with_aligns(&aligns).with_min_widths(&mins).with_priorities(&prios);
            }
            for rref in &self.content.ranked_rows {
                let mut cells: Vec<String> = Vec::with_capacity(col_field_names.len());
                for k in &col_field_names {
                    cells.push(
                        rref.resolve(&self.content.data_rc)
                            .and_then(|row| {
                                row.field_display(k).or_else(|| {
                                    crate::fields::row_field_value(row, k).map(|v| match v {
                                        crate::value::Value::Str(s) => s,
                                        crate::value::Value::Num(n) => crate::value::fmt_num(n),
                                    })
                                })
                            })
                            .unwrap_or_default(),
                    );
                }
                t.add_row(&cells);
            }
            // Render header through the Table (aligned with data columns).
            if has_table_header {
                let header_text = t.render_header(term_width.saturating_sub(2));
                lines.push(RenderLine {
                    text: header_text,
                    style: Style::HEADER,
                    is_header: true,
                    is_marker_row: false,
                    option: usize::MAX,
                });
                sticky_count += 1;
            }
            // Render data rows.
            let row_texts = t.render_multiline(term_width.saturating_sub(2));
            for (i, text) in row_texts.iter().enumerate() {
                for (j, sub) in text.split('\n').enumerate() {
                    lines.push(RenderLine {
                        text: sub.to_string(),
                        style: Style::PANEL,
                        is_header: false,
                        is_marker_row: j == 0,
                        option: i,
                    });
                }
            }
        }

        // Resolve active row indices.
        let active_rows: Vec<usize> = self
            .content
            .ranked_rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.resolve(&self.content.data_rc)
                    .map_or(false, |row| self.content.active_ids.contains(&row.id))
            })
            .map(|(i, _)| i)
            .collect();

        let lines_rc = Rc::new(lines);

        // Cache the lines, active_rows, and sticky_count.
        self.content.render.lines = Rc::clone(&lines_rc);
        self.content.render.active_rows = active_rows.clone();
        self.content.render.sticky_count = sticky_count;

        let active = if active_rows.is_empty() {
            Vec::new()
        } else {
            active_rows
        };

        RenderPlan {
            lines: lines_rc,
            selected: if self.content.ranked_rows.is_empty() {
                None
            } else {
                Some(self.epoch.cursor_idx)
            },
            active_rows: active,
            sticky_count,
        }
    }

    /// Cursor-move fast path: clone the cached lines Rc, only update
    /// markers. Zero allocation for the lines themselves.
    fn plan_from_cache(&self) -> RenderPlan {
        RenderPlan {
            lines: Rc::clone(&self.content.render.lines),
            selected: if self.content.ranked_rows.is_empty() {
                None
            } else {
                Some(self.epoch.cursor_idx)
            },
            active_rows: self.content.render.active_rows.clone(),
            sticky_count: self.content.render.sticky_count,
        }
    }
}

// --- Section parsing (single parse pass, §2.1) ---

/// Result of parsing a prompt section.
struct ParsedSection {
    /// Dot modifiers: (key, raw_value) pairs. Owned strings.
    modifiers: Vec<(String, String)>,
    /// Positional tokens (fuzzy terms / leaf args).
    positional: Vec<String>,
    /// Matched child: (row_id, child_index) if a word matched a row ID.
    matched_child: Option<(String, usize)>,
    /// Everything after the matched child's word (including leading space).
    tail: String,
}

/// Parse a prompt section one word at a time (single pass, §2.1).
///
/// Words are classified left-to-right:
/// - `.key:value` or `.key` → modifier
/// - word in row_index → matched child (section terminates)
/// - otherwise → positional (fuzzy term)
///
/// The first word matching a row ID terminates the section; everything
/// after it is the `tail` for the child.
fn parse_section(section: &str, row_index: &HashMap<String, u16>) -> ParsedSection {
    let mut modifiers: Vec<(String, String)> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut matched_child: Option<(String, usize)> = None;
    let mut pos = 0;

    while pos < section.len() {
        // Skip whitespace.
        while pos < section.len() && section.as_bytes()[pos] == b' ' {
            pos += 1;
        }
        if pos >= section.len() {
            break;
        }

        // Find end of word.
        let start = pos;
        while pos < section.len() && section.as_bytes()[pos] != b' ' {
            pos += 1;
        }
        let word = &section[start..pos];

        // Check if this word is a row ID.
        if let Some(&child_idx) = row_index.get(word) {
            matched_child = Some((word.to_string(), child_idx as usize));
            break;
        }

        // Check if dot modifier.
        if let Some(dot) = parse_dot_token(word) {
            match dot {
                crate::grammar::DotToken::Key(key) => {
                    modifiers.push((key, String::new()));
                }
                crate::grammar::DotToken::NameValue { key, value } => {
                    modifiers.push((key, value));
                }
            }
        } else {
            positional.push(word.to_string());
        }
    }

    let tail = match &matched_child {
        Some(_) => section[pos..].to_string(),
        None => String::new(),
    };

    ParsedSection {
        modifiers,
        positional,
        matched_child,
        tail,
    }
}

/// Build PathArgs for a leaf node's prepare/submit call.
fn build_path_args(path: &[String], parsed: &ParsedSection, suffix: &str) -> PathArgs {
    PathArgs {
        ids: path.to_vec(),
        suffix: suffix.to_string(),
        args: Args {
            modifiers: parsed.modifiers.clone(),
            positional: parsed.positional.clone(),
        },
    }
}

/// Construct the full prompt for the host from the drilled path and
/// the current level's suffix. For the root (empty path), this is
/// just the suffix.
fn full_prompt(path: &[String], suffix: &str) -> String {
    if path.is_empty() {
        suffix.to_string()
    } else {
        let base = path.join(" ");
        if suffix.is_empty() {
            base
        } else if suffix.starts_with(' ') {
            format!("{}{}", base, suffix)
        } else {
            format!("{} {}", base, suffix)
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_impl::DataResponse;
    use std::rc::Rc;

    /// A trivial command node: returns a fixed set of rows.
    struct CmdNode {
        rows: Vec<crate::fields::Row>,
        active: Vec<String>,
    }

    impl CmdNode {
        fn new(id: &str) -> Self {
            CmdNode {
                rows: vec![crate::fields::Row::new(id).desc(format!("{} command", id))],
                active: Vec::new(),
            }
        }

        fn with_active(id: &str, active: &str) -> Self {
            CmdNode {
                rows: vec![crate::fields::Row::new(id).desc(format!("{} command", id))],
                active: vec![active.to_string()],
            }
        }
    }

    impl SlashDotNode for CmdNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: self.rows.clone(),
                active_ids: self.active.clone(),
            }))
        }
    }

    /// A leaf node that records what it received on submit.
    struct LeafNode {
        last_submit: std::cell::RefCell<Option<String>>,
    }

    impl LeafNode {
        fn new() -> Self {
            LeafNode {
                last_submit: std::cell::RefCell::new(None),
            }
        }
    }

    impl SlashDotNode for LeafNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![crate::fields::Row::new("leaf")],
                active_ids: Vec::new(),
            }))
        }

        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            *self.last_submit.borrow_mut() = Some(
                path_args
                    .args
                    .positional
                    .first()
                    .cloned()
                    .unwrap_or_default(),
            );
            SubmitResult::Done(None)
        }
    }

    /// A node with a static header.
    struct HeaderLeaf {
        text: String,
    }

    impl SlashDotNode for HeaderLeaf {
        fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
            vec![self.text.clone()]
        }

        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![crate::fields::Row::new("hleaf")],
                active_ids: Vec::new(),
            }))
        }
    }

    /// A node with dynamic rows for testing fuzzy + filter + sort.
    struct DynamicNode {
        rows: Vec<crate::fields::Row>,
    }

    impl SlashDotNode for DynamicNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: self.rows.clone(),
                active_ids: Vec::new(),
            }))
        }
    }

    fn root_with_cmds(cmds: Vec<CmdNode>) -> NodeEntry {
        let mut root = NodeEntry::new_root();
        for cmd in cmds {
            root.children.push(NodeEntry::new_child(Box::new(cmd)));
        }
        root
    }

    fn ep() -> u64 { 1 }

    #[test]
    fn root_lists_commands() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "");
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("alpha")));
                assert!(row_texts.iter().any(|t| t.contains("beta")));
                assert_eq!(plan.selected, Some(0));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn fuzzy_filter_at_root() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "alp", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("alpha")));
                assert!(!row_texts.iter().any(|t| t.contains("beta")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn bare_id_shows_parent_listing() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "alpha", 80);
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "alpha");
                let row_count = plan.lines.iter().filter(|l| !l.is_header).count();
                assert_eq!(row_count, 2);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn tab_drills_into_child() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep());
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(_, prompt) => assert_eq!(prompt, "alpha "),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn move_cursor_wraps() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b"), CmdNode::new("c")]);
        root.refresh_data(ep());
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        let result = root.node_traverse(ep(), HostKey::MoveCursor(1), Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(1)),
            other => panic!("expected Continue, got {:?}", other),
        }
        let result = root.node_traverse(ep(), HostKey::MoveCursor(-1), Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(0)),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn active_marker_present() {
        let mut root = root_with_cmds(vec![
            CmdNode::new("a"),
            CmdNode::with_active("b", "b"),
        ]);
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.active_rows, vec![1]);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn esc_at_root_closes() {
        let mut root = root_with_cmds(vec![CmdNode::new("a")]);
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::Esc, Vec::new(), "", 80);
        assert!(matches!(result, MenuResult::DoneSuccess(None)));
    }

    /// A root-like node with children AND a header.
    struct HeadedRoot {
        rows: Vec<crate::fields::Row>,
        header_text: String,
    }

    impl SlashDotNode for HeadedRoot {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: self.rows.clone(),
                active_ids: Vec::new(),
            }))
        }
        fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
            vec![self.header_text.clone()]
        }
    }

    #[test]
    fn header_rendered() {
        let mut root = NodeEntry::new_child(Box::new(HeadedRoot {
            rows: vec![crate::fields::Row::new("item")],
            header_text: "Help text here".to_string(),
        }));
        // Give it a child so it's a listing node (not a leaf).
        root.children.push(NodeEntry::new_child(Box::new(CmdNode::new("item"))));
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let headers: Vec<&str> = plan.lines.iter()
                    .filter(|l| l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(headers.contains(&"Help text here"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // --- Section parse tests ---

    #[test]
    fn parse_empty_section() {
        let ri = HashMap::new();
        let p = parse_section("", &ri);
        assert!(p.modifiers.is_empty());
        assert!(p.positional.is_empty());
        assert!(p.matched_child.is_none());
    }

    #[test]
    fn parse_modifiers_only() {
        let ri = HashMap::new();
        let p = parse_section(".sort:price .tier:free", &ri);
        assert_eq!(p.modifiers.len(), 2);
        assert_eq!(p.modifiers[0].0, "sort");
        assert_eq!(p.modifiers[0].1, "price");
        assert_eq!(p.modifiers[1].0, "tier");
        assert_eq!(p.modifiers[1].1, "free");
    }

    #[test]
    fn parse_positional_and_modifier_interleave() {
        let ri = HashMap::new();
        let p = parse_section("fuzzy .sort:price more", &ri);
        assert_eq!(p.positional, vec!["fuzzy", "more"]);
        assert_eq!(p.modifiers.len(), 1);
        assert_eq!(p.modifiers[0].0, "sort");
    }

    #[test]
    fn parse_matched_child_terminates() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse_section("model extra words", &ri);
        assert_eq!(p.matched_child.as_ref().map(|(id, _)| id.as_str()), Some("model"));
        assert_eq!(p.tail, " extra words");
        assert!(p.positional.is_empty());
    }

    #[test]
    fn parse_no_trailing_space_means_empty_tail() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse_section("model", &ri);
        assert_eq!(p.matched_child.as_ref().map(|(id, _)| id.as_str()), Some("model"));
        assert!(p.tail.is_empty());
    }

    #[test]
    fn parse_fuzzy_terms_before_id() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse_section("fuzzy model", &ri);
        assert_eq!(p.positional, vec!["fuzzy"]);
        assert_eq!(p.matched_child.as_ref().map(|(id, _)| id.as_str()), Some("model"));
        assert!(p.tail.is_empty());
    }

    #[test]
    fn bare_dot_modifier() {
        let ri = HashMap::new();
        let p = parse_section(".sort", &ri);
        assert_eq!(p.modifiers.len(), 1);
        assert_eq!(p.modifiers[0].0, "sort");
        assert_eq!(p.modifiers[0].1, "");
    }

    // --- Epoch reset test ---

    #[test]
    fn epoch_reset_clears_cursor() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b")]);
        root.refresh_data(ep());
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        let _ = root.node_traverse(ep(), HostKey::MoveCursor(1), Vec::new(), "", 80);
        assert_eq!(root.epoch.cursor_idx, 1);
        // New epoch resets.
        let _ = root.node_traverse(ep() + 10, HostKey::None, Vec::new(), "", 80);
        assert_eq!(root.epoch.cursor_idx, 0);
    }

    // --- Esc clears filter ---

    #[test]
    fn esc_clears_filter() {
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                crate::fields::Row::new("m1").desc("first option"),
                crate::fields::Row::new("m2").desc("second option"),
            ],
        })));
        root.refresh_data(ep());
        // "first" is in m1's description but not a row ID → fuzzy filter.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "first", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_count = plan.lines.iter().filter(|l| !l.is_header).count();
                assert_eq!(row_count, 1, "fuzzy 'first' should narrow to 1 row");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        // Esc on filtered view clears to empty (root close or clear).
        let result = root.node_traverse(ep(), HostKey::Esc, Vec::new(), "first", 80);
        match result {
            MenuResult::Continue(_, _) | MenuResult::DoneSuccess(_) => {}
            other => panic!("expected Continue or DoneSuccess on Esc, got {:?}", other),
        }
    }

    // --- Fuzzy rank orders best first ---

    #[test]
    fn fuzzy_rank_orders_best_first() {
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                crate::fields::Row::new("p:gpt4o").desc("GPT-4o flagship"),
                crate::fields::Row::new("c:opus").desc("Claude opus"),
                crate::fields::Row::new("p:gpt4-mini").desc("GPT-4 mini fast"),
            ],
        })));
        root.refresh_data(ep());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "gpt", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("gpt4o")));
                assert!(row_texts.iter().any(|t| t.contains("gpt4-mini")));
                assert!(!row_texts.iter().any(|t| t.contains("opus")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // --- Step 5: Merged child set tests ---

    /// A leaf that records the child_index it was dispatched from.
    struct RecordingLeaf {
        submitted_via: std::cell::RefCell<Option<String>>,
    }

    impl RecordingLeaf {
        fn new() -> Self {
            RecordingLeaf {
                submitted_via: std::cell::RefCell::new(None),
            }
        }
        fn submitted_via(&self) -> Option<String> {
            self.submitted_via.borrow().clone()
        }
    }

    impl SlashDotNode for RecordingLeaf {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![crate::fields::Row::new("action")],
                active_ids: Vec::new(),
            }))
        }
        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            *self.submitted_via.borrow_mut() =
                Some(path_args.ids.last().cloned().unwrap_or_default());
            SubmitResult::Done(None)
        }
    }

    #[test]
    fn mixed_children_merged_listing() {
        // Sandbox shape: static "add" child + dynamic binding rows.
        let mut root = NodeEntry::new_root();

        // Static child: returns "add" row.
        root.children.push(NodeEntry::new_child(Box::new(CmdNode::new("add"))));

        // Dynamic child: returns "bind1", "bind2" rows.
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                crate::fields::Row::new("bind1").desc("binding one"),
                crate::fields::Row::new("bind2").desc("binding two"),
            ],
        })));

        root.refresh_data(ep());

        // Both children's rows should appear in the listing.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("add")));
                assert!(row_texts.iter().any(|t| t.contains("bind1")));
                assert!(row_texts.iter().any(|t| t.contains("bind2")));
                assert_eq!(row_texts.len(), 3, "should have exactly 3 rows");
            }
            other => panic!("expected Continue, got {:?}", other),
        }

        // Verify row_index maps correctly.
        assert_eq!(root.content.row_index.get("add"), Some(&0));
        assert_eq!(root.content.row_index.get("bind1"), Some(&1));
        assert_eq!(root.content.row_index.get("bind2"), Some(&1));
    }

    #[test]
    fn mixed_children_tab_drill_dispatches_correct_child() {
        let mut root = NodeEntry::new_root();
        let add_leaf = NodeEntry::new_child(Box::new(RecordingLeaf::new()));
        let bind_leaf = NodeEntry::new_child(Box::new(RecordingLeaf::new()));

        // add_child returns a static "add" row.
        let mut add_cmd = NodeEntry::new_child(Box::new(CmdNode::new("add")));
        add_cmd.children.push(add_leaf);

        // bind_child returns "bind1" row.
        let mut bind_cmd = NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![crate::fields::Row::new("bind1").desc("binding")],
        }));
        bind_cmd.children.push(bind_leaf);

        root.children.push(add_cmd);
        root.children.push(bind_cmd);
        root.refresh_data(ep());

        // Tab on "add" → drills into add_cmd → shows "action" leaf row.
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        // Move cursor to "add" (it's first) and Tab.
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(_, prompt) => assert!(prompt.starts_with("add ")),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Now at "add ", the add_cmd node shows its leaf child "action".
        // Tab into the leaf.
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "add ", 80);
        match result {
            MenuResult::Continue(_, prompt) => assert!(prompt.starts_with("add action ")),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Enter on "add action " → submit.
        let result = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "add action ", 80);
        match result {
            MenuResult::DoneSuccess(_) => {}
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn mixed_children_fuzzy_filter_crosses_boundary() {
        // Fuzzy filter should narrow across both children.
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(CmdNode::new("add"))));
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                crate::fields::Row::new("bind1").desc("binding one"),
                crate::fields::Row::new("bind2").desc("binding two"),
            ],
        })));
        root.refresh_data(ep());

        // "bind" fuzzy-matches both binding rows but not "add".
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "bind", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(!row_texts.iter().any(|t| t.contains("add")));
                assert!(row_texts.iter().any(|t| t.contains("bind1")));
                assert!(row_texts.iter().any(|t| t.contains("bind2")));
                assert_eq!(row_texts.len(), 2);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn empty_id_rows_skipped_with_warning() {
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                crate::fields::Row::new("").desc("no id"),       // empty id
                crate::fields::Row::new("valid").desc("has id"),
            ],
        })));
        root.refresh_data(ep());

        // Only "valid" should appear.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert_eq!(row_texts.len(), 1);
                assert!(row_texts[0].contains("valid"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }
}
