//! Presentation side of one menu level — the content vs presentation
//! cut (doc/slashdot/arg_and_modifier_autocomplete.md).
//!
//! `ViewRows` is the single input: source rows + display order + active
//! markers behind one Rc, minted by the content side (rows pipeline or
//! completer wrap). `RowView` owns the cursor and the row-line render
//! cache and assembles `RenderPlan`s. The render side never sees how
//! rows are stored: it mints `iter()`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::fields::ColumnDef;
use crate::fields::{row_field_value, Row};
use crate::node_entry::RowRef;
use crate::node_impl::{DataResponse, RenderContext};
use crate::render::{LogicalLine, RenderPlan};
use crate::table::{display_width, highlight_matches};

/// The view's single input: source rows + display order + markers,
/// bundled behind one Rc so "data OR query OR typed input changed"
/// collapses to one ptr_eq signal. Minted content-side only — fields
/// private: storage (slots / RowRef / id set) is an implementation
/// detail the render side never sees.
pub struct ViewRows {
    slots: Vec<Option<Rc<DataResponse>>>,
    order: Vec<RowRef>,
    active_ids: HashSet<String>,
    /// Per-row match indices for fuzzy highlighting: RowRef → (field_name → byte indices).
    /// Only populated for string columns that are displayed and have fuzzy terms.
    match_indices: HashMap<RowRef, HashMap<String, Vec<usize>>>,
}

impl ViewRows {
    /// Content-side constructor (rows pipeline / completer wrap only).
    pub(crate) fn new(
        slots: Vec<Option<Rc<DataResponse>>>,
        order: Vec<RowRef>,
        active_ids: HashSet<String>,
        match_indices: HashMap<RowRef, HashMap<String, Vec<usize>>>,
    ) -> Self {
        ViewRows {
            slots,
            order,
            active_ids,
            match_indices,
        }
    }

    /// Content-side constructor without match indices (backward compatible).
    pub(crate) fn new_empty_match(
        slots: Vec<Option<Rc<DataResponse>>>,
        order: Vec<RowRef>,
        active_ids: HashSet<String>,
    ) -> Self {
        ViewRows {
            slots,
            order,
            active_ids,
            match_indices: HashMap::new(),
        }
    }

    /// Completion source → view: one slot, identity order, no active
    /// markers. The public entry for a completer's `Vec<Row>` (host
    /// prose completion and the engine's `wrap_completion` shape).
    pub fn of(rows: Vec<Row>) -> Rc<ViewRows> {
        let n = rows.len();
        let resp = Some(Rc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }));
        Rc::new(ViewRows::new_empty_match(
            vec![resp],
            (0..n as u32)
                .map(|i| RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            HashSet::new(),
        ))
    }

    /// Completion source → view with match indices.
    pub fn of_with_match_indices(
        rows: Vec<Row>,
        match_indices: HashMap<RowRef, HashMap<String, Vec<usize>>>,
    ) -> Rc<ViewRows> {
        let n = rows.len();
        let resp = Some(Rc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }));
        Rc::new(ViewRows::new(
            vec![resp],
            (0..n as u32)
                .map(|i| RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            HashSet::new(),
            match_indices,
        ))
    }

    /// The render side's ONLY data surface: rows in display order with
    /// the active (`*`) flag resolved — resolution and active-set
    /// lookup happen inside. Stale handles degrade to skipped rows
    /// (§3.6). `iter().enumerate()` gives render + active indices;
    /// `position`/`nth` give seeding and the cursor row.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Row, bool)> + '_ {
        self.order.iter().filter_map(|rref| {
            let row = rref.resolve(&self.slots)?;
            Some((row, self.active_ids.contains(&row.id)))
        })
    }

    /// Get match indices for a specific row (for fuzzy highlighting).
    /// Returns field_name → byte indices, or empty if none.
    pub(crate) fn match_indices(&self, rref: &RowRef) -> Option<&HashMap<String, Vec<usize>>> {
        self.match_indices.get(rref)
    }

    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Per-view render cache: table header + row lines + active indices.
/// Row lines depend only on (view, columns, render_ctx).
#[derive(Default)]
struct RenderCache {
    table_header: Option<LogicalLine>,
    rows: Rc<Vec<LogicalLine>>,
    active_rows: Vec<usize>,
}

/// Everything per-frame that isn't the rows themselves.
pub struct ViewRequest<'a> {
    /// Listing: the node's `columns()`. Completion: two ColumnDefs
    /// built from the response labels.
    pub columns: &'a [ColumnDef],
    /// Fresh this frame: optional `[ERROR]` line + `header()`.
    pub detail: Vec<LogicalLine>,
    /// Cursor reconcile target when the view changed (matched child id
    /// / restore target); `None` for completion.
    pub seed_id: Option<&'a str>,
    /// §5.1 restore only: force cursor without a view change.
    pub pin: Option<&'a str>,
    /// Applied after reconcile (MoveCursor / page keys).
    pub cursor_delta: Option<i32>,
    /// Full render context — row lines rebuild when it differs
    /// (`PartialEq`); future metrics land here without API changes.
    pub render_ctx: RenderContext,
}

impl<'a> Default for ViewRequest<'a> {
    fn default() -> Self {
        ViewRequest {
            columns: &[],
            detail: Vec::new(),
            seed_id: None,
            pin: None,
            cursor_delta: None,
            render_ctx: RenderContext { term_width: 0 },
        }
    }
}

/// What one frame needs: the rendered plan + the cursor target.
pub struct ViewPlan {
    /// Row under the cursor after reconcile/delta — the drill /
    /// expansion target. `None` when the view has no rows.
    pub cursor_row: Option<Row>,
    pub plan: RenderPlan,
}

pub struct RowView {
    /// Last view passed to plan; ptr-diff vs incoming = dirty.
    last_view: Option<Rc<ViewRows>>,
    // --- cursor (per view; pub(crate) for cross-module tests) ---
    pub(crate) cursor_id: Option<String>,
    pub(crate) cursor_idx: usize,
    pub(crate) cursor_user_moved: bool,
    // --- render cache ---
    last_render_ctx: RenderContext,
    render: RenderCache,
}

impl RowView {
    pub fn new() -> Self {
        RowView {
            last_view: None,
            cursor_id: None,
            cursor_idx: 0,
            cursor_user_moved: false,
            last_render_ctx: RenderContext { term_width: 0 },
            render: RenderCache::default(),
        }
    }

    /// One-shot: ptr-compare `view` vs last → cursor reconcile →
    /// pin → cursor_delta → rebuild row lines iff view changed or
    /// `render_ctx` changed → assemble. No key, no query: all caching
    /// is ptr/context driven and internal. `view` is taken by value
    /// (stored as `last_view`); the returned `cursor_row` covers
    /// drill/expand — callers need no second call.
    pub fn plan(&mut self, view: Rc<ViewRows>, req: &ViewRequest<'_>) -> ViewPlan {
        // 1. View changed → cursor reconcile (today's seed_or_keep).
        let changed = match &self.last_view {
            Some(prev) => !Rc::ptr_eq(prev, &view),
            None => true,
        };
        if changed {
            self.reconcile(&view, req.seed_id);
        }
        // 2. Pin (§5.1 restore — applies even with no view change, which
        //    is what keeps restore cheap: memo hit ⇒ same Rc ⇒ no
        //    reconcile ⇒ pin alone moves the cursor).
        if let Some(id) = req.pin {
            self.cursor_id = Some(id.to_string());
            self.cursor_idx = view.iter().position(|(r, _)| r.id == id).unwrap_or(0);
        }
        // 3. Cursor delta (after reconcile — fresh view, today's order).
        if let Some(d) = req.cursor_delta {
            let n = view.len();
            if n > 0 {
                self.cursor_idx =
                    (self.cursor_idx as isize + d as isize).clamp(0, (n - 1) as isize) as usize;
                self.cursor_id = view.iter().nth(self.cursor_idx).map(|(r, _)| r.id.clone());
            }
            self.cursor_user_moved = true;
        }
        // 4. Rebuild row lines iff view changed or render ctx changed;
        //    detail is always fresh (passed in per call).
        if changed || self.last_render_ctx != req.render_ctx {
            self.rebuild_rows(&view, req.columns, req.render_ctx);
            self.last_render_ctx = req.render_ctx;
        }
        self.last_view = Some(Rc::clone(&view));
        // 5. Assemble.
        let selected = if view.is_empty() {
            None
        } else {
            Some(self.cursor_idx)
        };
        let cursor_row = view.iter().nth(self.cursor_idx).map(|(r, _)| r.clone());
        ViewPlan {
            cursor_row,
            plan: RenderPlan {
                table_header: self.render.table_header.clone(),
                detail: req.detail.clone(),
                rows: Rc::clone(&self.render.rows),
                selected,
                active_rows: self.render.active_rows.clone(),
            },
        }
    }

    /// Epoch gate / dialog close: drop view + cursor + caches.
    pub fn reset(&mut self) {
        self.last_view = None;
        self.cursor_id = None;
        self.cursor_idx = 0;
        self.cursor_user_moved = false;
        self.last_render_ctx = RenderContext { term_width: 0 };
        self.render = RenderCache::default();
    }

    /// Cursor reconcile on view change (today's `seed_or_keep_cursor`):
    /// `!user_moved` → re-seed (seed_id → first active → first row);
    /// `user_moved` → keep the id iff still present, else drop the flag
    /// and re-seed. Completion degenerates to keep-id-else-row0 (no
    /// seed, no active).
    fn reconcile(&mut self, view: &ViewRows, seed_id: Option<&str>) {
        if self.cursor_user_moved {
            if let Some(id) = self.cursor_id.clone() {
                if let Some(idx) = view.iter().position(|(r, _)| r.id == id) {
                    self.cursor_idx = idx;
                    return;
                }
            }
            self.cursor_user_moved = false;
        }
        // Re-seed: selection → first active → first row.
        self.cursor_id = seed_id
            .map(|s| s.to_string())
            .or_else(|| view.iter().find(|(_, a)| *a).map(|(r, _)| r.id.clone()))
            .or_else(|| view.iter().next().map(|(r, _)| r.id.clone()));
        self.cursor_idx = 0;
        if let Some(id) = self.cursor_id.clone() {
            if let Some(idx) = view.iter().position(|(r, _)| r.id == id) {
                self.cursor_idx = idx;
            }
        }
    }

    /// Rebuild table header + row lines + active indices from scratch
    /// (extracted from `NodeEntry::build_render_plan`).
    fn rebuild_rows(&mut self, view: &ViewRows, columns: &[ColumnDef], render_ctx: RenderContext) {
        use crate::table::{extract_snippet, Align, Table};
        let col_field_names: Vec<String> = if !columns.is_empty() {
            columns.iter().map(|c| c.field.clone()).collect()
        } else {
            vec!["id".to_string(), "description".to_string()]
        };
        let has_table_header =
            !columns.is_empty() && !view.is_empty() && columns.iter().any(|c| !c.header.is_empty());
        // Column count comes from the labels (Table::new), so the
        // default id/description pair must be passed even when the
        // header row itself is never rendered (has_table_header false).
        let header_labels: Vec<&str> = if !columns.is_empty() {
            columns.iter().map(|c| c.header.as_str()).collect()
        } else {
            col_field_names.iter().map(|s| s.as_str()).collect()
        };

        let width = render_ctx.term_width.saturating_sub(2);
        // Wrapping vs single-line is decided by the columndefs; when there
        // are none, the implicit id/description pair wraps (matching today).
        let use_wrap = columns.iter().all(|c| c.wrap);

        // Two-pass when single-line: compute column widths from the raw
        // (unhighlighted) cells first, so snippet extraction can target the
        // exact budget a cell will be rendered into. The wrap path needs no
        // snippet (wrapping handles overflow), so it stays single-pass.
        let col_widths: Vec<usize> = if use_wrap {
            Vec::new()
        } else {
            let mut temp = Table::new(&header_labels).set_gap(2);
            if !columns.is_empty() {
                let aligns: Vec<Align> = columns.iter().map(|c| c.align).collect();
                let mins: Vec<usize> = columns.iter().map(|c| c.min_width).collect();
                let prios: Vec<usize> = columns.iter().map(|c| c.priority).collect();
                temp = temp
                    .with_aligns(&aligns)
                    .with_min_widths(&mins)
                    .with_priorities(&prios);
            }
            for (row, _active) in view.iter() {
                let raw: Vec<String> = col_field_names
                    .iter()
                    .map(|k| {
                        row.field_display(k)
                            .or_else(|| {
                                row_field_value(row, k).map(|v| match v {
                                    crate::value::Value::Str(s) => s,
                                    crate::value::Value::Num(n) => crate::value::fmt_num(n),
                                })
                            })
                            .unwrap_or_default()
                    })
                    .collect();
                temp.add_row(&raw);
            }
            temp.column_widths(width)
        };

        let mut t = Table::new(&header_labels).set_gap(2);
        if !columns.is_empty() {
            let aligns: Vec<Align> = columns.iter().map(|c| c.align).collect();
            let mins: Vec<usize> = columns.iter().map(|c| c.min_width).collect();
            let prios: Vec<usize> = columns.iter().map(|c| c.priority).collect();
            t = t
                .with_aligns(&aligns)
                .with_min_widths(&mins)
                .with_priorities(&prios);
        }
        let mut active_rows = Vec::new();
        for (i, (row, active)) in view.iter().enumerate() {
            if active {
                active_rows.push(i);
            }
            let rref = view.order[i];
            let match_indices = view.match_indices(&rref);
            let mut cells: Vec<String> = Vec::with_capacity(col_field_names.len());
            for (col_idx, k) in col_field_names.iter().enumerate() {
                let cell = row
                    .field_display(k)
                    .or_else(|| {
                        row_field_value(row, k).map(|v| match v {
                            crate::value::Value::Str(s) => s,
                            crate::value::Value::Num(n) => crate::value::fmt_num(n),
                        })
                    })
                    .unwrap_or_default();

                // Highlight fuzzy matches on every string column (explicit
                // ColumnDefs and the implicit id/description pair alike).
                let cell = if let Some(indices_map) = match_indices {
                    if let Some(indices) = indices_map.get(k) {
                        // Single-line column whose content would truncate:
                        // extract a snippet centered on the matches (and
                        // highlight it). Otherwise highlight in place (the
                        // wrap path renders the whole cell).
                        let col_w = col_widths.get(col_idx).copied().unwrap_or(0);
                        if !use_wrap && col_w > 0 && display_width(&cell) > col_w {
                            extract_snippet(&cell, indices, col_w)
                        } else {
                            highlight_matches(&cell, indices)
                        }
                    } else {
                        cell
                    }
                } else {
                    cell
                };
                cells.push(cell);
            }
            t.add_row(&cells);
        }

        let table_header = if has_table_header {
            Some(LogicalLine::new(t.render_header(width)))
        } else {
            None
        };
        let lines: Vec<LogicalLine> = if use_wrap {
            t.render_multiline(width)
        } else {
            t.render(width)
        }
        .into_iter()
        .map(LogicalLine::new)
        .collect();
        self.render.table_header = table_header;
        self.render.rows = Rc::new(lines);
        self.render.active_rows = active_rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(rows: Vec<Row>) -> Option<Rc<DataResponse>> {
        Some(Rc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }))
    }

    fn view_of(rows: Vec<Row>) -> Rc<ViewRows> {
        let n = rows.len();
        Rc::new(ViewRows::new(
            vec![resp(rows)],
            (0..n as u32)
                .map(|i| crate::node_entry::RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            HashSet::new(),
            HashMap::new(),
        ))
    }

    fn req() -> ViewRequest<'static> {
        ViewRequest {
            render_ctx: RenderContext { term_width: 80 },
            ..Default::default()
        }
    }

    fn rows3() -> Rc<ViewRows> {
        view_of(vec![Row::new("a"), Row::new("b"), Row::new("c")])
    }

    #[test]
    fn first_plan_seeds_row0_and_returns_cursor_row() {
        let mut v = RowView::new();
        let out = v.plan(rows3(), &req());
        assert_eq!(out.plan.selected, Some(0));
        assert_eq!(out.cursor_row.map(|r| r.id), Some("a".to_string()));
    }

    #[test]
    fn delta_moves_cursor() {
        let mut v = RowView::new();
        let view = rows3();
        v.plan(Rc::clone(&view), &req());
        let mut r = req();
        r.cursor_delta = Some(2);
        let out = v.plan(view, &r);
        assert_eq!(out.plan.selected, Some(2));
        assert_eq!(out.cursor_row.map(|r| r.id), Some("c".to_string()));
    }

    #[test]
    fn same_rc_keeps_cursor_no_reconcile() {
        let mut v = RowView::new();
        let view = rows3();
        let mut r = req();
        r.cursor_delta = Some(1);
        v.plan(Rc::clone(&view), &r);
        // Same Rc handed again: cursor must survive even though a naive
        // reconcile (!user_moved path is skipped — user_moved is true).
        let out = v.plan(Rc::clone(&view), &req());
        assert_eq!(out.plan.selected, Some(1));
    }

    #[test]
    fn view_change_reseeds_when_cursor_dropped() {
        let mut v = RowView::new();
        let mut r = req();
        r.cursor_delta = Some(2);
        v.plan(rows3(), &r); // cursor on "c", user_moved = true
                             // New view without "c": drop flag, re-seed to row0.
        let out = v.plan(view_of(vec![Row::new("x"), Row::new("y")]), &req());
        assert_eq!(out.plan.selected, Some(0));
    }

    #[test]
    fn seed_id_used_on_reconcile() {
        let mut v = RowView::new();
        let mut r = req();
        r.seed_id = Some("b");
        let out = v.plan(rows3(), &r);
        assert_eq!(out.plan.selected, Some(1));
    }

    #[test]
    fn pin_forces_cursor_without_view_change() {
        let mut v = RowView::new();
        let view = rows3();
        v.plan(Rc::clone(&view), &req());
        let mut r = req();
        r.pin = Some("c");
        let out = v.plan(view, &r); // identical Rc — pin still applies
        assert_eq!(out.plan.selected, Some(2));
    }

    #[test]
    fn active_rows_resolved_through_iterator() {
        let slots = vec![Some(Rc::new(DataResponse {
            rows: vec![Row::new("a"), Row::new("b")],
            active_ids: vec!["b".to_string()],
        }))];
        let view = Rc::new(ViewRows::new(
            slots,
            vec![
                crate::node_entry::RowRef {
                    child_idx: 0,
                    row_offset: 0,
                },
                crate::node_entry::RowRef {
                    child_idx: 0,
                    row_offset: 1,
                },
            ],
            HashSet::from(["b".to_string()]),
            HashMap::new(),
        ));
        let flags: Vec<bool> = view.iter().map(|(_, a)| a).collect();
        assert_eq!(flags, vec![false, true]);
        let mut v = RowView::new();
        let out = v.plan(view, &req());
        assert_eq!(out.plan.active_rows, vec![1]);
    }

    #[test]
    fn empty_view_selected_none_and_no_cursor_row() {
        let mut v = RowView::new();
        let out = v.plan(view_of(vec![]), &req());
        assert_eq!(out.plan.selected, None);
        assert!(out.cursor_row.is_none());
    }

    #[test]
    fn reset_clears_cursor() {
        let mut v = RowView::new();
        let mut r = req();
        r.cursor_delta = Some(1);
        v.plan(rows3(), &r);
        assert_eq!(v.cursor_idx, 1);
        v.reset();
        assert_eq!(v.cursor_idx, 0);
        assert!(v.last_view.is_none());
    }

    #[test]
    fn of_mints_identity_order_view() {
        // Public completion entry: single slot, identity order, no
        // active markers — the shape prose completion feeds to plan().
        let view = ViewRows::of(vec![Row::new("a"), Row::new("b")]);
        assert_eq!(view.len(), 2);
        let ids: Vec<&str> = view
            .iter()
            .map(|(r, a)| {
                assert!(!a, "completion rows never carry active markers");
                r.id.as_str()
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn plan_over_of_round_trips_selection() {
        let mut v = RowView::new();
        let out = v.plan(ViewRows::of(vec![Row::new("x"), Row::new("y")]), &req());
        assert_eq!(out.plan.selected, Some(0));
        assert_eq!(out.cursor_row.map(|r| r.id), Some("x".to_string()));
        let mut r = req();
        r.cursor_delta = Some(1);
        let out = v.plan(ViewRows::of(vec![Row::new("x"), Row::new("y")]), &r);
        assert_eq!(out.cursor_row.map(|r| r.id), Some("y".to_string()));
    }
}
