//! Internal node entry: wraps a host `SlashDotNode` with children,
//! content cache, epoch state, and the v2.5 traversal algorithm.
//! Not exposed to the host.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::fields::{row_field_value, FieldSpace};
use crate::filter::compile_query;
use crate::grammar::ModifierToken;
use crate::node_impl::{
    MenuResult, PathArgs, RenderContext, SubmitResult,
    SlashDotNode,
};
use crate::render::{LogicalLine, RenderPlan};
use crate::value::Value;
use std::ops::Range;

/// Simplified key events the host sends to the engine.
pub enum HostKey {
    Tab,
    Enter,
    Esc,
    /// Move cursor by delta (+1 for down, -1 for up, ±N for pgup/pgdn).
    MoveCursor(i32),
    /// No key action — used when Tab drills but does not forward a key.
    None,
    /// Drilled into from a parent listing via Enter. Leaf nodes submit
    /// on this (unless `drill_rewrite` intercepts); listing nodes
    /// ignore it (stops recursion).
    DrillEnter,
    /// Drilled into from a parent listing via Tab. Listing and leaf
    /// nodes can use this to trigger `drill_rewrite`.
    DrillTab,
}

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
    /// Table column header. Single line. Changes on dirty/resize only.
    table_header: Option<crate::render::LogicalLine>,
    /// Scrollable data rows. Rc-shared — same pointer on cursor moves.
    rows: std::rc::Rc<Vec<crate::render::LogicalLine>>,
    /// Resolved active row indices (computed from active_ids + ranked_rows).
    active_rows: Vec<usize>,
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
                table_header: None,
                rows: Rc::new(Vec::new()),
                active_rows: Vec::new(),
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
    /// True when the user explicitly moved the cursor (arrow keys).
    /// When false, cursor re-seeds from ranked_rows on view changes
    /// (filter, sort, data refresh). When true, cursor persists.
    cursor_user_moved: bool,
    /// Last rendered section for this node (view cache key + restore source).
    view_args: String,
    /// Set when this node drills into a child; consumed on bare-ID re-entry.
    should_restore_view_args: bool,
    /// Confirm dialog pending (§9).
    confirm_pending: bool,
}

impl Default for EpochState {
    fn default() -> Self {
        EpochState {
            cursor_id: None,
            cursor_idx: 0,
            cursor_user_moved: false,
            view_args: String::new(),
            should_restore_view_args: false,
            confirm_pending: false,
        }
    }
}

// --- Internal traversal result ---

// Re-use MenuResult directly (user confirmed: single type).
// The Continue variant carries (RenderPlan, new_prompt_string).

// --- NodeEntry ---

pub(crate) struct NodeEntry {
    /// The host-provided node implementation. **NEVER access `inner`
    /// directly from another NodeEntry — always go through `node_data()`.
    /// Parent nodes must not call `child.inner.data()`; they call
    /// `child.node_data()` instead, which may augment the response
    /// (e.g. value forwarding). The only exception is this node's own
    /// `node_data()` implementation, which calls `self.inner.data()`. */
    inner: Box<dyn SlashDotNode>,
    children: Vec<NodeEntry>,
    content: ContentCache,
    epoch: EpochState,
    /// Last epoch this node was traversed at. Triggers EpochState reset.
    cache_epoch: u64,
    /// When true, `node_data()` enriches own rows with active values
    /// from children's `node_data()` responses. Exposed via
    /// `SlashDotMenuNode::with_value_forwarding()`.
    pub(crate) value_forwarding: bool,
}

impl NodeEntry {
    /// Create a root node: an empty listing node (no rows, no data).
    #[cfg(test)]
    pub(crate) fn new_root() -> Self {
        NodeEntry::new_child(Box::new(
            crate::node_impl::StaticListingNode::new(&[])
        ))
    }

    /// Create a child node from a host `SlashDotNode` impl.
    pub fn new_child(inner: Box<dyn SlashDotNode>) -> Self {
        NodeEntry {
            inner,
            children: Vec::new(),
            content: ContentCache::default(),
            epoch: EpochState::default(),
            cache_epoch: 0,
            value_forwarding: false,
        }
    }

    /// Append a child entry (pre-built `NodeEntry`). Returns `&mut Self`
    /// for chaining.
    pub fn add_child_entry(&mut self, child: NodeEntry) -> &mut Self {
        self.children.push(child);
        self
    }

    /// Get this node's data for its parent's listing. This is the **only**
    /// way a parent `NodeEntry` should access a child's data — never call
    /// `child.inner.data()` directly (see the `inner` field doc comment).
    ///
    /// Default: delegates to `self.inner.data()`. When `value_forwarding`
    /// is enabled, the response is enriched with active values from
    /// children — same cache that powers the listing view (`content.data_rc`,
    /// populated by `refresh_data()`). `self.inner.data()` provides only the
    /// static rows (ids/descriptions for the parent); dynamic values come
    /// from the refreshed cache, not from `data()`.
    fn node_data(
        &mut self,
        epoch: u64,
        path_args: &PathArgs,
    ) -> crate::node_impl::DataResult {
        if !self.value_forwarding {
            return self.inner.data(epoch, path_args);
        }

        // Static rows from the inner node (id + description for parent).
        let response = self.inner.data(epoch, path_args)?;

        // Populate our children's data cache — same mechanism the listing
        // view uses. This is on `self`, never on a child.
        self.refresh_data(epoch, &path_args.ids, &path_args.suffix);

        // Collect active IDs from the refreshed cache.
        let mut all_active = Vec::new();
        for slot in &self.content.data_rc {
            if let Some(child_resp) = slot.as_ref() {
                for id in &child_resp.active_ids {
                    if !all_active.contains(id) {
                        all_active.push(id.clone());
                    }
                }
            }
        }

        if all_active.is_empty() {
            return Ok(response);
        }

        // Inject active values as "value" field on own static rows.
        let value_str = all_active.join(", ");
        let enriched_rows: Vec<crate::fields::Row> = response
            .rows
            .iter()
            .map(|row| {
                let mut r = crate::fields::Row::new(row.id.clone())
                    .desc(row.description.clone().unwrap_or_default());
                for (k, v) in &row.fields {
                    r = r.field(k.clone(), v.clone());
                }
                r = r.field("value", crate::value::Value::Str(value_str.clone()));
                r
            })
            .collect();
        Ok(std::rc::Rc::new(crate::node_impl::DataResponse {
            rows: enriched_rows,
            active_ids: response.active_ids.clone(),
        }))
    }

    // --- Main entry point ---

    /// Drive one key event through this node's subtree.
    ///
    /// `path` is the accumulated drill IDs (passed by value for cheap clone
    /// on drill). `prompt_suffix` is this node's section of the prompt
    /// (everything after the parent's matched ID, or the full prompt at root).
    /// `render_ctx` carries display metrics (threaded by reference).
    pub fn node_traverse(
        &mut self,
        epoch: u64,
        key: HostKey,
        path: Vec<String>,
        prompt_suffix: &str,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        // Phase 0: Epoch gate — reset per-node epoch state on new activation.
        if epoch != self.cache_epoch {
            self.epoch = EpochState::default();
            self.cache_epoch = epoch;
        }

        // Materialize this level's PathArgs by value: `ids` moved in,
        // `suffix` copied once. Everything downstream borrows this local.
        let path_args = PathArgs::new(path, prompt_suffix);

        // Phase 0.5: Drill rewrite — if the parent drilled into us
        // (DrillEnter / DrillTab), let the node optionally rewrite the
        // prompt and restart from root. Runs before refresh: at drill
        // time the suffix is " " and needs no row data.
        if matches!(key, HostKey::DrillEnter | HostKey::DrillTab) {
            if let Some(new_prompt) = self.inner.drill_rewrite(&path_args) {
                return MenuResult::Continue(RenderPlan::default(), new_prompt);
            }
        }

        // Phase 1: Refresh data from children — `row_index` is fresh
        // for the single parse pass below (no re-parse needed).
        self.refresh_data(epoch, &path_args.ids, &path_args.suffix);

        // Phase 2: Parse section — fills `path_args.args` with byte
        // ranges into its own suffix; wraps it for dispatch.
        let parsed = parse_section(path_args, &self.content.row_index);
        let trailing_space = parsed.path_args.suffix.ends_with(' ');

        // Phase 3: Restore trigger (§5.1 step 1 — applies without Esc).
        if let Some(ref id_range) = parsed.matched_id {
            if parsed.tail.is_empty()
                && !trailing_space
                && self.epoch.should_restore_view_args
            {
                // Undo the drill: seed cursor on the matched ID's row,
                // replace the bare ID with stored view_args.
                let matched_word = parsed.path_args.suffix[id_range.clone()].to_string();
                let restore_to = self.epoch.view_args.clone();
                self.epoch.should_restore_view_args = false;
                self.epoch.cursor_id = Some(matched_word.clone());
                self.seed_cursor_idx_on_id(&matched_word);
                // Build view from restored section.
                let restore_args = parse_section(
                    PathArgs::new(parsed.path_args.ids.clone(), restore_to.clone()),
                    &self.content.row_index,
                );
                self.ensure_view(epoch, &restore_args, &restore_to);
                let plan = self.build_render_plan(&restore_args.path_args, render_ctx);
                return MenuResult::Continue(plan, restore_args.path_args.full_prompt());
            }
        }

        // Phase 4: Dispatch.

        // Leaf node (no children).
        if self.children.is_empty() {
            return self.handle_leaf(epoch, key, &parsed, render_ctx);
        }

        // Listing node: matched child with non-empty tail → recurse.
        // Forward key as-is; the child handles Enter (drill) vs
        // DrillEnter/DrillTab (stop recursion) at its own listing/leaf level.
        if let Some(ref id_range) = parsed.matched_id {
            if !parsed.tail.is_empty() {
                // The child gets a fresh `path` vector (parent ids +
                // matched ID, both by value) and borrows the tail slice
                // out of this level's suffix — no string copy for the tail.
                let mut child_path = parsed.path_args.ids.clone();
                child_path.push(parsed.path_args.suffix[id_range.clone()].to_string());
                let child_suffix = &parsed.path_args.suffix[parsed.tail.clone()];
                return self.children[parsed.child_idx].node_traverse(
                    epoch,
                    key,
                    child_path,
                    child_suffix,
                    render_ctx,
                );
            }
            // Matched child, empty tail, no restore → bare ID render (§2.5).
            // Fall through to show listing with cursor on the ID's row.
        }

        // Listing: show filtered listing, handle keys.
        self.handle_listing(epoch, key, &parsed, render_ctx)
    }

    // --- Leaf handling ---

    fn handle_leaf(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let pa = &parsed.path_args;
        match key {
            HostKey::Enter | HostKey::DrillEnter => {
                if self.epoch.confirm_pending {
                    // Confirm dialog: yea → submit.
                    self.epoch.confirm_pending = false;
                    match self.inner.submit(epoch, pa) {
                        SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg, pa.full_prompt()),
                        SubmitResult::NewPath(p) => MenuResult::Continue(
                            RenderPlan::default(),
                            p,
                        ),
                        SubmitResult::Prepared => Self::build_continue(pa),
                    }
                } else {
                    match self.inner.prepare(epoch, pa) {
                        SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg, pa.full_prompt()),
                        SubmitResult::Prepared => {
                            self.epoch.confirm_pending = true;
                            Self::build_continue(pa)
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
                    Self::build_continue(pa)
                } else {
                    Self::handle_esc(parsed)
                }
            }
            HostKey::None => {
                // Tab-drill or bare entry: show header/listing.
                let plan = self.plan_from_cache(pa, render_ctx);
                MenuResult::Continue(plan, pa.full_prompt())
            }
            _ => Self::build_continue(pa),
        }
    }

    // --- Listing handling ---

    fn handle_listing(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let pa = &parsed.path_args;
        // View args = everything before the matched tail (or entire
        // section when unmatched — `tail.start` is then the suffix end).
        let view_args_clean = pa.suffix[..parsed.tail.start].trim_end().to_string();

        match key {
            HostKey::MoveCursor(delta) => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let n = self.content.ranked_rows.len();
                if n > 0 {
                    self.epoch.cursor_idx = (self.epoch.cursor_idx as isize + delta as isize)
                        .clamp(0, (n - 1) as isize) as usize;
                    if let Some(rref) = self.content.ranked_rows.get(self.epoch.cursor_idx) {
                        self.epoch.cursor_id = rref
                            .resolve(&self.content.data_rc)
                            .map(|row| row.id.clone());
                    }
                }
                self.epoch.cursor_user_moved = true;
                let plan = self.plan_from_cache(pa, render_ctx);
                MenuResult::Continue(plan, pa.full_prompt())
            }
            HostKey::Tab | HostKey::Enter => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let Some(active_id) = self.cursor_id_or_fallback() else {
                    return Self::build_continue(pa);
                };
                let child_idx = match self.content.row_index.get(&active_id) {
                    Some(&idx) => idx as usize,
                    None => return Self::build_continue(pa),
                };
                self.epoch.view_args = view_args_clean;
                self.epoch.should_restore_view_args = true;
                let mut new_path = pa.ids.clone();
                new_path.push(active_id);
                // Listing drill: Tab forwards DrillTab, Enter forwards
                // DrillEnter — leaf nodes submit on DrillEnter (unless
                // drill_rewrite intercepts), listing nodes ignore both.
                let drill_key = if matches!(key, HostKey::Tab) {
                    HostKey::DrillTab
                } else {
                    HostKey::DrillEnter
                };
                self.children[child_idx].node_traverse(
                    epoch,
                    drill_key,
                    new_path,
                    " ",
                    render_ctx,
                )
            }
            HostKey::Esc => Self::handle_esc(parsed),
            _ => {
                self.ensure_view(epoch, parsed, &view_args_clean);
                let plan = self.build_render_plan(pa, render_ctx);
                MenuResult::Continue(plan, pa.full_prompt())
            }
        }
    }

    // --- Esc ladder (§5.1) ---

    fn handle_esc(parsed: &ParsedSection) -> MenuResult {
        let pa = &parsed.path_args;
        // Step 2: clear refinement — terms, modifiers, selection dropped.
        if !pa.args.positional.is_empty()
            || !pa.args.modifiers.is_empty()
            || parsed.matched()
        {
            // Rewrite section to just the parent path (no terms/modifiers/selection).
            return MenuResult::Continue(RenderPlan::default(), pa.prompt_with(""));
        }

        // Step 3: strip trailing space → parent becomes responsible.
        if pa.suffix.ends_with(' ') {
            let trimmed = pa.suffix.trim_end();
            return MenuResult::Continue(RenderPlan::default(), pa.prompt_with(trimmed));
        }

        // Step 4: nothing left at this level → close.
        MenuResult::DoneSuccess(None, pa.full_prompt())
    }

    // --- Data refresh ---

    /// Refresh data from children. Returns true if any child's Rc changed.
    ///
    /// **IMPORTANT:** Always call `child.node_data()` — never access
    /// `child.inner.data()` directly. `node_data()` is the only sanctioned
    /// path for parent→child data access (design §3.2). It may augment
    /// the response (e.g. value forwarding). Bypassing it breaks the
    /// encapsulation that `NodeEntry` provides over `SlashDotNode`.
    fn refresh_data(&mut self, epoch: u64, path: &[String], tail: &str) -> bool {
        // Children's `data()` receives this node's path + tail; this
        // node's `Args` are irrelevant to them (they describe *this*
        // node's section only) — hence two plain slices, not `&PathArgs`.
        let path_args = PathArgs::new(path.to_vec(), tail);
        let mut changed = false;
        let mut new_data: Vec<Option<Rc<crate::node_impl::DataResponse>>> =
            Vec::with_capacity(self.children.len());

        for (i, child) in self.children.iter_mut().enumerate() {
            // One shared `&PathArgs` for every child — built once above.
            match child.node_data(epoch, &path_args) {
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

        // Step 1: Filter by modifiers (sliced from suffix via ranges).
        let modifier_tokens: Vec<ModifierToken> = parsed
            .path_args
            .modifiers()
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
        let terms = parsed.path_args.positional().collect::<Vec<_>>().join(" ");
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
    /// Seed or keep the cursor position.
    ///
    /// When `cursor_user_moved` is true (user pressed arrow keys), the
    /// cursor persists across view changes. Otherwise, re-seed from the
    /// top of ranked_rows (or active_ids / matched_child).
    fn seed_or_keep_cursor(&mut self, parsed: &ParsedSection) {
        // If the user explicitly moved the cursor, keep it and refresh the index.
        if self.epoch.cursor_user_moved {
            if let Some(ref id) = self.epoch.cursor_id.clone() {
                if let Some(idx) = self.content.ranked_rows.iter().position(|r| {
                    r.resolve(&self.content.data_rc)
                        .map_or(false, |row| row.id == *id)
                }) {
                    self.epoch.cursor_idx = idx;
                    return;
                }
            }
            // Explicit cursor dropped out of filtered set — reset so
            // re-seed picks the new top match cleanly.
            self.epoch.cursor_user_moved = false;
        }

        // Re-seed: selection (if any) → first active → row 0.
        self.epoch.cursor_id = parsed
            .matched_id
            .as_ref()
            .map(|r| parsed.path_args.suffix[r.clone()].to_string())
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

    fn build_continue(path_args: &PathArgs) -> MenuResult {
        MenuResult::Continue(RenderPlan::default(), path_args.full_prompt())
    }

    // --- Render plan construction (§5) ---

    /// Rebuild lines and cache. Called when content is dirty or on
    /// non-cursor key events.
    ///
    /// Panel structure (§6):
    /// 1. header() lines — sticky, non-selectable
    /// 2. Table header row (if columns + data) — sticky, below header()
    /// 3. Data rows — scrollable, with > / * markers
    ///
    /// The host uses `sticky_count` to decide vertical truncation when
    /// the terminal is too short for all sticky + data lines.
    fn build_render_plan(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> RenderPlan {
        // Pre-compute: do we have data rows and table columns?
        let has_data = !self.content.ranked_rows.is_empty();
        let columns = self.inner.columns();
        let field_space = self.field_space();
        let has_columns = !columns.is_empty() || !field_space.fields.is_empty();
        let has_table_header = has_columns && has_data
            && columns.iter().any(|c| !c.header.is_empty());

        // 1. Header lines from the node's header() method → detail.
        let header = self.inner.header(path_args, render_ctx);
        let detail: Vec<LogicalLine> = header.into_iter()
            .map(LogicalLine::new)
            .collect();

        // 2. Table header + data rows.
        let term_width: usize = render_ctx.term_width;
        let mut table_header_line: Option<LogicalLine> = None;
        let row_lines: Vec<LogicalLine>;
        {
            use crate::table::{Align, Table};
            let col_field_names: Vec<String> = if !columns.is_empty() {
                columns.iter().map(|c| c.field.clone()).collect()
            } else {
                vec!["id".to_string(), "description".to_string()]
            };
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
            // Table header (single line, always one LogicalLine).
            if has_table_header {
                let header_text = t.render_header(term_width.saturating_sub(2));
                table_header_line = Some(LogicalLine::new(header_text));
            }
            // Data rows: wrapping (multi-line) or truncating (single-line + ellipsis)
            // based on the column wrap setting.
            let use_wrap = columns.iter().all(|c| c.wrap);
            row_lines = if use_wrap {
                t.render_multiline(term_width.saturating_sub(2))
            } else {
                t.render(term_width.saturating_sub(2))
            }
            .into_iter()
            .map(LogicalLine::new)
            .collect();
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

        let rows_rc = Rc::new(row_lines);

        // Cache table_header, rows, and active_rows.
        self.content.render.table_header = table_header_line.clone();
        self.content.render.rows = Rc::clone(&rows_rc);
        self.content.render.active_rows = active_rows.clone();

        let selected = if self.content.ranked_rows.is_empty() {
            None
        } else {
            Some(self.epoch.cursor_idx)
        };

        RenderPlan {
            table_header: table_header_line,
            detail,
            rows: rows_rc,
            selected,
            active_rows,
        }
    }

    /// Cursor-move fast path: reuse cached table_header + rows Rc,
    /// recompute detail (may depend on focused row) and selected/active.
    fn plan_from_cache(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> RenderPlan {
        let detail: Vec<LogicalLine> = self.inner.header(path_args, render_ctx).into_iter()
            .map(LogicalLine::new)
            .collect();

        let selected = if self.content.ranked_rows.is_empty() {
            None
        } else {
            Some(self.epoch.cursor_idx)
        };

        RenderPlan {
            table_header: self.content.render.table_header.clone(),
            detail,
            rows: Rc::clone(&self.content.render.rows),
            selected,
            active_rows: self.content.render.active_rows.clone(),
        }
    }
}

// --- Section parsing (single parse pass, §2.1) ---

/// Result of parsing a prompt section: the level's [`PathArgs`] (ids +
/// owned suffix + range-based args) plus the matched child / tail.
///
/// The new parser is the source of truth for v2.5: it records byte
/// ranges into `path_args.suffix` instead of copying strings.
struct ParsedSection {
    /// The level's path args (args filled by this parse).
    path_args: PathArgs,
    /// Byte range of the matched child ID within `path_args.suffix`.
    /// `None` = no child matched — this is the "matched?" boolean
    /// (`.is_some()`), as requested of the parse result.
    matched_id: Option<Range<usize>>,
    /// Child index for the matched ID (valid only when `matched_id`
    /// is `Some`).
    child_idx: usize,
    /// The child's section within `path_args.suffix`. Empty when no
    /// child matched (`start == suffix.len()`) or when the matched ID
    /// is the last token (bare-ID / restore checks).
    tail: Range<usize>,
}

impl ParsedSection {
    /// Whether a child row ID matched in this section.
    fn matched(&self) -> bool {
        self.matched_id.is_some()
    }
}

/// Parse a prompt section one word at a time (single pass, §2.1).
///
/// Fills `path_args.args` with byte ranges into `path_args.suffix`:
/// - `.key:value` / `.key` → modifier ranges (no string copies)
/// - word in `row_index` → matched child (section terminates)
/// - otherwise → positional (fuzzy term) range
///
/// The first word matching a row ID terminates the section; `tail`
/// covers everything after it (including the leading space).
fn parse_section(
    mut path_args: PathArgs,
    row_index: &HashMap<String, u16>,
) -> ParsedSection {
    let suffix_len = path_args.suffix.len();
    let mut matched_id: Option<Range<usize>> = None;
    let mut child_idx = 0usize;
    let mut pos = 0usize;

    while pos < suffix_len {
        // Skip whitespace.
        while pos < suffix_len && path_args.suffix.as_bytes()[pos] == b' ' {
            pos += 1;
        }
        if pos >= suffix_len {
            break;
        }

        // Find end of word.
        let start = pos;
        while pos < suffix_len && path_args.suffix.as_bytes()[pos] != b' ' {
            pos += 1;
        }
        let word = &path_args.suffix[start..pos];

        // Check if this word is a row ID.
        if let Some(&idx) = row_index.get(word) {
            matched_id = Some(start..pos);
            child_idx = idx as usize;
            break;
        }

        // Dot modifier: `.key` or `.key:value` (first colon splits).
        // Classified by range arithmetic over the word — no allocations
        // (grammar.rs serves the v2.0 interpreter only).
        if word.as_bytes()[0] == b'.' {
            match word.find(':') {
                Some(c) => {
                    // c is relative to `word`; shift to absolute offsets.
                    path_args.args.modifiers.push((
                        start + 1..start + c,
                        start + c + 1..pos,
                    ));
                }
                None => {
                    // Bare `.key` — empty value range at the word end.
                    path_args.args.modifiers.push((start + 1..pos, pos..pos));
                }
            }
        } else {
            path_args.args.positional.push(start..pos);
        }
    }

    ParsedSection {
        path_args,
        matched_id,
        child_idx,
        tail: pos..suffix_len,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_impl::{DataResponse, DataResult};
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
                    .positional()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            );
            SubmitResult::Done(None)
        }
    }

    /// A node with a static header.
    struct HeaderLeaf {
        text: String,
    }

    impl SlashDotNode for HeaderLeaf {
        fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
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

    /// Test render context: 80 columns.
    fn rctx() -> RenderContext {
        RenderContext { term_width: 80 }
    }

    #[test]
    fn root_lists_commands() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "");
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "alp", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "alpha", &rctx());
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "alpha");
                let row_count = plan.rows.len();
                assert_eq!(row_count, 2);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn tab_drills_into_child() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        root.refresh_data(ep(), &[], "");
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(_, prompt) => assert_eq!(prompt, "alpha "),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn move_cursor_wraps() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b"), CmdNode::new("c")]);
        root.refresh_data(ep(), &[], "");
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        let result = root.node_traverse(ep(), HostKey::MoveCursor(1), Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(1)),
            other => panic!("expected Continue, got {:?}", other),
        }
        let result = root.node_traverse(ep(), HostKey::MoveCursor(-1), Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(0)),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn move_cursor_clamps_not_wraps() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b"), CmdNode::new("c")]);
        root.refresh_data(ep(), &[], "");
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        // Move past last row: should clamp to last, not wrap to 0.
        let _ = root.node_traverse(ep(), HostKey::MoveCursor(100), Vec::new(), "", &rctx());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(2)),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Move before first row: should clamp to 0, not wrap.
        let _ = root.node_traverse(ep(), HostKey::MoveCursor(-100), Vec::new(), "", &rctx());
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::Esc, Vec::new(), "", &rctx());
        assert!(matches!(result, MenuResult::DoneSuccess(None, _)));
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
        fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let headers: Vec<&str> = plan.detail.iter().map(|l| l.0.as_str())
                    .collect();
                assert!(headers.contains(&"Help text here"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // --- Section parse tests ---

    /// Parse a suffix with empty ids (tests only).
    fn parse(section: &str, row_index: &HashMap<String, u16>) -> ParsedSection {
        parse_section(PathArgs::new(Vec::new(), section), row_index)
    }

    /// Modifier (key, value) pairs as owned strings.
    fn mods(p: &ParsedSection) -> Vec<(String, String)> {
        p.path_args
            .modifiers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Positional tokens as owned strings.
    fn pos(p: &ParsedSection) -> Vec<String> {
        p.path_args.positional().map(str::to_string).collect()
    }

    /// The matched child ID as an owned string.
    fn matched_id(p: &ParsedSection) -> Option<String> {
        p.matched_id
            .as_ref()
            .map(|r| p.path_args.suffix[r.clone()].to_string())
    }

    /// The child tail as an owned string.
    fn tail(p: &ParsedSection) -> String {
        p.path_args.suffix[p.tail.clone()].to_string()
    }

    #[test]
    fn parse_empty_section() {
        let ri = HashMap::new();
        let p = parse("", &ri);
        assert!(mods(&p).is_empty());
        assert!(pos(&p).is_empty());
        assert!(!p.matched());
    }

    #[test]
    fn parse_modifiers_only() {
        let ri = HashMap::new();
        let p = parse(".sort:price .tier:free", &ri);
        assert_eq!(
            mods(&p),
            vec![
                ("sort".to_string(), "price".to_string()),
                ("tier".to_string(), "free".to_string()),
            ]
        );
    }

    #[test]
    fn parse_positional_and_modifier_interleave() {
        let ri = HashMap::new();
        let p = parse("fuzzy .sort:price more", &ri);
        assert_eq!(pos(&p), vec!["fuzzy", "more"]);
        assert_eq!(
            mods(&p),
            vec![("sort".to_string(), "price".to_string())]
        );
    }

    #[test]
    fn parse_matched_child_terminates() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse("model extra words", &ri);
        assert!(p.matched());
        assert_eq!(matched_id(&p).as_deref(), Some("model"));
        assert_eq!(tail(&p), " extra words");
        assert!(pos(&p).is_empty());
    }

    #[test]
    fn parse_no_trailing_space_means_empty_tail() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse("model", &ri);
        assert!(p.matched());
        assert_eq!(matched_id(&p).as_deref(), Some("model"));
        assert!(p.tail.is_empty());
    }

    #[test]
    fn parse_fuzzy_terms_before_id() {
        let mut ri = HashMap::new();
        ri.insert("model".to_string(), 0);
        let p = parse("fuzzy model", &ri);
        assert_eq!(pos(&p), vec!["fuzzy"]);
        assert_eq!(matched_id(&p).as_deref(), Some("model"));
        assert!(p.tail.is_empty());
    }

    #[test]
    fn bare_dot_modifier() {
        let ri = HashMap::new();
        let p = parse(".sort", &ri);
        assert_eq!(mods(&p), vec![("sort".to_string(), String::new())]);
    }

    // --- Epoch reset test ---

    #[test]
    fn epoch_reset_clears_cursor() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b")]);
        root.refresh_data(ep(), &[], "");
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        let _ = root.node_traverse(ep(), HostKey::MoveCursor(1), Vec::new(), "", &rctx());
        assert_eq!(root.epoch.cursor_idx, 1);
        // New epoch resets.
        let _ = root.node_traverse(ep() + 10, HostKey::None, Vec::new(), "", &rctx());
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
        root.refresh_data(ep(), &[], "");
        // "first" is in m1's description but not a row ID → fuzzy filter.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "first", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_count = plan.rows.len();
                assert_eq!(row_count, 1, "fuzzy 'first' should narrow to 1 row");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        // Esc on filtered view clears to empty (root close or clear).
        let result = root.node_traverse(ep(), HostKey::Esc, Vec::new(), "first", &rctx());
        match result {
            MenuResult::Continue(_, _) | MenuResult::DoneSuccess(..) => {}
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
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "gpt", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
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

        root.refresh_data(ep(), &[], "");

        // Both children's rows should appear in the listing.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
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
        root.refresh_data(ep(), &[], "");

        // Tab on "add" → drills into add_cmd → shows "action" leaf row.
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        // Move cursor to "add" (it's first) and Tab.
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(_, prompt) => assert!(prompt.starts_with("add ")),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Now at "add ", the add_cmd node shows its leaf child "action".
        // Tab into the leaf.
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "add ", &rctx());
        match result {
            MenuResult::Continue(_, prompt) => assert!(prompt.starts_with("add action ")),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Enter on "add action " → submit.
        let result = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "add action ", &rctx());
        match result {
            MenuResult::DoneSuccess(..) => {}
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
        root.refresh_data(ep(), &[], "");

        // "bind" fuzzy-matches both binding rows but not "add".
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "bind", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
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
        root.refresh_data(ep(), &[], "");

        // Only "valid" should appear.
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
                    .collect();
                assert_eq!(row_texts.len(), 1);
                assert!(row_texts[0].contains("valid"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// A leaf node that implements `drill_rewrite` to pre-fill a value.
    struct RewriteLeaf {
        prefill: String,
    }

    impl SlashDotNode for RewriteLeaf {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![crate::fields::Row::new("rename").desc("rename session")],
                active_ids: Vec::new(),
            }))
        }

        fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
            vec!["rename header".into()]
        }

        fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
            Some(format!("abc rename {}", self.prefill))
        }
    }

    /// Build: root → [RewriteLeaf("rename")] as a single child.
    /// Root's Enter drill will send DrillEnter to the leaf.
    fn rewrite_root(prefill: &str) -> NodeEntry {
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(
            RewriteLeaf { prefill: prefill.to_string() },
        )));
        root
    }

    #[test]
    fn drill_rewrite_fires_on_drill_enter() {
        let mut root = rewrite_root("MySession");
        root.refresh_data(ep(), &[], "");
        // User presses Enter at root listing → drills into "rename" leaf
        // with DrillEnter → leaf's drill_rewrite fires.
        let result = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert!(plan.rows.is_empty(), "rewrite returns empty plan");
                assert_eq!(prompt, "abc rename MySession");
            }
            other => panic!("expected Continue with rewrite, got {:?}", other),
        }
    }

    #[test]
    fn drill_rewrite_fires_on_drill_tab() {
        let mut root = rewrite_root("TabFill");
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), "", &rctx());
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert!(plan.rows.is_empty());
                assert_eq!(prompt, "abc rename TabFill");
            }
            other => panic!("expected Continue with rewrite, got {:?}", other),
        }
    }

    #[test]
    fn drill_rewrite_does_not_fire_on_manual_enter() {
        let mut root = rewrite_root("MySession");
        root.refresh_data(ep(), &[], "");
        // User types "rename " and presses Enter — suffix routes through
        // node recursion with HostKey::Enter (not DrillEnter).
        // drill_rewrite should NOT fire; leaf submits normally.
        let result = root.node_traverse(
            ep(), HostKey::Enter,
            Vec::new(),
            "rename ", &rctx(),
        );
        match result {
            MenuResult::DoneSuccess(_, _) => {
                // Leaf submitted without drill_rewrite intercepting.
            }
            other => panic!("expected DoneSuccess (normal submit), got {:?}", other),
        }
    }

    #[test]
    fn drill_rewrite_none_falls_through_to_normal_submit() {
        // Node without drill_rewrite (default None) → DrillEnter falls
        // through to normal leaf submit.
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(LeafNode::new())));
        root.refresh_data(ep(), &[], "");
        // User presses Enter at root → drills into leaf with DrillEnter.
        let result = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "", &rctx());
        match result {
            MenuResult::DoneSuccess(_, _) => { /* leaf submitted */ }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }
}
