//! Internal node entry: wraps a host `SlashDotNode` with children,
//! content cache, epoch state, and the v2.5 traversal algorithm.
//! Not exposed to the host.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::fields::{row_field_value, FieldSpace, Row};
use crate::filter::compile_query;
use crate::node_impl::{
    CompletionRows, DataError, MenuResult, PathArgs, RenderContext, SubmitResult,
    SlashDotNode,
};
use crate::provider::ColumnDef;
use crate::render::{LogicalLine, RenderPlan};
use crate::value::Value;
use crate::view::{RowView, ViewPlan, ViewRequest, ViewRows};
use std::ops::Range;

/// Simplified key events the host sends to the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Resolve the handle against the slot snapshot. Returns `None` on
    /// a stale/foreign handle (§3.6: degrade/skip, never unwrap).
    pub(crate) fn resolve<'a>(&self, data: &'a [Option<Rc<crate::node_impl::DataResponse>>]) -> Option<&'a crate::fields::Row> {
        let resp = data.get(self.child_idx as usize)?.as_ref()?;
        resp.rows.get(self.row_offset as usize)
    }
}

// --- Content cache (content side: slots, dispatch index, pipeline memo) ---

struct ContentCache {
    /// Per-child data responses (slots), parallel to self.children.
    /// On DataError, slot is None (stale rows kept).
    data_rc: Vec<Option<Rc<crate::node_impl::DataResponse>>>,
    /// Rows marked active by any child (§5.2). HashSet for O(1) lookup.
    active_ids: HashSet<String>,
    /// Row ID → child index. Rebuilt when any child's Rc changes.
    row_index: HashMap<String, u16>,
    /// True when slots changed since the rows pipeline last ran.
    slots_dirty: bool,
    /// Rows-pipeline memo: key (`suffix.trim_end()`) + output.
    last_key: String,
    view_rows: Option<Rc<ViewRows>>,
}

impl Default for ContentCache {
    fn default() -> Self {
        ContentCache {
            data_rc: Vec::new(),
            active_ids: HashSet::new(),
            row_index: HashMap::new(),
            slots_dirty: true,
            last_key: String::new(),
            view_rows: None,
        }
    }
}

// --- Epoch state (user-intent, reset on new activation) ---

struct EpochState {
    /// Last rendered section for this node — the §5.1 restore source
    /// (traversal memory; kept in sync with the rows-pipeline key).
    /// Cursor state lives in `RowView` (per-view, view.rs).
    view_args: String,
    /// Set when this node drills into a child; consumed on bare-ID re-entry.
    should_restore_view_args: bool,
    /// Confirm dialog pending (§9).
    confirm_pending: bool,
}

impl Default for EpochState {
    fn default() -> Self {
        EpochState {
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
    /// Presentation side: cursor + row-line cache + plan assembly (view.rs).
    view: RowView,
    /// Completion dialog view — the second RowView instance (exclusive
    /// view: candidates fully replace rows while the dialog is up).
    completion_view: RowView,
    /// Completer-response → ViewRows wrap memo (ptr_eq passthrough for
    /// handler-cached responses).
    completion_wrap: Option<(Rc<CompletionRows>, Rc<ViewRows>)>,
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
            view: RowView::new(),
            completion_view: RowView::new(),
            completion_wrap: None,
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

    /// Drive one key event through this node's subtree, cursor at the
    /// end of the section (the common case: tests and EOL interaction).
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
        let cursor_pos = prompt_suffix.len();
        self.node_traverse_at(epoch, key, path, prompt_suffix, cursor_pos, render_ctx)
    }

    /// Like [`node_traverse`](Self::node_traverse) with an explicit
    /// cursor (byte offset into this level's `prompt_suffix`). v1 only
    /// supports end-of-line completion: the resolved level suppresses
    /// completion unless `cursor_pos == prompt_suffix.len()`, so a
    /// mid-line cursor can never complete a different word than the
    /// user edits.
    pub fn node_traverse_at(
        &mut self,
        epoch: u64,
        key: HostKey,
        path: Vec<String>,
        prompt_suffix: &str,
        cursor_pos: usize,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        // Phase 0: Epoch gate — reset per-node epoch state on new activation.
        if epoch != self.cache_epoch {
            self.epoch = EpochState::default();
            self.cache_epoch = epoch;
            self.view.reset();
            self.completion_view.reset();
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
                // Undo the drill: rebuild the restored view and pin the
                // cursor onto the matched ID's row (§5.1). The rows
                // memo hits (same key as at drill time), so the pin
                // alone moves the cursor — restore stays cheap.
                let matched_word = parsed.path_args.suffix[id_range.clone()].to_string();
                let restore_to = self.epoch.view_args.clone();
                self.epoch.should_restore_view_args = false;
                let restore_args = parse_section(
                    PathArgs::new(parsed.path_args.ids.clone(), restore_to.clone()),
                    &self.content.row_index,
                );
                let rows = self.ensure_rows(&restore_args.path_args);
                let out = self.view_plan(
                    &restore_args,
                    Some(&matched_word),
                    rows,
                    None,
                    None,
                    render_ctx,
                );
                return MenuResult::Continue(out.plan, restore_args.path_args.full_prompt());
            }
        }

        // Phase 4: Dispatch.

        // Leaf node (no children).
        if self.children.is_empty() {
            return self.handle_leaf(epoch, key, &parsed, cursor_pos, render_ctx);
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
                // Cursor is suffix-relative at each level: the child's
                // section starts at `tail.start`. Saturating — a cursor
                // above the tail leaves the child's completion inert.
                let child_cursor = cursor_pos.saturating_sub(parsed.tail.start);
                return self.children[parsed.child_idx].node_traverse_at(
                    epoch,
                    key,
                    child_path,
                    child_suffix,
                    child_cursor,
                    render_ctx,
                );
            }
            // Matched child, empty tail, no restore → bare ID render (§2.5).
            // Fall through to show listing with cursor on the ID's row.
        }

        // Listing: show filtered listing, handle keys.
        self.handle_listing(epoch, key, &parsed, cursor_pos, render_ctx)
    }

    // --- Leaf handling ---

    fn handle_leaf(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        cursor_pos: usize,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let pa = &parsed.path_args;
        // Completion: passive frames + cursor moves + Tab. Enter/Esc/
        // Drill* bypass entirely (submit raw word / ladder / drill
        // semantics); the confirm dialog has highest precedence.
        // A completer `Err` falls through carrying its message — passed
        // straight to `view_plan` below (rendered this frame, stored
        // nowhere).
        let mut frame_err: Option<String> = None;
        if !self.epoch.confirm_pending
            && matches!(
                key,
                HostKey::None | HostKey::MoveCursor(_) | HostKey::Tab
            )
        {
            match self.completion_pass(parsed, cursor_pos, key, render_ctx) {
                Ok(Some(m)) => return m,
                Ok(None) => {}
                Err(msg) => frame_err = Some(msg),
            }
        }
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
                        SubmitResult::Error(msg) => self.error_continue(parsed, &msg, render_ctx),
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
                        SubmitResult::Error(msg) => self.error_continue(parsed, &msg, render_ctx),
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
                let rows = self.ensure_rows(&parsed.path_args);
                let out =
                    self.view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx);
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
            _ => Self::build_continue(pa),
        }
    }

    /// Stay-open render for `SubmitResult::Error`: this level's rows
    /// with `[ERROR]: …` above `header()`. Direct render — the returned
    /// plan *is* the frame; nothing is stored, so the next eval
    /// re-derives without the error.
    fn error_continue(
        &mut self,
        parsed: &ParsedSection,
        error: &str,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let rows = self.ensure_rows(&parsed.path_args);
        let out = self.view_plan(parsed, None, rows, None, Some(error), render_ctx);
        MenuResult::Continue(out.plan, parsed.path_args.full_prompt())
    }

    // --- Listing handling ---

    fn handle_listing(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        cursor_pos: usize,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let pa = &parsed.path_args;
        // View args = everything before the matched tail (or entire
        // section when unmatched — `tail.start` is then the suffix end).
        let view_args_clean = pa.suffix[..parsed.tail.start].trim_end().to_string();

        // Completion first: while the dialog is up it owns the passive
        // render, cursor moves, and Tab (exclusive view); Enter/Esc/
        // Drill* bypass to the surrounding context. A completer `Err`
        // falls through carrying its message (rendered below, stored
        // nowhere).
        let mut frame_err: Option<String> = None;
        if !self.epoch.confirm_pending
            && matches!(
                key,
                HostKey::None | HostKey::MoveCursor(_) | HostKey::Tab
            )
        {
            match self.completion_pass(parsed, cursor_pos, key, render_ctx) {
                Ok(Some(m)) => return m,
                Ok(None) => {}
                Err(msg) => frame_err = Some(msg),
            }
        }

        match key {
            HostKey::MoveCursor(delta) => {
                let rows = self.ensure_rows(&parsed.path_args);
                let out =
                    self.view_plan(parsed, None, rows, Some(delta), frame_err.as_deref(), render_ctx);
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
            HostKey::Tab | HostKey::Enter => {
                let rows = self.ensure_rows(&parsed.path_args);
                let out =
                    self.view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx);
                let Some(row) = out.cursor_row else {
                    return Self::build_continue(pa);
                };
                let child_idx = match self.content.row_index.get(&row.id) {
                    Some(&idx) => idx as usize,
                    None => return Self::build_continue(pa),
                };
                self.epoch.view_args = view_args_clean;
                self.epoch.should_restore_view_args = true;
                let mut new_path = pa.ids.clone();
                new_path.push(row.id);
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
                let rows = self.ensure_rows(&parsed.path_args);
                let out =
                    self.view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx);
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
        }
    }

    // --- Esc ladder (§5.1) ---

    fn handle_esc(parsed: &ParsedSection) -> MenuResult {
        let pa = &parsed.path_args;
        // Step 2: clear refinement — terms, modifiers, selection dropped.
        if !pa.args.is_empty() || parsed.matched() {
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
            self.content.slots_dirty = true;
        }

        changed
    }

    // --- Rows pipeline (content side) + view plan ---

    /// Rows pipeline: memoized filter → fuzzy rank → sort over the flat
    /// args, minting a **fresh** `Rc<ViewRows>` whenever it runs — fresh
    /// even if content is equal (reconcile cadence must match today's
    /// derive cadence; `ptr_eq` is that signal). Key = `suffix.trim_end()`
    /// straight off `&PathArgs` — equivalent to today's
    /// `suffix[..tail.start].trim_end()` because the pipeline only runs
    /// at `handle_listing`-reachable frames where the tail is empty.
    fn ensure_rows(&mut self, path_args: &PathArgs) -> Rc<ViewRows> {
        let key = path_args.suffix.trim_end().to_string();
        // Traversal memory: §5.1 restore source — written exactly where
        // today's ensure_view wrote it (every pipeline touch).
        self.epoch.view_args = key.clone();
        if !self.content.slots_dirty && self.content.last_key == key {
            if let Some(rows) = &self.content.view_rows {
                return Rc::clone(rows);
            }
        }

        // Build field space (declared `fields()` else inferred from rows).
        let field_space = self.field_space();

        // Filter by modifiers — extracted from the flat args against
        // this level's field space (valid keys only; every other dot
        // word stays a fuzzy term).
        let (modifier_tokens, fuzzy_terms) = crate::filter::split_modifiers(
            &path_args.args,
            &path_args.suffix,
            &field_space,
        );
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

        // Fuzzy rank by the remaining flat words.
        let terms = fuzzy_terms.join(" ");
        let mut ranked: Vec<RowRef> = crate::cursor::rank_row_refs(&filtered, &terms, None)
            .rows;

        // Apply explicit sort (overrides fuzzy rank order).
        if let Some((ref sort_field, sort_dir)) = query.sort {
            self.sort_row_refs(&mut ranked, sort_field, sort_dir);
        }

        let rows = Rc::new(ViewRows::new(
            self.content.data_rc.clone(),
            ranked,
            self.content.active_ids.clone(),
        ));
        self.content.slots_dirty = false;
        self.content.last_key = key;
        self.content.view_rows = Some(Rc::clone(&rows));
        rows
    }

    /// Build this level's `ViewRequest` (detail from `error` line +
    /// `header()`, columns, seed from the parsed matched id) and run
    /// the view. `pin` is the §5.1 restore cursor force;
    /// `cursor_delta` covers MoveCursor / page keys. `error` is passed
    /// in by the caller for *this frame only* (completion `Err` /
    /// `SubmitResult::Error`) — never stored. Drill arms call this too
    /// and discard the returned plan (one `header()` + assembly on an
    /// unchanged view — rows Rc reused).
    fn view_plan(
        &mut self,
        parsed: &ParsedSection,
        pin: Option<&str>,
        rows: Rc<ViewRows>,
        cursor_delta: Option<i32>,
        error: Option<&str>,
        render_ctx: &RenderContext,
    ) -> ViewPlan {
        let path_args = &parsed.path_args;
        let seed: Option<String> = parsed
            .matched_id
            .as_ref()
            .map(|r| path_args.suffix[r.clone()].to_string());
        let columns = self.inner.columns();
        let mut detail: Vec<LogicalLine> = Vec::new();
        if let Some(msg) = error {
            detail.push(LogicalLine::new(format!("[ERROR]: {msg}")));
        }
        detail.extend(
            self.inner
                .header(path_args, render_ctx)
                .into_iter()
                .map(LogicalLine::new),
        );
        self.view.plan(
            rows,
            &ViewRequest {
                columns: &columns,
                detail,
                seed_id: seed.as_deref(),
                pin,
                cursor_delta,
                render_ctx: *render_ctx,
            },
        )
    }

    // --- Completion (dialog) ---

    /// One completion evaluation for this frame. `Ok(Some(result))`
    /// when the dialog owns the frame (render / cursor move / Tab
    /// expansion); `Ok(None)` falls through to the normal path;
    /// `Err(msg)` falls through carrying the completer's message for
    /// the caller to render above `header()` this frame. Listing
    /// levels use the internal `default_complete`; leaves use the
    /// `SlashDotNode::complete_parameter` trait method (empty by
    /// default). v1 guards: cursor at the end of this level's section,
    /// and a word directly before it (a space dismisses — nothing to
    /// complete).
    fn completion_pass(
        &mut self,
        parsed: &ParsedSection,
        cursor_pos: usize,
        key: HostKey,
        render_ctx: &RenderContext,
    ) -> Result<Option<MenuResult>, String> {
        let pa = &parsed.path_args;
        if cursor_pos != pa.suffix.len() {
            self.completion_view.reset();
            return Ok(None);
        }
        let word_range = match pa.word_before(cursor_pos) {
            Some(w) => w,
            None => {
                self.completion_view.reset();
                return Ok(None);
            }
        };
        let result = if self.children.is_empty() {
            self.inner.complete_parameter(pa, cursor_pos)
        } else {
            self.default_complete(parsed, cursor_pos)
        };
        let rc = match result {
            Ok(rc) => rc,
            Err(e) => {
                // No dialog; caller renders the message above header()
                // on the normal detail line this frame.
                self.completion_view.reset();
                return Err(e.message);
            }
        };
        if rc.rows.is_empty() {
            self.completion_view.reset();
            return Ok(None);
        }
        // Wrap the completer response — memoized on the response ptr so
        // a handler-cached Rc passes through unchanged (no reconcile;
        // dialog cursor survives).
        let wrap = self.wrap_completion(&rc);
        let (l1, l2) = rc.labels.clone().unwrap_or_else(|| {
            if self.children.is_empty() {
                ("Value".to_string(), "Description".to_string())
            } else {
                ("Modifier".to_string(), "Description".to_string())
            }
        });
        let columns = vec![
            ColumnDef::new("id", l1.as_str()),
            ColumnDef::new("description", l2.as_str()),
        ];
        let detail: Vec<LogicalLine> = self
            .inner
            .header(pa, render_ctx)
            .into_iter()
            .map(LogicalLine::new)
            .collect();
        let cursor_delta = match key {
            HostKey::MoveCursor(d) => Some(d),
            _ => None,
        };
        let out = self.completion_view.plan(
            wrap,
            &ViewRequest {
                columns: &columns,
                detail,
                seed_id: None,
                pin: None,
                cursor_delta,
                render_ctx: *render_ctx,
            },
        );
        match key {
            HostKey::Tab => {
                // In-place expansion: replace the word before cursor
                // with the cursor row's id (no trailing space written
                // directly), re-run completion on the expanded word in
                // the same turn — non-empty keeps the dialog going
                // (directory drilling / key→value progression);
                // empty/Err (final or exact) appends a trailing space
                // as a convenience (which also dismisses).
                let Some(row) = out.cursor_row else {
                    return Ok(None);
                };
                let mut new_suffix = pa.suffix.clone();
                new_suffix.replace_range(word_range.clone(), &row.id);
                let cursor2 = word_range.start + row.id.len();
                let reparsed = parse_section(
                    PathArgs::new(pa.ids.clone(), new_suffix.clone()),
                    &self.content.row_index,
                );
                let rerun = if self.children.is_empty() {
                    self.inner.complete_parameter(&reparsed.path_args, cursor2)
                } else {
                    self.default_complete(&reparsed, cursor2)
                };
                if !matches!(&rerun, Ok(r) if !r.rows.is_empty()) {
                    new_suffix.push(' ');
                }
                Ok(Some(MenuResult::Continue(
                    RenderPlan::default(),
                    reparsed.path_args.prompt_with(&new_suffix),
                )))
            }
            // None / MoveCursor: dialog render (delta applied inside).
            _ => Ok(Some(MenuResult::Continue(out.plan, pa.full_prompt()))),
        }
    }

    /// Mints the single-slot `ViewRows` for a completer response,
    /// memoized on the response ptr.
    fn wrap_completion(&mut self, rc: &Rc<CompletionRows>) -> Rc<ViewRows> {
        if let Some((prev, view)) = &self.completion_wrap {
            if Rc::ptr_eq(prev, rc) {
                return Rc::clone(view);
            }
        }
        let resp = Rc::new(crate::node_impl::DataResponse {
            rows: rc.rows.clone(),
            active_ids: Vec::new(),
        });
        let n = resp.rows.len();
        let view = Rc::new(ViewRows::new(
            vec![Some(resp)],
            (0..n as u32)
                .map(|i| RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            HashSet::new(),
        ));
        self.completion_wrap = Some((Rc::clone(rc), Rc::clone(&view)));
        view
    }

    /// Listing completion — port of v2.0 `Interpreter::field_options`
    /// over flat args + merged rows (doc: NodeEntry default
    /// completion). An empty `Ok` is the "nothing to complete" signal
    /// (gate miss, no matches, or every match exact).
    fn default_complete(
        &self,
        parsed: &ParsedSection,
        cursor_pos: usize,
    ) -> Result<Rc<CompletionRows>, DataError> {
        let pa = &parsed.path_args;
        let empty = || {
            Ok(Rc::new(CompletionRows {
                rows: Vec::new(),
                labels: None,
            }))
        };
        // Word + gate: dot-prefixed, not `..`.
        let Some(word_range) = pa.word_before(cursor_pos) else {
            return empty();
        };
        let word = &pa.suffix[word_range];
        if !word.starts_with('.') || word.starts_with("..") {
            return empty();
        }
        let field_space = self.field_space();
        // Description format (v2.0 field_options): type, or
        // "<desc>; <order>, <type>"; `sort` keeps its standing text.
        let field_desc = |name: &str| -> String {
            if name == "sort" {
                return "sort field, append `>` or `<` for direction; string".to_string();
            }
            let Some(spec) = field_space.spec(name) else {
                return String::new();
            };
            let ty = if spec.is_number { "number" } else { "string" };
            match &spec.description {
                Some(d) if !d.is_empty() => {
                    let order = match spec.default_dir {
                        crate::fields::SortDir::Asc => "min",
                        crate::fields::SortDir::Desc => "max",
                    };
                    format!("{}; {}, {}", d, order, ty)
                }
                _ => ty.to_string(),
            }
        };
        let finish = |rows: Vec<Row>| -> Result<Rc<CompletionRows>, DataError> {
            if rows.is_empty() || rows.iter().all(|r| r.id == word) {
                return empty();
            }
            Ok(Rc::new(CompletionRows { rows, labels: None }))
        };
        let body = &word[1..];
        match body.find(':') {
            None => {
                // Key mode: field names containing the typed prefix;
                // `sort` injected so it shows up like any modifier key.
                let prefix = body;
                let mut names: Vec<String> = field_space
                    .names(true)
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                if !names.iter().any(|n| n == "sort") {
                    names.push("sort".to_string());
                }
                let rows = crate::fuzzy::rank_candidates(
                    &names,
                    prefix,
                    |k| k.clone(),
                    |k| k.clone(),
                )
                .into_iter()
                .map(|k| Row::new(format!(".{}:", k)).desc(field_desc(k)))
                .collect();
                finish(rows)
            }
            Some(c) => {
                let key = &body[..c];
                let value = &body[c + 1..];
                // `.sort:` → field names as values (contains filter).
                if key == "sort" {
                    let names = field_space.names(true);
                    let rows = crate::fuzzy::rank_candidates(
                        &names,
                        value,
                        |f| f.to_string(),
                        |f| f.to_string(),
                    )
                    .into_iter()
                    .map(|f| Row::new(format!(".sort:{}", f)).desc(field_desc(f)))
                    .collect();
                    return finish(rows);
                }
                // Unknown key → no dialog (word stays positional).
                let Some(spec) = field_space.spec(key) else {
                    return empty();
                };
                // Numbers are free-form; non-filterable fields can't be
                // filtered at all — no completion either.
                if spec.is_number || !spec.filterable {
                    return empty();
                }
                // Dataset-derived string values (design D7): distinct,
                // non-empty, first-seen over the full merged row set
                // (the dataset, not the filtered view), fuzzy-matched
                // against the typed prefix (empty prefix → all).
                let mut values: Vec<String> = Vec::new();
                for slot in self.content.data_rc.iter().flatten() {
                    for row in &slot.rows {
                        if let Some(Value::Str(s)) = row_field_value(row, key) {
                            if !s.is_empty() && !values.contains(&s) {
                                values.push(s);
                            }
                        }
                    }
                }
                let rows = crate::fuzzy::rank_candidates(
                    &values,
                    value,
                    |s| s.clone(),
                    |s| s.clone(),
                )
                .into_iter()
                .map(|v| Row::new(format!(".{}:{}", key, v)).desc("string"))
                .collect();
                finish(rows)
            }
        }
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
/// Fills `path_args.args` with flat word ranges into `path_args.suffix`
/// (no modifier/positional split at parse time, no string copies):
/// - word in `row_index` → matched child (section terminates)
/// - otherwise → flat word (the view code later classifies dot words
///   against the level's field space; the rest are fuzzy terms / leaf
///   parameters)
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

        // Flat word — no modifier/positional split at parse time; the
        // view code classifies dot words against the level's field
        // space (arg_and_modifier_autocomplete.md).
        path_args.args.push(start..pos);
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
    use crate::node_impl::{CompletionRows, DataError, DataResponse, DataResult};
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
                    .args()
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

    /// Field space for the classification tests (plus implicit
    /// id/description and the injected `sort`).
    fn test_space() -> FieldSpace {
        FieldSpace::new(vec![
            crate::fields::FieldSpec::new("tier", false),
            crate::fields::FieldSpec::new("price", true),
        ])
    }

    /// Modifier (key, value) pairs — view-side classification of the
    /// flat args (split_modifiers), as owned strings.
    fn mods(p: &ParsedSection) -> Vec<(String, String)> {
        crate::filter::split_modifiers(&p.path_args.args, &p.path_args.suffix, &test_space())
            .0
            .iter()
            .map(|t| (t.key.clone(), t.value.clone()))
            .collect()
    }

    /// Flat word tokens as owned strings (all non-ID words,
    /// pre-classification).
    fn pos(p: &ParsedSection) -> Vec<String> {
        p.path_args.args().map(str::to_string).collect()
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
        // Flat args keep every word — classification happens later.
        assert_eq!(pos(&p), vec!["fuzzy", ".sort:price", "more"]);
        assert_eq!(
            mods(&p),
            vec![("sort".to_string(), "price".to_string())]
        );
    }

    #[test]
    fn parse_keeps_dot_words_flat() {
        let ri = HashMap::new();
        let p = parse(".sort:price ./src .gitignore", &ri);
        assert_eq!(
            pos(&p),
            vec![".sort:price", "./src", ".gitignore"]
        );
        // Only `.sort:price` classifies as a modifier for this space.
        assert_eq!(mods(&p).len(), 1);
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
        assert_eq!(pos(&p), vec![".sort"]);
        assert_eq!(mods(&p), vec![("sort".to_string(), String::new())]);
    }

    // --- Epoch reset test ---

    #[test]
    fn epoch_reset_clears_cursor() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::new("b")]);
        root.refresh_data(ep(), &[], "");
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        let _ = root.node_traverse(ep(), HostKey::MoveCursor(1), Vec::new(), "", &rctx());
        assert_eq!(root.view.cursor_idx, 1);
        // New epoch resets.
        let _ = root.node_traverse(ep() + 10, HostKey::None, Vec::new(), "", &rctx());
        assert_eq!(root.view.cursor_idx, 0);
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

    // --- Completion (implicit-open dialog; steps 4–6) ---

    /// Listing node whose rows carry typed fields (inferred field space:
    /// id, description, tier: string, price: number + injected sort).
    struct FieldedNode;

    impl SlashDotNode for FieldedNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![
                    Row::new("m1")
                        .desc("alpha model")
                        .field("tier", Value::Str("free".into()))
                        .field("price", Value::Num(0.0)),
                    Row::new("m2")
                        .desc("beta model")
                        .field("tier", Value::Str("paid".into()))
                        .field("price", Value::Num(3.0)),
                    Row::new("m3")
                        .desc("gamma model")
                        .field("tier", Value::Str("free".into()))
                        .field("price", Value::Num(1.0)),
                ],
                active_ids: Vec::new(),
            }))
        }
    }

    fn fielded_root() -> NodeEntry {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(FieldedNode)));
        root
    }

    /// Rendered row texts from a Continue result.
    fn rows_of(result: MenuResult) -> Vec<String> {
        match result {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn completion_key_mode_lists_field_names() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(ep(), HostKey::None, Vec::new(), ".t", &rctx());
        let dialog = match &result {
            MenuResult::Continue(plan, _) => {
                // Dialog discriminator: candidates render with the fixed
                // Modifier/Description columns (listing has none).
                plan.table_header.is_some()
            }
            other => panic!("expected Continue, got {:?}", other),
        };
        assert!(dialog, "dot word must open the dialog");
        let rows = rows_of(result);
        assert!(rows.iter().any(|r| r.contains(".tier:")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(".sort:")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(".description:")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains(".id:")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains(".price:")), "{rows:?}");
    }

    #[test]
    fn completion_bare_dot_lists_every_key() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".",
            &rctx(),
        ));
        for k in [".id:", ".description:", ".tier:", ".price:", ".sort:"] {
            assert!(rows.iter().any(|r| r.contains(k)), "missing {k}: {rows:?}");
        }
        assert_eq!(rows.len(), 5, "{rows:?}");
    }

    #[test]
    fn completion_ignores_plain_words() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "m1",
            &rctx(),
        ));
        // No dialog: the normal fuzzy listing renders.
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:?}");
    }

    #[test]
    fn completion_value_mode_string_field() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:fr",
            &rctx(),
        ));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains(".tier:free"), "{rows:?}");

        // Empty prefix: every distinct value (dataset-derived, D7).
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:",
            &rctx(),
        ));
        assert!(rows.iter().any(|r| r.contains(".tier:free")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(".tier:paid")), "{rows:?}");
    }

    #[test]
    fn completion_exact_match_closes_dialog() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        // `.tier:free` is an exact unique match → no dialog; the normal
        // listing renders (filtered by the modifier to m1/m3).
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:free",
            &rctx(),
        ));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m3")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("m2")), "{rows:?}");
    }

    #[test]
    fn completion_number_and_unknown_keys_stay_inert() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        for suffix in [".price:1", ".nope:x", "..", "./src"] {
            let rows = rows_of(root.node_traverse(
                ep(),
                HostKey::None,
                Vec::new(),
                suffix,
                &rctx(),
            ));
            assert!(
                !rows.iter().any(|r| r.starts_with('.')),
                "{suffix}: dialog must stay down: {rows:?}"
            );
        }
    }

    #[test]
    fn completion_invalid_key_fuzzy_narrows_listing() {
        // The accepted behavior delta: an invalid-key dot word is a
        // fuzzy term, not a silently-ignored modifier.
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".srot:price",
            &rctx(),
        ));
        assert!(rows.is_empty(), "fuzzy '.srot:price' matches nothing: {rows:?}");
    }

    #[test]
    fn completion_mid_cursor_suppressed() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        // v1: cursor mid-line → completion suppressed (EOL guard), even
        // though the word under the cursor would otherwise complete.
        let result = root.node_traverse_at(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:fr",
            3,
            &rctx(),
        );
        let rows = rows_of(result);
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_space_dismisses() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:free ",
            &rctx(),
        ));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_dialog_cursor_moves_and_persists() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        let sel_of = |root: &mut NodeEntry, key: HostKey| match root.node_traverse(
            ep(),
            key,
            Vec::new(),
            ".t",
            &rctx(),
        ) {
            MenuResult::Continue(plan, _) => plan.selected,
            other => panic!("expected Continue, got {:?}", other),
        };
        assert_eq!(sel_of(&mut root, HostKey::None), Some(0));
        assert_eq!(sel_of(&mut root, HostKey::MoveCursor(1)), Some(1));
        // Passive re-render keeps the moved cursor (keep-id reconcile,
        // even though the default completer mints a fresh Rc per call).
        assert_eq!(sel_of(&mut root, HostKey::None), Some(1));
        // Dismiss (space): dialog down, listing cursor untouched.
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".t ",
            &rctx(),
        ));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_tab_expands_key_and_progresses_to_values() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        // `.so` → prefix match `.sort:` ranked first; `.description:`
        // (subsequence s…o) follows in the fuzzy tier.
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".so",
            &rctx(),
        ));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains(".sort:"), "prefix must rank first: {rows:?}");
        // Tab: in-place expansion, NO trailing space — the same-turn
        // re-run lands in value mode (non-empty) and keeps going.
        let result = root.node_traverse(ep(), HostKey::Tab, Vec::new(), ".so", &rctx());
        match result {
            MenuResult::Continue(_, prompt) => assert_eq!(prompt, ".sort:"),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn completion_tab_final_value_appends_trailing_space() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        // `.tier:fr` → Tab expands to `.tier:free`; the same-turn
        // re-run is an exact/final match (empty) → trailing space.
        let result = root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            ".tier:fr",
            &rctx(),
        );
        match result {
            MenuResult::Continue(_, prompt) => assert_eq!(prompt, ".tier:free "),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn completion_confirm_pending_suppresses_dialog() {
        let mut root = fielded_root();
        root.refresh_data(ep(), &[], "");
        // Init epoch state at this epoch.
        let _ = root.node_traverse(ep(), HostKey::None, Vec::new(), "", &rctx());
        root.epoch.confirm_pending = true;
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".t",
            &rctx(),
        ));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    /// Leaf with fs-style completion + shared submit recorder.
    struct PathLeaf {
        last_submit: Rc<std::cell::RefCell<Option<String>>>,
        err: Option<String>,
        rows: Vec<Row>,
    }

    impl PathLeaf {
        fn new() -> Self {
            PathLeaf {
                last_submit: Rc::new(std::cell::RefCell::new(None)),
                err: None,
                rows: vec![
                    Row::new("/home/x/proj").desc("dir"),
                    Row::new("/home/x/pub").desc("dir"),
                ],
            }
        }
    }

    impl SlashDotNode for PathLeaf {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new("pathpick").desc("pick a path")],
                active_ids: Vec::new(),
            }))
        }

        fn complete_parameter(
            &mut self,
            _args: &PathArgs,
            _cursor_pos: usize,
        ) -> Result<Rc<CompletionRows>, DataError> {
            if let Some(m) = &self.err {
                return Err(DataError { message: m.clone() });
            }
            Ok(Rc::new(CompletionRows {
                rows: self.rows.clone(),
                labels: None,
            }))
        }

        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            *self.last_submit.borrow_mut() =
                Some(path_args.args().next().unwrap_or_default().to_string());
            SubmitResult::Done(None)
        }
    }

    fn leaf_root() -> (NodeEntry, Rc<std::cell::RefCell<Option<String>>>) {
        let mut root = NodeEntry::new_root();
        let leaf = PathLeaf::new();
        let rec = Rc::clone(&leaf.last_submit);
        root.children.push(NodeEntry::new_child(Box::new(leaf)));
        root.refresh_data(ep(), &[], "");
        (root, rec)
    }

    #[test]
    fn leaf_completion_dialog_and_enter_bypasses_to_submit() {
        let (mut root, rec) = leaf_root();
        // Resolved INTO the leaf (matched id + tail): trait completion.
        let result = root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        );
        match &result {
            MenuResult::Continue(plan, _) => {
                assert!(plan.table_header.is_some(), "dialog columns expected");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        let rows = rows_of(result);
        assert!(rows.iter().any(|r| r.contains("/home/x/proj")), "{rows:?}");

        // Enter bypasses the dialog entirely: submits the RAW word.
        let result = root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        );
        match result {
            MenuResult::DoneSuccess(_, _) => {}
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
        assert_eq!(rec.borrow().as_deref(), Some("/h"));
    }

    #[test]
    fn leaf_completion_error_surfaces_in_detail() {
        let mut root = NodeEntry::new_root();
        let mut leaf = PathLeaf::new();
        leaf.err = Some("boom".to_string());
        root.children.push(NodeEntry::new_child(Box::new(leaf)));
        root.refresh_data(ep(), &[], "");
        let result = root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        );
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.detail.len(), 1, "{:?}", plan.detail);
                assert!(plan.detail[0].0.contains("[ERROR]: boom"), "{:?}", plan.detail[0]);
                assert!(plan.rows.is_empty(), "no dialog on Err: {:?}", plan.rows);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// Param leaf: `submit` fails without an arg (the sandbox-shaped
    /// drill guard) and succeeds with one.
    struct ParamLeaf;

    impl SlashDotNode for ParamLeaf {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new("param").desc("needs an arg")],
                active_ids: Vec::new(),
            }))
        }

        fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
            vec!["Specify a host path.".to_string()]
        }

        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            if path_args.args().next().is_none() {
                SubmitResult::Error("host path required".to_string())
            } else {
                SubmitResult::Done(None)
            }
        }
    }

    fn param_root() -> NodeEntry {
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(ParamLeaf)));
        root.refresh_data(ep(), &[], "");
        root
    }

    #[test]
    fn submit_error_renders_inline_above_header_and_stays_open() {
        let mut root = param_root();
        // Drill arrival: Enter on the bare id forwards DrillEnter into
        // the leaf with an empty suffix → submit sees no args → Error.
        let result = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "param", &rctx());
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "param ", "{prompt:?}");
                assert_eq!(plan.detail.len(), 2, "{:?}", plan.detail);
                assert!(
                    plan.detail[0].0.contains("[ERROR]: host path required"),
                    "{:?}",
                    plan.detail[0]
                );
                assert!(
                    plan.detail[1].0.contains("Specify a host path"),
                    "{:?}",
                    plan.detail[1]
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn submit_error_is_one_shot() {
        let mut root = param_root();
        let first = root.node_traverse(ep(), HostKey::Enter, Vec::new(), "param", &rctx());
        assert!(matches!(first, MenuResult::Continue(..)), "{:?}", first);
        // The next frame over the SAME prompt re-derives without the
        // error: nothing was stored (direct render, stateless).
        let second = root.node_traverse(ep(), HostKey::None, Vec::new(), "param ", &rctx());
        match second {
            MenuResult::Continue(plan, _) => {
                assert!(
                    plan.detail.iter().all(|l| !l.0.contains("[ERROR]")),
                    "error leaked into next frame: {:?}",
                    plan.detail
                );
                assert_eq!(plan.detail.len(), 1, "{:?}", plan.detail);
                assert!(
                    plan.detail[0].0.contains("Specify a host path"),
                    "{:?}",
                    plan.detail[0]
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn submit_with_arg_succeeds() {
        let mut root = param_root();
        let result =
            root.node_traverse(ep(), HostKey::Enter, Vec::new(), "param /home/gab", &rctx());
        match result {
            MenuResult::DoneSuccess(_, prompt) => assert_eq!(prompt, "param /home/gab"),
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn leaf_no_word_no_dialog() {
        let (mut root, _rec) = leaf_root();
        // Trailing space (just drilled in): no word directly before the
        // cursor → nothing to complete; the leaf header renders instead.
        let result = root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h ",
            &rctx(),
        );
        match result {
            MenuResult::Continue(plan, _) => {
                assert!(plan.rows.is_empty(), "{:?}", plan.rows);
                assert!(plan.table_header.is_none());
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// Listing whose row fields are declared in deliberately shuffled
    /// order — key-mode ranking (inferred at the root level) must
    /// ignore field order and rank prefix/length first.
    struct RankedFieldsNode;

    impl SlashDotNode for RankedFieldsNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new("run")
                    .field("price_input", Value::Num(1.0))
                    .field("reasoning", Value::Str("chain".into()))
                    .field("price_output", Value::Num(2.0))
                    .field("price", Value::Num(3.0))],
                active_ids: Vec::new(),
            }))
        }
    }

    /// First whitespace token of a rendered row (= the row id).
    fn first_tok(rows: &[String]) -> Vec<&str> {
        rows.iter()
            .map(|r| r.split_whitespace().next().unwrap_or(""))
            .collect()
    }

    #[test]
    fn completion_key_mode_ranks_prefix_by_length() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(RankedFieldsNode)));
        root.refresh_data(ep(), &[], "");
        // `.pri` → prefix tier, shortest first, ignoring declaration
        // order: price, price_input, price_output.
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".pri",
            &rctx(),
        ));
        assert_eq!(
            first_tok(&rows),
            vec![".price:", ".price_input:", ".price_output:"],
            "{rows:?}"
        );
        // `.so` → prefix `.sort:` first, fuzzy tier after.
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".so",
            &rctx(),
        ));
        let ids = first_tok(&rows);
        assert_eq!(ids[0], ".sort:", "prefix must rank first: {ids:?}");
        assert!(ids.contains(&".reasoning:"), "fuzzy tier: {ids:?}");
        assert!(ids.contains(&".description:"), "fuzzy tier: {ids:?}");
    }

    #[test]
    fn listing_ranks_prefix_ids_shortest_first() {
        // Predictable drill order for listings: id-prefix matches first,
        // shortest id first, fuzzy-only matches after.
        let mut root = NodeEntry::new_root();
        root.children.push(NodeEntry::new_child(Box::new(DynamicNode {
            rows: vec![
                Row::new("model_xtra_long").desc("long"),
                Row::new("alphabet").desc("modular parts"),
                Row::new("model").desc("short"),
                Row::new("models").desc("plural"),
            ],
        })));
        root.refresh_data(ep(), &[], "");
        let rows = rows_of(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "mod",
            &rctx(),
        ));
        let ids = first_tok(&rows);
        assert_eq!(
            ids,
            vec!["model", "models", "model_xtra_long", "alphabet"],
            "{ids:?}"
        );
    }
}
