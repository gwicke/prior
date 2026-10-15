//! The frame elements a [`RenderPlan`] composes into (design
//! §RenderElement stack): one element per view section — detail, table
//! header, scrollable rows.
//!
//! Elements are built **once, from raw data** and pull their state inside
//! `render(h, w)` at the width being composed for. A plan that carries its
//! [`view`](RenderPlan::view) handle (every engine plan) mints elements that
//! hold the raw view and lay the table out at the render width — the
//! iterator logic (order-driven resolution, stale-slot skips, active-flag
//! lookup) runs *in* the rows element, and no row is copied into it. A
//! hand-built plan (the confirm dialog, tests) mints elements over its
//! pre-laid-out rows instead; both paths paint the same chrome.
//!
//! Nothing here sees a painted row before the width is known, so there is
//! no compose-width `PaintedSection` snapshot on either path.
//!
//! Since 4h-e a host that keeps a menu open mints an [`ElementSet`] — the
//! elements plus the [`Selection`] figures a keystroke moves — and a cursor
//! move updates the figures **in place** through the shared handle, without
//! re-minting (design §RenderElement stack: "a menu open constructs its
//! elements; a keystroke moves the selection through shared handles").
//!
//! [`window_keeps`] rides beside [`window_heights`]: pure windowing, and
//! its only user is the rows element below.

use std::sync::{Arc, Mutex};

use slashdot_tui::render::{pad_row, Element, RenderElement, Rendered, RowAsk};

use crate::fields::ColumnDef;
use crate::render::{
    paint_detail, paint_header, paint_rows, visual_height, window_heights, LogicalLine, RenderPlan,
    MARKER,
};
use crate::view::{assemble_table, has_table_header, ViewRows};

// --- the shared table layout ------------------------------------------------

/// One table laid out at one width, shared by a view's header and rows
/// elements (they are minted together and need the same layout).
///
/// The layout depends only on (view, columns, width); the view and columns
/// are fixed for the elements' lifetime, so the width is the only key. A
/// cursor move re-renders at the same width and hits the cache — the
/// O(table) layout runs once per width, the O(window) paint per frame.
struct TableLines {
    view: Arc<ViewRows>,
    columns: Vec<ColumnDef>,
    /// Content width the cached layout was computed at.
    laid_out_at: Option<usize>,
    header: Option<LogicalLine>,
    lines: Vec<LogicalLine>,
}

impl TableLines {
    fn new(view: Arc<ViewRows>, columns: Vec<ColumnDef>) -> Self {
        TableLines {
            view,
            columns,
            laid_out_at: None,
            header: None,
            lines: Vec::new(),
        }
    }

    /// The laid-out header (when the pair has one) and row lines at
    /// `content_w`, re-laid out only when the cached width differs.
    fn at(&mut self, content_w: usize) -> (Option<LogicalLine>, &[LogicalLine]) {
        if self.laid_out_at != Some(content_w) {
            let out = assemble_table(&self.view, &self.columns, content_w);
            self.header = out.header;
            self.lines = out.lines;
            self.laid_out_at = Some(content_w);
        }
        (self.header.clone(), &self.lines)
    }
}

// --- the overlay windowing rule ---------------------------------------------

/// `window_heights`, with the one rule it does not have: **the focus is always
/// inside the window**.
///
/// `window_heights` extends backward to fill a window that would otherwise
/// stop short, which is right for filling space and wrong here — when the
/// selection is *taller than the whole allocation*, that backward step pushes
/// the selection out of its own window, and the one row the reader needs is
/// the row that goes missing. Falling back to the focus alone keeps the cursor
/// on screen; the caller clips it to `h`.
pub(crate) fn window_keeps(heights: &[usize], focus: usize, budget: usize) -> (usize, usize) {
    let (start, count) = window_heights(heights, focus, budget);
    if heights.is_empty() || (start..start + count).contains(&focus) {
        (start, count)
    } else {
        (focus, 1)
    }
}

// --- the shared selection ---------------------------------------------------

/// The figures a keystroke moves in place: the cursor's row and the detail
/// lines above the table (the node's `header()` output tracks the focused
/// row, so it moves with the cursor too). Shared between the host, which
/// writes it on each engine plan, and the elements, which read it inside
/// `render` — a selection move is a write, not a rebuild (step 4h-e).
#[derive(Default)]
struct Selection {
    selected: Option<usize>,
    detail: Vec<LogicalLine>,
}

// --- the three section elements ----------------------------------------------

/// The node's `header()` output: context, above the table. Elastic (`min` 0),
/// for the same reason the header is — detail is the first thing to lose to a
/// visible row, and the last thing worth keeping when the rows have to be
/// clipped.
///
/// Within its allocation it keeps **whole logical lines**, dropping from
/// the bottom: a detail line too wide for the terminal was split when
/// painted, and it is kept or dropped whole rather than cut mid-line.
struct DetailItem {
    selection: Arc<Mutex<Selection>>,
}

impl RenderElement for DetailItem {
    fn height(&self, width: usize) -> RowAsk {
        // Painted at this width to measure: a line wider than the terminal
        // splits, and the ask is the rows it actually occupies here.
        let sel = self.selection.lock().unwrap();
        RowAsk::fixed(0, paint_detail(&sel.detail, width).height())
    }

    fn render(&self, h: usize, w: usize) -> Rendered {
        if h == 0 {
            return Rendered::default();
        }
        let section = {
            let sel = self.selection.lock().unwrap();
            paint_detail(&sel.detail, w)
        };
        // Whole logical lines only: a line's painted sub-rows are kept or
        // dropped together, so the cut never lands inside one.
        let mut used = 0usize;
        let mut end = 0usize;
        for &height in &section.heights {
            if used + height > h {
                break;
            }
            used += height;
            end += height;
        }
        let rows = section.rows[..end].iter().map(|r| pad_row(r, w)).collect();
        Rendered { rows, caret: None }
    }
}

/// Where a header element's line comes from.
enum HeaderSource {
    /// A view's table header: laid out with the rows, at the render width,
    /// through the layout the rows element shares.
    View(Arc<Mutex<TableLines>>),
    /// A host-supplied line (confirm dialog, tests): painted as-is.
    Line(Option<LogicalLine>),
}

/// The table's column-label line. **Elastic** (`min` 0), deliberately:
/// protecting it inverts the right priority when space runs out. Measured
/// against `allocate`, a budget of 3 with a protected header gives the
/// header its row and the menu *zero* rows — and given the choice, a row
/// beats a header. So the header yields.
struct HeaderItem {
    source: HeaderSource,
}

impl RenderElement for HeaderItem {
    fn height(&self, width: usize) -> RowAsk {
        // A laid-out header line never splits: the layout width *is* the
        // painter's content width, so the view variant is one row. A
        // host-supplied line was formatted at the host's width and may be
        // split here — measure it the way it will paint.
        let rows = match &self.source {
            HeaderSource::View(_) => 1,
            HeaderSource::Line(Some(line)) => paint_header(Some(line), width).height(),
            HeaderSource::Line(None) => 0,
        };
        RowAsk::fixed(0, rows)
    }

    fn render(&self, h: usize, w: usize) -> Rendered {
        if h == 0 {
            return Rendered::default();
        }
        let line = match &self.source {
            HeaderSource::View(lines) => lines.lock().unwrap().at(w.saturating_sub(MARKER)).0,
            HeaderSource::Line(line) => line.clone(),
        };
        let Some(line) = line else {
            return Rendered::default();
        };
        let section = paint_header(Some(&line), w);
        let rows = section.rows.iter().take(h).map(|r| pad_row(r, w)).collect();
        Rendered { rows, caret: None }
    }
}

/// Where a rows element's lines come from.
enum RowsSource {
    /// A view's rows: laid out at the render width through the shared
    /// table layout; active flags resolved through the view's iterator at
    /// paint time.
    View {
        lines: Arc<Mutex<TableLines>>,
        view: Arc<ViewRows>,
    },
    /// Host-supplied pre-laid-out rows (confirm dialog, tests): painted
    /// as they are, with the host's active indices.
    Lines {
        lines: Arc<Vec<LogicalLine>>,
        active: Vec<usize>,
    },
}

/// The scrollable rows, held **unpainted** and windowed at render time.
///
/// The cursor's *logical* row index (in the shared [`Selection`]) is what
/// this element windows around, so there is no painted-index arithmetic
/// anywhere: the selection is identified exactly the way the engine
/// identified it, and nothing painted above it can move it.
///
/// Holding unpainted rows is what makes painting O(window) rather than
/// O(table): a 1000-row menu paints the ~14 rows actually on screen instead
/// of all 1000 (measured 2.5ms → 19µs). For a view the layout is cached
/// beside the element ( [`TableLines`], keyed by width), so a cursor move
/// repaints the window without re-laying-out the table.
struct RowsItem {
    source: RowsSource,
    selection: Arc<Mutex<Selection>>,
    /// The floor the host pinned: a menu's `1`, a dialog's option count —
    /// `allocate` serves every `must` before any `might`, so this holds
    /// under pressure (with the caveat that below the summed `must`s
    /// `fill` degrades to one row each in element order, and this element
    /// is last).
    floor: usize,
}

impl RenderElement for RowsItem {
    fn height(&self, width: usize) -> RowAsk {
        let max = match &self.source {
            // The want is the height *if every row were shown*, measured
            // the way the layout writes them (embedded `\n` only). Exact
            // at this width: the layout width is the painter's content
            // width, so nothing will split further when painted.
            RowsSource::View { lines, .. } => {
                let content_w = width.saturating_sub(MARKER);
                lines
                    .lock()
                    .unwrap()
                    .at(content_w)
                    .1
                    .iter()
                    .map(visual_height)
                    .sum()
            }
            // Pre-laid-out rows were formatted at some host width; a row
            // too wide for *this* width splits when painted, so this is a
            // lower bound on the painted height — the safe direction: the
            // allocator is never promised more rows than the element can
            // actually produce. `render` re-measures and fits `h` anyway.
            RowsSource::Lines { lines, .. } => lines.iter().map(visual_height).sum(),
        };
        RowAsk::fixed(self.floor.min(max), max)
    }

    fn render(&self, h: usize, w: usize) -> Rendered {
        if h == 0 {
            return Rendered::default();
        }
        // Copied out of the shared handle first: the figure is `Copy`, and
        // taking it here means the selection lock is never held together /// with the table layout's below.
        let selected = self.selection.lock().unwrap().selected;
        match &self.source {
            RowsSource::View { lines, view } => {
                let mut guard = lines.lock().unwrap();
                let (_, lines) = guard.at(w.saturating_sub(MARKER));
                if lines.is_empty() {
                    return Rendered::default();
                }
                let selected = selected.filter(|s| *s < lines.len());
                // Window on the heights the layout wrote — painting cannot
                // make a row taller (the layout width *is* the painter's
                // content width), so this window is the painted window.
                let heights: Vec<usize> = lines.iter().map(visual_height).collect();
                let (start, count) = window_keeps(&heights, selected.unwrap_or(0), h);
                let count = count.min(lines.len().saturating_sub(start));
                if count == 0 {
                    return Rendered::default();
                }
                let in_window = |i: &usize| (start..start + count).contains(i);
                // Active flags resolved *here*, through the view's
                // iterator: order-driven resolution, stale-slot skips and
                // the active-set lookup all run in the element. Positions
                // align with the laid-out lines because the layout pushes
                // one line per `iter()` row.
                let active = view
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (_, a))| if a { Some(i) } else { None })
                    .filter(in_window)
                    // Rebased like the cursor: `paint_rows` reads the
                    // window as offsets into it, not as plan indices.
                    .map(|i| i - start)
                    .collect::<Vec<_>>();
                let section = paint_rows(
                    &lines[start..start + count],
                    selected.filter(in_window).map(|s| s - start),
                    &active,
                    w,
                );
                // `take(h)` is the contract: `render` fits its allocation,
                // even when the selected row is taller than the whole thing
                // — the marker is on its first row, so the cursor is still
                // visible.
                let rows = section.rows.iter().take(h).map(|r| pad_row(r, w)).collect();
                Rendered { rows, caret: None }
            }
            RowsSource::Lines { lines, active } => {
                if lines.is_empty() {
                    return Rendered::default();
                }
                let selected = selected.filter(|s| *s < lines.len());
                // Window on the heights the engine wrote (embedded `\n`),
                // paint only that window, then re-window on the *painted*
                // heights: a pre-laid-out row can be wider than this width
                // and split, and the window has to fit `h` in the unit the
                // frame counts in.
                let engine_heights: Vec<usize> = lines.iter().map(visual_height).collect();
                let (start, count) = window_keeps(&engine_heights, selected.unwrap_or(0), h);
                let count = count.min(lines.len().saturating_sub(start));
                if count == 0 {
                    return Rendered::default();
                }
                let in_window = |i: &usize| (start..start + count).contains(i);
                let section = paint_rows(
                    &lines[start..start + count],
                    selected.filter(in_window).map(|s| s - start),
                    &active
                        .iter()
                        .filter(|i| in_window(i))
                        // Rebased like the cursor, not just filtered:
                        // `active_rows` index the *plan's* rows, while
                        // `paint_rows` is handed the window and reads them
                        // as offsets into it. Without the `- start` an
                        // active row at index 7 in a window starting at 5
                        // marks offset 7 — i.e. plan row 12.
                        .map(|i| i - start)
                        .collect::<Vec<_>>(),
                    w,
                );
                // The selection's index *within the painted window* — the
                // thing this element windows around. Nothing above it can
                // move it, which is the whole point: there is no global
                // painted index to keep in sync.
                let focus = selected
                    .map(|s| s - start)
                    .filter(|f| *f < section.heights.len())
                    .unwrap_or(0);
                let (win_start, win_count) = window_keeps(&section.heights, focus, h);
                let rows = section
                    .slice(win_start, win_count)
                    .iter()
                    .take(h)
                    .map(|r| pad_row(r, w))
                    .collect();
                Rendered { rows, caret: None }
            }
        }
    }
}

// --- the mint ----------------------------------------------------------------

/// What an [`ElementSet`] was minted over — the identity
/// [`ElementSet::update`] compares a fresh plan against. An engine plan's
/// identity is its view handle (a filter or a drill mints a new one; a
/// cursor move hands back the same); a hand-built plan's is its rows
/// pointer, plus the figures baked beside them.
enum Origin {
    View(Arc<ViewRows>),
    Lines {
        rows: Arc<Vec<LogicalLine>>,
        active: Vec<usize>,
        header: Option<LogicalLine>,
    },
}

/// What [`ElementSet::update`] did with a fresh plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// The rows underneath were the ones the set was minted from: only /// the selection figures moved, through the shared handle — the *same* /// elements paint the new cursor.
    Moved,
    /// The rows changed (filter, new plan): the set re-minted itself /// from the plan.
    Rebuilt,
}

/// One open panel's element set: the frame elements, minted once, plus the
/// shared [`Selection`] their `render`s read.
///
/// Constructed by [`RenderPlan::element_set`] when a host keeps the panel
/// open across keystrokes. The active (`*`) flags are *not* in the shared
/// state: a view-backed rows element resolves them through the view's
/// iterator at paint time, and a host-built (pre-laid-out rows) set is
/// one-shot — [`RenderPlan::elements`] drops the state.
pub struct ElementSet {
    elements: Vec<Element>,
    selection: Arc<Mutex<Selection>>,
    /// The plan this set paints: the mint's original, refreshed by every /// [`Self::update`]. The host's cursor math (bounds, paging) reads /// it here, so dispatch and paint cannot disagree.
    plan: RenderPlan,
    /// The rows element was minted over this — the identity `update`
    /// compares.
    origin: Origin,
    /// Whether a detail element was minted: the set's shape is fixed at
    /// mint, so a plan that grows or drops its detail section is a
    /// rebuild, not a figure move.
    had_detail: bool,
    /// The floor a rebuild mints with (see [`RenderPlan::element_set`]).
    row_min: usize,
}

impl ElementSet {
    /// The elements to append to the frame stack — clones of the minted
    /// set (shared boxes), so the set outlives any one composition.
    pub fn elements(&self) -> Vec<Element> {
        self.elements.clone()
    }

    /// The engine's plan behind this set — what host cursor math reads.
    pub fn plan(&self) -> &RenderPlan {
        &self.plan
    }

    /// A fresh plan for this panel: the selection figures flow to the
    /// shared handle in place when the rows underneath are the ones this
    /// set was minted from — a cursor keystroke, no rebuild; rows that
    /// actually changed (filter, new plan) re-mint the set.
    ///
    /// A width change is *not* a rebuild: the elements lay themselves out /// at the render width (4h-d), so the same elements re-wrap for the /// new width — only the host's laid-out snapshot ([`RenderPlan::rows`]) /// is refreshed here for cursor math.
    pub fn update(&mut self, plan: &RenderPlan) -> Update {
        if self.same_panel(plan) {
            let mut sel = self.selection.lock().unwrap();
            sel.selected = plan.selected;
            sel.detail = plan.detail.clone();
            drop(sel);
            self.plan = plan.clone();
            Update::Moved
        } else {
            *self = plan.element_set(self.row_min);
            Update::Rebuilt
        }
    }

    /// Same panel underneath: the same rows identity (see [`Origin`]), and /// the same *sections* — the detail element is minted only when the /// plan has detail lines, so a section appearing or vanishing is a /// rebuild too, not a figure move.
    fn same_panel(&self, plan: &RenderPlan) -> bool {
        let same_sections = self.had_detail == !plan.detail.is_empty();
        same_sections
            && match (&self.origin, &plan.view) {
                // Engine plans: the same view handle means the same rows, /// the same `*` flags and the same columns — the elements /// re-lay-out from it at the render width.
                (Origin::View(v), Some(pv)) => Arc::ptr_eq(v, pv),
                // Hand-built plans: the same laid-out rows *and* the /// figures baked beside them.
                (
                    Origin::Lines {
                        rows,
                        active,
                        header,
                    },
                    None,
                ) => {
                    Arc::ptr_eq(rows, &plan.rows)
                        && *active == plan.active_rows
                        && *header == plan.table_header
                }
                _ => false,
            }
    }
}

impl RenderPlan {
    /// The frame elements for this panel, in paint order: the node's
    /// detail, the table header, the scrollable rows.
    ///
    /// One builder for both hosts (the repl's slash menu / prose dialog
    /// and the confirmation dialog) so the two cannot drift on which
    /// sections exist, in what order, or with what paint. Empty sections
    /// are dropped rather than contributed as zero-height elements: a plan
    /// with no columns has no header, and an empty element would still ask
    /// the allocator for a divider.
    ///
    /// `row_min` is the rows element's floor: a menu's `1` — show something
    /// if there is anything to show — a confirmation's option count,
    /// because `allocate` will happily starve a dialog down to nothing and
    /// a consent prompt with no visible options is not one.
    pub fn elements(&self, row_min: usize) -> Vec<Element> {
        self.element_set(row_min).elements
    }

    /// The element set for an open panel: the elements [`Self::elements`]
    /// returns, plus the shared selection handle a keystroke moves in
    /// place (step 4h-e).
    pub fn element_set(&self, row_min: usize) -> ElementSet {
        let selection = Arc::new(Mutex::new(Selection {
            selected: self.selected,
            detail: self.detail.clone(),
        }));
        let mut els = Vec::with_capacity(3);
        let had_detail = !self.detail.is_empty();
        if had_detail {
            els.push(Element::new(DetailItem {
                selection: Arc::clone(&selection),
            }));
        }
        let origin = match &self.view {
            Some(view) => {
                let lines = Arc::new(Mutex::new(TableLines::new(
                    Arc::clone(view),
                    self.columns.clone(),
                )));
                if has_table_header(view, &self.columns) {
                    els.push(Element::new(HeaderItem {
                        source: HeaderSource::View(Arc::clone(&lines)),
                    }));
                }
                if !view.is_empty() {
                    els.push(Element::new(RowsItem {
                        source: RowsSource::View {
                            lines,
                            view: Arc::clone(view),
                        },
                        selection: Arc::clone(&selection),
                        floor: row_min,
                    }));
                }
                Origin::View(Arc::clone(view))
            }
            None => {
                let header = self.table_header.clone();
                if let Some(th) = &header {
                    els.push(Element::new(HeaderItem {
                        source: HeaderSource::Line(Some(th.clone())),
                    }));
                }
                if !self.rows.is_empty() {
                    els.push(Element::new(RowsItem {
                        source: RowsSource::Lines {
                            lines: Arc::clone(&self.rows),
                            active: self.active_rows.clone(),
                        },
                        selection: Arc::clone(&selection),
                        floor: row_min,
                    }));
                }
                Origin::Lines {
                    rows: Arc::clone(&self.rows),
                    active: self.active_rows.clone(),
                    header,
                }
            }
        };
        ElementSet {
            elements: els,
            selection,
            plan: self.clone(),
            origin,
            had_detail,
            row_min,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::Row;
    use crate::node_entry::RowRef;
    use crate::node_impl::{DataResponse, RenderContext};
    use crate::render::RenderPlan;
    use crate::table::display_width;
    use crate::view::{RowView, ViewRequest};
    use slashdot_tui::render::{allocate, live_budget, render};

    /// Visible text of a painted row: drop the SGR runs, then trim.
    fn plain(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out.trim().to_string()
    }

    /// A hand-built plan (no view) mints the pre-laid-out-lines elements.
    fn lines_plan(
        header: Option<&str>,
        detail: &[&str],
        rows: &[&str],
        sel: Option<usize>,
    ) -> RenderPlan {
        RenderPlan {
            table_header: header.map(LogicalLine::new),
            detail: detail.iter().map(|l| LogicalLine::new(*l)).collect(),
            rows: Arc::new(rows.iter().map(|l| LogicalLine::new(*l)).collect()),
            selected: sel,
            active_rows: Vec::new(),
            view: None,
            columns: Vec::new(),
        }
    }

    /// A one-slot view over `rows`, with `active_ids` marking `*` rows.
    fn view_rows(rows: Vec<Row>, active_ids: Vec<String>) -> Arc<ViewRows> {
        let n = rows.len();
        let set = active_ids.iter().cloned().collect();
        Arc::new(ViewRows::new(
            vec![Some(Arc::new(DataResponse { rows, active_ids }))],
            (0..n as u32)
                .map(|i| RowRef {
                    child_idx: 0,
                    row_offset: i,
                })
                .collect(),
            set,
            Default::default(),
        ))
    }

    /// An engine-shaped plan carrying its view handle: mints the
    /// render-width-formatting elements.
    fn view_plan(view: Arc<ViewRows>, sel: Option<usize>) -> RenderPlan {
        RenderPlan {
            selected: sel,
            view: Some(view),
            ..Default::default()
        }
    }

    fn asks_of(els: &[Element], w: usize) -> Vec<RowAsk> {
        els.iter().map(|e| e.ask(w)).collect()
    }

    // --- the three overlay elements (pre-laid-out-lines path) ---------------

    /// The header is a *section*, not the first row of a scrollable block: it
    /// stays put while the rows under it move.
    #[test]
    fn table_header_survives_a_scrolling_row_section() {
        let rows: Vec<String> = (0..30).map(|i| format!("model-{i}")).collect();
        let rowrefs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let plan = lines_plan(Some("MODEL"), &["pick one"], &rowrefs, Some(25));
        let els = plan.elements(1);
        let asks = asks_of(&els, 60);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&els, &heights, 60);

        assert!(f.height() < 30, "the rows were windowed: {}", f.height());
        assert!(
            f.row(0).trim_end().contains("pick one"),
            "detail first: {:?}",
            f.row(0)
        );
        assert!(
            f.row(1).trim_end().contains("MODEL"),
            "header fixed: {:?}",
            f.row(1)
        );
    }

    /// Under *real* pressure the header yields to a visible row.
    #[test]
    fn header_element_extent_yields_to_a_row_under_starvation() {
        let asks = |h_min, h_max| {
            vec![
                RowAsk::fixed(1, 1), // input
                RowAsk::fixed(1, 1), // divider
                RowAsk::fixed(0, 5), // detail
                RowAsk::fixed(h_min, h_max),
                RowAsk::fixed(1, 20), // rows
            ]
        };
        let protected = allocate(&asks(1, 1), 24, 3);
        assert_eq!(protected[4], 0, "a protected header wins the last row");
        let elastic = allocate(&asks(0, 1), 24, 3);
        assert_eq!(elastic[4], 1, "a row beats a header");
        assert_eq!(elastic[3], 0, "the header yielded");
        for h in [5usize, 8, 16] {
            assert_eq!(allocate(&asks(0, 1), 24, h)[3], 1, "budget {h}");
        }
    }

    /// A painted section row is exactly `term_w` columns wide, with the padding
    /// inside the styled span so a menu background reaches the right edge.
    #[test]
    fn section_rows_fill_the_width_inside_the_styled_span() {
        for (label, tw) in [("ab", 8usize), ("abcdefgh", 8), ("ab", 5)] {
            let plan = lines_plan(Some(label), &[], &[], None);
            let els = plan.elements(1);
            assert_eq!(els.len(), 1, "just the header");
            let h = els[0].ask(tw).resolve(24).might;
            let f = render(&els, &[h], tw);
            assert_eq!(f.height(), h, "{label:?} at {tw}");
            for i in 0..f.height() {
                assert_eq!(
                    display_width(f.row(i)),
                    tw,
                    "{label:?} at {tw} row {i}: {:?}",
                    f.row(i)
                );
                assert!(f.row(i).ends_with("\x1b[0m"), "{:?}", f.row(i));
            }
        }
    }

    /// The rows element windows around the cursor and reports the selection in
    /// the window.
    #[test]
    fn rows_element_windows_around_the_cursor() {
        let rows: Vec<String> = (0..20).map(|i| format!("item {i}")).collect();
        let refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let els = lines_plan(None, &[], &refs, Some(10)).elements(1);
        let f = render(&els, &[4], 20);
        assert_eq!(f.height(), 4);
        assert!(f.row(0).contains("item 9"), "{:?}", f.row(0));
        assert!(f.row(3).contains("item 12"), "{:?}", f.row(3));
        assert!(f.row(1).contains("> item 10"), "cursor row: {:?}", f.row(1));
    }

    /// Moving the cursor moves the window and keeps the selection visible —
    /// the same `Arc` of rows, repainted only for the new window.
    #[test]
    fn rows_element_follows_the_cursor_without_moving_the_header() {
        let rows: Vec<String> = (0..30).map(|i| format!("item {i}")).collect();
        let refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        for sel in [0usize, 15, 29] {
            let els = lines_plan(Some("HDR"), &[], &refs, Some(sel)).elements(1);
            let asks = asks_of(&els, 20);
            let heights = allocate(&asks, 24, live_budget(24));
            let f = render(&els, &heights, 20);
            assert!(
                f.row(0).trim_end().contains("HDR"),
                "header fixed at {sel}: {:?}",
                f.row(0)
            );
            let marked = (0..f.height())
                .find(|i| f.row(*i).contains('>'))
                .unwrap_or_else(|| panic!("no cursor row painted at {sel}"));
            assert!(f.row(marked).contains(&format!("item {sel}")), "{sel}");
        }
    }

    /// The rows element honours its contract even when a single row is taller
    /// than the whole allocation: it clips rather than overflowing.
    #[test]
    fn rows_element_fits_a_selection_taller_than_its_budget() {
        let plan = lines_plan(None, &[], &["short", "tall\nrow\nof\nlines"], Some(1));
        let els = plan.elements(1);
        let f = render(&els, &[2], 20);
        assert!(f.height() <= 2, "never more than h: {}", f.height());
        assert!(f.row(0).contains("tall"), "{:?}", f.row(0));
    }

    /// An empty plan contributes no overlay elements at all, so the frame does
    /// not draw a divider for a menu that is not there.
    #[test]
    fn an_empty_plan_is_no_elements() {
        assert!(RenderPlan::default().elements(1).is_empty());
    }

    /// The three sections arrive in paint order — detail, header, rows — and
    /// a section with nothing in it is dropped rather than contributed as a
    /// zero-height element.
    #[test]
    fn elements_are_in_paint_order_and_skip_empty_sections() {
        let plan = lines_plan(Some("MODEL"), &["pick one"], &["m0", "m1"], Some(1));
        let els = plan.elements(1);
        let asks = asks_of(&els, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&els, &heights, 40);
        assert_eq!(f.height(), 4, "detail + header + 2 rows: {:?}", f.row(0));
        assert!(f.row(0).contains("pick one"), "{:?}", f.row(0));
        assert!(f.row(1).contains("MODEL"), "{:?}", f.row(1));
        assert!(
            f.row(3).contains("> m1"),
            "cursor on the last row: {:?}",
            f.row(3)
        );

        // No header, no detail: just the rows.
        let bare = lines_plan(None, &[], &["only"], Some(0));
        let els = bare.elements(1);
        assert_eq!(els.len(), 1, "only the rows element");
        let asks = asks_of(&els, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        assert_eq!(render(&els, &heights, 40).height(), 1);
    }

    /// An active row keeps its `*` after the rows have scrolled — the active
    /// indices are absolute (index the plan's rows) and get rebased by the
    /// window's start exactly as the cursor is.
    #[test]
    fn active_markers_survive_a_scrolled_window() {
        let n = 40usize;
        let rows: Vec<String> = (0..n).map(|i| format!("item {i}")).collect();
        let refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let stars = |sel: usize, active: Vec<usize>| {
            let mut plan = lines_plan(None, &[], &refs, Some(sel));
            plan.active_rows = active;
            let els = plan.elements(1);
            let asks = asks_of(&els, 20);
            let heights = allocate(&asks, 24, live_budget(24));
            let f = render(&els, &heights, 20);
            (0..f.height())
                .filter(|i| f.row(*i).contains('*'))
                .map(|i| plain(f.row(i)))
                .collect::<Vec<_>>()
        };
        assert_eq!(stars(30, vec![31]), vec!["* item 31".to_string()]);
        assert_eq!(stars(30, vec![7]), Vec::<String>::new());
    }

    // --- the view-backed path (engine plans) --------------------------------

    /// A view-backed rows element lays its table out at the *render* width,
    /// not at the width the plan was built at — a row laid out for 80 columns
    /// is re-laid-out (wrapped) for the 30 it is painted into, so every
    /// painted row is exactly the render width and no line runs over.
    #[test]
    fn view_elements_lay_out_at_the_render_width_not_the_plan_width() {
        // A plan built at width 80 (its cached `rows` are laid out for 78).
        let mut rv = RowView::new();
        let long = "a description long enough to wrap several times at thirty columns";
        let view = view_rows(vec![Row::new("item").desc(long)], vec![]);
        let built = rv.plan(
            view,
            &ViewRequest {
                render_ctx: RenderContext { term_width: 80 },
                ..Default::default()
            },
        );
        assert!(
            built.plan.view.is_some(),
            "engine plans carry the view handle"
        );
        // The plan's cached rows were laid out for width 80: one wide row.
        assert_eq!(built.plan.rows.len(), 1);

        // Mint at 30: the element re-lays-out through the view, and the
        // over-long description wraps into several 30-column rows instead
        // of splitting mid-line.
        let els = built.plan.elements(1);
        let asks = asks_of(&els, 30);
        let heights = allocate(&asks, 40, live_budget(40));
        let f = render(&els, &heights, 30);
        assert!(
            f.height() > 1,
            "the long description wrapped: {}",
            f.height()
        );
        for i in 0..f.height() {
            assert_eq!(
                display_width(f.row(i)),
                30,
                "row {i} not re-laid-out: {:?}",
                f.row(i)
            );
        }
        // The cursor stays on the (single) logical row wherever the window
        // lands: its marker is present on the first painted row of it.
        assert!(f.row(0).contains("> "), "cursor marker: {:?}", f.row(0));
    }

    /// Active flags are resolved through the view's iterator inside the
    /// element — order-driven, with stale slots skipped — not through a
    /// pre-computed index list on the plan.
    #[test]
    fn view_rows_element_resolves_active_flags_and_skips_stale_slots() {
        // Two slots, the second stale (None): its row must vanish, and the
        // active flag must follow the *id* through the iterator. The active
        // set lives on the *view*, not the slot (that is the surface the
        // render side sees).
        let rows = vec![Row::new("a"), Row::new("b")];
        let view = Arc::new(ViewRows::new(
            vec![
                Some(Arc::new(DataResponse {
                    rows,
                    active_ids: Vec::new(),
                })),
                None,
            ],
            vec![
                RowRef {
                    child_idx: 0,
                    row_offset: 0,
                },
                RowRef {
                    child_idx: 0,
                    row_offset: 1,
                },
                RowRef {
                    child_idx: 1,
                    row_offset: 0, // stale
                },
            ],
            ["b".to_string()].into_iter().collect(),
            Default::default(),
        ));
        let plan = view_plan(view, Some(1));
        let els = plan.elements(1);
        let asks = asks_of(&els, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&els, &heights, 40);
        assert_eq!(
            f.height(),
            2,
            "the stale slot skipped its row: {}",
            f.height()
        );
        assert!(plain(f.row(0)) == "a", "inactive row: {:?}", f.row(0));
        assert!(
            f.row(1).contains("* b"),
            "active flag via iterator: {:?}",
            f.row(1)
        );
    }

    /// A view plan's ask is the full wrapped height of its rows at this
    /// width (the layout width is the painter's content width, so nothing
    /// splits further): the floor is `row_min`, the want is everything.
    #[test]
    fn view_rows_ask_is_the_wrapped_height_at_this_width() {
        let long = "wrap this description across several rows at forty columns please";
        let view = view_rows(vec![Row::new("one").desc(long), Row::new("two")], vec![]);
        let els = view_plan(view, Some(0)).elements(1);
        // At 40 the description wraps to >1 visual row; the ask's max is
        // the wrapped count, and min is pinned to the floor.
        let ask = els.last().unwrap().ask(40);
        let ext = ask.resolve(24);
        assert!(ext.might > 2, "wrapped height counts: {}", ext.might);
        assert_eq!(ext.must, 1, "floor of 1");
        // At a wide enough width there is no wrap: 2 rows.
        let ext = els.last().unwrap().ask(200).resolve(24);
        assert_eq!(ext.might, 2, "unwrapped: one row per logical row");
    }

    // --- ElementSet::update (step 4h-e) ------------------------------------

    /// A cursor keystroke moves the selection figures through the shared /// handle: the *same* elements (clones taken before the move) paint /// the new cursor and the new detail — no rebuild.
    #[test]
    fn a_cursor_move_updates_the_set_in_place() {
        let view = view_rows(vec![Row::new("a"), Row::new("b"), Row::new("c")], vec![]);
        // Mint with detail already present: the *sections* are fixed at /// mint, so a later plan must keep the same shape to move in place.
        let open = RenderPlan {
            selected: Some(0),
            detail: vec![LogicalLine::new("focused row: a")],
            view: Some(view.clone()),
            ..Default::default()
        };
        let mut set = open.element_set(1);
        // The elements every paint clones out — captured *before* the move.
        let els = set.elements();
        let asks = asks_of(&els, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&els, &heights, 40);
        let marked = (0..f.height())
            .find(|i| f.row(*i).contains('>'))
            .expect("a cursor row painted");
        assert!(
            plain(f.row(marked)).ends_with('a'),
            "before the move: {:?}",
            f.row(marked)
        );

        // The engine's next plan: same view, cursor moved, detail refreshed.
        let moved = RenderPlan {
            selected: Some(2),
            detail: vec![LogicalLine::new("focused row: c")],
            view: Some(view),
            ..Default::default()
        };
        assert_eq!(set.update(&moved), Update::Moved);
        assert_eq!(set.plan().selected, Some(2), "host math sees the move");

        // The elements captured before the move paint the *new* selection — /// they share the handle, nothing was rebuilt.
        let f = render(&els, &heights, 40);
        let marked = (0..f.height())
            .find(|i| f.row(*i).contains('>'))
            .expect("a cursor row painted");
        assert!(
            plain(f.row(marked)).ends_with('c'),
            "after the move: {:?}",
            f.row(marked)
        );
        assert!(
            f.row(0).contains("focused row: c"),
            "detail flowed to the handle: {:?}",
            f.row(0)
        );
    }

    /// Rows that actually changed (a filter's new view) rebuild the set: /// the fresh plan's rows are painted, and the old element clones keep /// their old rows — the handle was not silently re-pointed.
    #[test]
    fn rows_that_change_rebuild_the_set() {
        let view = view_rows(vec![Row::new("a"), Row::new("b")], vec![]);
        let mut set = view_plan(view, Some(0)).element_set(1);
        let stale = set.elements(); // clones of the pre-filter elements

        let filtered = view_rows(vec![Row::new("b")], vec![]);
        let plan = view_plan(filtered, Some(0));
        assert_eq!(set.update(&plan), Update::Rebuilt);

        let fresh = set.elements();
        let painted = |els: &[Element]| {
            let asks = asks_of(els, 40);
            let heights = allocate(&asks, 24, live_budget(24));
            let f = render(els, &heights, 40);
            (0..f.height()).map(|i| plain(f.row(i))).collect::<Vec<_>>()
        };
        // The fresh set shows only the filtered rows… (painted rows carry /// their marker, so match on content.)
        let rows = painted(&fresh);
        assert!(
            !rows.iter().any(|r| r.contains("a")),
            "the filter dropped a: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains('b')), "kept b: {rows:?}");
        // …and the stale clones are detached, still showing the old panel: /// proof the rebuild minted new elements rather than re-pointing /// the old ones.
        let rows = painted(&stale);
        assert!(
            rows.iter().any(|r| r.contains('a')),
            "old clones keep old rows: {rows:?}"
        );
    }

    /// A detail section *appearing* is a rebuild, not a figure move: the /// set was minted without a DetailItem, so an [ERROR] line arriving /// on an unchanged view needs an element to show it.
    #[test]
    fn a_detail_section_appearing_rebuilds_the_set() {
        let view = view_rows(vec![Row::new("a")], vec![]);
        let no_detail = RenderPlan {
            selected: Some(0),
            detail: Vec::new(),
            view: Some(view.clone()),
            ..Default::default()
        };
        let mut set = no_detail.element_set(1);
        let with_error = RenderPlan {
            selected: Some(0),
            detail: vec![LogicalLine::new("[ERROR]: boom")],
            view: Some(view),
            ..Default::default()
        };
        assert_eq!(set.update(&with_error), Update::Rebuilt);

        let fresh = set.elements();
        let asks = asks_of(&fresh, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&fresh, &heights, 40);
        assert!(
            f.row(0).contains("[ERROR]: boom"),
            "the rebuilt set paints the error: {:?}",
            f.row(0)
        );
    }

    /// A hand-built (pre-laid-out rows) set moves its selection in place /// too — the lines path shares the handle exactly as the view path /// does.
    #[test]
    fn a_hand_built_set_moves_its_selection_in_place() {
        let rows: Vec<String> = (0..5).map(|i| format!("item {i}")).collect();
        let rowrefs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let plan = lines_plan(None, &[], &rowrefs, Some(0));
        let mut set = plan.element_set(1);
        let els = set.elements();

        let moved = RenderPlan {
            selected: Some(3),
            ..plan.clone()
        };
        assert_eq!(set.update(&moved), Update::Moved);

        let asks = asks_of(&els, 40);
        let heights = allocate(&asks, 24, live_budget(24));
        let f = render(&els, &heights, 40);
        let marked = (0..f.height())
            .find(|i| f.row(*i).contains('>'))
            .expect("a cursor row painted");
        assert!(
            plain(f.row(marked)).ends_with("item 3"),
            "the pre-move clones paint the new cursor: {:?}",
            f.row(marked)
        );
    }
}
