//! Internal node entry: wraps a host `SlashDotNode` with children,
//! content cache, epoch state, and the v2.5 traversal algorithm.
//! Not exposed to the host.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::fields::ColumnDef;
use crate::fields::{row_field_value, FieldSpace, Row};
use crate::filter::compile_query;
use crate::node_impl::{
    CompletionRows, ConfirmLabels, DataError, MenuResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult, CONFIRM_ACCEPT, CONFIRM_ACCEPT_LABEL, CONFIRM_CANCEL, CONFIRM_CANCEL_LABEL,
};
use crate::render::LogicalLine;
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
    /// Open the endpoint's editor (Ctrl-E) — dispatches to
    /// [`crate::node_impl::SlashDotNode::edit`]. Like Enter/Esc it
    /// bypasses the completion pass; a pending confirm swallows it
    /// (like any non-confirm key).
    Edit,
}

// --- Handle to a row in a child's DataResponse ---

/// A lightweight handle (child_idx, row_offset) into a child's
/// `Arc<DataResponse>.rows` vec. Copy, 8 bytes. Created by `refresh_data`
/// and `ensure_view`; resolved via `RowRef::resolve`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowRef {
    pub child_idx: u16,
    pub row_offset: u32,
}

impl RowRef {
    /// Resolve the handle against the slot snapshot. Returns `None` on
    /// a stale/foreign handle (§3.6: degrade/skip, never unwrap).
    pub(crate) fn resolve<'a>(
        &self,
        data: &'a [Option<Arc<crate::node_impl::DataResponse>>],
    ) -> Option<&'a crate::fields::Row> {
        let resp = data.get(self.child_idx as usize)?.as_ref()?;
        resp.rows.get(self.row_offset as usize)
    }
}

// --- Content cache (content side: slots, dispatch index, pipeline memo) ---

struct ContentCache {
    /// Per-child data responses (slots), parallel to self.children.
    /// On DataError, slot is None (stale rows kept).
    data_rc: Vec<Option<Arc<crate::node_impl::DataResponse>>>,
    /// Rows marked active by any child (§5.2). HashSet for O(1) lookup.
    active_ids: HashSet<String>,
    /// Row ID → child index. Rebuilt when any child's Arc changes.
    row_index: HashMap<String, u16>,
    /// True when slots changed since the rows pipeline last ran.
    slots_dirty: bool,
    /// Rows-pipeline memo: key (`suffix.trim_end()`) + output.
    last_key: String,
    view_rows: Option<Arc<ViewRows>>,
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

#[derive(Default)]
struct EpochState {
    /// Last rendered section for this node — the §5.1 restore source
    /// (traversal memory; kept in sync with the rows-pipeline key).
    /// Cursor state lives in `RowView` (per-view, view.rs).
    view_args: String,
    /// Set when this node drills into a child; consumed on bare-ID re-entry.
    should_restore_view_args: bool,
    /// Confirm dialog armed (§9). The engine owns the dialog itself —
    /// this flag is only the arm; the rows and cursor live in
    /// `dialog_view`.
    confirm_pending: bool,
    /// The prompt this node was reached with when `confirm_pending` was
    /// armed. A prompt that no longer matches disarms the dialog
    /// (§9: a buffer rewrite cancels), so an armed confirm can never
    /// resolve against arguments it was not prepared for. Empty when
    /// no confirm is armed.
    confirm_prompt: String,
    /// Arm descriptions from the `SubmitResult::Prepared` that armed
    /// the dialog — `None` for the engine defaults. Captured at arm
    /// time so every frame of one confirm renders the same text
    /// without re-asking the node.
    confirm_labels: Option<ConfirmLabels>,
}

// --- Internal traversal result ---

// Re-use MenuResult directly (user confirmed: single type).
// `Continue` carries (RenderPlan, prompt); `ContinuePrompt` is a pure
// prompt rewrite — the host re-evals with `HostKey::None` for the plan.

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
    /// The exclusive dialog view — the second `RowView` instance. Owned
    /// by whichever dialog owns the current frame: completion candidates
    /// (implicit open) or the confirm arms (§9). The two are mutually
    /// exclusive by construction (a confirm arms only from a key the
    /// completion pass does not own, and suppresses the pass while
    /// armed), so one instance serves both — with a `reset()` at each
    /// dialog boundary so neither leaks its cursor into the other.
    dialog_view: RowView,
    /// Completer-response → ViewRows wrap memo (ptr_eq passthrough for
    /// handler-cached responses).
    completion_wrap: Option<(Arc<CompletionRows>, Arc<ViewRows>)>,
    /// Confirm-arm rows → ViewRows, memoized on the descriptions they
    /// were minted from. The arms are fixed; only the text a
    /// `SubmitResult::Prepared` carried varies, so the rebuild is a
    /// two-string compare.
    confirm_wrap: Option<(ConfirmLabels, Arc<ViewRows>)>,
}

impl NodeEntry {
    /// Create a root node: an empty listing node (no rows, no data).
    #[cfg(test)]
    pub(crate) fn new_root() -> Self {
        NodeEntry::new_child(Box::new(crate::node_impl::StaticListingNode::new(&[])))
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
            dialog_view: RowView::new(),
            completion_wrap: None,
            confirm_wrap: None,
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
    // Manual `Box::pin` future: `node_data` → `refresh_data` → child
    // `node_data` is a genuine async cycle — unboxed futures would be
    // infinitely sized (E0733). Boxing breaks it; everything else on the
    // chain stays a plain `async fn`.
    fn node_data<'a>(
        &'a mut self,
        epoch: u64,
        path_args: &'a PathArgs,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::node_impl::DataResult> + 'a>>
    {
        Box::pin(async move {
            if !self.value_forwarding {
                return self.inner.data(epoch, path_args).await;
            }

            // Static rows from the inner node (id + description for parent).
            let response = self.inner.data(epoch, path_args).await?;

            // Populate our children's data cache — same mechanism the listing
            // view uses. This is on `self`, never on a child.
            self.refresh_data(epoch, &path_args.ids, &path_args.suffix)
                .await;

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
            Ok(std::sync::Arc::new(crate::node_impl::DataResponse {
                rows: enriched_rows,
                active_ids: response.active_ids.clone(),
            }))
        })
    }

    // --- Main entry point ---

    /// Drive one key event through this node's subtree, cursor at the
    /// end of the section (the common case: tests and EOL interaction).
    ///
    /// `path` is the accumulated drill IDs (passed by value for cheap clone
    /// on drill). `prompt_suffix` is this node's section of the prompt
    /// (everything after the parent's matched ID, or the full prompt at root).
    /// `render_ctx` carries display metrics (threaded by reference).
    pub async fn node_traverse(
        &mut self,
        epoch: u64,
        key: HostKey,
        path: Vec<String>,
        prompt_suffix: &str,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let cursor_pos = prompt_suffix.len();
        self.node_traverse_at(epoch, key, path, prompt_suffix, cursor_pos, render_ctx)
            .await
    }

    /// Like [`node_traverse`](Self::node_traverse) with an explicit
    /// cursor (byte offset into this level's `prompt_suffix`). v1 only
    /// supports end-of-line completion: the resolved level suppresses
    /// completion unless `cursor_pos == prompt_suffix.len()`, so a
    /// mid-line cursor can never complete a different word than the
    /// user edits.
    // Manual `Box::pin` future: `node_traverse_at` → `handle_listing` →
    // child `node_traverse` is a genuine async cycle (E0733 without
    // boxing). Same treatment as `node_data`.
    pub fn node_traverse_at<'a>(
        &'a mut self,
        epoch: u64,
        key: HostKey,
        path: Vec<String>,
        prompt_suffix: &'a str,
        cursor_pos: usize,
        render_ctx: &'a RenderContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = MenuResult> + 'a>> {
        Box::pin(async move {
            // Phase 0: Epoch gate — reset per-node epoch state on new activation.
            if epoch != self.cache_epoch {
                self.epoch = EpochState::default();
                self.cache_epoch = epoch;
                self.view.reset();
                self.dialog_view.reset();
            }

            // Materialize this level's PathArgs by value: `ids` moved in,
            // `suffix` copied once. Everything downstream borrows this local.
            let path_args = PathArgs::new(path, prompt_suffix);

            // A confirm armed for a different prompt no longer applies
            // (§9: a buffer rewrite cancels). The node keeps ownership of
            // its own staged payload — this only drops the engine's arm,
            // so a rewritten buffer can never resolve a confirm that was
            // prepared for other arguments.
            if self.epoch.confirm_pending && self.epoch.confirm_prompt != path_args.full_prompt() {
                self.disarm_confirm();
            }

            // Phase 0.5: Drill rewrite — if the parent drilled into us
            // (DrillEnter / DrillTab), let the node optionally rewrite the
            // prompt and restart from root. Runs before refresh: at drill
            // time the suffix is " " and needs no row data.
            if matches!(key, HostKey::DrillEnter | HostKey::DrillTab) {
                if let Some(new_prompt) = self.inner.drill_rewrite(&path_args).await {
                    return MenuResult::ContinuePrompt(new_prompt);
                }
            }

            // Phase 1: Refresh data from children — `row_index` is fresh
            // for the single parse pass below (no re-parse needed).
            self.refresh_data(epoch, &path_args.ids, &path_args.suffix)
                .await;

            // Phase 2: Parse section — fills `path_args.args` with byte
            // ranges into its own suffix; wraps it for dispatch.
            let parsed = parse_section(path_args, &self.content.row_index);
            let trailing_space = parsed.path_args.suffix.ends_with(' ');

            // Phase 3: Restore trigger (§5.1 step 1 — applies without Esc).
            if let Some(ref id_range) = parsed.matched_id {
                if parsed.tail.is_empty() && !trailing_space && self.epoch.should_restore_view_args
                {
                    // Undo the drill: rebuild the restored view and pin the
                    // cursor onto the matched ID's row (§5.1). The rows
                    // memo hits (same key as at drill time), so the pin
                    // alone moves the cursor — restore stays cheap.
                    let matched_word = parsed.path_args.suffix[id_range.clone()].to_string();
                    self.epoch.should_restore_view_args = false;
                    let restore_args = parse_section(
                        PathArgs::new(parsed.path_args.ids.clone(), self.epoch.view_args.clone()),
                        &self.content.row_index,
                    );
                    let rows = self.ensure_rows(&restore_args.path_args);
                    let out = self
                        .view_plan(
                            &restore_args,
                            Some(&matched_word),
                            rows,
                            None,
                            None,
                            render_ctx,
                        )
                        .await;
                    return MenuResult::Continue(out.plan, restore_args.path_args.full_prompt());
                }
            }

            // Phase 4: Dispatch.

            // Leaf node (no children).
            if self.children.is_empty() {
                return self
                    .handle_leaf(epoch, key, &parsed, cursor_pos, render_ctx)
                    .await;
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
                    return self.children[parsed.child_idx]
                        .node_traverse_at(
                            epoch,
                            key,
                            child_path,
                            child_suffix,
                            child_cursor,
                            render_ctx,
                        )
                        .await;
                }
                // Matched child, empty tail, no restore → bare ID render (§2.5).
                // Fall through to show listing with cursor on the ID's row.
            }

            // Listing: show filtered listing, handle keys.
            self.handle_listing(epoch, key, &parsed, cursor_pos, render_ctx)
                .await
        })
    }

    // --- Leaf handling ---

    async fn handle_leaf(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        cursor_pos: usize,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let pa = &parsed.path_args;
        // The confirm dialog owns the frame while armed (§9) — before
        // `Edit` (a pending confirm swallows that key) and before the
        // completion pass (lower precedence).
        if let Some(m) = self.confirm_pass(epoch, key, parsed, render_ctx).await {
            return m;
        }
        // `HostKey::Edit` first: the node decides (completion does not own
        // this key), and a `Handled` result degrades to a passive
        // re-derive — `key` becomes `None`, so the completion gate and the
        // `None` arm below produce exactly what a fresh eval would
        // (doc/editor_cleanup.md §5).
        let mut key = key;
        if matches!(key, HostKey::Edit) {
            match self.inner.edit(epoch, pa).await {
                SubmitResult::Handled => key = HostKey::None,
                SubmitResult::Done(msg) => return MenuResult::DoneSuccess(msg, pa.full_prompt()),
                SubmitResult::Prepared(labels) => {
                    return self.arm_confirm(parsed, labels, render_ctx).await
                }
                SubmitResult::NewPath(p) => return MenuResult::ContinuePrompt(p),
                SubmitResult::Error(msg) => {
                    return self.error_continue(parsed, &msg, render_ctx).await
                }
            }
        }
        // Completion: passive frames + cursor moves + Tab. Enter/Esc/
        // Drill* bypass entirely (submit raw word / ladder / drill
        // semantics). A completer `Err` falls through carrying its
        // message — passed straight to `view_plan` below (rendered this
        // frame, stored nowhere).
        let mut frame_err: Option<String> = None;
        if matches!(key, HostKey::None | HostKey::MoveCursor(_) | HostKey::Tab) {
            match self
                .completion_pass(parsed, cursor_pos, key, render_ctx)
                .await
            {
                Ok(Some(m)) => return m,
                Ok(None) => {}
                Err(msg) => frame_err = Some(msg),
            }
        }
        match key {
            // An armed confirm never reaches here — `confirm_pass` owns
            // the frame while it is up (§9).
            HostKey::Enter | HostKey::DrillEnter => {
                match self.inner.prepare(epoch, pa).await {
                    SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg, pa.full_prompt()),
                    SubmitResult::Prepared(labels) => {
                        self.arm_confirm(parsed, labels, render_ctx).await
                    }
                    SubmitResult::NewPath(p) => MenuResult::ContinuePrompt(p),
                    SubmitResult::Handled => {
                        // Nothing to commit: re-derive the current frame
                        // (the host re-evals the same prompt).
                        Self::build_continue(pa)
                    }
                    SubmitResult::Error(msg) => self.error_continue(parsed, &msg, render_ctx).await,
                }
            }
            HostKey::Esc => Self::handle_esc(parsed),
            HostKey::None => {
                // Tab-drill or bare entry: show header/listing.
                let rows = self.ensure_rows(&parsed.path_args);
                let out = self
                    .view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx)
                    .await;
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
            _ => Self::build_continue(pa),
        }
    }

    /// Stay-open render for `SubmitResult::Error`: this level's rows
    /// with `[ERROR]: …` above `header()`. Direct render — the returned
    /// plan *is* the frame; nothing is stored, so the next eval
    /// re-derives without the error.
    async fn error_continue(
        &mut self,
        parsed: &ParsedSection,
        error: &str,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let rows = self.ensure_rows(&parsed.path_args);
        let out = self
            .view_plan(parsed, None, rows, None, Some(error), render_ctx)
            .await;
        MenuResult::Continue(out.plan, parsed.path_args.full_prompt())
    }

    // --- Confirm dialog (§9) ---

    /// Drive the confirm dialog while it is armed. `None` when no
    /// confirm is armed, so both `handle_leaf` and `handle_listing` can
    /// call it unconditionally as their first step.
    ///
    /// The dialog owns the frame: every key renders it, and only Enter
    /// (resolving the cursor row) and Esc (nay) change state. `accept`
    /// runs `submit()`; `cancel` drops the arm in place. The staged
    /// payload is the node's own — a cancel leaves it to the node to
    /// invalidate (its `submit()` keys on the same `PathArgs`).
    async fn confirm_pass(
        &mut self,
        epoch: u64,
        key: HostKey,
        parsed: &ParsedSection,
        render_ctx: &RenderContext,
    ) -> Option<MenuResult> {
        if !self.epoch.confirm_pending {
            return None;
        }
        let pa = &parsed.path_args;
        // Plan first, then dispatch on the key — the same shape as
        // `completion_pass`: Enter needs the cursor row the plan
        // resolves, and every other key needs the frame itself.
        let delta = match key {
            HostKey::MoveCursor(d) => Some(d),
            _ => None,
        };
        let out = self.confirm_plan(parsed, delta, render_ctx).await;
        match key {
            HostKey::Enter | HostKey::DrillEnter => {
                let accepted = out
                    .cursor_row
                    .as_ref()
                    .is_some_and(|row| row.id == CONFIRM_ACCEPT);
                self.disarm_confirm();
                if !accepted {
                    // Nay: drop the arm, re-derive this level's frame.
                    return Some(self.passive_continue(parsed, render_ctx).await);
                }
                Some(match self.inner.submit(epoch, pa).await {
                    SubmitResult::Done(msg) => MenuResult::DoneSuccess(msg, pa.full_prompt()),
                    SubmitResult::NewPath(p) => MenuResult::ContinuePrompt(p),
                    // Re-arming: a `submit()` that preps another step
                    // (compaction stages, then commits) — dialog stays up.
                    SubmitResult::Prepared(labels) => {
                        self.arm_confirm(parsed, labels, render_ctx).await
                    }
                    // Nothing to commit: re-derive the current frame.
                    SubmitResult::Handled => Self::build_continue(pa),
                    SubmitResult::Error(msg) => self.error_continue(parsed, &msg, render_ctx).await,
                })
            }
            HostKey::Esc => {
                self.disarm_confirm();
                Some(self.passive_continue(parsed, render_ctx).await)
            }
            // Everything else (None, MoveCursor, Tab, Edit, DrillTab)
            // just re-renders the dialog.
            _ => Some(MenuResult::Continue(out.plan, pa.full_prompt())),
        }
    }

    /// Arm the confirm dialog and render it. `labels` is the optional
    /// `(accept, cancel)` phrasing from the `Prepared` that armed it.
    /// The direct `Continue` is the same shape as `error_continue`: the
    /// plan *is* the frame, so the host paints it without a prompt-rewrite
    /// round trip. The prompt is unchanged either way, which is what
    /// keeps `Repl::batch_slash`'s re-press heuristic working.
    async fn arm_confirm(
        &mut self,
        parsed: &ParsedSection,
        labels: Option<ConfirmLabels>,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        self.epoch.confirm_pending = true;
        self.epoch.confirm_prompt = parsed.path_args.full_prompt();
        self.epoch.confirm_labels = labels;
        // Fresh cursor at the `accept` arm (row 0) — a dialog view left
        // over from a completion dialog must not inherit its cursor.
        self.dialog_view.reset();
        let out = self.confirm_plan(parsed, None, render_ctx).await;
        MenuResult::Continue(out.plan, parsed.path_args.full_prompt())
    }

    /// Drop the arm. Resets the shared dialog view so the next
    /// completion dialog starts from its own reconcile.
    fn disarm_confirm(&mut self) {
        self.epoch.confirm_pending = false;
        self.epoch.confirm_prompt.clear();
        self.epoch.confirm_labels = None;
        self.dialog_view.reset();
    }

    /// This level's ordinary frame — the post-cancel render, shared by
    /// both resolve paths.
    async fn passive_continue(
        &mut self,
        parsed: &ParsedSection,
        render_ctx: &RenderContext,
    ) -> MenuResult {
        let rows = self.ensure_rows(&parsed.path_args);
        let out = self
            .view_plan(parsed, None, rows, None, None, render_ctx)
            .await;
        MenuResult::Continue(out.plan, parsed.path_args.full_prompt())
    }

    /// The dialog's frame: the node's `header()` as detail, the two arms
    /// as rows on the shared dialog view. No columns — the implicit
    /// `id`/`description` pair renders the arms as plain text, and no
    /// table header is drawn.
    async fn confirm_plan(
        &mut self,
        parsed: &ParsedSection,
        cursor_delta: Option<i32>,
        render_ctx: &RenderContext,
    ) -> ViewPlan {
        let rows = self.confirm_rows();
        let detail: Vec<LogicalLine> = self
            .inner
            .header(&parsed.path_args, render_ctx)
            .await
            .into_iter()
            .map(LogicalLine::new)
            .collect();
        self.dialog_view.plan(
            rows,
            &ViewRequest {
                columns: &[],
                detail,
                seed_id: None,
                pin: None,
                cursor_delta,
                render_ctx: *render_ctx,
            },
        )
    }

    /// The arm rows as a `ViewRows`, memoized on the descriptions they
    /// were minted from — the arms and their text are fixed for the
    /// life of an arm, so this rebuilds only when a new `Prepared`
    /// phrases them differently. Keeping one `Arc` across frames is what
    /// lets the dialog cursor survive a re-render (`ptr_eq` skips the
    /// reconcile).
    fn confirm_rows(&mut self) -> Arc<ViewRows> {
        let (accept, cancel) = self.epoch.confirm_labels.clone().unwrap_or_else(|| {
            (
                CONFIRM_ACCEPT_LABEL.to_string(),
                CONFIRM_CANCEL_LABEL.to_string(),
            )
        });
        if let Some(((a, c), view)) = &self.confirm_wrap {
            if *a == accept && *c == cancel {
                return Arc::clone(view);
            }
        }
        let view = ViewRows::of(vec![
            Row::new(CONFIRM_ACCEPT).desc(accept.clone()),
            Row::new(CONFIRM_CANCEL).desc(cancel.clone()),
        ]);
        self.confirm_wrap = Some(((accept, cancel), Arc::clone(&view)));
        view
    }

    // --- Listing handling ---

    async fn handle_listing(
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
        let view_args_clean = pa.suffix[..parsed.tail.start].to_string();

        // The confirm dialog owns the frame while armed (§9). A listing
        // can arm one from `edit()` → `Prepared`; this arm is what makes
        // it resolvable — without it Enter would drill and Esc would run
        // the ladder, stranding the flag (and suppressing completion) for
        // the rest of the epoch.
        if let Some(m) = self.confirm_pass(epoch, key, parsed, render_ctx).await {
            return m;
        }
        // `HostKey::Edit` first — same contract as `handle_leaf`: the node
        // decides, `Handled` degrades to a passive re-derive (`key = None`,
        // completion gate + `None` path below).
        let mut key = key;
        if matches!(key, HostKey::Edit) {
            match self.inner.edit(epoch, pa).await {
                SubmitResult::Handled => key = HostKey::None,
                SubmitResult::Done(msg) => return MenuResult::DoneSuccess(msg, pa.full_prompt()),
                SubmitResult::Prepared(labels) => {
                    return self.arm_confirm(parsed, labels, render_ctx).await
                }
                SubmitResult::NewPath(p) => return MenuResult::ContinuePrompt(p),
                SubmitResult::Error(msg) => {
                    return self.error_continue(parsed, &msg, render_ctx).await
                }
            }
        }

        // Completion first: while the dialog is up it owns the passive
        // render, cursor moves, and Tab (exclusive view); Enter/Esc/
        // Drill* bypass to the surrounding context. A completer `Err`
        // falls through carrying its message (rendered below, stored
        // nowhere).
        let mut frame_err: Option<String> = None;
        if matches!(key, HostKey::None | HostKey::MoveCursor(_) | HostKey::Tab) {
            match self
                .completion_pass(parsed, cursor_pos, key, render_ctx)
                .await
            {
                Ok(Some(m)) => return m,
                Ok(None) => {}
                Err(msg) => frame_err = Some(msg),
            }
        }

        match key {
            HostKey::MoveCursor(delta) => {
                let rows = self.ensure_rows(&parsed.path_args);
                let out = self
                    .view_plan(
                        parsed,
                        None,
                        rows,
                        Some(delta),
                        frame_err.as_deref(),
                        render_ctx,
                    )
                    .await;
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
            HostKey::Tab | HostKey::Enter => {
                let rows = self.ensure_rows(&parsed.path_args);
                let out = self
                    .view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx)
                    .await;
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
                self.children[child_idx]
                    .node_traverse(epoch, drill_key, new_path, " ", render_ctx)
                    .await
            }
            HostKey::Esc => Self::handle_esc(parsed),
            _ => {
                let rows = self.ensure_rows(&parsed.path_args);
                let out = self
                    .view_plan(parsed, None, rows, None, frame_err.as_deref(), render_ctx)
                    .await;
                MenuResult::Continue(out.plan, pa.full_prompt())
            }
        }
    }

    // --- Esc ladder (§5.1) ---

    fn handle_esc(parsed: &ParsedSection) -> MenuResult {
        let pa = &parsed.path_args;
        // Step 2: clear refinement — terms, modifiers, selection dropped.
        if !pa.args.is_empty() || parsed.matched() {
            // Rewrite section to just the parent path (no terms/modifiers/selection), but *with*
            // trailing space so we stay in the current listing.
            return if !pa.ids.is_empty() {
                MenuResult::ContinuePrompt(pa.prompt_with(" "))
            } else {
                MenuResult::ContinuePrompt(pa.prompt_with(""))
            };
        }

        // Step 3: strip trailing space → parent becomes responsible.
        if pa.suffix.ends_with(' ') {
            let trimmed = pa.suffix.trim_end();
            return MenuResult::ContinuePrompt(pa.prompt_with(trimmed));
        }

        // Step 4: nothing left at this level → close.
        MenuResult::DoneSuccess(None, pa.full_prompt())
    }

    // --- Data refresh ---

    /// Refresh data from children. Returns true if any child's Arc changed.
    ///
    /// **IMPORTANT:** Always call `child.node_data()` — never access
    /// `child.inner.data()` directly. `node_data()` is the only sanctioned
    /// path for parent→child data access (design §3.2). It may augment
    /// the response (e.g. value forwarding). Bypassing it breaks the
    /// encapsulation that `NodeEntry` provides over `SlashDotNode`.
    async fn refresh_data(&mut self, epoch: u64, path: &[String], tail: &str) -> bool {
        // Children's `data()` receives this node's path + tail; this
        // node's `Args` are irrelevant to them (they describe *this*
        // node's section only) — hence two plain slices, not `&PathArgs`.
        let path_args = PathArgs::new(path.to_vec(), tail);
        let mut changed = false;
        let mut new_data: Vec<Option<Arc<crate::node_impl::DataResponse>>> =
            Vec::with_capacity(self.children.len());

        for (i, child) in self.children.iter_mut().enumerate() {
            // One shared `&PathArgs` for every child — built once above.
            match child.node_data(epoch, &path_args).await {
                Ok(response) => {
                    // Fast path: the node handed back the same allocation.
                    // Fallback: same *content* — the node minted a fresh
                    // response but nothing a listing can see changed, so it
                    // contributes no change and the stored `Arc` is reused
                    // (writing the *stored* one, not the fresh one, so a
                    // sibling's change cannot churn it). This is what keeps
                    // the flicker-free carry on the real menu path, where
                    // most nodes do not memoize.
                    let reuse = match self.content.data_rc.get(i) {
                        Some(Some(prev)) if Arc::ptr_eq(prev, &response) => Some(Arc::clone(prev)),
                        Some(Some(prev)) if prev.same_content(&response) => Some(Arc::clone(prev)),
                        _ => None,
                    };
                    match reuse {
                        Some(arc) => new_data.push(Some(arc)),
                        None => {
                            changed = true;
                            new_data.push(Some(response));
                        }
                    }
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
                        self.content
                            .row_index
                            .insert(row.id.clone(), child_idx as u16);
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
    /// args, minting a **fresh** `Arc<ViewRows>` whenever it runs — fresh
    /// even if content is equal (reconcile cadence must match today's
    /// derive cadence; `ptr_eq` is that signal). Key = `suffix.trim_end()`
    /// straight off `&PathArgs` — equivalent to today's
    /// `suffix[..tail.start].trim_end()` because the pipeline only runs
    /// at `handle_listing`-reachable frames where the tail is empty.
    fn ensure_rows(&mut self, path_args: &PathArgs) -> Arc<ViewRows> {
        let key = path_args.suffix.trim_end().to_string();
        // Traversal memory: §5.1 restore source — written exactly where
        // today's ensure_view wrote it (every pipeline touch).
        self.epoch.view_args = key.clone();
        if !self.content.slots_dirty && self.content.last_key == key {
            if let Some(rows) = &self.content.view_rows {
                return Arc::clone(rows);
            }
        }

        // Build field space (declared `fields()` else inferred from rows).
        let field_space = self.field_space();

        // Filter by modifiers — extracted from the flat args against
        // this level's field space (valid keys only; every other dot
        // word stays a fuzzy term).
        let (modifier_tokens, fuzzy_terms) =
            crate::filter::split_modifiers(&path_args.args, &path_args.suffix, &field_space);
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
                rc.rows.iter().enumerate().map(move |(off, row)| {
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
                query.filters.iter().all(|(name, pred)| {
                    row_field_value(row, name)
                        .map(|v| pred.matches(&v))
                        .unwrap_or(false)
                })
            })
            .collect();

        // Fuzzy rank by the remaining flat words.
        let terms = fuzzy_terms.join(" ");

        // Collect displayed field names for match index computation.
        let display_fields: Vec<String> = if !self.inner.columns().is_empty() {
            self.inner
                .columns()
                .iter()
                .map(|c| c.field.clone())
                .collect()
        } else {
            vec!["id".to_string(), "description".to_string()]
        };

        let rank_result = crate::cursor::rank_row_refs(&filtered, &terms, None, &display_fields);
        let mut ranked: Vec<RowRef> = rank_result.rows;

        // Apply explicit sort (overrides fuzzy rank order).
        if let Some((ref sort_field, sort_dir)) = query.sort {
            self.sort_row_refs(&mut ranked, sort_field, sort_dir);
        }

        let rows = Arc::new(ViewRows::new(
            self.content.data_rc.clone(),
            ranked,
            self.content.active_ids.clone(),
            rank_result.match_indices,
        ));
        self.content.slots_dirty = false;
        self.content.last_key = key;
        self.content.view_rows = Some(Arc::clone(&rows));
        rows
    }

    /// Build this level's `ViewRequest` (detail from `error` line +
    /// `header()`, columns, seed from the parsed matched id) and run
    /// the view. `pin` is the §5.1 restore cursor force;
    /// `cursor_delta` covers MoveCursor / page keys. `error` is passed
    /// in by the caller for *this frame only* (completion `Err` /
    /// `SubmitResult::Error`) — never stored. Drill arms call this too
    /// and discard the returned plan (one `header()` + assembly on an
    /// unchanged view — rows Arc reused).
    async fn view_plan(
        &mut self,
        parsed: &ParsedSection,
        pin: Option<&str>,
        rows: Arc<ViewRows>,
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
                .await
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
    async fn completion_pass(
        &mut self,
        parsed: &ParsedSection,
        cursor_pos: usize,
        key: HostKey,
        render_ctx: &RenderContext,
    ) -> Result<Option<MenuResult>, String> {
        let pa = &parsed.path_args;
        if cursor_pos != pa.suffix.len() {
            self.dialog_view.reset();
            return Ok(None);
        }
        let word_range = match pa.word_before(cursor_pos) {
            Some(w) => w,
            None => {
                self.dialog_view.reset();
                return Ok(None);
            }
        };
        let result = if self.children.is_empty() {
            self.inner.complete_parameter(pa, cursor_pos).await
        } else {
            self.default_complete(parsed, cursor_pos)
        };
        let rc = match result {
            Ok(rc) => rc,
            Err(e) => {
                // No dialog; caller renders the message above header()
                // on the normal detail line this frame.
                self.dialog_view.reset();
                return Err(e.message);
            }
        };
        if rc.rows.is_empty() {
            self.dialog_view.reset();
            return Ok(None);
        }
        // Wrap the completer response — memoized on the response ptr so
        // a handler-cached Arc passes through unchanged (no reconcile;
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
            .await
            .into_iter()
            .map(LogicalLine::new)
            .collect();
        let cursor_delta = match key {
            HostKey::MoveCursor(d) => Some(d),
            _ => None,
        };
        let out = self.dialog_view.plan(
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
                    self.inner
                        .complete_parameter(&reparsed.path_args, cursor2)
                        .await
                } else {
                    self.default_complete(&reparsed, cursor2)
                };
                if !matches!(&rerun, Ok(r) if !r.rows.is_empty()) {
                    new_suffix.push(' ');
                }
                Ok(Some(MenuResult::ContinuePrompt(
                    reparsed.path_args.prompt_with(&new_suffix),
                )))
            }
            // None / MoveCursor: dialog render (delta applied inside).
            _ => Ok(Some(MenuResult::Continue(out.plan, pa.full_prompt()))),
        }
    }

    /// Mints the single-slot `ViewRows` for a completer response,
    /// memoized on the response ptr.
    fn wrap_completion(&mut self, rc: &Arc<CompletionRows>) -> Arc<ViewRows> {
        if let Some((prev, view)) = &self.completion_wrap {
            if Arc::ptr_eq(prev, rc) {
                return Arc::clone(view);
            }
        }
        let resp = Arc::new(crate::node_impl::DataResponse {
            rows: rc.rows.clone(),
            active_ids: Vec::new(),
        });
        let n = resp.rows.len();
        let view = Arc::new(ViewRows::new(
            vec![Some(resp)],
            (0..n as u32)
                .map(|i| RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            HashSet::new(),
            HashMap::new(),
        ));
        self.completion_wrap = Some((Arc::clone(rc), Arc::clone(&view)));
        view
    }

    /// Listing completion — field names and dataset value options
    /// over flat args + merged rows (doc: NodeEntry default
    /// completion). An empty `Ok` is the "nothing to complete" signal
    /// (gate miss, no matches, or every match exact).
    fn default_complete(
        &self,
        parsed: &ParsedSection,
        cursor_pos: usize,
    ) -> Result<Arc<CompletionRows>, DataError> {
        let pa = &parsed.path_args;
        let empty = || {
            Ok(Arc::new(CompletionRows {
                rows: Vec::new(),
                labels: None,
                match_indices: None,
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
        // Description format: type, or
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
        let body = &word[1..];
        match body.find(':') {
            None => {
                // Key mode: field names containing the typed prefix;
                // `sort` injected so it shows up like any modifier key.
                // The exactness contract (`value_completion`) cannot fire
                // here by construction: a key-mode word has no `:`, while
                // every candidate id ends with one — never byte-equal, and
                // the next Tab step is the key → value progression.
                let prefix = body;
                let mut names: Vec<String> = field_space
                    .names(true)
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                if !names.iter().any(|n| n == "sort") {
                    names.push("sort".to_string());
                }
                // Match against the *rendered* row id (`.name:`) so highlight
                // offsets line up with the dialog's `id` column.
                let field_extractors: Vec<(String, Box<dyn for<'a> Fn(&'a String) -> String>)> =
                    vec![(
                        "id".to_string(),
                        Box::new(|k: &String| format!(".{}:", k))
                            as Box<dyn for<'a> Fn(&'a String) -> String>,
                    )];
                let (ranked, match_by_rank) = crate::fuzzy::rank_candidates(
                    &names,
                    prefix,
                    |k| k.to_string(),
                    |k| k.to_string(),
                    &field_extractors,
                );
                let rows: Vec<Row> = ranked
                    .iter()
                    .map(|k| Row::new(format!(".{}:", k)).desc(field_desc(k)))
                    .collect();
                let match_indices = if rows.is_empty() {
                    None
                } else {
                    let mut map = HashMap::new();
                    for (idx, fm) in match_by_rank.into_iter().enumerate() {
                        if !fm.is_empty() {
                            map.insert(idx, fm);
                        }
                    }
                    Some(map)
                };
                Ok(Arc::new(CompletionRows {
                    rows,
                    labels: None,
                    match_indices,
                }))
            }
            Some(c) => {
                let key = &body[..c];
                let value = &body[c + 1..];
                // `.sort:` → field names as values (contains filter).
                if key == "sort" {
                    let names: Vec<String> = field_space
                        .names(true)
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect();
                    // Match against the rendered row id (`.sort:name`).
                    let field_extractors: Vec<(String, Box<dyn for<'a> Fn(&'a String) -> String>)> =
                        vec![(
                            "id".to_string(),
                            Box::new(|f: &String| format!(".sort:{}", f))
                                as Box<dyn for<'a> Fn(&'a String) -> String>,
                        )];
                    let (ranked, match_by_rank) = crate::fuzzy::rank_candidates(
                        &names,
                        value,
                        |f| f.to_string(),
                        |f| f.to_string(),
                        &field_extractors,
                    );
                    let rows: Vec<Row> = ranked
                        .iter()
                        .map(|f| Row::new(format!(".sort:{}", f)).desc(field_desc(f)))
                        .collect();
                    // `.sort:<field>` is a terminal token — nothing is left
                    // to drill into after the field name (the `>`/`<`
                    // direction suffix is free-form). The exactness
                    // contract is what lets `Tab` commit it: on the
                    // expanded word the candidate set is exactly the word
                    // itself, so the completer reports nothing left and the
                    // engine appends the trailing space.
                    return Ok(Self::value_completion(rows, match_by_rank, word));
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
                // Match against the rendered row id (`.key:value`).
                let key_owned = key.to_string();
                let field_extractors: Vec<(String, Box<dyn for<'a> Fn(&'a String) -> String>)> =
                    vec![(
                        "id".to_string(),
                        Box::new(move |v: &String| format!(".{}:{}", key_owned, v))
                            as Box<dyn for<'a> Fn(&'a String) -> String>,
                    )];
                let (ranked, match_by_rank) = crate::fuzzy::rank_candidates(
                    &values,
                    value,
                    |s| s.clone(),
                    |s| s.clone(),
                    &field_extractors,
                );
                let rows: Vec<Row> = ranked
                    .iter()
                    .map(|v| Row::new(format!(".{}:{}", key, v)).desc("string"))
                    .collect();
                // Exact match check: if all rows have the same id as the typed word, return empty (closes dialog).
                Ok(Self::value_completion(rows, match_by_rank, word))
            }
        }
    }

    /// The exactness contract, shared by every value-mode branch of
    /// [`NodeEntry::default_complete`]: an empty match set, or one in
    /// which *every* candidate is byte-equal to the typed word, means
    /// nothing is left to complete — the completer answers with **no
    /// rows**, so the dialog closes and the `Tab` arm's same-turn re-run
    /// sees that empty result and appends the trailing space which
    /// commits the token. Any non-exact candidate keeps the full set
    /// (exact matches included) with its match indices for highlighting.
    fn value_completion(
        rendered: Vec<Row>,
        match_by_rank: Vec<crate::fuzzy::MatchIndices>,
        word: &str,
    ) -> Arc<CompletionRows> {
        let all_exact = !rendered.is_empty() && rendered.iter().all(|r| r.id == word);
        if rendered.is_empty() || all_exact {
            return Arc::new(CompletionRows {
                rows: Vec::new(),
                labels: None,
                match_indices: None,
            });
        }
        let mut indices = HashMap::new();
        for (idx, fm) in match_by_rank.into_iter().enumerate() {
            if !fm.is_empty() {
                indices.insert(idx, fm);
            }
        }
        Arc::new(CompletionRows {
            rows: rendered,
            labels: None,
            match_indices: Some(indices),
        })
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
                let id_a = ra
                    .resolve(&self.content.data_rc)
                    .map(|r| r.id.as_str())
                    .unwrap_or("");
                let id_b = rb
                    .resolve(&self.content.data_rc)
                    .map(|r| r.id.as_str())
                    .unwrap_or("");
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
        MenuResult::ContinuePrompt(path_args.full_prompt())
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
fn parse_section(mut path_args: PathArgs, row_index: &HashMap<String, u16>) -> ParsedSection {
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
    use crate::node_impl::{
        CONFIRM_ACCEPT, CONFIRM_ACCEPT_LABEL, CONFIRM_CANCEL, CONFIRM_CANCEL_LABEL,
    };
    use std::rc::Rc;
    use std::sync::Arc;

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

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for CmdNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
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

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for LeafNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![crate::fields::Row::new("leaf")],
                active_ids: Vec::new(),
            }))
        }

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            *self.last_submit.borrow_mut() =
                Some(path_args.args().next().unwrap_or_default().to_string());
            SubmitResult::Done(None)
        }
    }

    /// A node with dynamic rows for testing fuzzy + filter + sort.
    struct DynamicNode {
        rows: Vec<crate::fields::Row>,
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for DynamicNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
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

    fn ep() -> u64 {
        1
    }

    /// Arm a confirm the way `arm_confirm` does — flag *and* the prompt
    /// it was armed for, which the buffer-rewrite check reads. Tests
    /// that only want the armed state (rather than driving a real
    /// `Prepared`) set both through here.
    fn arm(node: &mut NodeEntry, prompt: &str) {
        node.epoch.confirm_pending = true;
        node.epoch.confirm_prompt = prompt.to_string();
        node.dialog_view.reset();
    }

    /// Test render context: 80 columns.
    fn rctx() -> RenderContext {
        RenderContext { term_width: 80 }
    }

    /// Traverse like a host: resolve `ContinuePrompt` rewrite chains
    /// (apply prompt, re-key `HostKey::None`) until a frame or terminal
    /// result arrives. Tests that care about host-observable behavior
    /// use this; tests asserting a specific rewrite use `node_traverse`
    /// raw.
    fn traverse_resolved(root: &mut NodeEntry, key: HostKey, suffix: &str) -> MenuResult {
        const MAX_REWRITES: usize = 16;
        let mut suffix = suffix.to_string();
        let mut key = key;
        for _ in 0..MAX_REWRITES {
            match futures::executor::block_on(root.node_traverse(
                ep(),
                key,
                Vec::new(),
                &suffix,
                &rctx(),
            )) {
                MenuResult::ContinuePrompt(new) => {
                    suffix = new;
                    key = HostKey::None;
                }
                other => return other,
            }
        }
        panic!("prompt-rewrite cycle at `{}`", suffix);
    }

    #[test]
    fn root_lists_commands() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "");
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "alp",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts = rows_of(MenuResult::Continue(plan, String::new()));
                assert!(row_texts.iter().any(|t| t.contains("alpha")));
                assert!(!row_texts.iter().any(|t| t.contains("beta")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn bare_id_shows_parent_listing() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha"), CmdNode::new("beta")]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "alpha",
            &rctx(),
        ));
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, "alpha "),
            other => panic!("expected ContinuePrompt drill, got {:?}", other),
        }
    }

    #[test]
    fn move_cursor_wraps() {
        let mut root = root_with_cmds(vec![
            CmdNode::new("a"),
            CmdNode::new("b"),
            CmdNode::new("c"),
        ]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(1),
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(1)),
            other => panic!("expected Continue, got {:?}", other),
        }
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(-1),
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(0)),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn move_cursor_clamps_not_wraps() {
        let mut root = root_with_cmds(vec![
            CmdNode::new("a"),
            CmdNode::new("b"),
            CmdNode::new("c"),
        ]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        // Move past last row: should clamp to last, not wrap to 0.
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(100),
            Vec::new(),
            "",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(2)),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Move before first row: should clamp to 0, not wrap.
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(-100),
            Vec::new(),
            "",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => assert_eq!(plan.selected, Some(0)),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn active_marker_present() {
        let mut root = root_with_cmds(vec![CmdNode::new("a"), CmdNode::with_active("b", "b")]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Esc,
            Vec::new(),
            "",
            &rctx(),
        ));
        assert!(matches!(result, MenuResult::DoneSuccess(None, _)));
    }

    // --- HostKey::Edit / SlashDotNode::edit dispatch (editor_cleanup §5) ---

    enum EditReply {
        Handled,
        Done,
    }

    /// A leaf that records `edit()` calls; shares its flag with the test.
    struct EditSpyNode {
        reply: EditReply,
        called: Rc<std::cell::RefCell<bool>>,
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for EditSpyNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![crate::fields::Row::new("x").desc("ex")],
                active_ids: Vec::new(),
            }))
        }

        async fn edit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
            *self.called.borrow_mut() = true;
            match self.reply {
                EditReply::Handled => SubmitResult::Handled,
                EditReply::Done => SubmitResult::Done(Some("edited".to_string())),
            }
        }
    }

    fn spy_root(reply: EditReply) -> (NodeEntry, Rc<std::cell::RefCell<bool>>) {
        let called = Rc::new(std::cell::RefCell::new(false));
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(EditSpyNode {
                reply,
                called: called.clone(),
            })));
        (root, called)
    }

    #[test]
    fn edit_default_stays_open_with_a_frame() {
        // No edit() override (CmdNode): `Handled` → passive frame, stay open.
        let mut root = root_with_cmds(vec![CmdNode::new("alpha")]);
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Edit,
            Vec::new(),
            "alpha t",
            &rctx(),
        ));
        assert!(
            matches!(&result, MenuResult::Continue(..)),
            "expected a Continue frame, got {:?}",
            result
        );
    }

    #[test]
    fn edit_at_a_listing_level_stays_open() {
        let mut root = root_with_cmds(vec![CmdNode::new("alpha")]);
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Edit,
            Vec::new(),
            "",
            &rctx(),
        ));
        assert!(
            matches!(&result, MenuResult::Continue(..)),
            "expected a Continue frame, got {:?}",
            result
        );
    }

    #[test]
    fn edit_done_closes_the_menu() {
        let (mut root, _called) = spy_root(EditReply::Done);
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Edit,
            Vec::new(),
            "x t",
            &rctx(),
        ));
        match result {
            MenuResult::DoneSuccess(Some(msg), _) => assert_eq!(msg, "edited"),
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn edit_is_swallowed_while_a_confirm_is_pending() {
        let (mut root, called) = spy_root(EditReply::Done);
        // Epoch-gate-proof: prime the child so its first traverse with `ep()`
        // does not reset the state we set here.
        root.children[0].cache_epoch = ep();
        arm(&mut root.children[0], "x t");
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Edit,
            Vec::new(),
            "x t",
            &rctx(),
        ));
        // The armed dialog owns the frame (§9): Edit re-renders it
        // rather than reaching the node.
        match result {
            MenuResult::Continue(plan, prompt) => {
                assert_eq!(prompt, "x t");
                let arms: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert_eq!(arms.len(), 2, "{arms:?}");
                assert!(arms[0].contains(CONFIRM_ACCEPT), "{arms:?}");
            }
            other => panic!("expected the confirm dialog, got {:?}", other),
        }
        assert!(
            !*called.borrow(),
            "edit() must not run while a confirm is pending"
        );
    }

    #[test]
    fn edit_handled_rederives_the_same_prompt_as_a_passive_eval() {
        let (mut root, called) = spy_root(EditReply::Handled);
        let edited = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Edit,
            Vec::new(),
            "x t",
            &rctx(),
        ));
        assert!(*called.borrow(), "edit() must have run");

        let (mut root2, _c2) = spy_root(EditReply::Handled);
        let passive = futures::executor::block_on(root2.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "x t",
            &rctx(),
        ));
        match (edited, passive) {
            (MenuResult::Continue(_, p_edited), MenuResult::Continue(_, p_passive)) => {
                assert_eq!(p_edited, p_passive)
            }
            other => panic!("expected two Continue frames, got {:?}", other),
        }
    }

    // --- Confirm dialog (§9) ---

    /// A two-phase leaf: `prepare()` always arms, carrying `descs` as
    /// the confirm arms' text; `submit()` records what it was handed.
    /// `edit()` answers `EditReply` so a listing can arm through the
    /// editor path too. With `completer` set it also opens a completion
    /// dialog — the two share one `RowView`, which is what the
    /// cursor-leak test is about.
    struct ConfirmLeaf {
        row: &'static str,
        header: Vec<String>,
        prepares: Rc<std::cell::Cell<usize>>,
        submits: Rc<std::cell::RefCell<Vec<String>>>,
        edit_reply: EditReply,
        descs: Option<ConfirmLabels>,
        completer: bool,
    }

    impl ConfirmLeaf {
        fn new(row: &'static str) -> Self {
            ConfirmLeaf {
                row,
                header: vec!["node guidance".into()],
                prepares: Rc::new(std::cell::Cell::new(0)),
                submits: Rc::new(std::cell::RefCell::new(Vec::new())),
                edit_reply: EditReply::Handled,
                descs: None,
                completer: false,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for ConfirmLeaf {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![Row::new(self.row).desc("an action")],
                active_ids: Vec::new(),
            }))
        }

        async fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
            self.header.clone()
        }

        async fn prepare(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
            self.prepares.set(self.prepares.get() + 1);
            SubmitResult::Prepared(self.descs.clone())
        }

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            self.submits
                .borrow_mut()
                .push(path_args.args().collect::<Vec<_>>().join(" "));
            SubmitResult::Done(Some("committed".into()))
        }

        async fn edit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
            match self.edit_reply {
                EditReply::Handled => SubmitResult::Handled,
                EditReply::Done => SubmitResult::Done(Some("edited".into())),
            }
        }

        async fn complete_parameter(
            &mut self,
            _args: &PathArgs,
            _cursor_pos: usize,
        ) -> Result<Arc<CompletionRows>, DataError> {
            Ok(Arc::new(CompletionRows {
                rows: if self.completer {
                    vec![
                        Row::new("alpha").desc("first"),
                        Row::new("beta").desc("second"),
                    ]
                } else {
                    Vec::new()
                },
                labels: None,
                match_indices: None,
            }))
        }
    }

    /// The confirm arms' row ids, in order, from a rendered plan.
    fn confirm_arms(result: MenuResult) -> Vec<String> {
        match result {
            MenuResult::Continue(plan, _) => plan
                .rows
                .iter()
                .map(|l| l.0.split_whitespace().next().unwrap_or("").to_string())
                .collect(),
            other => panic!("expected a Continue frame, got {:?}", other),
        }
    }

    /// The selected row index of a rendered plan.
    fn confirm_sel(result: &MenuResult) -> Option<usize> {
        match result {
            MenuResult::Continue(plan, _) => plan.selected,
            other => panic!("expected a Continue frame, got {:?}", other),
        }
    }

    fn confirm_detail(result: &MenuResult) -> Vec<String> {
        match result {
            MenuResult::Continue(plan, _) => plan.detail.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected a Continue frame, got {:?}", other),
        }
    }

    /// root → ConfirmLeaf("act"), reached at `act `.
    fn confirm_leaf_root() -> (
        NodeEntry,
        Rc<std::cell::Cell<usize>>,
        Rc<std::cell::RefCell<Vec<String>>>,
    ) {
        let mut root = NodeEntry::new_root();
        let leaf = ConfirmLeaf::new("act");
        let p = Rc::clone(&leaf.prepares);
        let s = Rc::clone(&leaf.submits);
        root.children.push(NodeEntry::new_child(Box::new(leaf)));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        (root, p, s)
    }

    #[test]
    fn prepare_arms_the_dialog_with_accept_first() {
        let (mut root, prepares, submits) = confirm_leaf_root();
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(prepares.get(), 1, "prepare must have run");
        assert!(
            submits.borrow().is_empty(),
            "the arm must not submit: {:?}",
            submits.borrow()
        );
        // The dialog is the frame: two arms, accept first and selected.
        assert_eq!(
            confirm_arms(result),
            vec![CONFIRM_ACCEPT.to_string(), CONFIRM_CANCEL.to_string()]
        );
        assert_eq!(
            confirm_sel(&futures::executor::block_on(root.node_traverse(
                ep(),
                HostKey::None,
                Vec::new(),
                "act ",
                &rctx(),
            ))),
            Some(0)
        );
    }

    #[test]
    fn confirm_frame_keeps_the_node_header_as_detail() {
        let (mut root, _p, _s) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let passive = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(confirm_detail(&passive), vec!["node guidance".to_string()]);
    }

    #[test]
    fn enter_on_accept_submits() {
        let (mut root, _p, submits) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(
            matches!(result, MenuResult::DoneSuccess(Some(_), _)),
            "accept must submit, got {:?}",
            result
        );
        assert_eq!(submits.borrow().len(), 1, "{:?}", submits.borrow());
    }

    #[test]
    fn enter_on_cancel_does_not_submit_and_stays_open() {
        let (mut root, _p, submits) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        // Move the dialog cursor onto `cancel`, then resolve.
        let moved = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(1),
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(confirm_sel(&moved), Some(1), "cursor must be on cancel");
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(
            matches!(result, MenuResult::Continue(..)),
            "cancel stays open, got {:?}",
            result
        );
        assert!(
            submits.borrow().is_empty(),
            "cancel must not submit: {:?}",
            submits.borrow()
        );
    }

    #[test]
    fn esc_cancels_in_place_without_submitting() {
        let (mut root, _p, submits) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Esc,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(
            matches!(result, MenuResult::Continue(..)),
            "Esc cancels in place, got {:?}",
            result
        );
        assert!(submits.borrow().is_empty(), "{:?}", submits.borrow());
        // Disarmed: the next Enter re-prepares rather than submitting.
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(submits.borrow().is_empty(), "still not submitted");
    }

    #[test]
    fn cursor_clamps_within_the_two_arms() {
        let (mut root, _p, _s) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        for _ in 0..3 {
            let r = futures::executor::block_on(root.node_traverse(
                ep(),
                HostKey::MoveCursor(1),
                Vec::new(),
                "act ",
                &rctx(),
            ));
            assert_eq!(confirm_sel(&r), Some(1), "clamped to the last arm");
        }
        for _ in 0..3 {
            let r = futures::executor::block_on(root.node_traverse(
                ep(),
                HostKey::MoveCursor(-1),
                Vec::new(),
                "act ",
                &rctx(),
            ));
            assert_eq!(confirm_sel(&r), Some(0), "clamped to the first arm");
        }
    }

    /// `Prepared(None)` takes the engine's defaults — a node that has no
    /// phrasing to offer still gets a working dialog.
    #[test]
    fn prepared_without_labels_falls_back_to_the_engine_defaults() {
        let (mut root, _p, _s) = confirm_leaf_root();
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let rows: Vec<String> = match &result {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].contains(CONFIRM_ACCEPT_LABEL), "{rows:?}");
        assert!(rows[1].contains(CONFIRM_CANCEL_LABEL), "{rows:?}");
        // The arm ids are engine-owned either way.
        assert!(rows[0].starts_with(CONFIRM_ACCEPT), "{rows:?}");
        assert!(rows[1].starts_with(CONFIRM_CANCEL), "{rows:?}");
    }

    /// The phrasing is captured at arm time: the node is not re-asked on
    /// later frames, so every frame of one confirm renders the same text
    /// even if the node's answer would have changed.
    #[test]
    fn arm_text_is_captured_at_arm_time() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(ConfirmLeaf::new("act"))));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // Arm with the default phrasing.
        let armed = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let armed_rows: Vec<String> = match &armed {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert!(
            armed_rows[0].contains(CONFIRM_ACCEPT_LABEL),
            "{armed_rows:?}"
        );

        // A later frame of the *same* confirm keeps that text — the
        // engine does not re-derive labels mid-confirm.
        let again = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let again_rows: Vec<String> = match &again {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert_eq!(armed_rows, again_rows, "the dialog text must be stable");
    }

    #[test]
    fn a_rewritten_buffer_disarms_the_dialog() {
        let (mut root, _p, submits) = confirm_leaf_root();
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        // The user edits the buffer: the confirm was armed for `act `.
        let r = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act other args",
            &rctx(),
        ));
        let arms = confirm_arms(r);
        assert!(
            !arms.iter().any(|a| a == CONFIRM_ACCEPT),
            "a rewrite must disarm, got {arms:?}"
        );
        // And Enter re-prepares rather than submitting the old payload.
        let r = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act other args",
            &rctx(),
        ));
        assert_eq!(
            confirm_arms(r),
            vec![CONFIRM_ACCEPT.to_string(), CONFIRM_CANCEL.to_string()],
            "Enter must re-arm, not submit the stale payload"
        );
        assert!(submits.borrow().is_empty(), "{:?}", submits.borrow());
    }

    #[test]
    fn node_can_phrase_the_arm_descriptions() {
        let mut root = NodeEntry::new_root();
        let mut leaf = ConfirmLeaf::new("act");
        leaf.descs = Some((
            "write this summary to the session".into(),
            "discard the draft".into(),
        ));
        root.children.push(NodeEntry::new_child(Box::new(leaf)));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let rows: Vec<String> = match &result {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert!(rows[0].contains("write this summary"), "{rows:?}");
        assert!(rows[1].contains("discard the draft"), "{rows:?}");
        // The values stay engine-owned regardless of the phrasing.
        assert!(rows[0].starts_with(CONFIRM_ACCEPT), "{rows:?}");
        assert!(rows[1].starts_with(CONFIRM_CANCEL), "{rows:?}");
    }

    #[test]
    fn default_arm_descriptions_are_accept_and_cancel() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(ConfirmLeaf::new("act"))));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        let rows: Vec<String> = match &result {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| l.0.clone()).collect(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert!(rows[0].contains(CONFIRM_ACCEPT_LABEL), "{rows:?}");
        assert!(rows[1].contains(CONFIRM_CANCEL_LABEL), "{rows:?}");
    }

    /// A listing node that arms through `edit()`: before the dialog arm
    /// existed this state was unreachable — Enter drilled and Esc ran
    /// the ladder, stranding the flag for the rest of the epoch.
    #[test]
    fn a_listing_level_confirm_is_resolvable() {
        let mut root = NodeEntry::new_root();
        let mut listing = ConfirmLeaf::new("act");
        listing.edit_reply = EditReply::Handled;
        // Arm through prepare() at the listing level: `Enter` on a
        // listing drills, so the editor path is the only arm — drive it
        // directly to keep the test about resolvability.
        let submits = Rc::clone(&listing.submits);
        let mut listing_entry = NodeEntry::new_child(Box::new(listing));
        // A child so this is a listing, not a leaf.
        listing_entry
            .children
            .push(NodeEntry::new_child(Box::new(ConfirmLeaf::new("nested"))));
        root.children.push(listing_entry);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // Epoch-gate-proof: prime the child so its first traverse at
        // `ep()` does not reset the state we set here.
        root.children[0].cache_epoch = ep();
        // Arm the listing's confirm directly (as `edit()` → Prepared
        // would), then check both resolve paths.
        arm(&mut root.children[0], "act ");
        let dialog = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(
            confirm_arms(dialog),
            vec![CONFIRM_ACCEPT.to_string(), CONFIRM_CANCEL.to_string()],
            "the listing's confirm must render its arms"
        );

        // Enter on accept submits the listing, rather than drilling.
        let accepted = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(
            matches!(accepted, MenuResult::DoneSuccess(..)),
            "accept must submit at a listing, got {:?}",
            accepted
        );
        assert_eq!(submits.borrow().len(), 1, "{:?}", submits.borrow());

        // Esc disarms instead of running the ladder.
        arm(&mut root.children[0], "act ");
        let cancelled = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Esc,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert!(
            matches!(cancelled, MenuResult::Continue(..)),
            "Esc must cancel in place, got {:?}",
            cancelled
        );
        // Disarmed: the completion gate is live again (a dot word opens
        // the dialog instead of being swallowed by a stuck flag).
        let after = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act .a",
            &rctx(),
        ));
        assert!(
            !confirm_arms(after).iter().any(|a| a.starts_with('.')),
            "flag must be cleared, not stranded"
        );
    }

    /// The dialog view is shared with the completion dialog, so neither
    /// may inherit the other's cursor.
    #[test]
    fn a_completion_cursor_does_not_leak_into_the_confirm_dialog() {
        // One node drives both dialogs, so they really do share the
        // `RowView` the leak would travel through.
        let mut root = NodeEntry::new_root();
        let mut leaf = ConfirmLeaf::new("act");
        leaf.completer = true;
        root.children.push(NodeEntry::new_child(Box::new(leaf)));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // Open the completion dialog on the leaf's in-flight word and
        // walk its cursor off row 0.
        let opened = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act al",
            &rctx(),
        ));
        assert!(
            matches!(&opened, MenuResult::Continue(plan, _) if plan.table_header.is_some()),
            "the completion dialog was up: {opened:?}"
        );
        let walked = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(1),
            Vec::new(),
            "act al",
            &rctx(),
        ));
        assert_eq!(confirm_sel(&walked), Some(1), "dialog cursor moved");

        // Arming the confirm resets the shared view to `accept`.
        let armed = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(
            confirm_arms(armed),
            vec![CONFIRM_ACCEPT.to_string(), CONFIRM_CANCEL.to_string()],
        );
        // And the completion dialog is down (the confirm owns the frame).
        let after = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "act ",
            &rctx(),
        ));
        assert_eq!(confirm_sel(&after), Some(0), "confirm starts on accept");
    }

    /// A root-like node with children AND a header.
    struct HeadedRoot {
        rows: Vec<crate::fields::Row>,
        header_text: String,
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for HeadedRoot {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: self.rows.clone(),
                active_ids: Vec::new(),
            }))
        }
        async fn header(
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
        root.children
            .push(NodeEntry::new_child(Box::new(CmdNode::new("item"))));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let headers: Vec<&str> = plan.detail.iter().map(|l| l.0.as_str()).collect();
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
        assert_eq!(mods(&p), vec![("sort".to_string(), "price".to_string())]);
    }

    #[test]
    fn parse_keeps_dot_words_flat() {
        let ri = HashMap::new();
        let p = parse(".sort:price ./src .gitignore", &ri);
        assert_eq!(pos(&p), vec![".sort:price", "./src", ".gitignore"]);
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::MoveCursor(1),
            Vec::new(),
            "",
            &rctx(),
        ));
        assert_eq!(root.view.cursor_idx, 1);
        // New epoch resets.
        let _ = futures::executor::block_on(root.node_traverse(
            ep() + 10,
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        assert_eq!(root.view.cursor_idx, 0);
    }

    // --- Esc clears filter ---

    #[test]
    fn esc_clears_filter() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    crate::fields::Row::new("m1").desc("first option"),
                    crate::fields::Row::new("m2").desc("second option"),
                ],
            })));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // "first" is in m1's description but not a row ID → fuzzy filter.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "first",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let row_count = plan.rows.len();
                assert_eq!(row_count, 1, "fuzzy 'first' should narrow to 1 row");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        // Esc on filtered view clears the refinement (host re-derives
        // the cleared prompt), or closes outright at root.
        let result = traverse_resolved(&mut root, HostKey::Esc, "first");
        match result {
            MenuResult::Continue(_, _) | MenuResult::DoneSuccess(..) => {}
            other => panic!("expected Continue or DoneSuccess on Esc, got {:?}", other),
        }
    }

    // --- Fuzzy rank orders best first ---

    #[test]
    fn fuzzy_rank_orders_best_first() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    crate::fields::Row::new("p:gpt4o").desc("GPT-4o flagship"),
                    crate::fields::Row::new("c:opus").desc("Claude opus"),
                    crate::fields::Row::new("p:gpt4-mini").desc("GPT-4 mini fast"),
                ],
            })));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "gpt",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts = rows_of(MenuResult::Continue(plan, String::new()));
                assert!(row_texts.iter().any(|t| t.contains("gpt4o")));
                assert!(row_texts.iter().any(|t| t.contains("gpt4-mini")));
                assert!(!row_texts.iter().any(|t| t.contains("opus")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // --- Step 5: Merged child set tests ---

    /// A leaf that records the path element it was dispatched from. The
    /// record cell is handed in by the test — once the leaf is boxed
    /// into the tree the test has no handle to it — so the test can
    /// assert *which* leaf received the submit.
    struct RecordingLeaf {
        submitted_via: Rc<std::cell::RefCell<Option<String>>>,
    }

    impl RecordingLeaf {
        fn new(record: Rc<std::cell::RefCell<Option<String>>>) -> Self {
            RecordingLeaf {
                submitted_via: record,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for RecordingLeaf {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![crate::fields::Row::new("action")],
                active_ids: Vec::new(),
            }))
        }
        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
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
        root.children
            .push(NodeEntry::new_child(Box::new(CmdNode::new("add"))));

        // Dynamic child: returns "bind1", "bind2" rows.
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    crate::fields::Row::new("bind1").desc("binding one"),
                    crate::fields::Row::new("bind2").desc("binding two"),
                ],
            })));

        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // Both children's rows should appear in the listing.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
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
        let add_seen: Rc<std::cell::RefCell<Option<String>>> =
            Rc::new(std::cell::RefCell::new(None));
        let bind_seen: Rc<std::cell::RefCell<Option<String>>> =
            Rc::new(std::cell::RefCell::new(None));
        let add_leaf = NodeEntry::new_child(Box::new(RecordingLeaf::new(Rc::clone(&add_seen))));
        let bind_leaf = NodeEntry::new_child(Box::new(RecordingLeaf::new(Rc::clone(&bind_seen))));

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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // Tab on "add" → drills into add_cmd → shows "action" leaf row.
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        // Move cursor to "add" (it's first) and Tab.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(_, prompt) => assert!(prompt.starts_with("add ")),
            other => panic!("expected Continue, got {:?}", other),
        }
        // Now at "add ", the add_cmd node shows its leaf child "action".
        // Tab into the leaf.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            "add ",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert!(prompt.starts_with("add action ")),
            other => panic!("expected ContinuePrompt drill, got {:?}", other),
        }
        // Enter on "add action " → submit.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "add action ",
            &rctx(),
        ));
        match result {
            MenuResult::DoneSuccess(..) => {}
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
        // Routing: only the add-branch leaf receives the submit, and it
        // records the row id it was dispatched from.
        assert_eq!(
            add_seen.borrow().as_deref(),
            Some("action"),
            "add leaf should record its dispatched row"
        );
        assert!(
            bind_seen.borrow().is_none(),
            "bind leaf must not receive the add-branch submit"
        );
    }

    #[test]
    fn mixed_children_fuzzy_filter_crosses_boundary() {
        // Fuzzy filter should narrow across both children.
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(CmdNode::new("add"))));
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    crate::fields::Row::new("bind1").desc("binding one"),
                    crate::fields::Row::new("bind2").desc("binding two"),
                ],
            })));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // "bind" fuzzy-matches both binding rows but not "add".
        let row_texts = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "bind",
            &rctx(),
        )));
        assert!(!row_texts.iter().any(|t| t.contains("add")));
        assert!(row_texts.iter().any(|t| t.contains("bind1")));
        assert!(row_texts.iter().any(|t| t.contains("bind2")));
        assert_eq!(row_texts.len(), 2);
    }

    #[test]
    fn empty_id_rows_skipped_with_warning() {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    crate::fields::Row::new("").desc("no id"), // empty id
                    crate::fields::Row::new("valid").desc("has id"),
                ],
            })));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));

        // Only "valid" should appear.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
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

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for RewriteLeaf {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![crate::fields::Row::new("rename").desc("rename session")],
                active_ids: Vec::new(),
            }))
        }

        async fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
            vec!["rename header".into()]
        }

        async fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
            Some(format!("abc rename {}", self.prefill))
        }
    }

    /// Build: root → [RewriteLeaf("rename")] as a single child.
    /// Root's Enter drill will send DrillEnter to the leaf.
    fn rewrite_root(prefill: &str) -> NodeEntry {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(RewriteLeaf {
                prefill: prefill.to_string(),
            })));
        root
    }

    #[test]
    fn drill_rewrite_fires_on_drill_enter() {
        let mut root = rewrite_root("MySession");
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // User presses Enter at root listing → drills into "rename" leaf
        // with DrillEnter → leaf's drill_rewrite fires.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, "abc rename MySession"),
            other => panic!("expected ContinuePrompt rewrite, got {:?}", other),
        }
    }

    #[test]
    fn drill_rewrite_fires_on_drill_tab() {
        let mut root = rewrite_root("TabFill");
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, "abc rename TabFill"),
            other => panic!("expected ContinuePrompt rewrite, got {:?}", other),
        }
    }

    #[test]
    fn drill_rewrite_does_not_fire_on_manual_enter() {
        let mut root = rewrite_root("MySession");
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // User types "rename " and presses Enter — suffix routes through
        // node recursion with HostKey::Enter (not DrillEnter).
        // drill_rewrite should NOT fire; leaf submits normally.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "rename ",
            &rctx(),
        ));
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
        root.children
            .push(NodeEntry::new_child(Box::new(LeafNode::new())));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // User presses Enter at root → drills into leaf with DrillEnter.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "",
            &rctx(),
        ));
        match result {
            MenuResult::DoneSuccess(_, _) => { /* leaf submitted */ }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    // --- Completion (implicit-open dialog; steps 4–6) ---

    /// Listing node whose rows carry typed fields (inferred field space:
    /// id, description, tier: string, price: number + injected sort).
    struct FieldedNode;

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for FieldedNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
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

    /// Rendered row texts from a Continue result, with ANSI codes stripped.
    fn rows_of(result: MenuResult) -> Vec<String> {
        match result {
            MenuResult::Continue(plan, _) => plan.rows.iter().map(|l| strip_ansi(&l.0)).collect(),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// Strip ANSI SGR escape sequences from a string.
    fn strip_ansi(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Skip escape sequence
                for e in chars.by_ref() {
                    if e.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            result.push(c);
        }
        result
    }

    #[test]
    fn completion_key_mode_lists_field_names() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".t",
            &rctx(),
        ));
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".",
            &rctx(),
        )));
        for k in [".id:", ".description:", ".tier:", ".price:", ".sort:"] {
            assert!(rows.iter().any(|r| r.contains(k)), "missing {k}: {rows:?}");
        }
        assert_eq!(rows.len(), 5, "{rows:?}");
    }

    #[test]
    fn completion_ignores_plain_words() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "m1",
            &rctx(),
        )));
        // No dialog: the normal fuzzy listing renders.
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:?}");
    }

    #[test]
    fn completion_value_mode_string_field() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:fr",
            &rctx(),
        )));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].contains(".tier:free"), "{rows:?}");

        // Empty prefix: every distinct value (dataset-derived, D7).
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:",
            &rctx(),
        )));
        assert!(rows.iter().any(|r| r.contains(".tier:free")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains(".tier:paid")), "{rows:?}");
    }

    #[test]
    fn completion_exact_match_closes_dialog() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // `.tier:free` is an exact unique match → no dialog; the normal
        // listing renders (filtered by the modifier to m1/m3).
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:free",
            &rctx(),
        )));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("m3")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("m2")), "{rows:?}");
    }

    #[test]
    fn completion_number_and_unknown_keys_stay_inert() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        for suffix in [".price:1", ".nope:x", "..", "./src"] {
            let rows = rows_of(futures::executor::block_on(root.node_traverse(
                ep(),
                HostKey::None,
                Vec::new(),
                suffix,
                &rctx(),
            )));
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".srot:price",
            &rctx(),
        )));
        assert!(
            rows.is_empty(),
            "fuzzy '.srot:price' matches nothing: {rows:?}"
        );
    }

    #[test]
    fn completion_mid_cursor_suppressed() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // v1: cursor mid-line → completion suppressed (EOL guard), even
        // though the word under the cursor would otherwise complete.
        let result = futures::executor::block_on(root.node_traverse_at(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:fr",
            3,
            &rctx(),
        ));
        let rows = rows_of(result);
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_space_dismisses() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".tier:free ",
            &rctx(),
        )));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_dialog_cursor_moves_and_persists() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let sel_of = |root: &mut NodeEntry, key: HostKey| match futures::executor::block_on(
            root.node_traverse(ep(), key, Vec::new(), ".t", &rctx()),
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
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".t ",
            &rctx(),
        )));
        assert!(!rows.iter().any(|r| r.starts_with('.')), "{rows:?}");
    }

    #[test]
    fn completion_tab_expands_key_and_progresses_to_values() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // `.so` → prefix match `.sort:` ranked first; `.description:`
        // (subsequence s…o) follows in the fuzzy tier.
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".so",
            &rctx(),
        )));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            rows[0].contains(".sort:"),
            "prefix must rank first: {rows:?}"
        );
        // Tab: in-place expansion, NO trailing space — the same-turn
        // re-run lands in value mode (non-empty) and keeps going.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            ".so",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, ".sort:"),
            other => panic!("expected ContinuePrompt expansion, got {:?}", other),
        }
    }

    #[test]
    fn completion_tab_final_value_appends_trailing_space() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // `.tier:fr` → Tab expands to `.tier:free`; the same-turn
        // re-run is an exact/final match (empty) → trailing space.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            ".tier:fr",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, ".tier:free "),
            other => panic!("expected ContinuePrompt expansion, got {:?}", other),
        }
    }

    /// `.sort:<field>` is terminal, so the exactness contract is the only
    /// thing that lets `Tab` commit it: the expanded word must report
    /// "nothing left to complete", else the re-run keeps the dialog up and
    /// Tab degenerates into a no-op that rewrites the prompt unchanged.
    #[test]
    fn completion_tab_final_sort_field_appends_trailing_space() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // `.sort:t` → Tab expands to `.sort:tier`; the same-turn re-run
        // over the expanded word finds only the word itself → trailing
        // space (commits the modifier, closes the dialog).
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Tab,
            Vec::new(),
            ".sort:t",
            &rctx(),
        ));
        match result {
            MenuResult::ContinuePrompt(prompt) => assert_eq!(prompt, ".sort:tier "),
            other => panic!("expected ContinuePrompt expansion, got {:?}", other),
        }
    }

    /// The committed word must not re-open the dialog: `.sort:tier` is an
    /// exact match, so the passive frame falls through to the listing
    /// (sorted by `tier`) rather than offering the modifier again.
    #[test]
    fn completion_exact_sort_field_closes_dialog() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".sort:tier",
            &rctx(),
        ));
        let dialog = match &result {
            MenuResult::Continue(plan, _) => plan.table_header.is_some(),
            other => panic!("expected Continue, got {:?}", other),
        };
        assert!(!dialog, "an exact sort field must not re-open the dialog");
        let rows = rows_of(result);
        assert!(rows.iter().any(|r| r.contains("m1")), "{rows:?}");
    }

    /// A direction suffix is free-form and matches no field name, so the
    /// dialog is already down there — Tab must fall through to the
    /// ordinary listing Tab (drill), never a rewrite.
    #[test]
    fn completion_sort_direction_suffix_is_terminal() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".sort:tier>",
            &rctx(),
        )));
        assert!(
            !rows.iter().any(|r| r.starts_with('.')),
            "`>` matches no field name: {rows:?}"
        );
    }

    #[test]
    fn completion_confirm_pending_suppresses_dialog() {
        let mut root = fielded_root();
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // Init epoch state at this epoch.
        let _ = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "",
            &rctx(),
        ));
        arm(&mut root, ".t");
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".t",
            &rctx(),
        )));
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

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for PathLeaf {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![Row::new("pathpick").desc("pick a path")],
                active_ids: Vec::new(),
            }))
        }

        async fn complete_parameter(
            &mut self,
            _args: &PathArgs,
            _cursor_pos: usize,
        ) -> Result<Arc<CompletionRows>, DataError> {
            if let Some(m) = &self.err {
                return Err(DataError { message: m.clone() });
            }
            Ok(Arc::new(CompletionRows {
                rows: self.rows.clone(),
                labels: None,
                match_indices: None,
            }))
        }

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        (root, rec)
    }

    #[test]
    fn leaf_completion_dialog_and_enter_bypasses_to_submit() {
        let (mut root, rec) = leaf_root();
        // Resolved INTO the leaf (matched id + tail): trait completion.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        ));
        match &result {
            MenuResult::Continue(plan, _) => {
                assert!(plan.table_header.is_some(), "dialog columns expected");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        let rows = rows_of(result);
        assert!(rows.iter().any(|r| r.contains("/home/x/proj")), "{rows:?}");

        // Enter bypasses the dialog entirely: submits the RAW word.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        ));
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h",
            &rctx(),
        ));
        match result {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.detail.len(), 1, "{:?}", plan.detail);
                assert!(
                    plan.detail[0].0.contains("[ERROR]: boom"),
                    "{:?}",
                    plan.detail[0]
                );
                assert!(plan.rows.is_empty(), "no dialog on Err: {:?}", plan.rows);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    /// Param leaf: `submit` fails without an arg (the sandbox-shaped
    /// drill guard) and succeeds with one.
    struct ParamLeaf;

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for ParamLeaf {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
                rows: vec![Row::new("param").desc("needs an arg")],
                active_ids: Vec::new(),
            }))
        }

        async fn header(
            &mut self,
            _path_args: &PathArgs,
            _render_ctx: &RenderContext,
        ) -> Vec<String> {
            vec!["Specify a host path.".to_string()]
        }

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            if path_args.args().next().is_none() {
                SubmitResult::Error("host path required".to_string())
            } else {
                SubmitResult::Done(None)
            }
        }
    }

    fn param_root() -> NodeEntry {
        let mut root = NodeEntry::new_root();
        root.children
            .push(NodeEntry::new_child(Box::new(ParamLeaf)));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        root
    }

    #[test]
    fn submit_error_renders_inline_above_header_and_stays_open() {
        let mut root = param_root();
        // Drill arrival: Enter on the bare id forwards DrillEnter into
        // the leaf with an empty suffix → submit sees no args → Error.
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "param",
            &rctx(),
        ));
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
        let first = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "param",
            &rctx(),
        ));
        assert!(matches!(first, MenuResult::Continue(..)), "{:?}", first);
        // The next frame over the SAME prompt re-derives without the
        // error: nothing was stored (direct render, stateless).
        let second = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "param ",
            &rctx(),
        ));
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
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::Enter,
            Vec::new(),
            "param /home/gab",
            &rctx(),
        ));
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
        let result = futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "pathpick /h ",
            &rctx(),
        ));
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

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for RankedFieldsNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Arc::new(DataResponse {
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
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        // `.pri` → prefix tier, shortest first, ignoring declaration
        // order: price, price_input, price_output.
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".pri",
            &rctx(),
        )));
        assert_eq!(
            first_tok(&rows),
            vec![".price:", ".price_input:", ".price_output:"],
            "{rows:?}"
        );
        // `.so` → prefix `.sort:` first, fuzzy tier after.
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            ".so",
            &rctx(),
        )));
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
        root.children
            .push(NodeEntry::new_child(Box::new(DynamicNode {
                rows: vec![
                    Row::new("model_xtra_long").desc("long"),
                    Row::new("alphabet").desc("modular parts"),
                    Row::new("model").desc("short"),
                    Row::new("models").desc("plural"),
                ],
            })));
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let rows = rows_of(futures::executor::block_on(root.node_traverse(
            ep(),
            HostKey::None,
            Vec::new(),
            "mod",
            &rctx(),
        )));
        let ids = first_tok(&rows);
        assert_eq!(
            ids,
            vec!["model", "models", "model_xtra_long", "alphabet"],
            "{ids:?}"
        );
    }

    /// 4i-a: a node that mints a fresh response every call (the common,
    /// non-memoizing case) contributes no change when its content is
    /// unchanged, so the stored handle is reused and `slots_dirty` stays
    /// clear — the flicker-free carry on the real menu path.
    #[test]
    fn fresh_but_equal_response_reuses_the_stored_handle() {
        let mut root = root_with_cmds(vec![CmdNode::new("a")]);
        assert!(
            futures::executor::block_on(root.refresh_data(ep(), &[], "")),
            "first scan is a change"
        );
        let stored = Arc::clone(root.content.data_rc[0].as_ref().unwrap());
        // Stand in for the view pipeline having consumed the change.
        root.content.slots_dirty = false;

        // `CmdNode::data` builds a fresh `Arc` each call; the fallback
        // compares content and reuses the stored handle.
        assert!(
            !futures::executor::block_on(root.refresh_data(ep(), &[], "")),
            "an equal-but-fresh response is not a change"
        );
        assert!(!root.content.slots_dirty, "slots stay clean");
        assert!(
            Arc::ptr_eq(&stored, root.content.data_rc[0].as_ref().unwrap()),
            "the stored handle is reused, not the fresh one"
        );
    }

    /// The fallback still reports a real change: differing rows mint a new
    /// handle and dirty the slots.
    #[test]
    fn differing_response_is_a_change() {
        let mut root = root_with_cmds(vec![CmdNode::new("a")]);
        futures::executor::block_on(root.refresh_data(ep(), &[], ""));
        let stored = Arc::clone(root.content.data_rc[0].as_ref().unwrap());
        root.content.slots_dirty = false;

        // Swap the child's content.
        root.children[0].inner = Box::new(CmdNode::new("b"));
        assert!(
            futures::executor::block_on(root.refresh_data(ep(), &[], "")),
            "changed content is a change"
        );
        assert!(root.content.slots_dirty, "slots dirtied");
        assert!(
            !Arc::ptr_eq(&stored, root.content.data_rc[0].as_ref().unwrap()),
            "a new handle for changed content"
        );
    }

    #[test]
    fn same_content_compares_rows_and_active_ids_as_a_set() {
        let base = DataResponse {
            rows: vec![crate::fields::Row::new("a").desc("ay")],
            active_ids: vec!["a".into(), "b".into()],
        };
        // Same rows, active ids in a different order: same content.
        let reordered = DataResponse {
            rows: base.rows.clone(),
            active_ids: vec!["b".into(), "a".into()],
        };
        assert!(base.same_content(&reordered));

        // A different row breaks it.
        let other_row = DataResponse {
            rows: vec![crate::fields::Row::new("a").desc("bee")],
            active_ids: base.active_ids.clone(),
        };
        assert!(!base.same_content(&other_row));

        // A different active-id set breaks it.
        let other_active = DataResponse {
            rows: base.rows.clone(),
            active_ids: vec!["a".into()],
        };
        assert!(!base.same_content(&other_active));
    }
}
